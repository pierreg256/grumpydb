//! WAL record types and binary serialization (format v1 + v2).
//!
//! ## Format v1 (legacy)
//!
//! Each record has a fixed 33-byte header followed by variable-length data
//! and a CRC32 trailer. The on-disk layout is:
//!
//! ```text
//! [0..4]   record_len: u32 (LE)
//! [4..12]  lsn:        u64 (LE)
//! [12..20] tx_id:      u64 (LE)
//! [20]     op_type:    u8
//! [21..25] page_id:    u32 (LE)
//! [25..29] data_len:   u32 (LE)
//! [29..29+data_len]    payload (before+after for PageWrite)
//! [..+4]   checksum:   u32 (LE)
//! ```
//!
//! ## Format v2 (current)
//!
//! v2 prepends a single 8 KiB **WAL header page** to the log carrying a
//! magic and version number. Records are framed as:
//!
//! ```text
//! [0..4]    record_len: u32 (LE)  — total bytes including this field
//! [4..12]   lsn:        u64 (LE)
//! [12..20]  tx_id:      u64 (LE)
//! [20]      op_type:    u8
//! [21..37]  origin_node_id: u128 (LE)
//! [37..45]  hlc:        u64 (LE)
//! [45..47]  vclock_len: u16 (LE)
//! [47..47+vclock_len*24]   vector clock entries (each: u128+u64)
//! [..]      op-specific payload
//! [end-4..end] checksum: u32 (LE) — CRC32 of bytes 4..end-4
//! ```
//!
//! For `PageWrite`, the op-specific payload is identical to v1:
//! `page_id (u32 LE) + data_len (u32 LE) + after (data_len bytes) + before (data_len bytes)`.
//!
//! Records read from a v1 file are mapped to in-memory v2 records with
//! `origin_node_id = NIL_NODE_ID`, `hlc = Hlc::from_packed(lsn)`, and a
//! singleton vector clock keyed by `NIL_NODE_ID`.

use crate::error::{GrumpyError, Result};
use crate::page::PAGE_SIZE;
use crate::wal::hlc::Hlc;
use crate::wal::vclock::VectorClock;

/// WAL log file magic, present at the start of every v2 file.
pub const WAL_MAGIC: &[u8; 8] = b"GRUMPWAL";

/// WAL format v1 (pre-Phase 40b — no header, no HLC, no vclock).
pub const WAL_VERSION_V1: u16 = 1;
/// WAL format v2 (Phase 40b — adds origin_node_id + HLC + vector clock).
pub const WAL_VERSION_V2: u16 = 2;
/// Highest format version this build can write.
pub const WAL_VERSION_CURRENT: u16 = WAL_VERSION_V2;

/// Size of the v2 WAL header page in bytes (one full page reserved for
/// future expansion).
pub const WAL_HEADER_SIZE: usize = PAGE_SIZE;

/// All-zero UUID. Used as `origin_node_id` for v1 records that have been
/// auto-promoted to the v2 in-memory shape.
pub const NIL_NODE_ID: u128 = 0;

/// WAL operation types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WalOpType {
    /// Page modification: payload contains `after` then `before` images.
    PageWrite = 1,
    /// Transaction committed (durable).
    Commit = 2,
    /// Transaction rolled back.
    Rollback = 3,
    /// Checkpoint: all dirty pages flushed.
    Checkpoint = 4,
}

impl WalOpType {
    /// Parse from a raw byte.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::PageWrite),
            2 => Some(Self::Commit),
            3 => Some(Self::Rollback),
            4 => Some(Self::Checkpoint),
            _ => None,
        }
    }
}

/// Fixed v1 header size in bytes (excluding payload and checksum).
/// `record_len(4) + lsn(8) + tx_id(8) + op_type(1) + page_id(4) + data_len(4) + checksum(4) = 33`.
pub const WAL_RECORD_HEADER_SIZE_V1: usize = 33;

/// Fixed v2 header size in bytes (excluding the variable-length vector
/// clock — its `vclock_len` prefix and entries are counted by
/// [`super::vclock::VectorClock::encoded_len`] — the op-specific payload,
/// and the trailing checksum).
///
/// `record_len(4) + lsn(8) + tx_id(8) + op_type(1) + origin(16) + hlc(8) + checksum(4) = 49`.
pub const WAL_RECORD_HEADER_SIZE_V2: usize = 49;

