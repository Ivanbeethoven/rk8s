# SlayerFS Redis 元数据后端与 FUSE 接口风险评估

_Date: 2026-04-13_

## 1. 背景与目的

本文整理 SlayerFS 在 Redis 元数据后端场景下，FUSE 请求经由 VFS 进入 MetaStore 的关键路径，并基于静态代码阅读评估潜在错误。目标不是复述实现，而是回答三个问题：

1. 当前 Redis 元数据语义是否与 VFS/FUSE 暴露的 POSIX 语义一致。
2. 在并发、覆盖式重命名、硬链接和符号链接场景下，是否存在会直接影响正确性的风险。
3. 哪些问题应优先修复，哪些可以先通过补测试降低回归风险。

本文基于静态审查完成，没有编译项目，也没有运行测试或挂载流程。

## 2. 评估范围

本次评估聚焦以下模块：

- `src/meta/stores/redis/mod.rs`
- `src/meta/stores/redis/tests.rs`
- `src/vfs/fs/mod.rs`
- `src/vfs/fs/tests.rs`
- `src/fuse/mod.rs`
- `src/fuse/mount.rs`
- `src/fuse/adapter.rs`

其中需要特别说明的是，`src/fuse/adapter.rs` 当前仍是占位实现，真正对外承接 FUSE 请求的是 `src/fuse/mod.rs` 中对 `rfuse3::raw::Filesystem` 的实现。因此，“Redis 的 FUSE 接口问题”本质上主要表现为：

- FUSE 层的参数校验与 errno 映射是否正确。
- VFS 层的路径语义是否满足 POSIX。
- Redis MetaStore 的 Lua 原子操作是否与上层假设一致。

## 3. 调用链与语义边界

### 3.1 典型调用链

以 `rename`、`unlink`、`readlink` 为代表，当前链路如下：

1. FUSE 层在 `src/fuse/mod.rs` 接收内核请求。
2. FUSE 层将 inode 或目录项请求转换为路径级 VFS 调用。
3. VFS 层在 `src/vfs/fs/mod.rs` 负责：
   - 路径规范化
   - 父目录解析
   - 目标是否存在的判定
   - 目录/文件/符号链接替换规则
   - 循环重命名检查
4. Redis MetaStore 在 `src/meta/stores/redis/mod.rs` 中通过 Lua 脚本完成原子元数据变更。

### 3.2 各层职责划分

#### FUSE 层

- 做基础输入校验。
- 将错误映射到 `errno`。
- 将 inode 请求转换为 VFS 可识别的路径或 inode 操作。

#### VFS 层

- 负责 POSIX 语义协调。
- 当前实现中，目标覆盖逻辑大部分在 VFS 层先完成，再调用 MetaStore 的重命名接口。
- 这意味着上层默认 MetaStore 的 `rename` 可以在“目标已被处理干净”的前提下执行。

#### Redis MetaStore

- 负责单次元数据更新的 Redis 原子性。
- 当前 `RENAME_LUA` 明确是“no overwrite”语义，而不是“覆盖式 rename”语义。
- `UNLINK_LUA` 与 `RMDIR_LUA` 分别负责删除文件/符号链接和删除空目录。

## 4. 结论摘要

### 4.1 总体结论

当前实现可以支撑一部分基本路径，但在 Redis 后端下，`rename` 和 `unlink` 的关键语义仍有明显风险。最重要的问题不是“功能缺失”，而是“上层认为语义已经满足，但 Redis 后端实际只提供了更弱的语义保证”。

### 4.2 风险分级

