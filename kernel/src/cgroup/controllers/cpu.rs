//! CPU 控制器实现
//!
//! 对应 Linux `kernel/sched/core.c` 的 cgroup 部分

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU64, Ordering};
use system_error::SystemError;

use crate::libs::spinlock::SpinLock;
use super::super::{
    core::CgroupNode,
    subsys::{CgroupSubsys, CgroupSubsysId, CgroupSubsysState, CssFlags},
};

/// CPU 控制器的 CSS 状态
///
/// 对应 Linux `struct task_group`
#[derive(Debug)]
pub struct CpuCss {
    parent: Option<Weak<dyn CgroupSubsysState>>,
    /// 关联的 cgroup 节点
    cgroup: Weak<CgroupNode>,
    /// CSS 标志
    flags: SpinLock<CssFlags>,
    /// 底层 TaskGroup
    task_group: Arc<SpinLock<TaskGroupState>>,
}

/// TaskGroup 状态包装
///
/// 包含权重、带宽限制和统计信息
#[derive(Debug)]
pub struct TaskGroupState {
    /// CPU 权重（对应 cpu.weight，范围 1-10000）
    shares: u64,
    /// 带宽配额（微秒/周期），None 表示 max
    quota_us: Option<u64>,
    /// 带宽周期（微秒）
    period_us: u64,
    /// 当前周期是否已用第一个观测时钟初始化
    period_initialized: bool,
    /// 当前周期开始时间（纳秒）
    last_period_start: u64,
    /// 当前周期剩余配额（纳秒）
    runtime_remaining: i64,
    /// 当前是否已经进入节流。该状态必须独立于 runtime_remaining，
    /// 否则每个 tick 都会把同一次节流重复计数。
    throttled: bool,
    /// 进入当前节流区间的时间（纳秒）
    throttled_since: u64,
    /// 统计信息
    stats: CpuStats,
}
/// CPU 统计信息
///
/// 对应 Linux `struct task_group_cputime`
#[derive(Debug, Default)]
pub struct CpuStats {
    /// 用户态时间（纳秒）
    utime: AtomicU64,
    /// 系统态时间（纳秒）
    stime: AtomicU64,
    /// 节流次数
    nr_periods: AtomicU64,
    /// 节流周期数
    nr_throttled: AtomicU64,
    /// 节流总时间（纳秒）
    throttled_time: AtomicU64,
}

/// Result of charging a task's CPU runtime against a cgroup quota.
///
/// `Throttle` is emitted only on the transition into a throttled interval;
/// subsequent zero-delta checks keep returning it with `newly_throttled == false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuRuntimeDecision {
    Unlimited,
    Allow {
        remaining_ns: u64,
        period_deadline_ns: u64,
    },
    Throttle {
        period_deadline_ns: u64,
        newly_throttled: bool,
    },
}

impl Default for TaskGroupState {
    fn default() -> Self {
        Self {
            shares: 100, // Linux cgroup v2 CPU_WEIGHT_DFL
            quota_us: None,
            period_us: 100_000, // 100ms，Linux CFS_BANDWIDTH_SLICE_US
            period_initialized: false,
            last_period_start: 0,
            runtime_remaining: 0,
            throttled: false,
            throttled_since: 0,
            stats: CpuStats::default(),
    }
}
}

impl CpuCss {
    pub fn new(
        parent: Option<Arc<dyn CgroupSubsysState>>,
        cgroup: Weak<CgroupNode>,
    ) -> Arc<Self> {
        Arc::new(Self {
            parent: parent.map(|p| Arc::downgrade(&p)),
            cgroup,
            flags: SpinLock::new(CssFlags::default()),
            task_group: Arc::new(SpinLock::new(TaskGroupState::default())),
        })
    }

    /// 获取 shares（权重）
    pub fn shares(&self) -> u64 {
        self.task_group.lock().shares
    }

