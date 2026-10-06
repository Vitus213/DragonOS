# cgroup v2 控制器框架设计

## 目标

将当前硬编码的控制器（cpu/memory/freezer/pids）迁移到可扩展的 trait 框架，对齐 Linux `cgroup_subsys` 架构。

## 当前实现

当前代码采用 `CgroupNode::subsys[]` 保存每个控制器的 CSS，并在根节点和新建子节点创建 CSS。已注册控制器：

- `cpu`：`cpu.weight`、`cpu.max`、`cpu.stat`。`cpu.weight` 通过 CFS scaled load/reweight 实时生效（`set_shares` → `reweight_task_cpu_weight`，weight = shares·NICE_0_LOAD/100，即 v1 shares 语义下 ≈(shares/1024)·NICE_0_LOAD）；fair 执行路径（tick 与 pick_next 共同经过的 `update_current`）按 `cpu.max` 扣减配额，耗尽时实体被排除出选择（节流），周期边界自动恢复（unthrottle），`throttled_usec`/`nr_throttled`/`nr_periods` 计入 `cpu.stat`；任务唤醒（`check_enqueue_throttle`）与跨 cgroup 迁移（`task_change_group` 带宽重检）同样执行配额检查。
- `memory`：`memory.current/peak/min/low/high/max`、事件和统计接口。计费主体是 `MemoryCss::try_charge/uncharge`（对应 Linux `mm/memcontrol.c` 的 `try_charge/commit_charge/uncharge`）：对 leaf 与全部祖先做事务式 max 检查后逐层累加，任何一层超限则整体不更新（无回滚窗口）。钩子层 `mm/memcg.rs` 将计费嵌入两个架构的 `LockedFrameAllocator::{allocate,allocate_below,free}`，因此 mm 缺页、mmap COW、fork 复制、exit 释放以及内核内存（页表/slab/DMA）都在同一条计费路径上，无漏计无重计；每帧归属记录（Linux `page->memcg` 的等价物）保证释放方与计费方分属不同任务/cgroup 时统计仍一致，迁移（`cgroup.procs` write）后旧计费留在原 memcg、新分配跟随新 css_set。`memory.high` 越限只在计费路径设 trip 标志并唤醒回收线程，节流（有界同步回收 + 进度检查 + 有界睡眠）在缺页路径 `memcg_handle_over_high()` 执行（对应 Linux `mem_cgroup_handle_over_high()`）；`memory.max` 拒绝的计费记为 pending，由 `oom::pagefault_out_of_memory` 优先排水到 `oom.rs` 状态机的 cgroup 范围版本（`scoped_out_of_memory`，复用单飞选择、inflight victim、killable 等待与 mm 释放通知）。
- `pids`：`pids.max`、`pids.current`、`pids.events`；fork、退出和迁移执行层级计数，迁移不被 `pids.max` 阻塞。
- `freezer`：`cgroup.freeze`、`cgroup.events` 和 scheduler refrigerator；queued/current/sleeping 任务的冻结标志、计数和解冻唤醒路径保持幂等。
- `cpuset`：CPU/memory mask、继承后的 online-aware effective mask、fork/迁移/调度路径约束；affinity 与 effective mask 的空交集返回错误，不扩大用户请求。
- `io`：`io.max`、`io.weight`、`io.stat`；所有实际块分发/提交路径（GenDisk 读写入口、ext4 适配器 read_block/read_blocks/write_block/write_blocks/submit_ext4_read、MBR 扫描）在设备调用前执行祖先限速，完成后统计；限速为 100ms slice 的周期结算 token 桶（对齐 cpu.max 的 `refresh_period_locked` 与 Linux blk-throttle 的 `throtl_slice`/`throtl_charge_bio`），等待发生在设备表锁之外；ext4 异步读在提交任务上下文限速并捕获 cgroup，由等待方按提交者记账；无任何 io.max 配置时全局计数器为零开销直通。
- `cgroup.type`：实现 domain/threaded/domain threaded/domain invalid 的基础状态机；控制器 threaded/domain 组合校验。任务持有每个 cgroup 的 CSS-set token，迁移/fork/exit 维护 token 生命周期。

