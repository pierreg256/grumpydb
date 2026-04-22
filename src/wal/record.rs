//! WAL record types and binary serialization.
//!
//! Each WAL record has a fixed header (33 bytes) followed by variable-length data.
//! Records are checksummed with CRC32 to detect corruption.

use crate::error::{GrumpyError, Result};

/// WAL operation types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WalOpType {
    /// Page modification: data contains before_image ++ after_image.
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

/// Fixed header size in bytes (excluding variable data and checksum).
/// record_len(4) + lsn(8) + tx_id(8) + op_type(1) + page_id(4) + data_len(4) + checksum(4) = 33
pub const WAL_RECORD_HEADER_SIZE: usize = 33;

/// A single WAL record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    /// Total size of this record on disk (header + data).
    pub record_len: u32,
    /// Log Sequence Number — monotonically increasing.
    pub lsn: u64,
    /// Transaction identifier.
    pub tx_id: u64,
    /// Operation type.
    pub op_type: WalOpType,
    /// Page ID affected (0 for Commit/Checkpoint).
    pub page_id: u32,
    /// Payload: before+after images for PageWrite, empty for others.
    pub data: Vec<u8>,
    /// CRC32 checksum of all fields except checksum itself.
    pub checksum: u32,
}

impl WalRecord {
    /// Creates a new PageWrite record.
    pub fn page_write(lsn: u64, tx_id: u64, page_id: u32, before: &[u8], after: &[u8]) -> Self {
        let mut data = Vec::with_capacity(before.len() + after.len());
        data.extend_from_slice(before);
        data.extend_from_slice(after);
        let mut rec = Self {
            record_len: (WAL_RECORD_HEADER_SIZE + data.len()) as u32,
            lsn,
            tx_id,
            op_type: WalOpType::PageWrite,
            page_id,
            data,
            checksum: 0,
        };
        rec.checksum = rec.compute_checksum();
        rec
    }

    /// Creates a Commit record.
    pub fn commit(lsn: u64, tx_id: u64) -> Self {
        let mut rec = Self {
            record_len: WAL_RECORD_HEADER_SIZE as u32,
            lsn,
            tx_id,
            op_type: WalOpType::Commit,
            page_id: 0,
            data: Vec::new(),
            checksum: 0,
        };
        rec.checksum = rec.compute_checksum();
        rec
    }

    /// Creates a Checkpoint record.
    pub fn checkpoint(lsn: u64) -> Self {
        let mut rec = Self {
            record_len: WAL_RECORD_HEADER_SIZE as u32,
            lsn,
            tx_id: 0,
            op_type: WalOpType::Checkpoint,
            page_id: 0,
            data: Vec::new(),
            checksum: 0,
        };
        rec.checksum = rec.compute_checksum();
        rec
    }

