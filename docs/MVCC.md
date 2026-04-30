# MVCC Snapshot Reads (v5)

This document describes the v5 snapshot-read model used by GrumpyDB.

## Scope in v5

v5 provides snapshot-consistent reads indexed by HLC with in-memory version
history tracking:

- `Database::begin_read()` creates a read transaction pinned to a `snapshot_hlc`.
- Snapshot reads (`get`, `scan`, `query`, `query_range`) return the latest
  visible value with `version.hlc <= snapshot_hlc`.
- Active snapshot readers are tracked to compute a low watermark.
- Version GC prunes obsolete in-memory history while preserving versions still
  needed by active readers.

The wire protocol exposes `SNAPSHOT_HLC` so clients can pin reads.

## Current limitations

- Historical versions are in-memory only (not persisted page-version chains).
- The read path is not yet fully lock-free under concurrent writes.

These two items are planned follow-up work for v6+.

## Why HLC

HLC-based snapshots are comparable across nodes and align with the distributed
roadmap (`R`/`W` consistency and quorum reads in later phases).