    /// 设置 shares
    pub fn set_shares(&self, shares: u64) -> Result<(), SystemError> {
        // Linux 限制范围 CGROUP_WEIGHT_MIN(1) 到 CGROUP_WEIGHT_MAX(10000)
        if !(1..=10000).contains(&shares) {
            return Err(SystemError::EINVAL);
        }
        
        let old_shares = self.task_group.lock().shares;
        if old_shares == shares {
            return Ok(()); // 无变化，直接返回
        }
        
        self.task_group.lock().shares = shares;
        
        // 接入调度器：更新所有关联任务的权重
        if let Some(cgroup) = self.cgroup.upgrade() {
            update_cgroup_tasks_weight(&cgroup, shares)?;
        }
        
        Ok(())
    }

    /// 获取带宽限制（quota, period）
    pub fn bandwidth(&self) -> (Option<u64>, u64) {
        let tg = self.task_group.lock();
        (tg.quota_us, tg.period_us)
    }

    /// 设置带宽限制
    pub fn set_bandwidth(&self, quota_us: Option<u64>, period_us: u64) -> Result<(), SystemError> {
        // Linux 限制：period 范围 1ms-1s，quota 不超过 1s
        if period_us < 1000 || period_us > 1_000_000 {
            return Err(SystemError::EINVAL);
        }
        if let Some(q) = quota_us {
            if q > 1_000_000 {
                return Err(SystemError::EINVAL);
            }
        }

        let mut tg = self.task_group.lock();
        // 配置变更从一个干净的周期开始，避免旧 quota 的剩余量或
        // throttled 状态泄漏到新配置。统计计数则按 Linux 语义保留。
        tg.quota_us = quota_us;
        tg.period_us = period_us;
        tg.period_initialized = false;
        tg.last_period_start = 0;
        tg.runtime_remaining = 0;
        tg.throttled = false;
        tg.throttled_since = 0;
        Ok(())
    }

    /// 获取统计信息
    pub fn stats(&self) -> CpuStatsSnapshot {
        let tg = self.task_group.lock();
        CpuStatsSnapshot {
            utime: tg.stats.utime.load(Ordering::Relaxed),
            stime: tg.stats.stime.load(Ordering::Relaxed),
            nr_periods: tg.stats.nr_periods.load(Ordering::Relaxed),
            nr_throttled: tg.stats.nr_throttled.load(Ordering::Relaxed),
            throttled_time: tg.stats.throttled_time.load(Ordering::Relaxed),
        }
    }

    /// Advance quota state without charging runtime.
    pub fn refresh_period(&self, clock_ns: u64) -> CpuRuntimeDecision {
        let mut tg = self.task_group.lock();
        refresh_period_locked(&mut tg, clock_ns)
    }

    /// Query whether the group is throttled at `clock_ns`.
    pub fn is_throttled(&self, clock_ns: u64) -> bool {
        matches!(
            self.refresh_period(clock_ns),
            CpuRuntimeDecision::Throttle { .. }
        )
    }

    /// Return `(remaining runtime, period deadline)` for a limited group.
    pub fn remaining_runtime(&self, clock_ns: u64) -> Option<(u64, u64)> {
        let mut tg = self.task_group.lock();
        if tg.quota_us.is_none() {
            return None;
        }
        let _ = refresh_period_locked(&mut tg, clock_ns);
        let period_ns = tg.period_us.saturating_mul(1000);
        Some((
            tg.runtime_remaining.max(0) as u64,
            tg.last_period_start.saturating_add(period_ns),
        ))
    }

    /// Charge runtime and report whether this call entered throttling.
    pub fn try_consume_runtime(&self, clock_ns: u64, delta_ns: u64) -> CpuRuntimeDecision {
        let mut tg = self.task_group.lock();
        let state = refresh_period_locked(&mut tg, clock_ns);
        if tg.quota_us.is_none() || matches!(state, CpuRuntimeDecision::Throttle { .. }) {
            return state;
        }

        tg.runtime_remaining = tg
            .runtime_remaining
            .saturating_sub(delta_ns.min(i64::MAX as u64) as i64);
        let deadline = tg.last_period_start.saturating_add(tg.period_us * 1000);
        if tg.runtime_remaining <= 0 {
            enter_throttled_locked(&mut tg, clock_ns);
            CpuRuntimeDecision::Throttle {
                period_deadline_ns: deadline,
                newly_throttled: true,
            }
        } else {
            CpuRuntimeDecision::Allow {
                remaining_ns: tg.runtime_remaining as u64,
                period_deadline_ns: deadline,
            }
        }
    }

