# SlayerFS 本地挂载指南

使用 Docker 在宿主机上运行 SlayerFS，挂载到本地目录。

## 前置条件

- Docker 已安装
- 宿主机支持 FUSE（`/dev/fuse` 可用）
- 已构建 `slayerfs:local` 镜像（`docker build -t slayerfs:local -f Dockerfile ../..`）

## 两种后端模式

### 模式 1：local-fs + SQLite（纯本地，零依赖）

最简单的模式，所有数据存储在宿主机本地，无需额外服务。

```bash
HOST_MOUNT="/mnt/slayerfs-local"

docker rm -f slayerfs-local 2>/dev/null
umount "$HOST_MOUNT" 2>/dev/null || true
mkdir -p "$HOST_MOUNT" /var/lib/slayerfs-state /var/log/slayerfs

docker run -d \
  --name slayerfs-local \
  --privileged \
  -v "$HOST_MOUNT:/mnt/slayerfs:shared" \
  -v /var/lib/slayerfs-state:/var/lib/slayerfs \
  -v /var/log/slayerfs:/var/log/slayerfs \
  -e SLAYERFS_DATA_BACKEND=local-fs \
  -e SLAYERFS_DATA_DIR=/var/lib/slayerfs/data \
  -e SLAYERFS_META_BACKEND=sqlite \
  -e SLAYERFS_SQLITE_PATH=/var/lib/slayerfs/metadata.db \
  -e SLAYERFS_LOG_FILE=/var/log/slayerfs/slayerfs.log \
  -e RUST_LOG=slayerfs=info \
  slayerfs:local

sleep 3
ls "$HOST_MOUNT"
```

| 配置项 | 值 | 说明 |
|---|---|---|
| `SLAYERFS_DATA_BACKEND` | `local-fs` | 数据存储后端 |
| `SLAYERFS_DATA_DIR` | `/var/lib/slayerfs/data` | 数据目录 |
| `SLAYERFS_META_BACKEND` | `sqlite` | 元数据后端 |
| `SLAYERFS_SQLITE_PATH` | `/var/lib/slayerfs/metadata.db` | SQLite 数据库路径 |

### 模式 2：S3 (RustFS) + Redis（分布式）

需要 RustFS 和 Redis 服务，适合模拟生产环境。

**docker-compose 文件** (`slayerfs-host.yml`)：

```yaml
name: slayerfs-host

x-slayerfs-image: &slayerfs-image
  image: slayerfs:local

services:
  rustfs:
    image: rustfs/rustfs:latest
    command: ["/data"]
    environment:
      - RUSTFS_ADDRESS=:9000
      - RUSTFS_ACCESS_KEY=rustfsadmin
      - RUSTFS_SECRET_KEY=rustfsadmin
    volumes:
      - rustfs-data:/data

  rustfs-init:
    image: amazon/aws-cli:2.23.0
    depends_on: [rustfs]
    entrypoint: ["/bin/bash", "/init.sh"]
    volumes:
      - ./rustfs-init.sh:/init.sh:ro
    environment:
      AWS_ACCESS_KEY_ID: rustfsadmin
      AWS_SECRET_ACCESS_KEY: rustfsadmin
      AWS_DEFAULT_REGION: us-east-1
      AWS_EC2_METADATA_DISABLED: "true"

  redis:
    image: redis:7.2-alpine
    command: redis-server --appendonly yes --appendfsync everysec
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 5s
      timeout: 3s
      retries: 10

  slayerfs:
    <<: *slayerfs-image
    depends_on:
      redis: { condition: service_healthy }
      rustfs-init: { condition: service_completed_successfully }
    privileged: true
    devices:
      - /dev/fuse:/dev/fuse
    cap_add: [SYS_ADMIN]
    security_opt: [apparmor=unconfined]
    environment:
      SLAYERFS_DATA_BACKEND: s3
      SLAYERFS_S3_BUCKET: slayerfs-data
      SLAYERFS_S3_ENDPOINT: http://rustfs:9000
      SLAYERFS_S3_REGION: us-east-1
      SLAYERFS_S3_FORCE_PATH_STYLE: "true"
      SLAYERFS_META_BACKEND: redis
      SLAYERFS_META_URL: redis://redis:6379/0
      RUST_LOG: slayerfs=info
      SLAYERFS_LOG_FILE: /var/log/slayerfs/slayerfs.log
      AWS_ACCESS_KEY_ID: rustfsadmin
      AWS_SECRET_ACCESS_KEY: rustfsadmin
      AWS_DEFAULT_REGION: us-east-1
      AWS_EC2_METADATA_DISABLED: "true"
    volumes:
      - slayerfs-state:/var/lib/slayerfs
      - /mnt/slayerfs-redis:/mnt/slayerfs:shared
      - /var/log/slayerfs-redis:/var/log/slayerfs
    ulimits:
      nofile: { soft: 65536, hard: 65536 }

volumes:
  rustfs-data:
  slayerfs-state:
```

