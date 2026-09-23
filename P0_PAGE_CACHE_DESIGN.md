# P0: Page 粒度读缓存 — 详细设计

## 问题

`store.rs:337-348` 小范围读取（≤1MB）通过 `get_object_range` 直接读取后**丢弃结果不缓存**。
代码注释："not worth the complexity"。后果：重复 4KB 随机读每次都打 S3，~15ms/次。

## 设计

### 新增 `ReadPageCache`（`src/chunk/page_cache.rs`）

```
┌────────────────────────────────────────────────────┐
│                  ReadPageCache                      │
│  moka::future::Cache<PageKey, Bytes>               │
│  page_size: 64KB                                   │
│  capacity: 4096 pages (256MB)                       │
│  TTL: 120s, TTI: 30s                               │
├────────────────────────────────────────────────────┤
│  PageKey = (slice_id: u64, block_idx: u32,         │
│             page_idx: u32)                         │
│  ──────── 64KB ────────                            │
│  │ page 0 │ page 1 │ ... │ page 63 │              │
│  └────────┴────────┴─────┴─────────┘              │
│  ◄────────── 4MB block ──────────►                │
└────────────────────────────────────────────────────┘
```

### 读取流程变化（`ObjectBlockStore::read_range`）

```
read_range(key, offset, buf)
  │
  ├─ 1. 查 ChunksCache（全块缓存，不变）
  │     hit → copy from cached, return
  │
  ├─ 2. len ≤ range_size_threshold（1MB）？
  │     YES →
  │       for each page in [start_page..end_page]:
  │         a. 查 ReadPageCache
  │         b. miss → get_object_range(page_aligned_offset, page_size)
  │         c. 写入 ReadPageCache
  │         d. copy 所需字节到 buf
  │       return
  │
  └─ 3. 大读：SingleFlight + 全块缓存（不变）
```

### Page 对齐示例

```
读 offset=60KB, len=8KB:
  page 0 [0, 64KB): 取 [0, 64KB), 缓存, copy [60KB, 64KB) → 4KB
  page 1 [64KB, 128KB): 取 [64KB, 128KB), 缓存, copy [0, 4KB) → 4KB
  = 8KB 读出, 128KB 缓存写入

读 offset=4KB, len=4KB (单 page):
  page 0 [0, 64KB): 取 [0, 64KB), 缓存, copy [4KB, 8KB) → 4KB
  = 4KB 读出, 64KB 缓存写入
```

**Page padding 开销**：单次 4KB 读 → 实际取 64KB（16x 放大）。
**但**后续读同一 64KB page 内的任何偏移直接命中缓存，总网络流量在热点场景大幅下降。

### 配置

BlockStoreConfig 新增字段：

```rust
pub struct BlockStoreConfig {
    pub block_size: usize,           // 不变: 4MB
    pub range_read_threshold: f32,   // 不变: 0.25
    pub page_size: usize,            // 新增: 64KB
    pub page_cache_capacity: usize,  // 新增: 4096 pages
}
```

### 缓存一致性

Block 是 COW immutable。一旦写入不会变。Compaction 产生新 block/slice_id，
旧 block 的 page cache 通过 TTL 自然过期。无一致性问题。

### 与现有缓存层的关系

```
L0: FileReader slice cache  (per-handle, chunk granularity, 最热)
L1: ReadPageCache           (process-wide, page granularity, 64KB)
L2: ChunksCache             (process-wide, block granularity, 4MB, disk-backed)
L3: Object Storage          (S3/local)
```

查询顺序：L1 → L2（不存在则跳过 L0，因为 L0 已由 FileReader 管理）。
L0 和 L1 可以共存——L0 整 slice 预取，L1 拦截碎片化随机读。

## 改动文件

| 文件 | 改动 |
|---|---|
| `src/chunk/page_cache.rs` | **新建** - ReadPageCache 结构体 |
| `src/chunk/store.rs` | 集成 page cache 到 read_range() + BlockStoreConfig 扩展 |
| `src/chunk/mod.rs` | 注册 page_cache 模块 |
| `src/vfs/fs/mod.rs` | VFSState::new 中传递 page cache 配置（可选，先使用默认值） |

## 预期效果

| 指标 | 改前 | 改后 |
|---|---|---|
| 4KB 随机读（首次） | ~15ms (S3 range GET) | ~15ms（page 对齐，略增读量） |
| 4KB 随机读（复用同 page） | ~15ms (无缓存) | ~0.01ms（L1 命中） |
| 64KB 范围读（跨 2 page） | ~15ms | ~15ms 首读，后续 ~0.01ms |
| 内存开销 | 0 | ≤256MB（可配置） |