控制器文件仍由 `filesystem/cgroup2/files.rs` 管理；CSS 的 `dfl_cftypes` 是扩展接口，不是当前文件系统路由的唯一入口。

本轮（分支 `fix/io-blkcg-close-loop`，提交 `ca4ba968` 起）`make kernel` 干净构建通过；此前 `nix run .#yolo-x86_64 -- -nographic` 已启动到 guest shell，日志确认 ProcFS、SysFS、cgroup2 挂载和六个控制器注册成功。基础 cgroup 文件、控制器启用、cpuset effective、io.weight 和任务迁移曾在 QEMU guest 中验证；cpu.max 节流、memory.current 变化、pids.max fork 拦截、freeze/thaw 往返的 guest 内观察因 Darwin TCG 环境启动耗时不可接受未在本轮执行，以代码路径审计与构建验证替代（见各控制器"语义落点"）。

## 控制器×文件矩阵

| 控制器 | 文件 | 实现状态 | 语义落点 |
|---|---|---|---|
| core | `cgroup.controllers` / `cgroup.subtree_control` / `cgroup.type` | real | `files.rs` 路由；启用位掩码 + domain/threaded 状态机 |
| core | `cgroup.procs` | real | 迁移走 CSS-set token 生命周期（fork/exit/迁移维护） |
| core | `cgroup.freeze` / `cgroup.events` | real | freezer 状态机消费；frozen 计数上抛 events |
| cpu | `cpu.max` | real | `CpuCss::refresh_period/try_consume_runtime` 接入 fair `update_current`（tick/pick_next 共同路径），实体级节流期限 + 周期边界自动恢复 |
| cpu | `cpu.weight` | real | `set_shares` → `reweight_task_cpu_weight`，对已入队实体实时生效 |
| cpu | `cpu.stat` | real | `throttled_usec/nr_throttled/nr_periods` 在节流/恢复路径累计 |
| memory | `memory.current/peak` | real | `MemoryCss::try_charge/uncharge` + 每帧归属（PAGE_OWNERS） |
| memory | `memory.min/low/high/max` | real | min/low 存储 + 回收权重占位；high 缺页路径有界节流；max 事务式祖先链检查，拒绝走 `oom::scoped_out_of_memory` |
| memory | `memory.events` / `memory.stat` | real | high/max 事件计数；RSS 分项统计 |
| memory | `memory.swap.*` | stub | swap 未实现，接口占位（与边界声明一致） |
| io | `io.max` | real | 100ms slice 周期结算 token 桶；GenDisk/ext4 适配器/MBR 扫描全部分发路径前置限速 |
| io | `io.stat` | real | 完成时按提交任务捕获的 cgroup 记账 |
| io | `io.weight` | real | 权重存储（比例仲裁未接入派发排序） |
| pids | `pids.max` / `pids.current` / `pids.events` | real | fork can_fork 拦截 + 层级计数 + max 事件 |
| cpuset | `cpuset.cpus[.effective]` / `cpuset.mems` | real | 继承 + online-aware effective mask；affinity 空交集报错 |
| freezer | `cgroup.freeze`（经 core） | real | queued/current/sleeping 三态冻结标志 + 幂等解冻唤醒 |

## 与 Linux 6.6 的明确边界

