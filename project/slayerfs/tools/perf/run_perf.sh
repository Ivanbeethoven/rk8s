#!/usr/bin/env bash
# SlayerFS performance profiling + flame graph generation
#
# Prerequisites: docker, cargo, fio, linux-perf, inferno, python3
#
# Usage:
#   ./run_perf.sh              # Full run: build, benchmark, flame graph
#   ./run_perf.sh --no-build   # Skip rebuild
#   ./run_perf.sh --quick      # Shorter benchmarks for quick iteration
#   ./run_perf.sh --no-cleanup # Leave containers and mount point after run

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
PERF_DIR="${PERF_DIR:-/tmp/slayerfs-perf}"
CONFIG_PATH="$PERF_DIR/config.yaml"
FLAME_DIR="$PERF_DIR/flame"
MNT_DIR="$PERF_DIR/mnt"
DATA_DIR="$PERF_DIR/data"
RESULTS_DIR="$PERF_DIR/results"

REDIS_PORT="${REDIS_PORT:-16379}"
RUSTFS_S3_PORT="${RUSTFS_S3_PORT:-19000}"
S3_BUCKET="${S3_BUCKET:-slayerfs-perf-data}"
RUSTFS_ACCESS_KEY="${RUSTFS_ACCESS_KEY:-rustfsadmin}"
RUSTFS_SECRET_KEY="${RUSTFS_SECRET_KEY:-rustfsadmin}"

BUILD=1
QUICK=0
CLEANUP=1
SKIP_ONCPU=0
SKIP_OFFCPU=0

for arg in "$@"; do
    case "$arg" in
        --no-build) BUILD=0 ;;
        --quick) QUICK=1 ;;
        --no-cleanup) CLEANUP=0 ;;
        --skip-oncpu) SKIP_ONCPU=1 ;;
        --skip-offcpu) SKIP_OFFCPU=1 ;;
        *) echo "Unknown: $arg"; exit 1 ;;
    esac
done

RUNTIME=$([ "$QUICK" -eq 1 ] && echo 15 || echo 60)
SEQ_SIZE=$([ "$QUICK" -eq 1 ] && echo 256m || echo 1g)
RAND_SIZE=$([ "$QUICK" -eq 1 ] && echo 128m || echo 512m)

BOLD='\033[1m'
GREEN='\033[32m'
YELLOW='\033[33m'
RED='\033[31m'
NC='\033[0m'

info()  { echo -e "${GREEN}[perf]${NC} ${BOLD}$*${NC}"; }
warn()  { echo -e "${YELLOW}[perf]${NC} $*"; }
err()   { echo -e "${RED}[perf]${NC} $*"; }

cleanup() {
    if [ "$CLEANUP" -eq 1 ]; then
        info "cleaning up..."
        fusermount3 -u "$MNT_DIR" 2>/dev/null || true
        pkill -f "slayerfs mount" 2>/dev/null || true
        sleep 1
        docker compose -f "$SCRIPT_DIR/docker-compose.yml" down -v 2>/dev/null || true
        rm -rf "$PERF_DIR"
    fi
}
trap cleanup EXIT

check_cmd() {
    command -v "$1" >/dev/null 2>&1 || { err "$1 is required but not found"; exit 1; }
}
check_cmd docker
check_cmd cargo
check_cmd fio
check_cmd perf
check_cmd inferno-flamegraph
check_cmd python3

# ---- Build ----
if [ "$BUILD" -eq 1 ]; then
    info "building slayerfs with profiling + frame pointers..."
    cd "$PROJECT_DIR"
    # Use DWARF4 to avoid addr2line compatibility issues with large binaries.
    # Split debuginfo keeps the binary smaller and perf can still find symbols.
    RUSTFLAGS="-C force-frame-pointers=yes -C debuginfo=2 -C split-debuginfo=off" \
        CARGO_PROFILE_RELEASE_DEBUG=2 \
        cargo build --release -p slayerfs --features profiling 2>&1 | grep -E "error|warning|Finished" || true
    BINARY="$PROJECT_DIR/../target/release/slayerfs"
