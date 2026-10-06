use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use system_error::SystemError;

use crate::libs::spinlock::SpinLock;
use crate::process::{ProcessControlBlock, ProcessFlags, ProcessManager};

use super::super::{
    core::CgroupNode,
    subsys::{CgroupSubsys, CgroupSubsysId, CgroupSubsysState, CssFlags},
};

/// 冻结请求来源位。对应 Linux 的 CGROUP_FREEZING_SELF / CGROUP_FREEZING_PARENT：
/// SELF 表示本组的 cgroup.freeze 写入，PARENT 表示冻结由祖先传播而来；
/// 两者任一置位即处于冻结请求状态，读 cgroup.freeze 返回 1。
const FREEZING_SELF: u8 = 1 << 0;
const FREEZING_PARENT: u8 = 1 << 1;

/// Freezer 控制器状态
///
/// 对应 Linux 的 `struct freezer`
#[derive(Debug)]
pub struct FreezerCss {
    /// 所属 cgroup 节点
    cgroup: Weak<CgroupNode>,
    /// 状态标志
    flags: SpinLock<CssFlags>,
    /// Serialize task flag transitions with the scheduler refrigerator.
    task_lock: SpinLock<()>,
    ///
    /// A task which was already sleeping when freezing was requested must
    /// remain asleep when thawed; only tasks in this list are wakeable.
    wakeable_tasks: SpinLock<Vec<crate::process::RawPid>>,
    /// 冻结请求位掩码：FREEZING_SELF（本组 cgroup.freeze=1 发起）或
    /// FREEZING_PARENT（祖先传播）。对应 Linux 的 CGROUP_FREEZING_SELF/PARENT。
    freeze_mask: AtomicU8,

    /// 已冻结任务数
    nr_frozen_tasks: AtomicUsize,
    /// 子树中已冻结任务数
    nr_frozen_descendants: AtomicUsize,
}

impl FreezerCss {
    pub fn new(cgroup: Weak<CgroupNode>) -> Arc<Self> {
        Arc::new(Self {
            cgroup,
            flags: SpinLock::new(CssFlags::default()),
            task_lock: SpinLock::new(()),
            wakeable_tasks: SpinLock::new(Vec::new()),
            freeze_mask: AtomicU8::new(0),
            nr_frozen_tasks: AtomicUsize::new(0),
            nr_frozen_descendants: AtomicUsize::new(0),
        })
    }

    /// Create a child CSS while holding the hierarchy accounting lock.
    /// The parent request is inherited before the child becomes visible.
    pub fn new_child(
        parent: Option<&Arc<dyn CgroupSubsysState>>,
        cgroup: Weak<CgroupNode>,
    ) -> Arc<Self> {
        let child = Self::new(cgroup);
        if parent.is_some_and(|state| {
            state
                .as_any()
                .downcast_ref::<FreezerCss>()
                .is_some_and(FreezerCss::freeze_requested)
        }) {
            child.freeze_mask.store(FREEZING_PARENT, Ordering::Release);
        }
        child
    }

    pub fn freeze_requested(&self) -> bool {
        self.freeze_mask.load(Ordering::Acquire) != 0
    }

    /// 写 cgroup.freeze 的入口。与迁移（write_procs）互斥于
    /// cgroup_accounting_lock，保证 tasks() 快照期间成员集合不变
    /// （对应 Linux 在 cgroup_mutex 下变更 freezer 状态）。
    pub fn set_freeze_requested(&self, value: bool) -> Result<(), SystemError> {
        let _accounting = crate::cgroup::core::cgroup_accounting_lock().lock();
        if value {
            let old = self.freeze_mask.fetch_or(FREEZING_SELF, Ordering::AcqRel);
            if old & FREEZING_SELF != 0 {
                return Ok(());
            }
            if old & FREEZING_PARENT == 0 {
                let Some(cgroup) = self.cgroup.upgrade() else {
                    return Ok(());
                };
                self.freeze_tasks(&cgroup)?;
            }
        } else {
            let old = self.freeze_mask.fetch_and(!FREEZING_SELF, Ordering::AcqRel);
            if old & FREEZING_SELF == 0 {
                return Ok(());
            }
            if old & FREEZING_PARENT == 0 {
                let Some(cgroup) = self.cgroup.upgrade() else {
                    return Ok(());
                };
                self.unfreeze_tasks(&cgroup)?;
            }
        }
        Ok(())
    }

