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
| Redis metadata | `rename()` lets Lua perform source/destination dentry lookup and return invalidation inodes; overwritten file targets are tombstoned and queued for cleanup | Remove two Rust-side Redis dentry lookups on successful rename while preserving POSIX overwrite semantics and GC visibility |
| FUSE/VFS open | `open_fresh_ino()` performs the required fresh stat once and FUSE `open()` no longer does an extra cached stat before opening | Preserve close-to-open freshness while removing a duplicate metadata lookup on normal open |
| Redis metadata | `create_entry()` lets Lua do the parent directory lookup and setgid inheritance check; Rust updates only local node cache from the Lua result | Remove a duplicated Redis parent `GET` on cold create paths |
| Meta client open | `stat_fresh()` refreshes cached file metadata in place while still dropping stale slice/parent/children state | Avoid reallocating the inode cache entry on every close-to-open refresh |
| VFS open/read | File handles create `FileReader` lazily on first committed read instead of during open | Remove reader allocation and reader registry work from open-only workloads |
| Meta client create/mkdir | Successful create/mkdir no longer force-loads the parent inode into the client cache after Lua already validated it | Remove an extra parent stat/Redis `GET` on cold successful create paths |

Environment note:

| Item | Evidence | Decision |
| --- | --- | --- |
| Stale local SlayerFS perf mount was consuming memory before the latest focused reruns | Process `456320`, mount `/tmp/slayerfs-perf-455770/mnt` | Cleaned with lazy unmount and process kill before accepting new perf numbers |

Rejected experiment:

| Area | Artifact | Result | Decision |
| --- | --- | --- | --- |
| FUSE unlink/rmdir and VFS rmdir precheck removal | `docker/compose-xfstests/artifacts/perf-run-1780157265-27175` | `dirperf` was 25s, worse than the prior focused 24s result | Reverted; do not reintroduce without new evidence |
| Redis `lookup_with_attr` helper for open/create follow-up attrs | `docker/compose-xfstests/artifacts/perf-run-1780158557-8433` | `dirperf` was 25s, worse than the prior focused result; `metaperf` improved only slightly to 210s | Reverted; avoid broad metadata API expansion until operation traces prove the exact call pattern |
| Redis `rmdir()` Rust-side prelookup removal | `docker/compose-xfstests/artifacts/perf-run-1780163094-25634` | `dirperf` stayed 21s and `metaperf` regressed to 236s | Reverted; the safe rmdir Lua operation remains, but the prelookup removal did not earn its keep |
| VFS read-only open attr cache | `docker/compose-xfstests/artifacts/perf-run-1780171930-10210` | `dirperf` stayed 20s and `open` regressed to 2077.3 ops/s from 2103.9 ops/s, although `metaperf` wall time was 212s | Reverted; do not cache around close-to-open freshness until focused traces prove it pays for a real workload |

Focused comparison:

| Workload | SlayerFS artifact | SlayerFS result | JuiceFS artifact | JuiceFS result | Status |
| --- | --- | ---: | --- | ---: | --- |
| `fio-randrw` read BW | `perf-run-1780152993-885` | 115.41 MiB/s | `juicefs-perf-run-1780153510-28102` | 66.00 MiB/s | SlayerFS faster |
| `fio-randrw` write BW | `perf-run-1780152993-885` | 53.15 MiB/s | `juicefs-perf-run-1780153510-28102` | 29.29 MiB/s | SlayerFS faster |
| `dirperf` wall time | `perf-run-1780169879-22264` | 20s | `juicefs-perf-run-1780153510-28102` | 13s | Matched best SlayerFS run, still not close |
| `metaperf` wall time | `perf-run-1780169879-22264` | 213s | `juicefs-perf-run-1780153510-28102` | 222s | Better on wall time, worse than SlayerFS best 210s |

Important caveat: `metaperf` wall time is close, but individual metadata ops still lag JuiceFS on create/open/rename. The next bottleneck is metadata hot-path round trips rather than disk-cache throughput.

Latest focused metadata detail:

| Operation | SlayerFS `perf-run-1780169879-22264` | JuiceFS `juicefs-perf-run-1780153510-28102` | Gap |
| --- | ---: | ---: | --- |
| create | 187.9 ops/s | 285.1 ops/s | SlayerFS slower |
| open | 2103.9 ops/s | 6049.8 ops/s | SlayerFS much slower, but improved locally |
| stat | 1,125,651 ops/s | 1,101,680 ops/s | Similar |
| readdir | 28,576.0 ops/s | 37,075.7 ops/s | SlayerFS slower |
| rename | 910.2 ops/s | 1438.3 ops/s | SlayerFS slower |

