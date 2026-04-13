#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SLAYERFS_DIR="$(realpath "$SCRIPT_DIR/../..")"
PROJECT_DIR="$(realpath "$SLAYERFS_DIR/..")"
DOCKERFILE="${SLAYERFS_DOCKERFILE:-$SLAYERFS_DIR/docker/Dockerfile}"
COMPOSE_FILE="${SLAYERFS_DOCKER_COMPOSE_FILE:-$SLAYERFS_DIR/tests/docker-compose.image-test.yml}"

IMAGE_TAG="${SLAYERFS_DOCKER_IMAGE:-slayerfs:test}"
BACKENDS=()
PUSH_IMAGE=false
SKIP_BUILD=false
KEEP_ON_FAILURE=false
ACTIVE_PROJECT=""
TEST_FAILED=false

usage() {
    cat <<EOF
用法: $(basename "$0") [选项]

  --image <tag>        指定镜像标签，默认: ${IMAGE_TAG}
  --backend <name>     仅测试指定后端，可重复传入: sqlite | redis | etcd
  --skip-build         跳过 docker build，直接使用现有镜像
  --push               在测试通过后执行 docker push
  --keep-on-failure    失败时保留 compose 现场，便于手动排查
  -h, --help           显示帮助

环境变量:
  DOCKERHUB_USERNAME   可选，配合 DOCKERHUB_TOKEN 自动登录 Docker Hub
  DOCKERHUB_TOKEN      可选，配合 DOCKERHUB_USERNAME 自动登录 Docker Hub

示例:
  $(basename "$0") --image yourname/slayerfs:dev
  $(basename "$0") --image yourname/slayerfs:latest --push
  $(basename "$0") --skip-build --image yourname/slayerfs:latest --backend redis
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --image)
            IMAGE_TAG="$2"
            shift
            ;;
        --backend)
            BACKENDS+=("$2")
            shift
            ;;
        --skip-build)
            SKIP_BUILD=true
            ;;
        --push)
            PUSH_IMAGE=true
            ;;
        --keep-on-failure)
            KEEP_ON_FAILURE=true
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "未知参数: $1" >&2
            usage
            exit 1
            ;;
    esac
    shift
done

if [[ ${#BACKENDS[@]} -eq 0 ]]; then
    BACKENDS=(sqlite redis etcd)
fi

log() {
    echo "[$(date '+%H:%M:%S')] $*"
}

info() {
    log "INFO  $*"
}

ok() {
    log "OK    $*"
}

err() {
    log "ERROR $*" >&2
}

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        err "缺少命令: $1"
        exit 1
    fi
}

compose() {
    COMPOSE_PROJECT_NAME="$ACTIVE_PROJECT" \
    SLAYERFS_TEST_IMAGE="$IMAGE_TAG" \
    SLAYERFS_TEST_BACKEND="${SLAYERFS_TEST_BACKEND:-sqlite}" \
    SLAYERFS_TEST_META_URL="${SLAYERFS_TEST_META_URL:-}" \
    SLAYERFS_TEST_META_ETCD_URLS="${SLAYERFS_TEST_META_ETCD_URLS:-http://etcd:2379}" \
        docker compose -f "$COMPOSE_FILE" "$@"
}

cleanup_stack() {
    if [[ -n "$ACTIVE_PROJECT" ]]; then
        if [[ "$TEST_FAILED" == true && "$KEEP_ON_FAILURE" == true ]]; then
            info "保留 compose 现场: $ACTIVE_PROJECT"
        else
            compose down -v --remove-orphans >/dev/null 2>&1 || true
        fi
    fi
}

trap cleanup_stack EXIT INT TERM

wait_for_service() {
    local service="$1"
    local timeout_secs="${2:-120}"
    local start_ts
    start_ts=$(date +%s)

    while true; do
        local container_id
        container_id="$(compose ps -q "$service")"
        if [[ -n "$container_id" ]]; then
            local status
            status="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$container_id" 2>/dev/null || true)"
            case "$status" in
                healthy|running)
                    return 0
                    ;;
                exited|dead)
                    err "服务 $service 已退出"
                    return 1
                    ;;
            esac
        fi

        if (( $(date +%s) - start_ts >= timeout_secs )); then
            err "等待服务超时: $service"
            return 1
        fi
        sleep 2
    done
}

exec_in_slayerfs() {
    compose exec -T slayerfs sh -lc "$1"
}

dump_debug_info() {
    info "输出 compose 状态"
    compose ps || true

    for service in slayerfs redis etcd; do
        if [[ -n "$(compose ps -q "$service")" ]]; then
            info "输出 $service 日志"
            compose logs "$service" || true
        fi
    done

    if [[ -n "$(compose ps -q slayerfs)" ]]; then
        info "输出 slayerfs 容器内调试信息"
        exec_in_slayerfs 'mount | grep -E "slayerfs|fuse" || true; echo ---; ls -la /mnt/slayerfs || true; echo ---; find /var/lib/slayerfs -maxdepth 3 -mindepth 1 -print | sort || true' || true
    fi
}

