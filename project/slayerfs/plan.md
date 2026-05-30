# SlayerFS Metadata Maintenance And Performance Stability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep metadata GC/compaction assistance correct while preventing recent maintenance changes from causing measurable read/write performance regressions.

**Architecture:** Treat metadata maintenance, read retry recovery, local block-cache persistence, and perf tooling as one guarded change set. Metadata stores must reject stale compaction replacements, the VFS read path must refresh stale slice metadata only on transient failures, and disk-cache population must remain best-effort without making foreground or mixed fio workloads pay avoidable local I/O cost.

**Tech Stack:** Rust, Tokio, SeaORM/SQLite, Redis, etcd, `bytes::Bytes`, moka cache, FUSE/VFS tests, Docker compose xfstests perf scripts, fio, perf/inferno flamegraph tooling.

---

## Current Baseline

| Item | Status | Evidence |
| --- | --- | --- |
| Baseline commit before this perf iteration | Done | `137a96797 Improve metadata maintenance and cache perf` |
| Baseline full unit/integration suite | Passing | `cargo test -p slayerfs --lib --bins --tests` |
| Script syntax checks | Passing | `bash -n tools/perf/run_perf.sh docker/compose-xfstests/run_perf_in_container.sh docker/compose-xfstests/run_juicefs_perf_in_container.sh` |
| Whitespace check | Passing | `git diff --check` and `git diff --cached --check` |
| Focused perf check | Improved from low run | `docker/compose-xfstests/artifacts/perf-run-1780147102-22090` |

Perf comparison from the focused rerun:

| Workload | Low run `1780127871-11806` | Focused rerun `1780147102-22090` | Direction |
| --- | ---: | ---: | --- |
| `fio-randrw` read BW | 80.30 MiB/s | 114.68 MiB/s | Better |
| `fio-randrw` write BW | 36.67 MiB/s | 52.89 MiB/s | Better |
| `dirperf` wall time | 59s | 28s | Better |

The focused rerun is not a replacement for a full perf baseline because it only ran `fio-randrw dirperf`. Use it as a regression smoke result, not as the final performance certificate.

## 2026-05-30 Focused Performance Iteration

This iteration targets the recent metadata/cache hot paths that showed up in Docker compose perf runs, while keeping the previous GC/compaction safety work intact.

Implemented changes:

| Area | Change | Intent |
| --- | --- | --- |
| Disk cache | Opportunistic disk persistence now inserts hot cache first and skips background disk writes when write permits are saturated | Prevent mixed fio/write workloads from queueing avoidable local disk I/O |
| Disk cache | Disk-store paths accept `bytes::Bytes` | Avoid unnecessary `Vec` copies after writes and compression |
| Chunk store | Fresh write cache population keeps block data as `Bytes` | Preserve write visibility with fewer allocations |
| VFS create/mkdir | `create_file_at()` and `mkdir_at()` try the metadata mutation first, then fall back only for semantic errors | Remove redundant pre-stat/lookup on the successful create path |
| FUSE create/mkdir/mknod | Strict create-new semantics use `mkdir_at_new()` and `create_new=true`; `create` honors `O_EXCL` | Keep FUSE errors correct without extra parent/child probes |
| FUSE create/open | New entries use cached attrs and skip immediate close-to-open stat refresh | Avoid a redundant metadata stat right after create |
| FUSE setattr | `apply_new_entry_attrs()` skips no-op `set_attr` when uid/gid/mode already match | Avoid needless metadata writes on create-heavy workloads |
| Redis metadata | `create_entry()` updates the local node cache for parent and new inode after successful Lua create | Avoid immediate follow-up Redis reads from the same client |
| Redis metadata | `unlink()` now performs dentry lookup, node lookup, type check, nlink update, deleted marker insertion, hardlink parent restoration, and parent timestamp bump inside one Lua operation | Remove duplicate Redis round trips on successful unlink while preserving directory rejection and hardlink semantics |

Environment note:

| Item | Evidence | Decision |
| --- | --- | --- |
| Stale local SlayerFS perf mount was consuming memory before the latest focused reruns | Process `456320`, mount `/tmp/slayerfs-perf-455770/mnt` | Cleaned with lazy unmount and process kill before accepting new perf numbers |

Rejected experiment:

| Area | Artifact | Result | Decision |
| --- | --- | --- | --- |
| FUSE unlink/rmdir and VFS rmdir precheck removal | `docker/compose-xfstests/artifacts/perf-run-1780157265-27175` | `dirperf` was 25s, worse than the prior focused 24s result | Reverted; do not reintroduce without new evidence |
| Redis `lookup_with_attr` helper for open/create follow-up attrs | `docker/compose-xfstests/artifacts/perf-run-1780158557-8433` | `dirperf` was 25s, worse than the prior focused result; `metaperf` improved only slightly to 210s | Reverted; avoid broad metadata API expansion until operation traces prove the exact call pattern |

Focused comparison:

| Workload | SlayerFS artifact | SlayerFS result | JuiceFS artifact | JuiceFS result | Status |
| --- | --- | ---: | --- | ---: | --- |
| `fio-randrw` read BW | `perf-run-1780152993-885` | 115.41 MiB/s | `juicefs-perf-run-1780153510-28102` | 66.00 MiB/s | SlayerFS faster |
| `fio-randrw` write BW | `perf-run-1780152993-885` | 53.15 MiB/s | `juicefs-perf-run-1780153510-28102` | 29.29 MiB/s | SlayerFS faster |
| `dirperf` wall time | `perf-run-1780160758-17955` | 21s | `juicefs-perf-run-1780153510-28102` | 13s | Improved, still not close |
| `metaperf` wall time | `perf-run-1780160758-17955` | 212s | `juicefs-perf-run-1780153510-28102` | 222s | Close on wall time |

Important caveat: `metaperf` wall time is close, but individual metadata ops still lag JuiceFS on create/open/rename. The next bottleneck is metadata hot-path round trips rather than disk-cache throughput.

Latest focused metadata detail:

| Operation | SlayerFS `perf-run-1780160758-17955` | JuiceFS `juicefs-perf-run-1780153510-28102` | Gap |
| --- | ---: | ---: | --- |
| create | 182.7 ops/s | 285.1 ops/s | SlayerFS slower |
| open | 1937.5 ops/s | 6049.8 ops/s | SlayerFS much slower |
| stat | 1,115,124 ops/s | 1,101,680 ops/s | Similar |
| readdir | 28,261.3 ops/s | 37,075.7 ops/s | SlayerFS slower |
| rename | 840.5 ops/s | 1438.3 ops/s | SlayerFS slower |

Latest focused `dirperf` shape:

```text
docker/compose-xfstests/artifacts/perf-run-1780160758-17955
100 1.707
200 1.706
300 1.733
400 1.888
500 2.102
600 1.916
700 1.927
800 1.960
900 1.961
1000 1.945
```

Latest pre-commit verification:

| Command | Result |
| --- | --- |
| `cargo test -p slayerfs meta::stores::redis::tests::test_unlink_last_reference_updates_parent_and_deleted_child_atomically -- --ignored --nocapture` | Passed for lib and bin test targets |
| `cargo test -p slayerfs meta::stores::redis::tests::test_unlink_directory_rejected_fallback -- --ignored --nocapture` | Passed for lib and bin test targets |
| `cargo test -p slayerfs meta::stores::redis::tests::test_hardlink_state_machine_full_transition -- --ignored --nocapture` | Passed for lib and bin test targets |
| `cargo test -p slayerfs vfs::fs::tests::basic_tests -- --nocapture` | 7 passed for lib target and 7 passed for bin test target |
| `cargo test -p slayerfs --lib --bins --tests` | Passed: lib target 287 passed/134 ignored, bin test target 279 passed/134 ignored, integration tests passed with expected ignored external-service tests |
| `git diff --check` | Passed |

## Files And Responsibilities

