//! WAL writer: append-only log writer with fsync support.
//!
//! The WAL writer appends records sequentially and manages LSN generation.
//! Commit records trigger an fsync to guarantee durability.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::Result;
use crate::wal::record::WalRecord;

/// Append-only WAL writer.
///
/// Writes records sequentially to `wal.log`. Each record is self-contained
/// with its own checksum. The LSN (Log Sequence Number) increases monotonically.
pub struct WalWriter {
    file: File,
    /// Next LSN to assign.
    next_lsn: u64,
    /// Current transaction ID counter.
    next_tx_id: u64,
}

impl WalWriter {
    /// Opens or creates a WAL file.
    ///
    /// If the file exists, scans it to find the highest LSN for resumption.
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let exists = path.exists() && path.metadata()?.len() > 0;

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        let (next_lsn, next_tx_id) = if exists {
            // Scan to find highest LSN and tx_id
            let records = Self::read_all_records_from_file(&mut file)?;
            let max_lsn = records.iter().map(|r| r.lsn).max().unwrap_or(0);
            let max_tx = records.iter().map(|r| r.tx_id).max().unwrap_or(0);
            // Seek to end for appending
            file.seek(SeekFrom::End(0))?;
            (max_lsn + 1, max_tx + 1)
        } else {
            (1, 1)
        };

        Ok(Self {
            file,
            next_lsn,
            next_tx_id,
        })
    }

    /// Begins a new transaction. Returns the transaction ID.
    pub fn begin_tx(&mut self) -> u64 {
        let tx_id = self.next_tx_id;
        self.next_tx_id += 1;
        tx_id
    }

    /// Logs a page write (before + after images).
    ///
    /// Does NOT fsync — the record is buffered until commit.
    pub fn log_page_write(
        &mut self,
        tx_id: u64,
        page_id: u32,
        before: &[u8],
        after: &[u8],
    ) -> Result<u64> {
        let lsn = self.alloc_lsn();
        let record = WalRecord::page_write(lsn, tx_id, page_id, before, after);
        self.append_record(&record)?;
        Ok(lsn)
    }

    /// Logs a commit record and fsyncs the WAL.
    ///
    /// After this returns, the transaction is guaranteed durable.
    pub fn log_commit(&mut self, tx_id: u64) -> Result<u64> {
        let lsn = self.alloc_lsn();
        let record = WalRecord::commit(lsn, tx_id);
        self.append_record(&record)?;
        // fsync is CRITICAL here — commit must be durable before returning.
        self.file.sync_all()?;
        Ok(lsn)
    }

    /// Logs a checkpoint record and fsyncs.
    pub fn log_checkpoint(&mut self) -> Result<u64> {
        let lsn = self.alloc_lsn();
        let record = WalRecord::checkpoint(lsn);
        self.append_record(&record)?;
        self.file.sync_all()?;
        Ok(lsn)
    }

    /// Returns the current (next) LSN without incrementing.
    pub fn current_lsn(&self) -> u64 {
        self.next_lsn
    }

    /// Truncates the WAL file (after a checkpoint).
    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Reads all valid records from the WAL file.
    pub fn read_all_records(&mut self) -> Result<Vec<WalRecord>> {
        self.file.seek(SeekFrom::Start(0))?;
        Self::read_all_records_from_file(&mut self.file)
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

    /// Reads all valid records, stopping at first corruption.
    fn read_all_records_from_file(file: &mut File) -> Result<Vec<WalRecord>> {
        file.seek(SeekFrom::Start(0))?;
        let mut all_bytes = Vec::new();
        file.read_to_end(&mut all_bytes)?;

        let mut records = Vec::new();
        let mut offset = 0;

        while offset < all_bytes.len() {
            match WalRecord::from_bytes(&all_bytes[offset..]) {
                Ok((record, consumed)) => {
                    records.push(record);
                    offset += consumed;
                }
                Err(_) => {
                    // Corrupted or incomplete record — stop reading.
                    break;
                }
            }
        }

        // Seek back to end for future appends
        file.seek(SeekFrom::End(0))?;
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::record::WalOpType;
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
        let lsn1 = wal.log_page_write(tx, 1, &[], &[]).unwrap();
        let lsn2 = wal.log_page_write(tx, 2, &[], &[]).unwrap();
        let lsn3 = wal.log_commit(tx).unwrap();
        assert_eq!(lsn1, 1);
        assert_eq!(lsn2, 2);
        assert_eq!(lsn3, 3);
    }

    #[test]
    fn test_wal_checkpoint() {
        let (_dir, mut wal) = setup();
        let lsn = wal.log_checkpoint().unwrap();
        assert_eq!(lsn, 1);
        let records = wal.read_all_records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].op_type, WalOpType::Checkpoint);
    }

    #[test]
    fn test_wal_truncate() {
        let (_dir, mut wal) = setup();
        let tx = wal.begin_tx();
        wal.log_page_write(tx, 1, &[], &[]).unwrap();
        wal.log_commit(tx).unwrap();
        assert_eq!(wal.read_all_records().unwrap().len(), 2);

        wal.truncate().unwrap();
        assert_eq!(wal.read_all_records().unwrap().len(), 0);
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
            // LSN should resume after the last one (2 records → next LSN = 3)
            let lsn = wal.log_checkpoint().unwrap();
            assert!(lsn >= 3, "LSN should resume: got {lsn}");

            let records = wal.read_all_records().unwrap();
            assert_eq!(records.len(), 3);
        }
    }

    #[test]
    fn test_wal_multiple_transactions() {
        let (_dir, mut wal) = setup();

        let tx1 = wal.begin_tx();
        wal.log_page_write(tx1, 1, &[0; 50], &[1; 50]).unwrap();
        wal.log_commit(tx1).unwrap();

        let tx2 = wal.begin_tx();
        wal.log_page_write(tx2, 2, &[0; 50], &[2; 50]).unwrap();
        // tx2 not committed

        let records = wal.read_all_records().unwrap();
        assert_eq!(records.len(), 3); // pw1, commit1, pw2
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
}
