# SlayerFS vs JuiceFS 架构对比分析

## 概述

两者均为用户态分布式文件系统，基于 FUSE + 对象存储 + 元数据引擎的三层架构。本文聚焦
**VFS/FUSE 接口**和 **Chunk 处理**两个核心子系统的实现差异。

---

## 1. FUSE 接口层对比

| 维度 | SlayerFS (Rust/rfuse3) | JuiceFS (Go/go-fuse) |
|------|----------------------|---------------------|
| 语言 | Rust (async/tokio) | Go (goroutine) |
| FUSE 库 | rfuse3 (io_uring optional) | go-fuse/v2 |
| max_background | **512** | 50 |
| max_write | 4 MiB | 配置项 (默认 128 KiB) |
| max_readahead | 内核默认 (128 KiB) | 1 MiB |
| writeback_cache | ✅ 始终开启 | 可选 (mount option) |
| congestion_threshold | 未设置 | 未设置 |
| Open 返回 | `FOPEN_KEEP_CACHE` | 条件返回 `FOPEN_KEEP_CACHE` |
| 请求分发 | `Filesystem` trait impl, tokio worker pool | 每请求一个 goroutine |

### 关键差异

1. **并发模型**
   - SlayerFS: tokio worker pool + `max_background=512`，允许更多并发请求飞行
   - JuiceFS: `MaxBackground=50`，依赖 Go 调度器自动创建 goroutine

2. **Writeback Cache**
   - SlayerFS: 无条件启用，写入先到内核 page cache，由内核回写
   - JuiceFS: 可选启用；未启用时写入同步到 VFS 层

3. **读写一致性**
   - JuiceFS: `Read()` 前会先 `Flush()` 当前 inode 的 pending writes（保证 read-after-write）
   - SlayerFS: 依赖 overlay_dirty 覆盖读取，**不主动 flush**（可能更快但一致性模型不同）

4. **Cache 失效**
   - JuiceFS: Write 后立即 `Invalidate(ino, off, size)` reader cache + attrs
   - SlayerFS: Writer commit 后 invalidate reader cache（时序略延迟）

---

## 2. VFS Writer 对比

| 维度 | SlayerFS | JuiceFS |
|------|----------|---------|
| 数据模型 | `FileWriter` → `ChunkState` → `SliceState` | `fileWriter` → `chunkWriter` → `sliceWriter` |
| Slice 粒度 | 动态 (freeze_min_bytes=8~16 MiB) | 动态 (block 对齐) |
| 上传模式 | **Pipeline**: JoinSet 并发 block 上传 | **Per-slice goroutine**: 并发 block 上传 |
| Commit 顺序 | 保证 (front-of-queue check) | 保证 (`commitThread` 按序) |
| 全局背压 | `buffer_usage` AtomicU64, yield_now | `usedBufferSize()` 与 `bufferSize` 比较 |
| 背压策略 | yield_now backpressure | Sleep + 二级阈值 (1x warn, 2x block) |
| Flush 超时 | 300s (FLUSH_DEADLINE) | 5 min (retries derived) |
| Auto-flush 间隔 | 500ms (max_age) | 5s (flushDuration) |
| Disk staging | ✅ write-back cache (SSD, best-effort) | ✅ staging (configurable, hard-link into cache) |
| 写延迟来源 | S3 PUT (4MB block ~26ms) | Object PUT (block_size ~4MB) |

### 关键差异

1. **Upload Pipeline**
   - SlayerFS (新实现): 使用 `tokio::task::JoinSet` 在同一 slice 内并发上传多个 block。`dispatched_end` 记录分发边界，`block_done` 位掩码跟踪完成状态，`uploaded` 仅在连续完成时推进。
   - JuiceFS: 每个 slice 一个独立 goroutine (`go s.flushData()`)，内部按 block 串行 FlushTo()。多个 slice 可并行上传。

2. **全局内存管理**
   - JuiceFS: 有统一的 `bufferSize` 管理读写内存，Writer 超限时 slowdown/block
   - SlayerFS: 读写 buffer 独立管理 (300MB read + 独立 write buffer_usage)，无统一协调

3. **Auto-flush 策略**
   - SlayerFS: 500ms max_age 激进 freeze（减少 S3 小对象但增加 flush 频率）
   - JuiceFS: 5s duration 更宽松（积累更大 slice 减少 PUT 次数）

4. **Slice 复用**
   - JuiceFS: `findWritableSlice()` 可复用已有 slice（append-only 写入不创建新 slice）
   - SlayerFS: 每次 freeze 后必须创建新 slice