    /// Check the group's bandwidth without charging runtime, advancing any
    /// elapsed period first. This is the scheduler-side entry used on wakeup
    /// and cgroup-migration paths (Linux `check_enqueue_throttle` ->
    /// `account_cfs_rq_runtime(cfs_rq, 0)`): it returns the active period
    /// deadline while the group is throttled and `None` once runtime is
    /// available again.
    pub fn check_bandwidth_throttle(&self, clock_ns: u64) -> Option<u64> {
        match self.try_consume_runtime(clock_ns, 0) {
            CpuRuntimeDecision::Throttle {
                period_deadline_ns,
                ..
            } => Some(period_deadline_ns),
            CpuRuntimeDecision::Unlimited | CpuRuntimeDecision::Allow { .. } => None,
        }
    }

    /// Account process CPU time and charge the cgroup in one serialized step.
    pub fn account_runtime_checked(
        &self,
        clock_ns: u64,
        delta_ns: u64,
        is_user: bool,
    ) -> CpuRuntimeDecision {
        let mut tg = self.task_group.lock();
        if is_user {
            tg.stats.utime.fetch_add(delta_ns, Ordering::Relaxed);
        } else {
            tg.stats.stime.fetch_add(delta_ns, Ordering::Relaxed);
        }
        drop(tg);
        self.try_consume_runtime(clock_ns, delta_ns)
    }

    /// Legacy accounting entry point. New scheduler code should use
    /// `account_runtime_checked` so that the clock and transition are atomic.
    pub fn account_runtime(&self, delta_ns: u64, is_user: bool) {
        let mut tg = self.task_group.lock();
        if is_user {
            tg.stats.utime.fetch_add(delta_ns, Ordering::Relaxed);
        } else {
            tg.stats.stime.fetch_add(delta_ns, Ordering::Relaxed);
        }
        if tg.quota_us.is_some() && !tg.throttled {
            tg.runtime_remaining = tg
                .runtime_remaining
                .saturating_sub(delta_ns.min(i64::MAX as u64) as i64);
            if tg.runtime_remaining <= 0 {
                let period_start = tg.last_period_start;
                enter_throttled_locked(&mut tg, period_start);
            }
        }
    }

    /// 记录节流时间（任务被节流停止时调用）
    pub fn account_throttled_time(&self, delta_ns: u64) {
        self.task_group
            .lock()
            .stats
            .throttled_time
            .fetch_add(delta_ns, Ordering::Relaxed);
    }
}

fn refresh_period_locked(tg: &mut TaskGroupState, clock_ns: u64) -> CpuRuntimeDecision {
    let quota_ns = match tg.quota_us {
        Some(quota_us) => quota_us.saturating_mul(1000),
        None => return CpuRuntimeDecision::Unlimited,
    };
    let period_ns = tg.period_us.saturating_mul(1000);
    if !tg.period_initialized {
        tg.period_initialized = true;
        tg.last_period_start = clock_ns;
        tg.runtime_remaining = quota_ns.min(i64::MAX as u64) as i64;
        tg.throttled = false;
    } else {
        while clock_ns.saturating_sub(tg.last_period_start) >= period_ns {
            let deadline = tg.last_period_start.saturating_add(period_ns);
            if tg.throttled {
                tg.stats
                    .throttled_time
                    .fetch_add(deadline.saturating_sub(tg.throttled_since), Ordering::Relaxed);
                tg.throttled = false;
                tg.throttled_since = 0;
            }
            tg.stats.nr_periods.fetch_add(1, Ordering::Relaxed);
            tg.last_period_start = deadline;
            tg.runtime_remaining = quota_ns.min(i64::MAX as u64) as i64;
        }
    }
    let deadline = tg.last_period_start.saturating_add(period_ns);
    if tg.throttled {
        CpuRuntimeDecision::Throttle {
            period_deadline_ns: deadline,
            newly_throttled: false,
        }
    } else {
        CpuRuntimeDecision::Allow {
            remaining_ns: tg.runtime_remaining.max(0) as u64,
            period_deadline_ns: deadline,
        }
    }
}

