# SlayerFS 读写放大问题分析与修改规划

## 一、JuiceFS 方案逐条对照

| JuiceFS 机制 | SlayerFS 现状 | 差距 |
|---|---|---|
| 按范围读取，不整块硬拉 | `store.rs:337` 对 ≤1MB 的读做 range read | **小范围读取不缓存**，重复小读每次都打对象存储 |
| 本地读缓存 | ChunksCache（块级）+ FileReader slice cache + LRU cache | **缺少子块级（page）读缓存** |
| 顺序预读/随机不预读 | GlobalPrefetcher + session tracking | 已实现，较完善 |
| 内核页缓存 | FUSE 自动提供 | 无差距 |
| 小写追加 slice，不改整块 | COW write_fresh_vectored | 已实现 |
| 数据/元数据分离 | ObjectBlockStore + MetaLayer | 已实现 |
| 写缓冲再上传 | Page cache + auto_flush + writeback cache | 已实现 |
| 后台 compaction | Light + Heavy compaction worker | **compaction 间隔过长（3600s），碎片累积** |
| Slice/extent 元数据解耦 | SliceDesc + Span + ChunkLayout | 已实现 |

## 二、核心问题定位

### 问题 1（最关键）：小范围读取完全不缓存

**位置**: `project/slayerfs/src/chunk/store.rs:337-348`

```rust
if len <= range_size_threshold {
    // Small range read — fetch only the requested range, don't cache
    // partial blocks (not worth the complexity).
    self.client.get_object_range(&key_str, offset, buf).await?;
    return Ok(());
}
```

**根因**: 代码明确写了 "don't cache partial blocks (not worth the complexity)"。对于 4KB 随机读，每次都是独立的 range GET 到 S3，延迟 ~10-30ms/次。1000 次 4KB 随机读 = 1000 次 S3 round-trip。

**放大倍数**: 无缓存时，每次 4KB 读 = 1 次网络 IO。如果有 page cache，后续读同一 64KB page = 0 次网络 IO。对热点随机读，放大倍数可视为无穷大（本来该命中缓存的全都穿透了）。

### 问题 2：读缓存粒度太粗

**位置**: `project/slayerfs/src/chunk/cache.rs` ChunksCache

- ChunksCache 以完整 4MB block 为缓存单位
- 小范围读取直接跳过缓存
- FileReader 的 slice cache 是 chunk 级别（64MB），粒度更粗

**根因**: 缺少一个 64KB（page）粒度的读缓存层。对象存储返回的 range read 结果直接被丢弃。

### 问题 3：write_range() RMW 路径存在

**位置**: `project/slayerfs/src/chunk/store.rs:233-254`

```rust
async fn write_range(&self, key: BlockKey, offset: u64, data: &[u8]) -> Result<u64> {
    let mut buf = self.client.get_object(&key_str).await?;  // 读整个对象
    buf[start..end].copy_from_slice(data);
    self.client.put_object(&key_str, &buf).await?;           // 写整个对象
}
```

**根因**: 完整的 read-modify-write。如果被调用（需要确认），4KB 写入会触发 4MB 读取 + 4MB 写入。

### 问题 4：中等大小读仍然整块拉取

**位置**: `project/slayerfs/src/chunk/store.rs:350-382`

阈值是 25% × 4MB = 1MB。读 1.1MB → 取整块 4MB（3.6x 放大）。这个阈值偏保守。

### 问题 5：碎片累积导致读路径膨胀

每个小写产生新 slice。compaction 间隔 3600s，在高频小写场景下，一个 chunk 可能有几十个重叠 slice。读时 `DataFetcher::read_at()` 需要遍历所有 slice 做 interval cut，且可能跨多个 block 对象读取。

## 三、改造方案（按优先级排序）

### P0：实现 Page 粒度读缓存（解决读放大核心）

**目标**: 让 4KB 随机读第二次命中时走本地缓存，不再打对象存储。

**方案**:

1. **新增 `ReadPageCache` 结构**（新文件 `src/chunk/page_cache.rs` 或放在 `src/vfs/cache/` 下）

   - 以 `(chunk_id, block_index, page_index)` 为 key
   - page_size = 64KB（与现有 `DEFAULT_PAGE_SIZE` 一致）
   - 每个 4MB block = 64 个 page
   - 使用 moka 的 future::Cache，容量可配置（默认 256MB）
   - TTL 30s，TTI 10s（适合随机读热点）

2. **修改 `ObjectBlockStore::read_range()`**（`store.rs:337-348`）

   小范围读取不再丢弃结果，而是：
   ```
   1. 计算读取范围覆盖哪些 page
   2. 对每个 page：
      a. 先查 ReadPageCache
      b. 未命中 → range GET 该 page（对齐到 page 边界）
      c. 写入 ReadPageCache
      d. 从 page 中 copy 所需字节到 buf
   ```

3. **调整 range_read_threshold**

   将默认值从 0.25 提高到 0.5（即 2MB），减少不必要的全块获取。

**影响**:
- 4KB 随机读：第一次 ~15ms，后续命中缓存 ~0.01ms（1000x 改善）
- 增加 ~256MB 内存占用
- page 对齐读取可能导致单次读取量略增（读 64KB 对齐窗口），但换来缓存命中

**改动文件**:
- 新建 `src/chunk/page_cache.rs`
- 修改 `src/chunk/store.rs`（ObjectBlockStore::read_range，BlockStoreConfig）
- 修改 `src/chunk/mod.rs`（注册新模块）
- 修改 `src/vfs/fs/mod.rs`（VFSState::new 中初始化 page cache）

