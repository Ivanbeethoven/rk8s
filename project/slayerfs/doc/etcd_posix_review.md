# SlayerFS Etcd 元数据后端 POSIX 语义分析报告

_Date: 2026-04-13_

## 1. 目标与结论

本文分析 SlayerFS 在 etcd 元数据后端下的 POSIX 语义实现，重点评估路径解析、目录项可见性、重命名、硬链接、符号链接和错误码行为是否一致，并指出当前实现中的不足。

结论先行：

1. etcd 后端并不是简单的“事务版 Redis”，它采用了多索引模型，正确性依赖 forward key、reverse key、children key、link parent key 之间保持一致。
2. 当前实现最大的风险不是单次事务失败，而是“写事务完成后，部分辅助索引在事务外更新，失败后仍返回成功”。这会让不同 API 对同一目录项得出不同结论。
3. `rename_exchange` 在 etcd 后端下存在明显的不完整实现，只交换了 forward key，没有同步 reverse 元数据、硬链接索引和父目录 children 索引，这已经超出“边角不一致”，属于会直接破坏路径语义的 correctness 问题。
4. etcd 后端的范围锁虽然放进了事务，但锁状态按 inode 聚合在单个 key 下，并把整个 key 绑定到“最后写入者”的 session lease，这会导致不同会话之间的锁生命周期互相污染。

本文基于静态代码阅读完成，没有编译项目，也没有运行 etcd 集群与测试。

## 2. 评估范围

本次分析覆盖：

- `src/meta/stores/etcd/mod.rs`
- `src/meta/stores/etcd/txn.rs`
- `src/meta/stores/etcd/watch.rs`
- `src/meta/stores/etcd/tests.rs`
- `src/vfs/fs/mod.rs`
- `src/fuse/mod.rs`

分析对象不是单独的 MetaStore trait，而是“FUSE/VFS 通过 etcd 后端实际暴露给用户态的 POSIX 语义”。

## 3. Etcd 后端的数据模型

当前 etcd 后端至少维护四类关键索引：

### 3.1 forward key

格式：`f:{parent_inode}:{name}`

含义：

- 这是按父目录和名字定位 inode 的正向目录项索引。
- `lookup(parent, name)` 直接依赖它。

### 3.2 reverse key

格式：`r:{inode}`

含义：

- 保存 inode 的主元数据。
- 包含：
  - 是否为文件
  - 大小
  - 权限
  - 时间戳
  - `nlink`
  - `parent_inode`
  - `entry_name`
  - `deleted`
  - `symlink_target`

对单链接文件、目录、符号链接而言，`parent_inode` 和 `entry_name` 直接参与路径重建。

### 3.3 children key

格式：`c:{parent_inode}`

含义：

- 保存某个目录下的名字到 inode 的映射集合。
- `lookup_path` 和 `readdir` 不是直接扫描 forward key，而是先读 children key，再用 forward key 做补充查询。

### 3.4 link parent key

格式：`l:{inode}`

含义：

- 仅在 `nlink > 1` 的文件上使用。
- 保存所有 `(parent_inode, entry_name)` 绑定，用于硬链接路径恢复。

## 4. POSIX 语义的实现路径

### 4.1 FUSE 层

FUSE 层主要负责参数校验、路径构造和 errno 映射，并不直接理解 etcd 索引结构。

### 4.2 VFS 层

VFS 层负责 POSIX 语义协调，例如：

- 路径规范化
- 目录与文件替换规则
- 循环重命名检查
- `rename_exchange` / `rename_noreplace` 等扩展接口

### 4.3 Etcd MetaStore 层

etcd 后端通过 `EtcdTxn` 实现乐观并发控制：

- 每轮事务先记录读集合。
- 提交时用 etcd `Compare::mod_revision` 做 CAS。
- 若事务提交失败，自动回退并重试。

这部分设计本身是合理的，但它只能保证“某一组 key 的事务原子性”。如果关键索引被拆到事务外更新，整体语义仍然可能失真。

## 5. 总体评价

### 5.1 做得好的部分

etcd 后端有几个设计是扎实的：

