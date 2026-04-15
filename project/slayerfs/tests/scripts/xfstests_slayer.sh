#!/bin/bash

set -euo pipefail

current_dir=$(dirname "$(realpath "$0")")
workspace_dir=$(realpath "$current_dir/../../..")
default_config="$workspace_dir/slayerfs/slayerfs-sqlite.yml"
config_path="${SLAYERFS_CONFIG:-$default_config}"
backend_root=/tmp/slayerfs-xfstests
mount_dir=/tmp/mount
scratch_mount_dir=/tmp/test2/merged
log_dir=/tmp/slayerfs-logs
persistence_bin="$workspace_dir/target/release/examples/persistence_demo"
xfstests_repo=https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git
xfstests_branch="${XFSTESTS_BRANCH:-v2023.12.10}"
slayerfs_rust_log="${slayerfs_rust_log:-slayerfs=info,rfuse3::raw::logfs=debug}"
slayerfs_fuse_op_log="${slayerfs_fuse_op_log:-1}"

if [[ ! -f "$persistence_bin" ]]; then
    echo "Cannot find slayerfs persistence_demo binary."
    echo "Please run: cargo build -p slayerfs --example persistence_demo --release"
    exit 1
fi

if [[ ! -f "$config_path" ]]; then
    echo "Cannot find slayerfs config: $config_path"
    exit 1
fi

for dir in "$mount_dir" "$scratch_mount_dir"; do
    while mount | awk '{print $3}' | grep -Fxq "$dir"; do
        sudo umount -f "$dir" || sleep 1
    done
    sudo rm -rf "$dir"
done
sudo rm -rf "$backend_root"
sudo rm -rf /tmp/xfstests-dev
sudo mkdir -p "$backend_root" "$mount_dir" "$scratch_mount_dir" "$log_dir"
sudo rm -f "$log_dir"/*.log /tmp/slayerfs.log

export DEBIAN_FRONTEND=noninteractive
if command -v apt-get >/dev/null 2>&1; then
    sudo apt-get update
    sudo apt-get install -y acl attr automake bc dbench dump e2fsprogs fio gawk \
        gcc git indent libacl1-dev libaio-dev libcap-dev libgdbm-dev libtool \
        libtool-bin liburing-dev libuuid1 lvm2 make psmisc python3 quota sed \
        uuid-dev uuid-runtime xfsprogs sqlite3 \
        fuse3
    sudo apt-get install -y exfatprogs f2fs-tools ocfs2-tools udftools xfsdump \
        xfslibs-dev
    sudo apt-get install -y "linux-headers-$(uname -r)" || true
elif command -v dnf >/dev/null 2>&1; then
    sudo dnf install -y \
        acl attr automake bc dump e2fsprogs fio gawk gcc git hostname indent \
        libacl-devel libaio-devel libcap-devel gdbm-devel libtool \
        liburing-devel lvm2 make psmisc python3 quota quota-devel sed \
        sqlite xfsprogs xfsprogs-devel fuse3 util-linux-devel uuidd
    sudo dnf install -y f2fs-tools xfsdump || true
    sudo dnf install -y dbench exfatprogs ocfs2-tools udftools kernel-headers || true
else
    echo "Unsupported package manager: need apt-get or dnf"
    exit 1
fi

# clone xfstests and install.
cd /tmp/
git clone --depth=1 -b "$xfstests_branch" "$xfstests_repo"
cd xfstests-dev
make
sudo make install

# overwrite local config.
cat >local.config <<CFG
export TEST_DEV=slayerfs_test
export TEST_DIR=$mount_dir
export SCRATCH_DEV=slayerfs_scratch
export SCRATCH_MNT=$scratch_mount_dir
export FSTYP=fuse
export FUSE_SUBTYP=.slayerfs

# Deleting the following command will result in an error:
# TEST_DEV=slayerfs is mounted but not a type fuse filesystem.
export DF_PROG="df -T -P -a"
CFG

# create fuse mount script for slayerfs.
sudo tee /usr/sbin/mount.fuse.slayerfs >/dev/null <<EOF_HELPER
#!/bin/bash
set -euo pipefail

export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:\$PATH"

ulimit -n 1048576
CONFIG_PATH="$config_path"
BACKEND_ROOT="$backend_root"
LOG_DIR="$log_dir"
PERSISTENCE_BIN="$persistence_bin"
SLAYERFS_RUST_LOG="$slayerfs_rust_log"
SLAYERFS_FUSE_OP_LOG="$slayerfs_fuse_op_log"

source_arg=""
mount_target=""
args=("\$@")
index=0
while [[ \$index -lt \${#args[@]} ]]; do
  arg="\${args[\$index]}"
  case "\$arg" in
    -o|-O|-t)
      index=\$((index + 2))
      ;;
    -*)
      index=\$((index + 1))
      ;;
    *)
      if [[ -z "\$source_arg" ]]; then
        source_arg="\$arg"
      elif [[ -z "\$mount_target" ]]; then
        mount_target="\$arg"
      fi
      index=\$((index + 1))
      ;;
  esac
done

if [[ -z "\$mount_target" ]]; then
  echo "mount.fuse.slayerfs: could not determine mount target from args: \$*" >&2
  exit 1
fi

mount_key=\$(printf '%s' "\$mount_target" | sed 's#^/##; s#[/[:space:]]#-#g; s#[^A-Za-z0-9._-]#_#g')
if [[ -z "\$mount_key" ]]; then
  mount_key=root
fi

backend_dir="\$BACKEND_ROOT/\$mount_key"
log_file="\$LOG_DIR/\$mount_key.log"
mkdir -p "\$backend_dir" "\$mount_target" "\$LOG_DIR"

export SLAYERFS_FS_NAME="\${source_arg:-slayerfs}"

if [[ "\$SLAYERFS_FUSE_OP_LOG" == "1" ]]; then
  export RUST_LOG="\$SLAYERFS_RUST_LOG"
fi

echo "[\$(date --iso-8601=seconds)] mount source=\$source_arg target=\$mount_target fsname=\$SLAYERFS_FS_NAME backend=\$backend_dir" >>"\$log_file"
"\$PERSISTENCE_BIN" \
  -c "\$CONFIG_PATH" \
  -s "\$backend_dir" \
  -m "\$mount_target" >>"\$log_file" 2>&1 &
sleep 1
EOF_HELPER
sudo chmod +x /usr/sbin/mount.fuse.slayerfs

echo "====> Start to run xfstests."
# Copy exclude list
sudo cp "$current_dir/xfstests_slayer.exclude" /tmp/xfstests-dev/

# run tests.
cd /tmp/xfstests-dev
if [[ -n "${XFSTESTS_CASES:-}" ]]; then
    read -r -a selected_cases <<<"${XFSTESTS_CASES}"
    sudo LC_ALL=C ./check -fuse "${selected_cases[@]}"
else
    sudo LC_ALL=C ./check -fuse -E xfstests_slayer.exclude
fi
