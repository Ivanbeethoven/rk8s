# SlayerFS 性能分析报告

**测试日期**: 2026-05-19  
**后端**: Redis (元数据) + RustFS/S3 (数据存储)  
**挂载模式**: privileged (`/dev/fuse` 直通)  
**配置**: chunk_size=64MiB, block_size=4MiB, S3 part_size=16MiB  

---

## 1. FIO 基准测试结果

| 工作负载 | 吞吐量 | IOPS | 平均延迟 | 说明 |
|----------|--------|------|----------|------|
| 顺序写 (bs=4m, 1job) | **99 MiB/s** | 24.8 | 39.8ms | 单线程大块写 |
| 顺序读 (bs=4m, 1job) | **252 MiB/s** | 63.1 | 15.1ms | 单线程大块读 |
| 随机写 (bs=4m, 4jobs) | **109 MiB/s** | 27.3 | 140.7ms | 4 线程随机写 |
| 随机读 (bs=4m, 4jobs) | **119 MiB/s** | 29.8 | 132.9ms | 4 线程随机读 |
| 随机读写 70/30 (bs=4m, 4jobs) | 读 **67** / 写 **30** MiB/s | 18.3 / 8.6 | 202.6/17.1ms | 混合负载 |

### 关键观察

- **顺序读远快于顺序写** (252 vs 99 MiB/s)：读路径受益于 ChunksCache (moka) 预取命中；写路径受限于 S3 PUT 延迟和元数据提交
- **随机 IO 延迟较高** (130-200ms)：S3 over HTTP 的往返延迟 + RustFS 内部处理是主导因素
- **4job 并发有一定收益**：随机读从 ~63 提升到 119 MiB/s，但延迟也显著增加

---

## 2. ON-CPU 火焰图分析

### 2.1 CPU 采样总览

对 slayerfs 进程的 90,408 个 on-CPU 样本进行分类：

| 类别 | 占比 | 说明 |
|------|------|------|
| **Tokio async runtime** | ~21% | tokio worker 线程调度、任务 poll |
| **用户态逻辑 (未解析符号)** | ~27% | Rust async 任务——slayerfs 写管线、读管线、compaction |
| **内核网络栈 (TCP)** | ~10% | writev/recv → tcp_sendmsg/tcp_recvmsg → S3/Redis 通信 |
| **内核 VFS/块层** | ~5% | ext4 回写 (fio 的写入先到内核 page cache) |
| **内存操作** | ~4% | memcpy, clear_page, malloc/free |
| **其他内核** | ~33% | 中断处理、调度器、软中断等 |

### 2.2 瓶颈解读

**网络是最大 CPU 消费者**。火焰图中最深的调用栈几乎全是 TCP 收发路径：

```
slayerfs → writev → tcp_sendmsg → ... → __dev_queue_xmit
slayerfs → recv   → tcp_recvmsg → ... → ip_rcv
```

这说明 slayerfs 的 CPU 时间主要是**在等待网络 IO 的同时被内核唤醒处理 TCP 协议栈**。对于 S3 后端的 FUSE 文件系统，这种模式是预期之中的，但仍有优化空间。

**用户态符号未完全解析**。虽然用 `force-frame-pointers=yes` + `debug=true` 编译，但 Rust async 状态机的栈帧展开并不完整。slayerfs 的函数名在 perf 中只显示为 `[slayerfs]` 而非具体函数名。建议使用 `perf record --call-graph dwarf` 替代 frame pointers 来获取更准确的调用栈。

---

## 3. FUSE 写管线分析

根据代码审查和 perf 数据推断的写路径：

