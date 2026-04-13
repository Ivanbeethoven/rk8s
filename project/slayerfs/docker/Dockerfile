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

ARG XFSTESTS_BINARY=xfs_io

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        fuse3 \
        libsqlite3-0 \
        libssl3 \
        procps \
        tini \
        util-linux \
        xfsprogs && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/rk8s/project/target/release/slayerfs /usr/local/bin/slayerfs
COPY slayerfs/tests/scripts/xfstests-prebuilt/xfstests-prebuilt.tar.gz /tmp/xfstests-prebuilt.tar.gz
COPY slayerfs/docker/entrypoint.sh /usr/local/bin/slayerfs-entrypoint

RUN set -e; \
    mkdir -p /opt/xfstests/bin /opt/xfstests/lib /mnt/slayerfs /run/slayerfs /var/lib/slayerfs/data; \
    tar -xzf /tmp/xfstests-prebuilt.tar.gz -C /tmp xfstests/bundled/bin/${XFSTESTS_BINARY}; \
    cp /tmp/xfstests/bundled/bin/${XFSTESTS_BINARY} /opt/xfstests/bin/${XFSTESTS_BINARY}; \
    if tar -tzf /tmp/xfstests-prebuilt.tar.gz | grep -q '^xfstests/bundled/lib/'; then \
        tar -xzf /tmp/xfstests-prebuilt.tar.gz -C /tmp xfstests/bundled/lib; \
        cp -a /tmp/xfstests/bundled/lib/. /opt/xfstests/lib/; \
    fi; \
    chmod +x /usr/local/bin/slayerfs /usr/local/bin/slayerfs-entrypoint /opt/xfstests/bin/${XFSTESTS_BINARY}; \
    rm -rf /tmp/xfstests /tmp/xfstests-prebuilt.tar.gz; \
    grep -q '^user_allow_other$' /etc/fuse.conf 2>/dev/null || echo 'user_allow_other' >> /etc/fuse.conf

ENV SLAYERFS_HOME=/var/lib/slayerfs \
    SLAYERFS_MOUNT_POINT=/mnt/slayerfs \
    SLAYERFS_DATA_BACKEND=local-fs \
    SLAYERFS_DATA_DIR=/var/lib/slayerfs/data \
    SLAYERFS_META_BACKEND=sqlite \
    SLAYERFS_SQLITE_PATH=/var/lib/slayerfs/metadata.db \
    RUST_LOG=slayerfs=info \
    LD_LIBRARY_PATH=/opt/xfstests/lib

VOLUME ["/mnt/slayerfs", "/var/lib/slayerfs/data"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/slayerfs-entrypoint"]