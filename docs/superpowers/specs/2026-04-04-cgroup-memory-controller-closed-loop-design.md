# cgroup v2 memory controller 闭环设计

- 日期：2026-04-04
- 主题：在 DragonOS 中继续实现 cgroup v2 memory controller，使其从“文件节点与配置存储”推进到“匿名页真实记账 + 层级限制 + reclaim/OOM 可观测闭环”
- 范围：仅覆盖用户态匿名页；不覆盖 page cache、slab、swap、socket memory

## 1. 背景

当前仓库已经完成了 memory controller 的第一批基础能力：

- 暴露 `memory.current`、`memory.max`、`memory.high`、`memory.low`、`memory.stat`、`memory.events` 文件节点
- 为 `memory.max/high/low` 增加 per-cgroup 配置存储与文件读写语义
- 增加了基础可见性与读写测试

但当前实现仍停留在“控制面先行”的阶段：

- `memory.current` 仍返回静态占位值
- `memory.stat` / `memory.events` 仍返回静态占位内容
- 尚未建立匿名页 charge / uncharge 记账闭环
- `memory.high` / `memory.max` / `memory.low` 尚未真正影响内存分配、回收与 OOM 路径

因此，下一迭代目标是把 memory controller 从“可见、可配置”推进到“真实生效、可测试、可观测”的最小闭环实现。

## 2. 目标

本轮实现目标如下：

1. 对匿名页建立真实的 cgroup charge / uncharge 机制。
2. 让 `memory.current` 反映真实的匿名页层级使用量。
3. 让 `memory.stat` / `memory.events` 动态输出当前状态，而不是静态占位内容。
4. 在匿名页新增 charge 前，对祖先链上的 `memory.high` / `memory.max` 做层级判定。
5. 超过 `memory.high` 时触发定向 reclaim 尝试，但不直接等价于 OOM。
6. 超过 `memory.max` 时先 reclaim，失败后进入 cgroup-local OOM，再失败才返回分配失败。
7. `memory.low` 以“回收偏置保护”的近似语义参与 reclaim 选择。
8. 为上述行为补齐单元测试、文件接口测试与用户态集成测试。

## 3. 非目标

本轮明确不做：

- page cache / 文件页计账与限制
- slab / socket memory / hugetlb / PSI / NUMA 统计
- `memory.swap.*`
- 完整复刻 Linux 的 throttle、reclaim 打分、OOM victim 打分等全部细节
- 为了实现 memory controller 而对全局内存回收框架做大规模重构

## 4. 设计原则

1. **语义目标尽量贴近 Linux**：接口名、层级语义、历史页归属规则尽量与 Linux cgroup v2 memory controller 保持一致。
2. **范围只收敛到匿名页**：保证实现闭环，不把范围扩散到 page cache 等复杂对象。
3. **状态、策略、执行分层**：
   - cgroup 层维护 memory state 与策略判定
   - mm 层负责匿名页 charge/uncharge、reclaim、OOM 执行
   - cgroup2 fs 层只负责控制面与文本输出
4. **历史页不随 task 迁移重挂账**：页在 charge 时确定归属；task 迁移后，仅新页进入新 cgroup。
5. **可观测优先**：所有关键路径都要能通过 `memory.current` / `memory.stat` / `memory.events` 体现结果。

## 5. 语义定义

### 5.1 记账对象

本轮仅对**用户态匿名页**记账。`memory.current` 表示目标 cgroup 子树内已记账匿名页字节数。

### 5.2 层级语义

采用层级聚合语义：

- 匿名页 charge 到某个叶子 cgroup
- 同时把字节数向其所有祖先节点递增
- uncharge 时沿同一祖先链递减

因此：

- 叶子 cgroup 的 `memory.current` 表示其子树匿名页总量
- 父 cgroup 的 `memory.current` 表示整个子树匿名页总量
- `memory.high` / `memory.max` / `memory.low` 都按层级约束参与判定

### 5.3 迁移语义

当 task 通过 `cgroup.procs` 迁移后：

- 历史匿名页不重挂账
- 迁移后新分配的匿名页 charge 到新 cgroup
- 历史页释放时，从原始 charge 归属 cgroup uncharge

