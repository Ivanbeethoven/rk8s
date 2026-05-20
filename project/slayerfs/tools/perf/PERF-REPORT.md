# SlayerFS 性能分析报告

**测试日期**: 2026-05-20 (v2, io_uring + flush fix)  
**后端**: Redis (元数据) + RustFS/S3 (数据存储, 本机 docker)  
**挂载模式**: privileged (`/dev/fuse` 直通, io_uring FUSE 连接)  
**配置**: chunk_size=256MiB, block_size=4MiB, S3 part_size=16MiB, fuse_workers=8, max_background=512  

---

## 1. FIO 基准测试结果

### 1.1 大块 IO (bs=4M)

| 工作负载 | 吞吐量 | IOPS | 平均延迟 | P99 延迟 | 说明 |
|----------|--------|------|----------|----------|------|
| 顺序写 (1job) | **92.7 MiB/s** | 23.2 | 42.69ms | 566ms | 含 FUSE flush 持久化 |
| 顺序读 (1job, 冷) | **164.3 MiB/s** | 41.1 | 23.96ms | 45.88ms | 并发块预取 |
| 随机写 (4jobs) | **113 MiB/s** | 28.2 | 138ms | — | 4 线程随机写+flush |
| 随机读 (4jobs, 冷) | **94 MiB/s** | 23.4 | 168ms | — | 4 线程冷缓存 |
| 混合读写 (4j, 70/30) | **60/29 MiB/s** | 15/7.1 | 251/20ms | — | 读写混合竞争 |

### 1.2 中等块 IO (bs=1M)

| 工作负载 | 吞吐量 | IOPS | 平均延迟 | P99 延迟 | 说明 |
|----------|--------|------|----------|----------|------|
| 顺序写 (2jobs) | **138.8 MiB/s** | 138.8 | 14.24ms | 154ms | 双线程并发写 |
| 顺序读 (2jobs) | **156.5 MiB/s** | 156.5 | 12.60ms | 33.82ms | 双线程并发读 |

### 1.3 随机 IO (bs=64K)

| 工作负载 | 吞吐量 | IOPS | 平均延迟 | P99 延迟 | 说明 |
|----------|--------|------|----------|----------|------|
| 随机写 (4jobs) | **57.0 MiB/s** | 911 | 4.37ms | 42.73ms | 中等块随机写 |
| 随机读 (4jobs) | **24.7 MiB/s** | 394 | 10.12ms | 45.35ms | 中等块冷缓存读 |

### 1.4 小块 IO (bs=4K)

| 工作负载 | 吞吐量 | IOPS | 平均延迟 | 说明 |
|----------|--------|------|----------|------|
| 随机写 (4jobs) | **7.0 MiB/s** | 1799 | 2.22ms | io_uring 降低 syscall 开销 |
| 随机读 (4jobs) | **1.8 MiB/s** | 462 | 8.64ms | S3 GET 延迟为主 |

### 关键观察

- **顺序读 164 MiB/s**：受限于本机 docker S3 带宽，block 级并发 GET 已充分利用
- **顺序写 92.7 MiB/s (真实持久化)**：修复了 FUSE flush 语义后，close() 确保数据落盘到 S3
- **4K 随机写 1799 IOPS**：io_uring FUSE 通道显著降低了系统调用开销 (2.22ms avg vs 旧 12.6ms)
- **4K 随机读 462 IOPS**：受限于 S3 GET 延迟 (8.64ms avg)
- **P99 尾延迟**：顺序写 P99=566ms 表明偶发 S3 上传慢（可能触发重试或连接重建）

---

## 2. ON-CPU 火焰图分析

### 2.1 CPU 采样总览 (3,755 样本, frame-pointer 模式)

对 slayerfs 进程的 on-CPU 样本按功能模块分类：