| 级别 | 问题 | 影响 |
|------|------|------|
| 高 | 覆盖式 `rename` 在 VFS 与 Redis MetaStore 之间不是单次原子操作 | 并发下可能误删目标，破坏 POSIX rename 的原子替换语义 |
| 高 | Redis 范围锁冲突检查与写入不是原子过程 | 并发会出现两个冲突锁同时成功，直接破坏 POSIX 记录锁语义 |
| 中 | `UNLINK_LUA` 对节点缺失或损坏采取“成功删除”语义 | 并发双删或元数据损坏时，上层可能吞掉 `ENOENT` / `EIO` |
| 中 | FUSE 层将“同路径 rename”直接返回 `EINVAL` | 与下层 VFS/MetaStore 的 no-op 语义不一致，暴露非 POSIX 行为 |
| 中 | Redis 全局锁是基于时间戳的软租约，没有 owner/fencing token | 时钟漂移、长暂停或重复获取时容易出现锁误判 |
| 低 | `adapter.rs` 仍为占位模块，真实 FUSE 行为分散在 `fuse/mod.rs` | 可维护性风险，不是当前正确性根因 |

## 5. 详细发现

## 5.1 高危问题：覆盖式 rename 缺少端到端原子性

### 现象

VFS 层当前对覆盖式重命名采用“两阶段”处理：

1. 先在 `handle_destination_replacement` 中删除目标项。
2. 再调用 `meta_rename` 执行源项重命名。

对应代码位于：

- `src/vfs/fs/mod.rs` 中的 `handle_destination_replacement`
- `src/vfs/fs/mod.rs` 中的 `execute_rename`
- `src/meta/stores/redis/mod.rs` 中的 `RENAME_LUA`

### 关键实现特征

#### VFS 侧

VFS 在处理目标已存在时，会先根据目标类型执行：

- `meta_rmdir(new_parent_ino, new_name)`
- 或 `meta_unlink(new_parent_ino, new_name)`

随后才进入：

- `self.meta_rename(old_parent_ino, old_name, new_parent_ino, new_name)`

这说明覆盖动作和 rename 动作不是一个原子事务。

#### Redis 侧

Redis `RENAME_LUA` 的设计明确要求目标不存在：

- 它先读取源目录项。
- 然后验证目标父目录存在且是目录。
- 接着执行 `HEXISTS new_parent_dir_key new_name`。
- 若目标存在，直接返回 `already_exists`。

换句话说，Redis MetaStore 自身实现的是“无覆盖重命名”，不是 POSIX 意义上的“原子覆盖重命名”。

### 风险场景

以下场景会产生可见错误：

1. 线程 A 要把 `old` rename 到已存在的 `new`。
2. 线程 A 在 VFS 层先删除 `new`。
3. 在线程 A 调用 `meta_rename` 之前，线程 B 又创建了新的 `new`。
4. Redis `RENAME_LUA` 看到目标存在，返回 `AlreadyExists`。
5. 结果是：
   - 线程 A 的 rename 失败。
   - 原来的目标 `new` 已经被线程 A 删除。
   - 用户看到的不是“原子替换失败且目标保持不变”，而是“目标被意外删除后失败”。

这与 POSIX `rename(2)` 的核心预期不一致。

### 为什么这是高危问题

- 它不是错误码不准确，而是会造成用户可见的数据语义错误。
- 它会在并发目录操作、测试框架、构建缓存目录轮换等常见场景中放大。
- 这种问题即使平时不高频触发，一旦出现就很难从日志反推出真正原因。

### 当前测试覆盖情况

已有测试覆盖了：

- 同一源目录项的并发 rename。
- `rename_exchange` 的并发场景。
- rename 目标已存在时，Redis store 直接返回 `AlreadyExists`。

但缺失的关键测试是：

- VFS 层“覆盖式 rename”在并发竞争下是否保持原子替换语义。
- Redis 后端场景下，目标先删除再 rename 的竞态是否会误删目标。

### 修复建议

建议优先级最高。

可选方案有两类：

#### 方案 A：把覆盖式 rename 下沉为单个 Redis Lua 脚本

这是最干净的修复方式。脚本应同时完成：

- 源存在性检查
- 目标类型检查
- 空目录替换判断
- 目标删除或替换
- 源项移动
- 硬链接 `link_parents` 更新
- 父目录时间戳更新