Latest focused `dirperf` shape:

```text
docker/compose-xfstests/artifacts/perf-run-1780169879-22264
100 1.719
200 1.653
300 1.710
400 1.796
500 1.750
600 1.841
700 1.846
800 1.955
900 1.961
1000 1.967
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

Latest post-commit metadata continuation:

| Change | Evidence | Decision |
| --- | --- | --- |
| Redis rename Lua-side dentry lookup | New ignored commandstats test failed before the patch with 5 Redis `HGET` calls, then passed after the patch with only Lua-side dentry lookups plus the test verification lookup | Keep |
| Redis rename overwrite tombstone and same-inode hardlink no-op | `test_rename_lua_existing_file_target_is_replaced`, `test_rename_lua_overwrite_file`, and `test_rename_lua_hardlink_same_inode_target_is_noop` pass | Keep |
| Redis rmdir Lua-side dentry lookup | Commandstats test passed after patch, but focused Docker perf did not improve `dirperf` and worsened `metaperf` | Reverted |
| FUSE/VFS open duplicate stat removal | `test_open_fresh_by_ino_checks_current_attr_once` passes and focused Docker perf improved `metaperf` to 207s without changing `dirperf` | Keep |
| Redis create Lua-side parent lookup | New ignored commandstats test failed before the patch with 2 Redis `GET` calls, then passed after moving parent lookup/setgid inheritance into Lua | Keep |
| Meta client/VFS open local bookkeeping | New tests failed before the patch because fresh stat reallocated the inode cache entry and open eagerly created `FileReader`; focused Docker perf improved open throughput but not wall time | Keep cautiously; useful for open gap but insufficient |
| Meta client create/mkdir parent stat removal | New Redis commandstats tests failed before the patch with 2 Redis `GET` calls, then passed with at most 1 `GET` after skipping the client-side parent stat | Keep; `dirperf` matched the best 20s run |

Focused continuation artifacts:

| Artifact | Code state | `dirperf` | `metaperf` | Metadata note |
| --- | --- | ---: | ---: | --- |
| `perf-run-1780162288-9988` | Redis rename optimization only | 21s | 213s | `rename` improved to 935.6 ops/s from 840.5 ops/s |
| `perf-run-1780163094-25634` | Redis rename plus rejected rmdir prelookup removal | 21s | 236s | Rejected because wall time regressed |
| `perf-run-1780164106-6479` | Redis rename plus FUSE/VFS open fresh-stat de-duplication | 21s | 207s | `open` improved to 1984.6 ops/s; current best metadata wall time, still far from JuiceFS `dirperf` |
| `perf-run-1780165688-18917` | Redis create parent prelookup removal before response-size cleanup | 20s | 209s | `create` improved to 185.6 ops/s; first `dirperf` result at 20s |
| `perf-run-1780166421-11764` | Redis create parent prelookup removal with compact Lua response | 20s | 216s | `dirperf` repeated at 20s; `metaperf` outlier was worse |
| `perf-run-1780166694-29412` | Same code, metaperf-only rerun | n/a | 210s | `create` improved to 188.3 ops/s; used as latest metadata operation detail |
| `perf-run-1780168064-1374` | MetaClient `stat_fresh()` in-place cache refresh only | 21s | 211s | `open` improved to 2062.6 ops/s, but `dirperf` regressed from best |
| `perf-run-1780168941-8386` | In-place fresh stat plus lazy `FileReader` allocation | 21s | 214s | `open` improved to 2083.6 ops/s, but wall time did not improve |
| `perf-run-1780169879-22264` | Add MetaClient create/mkdir parent-stat removal | 20s | 213s | `open` improved to 2103.9 ops/s and `dirperf` matched best; still far from JuiceFS |
| `perf-run-1780170763-16114` | Open-only diagnostic with Redis commandstats kept for inspection | n/a | n/a | Open-only `metaperf` reported 2132.4 ops/s; Redis counters included setup/background traffic, so use as diagnostic only |
| `perf-run-1780171930-10210` | Rejected VFS read-only open attr cache experiment | 20s | 212s | `open` regressed to 2077.3 ops/s and `dirperf` did not improve; reverted instead of keeping speculative local caching |

Latest continuation verification:

| Command | Result |
| --- | --- |
| `cargo test -p slayerfs meta::stores::redis::tests::test_rename_uses_lua_dentry_lookup_without_rust_prelookups -- --ignored --nocapture` | Passed for lib and bin test targets |
| `cargo test -p slayerfs meta::stores::redis::tests::test_rename_lua -- --ignored --nocapture` | Passed: 13 rename Lua cases for lib and bin test targets |
| `cargo test -p slayerfs meta::stores::redis::tests::test_rmdir_lua -- --ignored --nocapture` | Passed: 4 rmdir Lua cases for lib and bin test targets |
| `cargo test -p slayerfs meta::client::tests::test_rename_operations -- --nocapture` | Passed for lib and bin test targets |
| `cargo test -p slayerfs --test rename_integration_test -- --nocapture` | Passed: 6 integration tests |
| `cargo test -p slayerfs vfs::fs::tests::basic_tests -- --nocapture` | Passed: 8 basic VFS tests for lib and bin test targets |
| `cargo test -p slayerfs meta::stores::redis::tests::test_create_entry_uses_lua_parent_lookup_without_rust_prelookup -- --ignored --nocapture` | Passed for lib and bin test targets after failing before the patch with 2 Redis `GET` calls |
| `cargo test -p slayerfs meta::stores::redis::tests::test_create_entry_updates_parent_node_cache -- --ignored --nocapture` | Passed for lib and bin test targets |
| `cargo test -p slayerfs meta::stores::redis::tests::test_create_entry_lua -- --ignored --nocapture` | Passed: 4 create Lua cases for lib and bin test targets |
| `cargo test -p slayerfs meta::client::tests::test_stat_fresh_refreshes_cached_file_entry_in_place -- --nocapture` | Red/green verified; passed for lib and bin test targets |
| `cargo test -p slayerfs vfs::fs::tests::basic_tests::test_open_defers_reader_until_first_read -- --nocapture` | Red/green verified; passed for lib and bin test targets |
| `cargo test -p slayerfs test_meta_client_ -- --ignored --nocapture` | Red/green verified: create_file and mkdir avoid the extra parent Redis `GET`; passed for lib and bin test targets |
| `cargo test -p slayerfs meta::stores::redis::tests::test_meta_client_stat_fresh_uses_warm_store_node_cache -- --ignored --nocapture` | Added diagnostic coverage showing hot `stat_fresh()` reuses the Redis store node cache instead of issuing Redis `GET` calls |
| `cargo test -p slayerfs vfs::fs::tests::basic_tests -- --nocapture` | Passed: 9 basic VFS tests for lib and bin test targets |
| `cargo test -p slayerfs --test gc_test -- --nocapture` | Passed after an earlier full-suite-only `test_gc_respects_min_age` flake |
| `timeout 20m cargo test -p slayerfs --lib --bins --tests -- --format terse` | Passed on rerun: lib target 290 passed/139 ignored, bin target 282 passed/139 ignored, integration tests passed with expected ignored external-service tests |
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

Latest diagnostic notes:

```text
docker/compose-xfstests/artifacts/perf-run-1780170763-16114
open-only metaperf: 2132.4 ops/s
Redis commandstats were not clean enough to isolate the timed phase because setup/background work was included.