该语义与 Linux 的“页归属由 charge 决定”方向一致，避免在迁移时扫描地址空间并整体重写归属。

### 5.4 `memory.low`

本轮将 `memory.low` 定义为**reclaim 选择时的近似保护阈值**：

- reclaim 优先从超过 low 保护量的 cgroup 回收
- 若所有候选都低于 low，仍允许继续回收，避免永久卡死
- 与 Linux 更细的 protection 算法差异将在实现文档与提交说明中明确写出

### 5.5 `memory.high`

本轮将 `memory.high` 定义为**同步 reclaim 触发阈值**：

- 新增 charge 前，如果祖先链任一节点超过 `memory.high`
- 增加 `events.high`
- 对触发链上的目标 cgroup 执行定向 reclaim 尝试
- 只要 reclaim 后能够继续推进，本次分配不报 OOM

本轮不实现完整 Linux throttle 行为，只实现可观测、可测试的 reclaim trigger。

### 5.6 `memory.max`

本轮将 `memory.max` 定义为**硬限制阈值**：

- 新增 charge 前，如果祖先链任一节点在本次分配后将超过 `memory.max`
- 增加 `events.max`
- 先执行定向 reclaim
- reclaim 后仍超限，则增加 `events.oom` 并进入 cgroup-local OOM
- 若实际 kill 发生，则增加 `events.oom_kill`
- 若 OOM 后仍不能满足本次分配，则返回分配失败

## 6. 架构设计

本轮实现拆成四个单元。

### 6.1 memory accounting core

位置：`kernel/src/cgroup/`

职责：

- 在 `CgroupNode` 上维护 memory controller 运行时状态
- 提供 charge / uncharge / stat / events 接口
- 处理祖先链聚合与限制判定

建议新增 `CgroupMemoryState`，至少包含：

- `anon_bytes`
- `anon_pages`
- `reclaim_count`
- `events_low`
- `events_high`
- `events_max`
- `events_oom`
- `events_oom_kill`

`memory.max/high/low` 现有字段继续保留，作为配置面；`CgroupMemoryState` 负责运行时数据面。

### 6.2 anonymous page charging hooks

位置：`kernel/src/mm/` 匿名页建立、释放、地址空间销毁相关路径

职责：

- 在匿名页真正建立物理占用时执行 charge
- 在匿名页被释放、回收、unmap、进程退出地址空间销毁时执行 uncharge
- 给匿名页写入“原始 charge 归属 cgroup”元数据

为支持“历史页不随迁移重挂账”，匿名页元数据中需要新增最小 memcg 归属信息。推荐使用 `Weak<CgroupNode>` 或等价 lightweight 标识，并带一个“该页是否已 charge”的布尔状态，避免重复 uncharge。

### 6.3 limit enforcement path

位置：`kernel/src/cgroup/` 与 `kernel/src/mm/` 的交界层

职责：

- 在新增 charge 前沿祖先链检查 `memory.high` / `memory.max`
- 触发 reclaim
- 触发 cgroup-local OOM
- 决定本次分配能否继续推进

建议 API 目标形态为：

- `memcg_try_charge(cgroup, bytes)`
- `memcg_commit_charge(page, charge_ctx)`
- `memcg_cancel_charge(charge_ctx)`
- `memcg_uncharge(page)`

若当前 mm 路径难以一次改成 try/commit/cancel，可先实现简化版，但代码边界仍按该接口设计，避免后续重构成本过高。

### 6.4 cgroup fs dynamic views

位置：`kernel/src/filesystem/cgroup2/mod.rs`

职责：

- 保留 `memory.max/high/low` 的现有读写接口
- 让 `memory.current` / `memory.stat` / `memory.events` 每次 read 时动态生成输出
- 不再把动态文件的业务状态保存在 inode `data` 缓冲区中

最小输出字段定义：

- `memory.current`
  - 当前子树匿名页使用字节数
- `memory.stat`
  - `anon`
  - `current`
  - `reclaim`
- `memory.events`
  - `low`
  - `high`
  - `max`
  - `oom`
  - `oom_kill`

## 7. 关键数据流

### 7.1 匿名页 charge 路径