- CPU 带宽闭环：fair 实体在 `update_current`（`entity_tick`/`pick_next_task` 的共同计费点）按 `cpu.max` 扣减配额，配额耗尽时写入实体级节流期限（`bandwidth_throttled_until`，扁平模型下 Linux "throttle cfs_rq" 的等价物——实体保留记账但 `entity_eligible`/pick 路径不再选中它），rq 记录最早期限并在周期边界唤醒调度器；下次计费时 `refresh_period` 滚动周期、补充配额并自动解除节流（unthrottle），`throttled_time`/`nr_throttled`/`nr_periods` 累计进 `cpu.stat`。与 Linux 6.6 的剩余差距：尚无 per-task_group 的 `cfs_rq`/组实体挂入运行队列树（`CfsRunQueue::throttled`/`throttled_count` 组级状态机与组路径入队/出队终止逻辑已对齐 `enqueue_task_fair`/`dequeue_task_fair`，但当前无触发者）；配额是 cgroup 全局池而非 per-CPU slice 分发（无 slack 分发/period timer，周期推进为惰性）；祖先 cgroup 配额不逐级计费；`cpu.stat` 用户/系统时间未按执行现场分类。
- fair/RT 支持边界：`cpu.max` 与 `cpu.weight` 只作用于 fair 调度类；SCHED_RT 任务不参与 CFS 带宽控制（与 Linux CFS bandwidth 仅约束 fair class 一致，RT 由独立的 rt bandwidth 机制管理——DragonOS 中为 `RealtimeScheduler` 的 rq 级 `rt.is_throttled()` 节流，不读 cpu 控制器状态）；`cpu.weight` 写入对非 fair 任务为 no-op；DL 调度类未实现。
- memory 计费具备祖先 max 原子检查、每帧归属记录（释放方与计费方解耦）和 cgroup 范围 OOM（复用 oom.rs 状态机）；`memory.high` 节流与 `memory.max` OOM 都在缺页路径执行，计费路径本身不睡眠不回收。与 Linux 6.6 的剩余差距：无 per-memcg LRU/回收目标（复用全局回收器）、无 charge 迁移（move_charge_at_immigrate）、无 memory.min/low 保护加权、无 swap 实际计费（swap.* 文件仅为接口占位）、slab 对象级记账并入页级计费（无 obj_cgroup）。
- IO 限速在块设备分发前端以同步等待实现（提交任务睡眠到 slice 边界后重查），不是 Linux block layer 的延迟派发队列（throtl_service_queue/pending_timer）与 bio 层分层节流；超大单次传输按“每 slice 首个请求放行”保证前进；io.stat 在完成时记账，无 per-cpu rstat 聚合。
- 本轮审计修复的明确范围：修正 `memory.min/low` 默认值、`cpu.max` quota 下界、freezer 生命周期/迁移/唤醒、memcg 内核分配绕过与 scoped OOM。以下审计项仍未实现，不能以“已支持”描述：`cpu.stat` user/system 现场分类与父级 rstat 聚合、`io.weight` 仲裁及 `io.stat` 层级聚合、cpuset 用户 affinity 在迁移后的独立恢复与 `cpuset.mems` NUMA 放置、`write_procs` 多控制器失败回滚；原因分别是调度现场分类、块设备公平队列、任务 affinity 双掩码、NUMA 分配器和迁移事务尚未存在。
- CSS-set token 是每个 cgroup 的稳定生命周期标识，不是 Linux 完整的跨 cgroup CSS 集合哈希去重；threaded 状态机覆盖基础文件语义，未实现完整 threaded domain CSS 传播。
- freezer 已避免直接伪造唤醒已有 sleeper，但尚未覆盖 Linux job-control freezer 的全部信号/停机交互。
- pids、cpuset 和 cgroup2 文件接口已覆盖本轮调用链；hugetlb、rdma、misc 等未注册控制器仍不属于当前移植范围。

## 核心数据结构

### 1. CgroupSubsysState (CSS)

```rust
/// 每个 cgroup 每个控制器的状态
pub struct CgroupSubsysState {
    /// 所属 cgroup
    cgroup: Weak<CgroupNode>,
    /// 控制器 ID
    ss_id: SubsysId,
    /// 引用计数（未来升级为 percpu_ref）
    refcnt: AtomicUsize,
    /// 父 CSS（用于层级继承）
    parent: Option<Arc<CgroupSubsysState>>,
    /// 子 CSS 列表
    children: RwLock<Vec<Arc<CgroupSubsysState>>>,
    /// CSS ID（用于快速查找）
    id: usize,
    /// 标志位（CSS_ONLINE 等）
    flags: AtomicU32,
    /// rstat per-CPU 状态（P0.5 引入）
    rstat_cpu: Option<Arc<CssRstatCpu>>,
    /// 控制器私有数据（Box<dyn Any>）
    private: RwLock<Option<Box<dyn Any + Send + Sync>>>,
}
```