---

### P1：移除 write_range RMW 路径 ✅ 已完成

**结果**: `write_range()` 在生产代码中无调用者。所有写入都通过 `DataUploader::write_fresh_vectored()` → `ObjectBlockStore::write_fresh_vectored()`（COW）。

**已执行改动**:
- `src/chunk/store.rs`: 从 `BlockStore` trait 中删除 `write_range()`，`write_fresh_range` 从默认方法变为必需方法
- `src/chunk/store.rs`: 完全删除 `ObjectBlockStore::write_range()`（GET+PUT 的 RMW 实现）
- `src/chunk/store.rs`: `InMemoryBlockStore::write_range` → `write_fresh_range`（重命名，逻辑不变）
- `src/vfs/io/writer.rs`: `BlockingStore` 和 `FailingStore` test helpers 同步更新
- 所有测试调用点从 `.write_range()` 改为 `.write_fresh_range()`

---

### P2：优化 compaction 策略 ✅ 已完成

**已执行改动**:
- `src/chunk/compact/worker.rs`: 扫描间隔 3600s → 600s（10 分钟）
- `src/meta/config.rs`:
  - `light_threshold`: 3 → 2（更快触发轻量 compact）
  - `heavy_slice_threshold`: 50 → 30（更早触发重 compact）
  - `min_slice_count`: 5 → 3
  - `sync_threshold`: 350 → 200
  - `interval`: 3600s → 600s（与 worker 一致）
- `VfsBackgroundConfig::from_compact_config()` 自动使用新值（从 `CompactConfig::interval` 读取）

注：P2.3（commit 后即时 compact）和 P2.4（自适应阈值）比较激进，留给后续迭代。

---

### P3：细粒度块读取策略优化

**目标**: 对中等大小读（1MB-4MB），改为按需读取所需 block，而非整块拉取。

**方案**:

1. 在 `DataFetcher::read_at()` 层面，已经按 `block_span_iter_slice` 逐个 block 读取
2. 在 `ObjectBlockStore::read_range()` 中，当 len 在 1MB-4MB 之间时：
   - 拆分为多个 ≤1MB 的 sub-range read
   - 每个 sub-range 走 page cache 路径
   - 这样可以避免取回不需要的 block 尾部数据

**改动文件**:
- `src/chunk/store.rs`（read_range 内部策略）

---

### P4：读缓存层级整合

**目标**: 统一 ChunksCache、ReadPageCache、FileReader slice cache 三层，避免重复缓存和一致性问题。

**方案**:

1. ChunksCache 保留作为 L2（disk-backed cold cache），但降低容量
2. ReadPageCache 作为 L1（memory hot cache）
3. FileReader slice cache 作为 L0（per-handle，最热）
4. 查询顺序: L0 → L1 → L2 → object storage

**改动文件**:
- `src/chunk/cache.rs`
- `src/chunk/page_cache.rs`（新建）
- `src/vfs/io/reader.rs`

---

## 四、实施顺序与依赖关系

```
Phase 1 (P0): Page 粒度读缓存          ← 最大收益，无依赖
Phase 2 (P1): 移除 RMW write_range      ← 独立，小改动
Phase 3 (P2): Compaction 策略优化       ← 独立，可并行
Phase 4 (P3): 中等读粒度优化            ← 依赖 P0 的 page cache 基础设施
Phase 5 (P4): 缓存层级整合              ← 依赖 P0，可后续做
```

建议 Phase 1 + Phase 2 并行开发，Phase 3 可同时进行。

## 五、预期收益估算

| 场景 | 当前行为 | 改造后 | 改善 |
|---|---|---|---|
| 4KB 随机读（重复） | 每次 S3 range GET (~15ms) | 首次 S3，后续内存缓存 (~0.01ms) | ~1000x 延迟 |
| 4KB 随机读（不重复） | S3 range GET | S3 range GET（page 对齐，略增读量） | 持平或略差（page padding） |
| 1.5MB 顺序读 | 取整块 4MB（2.7x 放大） | 按 page 取所需范围 | ~2.7x 读量减少 |
| 4KB 随机写 | COW append（无放大） | 不变 | 持平 |
| 碎片化 chunk 读 | N 个 slice × M 个 block IO | compaction 后 1 个 slice × 少 block | 取决于碎片率 |
| 大文件顺序读 | prefetch + full block cache | 不变 | 持平 |

## 六、风险与注意事项

1. **Page padding 开销**: 4KB 读对齐到 64KB page 后，单次读量增加 16x。但由于缓存命中，后续读不再产生网络 IO，总体网络流量会大幅下降（热点场景）。
2. **内存占用**: Page cache 默认 256MB，对低内存环境可能需要调低。
3. **缓存一致性**: page cache 需要感知 block 是否已被 compaction 替换。因为 block 是 immutable 的（COW），不存在一致性问题 —— 一旦写入就不会变。compaction 产生新 block，旧 block 的 page cache 自然过期（TTL 处理）。
4. **write_range 移除风险**: 需先穷举调用点，确认无生产路径依赖。

## 七、里程碑

- **M1** (Phase 1 完成): 随机读延迟显著下降，通过 `fio` randread 验证
- **M2** (Phase 1+2 完成): 写路径无 RMW 放大
- **M3** (Phase 1+2+3 完成): 长时间运行碎片率可控
- **M4** (全阶段完成): 读放大接近理论下限（只读需要的字节）
