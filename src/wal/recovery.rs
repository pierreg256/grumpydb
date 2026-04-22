//! Crash recovery: redo committed transactions, undo uncommitted ones.
//!
//! At startup, the recovery module reads the WAL and:
//! 1. **Redo**: replays page writes from committed transactions
//! 2. **Undo**: reverts page writes from uncommitted transactions

use std::collections::HashSet;

use crate::error::Result;
use crate::page::PAGE_SIZE;
use crate::page::manager::PageManager;
use crate::wal::record::{WalOpType, WalRecord};

/// Performs crash recovery by replaying the WAL.
///
/// - Committed transactions: their after-images are applied (redo).
/// - Uncommitted transactions: their before-images are applied (undo).
///
/// Returns the number of records processed and the set of committed tx_ids.
pub fn recover(
    records: &[WalRecord],
    data_pm: &mut PageManager,
    index_pm: &mut PageManager,
) -> Result<RecoveryResult> {
    if records.is_empty() {
        return Ok(RecoveryResult::default());
    }

    // Find the last checkpoint LSN
    let checkpoint_lsn = records
        .iter()
        .filter(|r| r.op_type == WalOpType::Checkpoint)
        .map(|r| r.lsn)
        .max()
        .unwrap_or(0);

    // Only process records after the last checkpoint
    let active: Vec<&WalRecord> = records.iter().filter(|r| r.lsn > checkpoint_lsn).collect();

    // Identify committed transactions
    let committed_txs: HashSet<u64> = active
        .iter()
        .filter(|r| r.op_type == WalOpType::Commit)
        .map(|r| r.tx_id)
        .collect();

    let mut redo_count = 0;
    let mut undo_count = 0;

    // REDO phase: apply after-images of committed transactions (in order)
    for record in active.iter() {
        if record.op_type == WalOpType::PageWrite && committed_txs.contains(&record.tx_id) {
            apply_after_image(record, data_pm, index_pm)?;
            redo_count += 1;
        }
    }

    // UNDO phase: revert before-images of uncommitted transactions (reverse order)
    for record in active.iter().rev() {
        if record.op_type == WalOpType::PageWrite && !committed_txs.contains(&record.tx_id) {
            apply_before_image(record, data_pm, index_pm)?;
            undo_count += 1;
        }
    }

    Ok(RecoveryResult {
        redo_count,
        undo_count,
        committed_txs: committed_txs.len(),
        records_processed: active.len(),
    })
}

/// Result of a recovery operation.
#[derive(Debug, Default)]
pub struct RecoveryResult {
    /// Number of page writes redone (committed TXs).
    pub redo_count: usize,
    /// Number of page writes undone (uncommitted TXs).
    pub undo_count: usize,
    /// Number of committed transactions found.
    pub committed_txs: usize,
    /// Total records processed.
    pub records_processed: usize,
}

/// Applies the after-image from a PageWrite record.
fn apply_after_image(
    record: &WalRecord,
    data_pm: &mut PageManager,
    index_pm: &mut PageManager,
) -> Result<()> {
    if record.data.len() < PAGE_SIZE * 2 {
        return Ok(()); // Malformed record, skip
    }
    let after = &record.data[PAGE_SIZE..PAGE_SIZE * 2];
    let mut page_buf = [0u8; PAGE_SIZE];
    page_buf.copy_from_slice(after);

    // Determine which PageManager owns this page
    // Convention: page_id's high bit indicates index file (bit 31)
    let pm = select_pm(record.page_id, data_pm, index_pm);
    let real_id = record.page_id & 0x7FFF_FFFF;

    if real_id < pm.num_pages() {
        pm.write_page(real_id, &page_buf)?;
    }
    Ok(())
}

/// Applies the before-image from a PageWrite record.
fn apply_before_image(
    record: &WalRecord,
    data_pm: &mut PageManager,
    index_pm: &mut PageManager,
) -> Result<()> {
    if record.data.len() < PAGE_SIZE {
        return Ok(()); // Malformed record, skip
    }
    let before = &record.data[..PAGE_SIZE];
    let mut page_buf = [0u8; PAGE_SIZE];
    page_buf.copy_from_slice(before);

    let pm = select_pm(record.page_id, data_pm, index_pm);
    let real_id = record.page_id & 0x7FFF_FFFF;

    if real_id < pm.num_pages() {
        pm.write_page(real_id, &page_buf)?;
    }
    Ok(())
}