```
fio write → FUSE_WRITE → VFS::write_ino
  → FileWriter::write_at (切片拆分)
    → auto_flush (冻结切片)
      → spawn_flush_slice (上传到 S3)
        → DataUploader::write_at_vectored
          → ObjectBlockStore::write_fresh_vectored
            → ObjectClient::put_object_vectored
              → S3Backend::put_object_simple (HTTP PUT)
                → AWS SigV4 签名 (SHA-256)
                → MD5 Content-MD5
                → CRC 校验
                  → TCP send
  → commit_chunk (元数据提交)
    → MetaClient (Redis RPUSH)
```

每个 chunk 的数据流需要经历：SHA-256 签名 → MD5 哈希 → CRC 校验 → TCP 发送 → Redis 元数据提交。在之前的分析中，加密操作（SHA-256 + MD5 + CRC）曾占 CPU 的 ~24%，是 RustFS（非 AWS S3）场景下不必要的开销。

---

## 4. 性能优化建议

### 4.1 高优先级

| 优化项 | 预期收益 | 实现难度 |
|--------|----------|----------|
| **禁用 S3 SigV4 payload signing** | 减少 ~20% CPU | 低 — 在 S3Backend 设置 `payload_checksum_enabled=false` |
| **跳过 MD5 Content-MD5** | 减少 ~7% CPU | 低 — S3 client 配置 |
| **增大 chunk_size 到 256MiB** | 降低元数据提交频率 | 低 — 修改 config |
| **增加 fuse_workers** | 提升并发处理能力 | 低 — `--fuse-workers 8` |

### 4.2 中优先级

| 优化项 | 预期收益 | 实现难度 |
|--------|----------|----------|
| **S3 连接池复用** | 减少 TCP 连接建立开销 | 中 — 调整 hyper/aws-smithy client pool 配置 |
| **批量 S3 PUT** | 减少 HTTP 往返次数 | 中 — 合并相邻切片上传 |
| **ChunksCache 预热** | 提升读吞吐 | 中 — 增大 moka cache 容量 |
| **使用 `perf record --call-graph dwarf`** | 获得准确的 Rust 函数级火焰图 | 低 — 修改采集脚本 |

### 4.3 可观测性增强

| 建议 | 说明 |
|------|------|
| 启用 `SLAYERFS_FUSE_OP_LOG=1` | 记录每个 FUSE 操作的耗时，定位慢操作 |
| 添加 S3 请求级别的 latency histogram | 区分 S3 延迟 vs 元数据延迟 |
| 使用 tokio-console | 实时监控 tokio 任务状态和锁竞争 |

---

## 5. 与历史数据对比

| 指标 | 本次 (Redis+RustFS) | 前次 (Redis+RustFS, 热缓存) |
|------|---------------------|------------------------------|
| 顺序写 | 99 MiB/s | 71-87 MiB/s |
| 顺序读 | 252 MiB/s | 120-175 MiB/s |
| 随机读写 (读) | 67 MiB/s | 29-57 MiB/s |

性能波动主要来自 RustFS/MinIO 的内部状态和 compaction 后台任务的影响。

---

## 6. 产物清单

| 文件 | 路径 |
|------|------|
| ON-CPU 火焰图 | `/tmp/slayerfs-perf/flame/oncpu-flame.svg` |
| 系统级火焰图 | `/tmp/slayerfs-perf/flame/system-flame.svg` |
| perf 原始数据 | `/tmp/slayerfs-perf/flame/oncpu-perf.data` (13MB, 90K 样本) |
| 测试脚本 | `tools/perf/run_perf.sh` |
| Docker 基础设施 | `tools/perf/docker-compose.yml` |

---

## 7. 复现命令

```bash
# 启动基础设施
cd tools/perf
docker compose up -d --wait

# 编译带 profiling 的 release 二进制
cd ../..
RUSTFLAGS="-C force-frame-pointers=yes" \
  CARGO_PROFILE_RELEASE_DEBUG=true \
  cargo build --release --features profiling

# 运行完整性能分析
./tools/perf/run_perf.sh

# 仅运行快速分析 (15s 每项)
./tools/perf/run_perf.sh --quick
```
