<div align="center">
  <img src="doc/icon.png" alt="SlayerFS icon" width="96" height="96" />
</div>

<h1 align="center">SlayerFS</h1>
<p align="center"><strong>High-performance Rust and layer-aware distributed filesystem</strong></p>
<p align="center"><a href="README.md"><b>English</b></a> | <a href="README_CN.md">中文</a></p>

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org/)

SlayerFS is a Rust filesystem for container, AI, and object-storage-heavy workloads. It exposes a POSIX-like FUSE interface, stores file data as chunked objects, and keeps namespace and slice metadata in a pluggable transactional metadata backend.

The core design goal is to decouple compute from storage: applications read and write normal paths, while SlayerFS handles chunk layout, object IO, caching, metadata transactions, compaction, and garbage collection.

## Architecture

<div align="center">
  <img src="doc/SlayerFS.png" alt="SlayerFS architecture" width="1280" />
</div>

Main layers:

- `fuse` and `vfs`: inode-based FUSE integration and POSIX-facing behavior.
- `meta`: metadata client, transaction-capable backends, sessions, control plane, compaction hooks, and GC metadata.
- `chunk`: chunk/block layout, read/write path, cache, compaction, delayed deletion, and block-store GC.
- `cadapter`: object backend abstraction with LocalFS and S3-compatible implementations.
- `fs` and SDK examples: path-based API and local examples that can run without FUSE.

Default data layout:

- Chunk size: 64 MiB
- Block size: 4 MiB
- Object granularity: blocks addressed under slice IDs

## Current Capabilities

Data backends:

- `local-fs`: stores object data in a local directory for development and tests.
- `s3`: supports AWS S3 and S3-compatible services such as RustFS, MinIO, and Ceph RGW.

Metadata backends:

- `sqlx`: SQLite for local/dev and PostgreSQL for server deployments.
- `redis`: low-latency metadata operations with Lua/CAS based chunk updates.
- `etcd`: distributed KV metadata with transaction and watch-oriented semantics.
- `tikv`: transactional TiKV metadata backend with namespace, file data, hardlinks, symlinks, rename exchange, compaction hooks, and delayed/uncommitted slice GC support.

Filesystem and runtime features:

- FUSE mount via `slayerfs mount`
- Path/inode operations for create, mkdir, readdir, stat, read, write, truncate, unlink, rmdir, rename, hardlink, and symlink
- Chunked sparse IO with zero-fill for holes
- Read/write cache with memory and SSD budgets
- Optional S3 writeback mode (`commit_before_upload`) with orphan cleanup support
- Slice compaction and two-phase block deletion
- Runtime control plane for `slayerfs info` and `slayerfs gc`

## Quick Start

Requirements:

- Rust 1.85+ (the crate uses Rust 2024 edition)
- Linux for FUSE mounting
- `fuse3` / `fusermount3` for unprivileged mounts

Run the SDK demo without FUSE:

```bash
cargo run -p slayerfs --example sdk_demo -- /tmp/slayerfs-sdk-data
```

Build the CLI:

```bash
cargo build -p slayerfs --release
```

Mount with local object storage and SQLite metadata:

```bash
mkdir -p /tmp/slayerfs-mnt /tmp/slayerfs-data

cargo run -p slayerfs -- mount /tmp/slayerfs-mnt \
  --data-backend local-fs \
  --data-dir /tmp/slayerfs-data \
  --meta-backend sqlx \
  --meta-url sqlite:///tmp/slayerfs-meta.db
```

Unmount with `Ctrl+C` in the mount process, or use `fusermount3 -u /tmp/slayerfs-mnt` if needed.

## Configuration

SlayerFS can be configured with CLI flags, a YAML file, or both. CLI flags override YAML values.

Minimal YAML:

```yaml
mount_point: /tmp/slayerfs

data:
  backend: local-fs
  localfs:
    data_dir: ./data

meta:
  backend: sqlx
  sqlx:
    url: "sqlite::memory:"

layout:
  chunk_size: 67108864
  block_size: 4194304
```

S3 plus Redis example:

```yaml
mount_point: /mnt/slayerfs

data:
  backend: s3
  s3:
    bucket: slayerfs-data
    endpoint: http://127.0.0.1:9000
    region: us-east-1
    force_path_style: true
    disable_payload_checksum: true
    part_size: 16777216
    max_concurrency: 16

meta:
  backend: redis
  redis:
    url: "redis://127.0.0.1:6379/0"

cache:
  root: /var/cache/slayerfs
  writeback_mode: upload_before_commit
```

TiKV metadata example:

```yaml
mount_point: /mnt/slayerfs

meta:
  backend: tikv
  tikv:
    pd_endpoints:
      - 127.0.0.1:2379
    namespace: tenant-a
```

See [doc/configuration.md](doc/configuration.md) and the files under [examples/](examples/) for the full configuration surface.

## CLI

```bash
slayerfs mount [OPTIONS] [MOUNT_POINT]
slayerfs info [MOUNT_POINT]
slayerfs gc [MOUNT_POINT] [--dry-run]
```

Useful mount options:

- `--config <FILE>`: YAML configuration file.
- `--data-backend <local-fs|s3>`: object data backend.
- `--meta-backend <sqlx|redis|etcd|tikv>`: metadata backend.
- `--chunk-size <BYTES>` and `--block-size <BYTES>`: data layout tuning.
- `--fuse-workers <N>`: `0` or `1` uses low-overhead rfuse3 session dispatch; values above `1` enable the worker pool.
- `--fuse-max-background <N>`: maximum queued and running FUSE requests.
- `--privileged`: use `/dev/fuse` directly instead of `fusermount3`.

## Testing

Fast local checks:

```bash
cargo check -p slayerfs
cargo test -p slayerfs
```

Focused checks used often during backend work:

```bash
cargo test -p slayerfs meta::stores::tikv --lib
cargo test -p slayerfs mount_config --bin slayerfs
```

Docker-based filesystem tests:

```bash
cd docker
bash compose-xfstests/run_redis_xfstests.sh --cases "generic/001"
bash compose-xfstests/run_redis_xfstests.sh --s3 --cases "generic/001"
```

More test and benchmark entry points:

- [docker/README.md](docker/README.md)
- [doc/docker-compose-test-guide.md](doc/docker-compose-test-guide.md)
- [doc/bench.md](doc/bench.md)
- [doc/fuzz_testing_guide.md](doc/fuzz_testing_guide.md)

## Feature Flags

```bash
cargo build -p slayerfs --release --features jemalloc
cargo build -p slayerfs --release --features profiling
cargo build -p slayerfs --release --features rkyv-serialization
```

Available features:

- `jemalloc`: use jemalloc as the global allocator on Linux.
- `jemalloc-profiling`: enable jemalloc heap profiling support.
- `profiling`: enable tracing flamegraph, Chrome trace, and tokio-console integrations.
- `rkyv-serialization`: enable rkyv-based metadata serialization support.

## Documentation

Start here:

- [Configuration](doc/configuration.md)
- [Architecture](doc/arch.md)
- [Metadata](doc/meta.md)
- [Chunk layout](doc/chunk.md)
- [Read path](doc/read-path.md)
- [Write path](doc/write-path.md)
- [Caching](doc/caching.md)
- [Compaction and GC](doc/compaction-gc.md)
- [Observability](doc/observability.md)
- [SDK](doc/sdk.md)
- [Control plane](docs/control-plane.md)
- [SlayerFS vs JuiceFS analysis](doc/slayerfs-vs-juicefs-analysis.md)

## Repository Map

- `src/`: core filesystem, metadata, chunk, object backend, FUSE, and CLI code.
- `examples/`: SDK, S3, persistence, and local mount examples.
- `doc/` and `docs/`: design notes, operations guides, audits, and debugging notes.
- `docker/`: compose stacks, xfstests/LTP/perf runners, and runtime image tooling.
- `tests/`: integration and native stress tests.
- `operator/`: Kubernetes operator prototype and CRD documentation.
- `tools/`: performance and stats helpers.

## Contributing

Issues and PRs are welcome. For larger changes, prefer keeping implementation, tests, and documentation updates together so backend capabilities and operational guidance stay in sync.