1. 写路径普遍使用统一的 `EtcdTxn` 封装，避免了随处手写 CAS。
2. `create_entry` 与 `delete_entry` 把“存在性检查 + 多 key 修改”放进了同一事务。
3. 硬链接在 `link` / `unlink` / `rename` 中有专门的 `link parent` 维护逻辑，说明设计上意识到了 `nlink > 1` 与单路径元数据模型之间的矛盾。
4. watch 机制具备缓存失效能力，为多客户端场景做了准备。

### 5.2 当前主要短板

最大的问题是：

- 路径读取和目录遍历使用 `children` 索引。
- 多个写操作却在 forward/reverse 事务提交后，单独再更新 `children`。
- 一旦后者失败，代码把“forward key 仍然正确”视为可接受状态。

这一假设与当前 `lookup_path` / `readdir` 的实现不一致，因此属于架构级短板，而不是个别 API 的小瑕疵。

## 6. 详细问题

## 6.1 高危问题：forward 索引与 children 索引不是同一事务更新，但路径解析依赖 children

### 问题描述

当前 etcd 后端存在两个不同的“目录事实来源”：

1. `lookup(parent, name)` 使用 forward key。
2. `lookup_path(path)` 与 `readdir(ino)` 使用 children key。

而多个写操作在成功提交 forward/reverse 事务后，会在事务外更新 children key，并在失败时继续返回成功。

### 证据链

#### 路径读取依赖 children key

`get_content_meta(parent_inode)` 的流程是：

1. 读取 `c:{parent_inode}`。
2. 按 children 中的名字列表构建目录内容。
3. 再批量取 `f:{parent}:*` 做补充映射。

因此，只要 children key 丢了某个名字，`lookup_path` 和 `readdir` 就会看不到该目录项，即便对应 forward key 仍然存在。

`lookup_path` 的实现也是逐层调用 `get_content_meta(current_inode)`，而不是逐层调用 `lookup(parent, name)`。

#### 多个写操作允许 children 更新失败后继续成功

以下操作都包含相同模式：

- `rmdir`
- `link`
- `unlink`
- `rename`

共同特征是：

1. 先用事务更新 forward/reverse 或删除 key。
2. 再调用 `update_parent_children(...)`。
3. 如果 children 更新失败，只记日志，仍然返回 `Ok(())`。

代码注释还明确写着：

- “forward key is the source of truth”
- “lookup will work correctly”

但这与 `lookup_path` 的真实实现不符。

### 影响

这会导致至少四类 POSIX 语义问题：

1. `lookup(parent, name)` 成功，但 `lookup_path("/dir/name")` 失败。
2. 文件已经 `unlink` 成功，但 `readdir` 仍可能看到旧名字，或者反过来新文件对路径遍历不可见。
3. `rename` 返回成功，但后续基于路径的操作无法继续访问新路径。
4. 多客户端场景下，watch 只能做缓存失效，无法自动修复持久层 `children` 索引本身的遗漏。

### 为什么这是高危问题

- 它影响 create/link/symlink/unlink/rmdir/rename 的共同语义基础。
- 它不是“缓存短暂不一致”，而是 etcd 持久层多索引不一致。
- 一旦出现，会直接让路径 API 与 inode API 对系统状态给出不同答案。

### 修复建议

建议把以下目标作为 etcd 后端的首要修复方向：

#### 方案 A：把 children key 纳入同一个事务

这是最直接的修复思路。对于 create/link/unlink/rmdir/rename：

- forward key
- reverse key
- children key
- 必要时的 link parent key

应尽量在同一次 `EtcdTxn` 中提交。

#### 方案 B：统一路径读取来源，弱化 children key 的权威性

如果出于性能考虑仍保留 children key，也至少要保证：

- `lookup_path` 不把 children key 当成唯一来源。
- children key 可以退化成缓存或加速索引，而非路径真相源。

否则 forward/reverse 与 children 的分裂会一直存在。

## 6.2 高危问题：`rename_exchange` 只交换 forward key，未同步 reverse 元数据和硬链接索引