    fn dec_frozen_count(&self) -> bool {
        self.nr_frozen_tasks
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            })
            .is_ok()
    }

    pub fn nr_frozen(&self) -> usize {
        self.nr_frozen_tasks.load(Ordering::Acquire)
    }

    pub fn nr_frozen_descendants(&self) -> usize {
        self.nr_frozen_descendants.load(Ordering::Acquire)
    }

    pub fn nr_frozen_total(&self) -> usize {
        self.nr_frozen()
            .saturating_add(self.nr_frozen_descendants())
    }

    pub fn self_freeze_requested(&self) -> bool {
        self.freeze_mask.load(Ordering::Acquire) & FREEZING_SELF != 0
    }

    pub fn is_frozen(&self) -> bool {
        let Some(cgroup) = self.cgroup.upgrade() else {
            return false;
        };
        self.freeze_requested() && self.nr_frozen_total() == cgroup.subtree_task_count()
    }
    /// 冻结本组任务并向全部后代传播 FREEZING_PARENT。
    /// 调用者必须持有 cgroup_accounting_lock，且本组 mask 刚从 0 变为非 0。
    ///
    /// #39 递归定界：旧实现 `freeze_tasks ↔ propagate_parent_freezing`
    /// 沿树深互递归，超深树按深度消耗内核栈；改为显式 worklist，语义
    /// 逐点保持——子组 mask 原为 0 才置 FREEZING_PARENT、冻结本组并继续
    /// 其子树（即原 `propagate_parent_freezing` 的 `old != 0` 短路）。
    fn freeze_tasks(&self, cgroup: &Arc<CgroupNode>) -> Result<(), SystemError> {
        self.freeze_group_tasks(cgroup)?;
        let mut pending: Vec<Arc<CgroupNode>> = cgroup.children();
        while let Some(child) = pending.pop() {
            let Some(css) = child.css(CgroupSubsysId::Freezer) else {
                continue;
            };
            let Some(child_freezer) = css.as_any().downcast_ref::<FreezerCss>() else {
                continue;
            };
            if child_freezer.mark_parent_freezing() {
                // 原 `let _ = self.freeze_tasks(...)`：子组错误只截断其自身
                // 子树的继续下探，不回抛给传播循环——逐点保持。
                if child_freezer.freeze_group_tasks(&child).is_ok() {
                    pending.extend(child.children());
                }
            }
        }
        Ok(())
    }

    /// 冻结单个组内的直属任务（后代由调用方 worklist 继续处理）。
    fn freeze_group_tasks(&self, cgroup: &Arc<CgroupNode>) -> Result<(), SystemError> {
        for pid in cgroup.tasks() {
            if let Some(task) = ProcessManager::find(pid) {
                self.freeze_task(&task)?;
            }
        }
        Ok(())
    }

    /// 置 FREEZING_PARENT；返回 mask 是否原为 0（首次进入冻结请求态）。
    fn mark_parent_freezing(&self) -> bool {
        let old = self.freeze_mask.fetch_or(FREEZING_PARENT, Ordering::AcqRel);
        old == 0
    }

    /// Request the scheduler to enter the refrigerator at a safe boundary.
    ///
    /// 锁序：task_lock（freezer）→ pi_lock（任务）。pi_lock 与 wakeup() 串行化
    /// FROZEN 置位与唤醒检查，关闭“冻结途中被唤醒逃逸”的窗口。
    fn freeze_task(&self, task: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        if task
            .flags()
            .intersects(ProcessFlags::KTHREAD | ProcessFlags::NOFREEZE)
        {
            return Ok(());
        }

        let _guard = self.task_lock.lock();
        let flags = task.flags().load();
        if flags.intersects(ProcessFlags::FROZEN | ProcessFlags::FREEZING) {
            return Ok(());
        }

        // 睡眠中的任务：在 pi_lock 临界区内直接完成 FROZEN 转换。
        // Linux 通过 fake_signal_wake_up 把它骗进 refrigerator 再睡回去；这里在
        // 等价的互斥点直接达成同一语义：保持原睡眠状态、不进 wakeable_tasks，
        // 冻结期间的唤醒被 wakeup() 的 FROZEN 检查吞掉，解冻后由原事件唤醒。
        let mut frozen_asleep = false;
        {
            let _pi = task.sched_info().pi_lock_irqsave();
            let state = task.sched_info().state();
            if state.is_blocked() {
                task.flags().insert(ProcessFlags::FROZEN);
                self.nr_frozen_tasks.fetch_add(1, Ordering::AcqRel);
                frozen_asleep = true;
            } else {
                // 可运行任务：仅置 FREEZING，等它在 __schedule() 切出路径上
                // 进入 __refrigerator()（current 需要 NEED_SCHEDULE 踢一脚）。
                task.flags().insert(ProcessFlags::FREEZING);
                if Arc::ptr_eq(task, &ProcessManager::current_pcb()) {
                    task.flags().insert(ProcessFlags::NEED_SCHEDULE);
                }
            }
        }
        if frozen_asleep {
            self.update_ancestor_counts(1);
        }
        Ok(())
    }

    /// 解冻本组任务并向后代清除 FREEZING_PARENT（SELF 仍在的后代保持冻结）。
    /// 调用者必须持有 cgroup_accounting_lock，且本组 mask 刚变为 0。
    ///
    /// #39 递归定界：旧实现 `unfreeze_tasks ↔ retract_parent_freezing`
    /// 沿树深互递归；改为显式 worklist，逐点保持原短路语义——仅当子组
    /// 的 FREEZING_PARENT 被清除后不再有任何冻结请求（原判定
    /// `old & (SELF|PARENT) == PARENT`）才解冻该组并继续下传。
    fn unfreeze_tasks(&self, cgroup: &Arc<CgroupNode>) -> Result<(), SystemError> {
        self.unfreeze_group_tasks(cgroup)?;
        let mut pending: Vec<Arc<CgroupNode>> = cgroup.children();
        while let Some(child) = pending.pop() {
            let Some(css) = child.css(CgroupSubsysId::Freezer) else {
                continue;
            };
            let Some(child_freezer) = css.as_any().downcast_ref::<FreezerCss>() else {
                continue;
            };
            if child_freezer.retract_parent_freezing() {
                // 同冻结侧：解冻错误不回抛，仅截断该子树继续下探。
                if child_freezer.unfreeze_group_tasks(&child).is_ok() {
                    pending.extend(child.children());
                }
            }
        }
        Ok(())
    }

    /// 解冻单个组内的直属任务（后代由调用方 worklist 继续处理）。
    fn unfreeze_group_tasks(&self, cgroup: &Arc<CgroupNode>) -> Result<(), SystemError> {
        for pid in cgroup.tasks() {
            if let Some(task) = ProcessManager::find(pid) {
                self.unfreeze_task(&task)?;
            }
        }
        Ok(())
    }

    /// 祖先解冻：清除 FREEZING_PARENT；返回 true 表示本组冻结请求就此
    /// 消失（SELF 不在场、PARENT 被撤），调用方应解冻本组并继续下传；
    /// false 表示 SELF 仍在或本来就没冻结，状态不变（对应原
    /// `retract_parent_freezing` 的 early-return）。
    fn retract_parent_freezing(&self) -> bool {
        let old = self
            .freeze_mask
            .fetch_and(!FREEZING_PARENT, Ordering::AcqRel);
        old & (FREEZING_SELF | FREEZING_PARENT) == FREEZING_PARENT
    }

    /// Clear freezer flags and wake tasks blocked by the refrigerator or with
    /// a wakeup saved while frozen (Linux saved-wakeup semantics).
    fn unfreeze_task(&self, task: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        let wake = {
            let _guard = self.task_lock.lock();
            let flags = task.flags().load();
            if !flags.intersects(ProcessFlags::FROZEN | ProcessFlags::FREEZING) {
                return Ok(());
            }

            let was_frozen = flags.contains(ProcessFlags::FROZEN);

            // 读取并清除 WAKE_PENDING 必须与 wakeup() 对 FROZEN 的检查
            // 共用 pi_lock；不能使用进入临界区前的 flags 快照，否则
            // wakeup() 可能在快照之后置位 WAKE_PENDING，随后被这里清掉。
            let (was_wakeable, saved_wakeup) = {
                let _pi = task.sched_info().pi_lock_irqsave();
                let saved_wakeup = task.flags().contains(ProcessFlags::WAKE_PENDING);
                task.flags().remove(
                    ProcessFlags::FROZEN | ProcessFlags::FREEZING | ProcessFlags::WAKE_PENDING,
                );
                let pid = task.raw_pid();
                (self.take_wakeable(pid), saved_wakeup)
            };

            if was_frozen && self.dec_frozen_count() {
                self.update_ancestor_counts(-1);
            }

            // refrigerator 可以从 Runnable 或事件睡眠两种现场完成冻结：
            // 前者登记在 wakeable（解冻必须唤醒），后者的唤醒仍归属原等待
            // 事件；冻结期间到达的唤醒被暂存（WAKE_PENDING），此刻重放。
            was_wakeable || saved_wakeup
        };

        if wake {
            thaw_wake(task);
        }
        Ok(())
    }

    /// 从本组 wakeable_tasks 摘除一个任务，返回它是否在场。
    /// 调用者必须持有本 css 的 task_lock。
    fn take_wakeable(&self, pid: crate::process::RawPid) -> bool {
        let mut wakeable = self.wakeable_tasks.lock();
        wakeable
            .iter()
            .position(|candidate| *candidate == pid)
            .map(|index| {
                wakeable.swap_remove(index);
            })
            .is_some()
    }

    /// 向本组 wakeable_tasks 添加一个任务（幂等）。
    /// 调用者必须持有本 css 的 task_lock。
    fn add_wakeable(&self, pid: crate::process::RawPid) {
        let mut wakeable = self.wakeable_tasks.lock();
        if !wakeable.iter().any(|candidate| *candidate == pid) {
            wakeable.push(pid);
        }
    }

    fn update_ancestor_counts(&self, delta: isize) {
        let Some(cgroup) = self.cgroup.upgrade() else {
            return;
        };
        // #39 递归定界：上溯祖先链的尾递归改为循环。逐级累加/回退
        // 冻结后代计数，访问序与更新点和原递归实现逐次相同；
        // CSS 缺失时同样停止（对应原 `if let` 不再下传）。
        let mut current = cgroup.parent();
        while let Some(node) = current {
            let Some(css) = node.css(CgroupSubsysId::Freezer) else {
                break;
            };
            let Some(parent_css) = css.as_any().downcast_ref::<FreezerCss>() else {
                break;
            };
            if delta > 0 {
                parent_css
                    .nr_frozen_descendants
                    .fetch_add(delta as usize, Ordering::AcqRel);
            } else {
                parent_css
                    .nr_frozen_descendants
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                        Some(count.saturating_sub((-delta) as usize))
                    })
                    .ok();
            }
            current = node.parent();
        }
    }
}

