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
xfstests_dir="${XFSTESTS_DIR:-/opt/xfstests-dev}"
artifact_root="${SLAYERFS_ARTIFACT_ROOT:-/artifacts}"
artifact_dir="${SLAYERFS_ARTIFACT_DIR:-}"
perf_tools="${PERF_TOOLS:-dirstress metaperf looptest fio-seqread fio-seqwrite fio-randread fio-randwrite}"

env_or_default() {
    local specific_var="$1"
    local common_var="$2"
    local default_value="$3"
    local value="${!specific_var:-}"
    if [[ -n "$value" ]]; then
        printf '%s' "$value"
    else
        printf '%s' "${!common_var:-$default_value}"
    fi
}

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
                cat <<EOF
data:
  backend: local-fs
  localfs:
    data_dir: ${data_dir}
EOF
                ;;
            s3)
                bucket="${SLAYERFS_S3_BUCKET:-slayerfs-data}"
                region="${SLAYERFS_S3_REGION:-us-east-1}"
                endpoint="${SLAYERFS_S3_ENDPOINT:-http://rustfs:9000}"
                force_path="${SLAYERFS_S3_FORCE_PATH_STYLE:-true}"
                part_size="${SLAYERFS_S3_PART_SIZE:-16777216}"
                max_conc="${SLAYERFS_S3_MAX_CONCURRENCY:-8}"
                cat <<EOF
data:
  backend: s3
  s3:
    bucket: ${bucket}
    region: ${region}
    part_size: ${part_size}
    max_concurrency: ${max_conc}
    force_path_style: ${force_path}
    endpoint: ${endpoint}
EOF
                ;;
            *)
                err "不支持的 SLAYERFS_DATA_BACKEND: $data_backend"
                exit 1
                ;;
        esac
        echo

        case "$meta_backend" in
            sqlite)
                mkdir -p "$(dirname "$sqlite_path")"
                local url="${meta_url:-sqlite://${sqlite_path}?mode=rwc}"
                cat <<EOF
meta:
  backend: sqlx
  sqlx:
    url: "$url"
EOF
                ;;
            redis)
                if [[ -z "$meta_url" ]]; then
                    err "SLAYERFS_META_URL 不能为空 (redis)"
                    exit 1
                fi
                cat <<EOF
meta:
  backend: redis
  redis:
    url: "$meta_url"
EOF
                ;;
            etcd)
                cat <<EOF
meta:
  backend: etcd
  etcd:
    urls:
EOF
                local old_ifs="$IFS"
                IFS=','
                for url in $meta_etcd_urls; do
                    echo "      - \"${url}\""
                done
                IFS="$old_ifs"
                ;;
            *)
                err "不支持的 SLAYERFS_META_BACKEND: $meta_backend"
                exit 1
                ;;
        esac

        echo
        cat <<EOF
layout:
  chunk_size: ${SLAYERFS_CHUNK_SIZE:-67108864}
  block_size: ${SLAYERFS_BLOCK_SIZE:-4194304}
EOF
    } >"$config_path"
}

install_mount_helper() {
    local helper="/usr/sbin/mount.fuse.slayerfs"
    cat >"$helper" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:$PATH"

src="${1:-}"
target="${2:-}"
shift 2 || true

config_path="${SLAYERFS_CONFIG_PATH:-/run/slayerfs/config.yaml}"
log_file="${SLAYERFS_LOG_FILE:-/artifacts/slayerfs.log}"

mkdir -p "$target" "$(dirname "$log_file")"

/usr/local/bin/slayerfs mount --config "$config_path" "$target" >>"$log_file" 2>&1 &
sleep "${SLAYERFS_MOUNT_WAIT_SECS:-1}"
exit 0
EOF
    chmod +x "$helper"
}

prepare_artifacts() {
    mkdir -p "$artifact_dir/results" "$artifact_dir/tools"
    touch "$artifact_dir/perf.log" "$artifact_dir/perf-summary.tsv" "$artifact_dir/report.md" >/dev/null 2>&1 || true
    printf 'tool\tstatus\tseconds\tlog\n' >"$artifact_dir/perf-summary.tsv"
}