---

## 3. VFS Reader 对比

| 维度 | SlayerFS | JuiceFS |
|------|----------|---------|
| Readahead sessions | 2 per handle | 2 per file |
| 初始 readahead | 4 MiB (prefetch_initial) | 从 0 增长 |
| 最大 readahead | 128 MiB (prefetch_max) | `min(Readahead, 256MiB)` |
| Prefetch 并发 | 32 | `Prefetch` workers (默认小) |
| Buffer 总限 | 300 MiB (独立) | 256 MiB 或 80% buffer (与 writer 共享) |
| 全局节流 | 无 | `readBufferUsed` 全局限制 |
| Cache 层级 | block cache (内存) → S3 | page cache (内存) → disk cache → S3 |
| SingleFlight | ✅ (coalesce 并发 block GET) | ✅ (coalesce 并发 key GET) |

### 关键差异

1. **Prefetch 策略**
   - SlayerFS: 更激进的预取（初始 4MB，最大 128MB，32 并发）
   - JuiceFS: 保守起步，根据访问模式渐进增长，受全局 buffer 压力限制

2. **Disk Cache**
   - JuiceFS: 完善的磁盘缓存层 (LRU/2-random eviction，staging，容量管理)
   - SlayerFS: 仅内存 block cache (1GB)，无持久化读缓存

3. **全局内存协调**
   - JuiceFS: 读写共享 buffer pool，读 readahead 在写 flush 繁忙时自动降低
   - SlayerFS: 读写独立 buffer，无跨子系统协调

---

## 4. Chunk 存储层对比

| 维度 | SlayerFS | JuiceFS |
|------|----------|---------|
| Chunk 大小 | 64 MiB | 64 MiB |
| Block 大小 | **4 MiB** | **4 MiB** (可配置) |
| Page 大小 | 64 KiB | 64 KiB |
| Object 键格式 | `chunks/{slice_id}/{block_index}` | `chunks/{chunk_id}/{subchunk}/{id}_{blockIdx}_{size}` |
| 上传并发限 | UPLOAD_SEM = 256 (全局) | MaxUpload (配置, 默认较小) |
| 下载并发限 | 无全局限制 (SingleFlight 去重) | MaxDownload (配置) |
| 带宽限制 | ❌ 无 | ✅ UploadLimit/DownloadLimit (token bucket) |
| Write-through | ✅ 上传后填充 block_cache | 依赖 disk cache |
| 压缩 | ❌ | ✅ (可配置 lz4/zstd) |

### 关键差异

1. **Block 到对象的映射**
   - SlayerFS: 每个 block 一个独立 S3 对象 (`slice_id/block_index`)
   - JuiceFS: 类似但键格式更复杂，支持子 chunk 分层

2. **并发控制**
   - SlayerFS: 全局 256 permit semaphore，简单但不分优先级
   - JuiceFS: 上传/下载分开控制 + 带宽限流，更精细

3. **数据压缩**
   - JuiceFS: 支持 lz4/zstd 压缩，可减少 40-60% 网络传输
   - SlayerFS: 无压缩，4MB block 直接上传

4. **Compaction**
   - JuiceFS: VFS 层 `Compact()`，等待内存余量后按 block 串行重写
   - SlayerFS: 后台 `Compactor`，upload_permit() 共享带宽，支持 light/heavy 两种模式

---

## 5. 总结：SlayerFS 的优势与不足

### 优势
- Rust async/io_uring 潜力，更低的系统调用开销
- Pipeline upload (新): 同一 slice 内多 block 并发，比 JuiceFS 粒度更细
- 更高 FUSE 并发 (max_background=512 vs 50)
- Write-through cache 保证读写路径 cache 一致

### 不足（相对 JuiceFS）
1. **无磁盘读缓存** — 冷读必须从 S3 获取，JuiceFS 有完善的 disk cache + eviction
2. **无数据压缩** — 每个 4MB block 全量上传，带宽浪费
3. **无全局内存协调** — 读写 buffer 各自为政，极端负载下可能 OOM
4. **无带宽限流** — 高并发时可能打满网络影响其他服务
5. **Auto-flush 过于激进** — 500ms freeze 在小文件场景增加 S3 PUT 数量
6. **Read-before-write 未 flush** — 不 flush pending writes 可能导致特定一致性问题

---

## 6. 性能提升路线图

见 [performance-roadmap.md](./performance-roadmap.md)