1. 匿名页即将真正占用物理页。
2. 读取当前 task 所属 cgroup，作为本次 charge 的目标叶子 cgroup。
3. 沿祖先链检查 `memory.high` / `memory.max`。
4. 若命中 `memory.high`：
   - 增加 `events.high`
   - 对目标 cgroup 子树执行定向 reclaim。
5. 若命中 `memory.max`：
   - 增加 `events.max`
   - 执行更强的 reclaim 尝试。
6. reclaim 后仍无法满足：
   - 增加 `events.oom`
   - 进入 cgroup-local OOM。
   - 若发生实际 kill，则增加 `events.oom_kill`。
7. 若最终允许分配成功：
   - 给页写入 memcg 归属元数据。
   - 对归属 cgroup 及其祖先链递增 usage/stat。

### 7.2 匿名页 uncharge 路径

触发点包括：

- `munmap`
- 地址空间销毁
- reclaim 回收匿名页
- 进程退出导致匿名页真正释放

顺序：

1. 读取页上记录的原始 charge 归属 cgroup。
2. 沿祖先链递减 usage/stat。
3. 若本次释放由 reclaim 导致，则增加 reclaim 统计。
4. 清理页上的 memcg 归属状态。

关键约束：uncharge 必须按“页原始归属”走，不能按 task 当前 cgroup 走。

### 7.3 迁移路径

`cgroup.procs` 迁移仅更新 task 当前所属 cgroup：

- 不扫描旧地址空间
- 不整批迁移历史匿名页的 charge 归属
- 迁移后新 fault / 新匿名页分配进入新 cgroup

因此会出现以下符合设计的可观测结果：

- 迁移前在 A 中分配的匿名页，usage 继续留在 A
- 迁移到 B 后新增匿名页，usage 进入 B
- 释放时分别从各自原始归属回账

### 7.4 reclaim 路径

本轮 reclaim 采用**最小可用的定向 reclaim**：

1. 某个层级节点命中 `memory.high` 或 `memory.max`
2. 构造 reclaim 目标 cgroup 域
3. 只在该 cgroup 子树内寻找可回收匿名页
4. 优先回收超过 `memory.low` 保护量的部分
5. 回收成功后更新 usage、`memory.stat`、`reclaim_count`
6. 重新判断是否仍然超限

本轮不做全局 LRU 重构；目标是先把 cgroup 定向回收路径跑通。

### 7.5 OOM 路径

当 `memory.max` reclaim 后仍不足：

1. 以触发超限的约束 cgroup 作为 OOM 域
2. 在该子树内选择 victim task
3. 优先选择匿名内存占用更高、释放收益更大的任务
4. 执行 kill，等待释放效果出现
5. 重新尝试本次 charge
6. 若仍失败，则返回分配失败

victim 选择不追求完全复刻 Linux 打分算法，但影响范围必须限制在目标 cgroup 子树内。

## 8. 错误处理与并发约束

### 8.1 charge 失败一致性

若 `memory.max` 路径最终失败：

- 不得留下半次 charge
- 不得留下错误的页归属状态
- `events.max` / `events.oom` / `events.oom_kill` 必须与实际路径一致

### 8.2 reclaim 不足是正常路径

- `memory.high` 下 reclaim 不足不等于 bug，只表示需要继续尝试推进
- `memory.max` 下 reclaim 不足后需要进入 OOM 或返回失败

### 8.3 迁移并发

当 task 迁移与页释放、新增分配并发发生时：

- charge 以分配当下 task 所属 cgroup 为准
- uncharge 以页原始 charge 归属为准
- 不允许通过“读取 task 当前 cgroup”来决定释放归属，否则会导致错账

### 8.4 计数安全

- 所有 bytes/page counters 使用 saturating 减法或等价保护
- 对重复 uncharge、未 charge 页 uncharge 等场景增加 debug 断言或保护分支

## 9. 测试设计

### 9.1 内核单元测试

新增或扩展 `kernel/src/cgroup/` 与 `kernel/src/filesystem/cgroup2/` 的测试，覆盖：

- 叶子 charge 导致祖先链同步增加
- 叶子 uncharge 导致祖先链同步减少
- `memory.max/high/low` 配置与读写语义不回归
- `memory.current/stat/events` 动态输出正确
- `events.high/max/oom/oom_kill` 计数逻辑正确