### 问题描述

当前 etcd 的 `rename_exchange` 实现只做了两件事：

1. 读取旧的 forward entry 和新的 forward entry。
2. 交换两个 forward key 指向的 inode。

它没有同步更新：

- `r:{inode}` 中的 `parent_inode` / `entry_name`
- `l:{inode}` 中的硬链接父绑定
- `c:{parent}` children 索引
- 父目录时间戳

### 影响范围

这不是单纯的“元数据字段没更新”，而是会让多个 API 直接失真：

#### 对单链接文件、目录、符号链接

`get_names` 在 `nlink <= 1` 时直接返回 reverse key 中的 `parent_inode` 和 `entry_name`。如果 `rename_exchange` 不更新 reverse key，那么：

- `get_names` 错误
- `get_paths` 错误
- `path_of` 错误
- 依赖这些路径恢复逻辑的后续 rename/unlink/link 等操作都可能偏离预期

#### 对硬链接文件

`get_names` 在 `nlink > 1` 时依赖 `link parent key`。但 `rename_exchange` 完全没有更新这部分数据，因此硬链接交换后路径绑定会继续停留在旧位置。

#### 对目录遍历

`children` 索引也没有更新。即使 forward key 已交换，`lookup_path` / `readdir` 依赖的目录内容仍然可能停留在交换前。

### 为什么这是高危问题

- 它会直接破坏路径重建和名字重建。
- 它不是只影响一个边缘 flag，而是影响 `RENAME_EXCHANGE` 的核心正确性。
- 一旦用户或测试调用该接口，etcd 后端状态会进入“forward 看起来正确，reverse/path 看起来错误”的混合状态。

### 测试覆盖情况

目前仓库里能看到 `rename_exchange` 的测试主要在 VFS 通用测试中，且使用的是 sqlite 路径。etcd 后端自身测试里没有看到对应覆盖，这也是为什么这个问题能留在实现里。

### 修复建议

修复 `rename_exchange` 时至少需要同时处理：

1. 两个 forward key 的交换。
2. 两个 inode 的 reverse 元数据更新。
3. 若任一 inode 为硬链接文件，更新对应 `link parent key`。
4. 交换涉及的两个父目录 children 索引。
5. 更新两个父目录时间戳。

换句话说，`rename_exchange` 应该被看作“成组元数据交换事务”，而不是“仅交换两个 forward entry”。

## 6.3 中危问题：`rename` 的“children 更新失败可接受”假设与实现不匹配

### 问题描述

普通 `rename` 在事务中会正确更新：

- 新旧 forward key
- reverse key
- 必要时的 link parent key

但事务提交后，代码显式承认：

- parent children map updates are NOT atomic
- children map may be stale
- 仍然视为成功

这与上一个高危问题本质相关，但在 `rename` 上尤其值得单独指出，因为重命名是最容易暴露路径一致性问题的 POSIX 操作。

### 影响

`rename` 返回成功后，可能出现：

- `lookup(old_parent, old_name)` 失败
- `lookup(new_parent, new_name)` 成功
- 但 `lookup_path(new_path)` 仍然失败
- 或 `readdir(new_parent)` 看不到新名字

这会让上层程序观察到“目录项存在但遍历不到”的异常状态。

### 修复建议

与 6.1 相同，核心不是给日志写得更清楚，而是把 children 更新纳入真正的事务一致性设计。

## 6.4 中危问题：`unlink` / `rmdir` 的类型错误返回不符合 POSIX 预期

### 问题描述

在 etcd store 内部：

- `rmdir` 发现目标其实是文件时，返回 `MetaError::Internal("Not a directory")`
- `unlink` 发现目标其实是目录时，返回 `MetaError::Internal("Is a directory")`

这类错误没有使用更准确的 `MetaError::NotDirectory`、`MetaError::NotSupported` 或专门的目录/文件类型错误。

### 影响

在常见 FUSE 路径下，这一问题可能被上层的预检查遮住，因为 FUSE 在调用 VFS 前通常已经检查了子项类型。

但它仍然是 etcd 后端实现上的不足：