| 类别 | 占比 | 说明 |
|------|------|------|
| **网络 I/O (libc syscalls)** | ~78.4% | recv/send → S3 GET/PUT 网络传输 |
| **预取/Readahead** | ~73% | GlobalPrefetcher → DataFetcher → S3 GET |
| **对象存储读** | ~49% | ObjectBlockStore::read_range → S3Backend |
| **缓存查找** | ~41% | moka ChunksCache::get (缓存 miss → fetch) |
| **AWS SDK** | ~4.8% | aws_sdk_s3 GetObject + orchestrator |
| **io_uring FUSE** | ~3% | fuse-io-uring 线程 (readv/writev) |
| **Tokio 调度** | ~1% | multi_thread::worker::Context::run |

### 2.2 核心发现：系统完全受限于 S3 网络 I/O

| 层级 | Self% | 含义 |
|------|-------|------|
| `libc 0x198c89` (recv) | **54.3%** | S3 响应接收 |
| `libc 0x19943a` (send) | **13.8%** | S3 请求发送 |
| `libc 0x198c2d` (poll/recv) | **10.3%** | 网络事件等待 |

> 合计 78.4% 的 CPU 时间花在 libc 网络系统调用上。用户态计算开销（缓存、调度、内存分配）不到 5%。

---

## 3. io_uring FUSE 连接分析

### 3.1 架构

```
用户进程 write()/read() → 内核 VFS → FUSE 模块
  → /dev/fuse fd
    → io_uring readv (ring thread) → FUSE request dispatch
    → io_uring writev (reply ring threads) → FUSE reply

每个 FuseConnection 拥有独立 ring thread + 64-entry io_uring ring
Reply task 使用 try_clone() 获取独立 fd + ring (避免死锁)
```

### 3.2 io_uring 效果

| 指标 | 旧 (tokio read/write) | 新 (io_uring) | 改善 |
|------|----------------------|---------------|------|
| 4K 随机写 IOPS | 308 | **1799** | **+484%** |
| 4K 随机写延迟 | 12.6ms | **2.22ms** | **-82%** |
| 4K 随机读 IOPS | 354 | **462** | +30% |
| FUSE 通道 CPU | ~3% worker | **~3%** ring thread | 持平 |

> 4K 写延迟从 12.6ms 降至 2.22ms，主要因为 io_uring 避免了每次 FUSE 操作的 read()/write() 系统调用上下文切换。

---

## 4. 写管线分析

```
fio write → FUSE_WRITE → VFS::write_ino / write_cached_ino
  → FileWriter::write_at (切片拆分)
    → auto_flush (冻结切片, 4MiB 阈值)
      → spawn_flush_slice (上传到 S3)
        → DataUploader::write_at_vectored
          → ObjectBlockStore::write_fresh_vectored
            → ObjectClient::put_object_vectored
              → S3Backend (HTTP PUT, payload checksum 已禁用)
                → TCP send (io_uring writev 通道)
  → FUSE FLUSH → VFS::flush → flush_required (等待所有 slice committed)
  → commit_chunk (元数据提交)
    → MetaClient (Redis RPUSH)
```

### 4.1 flush 语义修正

**修复前**：FUSE `flush` handler 是空操作，`close()` 路径仅有 5s deadline → 大文件 flush timeout  
**修复后**：FUSE `flush` 调用 VFS::flush (300s deadline)，确保 close() 语义正确

影响：顺序写基准从 146→92.7 MiB/s（因为现在包含 S3 持久化等待），但消除了 flush timeout 错误。

---

## 5. 性能优化建议

### 5.1 已完成优化 ✅