| File | Responsibility |
| --- | --- |
| `src/meta/stores/redis/mod.rs` | Redis compaction replacement conflict detection and delayed-slice metadata updates |
| `src/meta/stores/database/mod.rs` | SQLite/database rename overwrite semantics and delayed/uncommitted GC selection |
| `src/meta/stores/etcd/mod.rs` | etcd delayed-slice cutoff selection parity |
| `src/meta/client/mod.rs` | Meta-layer rename error normalization |
| `src/meta/layer.rs` | Trait default rename error normalization |
| `src/vfs/io/reader.rs` | Transient read retry and stale slice-cache invalidation |
| `src/vfs/fs/mod.rs` | VFS rename error mapping and read stats reporting |
| `src/chunk/cache.rs` | Disk cache atomic temp-file publish and low-copy cache persistence |
| `src/chunk/store.rs` | Hot-cache write visibility and cache test compatibility |
| `tests/redis_compact_conflict_test.rs` | Redis versioned compaction conflict regression test |
| `tests/compaction_worker_test.rs` | Compaction lock release flake guard |
| `tests/rename_integration_test.rs` | Serial rename integration tests to avoid SQLite deadlock noise |
| `tools/perf/run_perf.sh` | libc symbol/debuginfo perf reporting |
| `docker/compose-xfstests/run_perf_in_container.sh` | SlayerFS perf summary output |
| `docker/compose-xfstests/run_juicefs_perf_in_container.sh` | JuiceFS comparison perf runner parity |

## Task 1: Maintain The Verified Baseline

**Files:**
- Modify: `plan.md`
- Read: `git status --short`
- Read: `git show --stat --oneline HEAD`

- [x] **Step 1: Record the current commit**

Run:

```bash
git show --stat --oneline --summary HEAD
```

Expected: output starts with:

```text
137a96797 Improve metadata maintenance and cache perf
```

- [x] **Step 2: Record files that are intentionally outside this perf iteration**

Run:

```bash
git status --short
```

Current known unrelated local files. Do not stage these with the perf/cache metadata patch unless the user explicitly asks:

```text
 M benches/slayerfs_bench.rs
?? .VSCodeCounter/
?? doc/report.md
?? doc/superpowers/plans/2026-05-25-slayerfs-io-hotpath-performance-plan.md
?? doc/superpowers/plans/2026-05-26-read-pipeline-refactor-plan.md
?? examples/sdk_fio_bench.rs
```

`plan.md` belongs to this working iteration and should be staged together with the related perf/cache metadata patch if this iteration is committed.

## Task 2: Verify Metadata Compaction Conflict Safety

**Files:**
- Read: `src/meta/stores/redis/mod.rs`
- Read: `tests/redis_compact_conflict_test.rs`

- [x] **Step 1: Ensure Redis compaction checks the expected slice set**

Confirm `replace_slices_for_compact_with_version()` deserializes the current Redis slice list and compares it with `expected_slices` by count, `slice_id`, `offset`, and `length`.

Run:

```bash
rg -n "expected_slices|CompactConflict|current_slices" src/meta/stores/redis/mod.rs tests/redis_compact_conflict_test.rs
```

Expected: both implementation and regression test contain those terms.

- [ ] **Step 2: Run the ignored live Redis conflict test when Redis is available**

Run:

```bash
cargo test -p slayerfs --test redis_compact_conflict_test -- --ignored --nocapture
```

Expected with Redis on `127.0.0.1:6379`: `redis_versioned_compaction_rejects_changed_slice_set ... ok`.

If Redis is not available, keep the test ignored and rely on the normal full suite plus Docker perf runs.

## Task 3: Guard Disk Cache Against Perf Regression

**Files:**
- Read: `src/chunk/cache.rs`
- Read: `src/chunk/store.rs`

- [x] **Step 1: Preserve atomic disk-cache publication**

Confirm `DiskStorage::store_with_permit()` writes to a private temp path and publishes with `std::fs::rename`.

Run:

```bash
rg -n "DISK_CACHE_TMP_COUNTER|spawn_blocking|std::fs::rename|store_with_permit" src/chunk/cache.rs
```

Expected: output includes the temp counter, blocking write, and atomic rename path.

- [x] **Step 2: Verify cache and block-store tests**

Run:

```bash
cargo test -p slayerfs chunk::cache::tests
cargo test -p slayerfs chunk::store::tests
```

Expected: both commands pass.

- [x] **Step 3: Re-run focused mixed workload after future cache changes**

Run:

```bash
bash docker/compose-xfstests/run_redis_perf.sh --tools "fio-randrw dirperf"
```

Expected minimum smoke bar:

```text
fio-randrw read BW >= 100 MiB/s
fio-randrw write BW >= 45 MiB/s
dirperf wall time <= 35s
```

If this fails, inspect `src/chunk/cache.rs` and `src/chunk/store.rs` before touching metadata code.

Verified evidence:

```text
docker/compose-xfstests/artifacts/perf-run-1780152993-885
fio-randrw read BW 115.41 MiB/s
fio-randrw write BW 53.15 MiB/s
dirperf 28s

docker/compose-xfstests/artifacts/perf-run-1780156496-30677
dirperf 24s
metaperf 212s

docker/compose-xfstests/artifacts/perf-run-1780160758-17955
dirperf 21s
metaperf 212s
```

## Task 4: Stabilize Rename And Metadata Tests

**Files:**
- Read: `tests/rename_integration_test.rs`
- Read: `src/meta/stores/database/mod.rs`
- Read: `src/vfs/fs/mod.rs`

- [x] **Step 1: Serialize rename integration tests**

Confirm every `#[tokio::test]` in `tests/rename_integration_test.rs` also has:

```rust
#[serial(rename_integration)]
```

Run:

```bash
rg -n "#\\[tokio::test\\]|#\\[serial\\(rename_integration\\)\\]" tests/rename_integration_test.rs
```

Expected: the counts match.

- [x] **Step 2: Re-run the rename integration file**

Run:

```bash
cargo test -p slayerfs --test rename_integration_test -- --nocapture
```

Expected: `6 passed; 0 failed`.

## Task 5: Keep Perf Tooling Actionable

**Files:**
- Read: `tools/perf/run_perf.sh`
- Read: `docker/compose-xfstests/run_perf_in_container.sh`
- Read: `docker/compose-xfstests/run_juicefs_perf_in_container.sh`

- [x] **Step 1: Verify shell syntax**

Run:

```bash
bash -n tools/perf/run_perf.sh docker/compose-xfstests/run_perf_in_container.sh docker/compose-xfstests/run_juicefs_perf_in_container.sh
```

Expected: no output and exit code `0`.

- [ ] **Step 2: Install libc debuginfo before deep flamegraph analysis**

On Debian/Ubuntu perf hosts, run:

```bash
apt-get update
apt-get install -y libc6-dbg
```

Then run:

```bash
tools/perf/run_perf.sh
```

Expected: `tools/perf/results/<timestamp>/flame/libc-report.txt` contains resolved libc symbols instead of only `[libc.so.6]` or raw addresses.

## Task 6: Full Perf Baseline Gate Before The Next Merge

**Files:**
- Read: `docker/compose-xfstests/run_redis_perf.sh`
- Read: `docker/compose-xfstests/artifacts/*/report.md`

- [ ] **Step 1: Run the full Redis/RustFS perf suite**

Run:

```bash
bash docker/compose-xfstests/run_redis_perf.sh
```

Expected: a new artifact directory under:

```text
docker/compose-xfstests/artifacts/perf-run-*
```

- [ ] **Step 2: Compare against the last relevant artifacts**

Run:

