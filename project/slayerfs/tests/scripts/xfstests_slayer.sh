#!/bin/bash

# 用途:
#   为本地 SlayerFS FUSE 挂载准备 xfstests 运行环境，并执行指定的 xfstests 用例。
#   脚本支持通过环境变量覆写二进制、配置、目录与 xfstests 仓库位置，便于在本机、CI 或 VM 中复用。

set -euo pipefail

current_dir=$(dirname "$(realpath "$0")")
workspace_dir="${SLAYERFS_WORKSPACE_DIR:-$(realpath "$current_dir/../../..")}"
config_path="${SLAYERFS_CONFIG_PATH:-$workspace_dir/slayerfs/slayerfs-sqlite.yml}"
backend_dir="${SLAYERFS_BACKEND_DIR:-/tmp/data}"
mount_dir="${SLAYERFS_MOUNT_DIR:-/tmp/mount}"
log_file="${SLAYERFS_LOG_FILE:-/tmp/slayerfs.log}"
persistence_bin="${SLAYERFS_BIN_PATH:-$workspace_dir/target/release/examples/persistence_demo}"
xfstests_repo="${XFSTESTS_REPO:-https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git}"
xfstests_branch="${XFSTESTS_BRANCH:-v2023.12.10}"
xfstests_dir="${XFSTESTS_DIR:-/tmp/xfstests-dev}"
mount_helper_path="${SLAYERFS_MOUNT_HELPER_PATH:-/usr/sbin/mount.fuse.slayerfs}"
slayerfs_rust_log="${SLAYERFS_RUST_LOG:-slayerfs=info,rfuse3::raw::logfs=debug}"
slayerfs_fuse_op_log="${SLAYERFS_FUSE_OP_LOG:-1}"
mount_wait_secs="${SLAYERFS_MOUNT_WAIT_SECS:-1}"
install_deps="${XFSTESTS_INSTALL_DEPS:-1}"
reclone_xfstests="${XFSTESTS_FORCE_RECLONE:-1}"
exclude_file="$current_dir/xfstests_slayer.exclude"

if [[ ! -f "$persistence_bin" ]]; then
    echo "Cannot find slayerfs persistence_demo binary at: $persistence_bin"
    echo "Please run: cargo build -p slayerfs --example persistence_demo --release"
    exit 1
fi

if [[ ! -f "$config_path" ]]; then
    echo "Cannot find SlayerFS config file at: $config_path"
    exit 1
fi

cleanup() {
    sudo pkill -f "$persistence_bin" >/dev/null 2>&1 || true
    while mount | grep -q " on $mount_dir "; do
        sudo fusermount3 -u "$mount_dir" >/dev/null 2>&1 \
            || sudo umount -f "$mount_dir" >/dev/null 2>&1 \
            || sudo umount -l "$mount_dir" >/dev/null 2>&1 \
            || sleep 1
    done
}