impl CgroupSubsysState for FreezerCss {
    fn subsys_id(&self) -> CgroupSubsysId {
        CgroupSubsysId::Freezer
    }

    fn parent(&self) -> Option<Arc<dyn CgroupSubsysState>> {
        self.cgroup
            .upgrade()
            .and_then(|cgroup| cgroup.parent())
            .and_then(|parent| parent.css(CgroupSubsysId::Freezer))
    }

    fn cgroup_node(&self) -> Option<Arc<CgroupNode>> {
        // fail-closed（issue #30）：与 subsys trait 一致，节点已随
        // rmdir 拆除时返回 None，绝不 panic。
        self.cgroup.upgrade()
    }

    fn flags(&self) -> CssFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, flags: CssFlags) {
        *self.flags.lock() = flags;
    }

    fn can_attach(&self, _tasks: &[Arc<ProcessControlBlock>]) -> Result<(), SystemError> {
        // Linux 6.6 的 freezer 对迁移没有任何限制（无 can_attach 回调）：
        // 任务换组时的冻结/解冻调整由迁移核心在换组点直接调用
        // cgroup_freezer_migrate_task() 完成。
        Ok(())
    }

    fn attach(&self, _tasks: &[Arc<ProcessControlBlock>]) {
        // 迁移调整由迁移核心（write_procs）在任务换组时直接调用
        // cgroup_freezer_migrate_task(task, src, dst) 完成，与 Linux 6.6
        // 一致：cgroup core 在 css_set_move_task() 后直接调用该函数按
        // 目标组状态冻结或解冻，freezer 子系统不注册 attach 回调。
        // freezer 状态在 css（cgroup）一侧，通用 attach 回调拿不到源组，
        // 无法完成源组计数/wakeable 清理，故不在此处做任何事。
    }

    fn fork(&self, task: &Arc<ProcessControlBlock>) {
        if self.freeze_requested() {
            let _ = self.freeze_task(task);
        }
    }

    fn exit(&self, task: &Arc<ProcessControlBlock>) {
        let _guard = self.task_lock.lock();
        let flags = task.flags().load();
        task.flags()
            .remove(ProcessFlags::FROZEN | ProcessFlags::FREEZING | ProcessFlags::WAKE_PENDING);
        if flags.contains(ProcessFlags::FROZEN) && self.dec_frozen_count() {
            self.update_ancestor_counts(-1);
        }
        self.take_wakeable(task.raw_pid());
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// Freezer 控制器定义
#[derive(Debug)]
pub struct FreezerController;

impl FreezerController {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl CgroupSubsys for FreezerController {
    fn id(&self) -> CgroupSubsysId {
        CgroupSubsysId::Freezer
    }

    fn name(&self) -> &'static str {
        "freezer"
    }

    fn css_alloc(
        &self,
        parent: Option<&Arc<dyn CgroupSubsysState>>,
        cgroup: &Arc<CgroupNode>,
    ) -> Result<Arc<dyn CgroupSubsysState>, SystemError> {
        Ok(FreezerCss::new_child(parent, Arc::downgrade(cgroup)))
    }

    fn css_free(&self, _css: &Arc<dyn CgroupSubsysState>) {}

    fn dfl_cftypes(&self) -> alloc::vec::Vec<super::super::subsys::CfType> {
        alloc::vec::Vec::new()
    }
}

/// 初始化 Freezer 控制器
pub fn init_freezer_controller() {
    let controller = FreezerController::new();
    crate::cgroup::subsys::register_subsys(controller);
}

/// `__refrigerator()` 的状态裁决（纯函数，单元测试入口）：
///
/// - Runnable → `EnterBlocked`：从可运行态真正入冰箱——改判为冰箱阻塞态
///   并登记 wakeable（解冻必须唤醒它，对应 Linux 任务在 freeze trap 处
///   `cgroup_enter_frozen()` 后 `schedule()` 挂起、解冻时 wake_up_process）；
/// - Blocked(_) → `EnterInPlace`：任务此刻本就阻塞在事件上（`mark_sleep()`
///   之后、deactivate 之前被调度点截获），与 `freeze_task()` 的睡眠分支
///   同构地就地完成冻结——只置 FROZEN 与计数，不改状态、不登记
///   wakeable（唤醒仍由其原事件负责，冻结期到达的唤醒经 WAKE_PENDING 重放）；
/// - Stopped / Exited → `Defer`：不接管状态、不置 FROZEN、不计数。Stopped
///   的去留由 SIGCONT 决定，Exited 交给 exit 钩子清理；保留 FREEZING，
///   任务下一次到达 `__schedule()` 时再收敛。期间 `is_frozen()` 保守报假
///   ——这正是"计数 == 实际阻塞集合"的要求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefrigeratorEntry {
    EnterBlocked,
    EnterInPlace,
    Defer,
}