else
    BINARY="$PROJECT_DIR/../target/release/slayerfs"
    [ -x "$BINARY" ] || { err "binary not found: $BINARY"; exit 1; }
fi

# Verify the binary exists and has debug info
if [ -x "$BINARY" ]; then
    if ! file "$BINARY" | grep -q "with debug_info"; then
        warn "binary lacks debug info — flame graphs may have unresolved symbols"
    fi
fi

# ---- Setup ----
info "setting up environment..."
rm -rf "$PERF_DIR"
mkdir -p "$PERF_DIR" "$FLAME_DIR" "$MNT_DIR" "$DATA_DIR" "$RESULTS_DIR"

# ---- Start infrastructure ----
info "starting redis + rustfs..."
cd "$SCRIPT_DIR"
docker compose down -v 2>/dev/null || true
REDIS_PORT="$REDIS_PORT" \
    RUSTFS_S3_PORT="$RUSTFS_S3_PORT" \
    RUSTFS_ACCESS_KEY="$RUSTFS_ACCESS_KEY" \
    RUSTFS_SECRET_KEY="$RUSTFS_SECRET_KEY" \
    S3_BUCKET="$S3_BUCKET" \
    docker compose up -d --wait 2>&1 | tail -5

docker compose ps --format 'table {{.Service}}\t{{.Status}}' 2>/dev/null || true
redis-cli -p "$REDIS_PORT" ping >/dev/null 2>&1 || { err "redis not reachable"; exit 1; }
info "redis: OK, rustfs: OK"

# ---- Generate config ----
cat > "$CONFIG_PATH" << YEOF
mount_point: $MNT_DIR
data:
  backend: s3
  s3:
    bucket: $S3_BUCKET
    endpoint: http://127.0.0.1:$RUSTFS_S3_PORT
    region: us-east-1
    force_path_style: true
    disable_payload_checksum: true
    part_size: 16777216
    max_concurrency: 16
meta:
  backend: redis
  redis:
    url: "redis://127.0.0.1:$REDIS_PORT/0"
layout:
  chunk_size: 268435456
  block_size: 4194304
fuse:
  workers: 8
  max_background: 512
YEOF

# ---- Mount ----
info "mounting slayerfs..."
fusermount3 -u "$MNT_DIR" 2>/dev/null || true
sleep 1

AWS_ACCESS_KEY_ID="$RUSTFS_ACCESS_KEY" \
    AWS_SECRET_ACCESS_KEY="$RUSTFS_SECRET_KEY" \
    AWS_DEFAULT_REGION=us-east-1 \
    AWS_EC2_METADATA_DISABLED=true \
    RUST_LOG=error \
    "$BINARY" mount --privileged --config "$CONFIG_PATH" 2>/dev/null &

for i in $(seq 1 15); do
    mount | grep -q " on $MNT_DIR " && break
    sleep 1
done
mount | grep -q " on $MNT_DIR " || { err "mount failed"; exit 1; }
info "mounted at $MNT_DIR"

# ---- Warmup ----
info "warming up filesystem..."
fio --name=warmup --directory="$MNT_DIR" --rw=write --bs=4m --size=256m \
    --numjobs=1 --ioengine=sync --direct=0 --runtime=10 --time_based \
    --group_reporting --eta=never --output-format=terse 2>/dev/null || true