cargo test -p slayerfs meta::stores::redis::tests::test_meta_client_stat_fresh_uses_warm_store_node_cache -- --ignored --nocapture
hot stat_fresh() diagnostic: Redis GET calls stay at 0 after cache warmup.

docker/compose-xfstests/artifacts/perf-run-1780171930-10210
VFS read-only open attr cache: dirperf 20s, metaperf 212s, open 2077.3 ops/s.
Rejected because the target open operation regressed and dirperf did not move closer to JuiceFS 13s.
```

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

- [x] **Step 3: Collapse Redis rename source/destination dentry lookup into one Lua operation**

Run:

```bash
cargo test -p slayerfs meta::stores::redis::tests::test_rename_uses_lua_dentry_lookup_without_rust_prelookups -- --ignored --nocapture
cargo test -p slayerfs meta::stores::redis::tests::test_rename_lua -- --ignored --nocapture
cargo test -p slayerfs meta::client::tests::test_rename_operations -- --nocapture
cargo test -p slayerfs --test rename_integration_test -- --nocapture
bash docker/compose-xfstests/run_redis_perf.sh --tools "dirperf metaperf"
```

Verified evidence:

```text
docker/compose-xfstests/artifacts/perf-run-1780162288-9988
dirperf pass 21s
metaperf pass 213s
rename 935.6 ops/s
```

The optimization is kept because it reduced store-side Redis `HGET` calls and improved isolated rename throughput. It does not close the overall `dirperf` gap by itself.

- [x] **Step 4: Collapse Redis create parent lookup into the Lua operation**

`create_entry()` previously loaded the parent node in Rust to check directory type and setgid inheritance, then the Lua script loaded the same parent node again for the atomic create. The Lua script now owns that parent lookup and returns the new inode plus final gid, letting Rust update only its local node cache.

Run:

```bash
cargo test -p slayerfs meta::stores::redis::tests::test_create_entry_uses_lua_parent_lookup_without_rust_prelookup -- --ignored --nocapture
cargo test -p slayerfs meta::stores::redis::tests::test_create_entry_updates_parent_node_cache -- --ignored --nocapture
cargo test -p slayerfs meta::stores::redis::tests::test_create_entry_lua -- --ignored --nocapture
bash docker/compose-xfstests/run_redis_perf.sh --tools "dirperf metaperf"
bash docker/compose-xfstests/run_redis_perf.sh --tools "metaperf"
```

Verified evidence:

```text
docker/compose-xfstests/artifacts/perf-run-1780166421-11764
dirperf pass 20s
metaperf pass 216s