fn enter_throttled_locked(tg: &mut TaskGroupState, clock_ns: u64) {
    if !tg.throttled {
        tg.throttled = true;
        tg.throttled_since = clock_ns;
        tg.stats.nr_throttled.fetch_add(1, Ordering::Relaxed);
    }
}


/// 更新 cgroup 内所有任务的权重
fn update_cgroup_tasks_weight(cgroup: &Arc<CgroupNode>, shares: u64) -> Result<(), SystemError> {
    use crate::process::ProcessManager;

    for pid in cgroup.tasks() {
        if let Some(task) = ProcessManager::find(pid) {
            crate::sched::reweight_task_cpu_weight(&task, shares)?;
        }
    }
    Ok(())
}


/// 统计信息快照（用于读取）
#[derive(Debug, Clone, Copy)]
pub struct CpuStatsSnapshot {
    pub utime: u64,
    pub stime: u64,
    pub nr_periods: u64,
    pub nr_throttled: u64,
    pub throttled_time: u64,
}

impl CgroupSubsysState for CpuCss {
    fn subsys_id(&self) -> CgroupSubsysId {
        CgroupSubsysId::Cpu
    }

    fn cgroup(&self) -> Arc<CgroupNode> {
        self.cgroup.upgrade().expect("cpu cgroup dropped")
    }

    fn parent(&self) -> Option<Arc<dyn CgroupSubsysState>> {
        self.parent
            .as_ref()
            .and_then(|parent| parent.upgrade())
    }

    fn flags(&self) -> CssFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, flags: CssFlags) {
        *self.flags.lock() = flags;
    }

    fn css_online(&self) -> Result<(), SystemError> {
        Ok(())
    }

    fn css_offline(&self) -> Result<(), SystemError> {
        Ok(())
    }

    fn can_attach(
        &self,
        _tasks: &[Arc<crate::process::ProcessControlBlock>],
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn attach(&self, tasks: &[Arc<crate::process::ProcessControlBlock>]) {
        let shares = self.shares();
        for task in tasks {
            let _ = crate::sched::reweight_task_cpu_weight(task, shares);
            // Linux task_change_group_fair() also re-allocates bandwidth on
            // the new cfs_rq: drop any stale throttle deadline from the old
            // group and apply the new group's quota state.
            crate::sched::task_change_group_cpu_bandwidth(task);
        }
    }

    fn fork(&self, task: &Arc<crate::process::ProcessControlBlock>) {
        let _ = crate::sched::reweight_task_cpu_weight(task, self.shares());
    }

    fn exit(&self, _task: &Arc<crate::process::ProcessControlBlock>) {}

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// CPU 控制器定义
#[derive(Debug)]
pub struct CpuController;

impl CpuController {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl CgroupSubsys for CpuController {
    fn id(&self) -> CgroupSubsysId {
        CgroupSubsysId::Cpu
    }

    fn name(&self) -> &'static str {
        "cpu"
    }

    fn css_alloc(
        &self,
        parent: Option<&Arc<dyn CgroupSubsysState>>,
        cgroup: &Arc<CgroupNode>,
    ) -> Result<Arc<dyn CgroupSubsysState>, SystemError> {
        Ok(CpuCss::new(parent.cloned(), Arc::downgrade(cgroup)))
    }

    fn css_free(&self, _css: &Arc<dyn CgroupSubsysState>) {}

}


/// 初始化 CPU 控制器
pub fn init_cpu_controller() {
    let controller = CpuController::new();
    crate::cgroup::subsys::register_subsys(controller);
}