```bash
for d in \
  docker/compose-xfstests/artifacts/perf-run-1780126255-7366 \
  docker/compose-xfstests/artifacts/perf-run-1780127871-11806 \
  docker/compose-xfstests/artifacts/perf-run-1780147102-22090 \
  docker/compose-xfstests/artifacts/perf-run-*; do
  test -f "$d/results/fio-randrw.json" || continue
  echo "== $d"
  jq -r '.jobs[0] |
    "randrw read=\(.read.bw/1024)MiB/s write=\(.write.bw/1024)MiB/s read_p99=\(.read.clat_ns.percentile."99.000000"/1000000)ms write_p99=\(.write.clat_ns.percentile."99.000000"/1000000)ms"' \
    "$d/results/fio-randrw.json"
  test -f "$d/perf-summary.tsv" && awk '$1=="dirperf"{print "dirperf seconds="$3}' "$d/perf-summary.tsv"
done
```

Expected: the new run should not repeat the low-run shape unless there is a clear environmental explanation:

```text
fio-randrw read around or above 100 MiB/s
fio-randrw write around or above 45 MiB/s
dirperf around or below 35s
```

## Task 7: Close The Remaining Metadata Hot-Path Gap

**Files:**
- Read: `src/fuse/mod.rs`
- Read: `src/vfs/fs/mod.rs`
- Read: `src/meta/stores/redis/mod.rs`
- Read: `docker/compose-xfstests/artifacts/perf-run-1780156496-30677/report.md`
- Read: `docker/compose-xfstests/artifacts/juicefs-perf-run-1780153510-28102/report.md`

- [ ] **Step 1: Quantify operation counts before the next metadata patch**

Run a focused FUSE operation trace or perf profile around `dirperf` and `metaperf`.

Expected: identify whether create/open/rename are still paying extra lookup/stat/setattr Redis round trips compared with JuiceFS.

- [x] **Step 2: Collapse Redis unlink into one atomic operation**

Run:

```bash
cargo test -p slayerfs meta::stores::redis::tests::test_unlink_last_reference_updates_parent_and_deleted_child_atomically -- --ignored --nocapture
cargo test -p slayerfs meta::stores::redis::tests::test_unlink_directory_rejected_fallback -- --ignored --nocapture
cargo test -p slayerfs meta::stores::redis::tests::test_hardlink_state_machine_full_transition -- --ignored --nocapture
bash docker/compose-xfstests/run_redis_perf.sh --tools "dirperf metaperf"
```

Verified evidence:

```text
docker/compose-xfstests/artifacts/perf-run-1780160758-17955
dirperf pass 21s
metaperf pass 212s
```

- [ ] **Step 3: Return attrs from metadata mutations where safe**

Extend Redis/store APIs or helper methods so create/unlink/rmdir/rename can return the affected inode attrs or child inodes directly when the store already has the data inside the atomic operation.

Expected: preserve POSIX/FUSE error semantics while removing follow-up stat/lookup calls from successful hot paths.

- [ ] **Step 4: Review close-to-open refresh scope**

`Vfs::open()` still does `stat_fresh()` for normal opens. Keep this for correctness unless a narrower cache-validity rule can be proved by tests and Docker perf.

Expected: any relaxation must have explicit stale-read and multi-client regression coverage.

- [ ] **Step 5: Consider Redis rmdir/rename single-EVAL equivalents**

Apply the `unlink()` lesson narrowly: move only duplicated store-side round trips into existing atomic Redis scripts where the script already has the data, then verify with ignored Redis tests and focused `dirperf metaperf`.

Expected: improve create/open/rename or directory cleanup paths without reintroducing the reverted FUSE/VFS precheck-removal regression.

- [ ] **Step 6: Consider parent dentry caching or batched create handling**

Use this only after Step 1 proves repeated parent/child lookup traffic dominates `dirperf`.

Expected: maintain invalidation on create/unlink/rmdir/rename and reject cache-only correctness shortcuts.

## Maintenance Rules

- Update this file whenever a metadata maintenance, compaction, GC, read-retry, or perf-runner change lands.
- Keep finished work marked with `[x]` and leave future verification with `[ ]`.
- Add artifact IDs and exact commands, not prose-only claims.
- Do not add `.VSCodeCounter/` or transient report files to commits unless the user explicitly asks.