这样 VFS 只负责语义判定，不再提前拆分删除与 rename。

#### 方案 B：新增 Redis 专用 `rename_replace` 接口

如果暂时不想重写现有 `rename` 语义，可以在 MetaStore 层增加一个显式的“带替换语义”的接口，并由 VFS 在目标存在时调用该接口。

这会增加 API 面，但能降低对现有 no-overwrite `rename` 的扰动。

## 5.2 中危问题：UNLINK_LUA 会吞掉节点缺失和损坏错误

### 现象

`UNLINK_LUA` 的顺序是：

1. 先 `HDEL` 目录项。
2. 再 `SREM` 硬链接父绑定。
3. 然后才 `GET` inode 节点。
4. 如果节点不存在或 JSON 解码失败，直接返回：
   - `ok=true`
   - `nlink=0`
   - `deleted=true`

Rust 封装层收到这个结果后，会继续按成功路径走下去，必要时调用 `mark_deleted`。

### 风险场景

#### 场景 1：并发双重 unlink

如果两个请求几乎同时删除同一目录项：

- 一个请求先删掉目录项和节点。
- 另一个请求进入 `UNLINK_LUA` 时，节点可能已经不存在。
- Lua 仍然返回成功。

这样就可能把“第二次删除本应返回 `ENOENT`”的情况吞掉。

#### 场景 2：元数据损坏

如果 Redis 中 inode JSON 已损坏：

- Lua 不会返回 `corrupt_node`。
- 而是把它当成已删除处理。

这会把实际的存储损坏伪装成普通删除成功，排障成本会非常高。

### 为什么这是中危问题

- 它不一定直接导致数据丢失，但会明显削弱错误检测能力。
- 在 FUSE 层，最终用户拿到的是成功，而不是更接近现实的 `ENOENT` 或 `EIO`。
- 一旦线上出现 Redis 元数据损坏，这种吞错会掩盖真实故障。

### 修复建议

建议将 `UNLINK_LUA` 调整为更严格的语义：

- 如果目录项不存在，应返回明确的 `not_found`。
- 如果节点缺失，应区分：
  - 是并发已删导致的幂等删除。
  - 还是目录项与节点不一致的损坏状态。
- 如果 JSON 解码失败，应返回 `corrupt_node`，让上层映射为 `EIO`。

如果希望保留幂等删除语义，也应只在“目录项已不存在且节点已不存在”的可证明并发场景中返回成功，而不是无差别吞掉损坏。

## 5.3 中危问题：FUSE 层对同路径 rename 返回 EINVAL

### 现象

在 FUSE `rename` 实现中，代码显式禁止以下情况：

- `parent == new_parent && name == new_name`

并直接返回 `EINVAL`。

但在更下层：

- Redis MetaStore 的 `rename` 将同位置 rename 视为 no-op 成功。
- VFS 的注释也明确承认 POSIX 允许这种 no-op。

也就是说，同一个操作从 FUSE 入口进入时会失败，但从 VFS API 或 Redis store 入口进入时会成功。

### 为什么这是问题

- 语义不一致会让挂载态行为和非挂载态行为不一致。
- 某些用户态程序会使用“rename 到自己”作为幂等步骤，这类程序在 FUSE 挂载下会得到意外 `EINVAL`。
- 这不是实现缺失，而是接口层主动引入的偏差。

### 修复建议

最直接的做法是把 FUSE 层这一分支改为直接返回成功，或者直接把请求下放给 VFS，让底层保持统一语义。

## 5.4 低危问题：FUSE 适配层职责分散，`adapter.rs` 仍为占位模块

### 现象

从目录结构看，读代码的人容易以为 `src/fuse/adapter.rs` 是主要 FUSE 入口，但实际上：

- `adapter.rs` 目前只是占位。
- 真正的 FUSE 逻辑都堆在 `src/fuse/mod.rs`。

