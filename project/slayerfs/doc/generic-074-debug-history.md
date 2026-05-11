# generic/074 调试与修复全记录

## 1. 文档范围

本文档只记录这一轮 `xfstests generic/074` 调试过程中我实际做过、并且与问题定位和修复直接相关的修改。

不包含工作区中其它并行开发项，也不把无关改动混入本次问题分析。

最终结果：

```text
generic/074 Passed all 1 tests
artifacts/run-1777738548-15565
```

---

## 2. 问题背景

`generic/074` 主要覆盖 `fstest` 的几类场景：

1. 普通写入与校验
2. 单进程 mmap 写入与校验
3. 多进程普通写入与校验
4. 多进程 mmap 写入与校验
5. 多进程 mmap + sync 写入与校验

本轮 SlayerFS 的实际问题演化是：

1. 最开始卡在 `fstest.2`
2. 修掉卡死后，推进到 `fstest.3`
3. `fstest.3` 持续出现 mmap 写入后的读零数据损坏
4. 最后通过 `FUSE_WRITE_CACHE` 可见性修复，整个 `generic/074` 通过

---

## 3. 修复总览

这一轮真正解决问题的修改可以归为四类：

1. 调试基础设施：把日志拆开，并修正容器 mount helper 的环境变量丢失问题
2. `fstest.2` 卡死修复：避免在 `truncate/setattr(size)` 路径里持锁等待长时间 flush
3. 属性正确性修复：让 `size` / `blocks` 在 writeback-cache 与 sparse file 场景下更符合内核预期
4. `fstest.3` mmap 可见性修复：修正 userspace writer 异步提交与 `FUSE_WRITE_CACHE` 返回时机之间的竞态

---

## 4. Bug 点 1：FUSE 操作日志最初并没有真正落盘

### 现象

一开始需要靠 FUSE op log 分析 `generic/074`，但容器跑完后只有主日志，没有拿到预期的 FUSE 操作日志，导致无法判断 `WRITE`、`READ`、`FLUSH` 的实际顺序。

### 根因

`docker/compose-xfstests/run_xfstests_in_container.sh` 在生成 mount helper 时使用了会阻止变量展开的 heredoc 写法，导致 `SLAYERFS_FUSE_LOG_FILE` 在 helper 脚本里没有被正确带入。

结果是：

1. mount helper 实际启动时丢失了 FUSE 日志路径
2. `SLAYERFS_FUSE_OP_LOG=1` 虽然看起来设置了，但日志文件并没有按预期写入到 artifact 目录

### 修改

做了两层修复：

1. 在 `src/main.rs` 中增加分离日志能力：
   - 主日志走 `SLAYERFS_LOG_FILE`
   - FUSE op 日志走 `SLAYERFS_FUSE_LOG_FILE`
2. 在 `docker/compose-xfstests/run_xfstests_in_container.sh` 中把运行时确定的路径直接 baked 进 helper，避免 mount 时环境被清空或丢失

### 相关文件

1. `src/main.rs`
2. `docker/compose-xfstests/run_xfstests_in_container.sh`
3. `docker/compose-xfstests/docker-compose.redis.yml`
4. `docker/compose-xfstests/docker-compose.etcd.yml`
5. `docker/compose-xfstests/docker-compose.sqlite.yml`
6. `docker/compose-xfstests/docker-compose.redis-perf.yml`
7. `docker/compose-xfstests/docker-compose.etcd-perf.yml`

### 结果

后续 run 成功拿到了 `slayerfs_fuse_ops.log`，才得以继续分析 `WRITE_CACHE`、`READ`、`FLUSH` 的相对顺序。

---

## 5. Bug 点 2：`fstest.2` 卡死，根因是 truncate/setattr(size) 持锁等待 flush

### 现象

`generic/074` 最早不是数据损坏，而是卡死在 `fstest.2`。目录和文件已经创建出来了，但测试长时间不前进，最后被 xfstests 超时杀掉。

### 根因

核心问题在 `src/vfs/fs/mod.rs` 的 `truncate_inode()` 和 `set_attr()` 的 size 修改路径：

1. 先拿 inode 级别互斥
2. 再调用 `flush_required()`
3. `flush_required()` 可能等待很久，因为它会等 writer upload/commit 排空

而此时：

1. FUSE 后续的 `WRITE` 还在继续进来
2. 这些 `WRITE` 又需要同一个 inode 的互斥锁
3. 于是形成长时间互相等待

这在 `write_back=true` 的 FUSE writeback-cache 模式下尤其容易把内核 writeback 一起拖住，最终表现为 `pwrite` 长时间阻塞。

### 修改

把控制顺序改成：

