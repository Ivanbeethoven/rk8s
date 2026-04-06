#!/usr/bin/env bash

# 用途:
#   封装分布式测试过程中对远端节点的 SSH 和 SCP 操作。
#   统一处理 SSH 端口、密钥、附加参数以及 sudo 执行方式，供其他脚本复用。

ssh_exec() {
  local node="$1"
  shift

  local opts=()
  if [[ -n "${SSH_OPTS:-}" ]]; then
    read -r -a opts <<< "${SSH_OPTS}"
  fi
  if [[ -n "${SSH_KEY:-}" ]]; then
    opts+=( -i "${SSH_KEY}" )
  fi
  opts+=( -p "${SSH_PORT:-22}" )

  ssh "${opts[@]}" "${SSH_USER}@${node}" "$@"
}

ssh_run() {
  local node="$1"
  shift
  local cmd="$*"

  local opts=()
  if [[ -n "${SSH_OPTS:-}" ]]; then
    read -r -a opts <<< "${SSH_OPTS}"
  fi
  if [[ -n "${SSH_KEY:-}" ]]; then
    opts+=( -i "${SSH_KEY}" )
  fi
  opts+=( -p "${SSH_PORT:-22}" )

  printf '%s\n' "$cmd" | ssh "${opts[@]}" "${SSH_USER}@${node}" "bash -s"
}

ssh_exec_sudo() {
  local node="$1"
  shift
  local cmd="$*"

  local opts=()
  if [[ -n "${SSH_OPTS:-}" ]]; then
    read -r -a opts <<< "${SSH_OPTS}"
  fi
  if [[ -n "${SSH_KEY:-}" ]]; then
    opts+=( -i "${SSH_KEY}" )
  fi
  opts+=( -p "${SSH_PORT:-22}" )

  local sudo_parts=()
  read -r -a sudo_parts <<< "${REMOTE_SUDO:-sudo}"

  printf '%s\n' "$cmd" | ssh "${opts[@]}" "${SSH_USER}@${node}" "${sudo_parts[@]}" "bash -s"
}

scp_to() {
  local src="$1"
  local node="$2"
  local dest="$3"

  local opts=()
  if [[ -n "${SSH_OPTS:-}" ]]; then
    read -r -a opts <<< "${SSH_OPTS}"
  fi
  if [[ -n "${SSH_KEY:-}" ]]; then
    opts+=( -i "${SSH_KEY}" )
  fi
  opts+=( -P "${SSH_PORT:-22}" )

  scp "${opts[@]}" "$src" "${SSH_USER}@${node}:$dest"
}

scp_to_dir() {
  local src="$1"
  local node="$2"
  local dest="$3"

  local opts=()
  if [[ -n "${SSH_OPTS:-}" ]]; then
    read -r -a opts <<< "${SSH_OPTS}"
  fi
  if [[ -n "${SSH_KEY:-}" ]]; then
    opts+=( -i "${SSH_KEY}" )
  fi
  opts+=( -P "${SSH_PORT:-22}" -r )

  scp "${opts[@]}" "$src" "${SSH_USER}@${node}:$dest"
}

scp_from() {
  local node="$1"
  local src="$2"
  local dest="$3"

  local opts=()
  if [[ -n "${SSH_OPTS:-}" ]]; then
    read -r -a opts <<< "${SSH_OPTS}"
  fi
  if [[ -n "${SSH_KEY:-}" ]]; then
    opts+=( -i "${SSH_KEY}" )
  fi
  opts+=( -P "${SSH_PORT:-22}" )

  scp "${opts[@]}" "${SSH_USER}@${node}:$src" "$dest"
}

scp_from_dir() {
  local node="$1"
  local src="$2"
  local dest="$3"

  local opts=()
  if [[ -n "${SSH_OPTS:-}" ]]; then
    read -r -a opts <<< "${SSH_OPTS}"
  fi
  if [[ -n "${SSH_KEY:-}" ]]; then
    opts+=( -i "${SSH_KEY}" )
  fi
  opts+=( -P "${SSH_PORT:-22}" -r )

  scp "${opts[@]}" "${SSH_USER}@${node}:$src" "$dest"
}