### 2. CgroupSubsys trait

```rust
/// 控制器接口（对齐 Linux cgroup_subsys）
pub trait CgroupSubsys: Send + Sync {
    /// 控制器名称（"cpu"/"memory"/"pids"）
    fn name(&self) -> &'static str;
    
    /// 控制器 ID
    fn id(&self) -> SubsysId;
    
    /// 是否为 domain 控制器（影响 "no internal process" 约束）
    fn is_domain_controller(&self) -> bool {
        false
    }
    
    /// 是否支持 threaded 模式
    fn threaded(&self) -> bool {
        false
    }
    
    // === 生命周期钩子 ===
    
    /// 分配 CSS（创建 cgroup 时调用）
    fn css_alloc(&self, parent: Option<&Arc<CgroupSubsysState>>) 
        -> Result<Box<dyn Any + Send + Sync>, SystemError>;
    
    /// CSS 上线（cgroup 创建完成后调用）
    fn css_online(&self, css: &Arc<CgroupSubsysState>) -> Result<(), SystemError> {
        Ok(())
    }
    
    /// CSS 下线（rmdir 开始时调用）
    fn css_offline(&self, css: &Arc<CgroupSubsysState>) {
        // 默认空实现
    }
    
    /// 释放 CSS（引用计数归零后调用）
    fn css_free(&self, css: Arc<CgroupSubsysState>);
    
    /// rstat 刷新回调（per-CPU 统计聚合）
    fn css_rstat_flush(&self, css: &Arc<CgroupSubsysState>, cpu: u32) {
        // 默认空实现
    }
    
    // === 迁移钩子 ===
    
    /// 检查是否可以迁移任务（可失败）
    fn can_attach(&self, taskset: &CgroupTaskset) -> Result<(), SystemError> {
        Ok(())
    }
    
    /// 取消迁移（can_attach 失败后回滚）
    fn cancel_attach(&self, taskset: &CgroupTaskset) {
        // 默认空实现
    }
    
    /// 提交迁移（不可失败，在持有 css_set_lock 时调用）
    fn attach(&self, taskset: &CgroupTaskset) {
        // 默认空实现
    }
    
    // === fork/exit 钩子 ===
    
    /// fork 前检查（可失败，例如 pids.max）
    fn can_fork(&self, task: Arc<ProcessControlBlock>, cset: &Arc<CssSet>) -> Result<(), SystemError> {
        Ok(())
    }
    
    /// 取消 fork（can_fork 失败后回滚）
    fn cancel_fork(&self, task: Arc<ProcessControlBlock>, cset: &Arc<CssSet>) {
        // 默认空实现
    }
    
    /// fork 完成（子进程已加入 cgroup）
    fn fork(&self, task: Arc<ProcessControlBlock>) {
        // 默认空实现
    }
    
    /// 进程退出
    fn exit(&self, task: Arc<ProcessControlBlock>) {
        // 默认空实现
    }
    
    // === 文件接口 ===
    
    /// 返回控制器的 cgroup 文件定义
    fn cftypes(&self) -> &'static [CfType];
}
```

### 3. CssSet（重构）

```rust
/// 任务归属的 CSS 集合（对齐 Linux css_set）
pub struct CssSet {
    /// 每个控制器的 CSS 指针数组
    subsys: [Option<Arc<CgroupSubsysState>>; MAX_CGROUP_SUBSYS],
    /// 引用计数（任务数）
    refcnt: AtomicUsize,
    /// 默认层级的 cgroup（用于路径显示）
    dfl_cgrp: Weak<CgroupNode>,
    /// threaded 模式的 domain css_set
    dom_cset: Option<Arc<CssSet>>,
    /// 关联的任务列表（需要全局 css_set_lock 保护）
    tasks: Mutex<Vec<RawPid>>,
    /// 哈希表链接（用于 find_css_set）
    hlist_node: Mutex<Option<usize>>,
    /// 迁移状态（临时）
    mg_src_cgrp: RwLock<Option<Weak<CgroupNode>>>,
    mg_dst_cgrp: RwLock<Option<Weak<CgroupNode>>>,
    mg_dst_cset: RwLock<Option<Arc<CssSet>>>,
}
```