copy_artifacts() {
    mkdir -p "$artifact_dir"
    if [[ -f "$log_file" && "$log_file" != "$artifact_dir/slayerfs.log" ]]; then
        cp -f "$log_file" "$artifact_dir/slayerfs.log" || true
    fi
    if [[ -f "$config_path" ]]; then
        cp -f "$config_path" "$artifact_dir/backend.yml" || true
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
    cleanup || true
    trap - EXIT
    exit "$status"
}

require_tool_bin() {
    local bin="$1"
    if [[ ! -x "$bin" ]]; then
        err "找不到可执行工具: $bin"
        exit 1
    fi
}

mount_slayerfs() {
    mkdir -p "$mount_dir"
    if mountpoint -q "$mount_dir"; then
        cleanup
    fi

    info "挂载 SlayerFS: $mount_dir"
    mount -t fuse.slayerfs slayerfs "$mount_dir"

    local i=0
    for ((i = 0; i < 15; i++)); do
        if mountpoint -q "$mount_dir"; then
            ok "SlayerFS 已挂载"
            return 0
        fi
        sleep 1
    done

    err "SlayerFS 挂载失败: $mount_dir"
    exit 1
}

run_logged_tool() {
    local tool="$1"
    shift
    local log_path="$artifact_dir/tools/${tool}.log"
    local start end elapsed status

    start="$(date +%s)"
    info "运行压力工具: $tool"
    info "  命令: $*"
    set +e
    if [[ "${PERF_LOG_TO_CONSOLE:-false}" == "true" ]]; then
        "$@" 2>&1 | tee "$log_path"
        status="${PIPESTATUS[0]}"
    else
        "$@" >"$log_path" 2>&1
        status=$?
    fi
    set -e
    end="$(date +%s)"
    elapsed="$((end - start))"

    local log_size
    log_size=$(wc -c < "$log_path" 2>/dev/null || echo 0)

    if [[ "$status" -eq 0 ]]; then
        ok "压力工具完成: $tool (${elapsed}s, log=${log_size} bytes)"
        printf '%s\tpass\t%s\t%s\n' "$tool" "$elapsed" "$log_path" >>"$artifact_dir/perf-summary.tsv"
    else
        err "压力工具失败: $tool (exit=$status, ${elapsed}s, log=${log_size} bytes)"
        printf '%s\tfail(%s)\t%s\t%s\n' "$tool" "$status" "$elapsed" "$log_path" >>"$artifact_dir/perf-summary.tsv"
        # Show last 5 non-empty lines of the log to help diagnose failures
        if [[ -s "$log_path" ]]; then
            err "  最后几行日志:"
            grep -v '^$' "$log_path" | tail -5 | while read -r line; do
                err "    $line"
            done
        fi
    fi

    return "$status"
}

run_dirstress() {
    local bin="$xfstests_dir/src/dirstress"
    local work_dir="$mount_dir/.perf-dirstress"
    local -a args=()

    require_tool_bin "$bin"
    rm -rf "$work_dir"
    mkdir -p "$work_dir"

    if [[ -n "${PERF_DIRSTRESS_ARGS:-}" ]]; then
        read -r -a args <<<"${PERF_DIRSTRESS_ARGS}"
    else
        args=(
            -d "$work_dir"
            -p "${PERF_DIRSTRESS_PROCS:-4}"
            -f "${PERF_DIRSTRESS_FILES:-200}"
            -n "${PERF_DIRSTRESS_PROCS_PER_DIR:-2}"
            -s "${PERF_DIRSTRESS_SEED:-1}"
        )
    fi

    run_logged_tool dirstress "$bin" "${args[@]}"

    # Summarize dirstress errors (File exists errors are expected under concurrency)
    local dirstress_log="$artifact_dir/tools/dirstress.log"
    if [[ -f "$dirstress_log" ]]; then
        local total_errs mkdir_errs symlink_errs mknod_errs
        total_errs=$(grep -c '!!' "$dirstress_log" 2>/dev/null || echo 0)
        mkdir_errs=$(grep -c 'mkdir.*File exists' "$dirstress_log" 2>/dev/null || echo 0)
        symlink_errs=$(grep -c 'symlink.*File exists' "$dirstress_log" 2>/dev/null || echo 0)
        mknod_errs=$(grep -c "mknod.*Function not implemented" "$dirstress_log" 2>/dev/null || echo 0)
        info "dirstress 错误汇总: total=$total_errs mkdir_EEXIST=$mkdir_errs symlink_EEXIST=$symlink_errs mknod_ENOSYS=$mknod_errs"
    fi
}