1. 先 `flush_required()`
2. 再获取 inode 级别锁
3. 再执行 truncate / setattr(size)
4. 获锁后调用 `writer.clear()`，清掉 pre-flush 与加锁之间新落进来的脏 slice

这个修改同时做在：

1. `truncate_inode()`
2. `set_attr()` 的 `req.size.is_some()` 分支

### 相关文件

1. `src/vfs/fs/mod.rs`

### 结果

修复后：

1. `fstest.2` 不再卡死
2. `generic/074` 开始稳定推进到 `fstest.3`

这说明最初的 hang 主因已经被清掉。

---

## 6. Bug 点 3：`st_blocks` 以前按逻辑大小算，稀疏文件会报错

### 现象

在 sparse file / hole 场景里，`stat(2)` 看到的 `st_blocks` 不应该简单按 `size / 512` 推出来，因为逻辑大小和真实已提交数据量并不相同。

如果继续按逻辑大小算：

1. 带 hole 的文件会显得“占用太多 blocks”
2. mmap / truncate / sparse 校验类测试更容易把 SlayerFS 识别成语义错误

### 根因

FUSE 回复属性时原先直接按：

```text
blocks = size.div_ceil(512)
```

这对密集文件勉强成立，对 sparse file 明显不成立。

### 修改

最终采用的是“只在 VFS/FUSE 视图层推导 blocks，而不是把 blocks 存进持久化 attr”这条设计：

1. `FileAttr` 不新增持久化 `blocks` 字段
2. `src/vfs/inode.rs` 增加 `committed_bytes`
3. `commit_chunk` 成功后累加 committed bytes
4. truncate 时重置 committed bytes
5. `VFS::blocks_for_attr()` 用 committed bytes 计算 blocks
6. `src/fuse/mod.rs` 的 `vfs_to_fuse_attr()` 改为显式接收 blocks 参数

### 相关文件

1. `src/vfs/inode.rs`
2. `src/vfs/io/writer.rs`
3. `src/vfs/fs/mod.rs`
4. `src/fuse/mod.rs`

### 结果

`st_blocks` 从“逻辑大小近似值”改成“已提交数据量近似值”，避免把 sparse file 错误描述成 fully allocated file。

---

## 7. Bug 点 4：非 size 的 setattr 可能把内核看到的文件大小回退成旧值

### 现象

在 writeback-cache + mmap 路径里，内核可能发出只更新时间戳的 `setattr(size=None)`。如果这时 SlayerFS 回复的 attr.size 还是元数据层旧值，而不是本地已扩展的新值，会让内核误以为文件大小退回了旧状态。

这类问题会放大 mmap 读零或 page cache 错乱的风险。

### 根因

`set_attr()` 处理非 size 请求时，之前直接返回 meta 层 attr，而 meta 层 size 未必已经追上本地 writer 的最新扩展结果。

### 修改

在 `src/vfs/fs/mod.rs` 中增加逻辑：

1. 如果 `req.size.is_none()`
2. 且本地 inode cache 里有更大的 size
3. 则用本地 size 覆盖返回 attr.size

### 相关文件

1. `src/vfs/fs/mod.rs`

### 结果

这个修改是一次重要的 correctness hardening。后续日志确认：`setattr(size=None)` 的回复 size 已不再退回到 0。

它不是最后解决 `fstest.3` 的唯一根因，但属于必须保留的正确性修复。

---

## 8. Bug 点 5：读路径原先靠“先 flush 再 read”，但对 mmap/writeback 并不可靠

### 现象

在 `fstest.3` 里，mmap 写入之后的读校验持续读到 `00 00 00 ...`，而不是预期的数据模式。最典型的损坏是：

1. `file0`
2. 小 offset，比如 `4096` 或 `32768`
3. 预期是某个固定字节值重复
4. 实际全是零

### 根因

原读路径在 `src/vfs/fs/mod.rs` 里是：

1. 读之前尝试 `flush_if_exists()`
2. 然后直接 `handle.read()`

这条路径有两个问题：

1. flush 是 best-effort，不是强保证
2. 即便 writer 内部仍然持有比底层持久层更新的数据，读路径也没有把这些 dirty slices 覆盖到 read buffer 上

### 修改

把读路径改成：

1. 先执行底层 `handle.read()`
2. 再调用 `writer.overlay_dirty_if_exists()`，把尚未完全通过正常读路径可见的 dirty data 盖回到结果 buffer

同时新增：

1. `CacheSlice::copy_into()`
2. `Page::copy_slice()`
3. `FileWriter::overlay_dirty()`
4. `FileWriters::overlay_dirty_if_exists()`

### 相关文件