### 影响

这不是当前 correctness 的主要来源，但会带来两个副作用：

- 审查时容易误判真实入口。
- 后续维护时，接口适配、错误映射、平台行为说明不够集中。

### 建议

等高优问题修复后，再考虑做结构整理：

- 要么删掉占位适配层，避免误导。
- 要么把 `Filesystem` 实现按操作类别拆回 `adapter.rs` / `ops.rs` 之类的模块。

## 5.5 高危问题：Redis 分布式范围锁的冲突检查与提交不是原子操作

### 问题描述

Redis 后端的 `set_plock` 最终落到 `try_set_plock`。当前实现流程是：

1. 先读取当前会话自己的锁记录。
2. 再用 `HKEYS` 枚举该 inode 的所有其他锁字段。
3. 逐个 `HGET` 读取其他会话的锁区间。
4. 用本地内存判断是否冲突。
5. 如果没冲突，再执行 `HSET` 写回自己的新锁记录。

真正的冲突检查与写入提交之间没有 Lua 脚本、没有 `WATCH/MULTI/EXEC`，也没有 compare-and-set。

### 风险场景

如果两个客户端同时为同一 inode 申请互斥写锁：

1. 客户端 A 读取现有锁，发现无冲突。
2. 客户端 B 读取现有锁，也发现无冲突。
3. A 写入自己的锁。
4. B 也写入自己的锁。

最后两个本应互斥的锁都成功，系统却没有返回任何冲突。

### 为什么这是高危问题

- 这是典型的分布式锁丢失互斥问题。
- 它直接违反 POSIX 记录锁最基本的冲突语义。
- 一旦发生，会让两个进程同时认为自己持有写锁，后续数据路径保护完全失效。

### 修复建议

Redis 后端的范围锁至少需要改成以下任一形式：

1. 用 Lua 脚本把“读取现有锁、检查冲突、写回新锁”合并成单次原子执行。
2. 或使用 `WATCH` 监视整个 `plock` key，再在冲突检查后通过 `MULTI/EXEC` 提交，失败时重试。

如果不做这一步，Redis 后端的分布式 `fcntl` 语义不能认为是可靠的。

## 5.6 中危问题：Redis 全局锁是软租约，不具备明确 owner 与 fencing 语义

### 问题描述

Redis `get_global_lock` 当前维护的是 `LOCKS_KEY` 里的一个时间戳字段。逻辑是：

- 若字段不存在，则写入当前时间并视为获取成功。
- 若字段存在但超过 7 秒未更新，则覆盖时间戳并视为获取成功。
- 否则视为获取失败。

这个机制能实现“粗略互斥”，但它并不是严格意义上的分布式锁。

### 不足

它缺少几个关键属性：

1. 没有 owner 标识，无法知道当前是谁持锁。
2. 没有 fencing token，外部系统无法区分旧持锁者和新持锁者。
3. 只依赖本地时间差，进程暂停、GC 停顿、时钟问题都会影响判断。
4. 也没有显式释放协议，属于典型的“基于时间戳续租”的软锁。

### 影响判断

这个锁更像后台维护任务的 best-effort 抢占，不适合承担严格的元数据线性化职责。如果它只用于 cleanup 之类后台任务，风险可接受；但若未来把它扩展到更关键的元数据仲裁场景，就会不够。

## 6. 符号链接与硬链接语义补充观察

## 6.1 符号链接路径

当前 `symlink` / `read_symlink` 的基本语义是通的：

- 创建时通过 `create_entry` 分配一个 `Symlink` 节点。
- 随后写入 `symlink_target` 和 `size`。
- `read_symlink` 会检查节点类型，再返回目标字符串。
- VFS `readlink_ino` 和 `readlink` 都会先校验 `FileType::Symlink`。

这一段没有看到明显的高危逻辑错误。

### 需要注意的点

