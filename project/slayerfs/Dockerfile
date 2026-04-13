FROM rust:1-bookworm AS builder

RUN mkdir -p /root/.cargo && cat <<'EOF' >/root/.cargo/config.toml
[source.crates-io]
replace-with = 'rsproxy-sparse'

[source.rsproxy]
registry = "https://rsproxy.cn/crates.io-index"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[registries.rsproxy]
index = "https://rsproxy.cn/crates.io-index"

[net]
git-fetch-with-cli = true
EOF

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    clang \
    lld \
    pkg-config \
    protobuf-compiler \
    libssl-dev \
    libfuse3-dev \
    ca-certificates \
    git && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/rk8s/project
COPY . .
RUN cargo build --release -p slayerfs --example persistence_demo

FROM ubuntu:24.04

ARG XFSTESTS_REPO=https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git
ARG XFSTESTS_BRANCH=v2023.12.10

RUN rm -f /etc/apt/sources.list.d/*.sources && cat <<'EOF' >/etc/apt/sources.list
deb https://mirrors.tuna.tsinghua.edu.cn/ubuntu/ noble main restricted universe multiverse
deb https://mirrors.tuna.tsinghua.edu.cn/ubuntu/ noble-updates main restricted universe multiverse
deb https://mirrors.tuna.tsinghua.edu.cn/ubuntu/ noble-backports main restricted universe multiverse
deb https://mirrors.tuna.tsinghua.edu.cn/ubuntu/ noble-security main restricted universe multiverse
EOF

RUN apt-get -o Acquire::Retries=5 update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    acl \
    attr \
    automake \
    bc \
    ca-certificates \
    dbench \
    dump \
    e2fsprogs \
    exfatprogs \
    f2fs-tools \
    fio \
    fuse3 \
    gawk \
    gcc \
    git \
    indent \
    libacl1-dev \
    libaio-dev \
    libcap-dev \
    libgdbm-dev \
    libtool \
    libtool-bin \
    liburing-dev \
    libuuid1 \
    lvm2 \
    make \
    ocfs2-tools \
    psmisc \
    python3 \
    quota \
    sed \
    sqlite3 \
    udftools \
    uuid-dev \
    uuid-runtime \
    xfsdump \
    xfslibs-dev \
    xfsprogs && \
    git clone --depth=1 -b "$XFSTESTS_BRANCH" "$XFSTESTS_REPO" /opt/xfstests && \
    make -C /opt/xfstests && \
    rm -rf /var/lib/apt/lists/*

RUN sed -i 's/#user_allow_other/user_allow_other/' /etc/fuse.conf || \
    echo "user_allow_other" >> /etc/fuse.conf

COPY --from=builder /usr/src/rk8s/project/target/release/examples/persistence_demo /usr/local/bin/slayerfs-persistence
COPY --from=builder /usr/src/rk8s/project/target/release/slayerfs /usr/local/bin/slayerfs
COPY slayerfs/redis.yml /opt/slayerfs/redis.yml
COPY slayerfs/redis-docker.yml /opt/slayerfs/redis-docker.yml
COPY slayerfs/slayerfs-sqlite.yml /opt/slayerfs/sqlite.yml
COPY slayerfs/tests/scripts/xfstests_slayer.exclude /opt/slayerfs/xfstests_slayer.exclude

WORKDIR /opt/slayerfs
RUN mkdir -p /var/lib/slayerfs/data /mnt/slayerfs /var/log/slayerfs /opt/slayerfs/results

ENTRYPOINT ["/usr/local/bin/slayerfs-persistence"]