run_dirperf() {
    local bin="$xfstests_dir/src/dirperf"
    local work_dir="$mount_dir/.perf-dirperf"
    local -a args=()

    require_tool_bin "$bin"
    rm -rf "$work_dir"
    mkdir -p "$work_dir"

    if [[ -n "${PERF_DIRPERF_ARGS:-}" ]]; then
        read -r -a args <<<"${PERF_DIRPERF_ARGS}"
    else
        args=(
            -d "$work_dir"
            -a "${PERF_DIRPERF_ADDSTEP:-100}"
            -f "${PERF_DIRPERF_FIRST:-100}"
            -l "${PERF_DIRPERF_LAST:-1000}"
            -c "${PERF_DIRPERF_NAME_LEN:-16}"
            -n "${PERF_DIRPERF_DIRS:-2}"
            -s "${PERF_DIRPERF_STATS:-5}"
        )
    fi

    run_logged_tool dirperf "$bin" "${args[@]}"
}

run_metaperf() {
    local bin="$xfstests_dir/src/metaperf"
    local work_dir="$mount_dir/.perf-metaperf"
    local -a args=()

    require_tool_bin "$bin"
    rm -rf "$work_dir"
    mkdir -p "$work_dir"

    if [[ -n "${PERF_METAPERF_ARGS:-}" ]]; then
        read -r -a args <<<"${PERF_METAPERF_ARGS}"
    else
        args=(
            -d "$work_dir"
            -t "${PERF_METAPERF_SECONDS:-30}"
            -s "${PERF_METAPERF_FILE_SIZE:-4096}"
            -l "${PERF_METAPERF_NAME_LEN:-16}"
            -L "${PERF_METAPERF_BG_NAME_LEN:-16}"
            -n "${PERF_METAPERF_OP_FILES:-200}"
            -N "${PERF_METAPERF_BG_FILES:-2000}"
            create
            open
            stat
            readdir
            rename
        )
    fi

    run_logged_tool metaperf "$bin" "${args[@]}"
}

run_looptest() {
    local bin="$xfstests_dir/src/looptest"
    local work_dir="$mount_dir/.perf-looptest"
    local loop_file="$work_dir/looptest.dat"
    local -a args=()

    require_tool_bin "$bin"
    rm -rf "$work_dir"
    mkdir -p "$work_dir"

    if [[ -n "${PERF_LOOPTEST_ARGS:-}" ]]; then
        read -r -a args <<<"${PERF_LOOPTEST_ARGS}"
    else
        args=(
            -i "${PERF_LOOPTEST_ITERS:-200}"
            -o
            -r
            -w
            -t
            -f
            -s
            -v
            -b "${PERF_LOOPTEST_BUF_SIZE:-1048576}"
            "$loop_file"
        )
    fi

    run_logged_tool looptest "$bin" "${args[@]}"

    # Post-validation: verify the test file was created and modified
    if [[ -f "$loop_file" ]]; then
        local looptest_size
        looptest_size=$(stat -c%s "$loop_file" 2>/dev/null || echo 0)
        info "looptest 测试文件: $loop_file (size=$looptest_size)"
    else
        err "looptest 未能创建测试文件: $loop_file"
    fi
}

