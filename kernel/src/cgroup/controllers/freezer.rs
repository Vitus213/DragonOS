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

    pub fn is_frozen(&self) -> bool {
        let Some(cgroup) = self.cgroup.upgrade() else {
            return false;
        };
        let nr_tasks = cgroup.subtree_task_count();
        nr_tasks != 0 && self.nr_frozen_total() == nr_tasks
    }

    /// 冻结本组任务并向全部后代传播 FREEZING_PARENT。
    /// 调用者必须持有 cgroup_accounting_lock，且本组 mask 刚从 0 变为非 0。
    fn freeze_tasks(&self, cgroup: &Arc<CgroupNode>) -> Result<(), SystemError> {
        for pid in cgroup.tasks() {
            if let Some(task) = ProcessManager::find(pid) {
                self.freeze_task(&task)?;
            }
        }
        for child in cgroup.children() {
            let Some(css) = child.css(CgroupSubsysId::Freezer) else {
                continue;
            };
            let Some(child_freezer) = css.as_any().downcast_ref::<FreezerCss>() else {
                continue;
            };
            child_freezer.propagate_parent_freezing();
        }
        Ok(())
    }

    /// 祖先发起冻结：置 FREEZING_PARENT 并冻结本子树。
    fn propagate_parent_freezing(&self) {
        let old = self.freeze_mask.fetch_or(FREEZING_PARENT, Ordering::AcqRel);
        if old != 0 {
            return;
        }
        let Some(cgroup) = self.cgroup.upgrade() else {
            return;
        };
        let _ = self.freeze_tasks(&cgroup);
    }


    /// Request the scheduler to enter the refrigerator at a safe boundary.
    ///
    /// 锁序：task_lock（freezer）→ pi_lock（任务）。pi_lock 与 wakeup() 串行化
    /// FROZEN 置位与唤醒检查，关闭“冻结途中被唤醒逃逸”的窗口。
    fn freeze_task(&self, task: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        if task.flags().contains(ProcessFlags::KTHREAD | ProcessFlags::NOFREEZE) {
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
    fn unfreeze_tasks(&self, cgroup: &Arc<CgroupNode>) -> Result<(), SystemError> {
        for pid in cgroup.tasks() {
            if let Some(task) = ProcessManager::find(pid) {
                self.unfreeze_task(&task)?;
            }
        }
        for child in cgroup.children() {
            let Some(css) = child.css(CgroupSubsysId::Freezer) else {
                continue;
            };
            let Some(child_freezer) = css.as_any().downcast_ref::<FreezerCss>() else {
                continue;
            };
            child_freezer.retract_parent_freezing();
        }
        Ok(())
    }

    /// 祖先解冻：清除 FREEZING_PARENT；若本组不再有任何冻结请求则解冻本子树。
    fn retract_parent_freezing(&self) {
        let old = self
            .freeze_mask
            .fetch_and(!FREEZING_PARENT, Ordering::AcqRel);
        if old & (FREEZING_SELF | FREEZING_PARENT) != FREEZING_PARENT {
            // SELF 仍在（本组自己要求冻结）或本来就没冻结：状态不变。
            return;
        }
        let Some(cgroup) = self.cgroup.upgrade() else {
            return;
        };
        let _ = self.unfreeze_tasks(&cgroup);
    }


    /// Clear freezer flags and wake only tasks blocked by the refrigerator.
    fn unfreeze_task(&self, task: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        use crate::process::ProcessState;
        use crate::sched::{enqueue_task_on_cpu, OnRq, WakeupFlags};

        let (cpu, on_rq) = {
            let _guard = self.task_lock.lock();
            let flags = task.flags().load();
            if !flags.intersects(ProcessFlags::FROZEN | ProcessFlags::FREEZING) {
                return Ok(());
            }

            let was_frozen = flags.contains(ProcessFlags::FROZEN);
            task.flags()
                .remove(ProcessFlags::FROZEN | ProcessFlags::FREEZING);
            if was_frozen && self.dec_frozen_count() {
                self.update_ancestor_counts(-1);
            }

            let pid = task.raw_pid();
            let wake = {
                let mut wakeable = self.wakeable_tasks.lock();
                wakeable
                    .iter()
                    .position(|candidate| *candidate == pid)
                    .map(|index| {
                        wakeable.swap_remove(index);
                    })
                    .is_some()
            };
            if !wake {
                return Ok(());
            }

            (
                task.sched_info().on_cpu(),
                *task.sched_info().on_rq.lock_irqsave(),
            )
        };

        // The refrigerator always enters from Runnable.  If it was already
        // sleeping, no wakeable marker exists and its state is untouched.
        if !task.sched_info().state().is_blocked() {
            return Ok(());
        }
        task.sched_info().set_state(ProcessState::Runnable);
        if on_rq == OnRq::None {
            if let Some(cpu) = cpu {
                enqueue_task_on_cpu(task, cpu, WakeupFlags::empty(), false);
            }
        }
        Ok(())
    }

    fn update_ancestor_counts(&self, delta: isize) {
        let Some(cgroup) = self.cgroup.upgrade() else {
            return;
        };
        let Some(parent) = cgroup.parent() else {
            return;
        };
        if let Some(css) = parent.css(CgroupSubsysId::Freezer) {
            if let Some(parent_css) = css.as_any().downcast_ref::<FreezerCss>() {
                if delta > 0 {
                    parent_css
                        .nr_frozen_descendants
                        .fetch_add(delta as usize, Ordering::AcqRel);
                } else {
                    parent_css
                        .nr_frozen_descendants
                        .fetch_update(
                            Ordering::AcqRel,
                            Ordering::Acquire,
                            |count| Some(count.saturating_sub((-delta) as usize)),
                        )
                        .ok();
                }
                parent_css.update_ancestor_counts(delta);
            }
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

    fn cgroup(&self) -> Arc<CgroupNode> {
        self.cgroup.upgrade().expect("freezer cgroup dropped")
    }

    fn flags(&self) -> CssFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, flags: CssFlags) {
        *self.flags.lock() = flags;
    }

    fn can_attach(
        &self,
        _tasks: &[Arc<ProcessControlBlock>],
    ) -> Result<(), SystemError> {
        // Linux 6.6 的 freezer 对 attach 没有任何限制：
        // 迁入 FROZEN/FREEZING cgroup 的任务在 attach() 中被冻结
        // （对应 cgroup_freeze_attach），迁出者保持冻结状态直至解冻。
        Ok(())
    }

    fn attach(&self, tasks: &[Arc<ProcessControlBlock>]) {
        if self.freeze_requested() {
            for task in tasks {
                let _ = self.freeze_task(task);
            }
        }
    }

    fn fork(&self, task: &Arc<ProcessControlBlock>) {
        if self.freeze_requested() {
            let _ = self.freeze_task(task);
        }
    }

    fn exit(&self, task: &Arc<ProcessControlBlock>) {
        let _guard = self.task_lock.lock();
        let flags = task.flags().load();
        task.flags().remove(ProcessFlags::FROZEN | ProcessFlags::FREEZING);
        if flags.contains(ProcessFlags::FROZEN) && self.dec_frozen_count() {
            self.update_ancestor_counts(-1);
        }
        let pid = task.raw_pid();
        let mut wakeable = self.wakeable_tasks.lock();
        if let Some(index) = wakeable.iter().position(|candidate| *candidate == pid) {
            wakeable.swap_remove(index);
        }
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
        _parent: Option<&Arc<dyn CgroupSubsysState>>,
        cgroup: &Arc<CgroupNode>,
    ) -> Result<Arc<dyn CgroupSubsysState>, SystemError> {
        Ok(FreezerCss::new(Arc::downgrade(cgroup)))
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

/// __refrigerator - 进入冻结状态
///
/// 当任务设置了 FREEZING 标志时，在调度时调用此函数进入冻结状态。
/// 对应 Linux 的 `__refrigerator()`
pub fn __refrigerator(task: &Arc<ProcessControlBlock>) -> bool {
    use crate::process::ProcessState;

    if !task.flags().contains(ProcessFlags::FREEZING) {
        return false;
    }

    if task.flags().contains(ProcessFlags::KTHREAD | ProcessFlags::NOFREEZE) {
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

    let mut entered = false;
    {
        let _pi = task.sched_info().pi_lock_irqsave();
        // The transition is completed only here, never in freeze_task().
        task.flags().insert(ProcessFlags::FROZEN);
        task.flags().remove(ProcessFlags::FREEZING);

        // A task which was already sleeping remains asleep.  Only a task that
        // entered from Runnable is changed to the freezer's blocking state and
        // recorded for a later wakeup.
        if task.sched_info().state().is_runnable() {
            task.sched_info().set_state(ProcessState::Blocked(false));
            entered = true;
        }
    }
    if entered {
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
