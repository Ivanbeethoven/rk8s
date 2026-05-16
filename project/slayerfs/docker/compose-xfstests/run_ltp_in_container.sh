#!/usr/bin/env bash

set -euo pipefail

log()  { echo "[$(date '+%H:%M:%S')] $*"; }
info() { log "INFO  $*"; }
ok()   { log "OK    $*"; }
err()  { log "ERROR $*" >&2; }

config_path="${SLAYERFS_CONFIG_PATH:-/run/slayerfs/config.yaml}"
mount_dir="${SLAYERFS_MOUNT_POINT:-/mnt/slayerfs}"
data_backend="${SLAYERFS_DATA_BACKEND:-local-fs}"
data_dir="${SLAYERFS_DATA_DIR:-${SLAYERFS_HOME:-/var/lib/slayerfs}/data}"
meta_backend="${SLAYERFS_META_BACKEND:-redis}"
meta_url="${SLAYERFS_META_URL:-}"
meta_etcd_urls="${SLAYERFS_META_ETCD_URLS:-http://etcd:2379}"
sqlite_path="${SLAYERFS_SQLITE_PATH:-${SLAYERFS_HOME:-/var/lib/slayerfs}/metadata.db}"
log_file="${SLAYERFS_LOG_FILE:-/artifacts/slayerfs.log}"
ltp_dir="${LTP_DIR:-/opt/ltp}"
artifact_root="${SLAYERFS_ARTIFACT_ROOT:-/artifacts}"
artifact_dir="${SLAYERFS_ARTIFACT_DIR:-}"

ltp_scenarios="${LTP_SCENARIOS:-fs}"
ltp_extra_args="${LTP_EXTRA_ARGS:-}"
ltp_skip_files="${LTP_SKIP_FILES:-}"

write_config() {
    mkdir -p "$(dirname "$config_path")" "$mount_dir"
    if [[ "$data_backend" == "local-fs" ]]; then
        mkdir -p "$data_dir"
    fi

    {
        echo "mount_point: $mount_dir"
        echo
        case "$data_backend" in
            local-fs)
                cat <<YAML
data:
  backend: local-fs
  localfs:
    data_dir: ${data_dir}
YAML
                ;;
            s3)
                bucket="${SLAYERFS_S3_BUCKET:-slayerfs-data}"
                region="${SLAYERFS_S3_REGION:-us-east-1}"
                endpoint="${SLAYERFS_S3_ENDPOINT:-http://rustfs:9000}"
                force_path="${SLAYERFS_S3_FORCE_PATH_STYLE:-true}"
                part_size="${SLAYERFS_S3_PART_SIZE:-16777216}"
                max_conc="${SLAYERFS_S3_MAX_CONCURRENCY:-8}"
                cat <<YAML
data:
  backend: s3
  s3:
    bucket: ${bucket}
    region: ${region}
    part_size: ${part_size}
    max_concurrency: ${max_conc}
    force_path_style: ${force_path}
    endpoint: ${endpoint}
YAML
                ;;
            *)
                err "unsupported SLAYERFS_DATA_BACKEND: $data_backend"
                exit 1
                ;;
        esac
        echo

        case "$meta_backend" in
            sqlite)
                mkdir -p "$(dirname "$sqlite_path")"
                local url="${meta_url:-sqlite://${sqlite_path}?mode=rwc}"
                cat <<YAML
meta:
  backend: sqlx
  sqlx:
    url: "$url"
YAML
                ;;
            redis)
                if [[ -z "$meta_url" ]]; then
                    err "SLAYERFS_META_URL must not be empty (redis)"
                    exit 1
                fi
                cat <<YAML
meta:
  backend: redis
  redis:
    url: "$meta_url"
YAML
                ;;
            etcd)
                cat <<YAML
meta:
  backend: etcd
  etcd:
    urls:
YAML
                local old_ifs="$IFS"
                IFS=','
                for url in $meta_etcd_urls; do
                    echo "      - \"${url}\""
                done
                IFS="$old_ifs"
                ;;
            *)
                err "unsupported SLAYERFS_META_BACKEND: $meta_backend"
                exit 1
                ;;
        esac

        echo
        cat <<YAML
layout:
  chunk_size: ${SLAYERFS_CHUNK_SIZE:-67108864}
  block_size: ${SLAYERFS_BLOCK_SIZE:-4194304}
YAML
    } >"$config_path"
}

install_mount_helper() {
    local helper="/usr/sbin/mount.fuse.slayerfs"
    local baked_log_file="${log_file:-/artifacts/slayerfs.log}"
    local baked_fuse_log_file="${fuse_log_file:-}"

    cat >"$helper" <<SCRIPTEOF
#!/usr/bin/env bash
set -euo pipefail

export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:\$PATH"

src="\${1:-}"
target="\${2:-}"
shift 2 || true

config_path="\${SLAYERFS_CONFIG_PATH:-/run/slayerfs/config.yaml}"
log_file="${baked_log_file}"

mkdir -p "\$target" "\$(dirname "\$log_file")"

SCRIPTEOF

    if [[ -n "$baked_fuse_log_file" ]]; then
        cat >>"$helper" <<SCRIPTEOF
mkdir -p "\$(dirname "${baked_fuse_log_file}")"
SLAYERFS_FUSE_OP_LOG=1 SLAYERFS_FUSE_LOG_FILE="${baked_fuse_log_file}" \\
    /usr/local/bin/slayerfs mount --config "\$config_path" "\$target" >>"\$log_file" 2>&1 &
SCRIPTEOF
    else
        cat >>"$helper" <<'SCRIPTEOF'
/usr/local/bin/slayerfs mount --config "$config_path" "$target" >>"$log_file" 2>&1 &
SCRIPTEOF
    fi

    cat >>"$helper" <<'SCRIPTEOF'
sleep "${SLAYERFS_MOUNT_WAIT_SECS:-1}"
exit 0
SCRIPTEOF
    chmod +x "$helper"
}