fn classify_refrigerator_entry(state: crate::process::ProcessState) -> RefrigeratorEntry {
    use crate::process::ProcessState;
    match state {
        ProcessState::Runnable => RefrigeratorEntry::EnterBlocked,
        ProcessState::Blocked(_) => RefrigeratorEntry::EnterInPlace,
        _ => RefrigeratorEntry::Defer,
    }
}

/// __refrigerator - 进入冻结状态
///
/// 当任务设置了 FREEZING 标志时，在调度时调用此函数进入冻结状态。
/// 对应 Linux 的 `__refrigerator()` 与 cgroup freezer 的 freeze trap
/// （`JOBCTL_TRAP_FREEZE` → `cgroup_enter_frozen()`）。
///
/// 计数纪律（issue #32 的根因所在）：FROZEN 标志、nr_frozen_tasks 的增、
/// wakeable 登记必须在同一个 pi_lock 临界区内一并完成，三者一一对应。
/// 旧实现对已睡眠任务也先置 FROZEN、无条件计数并返回 true，而任务状态
/// 并未转入冰箱；随后 `__schedule()` 的 `signal_pending_state` 把
/// Blocked(true)+pending 信号恢复成 Runnable，任务带着 FROZEN 与虚增计数
/// 继续运行——`is_frozen()` 误报冻结完成，且残留 FROZEN 使后续每次唤醒
/// 被 `wakeup()` 暂存吞掉，直至解冻才恢复。Linux 用 TASK_FROZEN（无任何
/// 唤醒位）与 trap 优先级保证冻结态不可被信号恢复、计数只归真实入箱者；
/// 本实现等价收口：裁决未完成的转移一律不置位、不计数、返回 false。
pub fn __refrigerator(task: &Arc<ProcessControlBlock>) -> bool {
    use crate::process::ProcessState;

    if !task.flags().contains(ProcessFlags::FREEZING) {
        return false;
    }

    if task
        .flags()
        .intersects(ProcessFlags::KTHREAD | ProcessFlags::NOFREEZE)
    {
        task.flags().remove(ProcessFlags::FREEZING);
        return false;
    }

    let cgroup = task.task_cgroup_node();
    let Some(freezer_css) = cgroup.css(CgroupSubsysId::Freezer) else {
        task.flags().remove(ProcessFlags::FREEZING);
        return false;
    };
    let Some(freezer) = freezer_css.as_any().downcast_ref::<FreezerCss>() else {
        task.flags().remove(ProcessFlags::FREEZING);
        return false;
    };

    // 锁序：task_lock（freezer）→ pi_lock（任务）。
    // 本函数只在 __schedule() 的切出路径上作用于 current 任务（此时它不可能
    // 并发地出现在 wakeup() 的处理集合里），pi_lock 进一步把 FROZEN 置位与
    // state 转换做成原子对，使 wakeup() 的 FROZEN 检查不存在撕裂窗口。
    let _guard = freezer.task_lock.lock();
    if !task.flags().contains(ProcessFlags::FREEZING) {
        return false;
    }

    let entry = {
        let _pi = task.sched_info().pi_lock_irqsave();
        let verdict = classify_refrigerator_entry(task.sched_info().state());
        match verdict {
            RefrigeratorEntry::EnterBlocked => {
                // 从 Runnable 真正入冰箱：置位、清请求、转入冰箱阻塞态。
                task.flags().insert(ProcessFlags::FROZEN);
                task.flags().remove(ProcessFlags::FREEZING);
                task.sched_info().set_state(ProcessState::Blocked(false));
            }
            RefrigeratorEntry::EnterInPlace => {
                // 已在事件睡眠：就地完成冻结，状态原样保留，稍后由本轮
                // __schedule() 的 deactivate 分支挂起。
                task.flags().insert(ProcessFlags::FROZEN);
                task.flags().remove(ProcessFlags::FREEZING);
            }
            RefrigeratorEntry::Defer => {}
        }
        verdict
    };
    if entry == RefrigeratorEntry::Defer {
        // 未完成转移：FREEZING 保留，下轮调度再收敛；不置 FROZEN、不计数。
        return false;
    }
    if entry == RefrigeratorEntry::EnterBlocked {
        let pid = task.raw_pid();
        let mut wakeable = freezer.wakeable_tasks.lock();
        if !wakeable.iter().any(|candidate| *candidate == pid) {
            wakeable.push(pid);
        }
    }
    freezer.nr_frozen_tasks.fetch_add(1, Ordering::AcqRel);
    freezer.update_ancestor_counts(1);

    true
}