append_fio_log_summary() {
    local json_path="$1"
    local log_path="$2"
    local label="${3:-fio}"

    if [[ -f "$json_path" ]] && command -v python3 >/dev/null 2>&1; then
        python3 -c "
import json, sys
with open('$json_path') as f:
    data = json.load(f)
jobs = data.get('jobs', [])
if not jobs:
    sys.exit(1)
read = jobs[0].get('read', {})
write = jobs[0].get('write', {})
opts = jobs[0].get('job options', {})
print(f\"${label}: {opts.get('rw','?')} bs={opts.get('bs','?')} numjobs={opts.get('numjobs','?')} runtime={opts.get('runtime','?')}s\")
print(f\"  read:  bw={read.get('bw','?')} KiB/s  iops={read.get('iops','?'):.1f}  lat_avg={read.get('clat_ns',{}).get('mean',0)/1e6:.2f}ms  lat_p99={read.get('clat_ns',{}).get('percentile',{}).get('99.000000',0)/1e6:.2f}ms\")
print(f\"  write: bw={write.get('bw','?')} KiB/s  iops={write.get('iops','?'):.1f}  lat_avg={write.get('clat_ns',{}).get('mean',0)/1e6:.2f}ms  lat_p99={write.get('clat_ns',{}).get('percentile',{}).get('99.000000',0)/1e6:.2f}ms\")
print(f\"  total: {read.get('io_bytes',0)+write.get('io_bytes',0)} bytes, {read.get('total_ios',0)+write.get('total_ios',0)} IOs\")
" >> "$log_path" 2>/dev/null || true
    fi
}

prepare_fio_dataset() {
    local tool="$1"
    local work_dir="$2"
    local dataset_size="$3"
    local direct_mode="$4"
    local prep_log="$artifact_dir/tools/${tool}-prepare.log"
    local -a prep_args=(
        --name="${tool}-prepare"
        --directory="$work_dir"
        --rw=write
        --bs="${PERF_FIO_PREP_BS:-4m}"
        --size="$dataset_size"
        --numjobs=1
        --ioengine="${PERF_FIO_PREP_IOENGINE:-sync}"
        --iodepth="${PERF_FIO_PREP_IODEPTH:-1}"
        --direct="$direct_mode"
        --end_fsync=1
        --group_reporting
        --eta=never
    )

    info "预填充 fio 数据集: $tool"
    if [[ "${PERF_LOG_TO_CONSOLE:-false}" == "true" ]]; then
        fio "${prep_args[@]}" 2>&1 | tee "$prep_log"
        return "${PIPESTATUS[0]}"
    fi
    fio "${prep_args[@]}" >"$prep_log" 2>&1
}

run_fio_custom() {
    local work_dir="$mount_dir/.perf-fio"
    local json_path="$artifact_dir/results/fio.json"
    local -a args=()

    if ! command -v fio >/dev/null 2>&1; then
        err "找不到 fio"
        exit 1
    fi

    rm -rf "$work_dir"
    mkdir -p "$work_dir"

    if [[ -n "${PERF_FIO_ARGS:-}" ]]; then
        read -r -a args <<<"${PERF_FIO_ARGS}"
    else
        args=(
            --name="${PERF_FIO_NAME:-slayerfs-randrw}"
            --directory="$work_dir"
            --rw="${PERF_FIO_RW:-randrw}"
            --rwmixread="${PERF_FIO_RWMIXREAD:-70}"
            --bs="${PERF_FIO_BS:-4m}"
            --size="${PERF_FIO_SIZE:-256m}"
            --numjobs="${PERF_FIO_NUMJOBS:-4}"
            --ioengine="${PERF_FIO_IOENGINE:-sync}"
            --iodepth="${PERF_FIO_IODEPTH:-1}"
            --direct="${PERF_FIO_DIRECT:-0}"
            --runtime="${PERF_FIO_RUNTIME:-60}"
            --time_based
            --group_reporting
            --eta=never
        )
    fi

    args+=(--output-format=json --output="$json_path")
    run_logged_tool fio fio "${args[@]}"
    append_fio_log_summary "$json_path" "$artifact_dir/tools/fio.log" "fio"
}

run_fio_profile() {
    local tool="$1"
    local mode="$2"
    local work_dir="$mount_dir/.perf-${tool}"
    local json_path="$artifact_dir/results/${tool}.json"
    local profile_suffix="${tool#fio-}"
    local profile_key
    local profile_args_var
    local name_var
    local rw_var
    local rwmixread_var
    local bs_var
    local size_var
    local numjobs_var
    local ioengine_var
    local iodepth_var
    local direct_var
    local runtime_var
    local name rw rwmixread bs size numjobs ioengine iodepth direct runtime
    local needs_prefill=false
    local -a args=()

    profile_key="$(printf '%s' "$profile_suffix" | tr '[:lower:]-' '[:upper:]_')"
    profile_args_var="PERF_FIO_${profile_key}_ARGS"
    name_var="PERF_FIO_${profile_key}_NAME"
    rw_var="PERF_FIO_${profile_key}_RW"
    rwmixread_var="PERF_FIO_${profile_key}_RWMIXREAD"
    bs_var="PERF_FIO_${profile_key}_BS"
    size_var="PERF_FIO_${profile_key}_SIZE"
    numjobs_var="PERF_FIO_${profile_key}_NUMJOBS"
    ioengine_var="PERF_FIO_${profile_key}_IOENGINE"
    iodepth_var="PERF_FIO_${profile_key}_IODEPTH"
    direct_var="PERF_FIO_${profile_key}_DIRECT"
    runtime_var="PERF_FIO_${profile_key}_RUNTIME"

    rm -rf "$work_dir"
    mkdir -p "$work_dir"

    if [[ -n "${!profile_args_var:-}" ]]; then
        read -r -a args <<<"${!profile_args_var}"
    else
        case "$mode" in
            seqread)
                name="$(env_or_default "$name_var" PERF_FIO_NAME slayerfs-seqread)"
                rw="$(env_or_default "$rw_var" PERF_FIO_RW read)"
                bs="$(env_or_default "$bs_var" PERF_FIO_BS 4m)"
                size="$(env_or_default "$size_var" PERF_FIO_SIZE 1g)"
                numjobs="$(env_or_default "$numjobs_var" PERF_FIO_NUMJOBS 1)"
                ioengine="$(env_or_default "$ioengine_var" PERF_FIO_IOENGINE sync)"
                iodepth="$(env_or_default "$iodepth_var" PERF_FIO_IODEPTH 1)"
                direct="$(env_or_default "$direct_var" PERF_FIO_DIRECT 0)"
                runtime="$(env_or_default "$runtime_var" PERF_FIO_RUNTIME 60)"
                needs_prefill=true
                ;;
            seqwrite)
                name="$(env_or_default "$name_var" PERF_FIO_NAME slayerfs-seqwrite)"
                rw="$(env_or_default "$rw_var" PERF_FIO_RW write)"
                bs="$(env_or_default "$bs_var" PERF_FIO_BS 4m)"
                size="$(env_or_default "$size_var" PERF_FIO_SIZE 1g)"
                numjobs="$(env_or_default "$numjobs_var" PERF_FIO_NUMJOBS 1)"
                ioengine="$(env_or_default "$ioengine_var" PERF_FIO_IOENGINE sync)"
                iodepth="$(env_or_default "$iodepth_var" PERF_FIO_IODEPTH 1)"
                direct="$(env_or_default "$direct_var" PERF_FIO_DIRECT 0)"
                runtime="$(env_or_default "$runtime_var" PERF_FIO_RUNTIME 60)"
                ;;
            randread)
                name="$(env_or_default "$name_var" PERF_FIO_NAME slayerfs-randread)"
                rw="$(env_or_default "$rw_var" PERF_FIO_RW randread)"
                bs="$(env_or_default "$bs_var" PERF_FIO_BS 4m)"
                size="$(env_or_default "$size_var" PERF_FIO_SIZE 512m)"
                numjobs="$(env_or_default "$numjobs_var" PERF_FIO_NUMJOBS 4)"
                ioengine="$(env_or_default "$ioengine_var" PERF_FIO_IOENGINE sync)"
                iodepth="$(env_or_default "$iodepth_var" PERF_FIO_IODEPTH 1)"
                direct="$(env_or_default "$direct_var" PERF_FIO_DIRECT 0)"
                runtime="$(env_or_default "$runtime_var" PERF_FIO_RUNTIME 60)"
                needs_prefill=true
                ;;
            randwrite)
                name="$(env_or_default "$name_var" PERF_FIO_NAME slayerfs-randwrite)"
                rw="$(env_or_default "$rw_var" PERF_FIO_RW randwrite)"
                bs="$(env_or_default "$bs_var" PERF_FIO_BS 4m)"
                size="$(env_or_default "$size_var" PERF_FIO_SIZE 512m)"
                numjobs="$(env_or_default "$numjobs_var" PERF_FIO_NUMJOBS 4)"
                ioengine="$(env_or_default "$ioengine_var" PERF_FIO_IOENGINE sync)"
                iodepth="$(env_or_default "$iodepth_var" PERF_FIO_IODEPTH 1)"
                direct="$(env_or_default "$direct_var" PERF_FIO_DIRECT 0)"
                runtime="$(env_or_default "$runtime_var" PERF_FIO_RUNTIME 60)"
                ;;
            randrw)
                name="$(env_or_default "$name_var" PERF_FIO_NAME slayerfs-randrw)"
                rw="$(env_or_default "$rw_var" PERF_FIO_RW randrw)"
                rwmixread="$(env_or_default "$rwmixread_var" PERF_FIO_RWMIXREAD 70)"
                bs="$(env_or_default "$bs_var" PERF_FIO_BS 4m)"
                size="$(env_or_default "$size_var" PERF_FIO_SIZE 512m)"
                numjobs="$(env_or_default "$numjobs_var" PERF_FIO_NUMJOBS 4)"
                ioengine="$(env_or_default "$ioengine_var" PERF_FIO_IOENGINE sync)"
                iodepth="$(env_or_default "$iodepth_var" PERF_FIO_IODEPTH 1)"
                direct="$(env_or_default "$direct_var" PERF_FIO_DIRECT 0)"
                runtime="$(env_or_default "$runtime_var" PERF_FIO_RUNTIME 60)"
                needs_prefill=true
                ;;
            *)
                err "未知的 fio profile: $mode"
                return 1
                ;;
        esac

        args=(
            --name="$name"
            --directory="$work_dir"
            --rw="$rw"
            --bs="$bs"
            --size="$size"
            --numjobs="$numjobs"
            --ioengine="$ioengine"
            --iodepth="$iodepth"
            --direct="$direct"
            --runtime="$runtime"
            --time_based
            --group_reporting
            --eta=never
        )

        if [[ -n "${rwmixread:-}" ]]; then
            args+=(--rwmixread="$rwmixread")
        fi
    fi

    if [[ "$needs_prefill" == true ]]; then
        prepare_fio_dataset "$tool" "$work_dir" "$size" "$direct" || return $?
    fi

    args+=(--output-format=json --output="$json_path")
    run_logged_tool "$tool" fio "${args[@]}"
    append_fio_log_summary "$json_path" "$artifact_dir/tools/${tool}.log" "$tool"
}