prepare_results_dir() {
    mkdir -p "$artifact_dir/results" "$artifact_dir/output"
}

copy_artifacts() {
    mkdir -p "$artifact_dir"
    if [[ -f "$log_file" && "$log_file" != "$artifact_dir/slayerfs.log" ]]; then
        cp -f "$log_file" "$artifact_dir/slayerfs.log" || true
    fi
    if [[ -n "${SLAYERFS_FUSE_LOG_FILE:-}" && -f "${SLAYERFS_FUSE_LOG_FILE}" && "${SLAYERFS_FUSE_LOG_FILE}" != "$artifact_dir/slayerfs_fuse_ops.log" ]]; then
        cp -f "${SLAYERFS_FUSE_LOG_FILE}" "$artifact_dir/slayerfs_fuse_ops.log" || true
    fi
    if [[ -f "$config_path" ]]; then
        cp -f "$config_path" "$artifact_dir/backend.yml" || true
    fi

    if [[ -d "$ltp_dir/results" ]]; then
        cp -a "$ltp_dir/results/." "$artifact_dir/results/" 2>/dev/null || true
    fi
    if [[ -d "$ltp_dir/output" ]]; then
        cp -a "$ltp_dir/output/." "$artifact_dir/output/" 2>/dev/null || true
    fi

    chmod -R a+rwX "$artifact_dir" >/dev/null 2>&1 || true
}

cleanup() {
    while mount | grep -q " on $mount_dir "; do
        fusermount3 -u "$mount_dir" >/dev/null 2>&1 \
            || umount -f "$mount_dir" >/dev/null 2>&1 \
            || umount -l "$mount_dir" >/dev/null 2>&1 \
            || sleep 1
    done
    pkill -f "/usr/local/bin/slayerfs mount" >/dev/null 2>&1 || true
}

on_exit() {
    local status=$?
    copy_artifacts || true
    if [[ -x /usr/local/bin/ltp_report.sh ]]; then
        bash /usr/local/bin/ltp_report.sh "$artifact_dir" --no-tar >/dev/null 2>&1 || true
    fi
    cleanup || true
    trap - EXIT
    exit "$status"
}

run_ltp() {
    local -a ltp_args=(-S "$ltp_scenarios" -d "$mount_dir" -q)

    if [[ -n "$ltp_skip_files" ]]; then
        ltp_args+=(-s "$ltp_skip_files")
    fi

    if [[ -n "$ltp_extra_args" ]]; then
        read -r -a extra <<<"$ltp_extra_args"
        ltp_args+=("${extra[@]}")
    fi

    info "LTP scenarios: $ltp_scenarios, mount: $mount_dir"
    info "LTP args: ${ltp_args[*]}"

    export LTP_DEV="$mount_dir"
    export LTP_DEV_FS_TYPE="fuse"

    cd "$ltp_dir"
    set +e
    ./runltp "${ltp_args[@]}" 2>&1 | tee -a "$artifact_dir/ltp.console.log"
    status="${PIPESTATUS[0]}"
    set -e

    if [[ -f "$artifact_dir/ltp.console.log" ]]; then
        cp -f "$artifact_dir/ltp.console.log" "$artifact_dir/results/ltp.console.log" >/dev/null 2>&1 || true
    fi

    return "$status"
}

main() {
    local normalized_fuse_op_log

    if [[ -z "$artifact_dir" ]]; then
        ts="$(date +%s)-$RANDOM"
        artifact_dir="${artifact_root%/}/run-${ts}"
    fi
    mkdir -p "$artifact_dir"
    chmod a+rwx "$artifact_dir" >/dev/null 2>&1 || true
    log_file="$artifact_dir/slayerfs.log"
    export SLAYERFS_LOG_FILE="$log_file"
    normalized_fuse_op_log="${SLAYERFS_FUSE_OP_LOG:-0}"
    normalized_fuse_op_log="${normalized_fuse_op_log,,}"
    if [[ "$normalized_fuse_op_log" =~ ^(1|true|yes|on)$ ]]; then
        fuse_log_file="$artifact_dir/slayerfs_fuse_ops.log"
        export SLAYERFS_FUSE_LOG_FILE="$fuse_log_file"
    else
        fuse_log_file=""
        unset SLAYERFS_FUSE_LOG_FILE || true
    fi

    trap on_exit EXIT INT TERM

    info "write SlayerFS config: $config_path"
    write_config

    info "install mount helper: /usr/sbin/mount.fuse.slayerfs"
    install_mount_helper

    info "prepare results dir"
    prepare_results_dir

    info "run LTP ($ltp_scenarios): mount=$mount_dir"
    set +e
    run_ltp
    status=$?
    set -e

    copy_artifacts || true
    if [[ -x /usr/local/bin/ltp_report.sh ]]; then
        bash /usr/local/bin/ltp_report.sh "$artifact_dir" --no-tar >/dev/null 2>&1 || true
    fi

    if [[ "$status" -eq 0 ]]; then
        ok "LTP PASS"
    else
        err "LTP FAIL (exit=$status)"
    fi
    ok "artifacts: $artifact_dir"
    exit "$status"
}

main "$@"