rm -rf "$MNT_DIR"/* 2>/dev/null || true

# ---- Helper: run fio and print summary ----
run_fio() {
    local label="$1"; shift
    info "  fio $label..."
    local tmp_json="$RESULTS_DIR/fio-${label}.json.tmp"
    fio "$@" --directory="$MNT_DIR" --runtime="$RUNTIME" --time_based \
        --group_reporting --eta=never --output-format=json 2>/dev/null \
        | tee "$tmp_json" \
        | python3 -c "
import json,sys
d=json.load(sys.stdin)
for j in d.get('jobs',[]):
    for op in ('read','write'):
        bw=j.get(op,{}).get('bw_bytes',0)
        if bw>0:
            iops=j[op]['iops']
            lat=j[op]['lat_ns']['mean']/1e6
            print(f'    {op}: {bw/1024/1024:.0f} MiB/s, iops={iops:.1f}, lat_avg={lat:.2f}ms')
" 2>/dev/null || true
    mv "$tmp_json" "$RESULTS_DIR/fio-${label}.json" 2>/dev/null || true
}

# ---- Helper: run perf script safely ----
# Handles addr2line issues with large debug binaries by using --no-inline
# and pointing at the correct symbol file.
run_perf_script() {
    local perf_data="$1"
    local output="$2"

    # Try with --no-inline first (avoids addr2line "could not read first record" on large binaries)
    if perf script --no-inline -i "$perf_data" --symfs "$(dirname "$BINARY")" > "$output" 2>/dev/null; then
        return 0
    fi

    # Fallback: plain perf script without --no-inline
    if perf script -i "$perf_data" > "$output" 2>/dev/null; then
        return 0
    fi

    # Last resort: only emit basic fields (no addr2line at all)
    warn "perf script failed to resolve symbols; using basic output"
    perf script -i "$perf_data" -F comm,pid,tid,cpu,time,event,ip,sym,dso 2>/dev/null > "$output" || true
}

# =========================================================================
# ON-CPU FLAME GRAPH
# =========================================================================
if [ "$SKIP_ONCPU" -eq 0 ]; then
    info "=== ON-CPU profiling (perf record -F 99 --call-graph fp) ==="

    # Use frame-pointer based call graphs (fp) — faster and more reliable
    # than dwarf for large binaries. Requires -C force-frame-pointers=yes at build.
    perf record -F 99 --call-graph fp -a -o "$FLAME_DIR/oncpu-perf.data" &
    PERF_ONCPU_PID=$!
    sleep 1

    run_fio "seqwrite"  --name=seqwrite  --rw=write    --bs=4m --size="$SEQ_SIZE" --numjobs=1 --ioengine=sync --direct=0
    run_fio "seqread"   --name=seqread   --rw=read     --bs=4m --size="$SEQ_SIZE" --numjobs=1 --ioengine=sync --direct=0
    run_fio "randwrite" --name=randwrite --rw=randwrite --bs=4m --size="$RAND_SIZE" --numjobs=4 --ioengine=sync --direct=0
    run_fio "randread"  --name=randread  --rw=randread  --bs=4m --size="$RAND_SIZE" --numjobs=4 --ioengine=sync --direct=0
    run_fio "randrw"    --name=randrw    --rw=randrw --rwmixread=70 --bs=4m --size="$RAND_SIZE" --numjobs=4 --ioengine=sync --direct=0

    kill -INT "$PERF_ONCPU_PID" 2>/dev/null || true
    wait "$PERF_ONCPU_PID" 2>/dev/null || true

    if [ -f "$FLAME_DIR/oncpu-perf.data" ]; then
        info "generating on-CPU flame graph..."
        run_perf_script "$FLAME_DIR/oncpu-perf.data" "$FLAME_DIR/oncpu-raw.txt"

        inferno-collapse-perf < "$FLAME_DIR/oncpu-raw.txt" \
            > "$FLAME_DIR/oncpu.folded" 2>/dev/null || true

        grep "slayerfs" "$FLAME_DIR/oncpu.folded" > "$FLAME_DIR/oncpu-slayerfs.folded" 2>/dev/null || true

        if [ -s "$FLAME_DIR/oncpu-slayerfs.folded" ]; then
            inferno-flamegraph "$FLAME_DIR/oncpu-slayerfs.folded" \
                > "$FLAME_DIR/oncpu-flame.svg"
            info "  on-CPU flame graph: $FLAME_DIR/oncpu-flame.svg"

            # Hotspot analysis
            info "  analyzing hotspots..."
            python3 "$SCRIPT_DIR/analyze_flame.py" --hotspots "$FLAME_DIR/oncpu-slayerfs.folded" 2>/dev/null || true
        else
            warn "  no slayerfs samples captured — is the workload too short?"
        fi

        # Clean up intermediate file
        rm -f "$FLAME_DIR/oncpu-raw.txt"
    fi
fi

# =========================================================================
# OFF-CPU FLAME GRAPH
# =========================================================================
if [ "$SKIP_OFFCPU" -eq 0 ]; then
    info "=== OFF-CPU profiling (sched:sched_switch) ==="

    rm -rf "$MNT_DIR"/* 2>/dev/null || true

    # Use fp call graph for off-cpu too — dwarf on large binaries causes
    # "could not read first record" errors with addr2line.
    perf record -e 'sched:sched_switch' --call-graph fp -a -o "$FLAME_DIR/offcpu-perf.data" &
    PERF_OFFCPU_PID=$!
    sleep 1

    info "  fio seqwrite (off-CPU)..."
    fio --name=offcpu-seqwrite --directory="$MNT_DIR" --rw=write --bs=4m \
        --size="$SEQ_SIZE" --numjobs=1 --ioengine=sync --direct=0 \
        --runtime="$RUNTIME" --time_based --group_reporting --eta=never \
        --output-format=terse 2>/dev/null || true

    info "  fio seqread (off-CPU)..."
    fio --name=offcpu-seqread --directory="$MNT_DIR" --rw=read --bs=4m \
        --size="$SEQ_SIZE" --numjobs=1 --ioengine=sync --direct=0 \
        --runtime="$RUNTIME" --time_based --group_reporting --eta=never \
        --output-format=terse 2>/dev/null || true

    kill -INT "$PERF_OFFCPU_PID" 2>/dev/null || true
    wait "$PERF_OFFCPU_PID" 2>/dev/null || true

    if [ -f "$FLAME_DIR/offcpu-perf.data" ]; then
        info "generating off-CPU flame graph..."
        python3 "$SCRIPT_DIR/analyze_flame.py" --offcpu \
            "$FLAME_DIR/offcpu-perf.data" "$FLAME_DIR/offcpu-slayerfs.folded" 2>/dev/null || true

        if [ -s "$FLAME_DIR/offcpu-slayerfs.folded" ]; then
            inferno-flamegraph --title "SlayerFS Off-CPU" \
                "$FLAME_DIR/offcpu-slayerfs.folded" \
                > "$FLAME_DIR/offcpu-flame.svg"
            info "  off-CPU flame graph: $FLAME_DIR/offcpu-flame.svg"
        else
            warn "  no off-CPU slayerfs samples captured"
        fi
    fi
fi

# =========================================================================
# Crypto overhead analysis
# =========================================================================
if [ -f "$FLAME_DIR/oncpu-slayerfs.folded" ]; then
    info "=== crypto overhead ==="
    python3 "$SCRIPT_DIR/analyze_flame.py" --crypto "$FLAME_DIR/oncpu-slayerfs.folded" 2>/dev/null || true
fi

# =========================================================================
# LLM-readable report
# =========================================================================
info "=== LLM-readable report ==="
HOTSPOTS_ARG=""
if [ -f "$FLAME_DIR/oncpu-slayerfs.folded" ]; then
    HOTSPOTS_ARG="--hotspots $FLAME_DIR/oncpu-slayerfs.folded"
fi
python3 "$SCRIPT_DIR/analyze_perf.py" --llm $HOTSPOTS_ARG "$PERF_DIR" \
    > "$PERF_DIR/llm-report.txt" 2>/dev/null || true
info "  LLM report: $PERF_DIR/llm-report.txt"
cat "$PERF_DIR/llm-report.txt" 2>/dev/null || true

# =========================================================================
# Summary
# =========================================================================
info "=============================================="
info "results saved to: $FLAME_DIR"
ls -lh "$FLAME_DIR"/*.svg 2>/dev/null || true
info "=============================================="
echo ""
echo "  open flame graphs:"
for svg in "$FLAME_DIR"/*.svg; do
    [ -f "$svg" ] || continue
    echo "    file://$svg"
done
echo ""
echo "  perf data (for further analysis):"
ls -lh "$FLAME_DIR"/*.data 2>/dev/null || true