1. store 语义不自洽。
2. 一旦上层预检查与底层状态发生竞态，最终可能把类型错误映射成 `EIO` 而不是 `EISDIR` / `ENOTDIR`。
3. 对直接调用 MetaStore 的测试或工具而言，错误类型会失真。

### 修复建议

建议把类型不匹配显式建模为合适的 `MetaError`，避免内部错误字符串承担协议语义。

## 6.5 中危问题：etcd store 级 `rename` 对同路径操作不是 no-op

### 问题描述

当前 etcd `rename` 没有像 Redis store 那样对“同 parent、同名字”的情况做 no-op 优化。

按照当前实现：

- 若 `old_forward_key == new_forward_key`
- 事务里会执行 `tx.exists(&new_forward_key)`
- 然后直接返回 `AlreadyExists`

这意味着 etcd store 级别的 `rename(old, old)` 不是成功 no-op，而是错误。

### 影响

虽然当前 FUSE 层本身也对同路径 rename 有额外限制，但从 POSIX 和多后端一致性角度看，这仍然是不足：

- Redis 后端已经把同位置 rename 视为 no-op。
- etcd 后端在同一接口上表现不同。
- 这会让跨后端行为不一致，增加上层语义分支。

### 修复建议

在 etcd store 的 `rename` 开头补齐 self-rename no-op 处理，与其他后端保持一致。

## 6.6 高危问题：Etcd 范围锁把多会话锁状态聚合到单 key，却绑定到当前会话 lease

### 问题描述

etcd 后端的 `try_set_plock` 把某个 inode 的全部锁状态存到同一个 key：

- key 形如 `p:{inode}`
- value 是 `Vec<EtcdPlock>`，其中包含多个 `(sid, owner, records)`

同时，每次写回这个 key 时又会附带当前会话的 lease：

- `PutOptions::new().with_lease(*lease)`

这意味着：

- 一个 key 里保存了多个会话的锁。
- 但 key 的 TTL/生命周期只能属于“最后一次写它的那个会话”。

### 风险场景

#### 场景 1：会话 B 覆盖会话 A 的 lease 归属

1. 会话 A 在 inode X 上加锁，`p:X` 绑定到 A 的 lease。
2. 会话 B 也在 inode X 上加锁，事务会把完整锁表重写一次，并把 `p:X` 改绑到 B 的 lease。
3. 此时 key 中既有 A 的锁，也有 B 的锁，但 lease 已经属于 B。

如果 B 会话随后失效，etcd 可能删除整个 `p:X`，把 A 仍然有效的锁一起删掉。

#### 场景 2：失效会话的锁被活跃会话“续命”

1. 会话 A 已经失效，但其锁记录仍存在于 `p:X` 的 value 中。
2. 会话 B 后续更新同一 inode 上的锁，又把整个向量写回，并绑上 B 的 lease。
3. A 的陈旧锁记录被一并保留下来，生命周期被 B 的 lease 间接延长。

这会导致失效锁不按预期释放。

### 为什么这是高危问题

- 锁记录的归属单位是“每个会话”，但 lease 绑定单位却是“整个 inode 键”。
- 这会让锁清理和会话生命周期失去一一对应关系。
- 结果不是单纯的泄漏，而是“误删他人锁”和“替他人续命”两种方向都可能发生。

### 修复建议

如果继续使用 etcd lease 管理锁生命周期，建议改成以下任一设计：

1. 每个 `(inode, sid, owner)` 单独占用一个 etcd key，并由对应 sid 的 lease 管理。
2. 保持聚合 value 设计，但不要把整个 key 绑到会话 lease，而是改为显式会话清理逻辑。

当前实现把这两种设计混在一起了，是最危险的地方。

## 6.7 中危问题：Etcd 会话关闭路径并没有显式清理范围锁

### 问题描述

etcd 后端的 `shutdown_session_by_id` 只删除：

- `session:{sid}`
- `session_info:{sid}`

它不会主动扫描和删除该 sid 持有的 `plock` 记录。当前注释还写明：

- cleanup 由 lease keeper 负责。