### 4. CgroupNode 重构

```rust
pub struct CgroupNode {
    id: usize,
    name: String,
    parent: Option<Weak<CgroupNode>>,
    children: RwLock<HashMap<String, Arc<CgroupNode>>>,
    
    /// 任务直接使用 css_set 索引，不再存储 RawPid
    tasks: RwLock<HashSet<RawPid>>,  // 保留用于快速查找
    
    /// 启用的控制器掩码（subtree_control 文件）
    subtree_control: RwLock<u32>,
    
    /// 每个控制器的 CSS（替代原有的 cpu/memory/freezer 字段）
    subsys: [RwLock<Option<Arc<CgroupSubsysState>>>; MAX_CGROUP_SUBSYS],
    
    /// cgroup 标志（CGRP_FREEZE/CGRP_FROZEN 等）
    flags: AtomicU32,
    
    /// rstat per-CPU 基础统计
    rstat_base_cpu: Option<Arc<CgroupRstatBaseCpu>>,
    
    /// threaded 模式的 domain cgroup
    dom_cgrp: RwLock<Option<Weak<CgroupNode>>>,
    
    /// 设备 BPF 状态（保留，已实现）
    device_bpf: RwLock<DeviceBpfState>,
}
```

### 5. 控制器注册表

```rust
/// 控制器 ID 枚举
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubsysId {
    Cpu = 0,
    Memory = 1,
    Pids = 2,
    Freezer = 3,
    Cpuset = 4,
    Io = 5,
    // 预留到 16 个
}

pub const MAX_CGROUP_SUBSYS: usize = 16;

lazy_static! {
    /// 全局控制器注册表
    static ref CGROUP_SUBSYS: [Option<Arc<dyn CgroupSubsys>>; MAX_CGROUP_SUBSYS] = {
        let mut arr: [Option<Arc<dyn CgroupSubsys>>; MAX_CGROUP_SUBSYS] = Default::default();
        arr[SubsysId::Cpu as usize] = Some(Arc::new(CpuSubsys::new()));
        arr[SubsysId::Memory as usize] = Some(Arc::new(MemorySubsys::new()));
        arr[SubsysId::Pids as usize] = Some(Arc::new(PidsSubsys::new()));
        arr[SubsysId::Freezer as usize] = Some(Arc::new(FreezerSubsys::new()));
        arr
    };
}

/// 获取控制器实例
pub fn cgroup_subsys(id: SubsysId) -> Option<&'static Arc<dyn CgroupSubsys>> {
    CGROUP_SUBSYS[id as usize].as_ref()
}
```

> 下列 Phase 条目是原始设计路线，保留用于记录决策；当前实现状态以上面的“当前实现”为准。`css_set`、旧状态结构和独立 `registry.rs` 未作为运行时依赖保留。

## 迁移路线

### Phase 0.1: 定义框架（本次）
- 创建 `cgroup/subsys.rs`：定义 `CgroupSubsys` trait
- 创建 `cgroup/css.rs`：实现 `CgroupSubsysState`
- 创建 `cgroup/css_set.rs`：实现 `CssSet` 和哈希表

### Phase 0.2: 重构 CgroupNode
- 将 `cpu/memory/freezer/pids_*` 字段替换为 `subsys[]` 数组
- 实现 `cgroup_css(cgrp, ss_id)` 查找函数
- 迁移 `set_cpu_weight` 等方法到 CSS 访问模式

### Phase 0.3: 迁移 Pids 控制器
- 实现 `PidsSubsys` 满足 `CgroupSubsys` trait
- 定义 `PidsCss` 私有数据结构（max/counter/events）
- 实现 `can_fork`/`fork`/`exit` 钩子
- 迁移 `pids.max`/`pids.current` 文件到 `cftypes`