    /// Serializes the record to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.record_len as usize);
        buf.extend_from_slice(&self.record_len.to_le_bytes());
        buf.extend_from_slice(&self.lsn.to_le_bytes());
        buf.extend_from_slice(&self.tx_id.to_le_bytes());
        buf.push(self.op_type as u8);
        buf.extend_from_slice(&self.page_id.to_le_bytes());
        buf.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.data);
        buf.extend_from_slice(&self.checksum.to_le_bytes());
        buf
    }

    /// Deserializes a record from bytes. Returns `(record, bytes_consumed)`.
    pub fn from_bytes(buf: &[u8]) -> Result<(Self, usize)> {
        if buf.len() < WAL_RECORD_HEADER_SIZE {
            return Err(GrumpyError::WalCorrupted(0));
        }

        let record_len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        if buf.len() < record_len {
            return Err(GrumpyError::WalCorrupted(0));
        }

        let lsn = u64::from_le_bytes(buf[4..12].try_into().unwrap());
        let tx_id = u64::from_le_bytes(buf[12..20].try_into().unwrap());
        let op_type = WalOpType::from_u8(buf[20]).ok_or(GrumpyError::WalCorrupted(lsn))?;
        let page_id = u32::from_le_bytes(buf[21..25].try_into().unwrap());
        let data_len = u32::from_le_bytes(buf[25..29].try_into().unwrap()) as usize;

        if 29 + data_len + 4 > record_len {
            return Err(GrumpyError::WalCorrupted(lsn));
        }

        let data = buf[29..29 + data_len].to_vec();
        let checksum =
            u32::from_le_bytes(buf[29 + data_len..29 + data_len + 4].try_into().unwrap());

        let rec = Self {
            record_len: record_len as u32,
            lsn,
            tx_id,
            op_type,
            page_id,
            data,
            checksum,
        };

        // Verify checksum
        if rec.compute_checksum() != checksum {
            return Err(GrumpyError::WalCorrupted(lsn));
        }

        Ok((rec, record_len))
    }

    /// Computes CRC32 checksum over all fields except checksum.
    fn compute_checksum(&self) -> u32 {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&self.record_len.to_le_bytes());
        hasher.update(&self.lsn.to_le_bytes());
        hasher.update(&self.tx_id.to_le_bytes());
        hasher.update(&[self.op_type as u8]);
        hasher.update(&self.page_id.to_le_bytes());
        hasher.update(&(self.data.len() as u32).to_le_bytes());
        hasher.update(&self.data);
        hasher.finalize()
    }

    /// Validates the record's checksum.
    pub fn is_valid(&self) -> bool {
        self.compute_checksum() == self.checksum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_commit_record_round_trip() {
        let rec = WalRecord::commit(1, 42);
        let bytes = rec.to_bytes();
        let (decoded, consumed) = WalRecord::from_bytes(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded, rec);
        assert!(decoded.is_valid());
    }

    #[test]
    fn test_checkpoint_record_round_trip() {
        let rec = WalRecord::checkpoint(100);
        let bytes = rec.to_bytes();
        let (decoded, _) = WalRecord::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, rec);
    }

    #[test]
    fn test_page_write_record_round_trip() {
        let before = vec![0xAA; 8192];
        let after = vec![0xBB; 8192];
        let rec = WalRecord::page_write(5, 1, 42, &before, &after);
        let bytes = rec.to_bytes();
        assert_eq!(bytes.len(), WAL_RECORD_HEADER_SIZE + 8192 * 2);
        let (decoded, _) = WalRecord::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.lsn, 5);
        assert_eq!(decoded.page_id, 42);
        assert_eq!(&decoded.data[..8192], before.as_slice());
        assert_eq!(&decoded.data[8192..], after.as_slice());
    }

    #[test]
    fn test_corrupted_checksum_detected() {
        let rec = WalRecord::commit(1, 1);
        let mut bytes = rec.to_bytes();
        // Corrupt the last byte (checksum)
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let result = WalRecord::from_bytes(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_truncated_record_detected() {
        let rec = WalRecord::commit(1, 1);
        let bytes = rec.to_bytes();
        let result = WalRecord::from_bytes(&bytes[..10]);
        assert!(result.is_err());
    }

    #[test]
    fn test_op_type_from_u8() {
        assert_eq!(WalOpType::from_u8(1), Some(WalOpType::PageWrite));
        assert_eq!(WalOpType::from_u8(2), Some(WalOpType::Commit));
        assert_eq!(WalOpType::from_u8(4), Some(WalOpType::Checkpoint));
        assert_eq!(WalOpType::from_u8(99), None);
    }

    #[test]
    fn test_multiple_records_sequential() {
        let r1 = WalRecord::page_write(1, 1, 5, &[0; 100], &[1; 100]);
        let r2 = WalRecord::commit(2, 1);
        let r3 = WalRecord::checkpoint(3);

        let mut buf = Vec::new();
        buf.extend_from_slice(&r1.to_bytes());
        buf.extend_from_slice(&r2.to_bytes());
        buf.extend_from_slice(&r3.to_bytes());

        let (d1, c1) = WalRecord::from_bytes(&buf).unwrap();
        let (d2, c2) = WalRecord::from_bytes(&buf[c1..]).unwrap();
        let (d3, _) = WalRecord::from_bytes(&buf[c1 + c2..]).unwrap();

        assert_eq!(d1.lsn, 1);
        assert_eq!(d2.op_type, WalOpType::Commit);
        assert_eq!(d3.op_type, WalOpType::Checkpoint);
    }
}