1. `src/vfs/fs/mod.rs`
2. `src/vfs/io/writer.rs`
3. `src/vfs/cache/page.rs`

### 结果

这一步把问题从“完全看不到 writer 内存态”推进成“能看到一部分 dirty view，但仍然存在特定窗口下的零读”。

也就是说，它是必要修复，但还不够。

---

## 9. Bug 点 6：`overlay_dirty` 最初忽略了 `Uploaded/Committed` 切片

### 现象

即使已经做了读路径 overlay，`fstest.3` 仍然失败。进一步排查发现，writer 中并不是只有 `Writable/Readonly` 切片，很多 mmap 写回切片已经推进到了：

1. `Uploaded`
2. `Committed`

但它们在真正从 chunk slice 队列里移除前，仍然可能代表“比底层读路径更新的数据”。

### 根因

`src/vfs/io/writer.rs` 中 `can_overlay_read()` 原先只允许：

1. `Writable`
2. `Readonly`
3. `Failed`

这意味着一旦切片推进到 `Uploaded/Committed`，读覆盖逻辑就会把它们忽略掉。

### 修改

把可参与读覆盖的状态扩展为：

1. `Writable`
2. `Readonly`
3. `Uploaded`
4. `Failed`
5. `Committed`

### 相关文件

1. `src/vfs/io/writer.rs`

### 结果

这一步修复了一个清晰的状态判断错误，但仍没有完全消除 `fstest.3` 的 mmap 零读。

说明问题不只是“状态不参与 overlay”，还有更深一层的时序问题。

---

## 10. Bug 点 7：uploaded page 在 metadata commit 前被提前释放，导致 overlay 读到的仍是零

### 现象

继续往下挖后发现，即使切片状态允许 overlay，也不代表切片里还保留着真正的数据页。

### 根因

`src/vfs/io/writer.rs` 的 `advance_upload()` 在 upload 成功后会立刻：

1. 增加 `uploaded` 偏移
2. 调用 `release_block()` 释放已上传 block 的 page
3. 而 metadata commit 还没完成

于是出现一个危险窗口：

1. upload 已成功
2. metadata 还没 commit
3. read 想靠 overlay 看到最新数据
4. 但内存页已经提前释放
5. 结果 overlay 拷出来的又是零页

### 修改

把这一步改成“延后释放”：

1. upload 成功后不再提前 `release_block()`
2. 把 page 保留到 commit 后 slice 被真正 pop/remove 为止

### 相关文件

1. `src/vfs/io/writer.rs`

### 结果

这一步清掉了 “uploaded but not yet committed” 窗口里的零读问题，但 `fstest.3` 仍然还有最后一个竞态没有解决。

---

## 11. Bug 点 8：最终根因，`FUSE_WRITE_CACHE` 返回成功时数据对 close 后重开读仍不可见

### 现象

这是最后真正打穿 `generic/074` 的根因。

结合 `fstest.c` 源码，`fstest.3` 的关键顺序是：

1. `open(O_RDWR|O_TRUNC)`
2. `ftruncate(file_size)`
3. `mmap(MAP_SHARED)`
4. 直接写映射内存
5. `munmap()`
6. `close(fd)`
7. 重新 `open(O_RDONLY)`
8. `pread()` 校验所有块

所以最终失败点已经被缩小成：

```text
close(fd) 返回以后，下一次 open + pread 仍然可能看到旧零块
```

### 根因

`FUSE_WRITE_CACHE` 请求在 SlayerFS 里之前是这样处理的：

1. 落进 userspace writer buffer
2. 立即给 FUSE `ReplyWrite { written: ... }`
3. 实际 upload / commit 继续异步进行

这就留下了一个最后的可见性竞态：

1. 内核已经认为 cached page writeback 成功了
2. `close(fd)` 返回
3. 测试马上 `open + pread`
4. 但 SlayerFS userspace 里的 writer 还没把数据真正 flush/commit 到可读路径
5. 于是读回旧的零块

这也是为什么前面只靠 overlay 修修补补还不够，因为这里已经进入了“close 后重开读”的阶段。

### 修改

在 `src/vfs/fs/mod.rs` 中，把 `write_cached_ino()` 改成：

1. 先执行 `write_ino_inner()`
2. 再立刻 `writer.flush_required(ino)`
3. 只有 flush/commit 真正完成后，才向 FUSE 返回 cached write 成功

也就是把 `FUSE_WRITE_CACHE` 从“异步可见”改成“返回成功前同步可见”。

### 相关文件

1. `src/vfs/fs/mod.rs`
2. `src/fuse/mod.rs`

### 结果

这是最终让 `generic/074` 通过的决定性修复。

验证结果：