run_checks() {
    exec_in_slayerfs 'set -e; echo hello > /mnt/slayerfs/a; grep -q hello /mnt/slayerfs/a; mv /mnt/slayerfs/a /mnt/slayerfs/b; rm /mnt/slayerfs/b; mkdir /mnt/slayerfs/dir; rmdir /mnt/slayerfs/dir'

    exec_in_slayerfs 'set -e; echo persist > /mnt/slayerfs/persist.txt; mkdir -p /mnt/slayerfs/pdir/sub; sync'

    compose restart slayerfs >/dev/null
    wait_for_service slayerfs 120

    exec_in_slayerfs 'set -e; grep -q persist /mnt/slayerfs/persist.txt; test -d /mnt/slayerfs/pdir/sub'

    exec_in_slayerfs 'set -e; mkdir -p /mnt/slayerfs/edge/dir1 /mnt/slayerfs/edge/dir2; echo original > /mnt/slayerfs/edge/dir1/file; ln /mnt/slayerfs/edge/dir1/file /mnt/slayerfs/edge/dir1/hardlink; test "$(stat -c %h /mnt/slayerfs/edge/dir1/file)" -eq 2; echo via-link >> /mnt/slayerfs/edge/dir1/hardlink; grep -q via-link /mnt/slayerfs/edge/dir1/file; ln -s /mnt/slayerfs/edge/dir1/file /mnt/slayerfs/edge/dir2/symlink; grep -q original /mnt/slayerfs/edge/dir2/symlink; mv /mnt/slayerfs/edge/dir1/file /mnt/slayerfs/edge/dir2/moved; test -f /mnt/slayerfs/edge/dir2/moved; test ! -f /mnt/slayerfs/edge/dir1/file'

    exec_in_slayerfs 'set -e; mkdir -p /mnt/slayerfs/concurrency; rm -f /mnt/slayerfs/concurrency/file-*; for i in $(seq 1 4); do for j in $(seq 1 50); do echo "$i-$j" >> /mnt/slayerfs/concurrency/file-$i; done; done; total=0; for i in $(seq 1 4); do count=$(wc -l < /mnt/slayerfs/concurrency/file-$i); test "$count" -eq 50; total=$((total + count)); done; test "$total" -eq 200'
}

build_image() {
    info "构建镜像: $IMAGE_TAG"
    docker build -f "$DOCKERFILE" -t "$IMAGE_TAG" "$PROJECT_DIR"
}

push_image() {
    if [[ -n "${DOCKERHUB_USERNAME:-}" && -n "${DOCKERHUB_TOKEN:-}" ]]; then
        info "登录 Docker Hub: $DOCKERHUB_USERNAME"
        printf '%s' "$DOCKERHUB_TOKEN" | docker login --username "$DOCKERHUB_USERNAME" --password-stdin
    fi

    info "推送镜像: $IMAGE_TAG"
    docker push "$IMAGE_TAG"
}

run_backend() {
    local backend="$1"

    ACTIVE_PROJECT="slayerfs-image-${backend}-$$"
    SLAYERFS_TEST_BACKEND="$backend"
    SLAYERFS_TEST_META_URL=""
    SLAYERFS_TEST_META_ETCD_URLS="http://etcd:2379"

    info "=== 开始测试后端: $backend ==="

    case "$backend" in
        sqlite)
            ;;
        redis)
            SLAYERFS_TEST_META_URL="redis://redis:6379/0"
            compose up -d redis
            wait_for_service redis 60
            ;;
        etcd)
            compose up -d etcd
            wait_for_service etcd 60
            ;;
        *)
            err "不支持的后端: $backend"
            exit 1
            ;;
    esac

    compose up -d slayerfs
    wait_for_service slayerfs 120
    run_checks
    ok "后端测试通过: $backend"

    compose down -v --remove-orphans >/dev/null
    ACTIVE_PROJECT=""
}

main() {
    require_cmd docker

    if ! docker info >/dev/null 2>&1; then
        err "Docker 不可用，或当前用户无权限访问 Docker"
        exit 1
    fi

    if ! docker compose version >/dev/null 2>&1; then
        err "需要 docker compose v2"
        exit 1
    fi

    if [[ "$SKIP_BUILD" == false ]]; then
        build_image
    else
        info "跳过镜像构建，直接使用: $IMAGE_TAG"
    fi

    for backend in "${BACKENDS[@]}"; do
        if ! run_backend "$backend"; then
            TEST_FAILED=true
            dump_debug_info
            exit 1
        fi
    done

    if [[ "$PUSH_IMAGE" == true ]]; then
        push_image
    fi

    ok "全部 Docker 镜像测试完成"
}

main "$@"