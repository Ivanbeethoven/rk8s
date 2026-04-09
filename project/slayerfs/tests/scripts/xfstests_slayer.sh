#!/bin/bash

# 用途:
#   为本地 SlayerFS FUSE 挂载准备 xfstests 运行环境，并执行指定的 xfstests 用例。
#   脚本支持通过环境变量覆写二进制、配置、目录与 xfstests 仓库位置，便于在本机、CI 或 VM 中复用。

set -euo pipefail

# Redirect all output (stdout+stderr) to a log file for post-mortem debugging.
_LOG=/tmp/xfstests-script.log
exec > >(tee -a "$_LOG") 2>&1

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

    # Stale partial downloads cause "Transaction already aborted" errors.
    # Main kill/swap setup already done in the script body; just clean leftovers.
    sudo rm -rf /var/lib/apt/lists/partial /var/cache/apt/archives/partial 2>/dev/null || true
    sudo dpkg --configure -a 2>/dev/null || true

    # Disable deb-src to avoid fetching large Sources indices.
    sudo sed -i 's/^deb-src /#deb-src /' /etc/apt/sources.list 2>/dev/null || true

    # apt-get update with retry.
    if ! sudo bash -c 'export MALLOC_ARENA_MAX=1; apt-get update -qq' 2>&1; then
        echo "[xfstests] apt-get update failed, cleaning and retrying..."
        sudo rm -rf /var/lib/apt/lists/partial /var/cache/apt/archives/partial
        sudo rm -f  /var/lib/apt/lists/lock /var/lib/dpkg/lock /var/lib/dpkg/lock-frontend
        sudo bash -c 'export MALLOC_ARENA_MAX=1; apt-get update -qq'
    fi

    # Runtime deps: always required when running xfstests against a FUSE mount.
    sudo bash -c 'export MALLOC_ARENA_MAX=1 DEBIAN_FRONTEND=noninteractive; \
        apt-get install -y -qq --no-install-recommends \
        acl attr bc dbench dump e2fsprogs fio gawk \
        ca-certificates libuuid1 lvm2 make psmisc python3 quota sed \
        uuid-runtime xfsprogs sqlite3 fuse3 \
        exfatprogs f2fs-tools udftools xfsdump' || true

    # Build deps: only needed when compiling xfstests from source.
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
        # The top-level directory inside our prebuilt tarball is always
        # 'xfstests' (it was built that way).  We hardcode it here to avoid
        # `tar | head` which triggers SIGPIPE and kills the script under
        # `set -o pipefail`.
        local top_dir="xfstests"
        sudo mkdir -p "$xfstests_dir"
        sudo tar -xzf "$prebuilt_tar" -C "$(dirname "$xfstests_dir")" \
            --transform "s|^${top_dir}|$(basename "$xfstests_dir")|"
        # Ensure check and helpers are executable.
        sudo chmod +x "$xfstests_dir/check" "$xfstests_dir/src/"* 2>/dev/null || true
        echo "[xfstests] Extracted to $xfstests_dir"

        # Install bundled tools (xfs_io, fusermount3, bc) and their libs
        # into system-wide paths so they're accessible without LD_LIBRARY_PATH.
        local bundled_dir="$xfstests_dir/bundled"
        if [[ -d "$bundled_dir" ]]; then
            echo "[xfstests] Installing bundled tools from $bundled_dir..."
            # Libs first so ldconfig covers them
            if [[ -d "$bundled_dir/lib" ]]; then
                sudo cp -n "$bundled_dir/lib/"* /usr/local/lib/ 2>/dev/null || true
                sudo ldconfig 2>/dev/null || true
            fi
            # Binaries
            for b in "$bundled_dir/bin/"*; do
                [[ -f "$b" ]] || continue
                local dest_dir
                case "$(basename "$b")" in
                    fusermount3) dest_dir=/usr/local/bin ;;
                    *)           dest_dir=/usr/local/sbin ;;
                esac
                sudo install -m 755 "$b" "$dest_dir/$(basename "$b")"
            done
            echo "[xfstests] Bundled tools installed: $(ls $bundled_dir/bin/ 2>/dev/null | tr '\n' ' ')"
        fi
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
        if [[ -f "$xfstests_dir/xfstests_slayer.exclude" ]]; then
            check_args=(-fuse -E xfstests_slayer.exclude "${check_args[@]}")
        else
            check_args=(-fuse "${check_args[@]}")
        fi
    else
        check_args=(-fuse -E xfstests_slayer.exclude)
    fi

    (
        cd "$xfstests_dir"
        # Add xfstests dir to PATH so bundled tools (bc→busybox, etc.) are found
        # even when the system image doesn't have them installed.
        export PATH="$xfstests_dir:$PATH"
        # sudo resets PATH via secure_path in /etc/sudoers, so the export above
        # won't take effect inside the sudo-rooted check process.
        # Solve this by linking our bundled bc into /usr/local/bin which is always
        # in sudo's default secure_path.
        sudo ln -sf "$xfstests_dir/bc" /usr/local/bin/bc 2>/dev/null || true
        sudo -E LC_ALL=C ./check "${check_args[@]}"
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

# Prevent unattended-upgrades from consuming all VM RAM.
# We AVOID systemctl (which uses D-Bus) because D-Bus can block under OOM,
# causing the SSH connection to be lost and the test to fail.
# Instead: directly SIGKILL the processes and create mask symlinks on the
# filesystem (which is what systemctl-mask does under the hood, without D-Bus).
sudo mkdir -p /etc/systemd/system
for svc in unattended-upgrades apt-daily.service apt-daily-upgrade.service \
           apt-daily.timer apt-daily-upgrade.timer; do
    sudo ln -sf /dev/null "/etc/systemd/system/${svc}" 2>/dev/null || true
done
sudo pkill -9 -f 'unattended' 2>/dev/null || true
sudo pkill -9 -x apt-get 2>/dev/null || true
sleep 3

# Wait for apt-get to actually disappear (max 60 seconds).
# Even with SIGKILL, the kernel may take a moment to reclaim memory.
# Proceeding while apt-get still holds 7-8 GB can cause OOM kills.
echo "[vm-setup] waiting for apt-get to exit..."
for _i in $(seq 1 30); do
    if ! pgrep -x apt-get >/dev/null 2>&1; then
        break
    fi
    sleep 2
done

sudo rm -f /var/lib/apt/lists/lock /var/cache/apt/archives/lock \
           /var/lib/dpkg/lock /var/lib/dpkg/lock-frontend 2>/dev/null || true

if ! swapon --show | grep -q .; then
    # Use dd (not fallocate) to ensure real disk blocks are allocated on the
    # qcow2-backed filesystem.  fallocate may create a sparse file in qcow2
    # that has no actual backing blocks, making the swap unusable.
    dd if=/dev/zero of=/swapfile bs=1M count=4096 status=none oflag=direct
    chmod 600 /swapfile
    mkswap /swapfile >/dev/null
    swapon /swapfile
    echo "[vm-setup] swap ready: $(free -h | awk '/^Swap/{print $2}')"
fi

if [[ "$install_deps" == "1" ]]; then
    install_xfstests_deps
else
    # Even when skipping full deps installation, install the bare minimum
    # tools that xfstests requires at runtime.
    # bc, xfs_io, and fusermount3 are bundled inside the prebuilt tarball and
    # will be installed by prepare_xfstests_tree() above, so no apt is needed
    # for those.  We only check+install fuse3 kernel module userspace here.
    if ! command -v fusermount3 >/dev/null 2>&1; then
        # fuse3 might be installed by the bundled tools above; check again later.
        # As a last resort attempt apt, but allow failure since bundled covers it.
        sudo bash -c 'export MALLOC_ARENA_MAX=1 DEBIAN_FRONTEND=noninteractive; \
            apt-get install -y -qq --no-install-recommends fuse3' 2>/dev/null \
        || true
    fi
fi

prepare_xfstests_tree
write_local_config
install_mount_helper

if [[ -f "$exclude_file" ]]; then
    sudo cp "$exclude_file" "$xfstests_dir/xfstests_slayer.exclude"
fi

echo "====> Start to run xfstests."
run_xfstests
