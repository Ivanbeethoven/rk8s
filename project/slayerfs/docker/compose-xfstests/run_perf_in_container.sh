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
perf_tools="${PERF_TOOLS:-dirstress metaperf looptest fio}"

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

    if [[ "$status" -eq 0 ]]; then
        ok "压力工具完成: $tool (${elapsed}s)"
        printf '%s\tpass\t%s\t%s\n' "$tool" "$elapsed" "$log_path" >>"$artifact_dir/perf-summary.tsv"
    else
        err "压力工具失败: $tool (exit=$status, ${elapsed}s)"
        printf '%s\tfail(%s)\t%s\t%s\n' "$tool" "$status" "$elapsed" "$log_path" >>"$artifact_dir/perf-summary.tsv"
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
            -b "${PERF_LOOPTEST_BUF_SIZE:-1048576}"
            "$loop_file"
        )
    fi

    run_logged_tool looptest "$bin" "${args[@]}"
}

run_fio() {
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
            --bs="${PERF_FIO_BS:-4k}"
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
fio_json_path = artifact_dir / "results" / "fio.json"

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

if fio_json_path.exists():
    try:
        data = json.loads(fio_json_path.read_text())
        jobs = data.get("jobs", [])

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

        fio_version = data.get("fio version", "unknown")
        timestamp = data.get("timestamp")
        started_at = ""
        if timestamp:
            started_at = dt.datetime.fromtimestamp(int(timestamp), tz=dt.timezone.utc).isoformat()

        options = first_job_options()
        read = op_totals("read")
        write = op_totals("write")
        total_bw = read["bw_bytes"] + write["bw_bytes"]
        total_iops = read["iops"] + write["iops"]
        total_io = read["io_bytes"] + write["io_bytes"]
        max_runtime_ms = max(read["runtime_ms"], write["runtime_ms"])

        lines.extend([
            "",
            "## Fio",
            "",
            "### Test Setup",
            "",
            "| Field | Value |",
            "| --- | ---: |",
            f"| fio version | {fio_version} |",
            f"| Started at | {started_at or 'unknown'} |",
            f"| Jobs reported | {len(jobs)} |",
            f"| Job name | {options.get('name', jobs[0].get('jobname', 'unknown') if jobs else 'unknown')} |",
            f"| Workload | {options.get('rw', 'unknown')} |",
            f"| Block size | {options.get('bs', 'unknown')} |",
            f"| Size | {options.get('size', 'unknown')} |",
            f"| Runtime | {options.get('runtime', max_runtime_ms / 1000 if max_runtime_ms else 'unknown')} s |",
            f"| Num jobs | {options.get('numjobs', 'unknown')} |",
            f"| IO engine | {options.get('ioengine', 'unknown')} |",
            f"| IO depth | {options.get('iodepth', 'unknown')} |",
            f"| Direct IO | {options.get('direct', 'unknown')} |",
            "",
            "### Overall Result",
            "",
            "| Metric | Value |",
            "| --- | ---: |",
            f"| Total bandwidth | {fmt_rate(total_bw)} |",
            f"| Total IOPS | {fmt_iops(total_iops)} |",
            f"| Total data transferred | {fmt_bytes(total_io)} |",
            f"| Effective runtime | {max_runtime_ms / 1000:.2f} s |",
            "",
            "### Read / Write Detail",
            "",
            "| Operation | Bandwidth | IOPS | Data | IOs | Avg clat | P95 clat | P99 clat |",
            "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
            (
                f"| Read | {fmt_rate(read['bw_bytes'])} | {fmt_iops(read['iops'])} | "
                f"{fmt_bytes(read['io_bytes'])} | {read['total_ios']:,.0f} | "
                f"{fmt_ms_from_ns(read['mean_ns'])} | {fmt_ms_from_ns(read['p95_ns'])} | "
                f"{fmt_ms_from_ns(read['p99_ns'])} |"
            ),
            (
                f"| Write | {fmt_rate(write['bw_bytes'])} | {fmt_iops(write['iops'])} | "
                f"{fmt_bytes(write['io_bytes'])} | {write['total_ios']:,.0f} | "
                f"{fmt_ms_from_ns(write['mean_ns'])} | {fmt_ms_from_ns(write['p95_ns'])} | "
                f"{fmt_ms_from_ns(write['p99_ns'])} |"
            ),
            "",
            f"Raw fio JSON: results/{fio_json_path.name}",
        ])
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
                run_fio || status=1
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

    info "开始性能测试: tools=$perf_tools"
    set +e
    run_perf_suite
    status=$?
    set -e
    generate_perf_report || true

    if [[ "$status" -eq 0 ]]; then
        ok "性能测试全部完成"
    else
        err "性能测试存在失败项 (exit=$status)"
    fi

    return "$status"
}

main "$@"
