# xfstests generic/091 & generic/001 Fix Analysis

## Summary

Two bugs fixed that caused data corruption in xfstests:

1. **generic/091** (fsx data corruption): FUSE write ordering race condition
2. **generic/001** (file copy chain corruption): Redis `node_cache` staleness

## generic/091: Write Ordering Race

### Root Cause

rfuse3 spawns each FUSE request as a separate tokio task. With writeback cache
enabled, the kernel batches dirty pages and sends them as `FUSE_WRITE` requests.
Linux assigns monotonically increasing `unique` IDs to each request (lower =
older).

When two overlapping writes arrive concurrently:
- Task A (unique=100, offset=0, len=4096)
- Task B (unique=101, offset=0, len=4096)

If Task B finishes first and pushes its slice to the back of the deque, then
Task A finishes and pushes to the back, the overlay_dirty reader iterates
forward and Task A's (older) data **overwrites** Task B's (newer) data.

### Fix

Insert new slices in sorted position by `creation_unique` so that slices
committed in FIFO (front-first) order reflect the kernel's temporal write
ordering. With monotonically increasing unique values, this ensures the newest
write is always at the back of the deque and wins in overlay_dirty.

For non-cached writes (`creation_unique = 0`), we fall back to `push_back`
since the kernel serializes non-cached (direct I/O) writes.

### Files Changed

- `src/vfs/io/writer.rs`: Added `creation_unique` field to `SliceState`,
  sorted insertion in `find_slice_or_create`
- `src/vfs/fs/mod.rs`: Thread `creation_unique` through `write_cached_ino`
- `src/fuse/mod.rs`: Pass `_req.unique` to `write_cached_ino`

## generic/001: Stale node_cache

### Root Cause

A `node_cache` (moka, TTL=2s) was added to `RedisMetaStore::get_node()` to
reduce Redis round-trips. However, the `write()` function (which atomically
appends a slice and extends file size via Lua script) did **not** invalidate
the cache after updating the node.

Sequence:
1. `fill` writes 102400 bytes → `store.write()` updates Redis size to 102400
2. `node_cache` still holds stale entry (size=0) from file creation
3. `cp` opens the file within 2s → `stat_fresh()` → `store.stat()` →
   `get_node()` returns **stale size=0**
4. Reader bounds read to `file_size()=0` → returns empty data → corruption

### Fix

Added `self.node_cache.invalidate(&ino).await` after successful `write()` in
the Redis store. This matches the pattern already used by `extend_file_size()`
and `set_attr()`.

### Files Changed

- `src/meta/stores/redis/mod.rs`: Invalidate `node_cache` on successful write

## generic/091: Hang During Long-Running fsx (Known Issue)

### Symptom

After fixing the corruption, generic/091's fsx test runs longer (no early
failure) but may hang indefinitely with only `auto_flush: alive` heartbeat
messages visible.

### Analysis

The hang is caused by an S3 upload operation that never completes (no timeout
configured on the AWS SDK HTTP client). When `commit_chunk` waits for a slice's
upload to finish, it loops indefinitely with 100ms timeouts but can never make
progress if the underlying HTTP request is stuck.

This is **not** caused by the write-ordering fix (verified: sorted insertion
with monotonically increasing uniques is identical to push_back). The issue is
pre-existing but was masked because fsx would fail early on data corruption.

### Mitigation

- The S3 client needs operation-level timeouts (connect, read, operation)
- Consider adding a maximum upload duration in `commit_chunk` that marks the
  slice as Failed after exceeding a threshold

## Test Commands

```bash
# Run individual test
cd docker/compose-xfstests
bash run_redis_xfstests.sh --cases "generic/001"
bash run_redis_xfstests.sh --cases "generic/091"

# Run full excluded suite
bash run_redis_xfstests.sh
```

## generic/095: Excluded (FUSE Subtype Detection)

The xfstests `_fs_type()` helper reports `fuse` (from `df -T`) instead of the
configured `FSTYP=fuse`. The framework's sed translations handle known FUSE
filesystems (e.g., `fuse.glusterfs` → `glusterfs`) but not `fuse.slayerfs`.
This causes `_check_mounted_on` to fail. Added to exclude list pending
upstream xfstests patch or local sed fixup.
