//! WAL writer: append-only log writer with fsync support.
//!
//! v2 layout (Phase 40b): the file starts with an 8 KiB header page
//! carrying the magic and format version, followed by zero or more v2
//! record frames. v1 files (no header) are detected on open and migrated
//! eagerly to v2 before any new writes happen — see [`WalWriter::new`].

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::Result;
use crate::wal::hlc::{Hlc, HlcClock};
use crate::wal::record::{
    NIL_NODE_ID, WAL_HEADER_SIZE, WAL_MAGIC, WAL_VERSION_CURRENT, WAL_VERSION_V1, WAL_VERSION_V2,
    WalRecord, build_wal_header, parse_wal_header,
};
use crate::wal::vclock::VectorClock;

/// Append-only WAL writer.
///
/// Writes records sequentially. Each record is self-contained with its
/// own checksum. The LSN (Log Sequence Number) increases monotonically.
///
/// In Phase 40b every record additionally carries an `origin_node_id`,
/// an HLC, and a vector clock. The writer stamps the origin from
/// `node_id` and pulls the HLC from the shared `Arc<HlcClock>` at every
/// `log_*` call. The vector clock is the single-writer singleton
/// `{ node_id: hlc }` (Phase 40e will populate it from a peer).
pub struct WalWriter {
    file: File,
    /// Path to the WAL file (held for v1 → v2 atomic migration via
    /// tmpfile + rename, even when no migration ever fires in the
    /// current process). Unused otherwise.
    #[allow(dead_code)]
    path: PathBuf,
    /// Next LSN to assign.
    next_lsn: u64,
    /// Current transaction ID counter.
    next_tx_id: u64,
    /// Format version of the file currently on disk.
    file_version: u16,
    /// Origin node identifier stamped on every record.
    node_id: u128,
    /// HLC source (shared across the engine).
    clock: Arc<HlcClock>,
}