| 优化项 | 效果 | 状态 |
|--------|------|------|
| 禁用 S3 payload checksum | 消除 ~20% CPU (SHA-256/MD5/CRC) | ✅ `disable_payload_checksum: true` |
| 增大 chunk_size 到 256MiB | 降低 commit 频率 | ✅ 配置 |
| 增加 fuse_workers=8 | 提升并发处理能力 | ✅ 配置 |
| 增加 max_background=512 | 允许更多并发 FUSE 后台请求 | ✅ 配置 |
| 移除 FUSE read 的 stat_ino 检查 | 减少每次读的元数据查询 | ✅ 代码优化 |
| **并发块读取 (DataFetcher)** | 随机读 58→156 MiB/s (+169%) | ✅ FuturesUnordered |
| **S3 上传并发 4→16** | 随机写 117→129 MiB/s (+10%) | ✅ max_concurrency=16 |
| **io_uring FUSE 通道** | 4K 写 IOPS 308→1799 (+484%) | ✅ ring thread 架构 |
| **FUSE flush 语义修正** | 消除 flush timeout 错误 | ✅ flush 持久化数据 |

### 5.2 下一步优化方向

| 优化项 | 预期收益 | 实现难度 | 说明 |
|--------|----------|----------|------|
| **S3 GET 请求合并** | 大文件读 +30% | 中 | 合并相邻 block 的 GET Range 请求，减少 HTTP 往返 |
| **S3 连接池预热** | P99 延迟 -50% | 低 | 预建立 HTTP/2 连接，避免首次请求的 TCP+TLS 握手 |
| **写管线流水线** | 顺序写 +40% | 高 | 上传与写入重叠：在上传 slice N 的同时填充 slice N+1 |
| **本地 SSD 缓存层** | 重复读 +10x | 中 | 使用 SSD 做 L2 cache，S3 只在 cache miss 时访问 |
| **io_uring 批量提交** | 系统调用 -30% | 中 | 批量提交多个 writev（reply），减少 submit_and_wait 调用 |
| **预取窗口自适应** | 顺序读延迟 -20% | 中 | 根据访问模式动态调整预取块数 |
| **Zero-copy read** | memcpy 开销 -5% | 高 | splice/registered buffers 直接传递 S3 数据到 FUSE |

### 5.3 性能瓶颈优先级

```
1. S3 网络延迟 (78.4% CPU 时间) ←← 主要瓶颈
   → 解法：连接复用、请求合并、本地缓存
   
2. FUSE flush 等待 (P99=566ms)
   → 解法：写管线流水线化、异步上传 overlap
   
3. 小 IO FUSE 开销 (2.22ms per 4K op)
   → 解法：io_uring 已优化，进一步需 kernel FUSE passthrough
   
4. 内存分配/缓存 (<5%)
   → 暂不需要优化
```

---

## 6. 与历史数据对比

| 指标 | v0 (chunk=64M, tokio) | v1 (chunk=256M, 并发块读) | v2 (io_uring + flush fix) | 说明 |
|------|----------------------|---------------------------|---------------------------|------|
| 顺序写 4M | 99 MiB/s | 146 MiB/s | **92.7 MiB/s** | v2 含真实持久化 |
| 顺序读 4M (冷) | — | 147 MiB/s | **164 MiB/s** | 读路径改善 |
| 随机写 4j 4M | 109 MiB/s | 129 MiB/s | **113 MiB/s** | 含 flush |
| 随机读 4j 4M | 58 MiB/s | 156 MiB/s | **94 MiB/s** | 测试条件不同 |
| 4K 随机写 IOPS | — | 308 | **1799** | **+484%** io_uring |
| 4K 随机写延迟 | — | 12.6ms | **2.22ms** | **-82%** |
| 4K 随机读 IOPS | — | 354 | **462** | +30% |

**注意**：
- v2 的顺序写/随机写看似"降低"，实际是因为修复了 FUSE flush 语义——数据在 close() 时必须落盘到 S3
- v1 的高写吞吐是假象：flush 是空操作，数据可能丢失
- 4K IOPS 大幅提升 (+484%) 来自 io_uring 消除 FUSE read/write 系统调用开销

---

## 7. 产物清单

| 文件 | 路径 |
|------|------|
| 性能分析脚本 | `tools/perf/run_perf.sh` |
| 火焰图分析脚本 | `tools/perf/analyze_flame.py` |
| Docker 基础设施 | `tools/perf/docker-compose.yml` |

---

## 8. 复现命令

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