### Phase 0.4: 迁移其他控制器
- CPU: `CpuSubsys`（权重/带宽存储，调度器集成在 P1）
- Memory: `MemorySubsys`（限制存储，记账在 P3）
- Freezer: `FreezerSubsys`（freeze 标志，冻结逻辑在 P2）

### Phase 0.5: 实现 css_set 哈希表
- 实现 `find_css_set(old_cset, target_cgrp)`
- 实现 `css_set_hash()` 哈希函数
- 在 `cgroup_migrate` 中使用 css_set 查找

## 兼容性策略

### 文件接口兼容
- 保留现有 `files.rs` 的 `CgroupFileType` 枚举
- 新增 `cftype_to_filetype()` 转换函数
- `Cgroup2Inode::read/write` 路由到控制器的 cftype 回调

### 任务迁移兼容
- 保留 `CgroupNode::add_task/remove_task` 用于快速查找
- 内部调用 `css_set_move_task` 更新 css_set 归属
- `cgroup_migrate_vet_dst` 改为调用控制器的 `can_attach`

### 向后兼容
- 旧代码通过 `cgroup_css(node, SubsysId::Cpu)` 获取 CSS
- CSS 的 `private` 字段存储旧的 `CgroupCpuState` 等结构体
- 文件读写通过 `css.private.downcast_ref::<CpuCss>()` 访问

## 依赖项

### P0 阶段需要
- ✅ `Arc<dyn CgroupSubsys>`：trait object 用于控制器多态
- ✅ `Box<dyn Any>`：CSS 私有数据类型擦除
- ✅ `HashMap<u64, Arc<CssSet>>`：css_set 哈希表
- ❌ `percpu_ref`：P0 使用 `AtomicUsize`，P0.5 升级
- ❌ `per-CPU 变量`：P0 跳过 rstat，P0.5 补充

### 后续阶段
- P1: 调度器集成（CFS 组调度、带宽限流）
- P2: 进程冻结循环（`__refrigerator`）
- P3: 内存子系统（page_counter、LRU、OOM）
- P4+: IO/cpuset/其他控制器

## 测试计划

### 单元测试
- `css_set_hash()` 哈希冲突率
- `find_css_set()` 复用率（相同 CSS 集合）
- 控制器注册表初始化

### 集成测试
- Pids 控制器迁移后 `pids.max` 行为不变
- 任务迁移触发 `can_attach`/`attach` 钩子
- fork 触发 `can_fork`/`fork` 钩子

### 回归测试
- 现有 gvisor 测试套件
- 容器启动（runc/docker）
- cgroup 文件读写（systemd）

## 风险与缓解

### 风险1：引用计数死锁
- **问题**：CSS 相互引用导致 Arc 循环
- **缓解**：parent 使用 `Weak<CgroupSubsysState>`

### 风险2：哈希表性能
- **问题**：css_set 查找成为热路径瓶颈
- **缓解**：使用 `DashMap` 无锁并发哈希表

### 风险3：向后兼容性破坏
- **问题**：旧代码依赖 `node.cpu.read()` 直接访问
- **缓解**：分阶段迁移，保留兼容层 3 个版本

## 代码量估算

| 模块 | 新增行数 | 修改行数 |
|------|---------|---------|
| subsys.rs | ~300 | 0 |
| css.rs | ~400 | 0 |
| css_set.rs | ~500 | 0 |
| core.rs 重构 | ~200 | ~500 |
| pids 控制器迁移 | ~300 | ~200 |
| cpu/memory/freezer 迁移 | ~600 | ~300 |
| 测试 | ~400 | 0 |
| **总计** | **~2700** | **~1000** |

## 时间估算

- P0.1 框架定义：3 天
- P0.2 CgroupNode 重构：4 天
- P0.3 Pids 迁移：3 天
- P0.4 其他控制器迁移：4 天
- P0.5 css_set 哈希表：3 天
- 测试与调试：3 天
- **总计：20 天（4 周）**
