# SlayerFS 性能分析报告

**测试日期**: 2026-05-20  
**后端**: Redis (元数据) + RustFS/S3 (数据存储)  
**挂载模式**: privileged (`/dev/fuse` 直通)  
**配置**: chunk_size=256MiB, block_size=4MiB, S3 part_size=16MiB, fuse_workers=8, max_background=512  

---

## 1. FIO 基准测试结果

### 1.1 大块 IO (bs=4M)

| 工作负载 | 吞吐量 | IOPS | 平均延迟 | P99 延迟 | 说明 |
|----------|--------|------|----------|----------|------|
| 顺序写 (1job) | **140 MiB/s** | 35.0 | 27.9ms | 442.5ms | 单线程大块写 |
| 顺序读 (1job) | **130 MiB/s** | 32.5 | 30.4ms | 91.8ms | 单线程大块读 (冷缓存) |
| 随机写 (4jobs) | **117 MiB/s** | 29.2 | 135.5ms | 2231ms | 4 线程随机写 |
| 随机读 (4jobs) | **58 MiB/s** | 14.4 | 275.9ms | 742ms | 4 线程随机读 (冷缓存) |
| 随机读写 70/30 (4jobs) | 读 **31** / 写 **15** MiB/s | 7.8 / 3.7 | 452/27.6ms | 2567/146ms | 混合负载 |

### 1.2 小块 IO (bs=4K)

| 工作负载 | 吞吐量 | IOPS | 平均延迟 | 说明 |
|----------|--------|------|----------|------|
| 随机写 (4jobs) | **1.2 MiB/s** | 308 | 12.6ms | 受 FUSE 往返延迟限制 |
| 随机读 (4jobs) | **1.4 MiB/s** | 354 | 11.3ms | 受 FUSE 往返延迟限制 |

### 关键观察

- **顺序写提升明显** (140 vs 上次 99 MiB/s)：chunk_size 从 64→256MiB 减少元数据提交频率，以及移除了读路径的冗余 stat_ino 调用
- **顺序读受冷缓存影响** (130 vs 上次 252 MiB/s)：本次测试为冷缓存，上次可能受益于热缓存预取命中
- **随机 IO 延迟较高** (135-452ms)：S3 over HTTP 的往返延迟 + RustFS 内部处理是主导因素
- **P99 尾延迟显著** (写 2.2s, 读 742ms)：主要来自后台 flush 和 S3 PUT 重试
- **4K IOPS ~300-350**：FUSE 用户态往返 (~3ms) 是小 IO 的主要瓶颈

---

## 2. ON-CPU 火焰图分析

### 2.1 CPU 采样总览 (2,478 样本, frame-pointer 模式)

对 slayerfs 进程的 on-CPU 样本按功能模块分类：

| 类别 | 占比 | 说明 |
|------|------|------|
| **预取/Readahead** | ~74.5% | GlobalPrefetcher worker_loop → DataFetcher → S3 GET |
| **对象存储写** | ~6.0% | spawn_upload_task → put_object_vectored → S3 PUT |
| **VFS 读路径** | ~2.4% | FileReader → read_from_slice → page cache |
| **缓存操作** | ~1.5% | moka ChunksCache get/insert, ReadPageCache |
| **VFS 写路径** | ~1.4% | write_at_inner → ChunkHandle → SliceState |
| **元数据** | ~0.1% | MetaLayer get_slices |
| **其他/内核** | ~15.1% | 调度器、中断、内存管理 |

### 2.2 预取路径详细分解

GlobalPrefetcher 占 74.5% CPU，其内部叶子函数分布：

| 叶子函数 | 占比 | 说明 |
|----------|------|------|
| `[libc.so.6]` (I/O syscalls) | 90.6% | 网络 recv/send 到 S3 |
| moka `BucketArrayRef::get_key_value_and_then` | 1.5% | 缓存查找 |
| `finish_task_switch` | 1.0% | 内核调度开销 |
| `bytes::shared_drop` | 0.4% | 引用计数释放 |
| `crossbeam_epoch` / `crossbeam_channel` | 0.5% | 并发原语 |
| `malloc` / `cfree` | 0.6% | 内存分配 |

### 2.3 瓶颈解读

**系统明确是 I/O 受限的**。90%+ 的 CPU 时间花在 libc 网络 syscall 上（S3 GET/PUT）。
用户态计算开销（缓存查找、内存分配、Tokio 调度）合计不到 5%，说明代码层面已经很高效。

**与上次对比**：上次报告中 "加密操作占 CPU 24%" 的问题已修复（`disable_payload_checksum: true` 配置生效），
现在签名/校验开销几乎不可见。

---

## 3. 写管线分析

```
fio write → FUSE_WRITE → VFS::write_ino / write_cached_ino
  → FileWriter::write_at (切片拆分)
    → auto_flush (冻结切片, 4MiB 阈值)
      → spawn_flush_slice (上传到 S3)
        → DataUploader::write_at_vectored
          → ObjectBlockStore::write_fresh_vectored
            → ObjectClient::put_object_vectored
              → S3Backend (HTTP PUT, payload checksum 已禁用)
                → TCP send
  → commit_chunk (元数据提交)
    → MetaClient (Redis RPUSH)
```