install_xfstests_deps() {
    export DEBIAN_FRONTEND=noninteractive

    # Ensure swap exists so apt-get is not OOM-killed.
    # fallocate is instant; swapon verifies the blocks are usable.
    if ! swapon --show | grep -q .; then
        fallocate -l 4G /swapfile 2>/dev/null \
            || dd if=/dev/zero of=/swapfile bs=1M count=4096 status=none
        chmod 600 /swapfile
        mkswap /swapfile >/dev/null
        swapon /swapfile
        echo "[xfstests] Swap enabled: $(free -h | awk '/^Swap/{print $2}')"
    fi

    # Stop background apt/unattended-upgrades so it doesn't hold the apt lock
    # or leave corrupted partial downloads (which cause "Transaction already aborted").
    sudo systemctl stop unattended-upgrades apt-daily.service apt-daily-upgrade.service \
        apt-daily.timer apt-daily-upgrade.timer 2>/dev/null || true
    sudo pkill -9 -x apt-get 2>/dev/null || true
    sleep 2

    # Remove stale locks and partial download files left by killed apt processes.
    sudo rm -f  /var/lib/apt/lists/lock \
                /var/cache/apt/archives/lock \
                /var/lib/dpkg/lock \
                /var/lib/dpkg/lock-frontend
    sudo rm -rf /var/lib/apt/lists/partial \
                /var/cache/apt/archives/partial
    sudo dpkg --configure -a 2>/dev/null || true

    # Disable deb-src to avoid downloading huge Sources indices (which can use
    # several GB of memory during decompression).
    sudo sed -i 's/^deb-src /#deb-src /' /etc/apt/sources.list 2>/dev/null || true

    # apt-get update with retry: unattended-upgrades may still race; retry once.
    if ! sudo apt-get update -qq 2>&1; then
        echo "[xfstests] apt-get update failed, cleaning and retrying..."
        sudo rm -rf /var/lib/apt/lists/partial /var/cache/apt/archives/partial
        sudo rm -f  /var/lib/apt/lists/lock /var/lib/dpkg/lock /var/lib/dpkg/lock-frontend
        sudo apt-get update -qq
    fi

    # Runtime deps: always required when running xfstests against a FUSE mount.
    sudo apt-get install -y -qq --no-install-recommends \
        acl attr bc dbench dump e2fsprogs fio gawk \
        ca-certificates libuuid1 lvm2 make psmisc python3 quota sed \
        uuid-runtime xfsprogs sqlite3 fuse3 \
        exfatprogs f2fs-tools udftools xfsdump || true

    # Build deps: only needed when compiling xfstests from source.
    # Skip them when a prebuilt tarball is available to save memory and time.
    local use_prebuilt="${XFSTESTS_PREBUILT_TAR:-1}"
    local script_dir
    script_dir="$(dirname "$(realpath "$0")")"
    if [[ "$reclone_xfstests" == "1" ]] || \
       [[ "$use_prebuilt" != "1" ]] || \
       [[ -z "$(ls "${script_dir}/xfstests-prebuilt/"*.tar.gz 2>/dev/null | head -1 || true)" ]]; then
        sudo apt-get install -y -qq \
            automake gcc git indent libacl1-dev libaio-dev libcap-dev libgdbm-dev libtool \
            libtool-bin liburing-dev uuid-dev xfslibs-dev
        sudo apt-get install -y -qq ocfs2-tools || true
        sudo apt-get install -y -qq "linux-headers-$(uname -r)" || true
    fi
}

prepare_xfstests_tree() {
    # Prefer a prebuilt tarball (stored in the repo via git lfs) to avoid slow
    # network clone+compile during test runs.  The tarball is expected to be
    # placed next to this script as:
    #   xfstests-prebuilt/<anything>.tar.gz
    # The archive must contain a top-level directory (e.g. "xfstests/") whose
    # contents mirror a `make install` tree (has "check", "common/", "tests/",
    # "src/").
    # Set XFSTESTS_PREBUILT_TAR=0 or XFSTESTS_FORCE_RECLONE=1 to skip this
    # and fall back to clone+build.
    local use_prebuilt="${XFSTESTS_PREBUILT_TAR:-1}"
    local prebuilt_tar=""
    local script_dir
    script_dir="$(dirname "$(realpath "$0")")"

    if [[ "$reclone_xfstests" == "1" ]]; then
        use_prebuilt="0"
    fi

    if [[ "$use_prebuilt" == "1" ]]; then
        prebuilt_tar="$(ls "${script_dir}/xfstests-prebuilt/"*.tar.gz 2>/dev/null | head -1 || true)"
        if [[ -z "$prebuilt_tar" ]]; then
            echo "[xfstests] No prebuilt tarball found under ${script_dir}/xfstests-prebuilt/, falling back to clone+build."
            use_prebuilt="0"
        fi
    fi

    if [[ "$use_prebuilt" == "1" ]]; then
        echo "[xfstests] Using prebuilt tarball: $prebuilt_tar"
        sudo rm -rf "$xfstests_dir"
        sudo mkdir -p "$(dirname "$xfstests_dir")"
        # The tarball has a single top-level directory; strip it and extract
        # directly into $xfstests_dir.
        local top_dir
        top_dir="$(tar -tzf "$prebuilt_tar" | head -1 | cut -d/ -f1)"
        sudo mkdir -p "$xfstests_dir"
        sudo tar -xzf "$prebuilt_tar" -C "$(dirname "$xfstests_dir")" \
            --transform "s|^${top_dir}|$(basename "$xfstests_dir")|"
        # Ensure check and helpers are executable.
        sudo chmod +x "$xfstests_dir/check" "$xfstests_dir/src/"* 2>/dev/null || true
        echo "[xfstests] Extracted to $xfstests_dir"
        return 0
    fi

    # Fallback: clone and build from source.
    if [[ "$reclone_xfstests" == "1" ]]; then
        sudo rm -rf "$xfstests_dir"
    fi

    if [[ ! -d "$xfstests_dir" ]]; then
        git clone --depth=1 -b "$xfstests_branch" "$xfstests_repo" "$xfstests_dir"
    fi

    (
        cd "$xfstests_dir"
        make -j"$(nproc)"
        sudo make install
    )
}