/// A single WAL record (in-memory representation; always carries v2 fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    /// Total size of this record on disk (including the leading
    /// `record_len` field and the trailing checksum).
    pub record_len: u32,
    /// Log Sequence Number — monotonically increasing per WAL file.
    pub lsn: u64,
    /// Transaction identifier.
    pub tx_id: u64,
    /// Operation type.
    pub op_type: WalOpType,
    /// Identifier of the node that produced this record. `NIL_NODE_ID`
    /// for records read from a v1 file.
    pub origin_node_id: u128,
    /// Hybrid Logical Clock at the time the record was produced.
    pub hlc: Hlc,
    /// Vector clock snapshot at the time the record was produced.
    pub vector_clock: VectorClock,
    /// Page ID affected (0 for Commit/Checkpoint).
    pub page_id: u32,
    /// Op-specific payload (PageWrite: `after ++ before` for v2;
    /// historical v1 files used `before ++ after`, which is preserved
    /// byte-for-byte during decode and treated symmetrically by the
    /// recovery code).
    pub data: Vec<u8>,
    /// CRC32 checksum stored on disk.
    pub checksum: u32,
}

impl WalRecord {
    /// Constructs a `PageWrite` v2 record.
    #[allow(clippy::too_many_arguments)]
    pub fn page_write(
        lsn: u64,
        tx_id: u64,
        origin_node_id: u128,
        hlc: Hlc,
        vector_clock: VectorClock,
        page_id: u32,
        before: &[u8],
        after: &[u8],
    ) -> Self {
        // v2 stores after first then before. The recovery code accesses
        // both halves explicitly via `after_image()` / `before_image()`.
        let mut data = Vec::with_capacity(after.len() + before.len());
        data.extend_from_slice(after);
        data.extend_from_slice(before);
        let mut rec = Self {
            record_len: 0,
            lsn,
            tx_id,
            op_type: WalOpType::PageWrite,
            origin_node_id,
            hlc,
            vector_clock,
            page_id,
            data,
            checksum: 0,
        };
        rec.record_len = rec.encoded_v2_len() as u32;
        rec.checksum = rec.compute_checksum_v2();
        rec
    }

    /// Constructs a `Commit` v2 record.
    pub fn commit(
        lsn: u64,
        tx_id: u64,
        origin_node_id: u128,
        hlc: Hlc,
        vector_clock: VectorClock,
    ) -> Self {
        let mut rec = Self {
            record_len: 0,
            lsn,
            tx_id,
            op_type: WalOpType::Commit,
            origin_node_id,
            hlc,
            vector_clock,
            page_id: 0,
            data: Vec::new(),
            checksum: 0,
        };
        rec.record_len = rec.encoded_v2_len() as u32;
        rec.checksum = rec.compute_checksum_v2();
        rec
    }

    /// Constructs a `Checkpoint` v2 record.
    pub fn checkpoint(lsn: u64, origin_node_id: u128, hlc: Hlc, vector_clock: VectorClock) -> Self {
        let mut rec = Self {
            record_len: 0,
            lsn,
            tx_id: 0,
            op_type: WalOpType::Checkpoint,
            origin_node_id,
            hlc,
            vector_clock,
            page_id: 0,
            data: Vec::new(),
            checksum: 0,
        };
        rec.record_len = rec.encoded_v2_len() as u32;
        rec.checksum = rec.compute_checksum_v2();
        rec
    }

    /// Total v2 on-disk frame length, including `record_len` and `checksum`.
    pub fn encoded_v2_len(&self) -> usize {
        let payload = match self.op_type {
            WalOpType::PageWrite => 4 + 4 + self.data.len(), // page_id + data_len + payload
            _ => 0,
        };
        WAL_RECORD_HEADER_SIZE_V2 + self.vector_clock.encoded_len() + payload
    }