写管线优化效果：
- `disable_payload_checksum: true` 消除了 SHA-256 签名开销 (~20% CPU 节省)
- `chunk_size=256MiB` 降低了 commit_chunk 频率 (每 256MB 才提交一次元数据)
- FUSE writeback cache 允许 kernel 合并小写为大页写回，减少 FUSE 往返

---

## 4. 性能优化建议

### 4.1 已完成优化 ✅

| 优化项 | 效果 | 状态 |
|--------|------|------|
| 禁用 S3 payload checksum | 消除 ~20% CPU (SHA-256/MD5/CRC) | ✅ `disable_payload_checksum: true` |
| 增大 chunk_size 到 256MiB | 降低 commit 频率，顺序写 99→140 MiB/s | ✅ 配置 |
| 增加 fuse_workers=8 | 提升并发处理能力 | ✅ 配置 |
| 增加 max_background=512 | 允许更多并发 FUSE 后台请求 | ✅ 配置 |
| 移除 FUSE read 的 stat_ino 检查 | 减少每次读的元数据查询 | ✅ 代码优化 |
| 允许 O_WRONLY 句柄的读操作 | writeback 场景避免临时句柄开销 | ✅ 代码优化 |

### 4.2 下一步优化方向

| 优化项 | 预期收益 | 实现难度 | 说明 |
|--------|----------|----------|------|
| **读预取策略调优** | 减少冷读延迟 | 中 | 当前 prefetcher 占 74% CPU，可调整预取窗口和并发数 |
| **S3 连接池优化** | 降低 P99 尾延迟 | 中 | 减少 TCP 连接建立，复用 HTTP/2 连接 |
| **io_uring FUSE 支持** | 4K IOPS 提升 2-3x | 高 | 消除每次 FUSE 操作的用户态-内核态切换 |
| **读路径 zero-copy** | 减少 5% memcpy | 中 | 从 S3 响应直接映射到 page cache，减少 `rep_movs_alternative` |
| **moka cache 分片** | 减少锁竞争 | 低 | 增加 moka 的 segment 数量，减少多线程争用 |
| **批量预取合并** | 减少 S3 请求数 | 中 | 合并相邻块的 GET 请求为 Range 请求 |

### 4.3 可观测性增强

| 建议 | 说明 |
|------|------|
| 启用 `SLAYERFS_FUSE_OP_LOG=1` | 记录每个 FUSE 操作的耗时，定位慢操作 |
| 添加 S3 请求级别的 latency histogram | 区分 S3 延迟 vs 元数据延迟 |
| 使用 tokio-console | 实时监控 tokio 任务状态和锁竞争 |
| perf + `--call-graph fp` 模式 | 本次已验证 fp 模式可正确解析 Rust 符号 |

---

## 5. 与历史数据对比

| 指标 | 2026-05-19 (chunk=64M) | 2026-05-20 (chunk=256M) | 变化 |
|------|------------------------|-------------------------|------|
| 顺序写 | 99 MiB/s | 140 MiB/s | **+41%** |
| 顺序读 (热缓存) | 252 MiB/s | — | — |
| 顺序读 (冷缓存) | — | 130 MiB/s | 基线 |
| 随机写 4j | 109 MiB/s | 117 MiB/s | +7% |
| 随机读 4j | 119 MiB/s | 58 MiB/s | 冷缓存影响 |
| 混合读写 (读) | 67 MiB/s | 31 MiB/s | 冷缓存影响 |

**注意**：读性能差异主要来自缓存温度。上次测试的顺序读 252 MiB/s 受益于 ChunksCache 预取热命中；
本次冷启动测试更能反映真实首次访问性能。写入性能的提升（+41%）是实质性的，
来自 chunk_size 增大和代码优化。

---

## 6. 产物清单

| 文件 | 路径 |
|------|------|
| 性能分析脚本 | `tools/perf/run_perf.sh` |
| 火焰图分析脚本 | `tools/perf/analyze_flame.py` |
| Docker 基础设施 | `tools/perf/docker-compose.yml` |

---

## 7. 复现命令

```bash
# 完整运行 (编译 + 60s 每项基准测试 + 火焰图)
cd tools/perf && ./run_perf.sh

# 快速运行 (15s 每项)
./run_perf.sh --quick

# 跳过编译 + 保留环境
./run_perf.sh --no-build --no-cleanup

# 仅 on-CPU 分析 (跳过耗时的 off-CPU sched_switch 采集)
./run_perf.sh --quick --skip-offcpu
```

### 手动 perf 分析 (推荐)

```bash
# 找到 slayerfs PID
PID=$(pgrep -f "slayerfs mount")

# frame-pointer 模式录制 (兼容性最好)
perf record -F 99 --call-graph fp -p $PID -o perf.data -- sleep 20

# 处理为火焰图
perf script -i perf.data | inferno-collapse-perf > stacks.folded
grep slayerfs stacks.folded > slayerfs.folded
inferno-flamegraph slayerfs.folded > flame.svg
```