write_local_config() {
    cat >"$xfstests_dir/local.config" <<EOF
export TEST_DEV=slayerfs
export TEST_DIR=$mount_dir
#export SCRATCH_DEV=slayerfs
#export SCRATCH_MNT=/tmp/test2/merged
export FSTYP=fuse
export FUSE_SUBTYP=.slayerfs

# Deleting the following command results in:
# TEST_DEV=slayerfs is mounted but not a type fuse filesystem.
export DF_PROG="df -T -P -a"
EOF
}

install_mount_helper() {
    sudo tee "$mount_helper_path" >/dev/null <<EOF
#!/bin/bash
set -euo pipefail

export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:\$PATH"

ulimit -n 1048576
CONFIG_PATH="$config_path"
LOG_FILE="$log_file"
PERSISTENCE_BIN="$persistence_bin"

BACKEND_DIR="$backend_dir"
MOUNT_DIR="$mount_dir"
SLAYERFS_RUST_LOG="$slayerfs_rust_log"
SLAYERFS_FUSE_OP_LOG="$slayerfs_fuse_op_log"

if [[ "\$SLAYERFS_FUSE_OP_LOG" == "1" ]]; then
  export RUST_LOG="\$SLAYERFS_RUST_LOG"
fi

"\$PERSISTENCE_BIN" \
  -c "\$CONFIG_PATH" \
  -s "\$BACKEND_DIR" \
  -m "\$MOUNT_DIR" >>"\$LOG_FILE" 2>&1 &
sleep "$mount_wait_secs"
EOF
    sudo chmod +x "$mount_helper_path"
}

run_xfstests() {
    local -a check_args=()

    if [[ -n "${XFSTESTS_CHECK_ARGS:-}" ]]; then
        read -r -a check_args <<<"${XFSTESTS_CHECK_ARGS}"
    elif [[ -n "${XFSTESTS_CASES:-}" ]]; then
        read -r -a check_args <<<"${XFSTESTS_CASES}"
        check_args=(-fuse "${check_args[@]}")
    else
        check_args=(-fuse -E xfstests_slayer.exclude)
    fi

    (
        cd "$xfstests_dir"
        sudo LC_ALL=C ./check "${check_args[@]}"
    )
}

trap cleanup EXIT

cleanup
sudo rm -rf "$backend_dir" "$mount_dir"
sudo mkdir -p "$backend_dir" "$mount_dir"
sudo rm -f "$log_file"
sudo mkdir -p "$(dirname "$mount_helper_path")"
sudo mkdir -p "$(dirname "$log_file")"
sudo chmod 1777 "$backend_dir"
sudo chmod 755 "$mount_dir"
sudo chown root:root "$mount_dir"
sudo grep -q '^user_allow_other' /etc/fuse.conf 2>/dev/null || echo 'user_allow_other' | sudo tee -a /etc/fuse.conf >/dev/null

if [[ "$install_deps" == "1" ]]; then
    install_xfstests_deps
fi

prepare_xfstests_tree
write_local_config
install_mount_helper

if [[ -f "$exclude_file" ]]; then
    sudo cp "$exclude_file" "$xfstests_dir/xfstests_slayer.exclude"
fi

echo "====> Start to run xfstests."
run_xfstests