impl WalWriter {
    /// Opens or creates a WAL file. Equivalent to
    /// [`Self::new_with_identity`] using a fresh embedded identity:
    /// `NIL_NODE_ID` for the origin and a brand-new [`HlcClock`].
    ///
    /// Provided as a convenience for legacy single-node tests; new
    /// callers should prefer [`Self::new_with_identity`].
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        Self::new_with_identity(path, NIL_NODE_ID, Arc::new(HlcClock::new()))
    }

    /// Opens or creates a WAL file, stamping records with the given
    /// `node_id` and HLC source.
    pub fn new_with_identity(
        path: impl AsRef<Path>,
        node_id: u128,
        clock: Arc<HlcClock>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let exists_with_data = path.exists() && path.metadata()?.len() > 0;

        if !exists_with_data {
            // Brand new file (or zero bytes) — write a fresh v2 header.
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            file.write_all(&build_wal_header(WAL_VERSION_CURRENT))?;
            file.sync_all()?;
            file.seek(SeekFrom::End(0))?;
            return Ok(Self {
                file,
                path,
                next_lsn: 1,
                next_tx_id: 1,
                file_version: WAL_VERSION_CURRENT,
                node_id,
                clock,
            });
        }

        // File exists with content — detect version.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(false)
            .truncate(false)
            .open(&path)?;
        let file_len = file.metadata()?.len();

        let mut magic = [0u8; 8];
        file.seek(SeekFrom::Start(0))?;
        let read = file.read(&mut magic)?;
        let detected_version = if read == 8 && &magic == WAL_MAGIC {
            // Read full header for version.
            let mut hdr = [0u8; 18];
            file.seek(SeekFrom::Start(0))?;
            // It's fine if the file is shorter than 18 bytes — defensive
            // here (an empty header was written above for new files).
            let n = file.read(&mut hdr)?;
            parse_wal_header(&hdr[..n])?
        } else {
            // No magic → v1. Migrate eagerly. We deviate slightly from
            // the spec note about "lazy migration on first write": doing
            // it on open is functionally equivalent (the next write was
            // imminent anyway) and keeps the writer state simple.
            WAL_VERSION_V1
        };

        if detected_version == WAL_VERSION_V2 {
            // Read all records from after the header to figure out next LSN/TX.
            let records = Self::read_v2_records_from_file(&mut file)?;
            let max_lsn = records.iter().map(|r| r.lsn).max().unwrap_or(0);
            let max_tx = records.iter().map(|r| r.tx_id).max().unwrap_or(0);
            file.seek(SeekFrom::End(0))?;
            return Ok(Self {
                file,
                path,
                next_lsn: max_lsn + 1,
                next_tx_id: max_tx + 1,
                file_version: WAL_VERSION_V2,
                node_id,
                clock,
            });
        }

        // v1: read all records, migrate to v2 atomically (tmp + rename).
        let v1_records = Self::read_v1_records_from_file(&mut file, file_len)?;
        let max_lsn = v1_records.iter().map(|r| r.lsn).max().unwrap_or(0);
        let max_tx = v1_records.iter().map(|r| r.tx_id).max().unwrap_or(0);
        // Drop the v1 handle before doing the rename.
        drop(file);

        Self::migrate_v1_to_v2(&path, &v1_records)?;

        // Reopen the freshly migrated v2 file.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(false)
            .truncate(false)
            .open(&path)?;
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file,
            path,
            next_lsn: max_lsn + 1,
            next_tx_id: max_tx + 1,
            file_version: WAL_VERSION_V2,
            node_id,
            clock,
        })
    }

    /// Replaces the file with a v2-formatted version of the same records.
    /// Atomic via a temp file + rename.
    fn migrate_v1_to_v2(path: &Path, v1_records: &[WalRecord]) -> Result<()> {
        let mut tmp = path.to_path_buf();
        let new_name = match path.file_name() {
            Some(n) => format!("{}.v2-tmp", n.to_string_lossy()),
            None => "wal.v2-tmp".to_string(),
        };
        tmp.set_file_name(new_name);

        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&build_wal_header(WAL_VERSION_V2))?;
            for rec in v1_records {
                // The v1 → v2 in-memory mapping has already happened on
                // decode; encoding here uses the v2 layout.
                f.write_all(&rec.to_bytes())?;
            }
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        // Best-effort fsync the directory so the rename is durable.
        if let Some(parent) = path.parent()
            && let Ok(d) = OpenOptions::new().read(true).open(parent)
        {
            let _ = d.sync_all();
        }
        Ok(())
    }

    /// Begins a new transaction. Returns the transaction ID.
    pub fn begin_tx(&mut self) -> u64 {
        let tx_id = self.next_tx_id;
        self.next_tx_id += 1;
        tx_id
    }

    /// Returns the configured node identifier (origin stamp).
    pub fn node_id(&self) -> u128 {
        self.node_id
    }

    /// Returns a clone of the shared HLC clock.
    pub fn clock(&self) -> Arc<HlcClock> {
        Arc::clone(&self.clock)
    }

    /// Returns the WAL file format version currently on disk (1 or 2).
    pub fn file_version(&self) -> u16 {
        self.file_version
    }

    /// Logs a page write (before + after images).
    ///
    /// Does NOT fsync — the record is buffered until commit. Returns
    /// `(lsn, hlc)` for downstream tracking.
    pub fn log_page_write(
        &mut self,
        tx_id: u64,
        page_id: u32,
        before: &[u8],
        after: &[u8],
    ) -> Result<(u64, Hlc)> {
        let lsn = self.alloc_lsn();
        let hlc = self
            .clock
            .now()
            .map_err(|e| crate::error::GrumpyError::Hlc(e.to_string()))?;
        let vc = VectorClock::singleton(self.node_id, hlc.0);
        let record =
            WalRecord::page_write(lsn, tx_id, self.node_id, hlc, vc, page_id, before, after);
        self.append_record(&record)?;
        Ok((lsn, hlc))
    }

    /// Logs a commit record and fsyncs the WAL.
    ///
    /// After this returns, the transaction is guaranteed durable.
    pub fn log_commit(&mut self, tx_id: u64) -> Result<(u64, Hlc)> {
        let lsn = self.alloc_lsn();
        let hlc = self
            .clock
            .now()
            .map_err(|e| crate::error::GrumpyError::Hlc(e.to_string()))?;
        let vc = VectorClock::singleton(self.node_id, hlc.0);
        let record = WalRecord::commit(lsn, tx_id, self.node_id, hlc, vc);
        self.append_record(&record)?;
        self.file.sync_all()?;
        Ok((lsn, hlc))
    }

    /// Logs a checkpoint record and fsyncs.
    pub fn log_checkpoint(&mut self) -> Result<(u64, Hlc)> {
        let lsn = self.alloc_lsn();
        let hlc = self
            .clock
            .now()
            .map_err(|e| crate::error::GrumpyError::Hlc(e.to_string()))?;
        let vc = VectorClock::singleton(self.node_id, hlc.0);
        let record = WalRecord::checkpoint(lsn, self.node_id, hlc, vc);
        self.append_record(&record)?;
        self.file.sync_all()?;
        Ok((lsn, hlc))
    }

    /// Returns the current (next) LSN without incrementing.
    pub fn current_lsn(&self) -> u64 {
        self.next_lsn
    }

    /// Truncates the WAL file (after a checkpoint). Re-writes the v2
    /// header so the file always starts with the magic.
    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file
            .write_all(&build_wal_header(WAL_VERSION_CURRENT))?;
        self.file.sync_all()?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    /// Reads all valid records from the WAL file.
    pub fn read_all_records(&mut self) -> Result<Vec<WalRecord>> {
        Self::read_v2_records_from_file(&mut self.file)
    }

    fn alloc_lsn(&mut self) -> u64 {
        let lsn = self.next_lsn;
        self.next_lsn += 1;
        lsn
    }

    fn append_record(&mut self, record: &WalRecord) -> Result<()> {
        let bytes = record.to_bytes();
        self.file.write_all(&bytes)?;
        Ok(())
    }

    /// Reads all v2 records, stopping at the first corruption. Skips
    /// the leading 8 KiB header.
    fn read_v2_records_from_file(file: &mut File) -> Result<Vec<WalRecord>> {
        let len = file.metadata()?.len() as usize;
        if len <= WAL_HEADER_SIZE {
            file.seek(SeekFrom::End(0))?;
            return Ok(Vec::new());
        }
        file.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;
        let mut all = Vec::with_capacity(len - WAL_HEADER_SIZE);
        file.read_to_end(&mut all)?;
        let mut records = Vec::new();
        let mut offset = 0usize;
        while offset < all.len() {
            match WalRecord::from_bytes_v2(&all[offset..]) {
                Ok((rec, consumed)) => {
                    records.push(rec);
                    offset += consumed;
                }
                Err(_) => break,
            }
        }
        file.seek(SeekFrom::End(0))?;
        Ok(records)
    }

    /// Reads all v1 records (no header) from a file, used during the
    /// v1 → v2 migration path.
    fn read_v1_records_from_file(file: &mut File, file_len: u64) -> Result<Vec<WalRecord>> {
        file.seek(SeekFrom::Start(0))?;
        let mut all = Vec::with_capacity(file_len as usize);
        file.read_to_end(&mut all)?;
        let mut records = Vec::new();
        let mut offset = 0usize;
        while offset < all.len() {
            match WalRecord::from_bytes_v1(&all[offset..]) {
                Ok((rec, consumed)) => {
                    records.push(rec);
                    offset += consumed;
                }
                Err(_) => break,
            }
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::record::{WAL_MAGIC, WalOpType, encode_v1_record};
    use std::io::Read;
    use tempfile::TempDir;

    fn setup() -> (TempDir, WalWriter) {
        let dir = TempDir::new().unwrap();
        let wal = WalWriter::new(dir.path().join("wal.log")).unwrap();
        (dir, wal)
    }

    #[test]
    fn test_wal_writer_new_empty() {
        let (_dir, wal) = setup();
        assert_eq!(wal.current_lsn(), 1);
        assert_eq!(wal.file_version(), WAL_VERSION_V2);
    }

    #[test]
    fn test_wal_fresh_file_starts_with_magic() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("wal.log");
        let _ = WalWriter::new(&p).unwrap();
        let mut f = std::fs::File::open(&p).unwrap();
        let mut buf = [0u8; 16];
        f.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..8], WAL_MAGIC);
        assert_eq!(u16::from_le_bytes([buf[8], buf[9]]), WAL_VERSION_V2);
    }

    #[test]
    fn test_wal_write_and_read_records() {
        let (_dir, mut wal) = setup();
        let tx = wal.begin_tx();
        wal.log_page_write(tx, 5, &[0; 100], &[1; 100]).unwrap();
        wal.log_commit(tx).unwrap();

        let records = wal.read_all_records().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].op_type, WalOpType::PageWrite);
        assert_eq!(records[0].page_id, 5);
        assert_eq!(records[1].op_type, WalOpType::Commit);
        assert_eq!(records[1].tx_id, tx);
    }

    #[test]
    fn test_wal_lsn_increments() {
        let (_dir, mut wal) = setup();
        let tx = wal.begin_tx();
        let (lsn1, _) = wal.log_page_write(tx, 1, &[], &[]).unwrap();
        let (lsn2, _) = wal.log_page_write(tx, 2, &[], &[]).unwrap();
        let (lsn3, _) = wal.log_commit(tx).unwrap();
        assert_eq!(lsn1, 1);
        assert_eq!(lsn2, 2);
        assert_eq!(lsn3, 3);
    }

    #[test]
    fn test_wal_checkpoint() {
        let (_dir, mut wal) = setup();
        let (lsn, _) = wal.log_checkpoint().unwrap();
        assert_eq!(lsn, 1);
        let records = wal.read_all_records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].op_type, WalOpType::Checkpoint);
    }

    #[test]
    fn test_wal_truncate_keeps_header() {
        let (_dir, mut wal) = setup();
        let tx = wal.begin_tx();
        wal.log_page_write(tx, 1, &[], &[]).unwrap();
        wal.log_commit(tx).unwrap();
        assert_eq!(wal.read_all_records().unwrap().len(), 2);

        wal.truncate().unwrap();
        assert_eq!(wal.read_all_records().unwrap().len(), 0);
        // Header is still present after truncate.
        let mut f = std::fs::File::open(wal.path.clone()).unwrap();
        let mut magic = [0u8; 8];
        f.read_exact(&mut magic).unwrap();
        assert_eq!(&magic, WAL_MAGIC);
    }

    #[test]
    fn test_wal_reopen_resumes_lsn() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wal.log");
        {
            let mut wal = WalWriter::new(&path).unwrap();
            let tx = wal.begin_tx();
            wal.log_page_write(tx, 1, &[0; 50], &[1; 50]).unwrap();
            wal.log_commit(tx).unwrap();
        }
        {
            let mut wal = WalWriter::new(&path).unwrap();
            let (lsn, _) = wal.log_checkpoint().unwrap();
            assert!(lsn >= 3, "LSN should resume: got {lsn}");
            let records = wal.read_all_records().unwrap();
            assert_eq!(records.len(), 3);
        }
    }

    #[test]
    fn test_wal_v1_file_auto_migrates_on_open() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wal.log");

        // Synthesise a v1 file with two records (no header).
        {
            let mut f = std::fs::File::create(&path).unwrap();
            let r1 = encode_v1_record(1, 1, WalOpType::PageWrite, 5, &[0u8; 200]);
            let r2 = encode_v1_record(2, 1, WalOpType::Commit, 0, &[]);
            f.write_all(&r1).unwrap();
            f.write_all(&r2).unwrap();
            f.sync_all().unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() > 0);

        let mut wal = WalWriter::new(&path).unwrap();
        // After open, the file must start with the v2 magic.
        let mut f = std::fs::File::open(&path).unwrap();
        let mut magic = [0u8; 8];
        f.read_exact(&mut magic).unwrap();
        assert_eq!(&magic, WAL_MAGIC);

        let records = wal.read_all_records().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].op_type, WalOpType::PageWrite);
        assert_eq!(records[1].op_type, WalOpType::Commit);
        // Origin should be NIL for migrated v1 records.
        assert_eq!(records[0].origin_node_id, NIL_NODE_ID);
        // Issuing a new write succeeds and the file remains v2.
        let tx = wal.begin_tx();
        wal.log_commit(tx).unwrap();
    }

    #[test]
    fn test_wal_multiple_transactions() {
        let (_dir, mut wal) = setup();
        let tx1 = wal.begin_tx();
        wal.log_page_write(tx1, 1, &[0; 50], &[1; 50]).unwrap();
        wal.log_commit(tx1).unwrap();

        let tx2 = wal.begin_tx();
        wal.log_page_write(tx2, 2, &[0; 50], &[2; 50]).unwrap();

        let records = wal.read_all_records().unwrap();
        assert_eq!(records.len(), 3);
        assert!(
            records
                .iter()
                .any(|r| r.tx_id == tx1 && r.op_type == WalOpType::Commit)
        );
        assert!(
            !records
                .iter()
                .any(|r| r.tx_id == tx2 && r.op_type == WalOpType::Commit)
        );
    }

    #[test]
    fn test_wal_origin_and_hlc_round_trip() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("wal.log");
        let node = 0xdeadbeefcafef00d_u128;
        let clock = Arc::new(HlcClock::new());
        let mut wal = WalWriter::new_with_identity(&p, node, clock.clone()).unwrap();
        let tx = wal.begin_tx();
        let (_, hlc1) = wal.log_page_write(tx, 1, &[1; 8], &[2; 8]).unwrap();
        let (_, hlc2) = wal.log_commit(tx).unwrap();
        assert!(hlc2 > hlc1);
        let records = wal.read_all_records().unwrap();
        for r in &records {
            assert_eq!(r.origin_node_id, node);
            assert!(r.vector_clock.get(node) > 0);
        }
    }
}