generate_perf_report() {
    python3 - "$artifact_dir" "$meta_backend" <<'PY'
import csv
import datetime as dt
import json
import pathlib
import sys

artifact_dir = pathlib.Path(sys.argv[1])
meta_backend = sys.argv[2]
summary_path = artifact_dir / "perf-summary.tsv"
report_path = artifact_dir / "report.md"
fio_json_paths = sorted((artifact_dir / "results").glob("fio*.json"))

rows = []
if summary_path.exists():
    with summary_path.open(newline="") as f:
        rows = list(csv.DictReader(f, delimiter="\t"))

lines = [
    "# SlayerFS Perf Report",
    "",
    f"Meta backend: {meta_backend}",
    "",
    "## Summary",
    "",
    "| Tool | Status | Seconds | Log |",
    "| --- | --- | ---: | --- |",
]

for row in rows:
    log = pathlib.Path(row.get("log", "")).name
    lines.append(
        f"| {row.get('tool', '')} | {row.get('status', '')} | "
        f"{row.get('seconds', '')} | tools/{log} |"
    )

if fio_json_paths:
    try:
        def num(value, default=0):
            try:
                return float(value)
            except (TypeError, ValueError):
                return default

        def fmt_bytes(value):
            value = num(value)
            units = ["B", "KiB", "MiB", "GiB", "TiB"]
            for unit in units:
                if abs(value) < 1024 or unit == units[-1]:
                    return f"{value:.2f} {unit}"
                value /= 1024
            return f"{value:.2f} TiB"

        def fmt_rate(value):
            return f"{fmt_bytes(value)}/s"

        def fmt_iops(value):
            return f"{num(value):,.2f}"

        def fmt_ms_from_ns(value):
            return f"{num(value) / 1_000_000:.3f} ms"

        def latency_percentile(op, pct):
            percentiles = op.get("clat_ns", {}).get("percentile", {})
            return percentiles.get(f"{pct:.6f}") or percentiles.get(str(pct))

        def op_totals(op_name):
            ops = [job.get(op_name, {}) for job in jobs]
            io_bytes = sum(num(op.get("io_bytes")) for op in ops)
            bw_bytes = sum(num(op.get("bw_bytes")) for op in ops)
            iops = sum(num(op.get("iops")) for op in ops)
            total_ios = sum(num(op.get("total_ios")) for op in ops)
            runtimes = [num(op.get("runtime")) for op in ops if num(op.get("runtime")) > 0]
            runtime_ms = max(runtimes) if runtimes else 0
            means = [
                (num(op.get("clat_ns", {}).get("mean")), num(op.get("clat_ns", {}).get("N")))
                for op in ops
                if num(op.get("clat_ns", {}).get("N")) > 0
            ]
            total_n = sum(n for _, n in means)
            mean_ns = sum(mean * n for mean, n in means) / total_n if total_n else 0
            p95 = max((num(latency_percentile(op, 95)) for op in ops), default=0)
            p99 = max((num(latency_percentile(op, 99)) for op in ops), default=0)
            return {
                "io_bytes": io_bytes,
                "bw_bytes": bw_bytes,
                "iops": iops,
                "total_ios": total_ios,
                "runtime_ms": runtime_ms,
                "mean_ns": mean_ns,
                "p95_ns": p95,
                "p99_ns": p99,
            }

        def first_job_options():
            for job in jobs:
                options = job.get("job options", {})
                if options:
                    return options
            return {}

        lines.extend([
            "",
            "## Fio",
            "",
            "| Tool | Workload | BS | Jobs | Read BW | Read IOPS | Write BW | Write IOPS | Read P99 | Write P99 | Raw |",
            "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
        ])

        for fio_json_path in fio_json_paths:
            data = json.loads(fio_json_path.read_text())
            jobs = data.get("jobs", [])
            if not jobs:
                continue
            options = first_job_options()
            read = op_totals("read")
            write = op_totals("write")
            tool_name = fio_json_path.stem
            lines.append(
                f"| {tool_name} | {options.get('rw', 'unknown')} | {options.get('bs', 'unknown')} | "
                f"{options.get('numjobs', 'unknown')} | {fmt_rate(read['bw_bytes'])} | "
                f"{fmt_iops(read['iops'])} | {fmt_rate(write['bw_bytes'])} | "
                f"{fmt_iops(write['iops'])} | {fmt_ms_from_ns(read['p99_ns'])} | "
                f"{fmt_ms_from_ns(write['p99_ns'])} | results/{fio_json_path.name} |"
            )
    except Exception as exc:
        lines.extend(["", "## Fio", "", f"Failed to parse fio JSON: {exc}"])

report_path.write_text("\n".join(lines) + "\n")
PY
}