    /// Serialises the record in v2 format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let total = self.encoded_v2_len();
        let mut buf = Vec::with_capacity(total);
        buf.extend_from_slice(&self.record_len.to_le_bytes());
        buf.extend_from_slice(&self.lsn.to_le_bytes());
        buf.extend_from_slice(&self.tx_id.to_le_bytes());
        buf.push(self.op_type as u8);
        buf.extend_from_slice(&self.origin_node_id.to_le_bytes());
        buf.extend_from_slice(&self.hlc.to_le_bytes());
        self.vector_clock.encode_to(&mut buf);
        if self.op_type == WalOpType::PageWrite {
            buf.extend_from_slice(&self.page_id.to_le_bytes());
            buf.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
            buf.extend_from_slice(&self.data);
        }
        buf.extend_from_slice(&self.checksum.to_le_bytes());
        debug_assert_eq!(buf.len(), total);
        buf
    }

    /// Deserialises a record assuming the v2 frame layout. Returns the
    /// record and the number of bytes consumed.
    pub fn from_bytes_v2(buf: &[u8]) -> Result<(Self, usize)> {
        if buf.len() < 4 {
            return Err(GrumpyError::WalCorrupted(0));
        }
        let record_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if record_len < WAL_RECORD_HEADER_SIZE_V2 || buf.len() < record_len {
            return Err(GrumpyError::WalCorrupted(0));
        }

        let lsn = u64::from_le_bytes([
            buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
        ]);
        let tx_id = u64::from_le_bytes([
            buf[12], buf[13], buf[14], buf[15], buf[16], buf[17], buf[18], buf[19],
        ]);
        let op_type = WalOpType::from_u8(buf[20]).ok_or(GrumpyError::WalCorrupted(lsn))?;

        let mut origin_bytes = [0u8; 16];
        origin_bytes.copy_from_slice(&buf[21..37]);
        let origin_node_id = u128::from_le_bytes(origin_bytes);

        let mut hlc_bytes = [0u8; 8];
        hlc_bytes.copy_from_slice(&buf[37..45]);
        let hlc = Hlc::from_le_bytes(hlc_bytes);

        let payload_end = record_len.saturating_sub(4);
        let (vector_clock, vc_consumed) = VectorClock::decode(&buf[45..payload_end])
            .map_err(|e| GrumpyError::VectorClock(e.to_string()))?;
        let mut cursor = 45 + vc_consumed;

        let (page_id, data) = if op_type == WalOpType::PageWrite {
            if cursor + 8 > payload_end {
                return Err(GrumpyError::WalCorrupted(lsn));
            }
            let page_id = u32::from_le_bytes([
                buf[cursor],
                buf[cursor + 1],
                buf[cursor + 2],
                buf[cursor + 3],
            ]);
            cursor += 4;
            let data_len = u32::from_le_bytes([
                buf[cursor],
                buf[cursor + 1],
                buf[cursor + 2],
                buf[cursor + 3],
            ]) as usize;
            cursor += 4;
            if cursor + data_len > payload_end {
                return Err(GrumpyError::WalCorrupted(lsn));
            }
            let data = buf[cursor..cursor + data_len].to_vec();
            cursor += data_len;
            (page_id, data)
        } else {
            (0u32, Vec::new())
        };

        if cursor != payload_end {
            return Err(GrumpyError::WalCorrupted(lsn));
        }
        let checksum = u32::from_le_bytes([
            buf[cursor],
            buf[cursor + 1],
            buf[cursor + 2],
            buf[cursor + 3],
        ]);

        let rec = Self {
            record_len: record_len as u32,
            lsn,
            tx_id,
            op_type,
            origin_node_id,
            hlc,
            vector_clock,
            page_id,
            data,
            checksum,
        };
        if rec.compute_checksum_v2() != checksum {
            return Err(GrumpyError::WalCorrupted(lsn));
        }
        Ok((rec, record_len))
    }

    /// Deserialises a record assuming the legacy v1 frame layout. The
    /// returned record is mapped to the v2 in-memory shape (NIL origin,
    /// `hlc = Hlc::from_packed(lsn)`, singleton vclock keyed by NIL).
    pub fn from_bytes_v1(buf: &[u8]) -> Result<(Self, usize)> {
        if buf.len() < WAL_RECORD_HEADER_SIZE_V1 {
            return Err(GrumpyError::WalCorrupted(0));
        }
        let record_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if record_len < WAL_RECORD_HEADER_SIZE_V1 || buf.len() < record_len {
            return Err(GrumpyError::WalCorrupted(0));
        }

        let lsn = u64::from_le_bytes([
            buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
        ]);
        let tx_id = u64::from_le_bytes([
            buf[12], buf[13], buf[14], buf[15], buf[16], buf[17], buf[18], buf[19],
        ]);
        let op_type = WalOpType::from_u8(buf[20]).ok_or(GrumpyError::WalCorrupted(lsn))?;
        let page_id = u32::from_le_bytes([buf[21], buf[22], buf[23], buf[24]]);
        let data_len = u32::from_le_bytes([buf[25], buf[26], buf[27], buf[28]]) as usize;

        if 29 + data_len + 4 > record_len {
            return Err(GrumpyError::WalCorrupted(lsn));
        }
        let data = buf[29..29 + data_len].to_vec();
        let checksum_off = 29 + data_len;
        let checksum = u32::from_le_bytes([
            buf[checksum_off],
            buf[checksum_off + 1],
            buf[checksum_off + 2],
            buf[checksum_off + 3],
        ]);

        let computed = compute_v1_checksum(record_len as u32, lsn, tx_id, op_type, page_id, &data);
        if computed != checksum {
            return Err(GrumpyError::WalCorrupted(lsn));
        }

        let hlc = Hlc::from_packed(lsn);
        let vector_clock = VectorClock::singleton(NIL_NODE_ID, lsn);
        let mut rec = Self {
            record_len: 0,
            lsn,
            tx_id,
            op_type,
            origin_node_id: NIL_NODE_ID,
            hlc,
            vector_clock,
            page_id,
            data,
            checksum: 0,
        };
        // Re-encode in v2: the in-memory representation is uniformly v2.
        rec.record_len = rec.encoded_v2_len() as u32;
        rec.checksum = rec.compute_checksum_v2();
        Ok((rec, record_len))
    }

    /// Convenience accessor: for `PageWrite` records, returns the
    /// `(after, before)` slice pair (v2 layout).
    pub fn page_images(&self) -> Option<(&[u8], &[u8])> {
        if self.op_type != WalOpType::PageWrite {
            return None;
        }
        let half = self.data.len() / 2;
        Some((&self.data[..half], &self.data[half..]))
    }

    /// Returns the after-image slice for `PageWrite`.
    pub fn after_image(&self) -> Option<&[u8]> {
        self.page_images().map(|(a, _)| a)
    }

    /// Returns the before-image slice for `PageWrite`.
    pub fn before_image(&self) -> Option<&[u8]> {
        self.page_images().map(|(_, b)| b)
    }

    /// Recomputes and validates the on-disk checksum (v2).
    pub fn is_valid(&self) -> bool {
        self.compute_checksum_v2() == self.checksum
    }

    fn compute_checksum_v2(&self) -> u32 {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&self.lsn.to_le_bytes());
        hasher.update(&self.tx_id.to_le_bytes());
        hasher.update(&[self.op_type as u8]);
        hasher.update(&self.origin_node_id.to_le_bytes());
        hasher.update(&self.hlc.to_le_bytes());
        let mut vc_buf = Vec::with_capacity(self.vector_clock.encoded_len());
        self.vector_clock.encode_to(&mut vc_buf);
        hasher.update(&vc_buf);
        if self.op_type == WalOpType::PageWrite {
            hasher.update(&self.page_id.to_le_bytes());
            hasher.update(&(self.data.len() as u32).to_le_bytes());
            hasher.update(&self.data);
        }
        hasher.finalize()
    }
}