/// 解冻唤醒收尾：把一个已清除 FROZEN 标志的阻塞任务恢复 runnable。
///
/// 适用对象：
/// - refrigerator 冻结的任务（原本 runnable，被置为 Blocked）；
/// - 冻结期间收到暂存唤醒（WAKE_PENDING）的睡眠任务。
/// 原样睡眠且无暂存唤醒的任务不经过本函数（解冻后保持原睡眠）。
///
/// 对应 wakeup() 的 ttwu 路径：归还 uninterruptible/iowait 计数、
/// 置 Runnable、重新入队。调用者必须已释放 freezer task_lock 与 pi_lock
///（本函数会获取 rq 锁，而 __schedule 持 rq 锁时可能反过来获取
/// freezer task_lock，持锁调用会构成 ABBA）。
fn thaw_wake(task: &Arc<ProcessControlBlock>) {
    use crate::process::ProcessState;
    use crate::sched::{enqueue_task_on_cpu, OnRq, WakeupFlags};

    // 等待目标任务彻底切出 CPU（对应 wakeup() 的 wait_until_not_running）：
    // __refrigerator() 置 FROZEN 后任务可能仍在本轮 __schedule() 中，
    // 此时入队会与其 dequeue 路径竞争，造成 runnable 任务掉出运行队列。
    task.sched_info().wait_until_not_running();

    let was_uninterruptible = matches!(task.sched_info().state(), ProcessState::Blocked(false));
    if !task.sched_info().state().is_blocked() {
        return;
    }

    // 归还睡眠期间贡献的 iowait 计数（对应 wakeup() 的 dec_nr_iowait）。
    if task.flags().contains(ProcessFlags::IN_IOWAIT) {
        if let Some(prev) = task.sched_info().on_cpu() {
            let prev_rq = crate::sched::cpu_rq(prev.data() as usize);
            let (prev_rq, _guard) = prev_rq.self_lock();
            prev_rq.dec_nr_iowait();
        }
    }

    task.sched_info().set_state(ProcessState::Runnable);
    if *task.sched_info().on_rq.lock_irqsave() == OnRq::None {
        if let Some(cpu) = task.sched_info().on_cpu() {
            enqueue_task_on_cpu(task, cpu, WakeupFlags::empty(), was_uninterruptible);
        }
    }
}