docker/compose-xfstests/artifacts/perf-run-1780166694-29412
metaperf pass 210s
create 188.3 ops/s
```

The optimization is kept because it moves `dirperf` from 21s to 20s and improves the create operation on rerun. It does not close the `dirperf` gap to JuiceFS.

- [x] **Step 5: Remove duplicate FUSE open stat without relaxing close-to-open refresh**

FUSE `open()` previously did a cached `stat_ino()` and then called `Vfs::open()`, which performed `meta_stat_fresh()` again for close-to-open semantics. It now calls `open_fresh_ino()`, which performs the fresh stat once, rejects directories and missing inodes, and then opens with the already-fresh attr.

Run:

```bash
cargo test -p slayerfs vfs::fs::tests::basic_tests::test_open_fresh_by_ino_checks_current_attr_once -- --nocapture
bash docker/compose-xfstests/run_redis_perf.sh --tools "dirperf metaperf"
```

Verified evidence:

```text
docker/compose-xfstests/artifacts/perf-run-1780164106-6479
dirperf pass 21s
metaperf pass 207s
open 1984.6 ops/s
```

The optimization is kept because it preserves the fresh stat and improves the focused metadata wall time. It does not close the `dirperf` gap to JuiceFS.

- [x] **Step 6: Trim local open bookkeeping and client-side create parent stats**

`open_fresh_ino()` still needs a fresh metadata check, but `MetaClient::stat_fresh()` no longer deletes and reallocates the whole inode cache entry when it can refresh the existing one in place. VFS file handles also defer `FileReader` allocation until the first committed read, so open-only workloads do not pay reader setup. On create/mkdir, `MetaClient` no longer force-loads the parent inode after Redis Lua has already validated it; cached parents still get incremental `add_child()` updates, while cold parents avoid the extra stat.

Run:

```bash
cargo test -p slayerfs meta::client::tests::test_stat_fresh_refreshes_cached_file_entry_in_place -- --nocapture
cargo test -p slayerfs vfs::fs::tests::basic_tests::test_open_defers_reader_until_first_read -- --nocapture
cargo test -p slayerfs test_meta_client_ -- --ignored --nocapture
cargo test -p slayerfs vfs::fs::tests::basic_tests -- --nocapture
bash docker/compose-xfstests/run_redis_perf.sh --tools "dirperf metaperf"
```

Verified evidence:

```text
docker/compose-xfstests/artifacts/perf-run-1780169879-22264
dirperf pass 20s
metaperf pass 213s
open 2103.9 ops/s
```

The optimization is kept because it improves the open operation and removes proven extra Redis `GET` calls on cold create/mkdir, while matching the best 20s `dirperf` run. It still does not close the `dirperf` gap to JuiceFS.

- [ ] **Step 7: Consider remaining Redis single-EVAL equivalents**

Apply the `unlink()` lesson narrowly: move only duplicated store-side round trips into existing atomic Redis scripts where the script already has the data, then verify with ignored Redis tests and focused `dirperf metaperf`.

Expected: improve create/open/rename or directory cleanup paths without reintroducing the reverted FUSE/VFS precheck-removal regression.

- [ ] **Step 8: Consider parent dentry caching or batched create handling**

Use this only after Step 1 proves repeated parent/child lookup traffic dominates `dirperf`.

Expected: maintain invalidation on create/unlink/rmdir/rename and reject cache-only correctness shortcuts.

## Maintenance Rules

- Update this file whenever a metadata maintenance, compaction, GC, read-retry, or perf-runner change lands.
- Keep finished work marked with `[x]` and leave future verification with `[ ]`.
- Add artifact IDs and exact commands, not prose-only claims.
- Do not add `.VSCodeCounter/` or transient report files to commits unless the user explicitly asks.
