#!/usr/bin/env bash

set -euo pipefail

mountpoint -q "${SLAYERFS_MOUNT_POINT:-/mnt/slayerfs}"