/// cgroup_freezer_migrate_task - 任务在 cgroup 间迁移时的 freezer 调整
///
/// 对应 Linux 6.6 的 `cgroup_freezer_migrate_task(task, src, dst)`：
/// cgroup core 在任务换组点（css_set_move_task 之后）直接调用本函数，
/// 按源/目标组的冻结请求状态调整任务状态与两边的冻结计数：
/// - 任务已冻结：冻结计数从源组迁出；目标组请求冻结则迁入并保持冻结
///   （wakeable 标记一并迁移），否则解冻——refrigerator 冻结的与冻结期
///   收到暂存唤醒的任务立即唤醒，原样睡眠的任务保持原睡眠；
/// - 任务未冻结：目标组请求冻结则冻结之；否则清除可能残留的 FREEZING，
///   防止任务迁入后在 __refrigerator() 里被幽灵请求冻死在非冻结组。
///
/// 调用时机：任务已调用 set_task_cgroup_node() 换入目标组之后、通用
/// attach 回调之前；调用者必须持有 cgroup_accounting_lock（与
/// set_freeze_requested/unfreeze_tasks 串行）。
pub fn cgroup_freezer_migrate_task(
    task: &Arc<ProcessControlBlock>,
    src: &Arc<CgroupNode>,
    dst: &Arc<CgroupNode>,
) {
    // 内核线程不允许冻结（对应 Linux 的 PF_KTHREAD 检查）。
    if task
        .flags()
        .intersects(ProcessFlags::KTHREAD | ProcessFlags::NOFREEZE)
    {
        return;
    }

    let Some(src_css) = src.css(CgroupSubsysId::Freezer) else {
        return;
    };
    let Some(src_freezer) = src_css.as_any().downcast_ref::<FreezerCss>() else {
        return;
    };
    let Some(dst_css) = dst.css(CgroupSubsysId::Freezer) else {
        return;
    };
    let Some(dst_freezer) = dst_css.as_any().downcast_ref::<FreezerCss>() else {
        return;
    };

    // 同时锁住源/目标两组的 task_lock（按地址定序防 ABBA，源/目标为
    // 同一 css 时只锁一次）：__refrigerator() 的串行化点在任务当前
    // cgroup 的 css 上，换组前后分别可能是源组或目标组，只锁一侧会与
    // “读旧节点、锁旧 css”的实例竞争，出现计数迁出早于计数迁入的
    // 负计数/泄漏窗口。
    let (first, second) = if (src_freezer as *const FreezerCss as usize)
        <= (dst_freezer as *const FreezerCss as usize)
    {
        (src_freezer, dst_freezer)
    } else {
        (dst_freezer, src_freezer)
    };
    let _g1 = first.task_lock.lock();
    let _g2 = (!core::ptr::eq(first, second)).then(|| second.task_lock.lock());

    let flags = task.flags().load();
    let frozen = flags.contains(ProcessFlags::FROZEN);
    if !frozen
        && !flags.contains(ProcessFlags::FREEZING)
        && !src_freezer.freeze_requested()
        && !dst_freezer.freeze_requested()
    {
        return;
    }

    if frozen {
        // 冻结计数从源组摘除，wakeable 残留一并清出（对应 Linux 的
        // cgroup_dec_frozen_cnt(src)）。
        let pid = task.raw_pid();
        let was_wakeable = src_freezer.take_wakeable(pid);
        if src_freezer.dec_frozen_count() {
            src_freezer.update_ancestor_counts(-1);
        }

        if dst_freezer.freeze_requested() {
            // 目标组请求冻结：保持冻结，计数与 wakeable 标记迁入目标组
            //（对应 Linux 的 cgroup_inc_frozen_cnt(dst)）。
            dst_freezer.nr_frozen_tasks.fetch_add(1, Ordering::AcqRel);
            dst_freezer.update_ancestor_counts(1);
            if was_wakeable {
                dst_freezer.add_wakeable(pid);
            }
            return;
        }

        // 目标组未请求冻结：解冻。refrigerator 冻结的任务与冻结期间收到
        // 暂存唤醒的任务立即唤醒（对应 Linux 的 wake_up_process）；
        // 原样睡眠的任务保持原睡眠，由其原本的事件/定时器继续唤醒。
        let saved_wakeup = {
            let _pi = task.sched_info().pi_lock_irqsave();
            let saved_wakeup = task.flags().contains(ProcessFlags::WAKE_PENDING);
            task.flags()
                .remove(ProcessFlags::FROZEN | ProcessFlags::FREEZING | ProcessFlags::WAKE_PENDING);
            saved_wakeup
        };
        drop(_g2);
        drop(_g1);
        if was_wakeable || saved_wakeup {
            thaw_wake(task);
        }
        return;
    }

    if dst_freezer.freeze_requested() {
        // 目标组请求冻结而任务尚未冻结：冻结之（freeze_task 自取
        // task_lock，须在释放本函数所持锁后调用）。
        drop(_g2);
        drop(_g1);
        let _ = dst_freezer.freeze_task(task);
        return;
    }

    // 任务带着 FREEZING（尚未到达 __refrigerator）迁入非冻结组：
    // 清除请求，__refrigerator() 在 task_lock 下复查 FREEZING 后会放弃
    // 冻结，任务得以继续运行。
    if flags.contains(ProcessFlags::FREEZING) {
        let _pi = task.sched_info().pi_lock_irqsave();
        task.flags()
            .remove(ProcessFlags::FREEZING | ProcessFlags::WAKE_PENDING);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessState;

    /// issue #32 复现场景的裁决回归：可中断睡眠（pending 信号在场）的
    /// FREEZING 任务绝不允许"置 FROZEN + 计数而状态不动"的半程转移——
    /// 那正是旧实现被 `signal_pending_state` 抬回 Runnable 后计数虚增、
    /// FROZEN 残留吞唤醒的根因。修复后的形态只有两种合法终态：
    /// - 就地完成冻结（EnterInPlace）：FROZEN/计数与"任务确实阻塞"同拍；
    /// - 本轮不接管（Defer）：不置位、不计数，FREEZING 留待下轮。
    #[test]
    fn sleeping_task_never_gets_uncounted_frozen_transition() {
        assert_eq!(
            classify_refrigerator_entry(ProcessState::Blocked(true)),
            RefrigeratorEntry::EnterInPlace
        );
        assert_eq!(
            classify_refrigerator_entry(ProcessState::Blocked(false)),
            RefrigeratorEntry::EnterInPlace
        );
    }

    /// 只有真正从 Runnable 入箱的任务才改变状态并登记 wakeable
    ///（对应 Linux freeze trap 处 cgroup_enter_frozen 后 schedule 挂起，
    /// 解冻必须唤醒它）。
    #[test]
    fn runnable_task_enters_refrigerator_as_wakeable_blocked() {
        assert_eq!(
            classify_refrigerator_entry(ProcessState::Runnable),
            RefrigeratorEntry::EnterBlocked
        );
    }

    /// Stopped 的去留由 SIGCONT 决定、Exited 交给 exit 钩子，均不得被
    /// 冰箱接管计数；保留 FREEZING 等任务下一次到达 `__schedule()` 再收敛，
    /// 期间 `is_frozen()` 保守报假，保证"计数 == 实际阻塞集合"。
    #[test]
    fn stopped_and_exited_defer_freeze_without_counting() {
        assert_eq!(
            classify_refrigerator_entry(ProcessState::Stopped),
            RefrigeratorEntry::Defer
        );
        assert_eq!(
            classify_refrigerator_entry(ProcessState::Exited(0)),
            RefrigeratorEntry::Defer
        );
    }

    /// 不变式：凡完成 FROZEN 转移的裁决（EnterBlocked/EnterInPlace），
    /// 任务都真实阻塞在睡眠态——计数只归实际入冰箱者；Defer 则相反，
    /// 计数必须不动。signal_wake 守卫（sched/mod.rs）与 undo_mark_sleep
    /// 守卫（process/manager/sched.rs）共同保证冻结态不可被信号/撤销
    /// 恢复回 Runnable，即"被计数的任务不会逃出阻塞集合"。
    #[test]
    fn counted_transitions_are_exactly_the_blocked_set() {
        let all_states = [
            ProcessState::Runnable,
            ProcessState::Blocked(true),
            ProcessState::Blocked(false),
            ProcessState::Stopped,
            ProcessState::Exited(0),
        ];
        for state in all_states {
            let counted = classify_refrigerator_entry(state) != RefrigeratorEntry::Defer;
            // 被计数者要么本轮从 Runnable 转入 Blocked(false)（EnterBlocked
            // 改判后阻塞），要么本就阻塞（EnterInPlace 状态不动）。
            let blocked_after = match state {
                ProcessState::Runnable => true,
                ProcessState::Blocked(_) => true,
                _ => false,
            };
            assert_eq!(counted, blocked_after, "state={state:?} 计数与阻塞集合失配");
        }
    }
}