fn compute_v1_checksum(
    record_len: u32,
    lsn: u64,
    tx_id: u64,
    op_type: WalOpType,
    page_id: u32,
    data: &[u8],
) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&record_len.to_le_bytes());
    hasher.update(&lsn.to_le_bytes());
    hasher.update(&tx_id.to_le_bytes());
    hasher.update(&[op_type as u8]);
    hasher.update(&page_id.to_le_bytes());
    hasher.update(&(data.len() as u32).to_le_bytes());
    hasher.update(data);
    hasher.finalize()
}

/// Encodes a v1-format record from raw fields. Used by the migration
/// test harness to synthesise legacy WAL files.
#[doc(hidden)]
pub fn encode_v1_record(
    lsn: u64,
    tx_id: u64,
    op_type: WalOpType,
    page_id: u32,
    data: &[u8],
) -> Vec<u8> {
    let record_len = (WAL_RECORD_HEADER_SIZE_V1 + data.len()) as u32;
    let checksum = compute_v1_checksum(record_len, lsn, tx_id, op_type, page_id, data);
    let mut buf = Vec::with_capacity(record_len as usize);
    buf.extend_from_slice(&record_len.to_le_bytes());
    buf.extend_from_slice(&lsn.to_le_bytes());
    buf.extend_from_slice(&tx_id.to_le_bytes());
    buf.push(op_type as u8);
    buf.extend_from_slice(&page_id.to_le_bytes());
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(data);
    buf.extend_from_slice(&checksum.to_le_bytes());
    buf
}