run_perf_suite() {
    local -a tools=()
    local status=0
    local tool=""

    read -r -a tools <<<"$perf_tools"
    if [[ "${#tools[@]}" -eq 0 ]]; then
        err "PERF_TOOLS 不能为空"
        exit 1
    fi

    for tool in "${tools[@]}"; do
        case "$tool" in
            dirstress)
                run_dirstress || status=1
                ;;
            dirperf)
                run_dirperf || status=1
                ;;
            metaperf)
                run_metaperf || status=1
                ;;
            looptest)
                run_looptest || status=1
                ;;
            fio)
                run_fio_custom || status=1
                ;;
            fio-seqread)
                run_fio_profile "$tool" seqread || status=1
                ;;
            fio-seqwrite)
                run_fio_profile "$tool" seqwrite || status=1
                ;;
            fio-randread)
                run_fio_profile "$tool" randread || status=1
                ;;
            fio-randwrite)
                run_fio_profile "$tool" randwrite || status=1
                ;;
            fio-randrw)
                run_fio_profile "$tool" randrw || status=1
                ;;
            *)
                err "不支持的 PERF_TOOLS 项: $tool"
                status=1
                ;;
        esac
    done

    return "$status"
}

main() {
    if [[ -z "$artifact_dir" ]]; then
        local ts
        ts="$(date +%s)-$RANDOM"
        artifact_dir="${artifact_root%/}/perf-run-${ts}"
    fi

    mkdir -p "$artifact_dir"
    chmod a+rwx "$artifact_dir" >/dev/null 2>&1 || true
    log_file="$artifact_dir/slayerfs.log"
    export SLAYERFS_LOG_FILE="$log_file"

    trap on_exit EXIT INT TERM

    info "写入 SlayerFS 配置: $config_path"
    write_config

    info "安装 mount helper: /usr/sbin/mount.fuse.slayerfs"
    install_mount_helper

    info "准备产物目录: $artifact_dir"
    prepare_artifacts

    mount_slayerfs

    # Pre-flight sanity check: verify the filesystem can create, write, and read files.
    info "执行挂载点预检: $mount_dir"
    local preflight_dir="$mount_dir/.perf-preflight"
    local preflight_file="$preflight_dir/test.bin"
    rm -rf "$preflight_dir"
    mkdir -p "$preflight_dir"
    if ! echo "slayerfs-preflight-$(date +%s)" > "$preflight_file"; then
        err "预检失败: 无法写入 $preflight_file"
        exit 1
    fi
    local preflight_read
    preflight_read=$(cat "$preflight_file" 2>/dev/null)
    if [[ -z "$preflight_read" ]]; then
        err "预检失败: 无法读取 $preflight_file"
        exit 1
    fi
    rm -rf "$preflight_dir"
    ok "预检通过: 写入/读取正常"

    info "开始性能测试: tools=$perf_tools"
    set +e
    run_perf_suite
    status=$?
    set -e

    # Post-test filesystem statistics
    info "测试完成后文件系统统计:"
    if command -v df >/dev/null 2>&1; then
        df -h "$mount_dir" 2>/dev/null | tail -1 | while read -r fs size used avail pct mnt; do
            info "  磁盘使用: $used / $size ($pct)"
        done
    fi
    if [[ -d "$mount_dir" ]]; then
        local total_files
        total_files=$(find "$mount_dir" -type f 2>/dev/null | wc -l)
        info "  残留文件数: $total_files"
    fi

    generate_perf_report || true

    if [[ "$status" -eq 0 ]]; then
        ok "性能测试全部完成"
    else
        err "性能测试存在失败项 (exit=$status)"
    fi

    return "$status"
}

main "$@"