**rustfs-init.sh**：

```bash
#!/bin/bash
set -e
mkdir -p /root/.aws
printf '[default]\ns3 =\n  addressing_style = path\n' > /root/.aws/config
until aws --endpoint-url http://rustfs:9000 s3api list-buckets >/dev/null 2>&1; do sleep 2; done
aws --endpoint-url http://rustfs:9000 s3api head-bucket --bucket slayerfs-data >/dev/null 2>&1 || \
  aws --endpoint-url http://rustfs:9000 s3api create-bucket --bucket slayerfs-data
echo "Bucket ready"
```

**启动**：

```bash
mkdir -p /mnt/slayerfs-redis /var/log/slayerfs-redis
docker compose -f slayerfs-host.yml up -d
```

| 配置项 | 值 | 说明 |
|---|---|---|
| `SLAYERFS_DATA_BACKEND` | `s3` | 数据存储后端 |
| `SLAYERFS_S3_ENDPOINT` | `http://rustfs:9000` | S3 端点 |
| `SLAYERFS_S3_BUCKET` | `slayerfs-data` | S3 bucket |
| `SLAYERFS_META_BACKEND` | `redis` | 元数据后端 |
| `SLAYERFS_META_URL` | `redis://redis:6379/0` | Redis 连接地址 |

## 后端组合一览

| 模式 | Data | Meta | 挂载点 | 适用场景 |
|---|---|---|---|---|
| 纯本地 | local-fs | sqlite | `/mnt/slayerfs-local` | 开发测试 |
| 分布式 | s3 (RustFS) | redis | `/mnt/slayerfs-redis` | 模拟生产 |

其他支持的后端可通过环境变量切换：

| 变量 | 可选值 |
|---|---|
| `SLAYERFS_DATA_BACKEND` | `local-fs`, `s3` |
| `SLAYERFS_META_BACKEND` | `sqlite`, `redis`, `etcd`, `postgres` |

## 常用管理命令

```bash
# 查看日志
docker logs slayerfs-local -f
tail -f /var/log/slayerfs/slayerfs.log

# 进入容器
docker exec -it slayerfs-local bash

# 查看挂载状态
mount | grep slayerfs

# 停止 & 清理
docker rm -f slayerfs-local
umount /mnt/slayerfs-local
```

## 核心参数

| 参数 | 默认值 | 说明 |
|---|---|---|
| `chunk_size` | `67108864` (64 MiB) | 文件分割块大小 |
| `block_size` | `4194304` (4 MiB) | IO 最小单元 |
| `buffer_size` | `314572800` (300 MiB) | 写缓冲软限制 |

可通过 `SLAYERFS_CHUNK_SIZE`、`SLAYERFS_BLOCK_SIZE` 环境变量调整。

## 故障排查

```bash
# FUSE 不可用
ls /dev/fuse              # 检查设备是否存在
modprobe fuse             # 加载内核模块

# 端口冲突 (6379, 9000)
ss -tlnp | grep -E '6379|9000'   # 检查占用
# 解决：不对外暴露 rustfs/redis 端口，容器通过 Docker 网络互联

# 挂载点 busy
fusermount3 -u /mnt/slayerfs-local   # 强制卸载
```
