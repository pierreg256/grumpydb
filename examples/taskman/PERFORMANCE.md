# TaskMan — Performance Guide

## Buffer Pool

GrumpyDB uses an **LRU buffer pool** to cache frequently accessed pages in memory.
This dramatically reduces disk I/O for repeated access patterns.

### How it works

```
Application → GrumpyDb → BufferPool → PageManager → Disk
                            ↑
                         LRU cache
                      (256 frames × 8 KiB = 2 MiB)
```

1. When the engine reads a page, the buffer pool checks its cache first
2. **Cache hit**: returns the page instantly (no disk I/O)
3. **Cache miss**: reads from disk, stores in a frame, returns it
4. When the pool is full, the **least recently used** unpinned frame is evicted
5. Dirty (modified) frames are flushed to disk before eviction

### Impact on operations

| Operation | Without pool | With pool |
|-----------|-------------|-----------|
| Sequential inserts | 1 disk read per insert (same page) | 1 read total (page stays cached) |
| Random reads | 1 disk read per get | Frequently accessed pages cached |
| Full scan | N disk reads for N pages | First scan loads, subsequent scans hit cache |
| Bulk delete | 2 reads per delete (index + data) | Hot pages stay in cache |

### Measuring performance

Use the `generate` and `search` commands to see buffer pool stats:

```bash
# Generate 5000 tasks — observe disk reads vs inserts
cargo run --example taskman -- generate --count 5000

# Search by tag — first scan loads pages, stats show cache usage
cargo run --example taskman -- search --tag urgent

# Run again — more cache hits on the second scan
cargo run --example taskman -- search --tag work
```

The output shows:
- **reads**: number of pages loaded from disk (cache misses)
- **writes**: number of dirty pages flushed to disk
- **cached**: pages currently in the pool / total capacity

### Why reads are low during inserts

When inserting documents sequentially, they are packed into the same data page.
The buffer pool keeps this page cached, so subsequent inserts don't trigger disk reads.
A new disk read only happens when:
- The current page fills up and a new one is allocated
- The auto-checkpoint (every 100 writes) flushes and evicts pages

### Pool capacity

The default pool size is **256 frames** (2 MiB). This can be customized via
`GrumpyDb::open_with_pool_capacity()`. Larger pools cache more pages but use more memory.

For most workloads:
- 256 frames handles ~50K small documents well
- For very large datasets (>100K docs), increase to 1024+ frames
- The pool only caches data pages — B+Tree index has its own page manager

### Concurrency note

The buffer pool is inside `GrumpyDb` (behind `&mut self`), so it is protected
by the `SharedDb`'s `RwLock`. In the SWMR model, readers and writers never
access the pool simultaneously.