但由于 6.6 所述的“整 key 绑定最后写入者 lease”问题，这种依赖 lease 的做法本身就不稳固。

### 影响

在理想情况下，lease 可以顺带清掉该会话锁；但在当前设计下：

- 它可能把其他会话的锁也删掉。
- 也可能完全删不掉当前会话留下的陈旧锁。

因此，session cleanup 与 lock cleanup 之间并不是可靠闭环。

### 修复建议

无论最终是否保留 lease，etcd 后端都应具备基于 sid 的显式锁清理路径。

## 6.8 中危问题：全局锁同样是时间戳软锁，不具备严格 fencing 语义

### 问题描述

etcd `get_global_lock` 与 Redis 版本类似，本质上也是：

- 读取一个时间戳
- 判断是否超过 7 秒
- 超时则覆盖并获取锁

这个锁虽然借助了事务 CAS，但依然没有：

- owner 信息
- fencing token
- 显式释放协议
- 与 session lease 的强绑定

### 影响判断

如果它只用于弱一致后台任务，问题不大；如果将来用于更严格的主从仲裁、全局清理排他或一致性修复，就不够安全。

## 7. 其他观察

## 7.1 watch worker 不是索引修复器

watch worker 的职责是缓存失效，不是持久层一致性修复。它无法修复“事务成功但 children key 更新失败”这种结构性问题。

因此，不能把 watch 视为 6.1 的补救手段。

## 7.2 事务封装本身没有明显设计错误

`EtcdTxn` 的乐观事务模型是比较干净的：

- 读写集合显式维护。
- blind write 也会在提交前建立基线。
- CAS 冲突会重试。

问题出在：关键索引并没有完全纳入同一次事务。

## 8. 测试覆盖评估

### 8.1 已有覆盖

etcd 后端已经有一些基础测试，尤其是：

- 硬链接跨目录 rename / unlink
- 文件锁基础行为

这些说明 etcd 不是完全未测试状态。

### 8.2 关键缺口

但以下场景看不到充分覆盖：

1. `rename_exchange` 的 etcd 后端测试。
2. children 索引更新失败后的路径可见性测试。
3. `lookup(parent, name)` 与 `lookup_path(path)` 一致性测试。
4. `readdir` 与 forward key 事实是否一致的测试。
5. 同路径 rename 的 etcd store 语义测试。

从当前问题分布看，etcd 后端最缺的不是“更多基础 CRUD 测试”，而是“多索引一致性测试”。

## 9. 修复优先级建议

### P0

重构 `rename_exchange`，确保 forward/reverse/link-parent/children/timestamp 全量一致更新。

### P0

统一目录事实来源，解决 forward key 与 children key 不在同一事务的问题。

### P0

重构 etcd 范围锁的存储模型，解决“多会话共享单 key + 单会话 lease 绑定”的结构性缺陷。

### P1

修正 `rename`、`unlink`、`rmdir` 等操作在 children 更新失败后的成功策略，至少不能再默认认为路径解析不受影响。

### P1

补齐 etcd store 级 `rename(old, old)` 的 no-op 语义。

### P1

为 session shutdown 增加基于 sid 的显式锁清理，不能只依赖 lease。

### P2

修复 `unlink` / `rmdir` 的错误类型建模，使其更接近 POSIX 错误码预期。

## 10. 最终判断

SlayerFS 的 etcd 后端已经具备较好的事务基础，但当前 POSIX 语义还不够完整。最大的不足不是“etcd 不支持事务”，而是“事务只覆盖了一部分索引，而路径语义又依赖另一部分索引”。

此外，etcd 范围锁的当前存储方式也不够稳固：锁记录属于多个会话，lease 却只能属于最后一个写入者。这使得会话失效与锁释放之间的关系变得不可信。

如果不优先解决多索引一致性问题，后续继续在 FUSE 层或 VFS 层补丁式修修补补，只会让行为更复杂，不能真正提升 etcd 后端的 POSIX 可靠性。

因此，对 etcd 后端的整改建议只有一句话：

先统一目录项真相源，再谈性能优化和缓存失效。