#!/usr/bin/env bash

# 用途:
#   提供分布式测试脚本共享的基础工具函数。
#   包括日志输出、错误退出、必填变量校验、目录创建和时间戳生成等通用能力。

log_info() {
  printf '[INFO] %s\n' "$*"
}

log_warn() {
  printf '[WARN] %s\n' "$*"
}

log_error() {
  printf '[ERROR] %s\n' "$*" >&2
}

log_success() {
  printf '[OK] %s\n' "$*"
}

die() {
  log_error "$*"
  exit 1
}

require_var() {
  local name="$1"
  if [[ -z "${!name:-}" ]]; then
    die "Missing required config: ${name}"
  fi
}

ensure_dir() {
  local path="$1"
  if [[ -z "$path" ]]; then
    die "ensure_dir called with empty path"
  fi
  mkdir -p "$path"
}

now_stamp() {
  date '+%Y%m%d-%H%M%S'
}
