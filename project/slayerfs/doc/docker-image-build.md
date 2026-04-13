# SlayerFS Docker Image Build

本文档说明当前仓库中 SlayerFS Docker 镜像的构建入口、前置条件和 CI 发布方式。

## 构建入口

- Dockerfile: `project/slayerfs/docker/Dockerfile`
- Entrypoint: `project/slayerfs/docker/entrypoint.sh`
- GitHub Actions workflow: `.github/workflows/slayerfs-docker.yml`

镜像当前只承担两件事：

1. 构建并封装 `slayerfs` 主二进制。
2. 从 xfstests 预构建包中复制一个辅助二进制到镜像内，默认是 `xfs_io`。

## 前置条件

Dockerfile 依赖 Git LFS 管理的预构建压缩包：

- `project/slayerfs/tests/scripts/xfstests-prebuilt/xfstests-prebuilt.tar.gz`

构建前先在仓库根目录拉取该资源：

```bash
git lfs install --local
git lfs pull --include="project/slayerfs/tests/scripts/xfstests-prebuilt/*.tar.gz"
```

如果没有先拉取 LFS 文件，构建阶段在解压预构建包时会失败。

## 本地构建

需要从 `rk8s` 仓库根目录执行构建，使上下文与 CI 保持一致：

```bash
docker build \
  -f project/slayerfs/docker/Dockerfile \
  -t slayerfs:local \
  project
```

## 可选构建参数

Dockerfile 暴露了一个可选参数：

- `XFSTESTS_BINARY`：指定要从预构建包中复制到镜像内的 xfstests 辅助工具，默认值为 `xfs_io`

例如：

```bash
docker build \
  -f project/slayerfs/docker/Dockerfile \
  --build-arg XFSTESTS_BINARY=xfs_io \
  -t slayerfs:local \
  project
```

## 镜像内容

镜像构建完成后，关键内容如下：

- `/usr/local/bin/slayerfs`：主程序
- `/usr/local/bin/slayerfs-entrypoint`：默认入口脚本
- `/opt/xfstests/bin/<tool>`：从预构建包提取出的 xfstests 辅助二进制
- `/opt/xfstests/lib`：预构建包中存在时一并复制的运行库

默认入口会执行：

```bash
slayerfs mount --config /run/slayerfs/config.yaml /mnt/slayerfs
```

## 默认运行时环境变量

镜像内预置了以下默认值：

- `SLAYERFS_HOME=/var/lib/slayerfs`
- `SLAYERFS_MOUNT_POINT=/mnt/slayerfs`
- `SLAYERFS_DATA_BACKEND=local-fs`
- `SLAYERFS_DATA_DIR=/var/lib/slayerfs/data`
- `SLAYERFS_META_BACKEND=sqlite`
- `SLAYERFS_SQLITE_PATH=/var/lib/slayerfs/metadata.db`
- `LD_LIBRARY_PATH=/opt/xfstests/lib`

其中：

- 数据后端默认使用本地目录。
- 元数据后端默认使用 sqlite。
- 如需切换到 redis、etcd 或 sqlx/postgres，对应参数由 `entrypoint.sh` 在启动时写入配置文件。

## CI 行为

`.github/workflows/slayerfs-docker.yml` 包含两个 job：

1. `docker-build`
   - 在 pull request 和 push 到 `main` 时执行。
   - 先安装 `git-lfs` 并拉取预构建压缩包，再构建镜像。
2. `docker-publish`
   - 只在非 pull request 场景下触发。
   - 只有同时配置以下仓库变量或密钥时才会执行：
     - `SLAYERFS_DOCKERHUB_REPOSITORY`
     - `DOCKERHUB_USERNAME`
     - `DOCKERHUB_TOKEN`

发布时使用 `docker/metadata-action` 生成标签，包含默认分支的 `latest`、分支名、tag 和 sha 标签。