/// Builds the 8 KiB v2 WAL header page.
pub fn build_wal_header(version: u16) -> [u8; WAL_HEADER_SIZE] {
    let mut hdr = [0u8; WAL_HEADER_SIZE];
    hdr[..8].copy_from_slice(WAL_MAGIC);
    hdr[8..10].copy_from_slice(&version.to_le_bytes());
    // bytes 10..18 are reserved (zero in v2).
    hdr
}

/// Parses an 8 KiB WAL header page. Returns `Ok(version)` if the magic
/// matches, `Ok(None)`-equivalent (handled by caller as "v1 / no header")
/// is signalled by returning `Err(GrumpyError::WalCorrupted(0))` here —
/// callers should branch on magic detection before calling this.
pub fn parse_wal_header(hdr: &[u8]) -> Result<u16> {
    if hdr.len() < 10 {
        return Err(GrumpyError::WalCorrupted(0));
    }
    if &hdr[..8] != WAL_MAGIC {
        return Err(GrumpyError::WalCorrupted(0));
    }
    let version = u16::from_le_bytes([hdr[8], hdr[9]]);
    if version > WAL_VERSION_CURRENT {
        return Err(GrumpyError::UnsupportedWalVersion(version));
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_origin() -> u128 {
        0x0123_4567_89ab_cdef_0123_4567_89ab_cdef
    }

    #[test]
    fn test_v2_commit_round_trip() {
        let rec = WalRecord::commit(
            1,
            42,
            dummy_origin(),
            Hlc::pack(1_700_000_000_000, 3),
            VectorClock::singleton(dummy_origin(), 1),
        );
        let bytes = rec.to_bytes();
        let (decoded, consumed) = WalRecord::from_bytes_v2(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded, rec);
        assert!(decoded.is_valid());
    }

    #[test]
    fn test_v2_checkpoint_round_trip() {
        let rec = WalRecord::checkpoint(
            100,
            dummy_origin(),
            Hlc::pack(1_700_000_000_000, 0),
            VectorClock::new(),
        );
        let bytes = rec.to_bytes();
        let (decoded, _) = WalRecord::from_bytes_v2(&bytes).unwrap();
        assert_eq!(decoded, rec);
    }

    #[test]
    fn test_v2_encode_decode_round_trip() {
        let before = vec![0xAA; 8192];
        let after = vec![0xBB; 8192];
        let rec = WalRecord::page_write(
            5,
            1,
            dummy_origin(),
            Hlc::pack(1, 2),
            VectorClock::singleton(dummy_origin(), 7),
            42,
            &before,
            &after,
        );
        let bytes = rec.to_bytes();
        let (decoded, _) = WalRecord::from_bytes_v2(&bytes).unwrap();
        assert_eq!(decoded.lsn, 5);
        assert_eq!(decoded.page_id, 42);
        assert_eq!(decoded.after_image().unwrap(), after.as_slice());
        assert_eq!(decoded.before_image().unwrap(), before.as_slice());
        assert_eq!(decoded.origin_node_id, dummy_origin());
        assert_eq!(decoded.hlc, Hlc::pack(1, 2));
    }

    #[test]
    fn test_v2_record_with_vclock() {
        let mut vc = VectorClock::new();
        vc.set(1, 100);
        vc.set(2, 200);
        vc.set(3, 300);
        let rec = WalRecord::commit(7, 7, dummy_origin(), Hlc::pack(99, 1), vc.clone());
        let bytes = rec.to_bytes();
        let (decoded, _) = WalRecord::from_bytes_v2(&bytes).unwrap();
        assert_eq!(decoded.vector_clock, vc);
    }

    #[test]
    fn test_v1_record_decoded_as_v2() {
        let v1 = encode_v1_record(11, 3, WalOpType::Commit, 0, &[]);
        let (rec, consumed) = WalRecord::from_bytes_v1(&v1).unwrap();
        assert_eq!(consumed, v1.len());
        assert_eq!(rec.lsn, 11);
        assert_eq!(rec.tx_id, 3);
        assert_eq!(rec.op_type, WalOpType::Commit);
        assert_eq!(rec.origin_node_id, NIL_NODE_ID);
        assert_eq!(rec.hlc, Hlc::from_packed(11));
        assert_eq!(rec.vector_clock, VectorClock::singleton(NIL_NODE_ID, 11));
    }

    #[test]
    fn test_v1_page_write_decoded_as_v2() {
        let before = vec![1u8; 64];
        let after = vec![2u8; 64];
        let mut payload = Vec::new();
        payload.extend_from_slice(&before);
        payload.extend_from_slice(&after);
        let v1 = encode_v1_record(5, 2, WalOpType::PageWrite, 99, &payload);
        let (rec, _) = WalRecord::from_bytes_v1(&v1).unwrap();
        assert_eq!(rec.op_type, WalOpType::PageWrite);
        assert_eq!(rec.page_id, 99);
        // v1 stored before++after; we keep the bytes verbatim. The
        // recovery code reads the two halves symmetrically so the
        // ordering convention only matters when calling
        // page_images/before_image/after_image, which always interpret
        // the v2 layout (after first, then before).
        assert_eq!(&rec.data[..64], before.as_slice());
        assert_eq!(&rec.data[64..], after.as_slice());
        let bytes = rec.to_bytes();
        let (re, _) = WalRecord::from_bytes_v2(&bytes).unwrap();
        assert_eq!(re, rec);
    }

    #[test]
    fn test_v2_record_checksum_mismatch_returns_error() {
        let rec = WalRecord::commit(1, 1, dummy_origin(), Hlc::pack(1, 0), VectorClock::new());
        let mut bytes = rec.to_bytes();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        assert!(WalRecord::from_bytes_v2(&bytes).is_err());
    }

    #[test]
    fn test_truncated_v2_record_detected() {
        let rec = WalRecord::commit(1, 1, dummy_origin(), Hlc::pack(1, 0), VectorClock::new());
        let bytes = rec.to_bytes();
        assert!(WalRecord::from_bytes_v2(&bytes[..10]).is_err());
    }

    #[test]
    fn test_op_type_from_u8() {
        assert_eq!(WalOpType::from_u8(1), Some(WalOpType::PageWrite));
        assert_eq!(WalOpType::from_u8(2), Some(WalOpType::Commit));
        assert_eq!(WalOpType::from_u8(4), Some(WalOpType::Checkpoint));
        assert_eq!(WalOpType::from_u8(99), None);
    }

    #[test]
    fn test_v2_multiple_records_sequential() {
        let r1 = WalRecord::page_write(
            1,
            1,
            dummy_origin(),
            Hlc::pack(1, 0),
            VectorClock::new(),
            5,
            &[0; 100],
            &[1; 100],
        );
        let r2 = WalRecord::commit(2, 1, dummy_origin(), Hlc::pack(2, 0), VectorClock::new());
        let r3 = WalRecord::checkpoint(3, dummy_origin(), Hlc::pack(3, 0), VectorClock::new());

        let mut buf = Vec::new();
        buf.extend_from_slice(&r1.to_bytes());
        buf.extend_from_slice(&r2.to_bytes());
        buf.extend_from_slice(&r3.to_bytes());

        let (d1, c1) = WalRecord::from_bytes_v2(&buf).unwrap();
        let (d2, c2) = WalRecord::from_bytes_v2(&buf[c1..]).unwrap();
        let (d3, _) = WalRecord::from_bytes_v2(&buf[c1 + c2..]).unwrap();

        assert_eq!(d1.lsn, 1);
        assert_eq!(d2.op_type, WalOpType::Commit);
        assert_eq!(d3.op_type, WalOpType::Checkpoint);
    }

    #[test]
    fn test_build_and_parse_wal_header() {
        let hdr = build_wal_header(WAL_VERSION_V2);
        assert_eq!(&hdr[..8], WAL_MAGIC);
        assert_eq!(u16::from_le_bytes([hdr[8], hdr[9]]), WAL_VERSION_V2);
        for b in &hdr[10..18] {
            assert_eq!(*b, 0);
        }
        assert_eq!(hdr.len(), WAL_HEADER_SIZE);
        assert_eq!(parse_wal_header(&hdr).unwrap(), WAL_VERSION_V2);
    }

    #[test]
    fn test_parse_wal_header_rejects_unknown_version() {
        let mut hdr = build_wal_header(99);
        hdr[8] = 99;
        hdr[9] = 0;
        assert!(matches!(
            parse_wal_header(&hdr),
            Err(GrumpyError::UnsupportedWalVersion(99))
        ));
    }
}