/// Selects the appropriate PageManager based on the page_id convention.
/// Bit 31 set = index file, clear = data file.
fn select_pm<'a>(
    page_id: u32,
    data_pm: &'a mut PageManager,
    index_pm: &'a mut PageManager,
) -> &'a mut PageManager {
    if page_id & 0x8000_0000 != 0 {
        index_pm
    } else {
        data_pm
    }
}

/// Flag to mark a page_id as belonging to the index file.
pub const INDEX_PAGE_FLAG: u32 = 0x8000_0000;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, PageManager, PageManager) {
        let dir = TempDir::new().unwrap();
        let data = PageManager::new(dir.path().join("data.db")).unwrap();
        let index = PageManager::new(dir.path().join("index.db")).unwrap();
        (dir, data, index)
    }

    #[test]
    fn test_recovery_empty() {
        let (_dir, mut data, mut index) = setup();
        let result = recover(&[], &mut data, &mut index).unwrap();
        assert_eq!(result.redo_count, 0);
        assert_eq!(result.undo_count, 0);
    }

    #[test]
    fn test_recovery_committed_tx_redo() {
        let (_dir, mut data, mut index) = setup();

        // Create a page with known data
        let page_id = data.allocate_page().unwrap();
        let before = [0xAA; PAGE_SIZE];
        data.write_page(page_id, &before).unwrap();

        // Simulate: WAL says page should have after-image
        let after = [0xBB; PAGE_SIZE];
        let records = vec![
            WalRecord::page_write(1, 1, page_id, &before, &after),
            WalRecord::commit(2, 1),
        ];

        // "Crash" — page still has before-image (0xAA)
        // Recovery should redo → apply after-image (0xBB)
        let result = recover(&records, &mut data, &mut index).unwrap();
        assert_eq!(result.redo_count, 1);
        assert_eq!(result.undo_count, 0);

        let page = data.read_page(page_id).unwrap();
        assert_eq!(page[0], 0xBB);
    }

    #[test]
    fn test_recovery_uncommitted_tx_undo() {
        let (_dir, mut data, mut index) = setup();

        let page_id = data.allocate_page().unwrap();
        let before = [0xAA; PAGE_SIZE];
        let after = [0xCC; PAGE_SIZE];
        data.write_page(page_id, &after).unwrap(); // Page has the "dirty" state

        // WAL has the write but NO commit
        let records = vec![
            WalRecord::page_write(1, 1, page_id, &before, &after),
            // No commit!
        ];

        let result = recover(&records, &mut data, &mut index).unwrap();
        assert_eq!(result.redo_count, 0);
        assert_eq!(result.undo_count, 1);

        // Page should be reverted to before-image
        let page = data.read_page(page_id).unwrap();
        assert_eq!(page[0], 0xAA);
    }

    #[test]
    fn test_recovery_mixed_transactions() {
        let (_dir, mut data, mut index) = setup();

        let p1 = data.allocate_page().unwrap();
        let p2 = data.allocate_page().unwrap();

        let before1 = [0x11; PAGE_SIZE];
        let after1 = [0x22; PAGE_SIZE];
        let before2 = [0x33; PAGE_SIZE];
        let after2 = [0x44; PAGE_SIZE];

        // Both pages start with before images
        data.write_page(p1, &before1).unwrap();
        data.write_page(p2, &after2).unwrap(); // p2 has dirty state

        let records = vec![
            WalRecord::page_write(1, 1, p1, &before1, &after1),
            WalRecord::commit(2, 1), // TX1 committed
            WalRecord::page_write(3, 2, p2, &before2, &after2),
            // TX2 NOT committed
        ];

        let result = recover(&records, &mut data, &mut index).unwrap();
        assert_eq!(result.redo_count, 1); // TX1 redone
        assert_eq!(result.undo_count, 1); // TX2 undone

        assert_eq!(data.read_page(p1).unwrap()[0], 0x22); // redo → after
        assert_eq!(data.read_page(p2).unwrap()[0], 0x33); // undo → before
    }

    #[test]
    fn test_recovery_respects_checkpoint() {
        let (_dir, mut data, mut index) = setup();

        let p1 = data.allocate_page().unwrap();
        let before = [0; PAGE_SIZE];
        let after = [0xFF; PAGE_SIZE];
        data.write_page(p1, &before).unwrap();

        let records = vec![
            WalRecord::page_write(1, 1, p1, &before, &after),
            WalRecord::commit(2, 1),
            WalRecord::checkpoint(3), // checkpoint after TX1
                                      // Only records after LSN 3 are processed
        ];

        let result = recover(&records, &mut data, &mut index).unwrap();
        // Nothing to redo — TX1 was before the checkpoint
        assert_eq!(result.redo_count, 0);
        assert_eq!(result.undo_count, 0);
    }
}
