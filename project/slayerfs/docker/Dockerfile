FROM rust:1.85-bookworm AS builder

WORKDIR /usr/src/rk8s/project

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        pkg-config \
        protobuf-compiler \
        libfuse3-dev \
        libssl-dev \
        libsqlite3-dev && \
    rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY rfuse3 ./rfuse3
COPY slayerfs ./slayerfs

RUN cargo build --release -p slayerfs --bin slayerfs

FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        fuse3 \
        libsqlite3-0 \
        libssl3 \
        procps \
        tini \
        util-linux && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/rk8s/project/target/release/slayerfs /usr/local/bin/slayerfs
COPY slayerfs/docker/entrypoint.sh /usr/local/bin/slayerfs-entrypoint
COPY slayerfs/docker/healthcheck.sh /usr/local/bin/slayerfs-healthcheck

RUN chmod +x /usr/local/bin/slayerfs /usr/local/bin/slayerfs-entrypoint /usr/local/bin/slayerfs-healthcheck && \
    mkdir -p /mnt/slayerfs /run/slayerfs /var/lib/slayerfs/data /var/lib/slayerfs/meta

ENV SLAYERFS_HOME=/var/lib/slayerfs \
    SLAYERFS_MOUNT_POINT=/mnt/slayerfs \
    SLAYERFS_DATA_BACKEND=local-fs \
    SLAYERFS_DATA_DIR=/var/lib/slayerfs/data \
    SLAYERFS_META_BACKEND=sqlite \
    SLAYERFS_SQLITE_PATH=/var/lib/slayerfs/meta/metadata.db \
    RUST_LOG=slayerfs=info

VOLUME ["/mnt/slayerfs", "/var/lib/slayerfs/data", "/var/lib/slayerfs/meta"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/slayerfs-entrypoint"]