### 9.2 cgroup2 文件接口测试

扩展现有 cgroup2 文件测试，验证：

- `memory.current` 随真实 usage 变化
- `memory.stat` 输出稳定 `key value` 文本
- `memory.events` 输出稳定字段集
- 仍保持现有 `memory.max/high/low` roundtrip 语义

### 9.3 用户态集成测试

新增或扩展 C 测试程序，覆盖：

1. **基础记账**
   - 分配匿名页后 `memory.current` 增加
   - 释放后减少

2. **层级聚合**
   - 在子 cgroup 分配后，父子 `memory.current` 都增加

3. **迁移语义**
   - 在 A 分配后迁移到 B 再分配
   - A 保留历史页 usage，B 只包含新页 usage

4. **high 路径**
   - 配小 `memory.high`
   - 分配后 `memory.events.high` 增加
   - 不直接等价为 OOM

5. **max 路径**
   - 配小 `memory.max`
   - 观测到 `max/oom/oom_kill` 相关事件变化
   - 路径顺序为“先 reclaim，再 OOM/失败”

## 10. 实现切分

为了降低实现风险，本轮实现分四步推进。

### Step 1：真实记账闭环

- 为 `CgroupNode` 增加 `CgroupMemoryState`
- 为匿名页增加 memcg 归属元数据
- 接入匿名页 charge / uncharge
- 让 `memory.current` 动态反映真实层级 usage

### Step 2：动态观测面

- 让 `memory.stat` / `memory.events` 动态输出
- 补齐对应单测与文件接口测试

### Step 3：层级限制判定

- 在 charge 前沿祖先链检查 `memory.high` / `memory.max`
- `memory.high` 命中时触发 reclaim 与 `events.high`
- `memory.max` 命中时触发 reclaim 与 `events.max`

### Step 4：最小 cgroup-local reclaim / OOM

- 在目标 cgroup 子树内执行 reclaim
- 以 `memory.low` 作为回收偏置保护
- reclaim 失败后进入 cgroup-local OOM
- 完成集成测试闭环

## 11. 风险与缓解

### 风险 1：匿名页 charge 点选错，导致漏记或双记

缓解：

- 把 charge 点放在“匿名页真正建立物理占用”的统一路径
- 用页级归属标记防止重复 charge / uncharge
- 先通过单元测试把 charge/uncharge 守恒验证出来

### 风险 2：迁移语义与释放路径错位，导致负数或错账

缓解：

- 明确规定：释放只认页原始归属，不认 task 当前归属
- 在 debug 场景加断言与 saturating 保护

### 风险 3：把 reclaim / OOM 逻辑塞进 fs 层，导致边界混乱

缓解：

- fs 层只做文本接口
- cgroup 层做策略
- mm 层做执行

### 风险 4：一次追太多 Linux 细节，拖慢迭代

缓解：

- 本轮明确只做匿名页
- `memory.low` / `memory.high` 采用近似但稳定、可测试的语义
- 差异显式文档化

## 12. 成功标准

本轮完成后，应满足以下结果：

1. `memory.current` 能真实反映匿名页层级使用量。
2. `memory.stat` / `memory.events` 不再是静态占位内容。
3. task 迁移后，历史页与新页归属行为可通过测试稳定验证。
4. 超过 `memory.high` 时能观测到 reclaim 触发与事件变化。
5. 超过 `memory.max` 时能观测到“先 reclaim，再 OOM/失败”的顺序。
6. 所有新增能力有对应的内核测试与用户态测试支撑。

## 13. 当前代码落点

本设计直接对应当前仓库中的以下位置：

- cgroup memory 配置存储：`kernel/src/cgroup/core.rs`
- cgroup v2 memory 文件节点与读写：`kernel/src/filesystem/cgroup2/mod.rs`
- 当前 cgroup2 memory 文件测试：`kernel/src/filesystem/cgroup2/tests_tmp.rs`
- 当前用户态基础测试：`user/apps/c_unitest/test_cgroup_mvp_basic.c`

后续实现会优先在这些位置扩展，而不是引入新的并行控制面。