- 这条路径主要依赖节点 JSON 的完整性。
- 如果 Redis 中的 `symlink_target` 丢失，当前实现会返回内部错误，而不是更细粒度错误码。
- 目前这还属于合理范围，但后续可以考虑更明确地区分“损坏”和“类型不匹配”。

## 6.2 硬链接路径

Redis 后端通过 `link_parents` 集合管理 `nlink > 1` 的文件。当前设计整体是清晰的：

- 从 `nlink = 1` 过渡到 `nlink = 2` 时，把原始 `(parent, name)` 放入集合。
- rename 时，如果是硬链接文件，则更新 `link_parents` 而不是直接写 `parent/name`。
- `get_names` / `get_paths` 会基于 `link_parents` 重建路径集合。

### 需要继续关注的点

- 这一设计与覆盖式 rename 的竞态叠加后，状态恢复会更复杂。
- 因为一旦目标替换提前发生，后续失败时不只是目录项可能丢失，`link_parents` 的期望状态也会被打破。

这进一步说明高优先级问题应在 MetaStore 原子语义层解决，而不是继续在 VFS 层打补丁。

## 7. 测试覆盖评估

### 7.1 已有覆盖

当前已有测试覆盖了不少基础场景：

- 符号链接 round-trip 与 unlink。
- 硬链接跨目录重命名与删除。
- `rename_exchange` 的并发场景。
- VFS 层的基础 rename、目录替换、循环重命名、`RENAME_NOREPLACE` 等。

### 7.2 关键缺口

以下场景值得补充：

1. Redis 后端下，覆盖式 rename 的并发竞争测试。
2. 目标存在时，VFS 先删目标、后 rename 的失败回滚语义测试。
3. 双重 unlink 并发是否应该第二次返回 `ENOENT` 的行为测试。
4. FUSE 层“同路径 rename”应为成功还是 `EINVAL` 的接口语义测试。
5. 节点 JSON 损坏时，`unlink` / `readlink` / `rename` 的 errno 映射测试。

## 8. 修复优先级建议

### P0

修复覆盖式 `rename` 的原子性问题。建议把“目标替换 + 源移动”合并到单个 Redis 原子操作中完成。

### P1

收紧 `UNLINK_LUA` 的成功条件，不要把节点缺失或损坏统一吞成成功删除。

### P1

修正 FUSE 层同路径 rename 的行为，使其与 VFS/MetaStore 保持一致，返回成功 no-op。

### P2

补齐 Redis 覆盖式 rename 和 FUSE errno 行为的针对性测试。

### P3

整理 FUSE 模块结构，减少 `adapter.rs` 与 `mod.rs` 的职责错位。

## 9. 推荐整改路线

如果按最小扰动原则推进，可以采用如下顺序：

1. 先修 FUSE 同路径 rename 的错误返回。
2. 再修 `UNLINK_LUA` 的吞错语义。
3. 最后重构 Redis 覆盖式 rename，使目标替换与源移动在单个原子步骤内完成。

如果按风险优先级推进，则应反过来：

1. 先解决覆盖式 rename 的原子性缺口。
2. 再解决 unlink 吞错。
3. 最后处理 FUSE 接口一致性。

从 correctness 角度看，第二种顺序更合理。

## 10. 最终判断

这次评估中，最重要的结论是：

Redis 后端当前已经具备基础文件元数据操作能力，但它还没有完整承接 VFS 层对 POSIX rename 原子替换语义的要求。问题不在单个函数是否“能工作”，而在多层组合后的整体语义比接口声称的更弱。

另外，Redis 的记录锁实现目前也不能被视为严格分布式锁：冲突检查与写入提交之间存在竞态窗口，这会直接破坏互斥性。

因此，后续整改不应只盯着 FUSE errno 映射，而应优先把 Redis MetaStore 的 rename 替换语义补齐。只有这样，VFS 与 FUSE 暴露给用户态程序的行为才会真正稳定。