```text
generic/074 656s
Passed all 1 tests
```

artifact：

```text
docker/compose-xfstests/artifacts/run-1777738548-15565
```

---

## 12. Bug 点 9：FUSE op log 默认开启会严重放大排查成本与产物体积

### 现象

在问题已经定位完成后，继续默认开启 FUSE op log 会带来两个副作用：

1. 日志体积非常大
2. 后续每次回归验证都更慢、更难读

### 根因

`run_xfstests_in_container.sh` 曾经是默认在 helper 启动时直接硬编码：

```text
SLAYERFS_FUSE_OP_LOG=1
```

这意味着后续每次 xfstests run 都会开 FUSE op log，即使只是做回归验证。

### 修改

把策略改成：

1. 默认关闭
2. 只有显式设置 `SLAYERFS_FUSE_OP_LOG=1|true|yes|on` 才开启
3. compose 文件保留开关透传，方便以后需要时重新抓日志

### 相关文件

1. `docker/compose-xfstests/run_xfstests_in_container.sh`
2. `docker/compose-xfstests/docker-compose.redis.yml`
3. `docker/compose-xfstests/docker-compose.etcd.yml`
4. `docker/compose-xfstests/docker-compose.sqlite.yml`
5. `docker/compose-xfstests/docker-compose.redis-perf.yml`
6. `docker/compose-xfstests/docker-compose.etcd-perf.yml`

### 结果

后续回归跑 `generic/074` 时默认只保留主日志，必要时再显式打开 FUSE op log。

---

## 13. 其它与本次问题相关的辅助修复

这些修改不是最终单点根因，但属于同一调试链路中必要的正确性补强：

### 13.1 `release` 路径改为在 `_flush=true` 时先显式 flush 再 close

目的：

1. 减少 close/release 语义不一致
2. 让 FUSE 释放路径更贴近内核预期

相关文件：

1. `src/fuse/mod.rs`

### 13.2 `flush_and_sync_handle()` 不再只依赖 handle 是否可写

目的：

1. mmap 写回可以通过共享 writer 落在 inode 上
2. 即使当前 handle 不是 write handle，`fsync/flush` 也应该能把共享 writer 排空

相关文件：

1. `src/vfs/fs/mod.rs`

### 13.3 `HandleWriteGate` 去掉“每次 write 后强制同步 flush”

目的：

1. 避免每个 write 都走完整 upload/commit
2. 降低 writer 状态机被小写放大的概率

相关文件：

1. `src/vfs/handles.rs`

### 13.4 `commit_chunk` 增加 upload re-kick

目的：

1. 避免 frozen slice 存在但 uploader 没继续推进时，commit 一直空等
2. 这是先前 `generic/013 --s3` hang 分析里也验证过的重要修复点

相关文件：

1. `src/vfs/io/writer.rs`

---

## 14. 这轮实际修改过的关键文件

与 `generic/074` 调试和修复直接相关、并且在这一轮被改动的核心文件如下：

1. `src/main.rs`
2. `src/fuse/mod.rs`
3. `src/vfs/fs/mod.rs`
4. `src/vfs/io/writer.rs`
5. `src/vfs/cache/page.rs`
6. `src/vfs/inode.rs`
7. `src/vfs/handles.rs`
8. `docker/compose-xfstests/run_xfstests_in_container.sh`
9. `docker/compose-xfstests/docker-compose.redis.yml`
10. `docker/compose-xfstests/docker-compose.etcd.yml`
11. `docker/compose-xfstests/docker-compose.sqlite.yml`
12. `docker/compose-xfstests/docker-compose.redis-perf.yml`
13. `docker/compose-xfstests/docker-compose.etcd-perf.yml`

---

## 15. 最终结论

这轮 `generic/074` 的问题并不是单一 bug，而是一串彼此叠加的问题链：

1. 先是调试信息拿不到
2. 再是 `fstest.2` 因为 truncate/flush 锁顺序问题卡死
3. 推进到 `fstest.3` 后，又暴露出 mmap/writeback-cache 可见性问题
4. 中间还夹杂 `size` / `blocks` / dirty overlay / uploaded page 生命周期等 correctness 细节
5. 最后真正决定测试成败的根因，是 `FUSE_WRITE_CACHE` 在返回成功时数据还停留在 userspace 异步 writer 中，导致 close 后重开读与 commit 可见性之间存在竞态

最终修复思路可以概括为一句话：

```text
把 mmap/writeback-cache 路径上的“异步最终一致”收紧成 close 后读所需要的“同步可见”
```

也正是这个收口，最终让 `generic/074` 从：

1. hang
2. `fstest.3` 读零损坏
3. 输出不匹配

推进到了完全通过。
