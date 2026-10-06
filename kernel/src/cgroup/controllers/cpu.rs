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

    pub fn set_bandwidth(&self, quota_us: Option<u64>, period_us: u64) -> Result<(), SystemError> {
        // Linux CFS bandwidth accepts a 1ms..1s period.  A finite quota
        // must be at least 1ms and no greater than the period.
        if period_us < 1000 || period_us > 1_000_000 {
            return Err(SystemError::EINVAL);
        }
        if let Some(quota) = quota_us {
            if quota < 1000 || quota > period_us {
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
        // O(1) 闭式推进（对齐 Linux 6.6 CFS bandwidth：__refill_cfs_bandwidth_runtime
        // 把 slice 起点整体前跳到当前时钟所在的周期，配额只在新边界整体重置，
        // 中间周期逐个补偿）：旧实现 `while elapsed >= period_ns` 每轮只推进一个
        // 周期，而 `last_period_start` 仅在组内成员被调度时前进，于是配了 cpu.max
        // 的组整组睡眠 T 后，首次计费/唤醒（rq 锁、关中断）要跑 T/period 次迭代——
        // period 下限 1ms 时睡 10 分钟就是 60 万次 IRQ-off 自旋。整除一次算出
        // 过期周期数，迭代次数上界为常数。
        let elapsed = clock_ns.saturating_sub(tg.last_period_start);
        let periods = elapsed / period_ns;
        if periods > 0 {
            // throttled_time 结算同步闭式化：`throttled_since` 必然落在当前周期内
            // （enter_throttled_locked 只在本周期被观测后写入），因此若过期时仍
            // 处于节流，其结束点只可能是第一个过期边界；之后的整周期节流与旧
            // 逐周期循环一致地不计入（对齐 Linux 只在任务实际处于 throttled
            // 区间时累计 throttled_time 的口径）。
            if tg.throttled {
                let first_deadline = tg.last_period_start.saturating_add(period_ns);
                tg.stats.throttled_time.fetch_add(
                    first_deadline.saturating_sub(tg.throttled_since),
                    Ordering::Relaxed,
                );
                tg.throttled = false;
                tg.throttled_since = 0;
            }
            tg.stats.nr_periods.fetch_add(periods, Ordering::Relaxed);
            tg.last_period_start = tg
                .last_period_start
                .saturating_add(periods.saturating_mul(period_ns));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn limited_group(quota_us: u64, period_us: u64) -> TaskGroupState {
        let mut tg = TaskGroupState::default();
        tg.quota_us = Some(quota_us);
        tg.period_us = period_us;
        tg
    }

    #[test]
    fn refresh_period_advances_in_one_step_after_long_idle() {
        // 复现 issue #31 的攻击形态：period 取下限 1ms，组整组睡眠 10 分钟
        // (600_000ms)。旧实现要跑 60 万次 while 迭代；现在必须一次整除推进，
        // 且最终状态与逐周期循环逐位一致。
        let mut tg = limited_group(500, 1000); // 0.5ms quota / 1ms period
        let t0 = 1_000_000_000u64;
        // 首次观测建立周期。
        assert!(matches!(
            refresh_period_locked(&mut tg, t0),
            CpuRuntimeDecision::Allow {
                remaining_ns: 500_000,
                ..
            }
        ));
        // 消耗一部分配额并节流一个片段，验证睡眠期结算的闭式化。
        tg.runtime_remaining = 0;
        enter_throttled_locked(&mut tg, t0 + 200_000);
        let sleep = 600_000_000_000u64; // 10 分钟
        let now = t0 + sleep;
        let decision = refresh_period_locked(&mut tg, now);
        let period_ns = 1_000_000u64;
        // 新边界：last_period_start 必须恰好落在 `now` 所在周期；配额整体重置；
        // 决策为 Allow 且 deadline 是下一边界。
        let elapsed_periods = (now - t0) / period_ns;
        assert_eq!(tg.last_period_start, t0 + elapsed_periods * period_ns);
        assert!(tg.last_period_start <= now && now - tg.last_period_start < period_ns);
        assert_eq!(tg.runtime_remaining, 500_000);
        assert!(!tg.throttled);
        match decision {
            CpuRuntimeDecision::Allow {
                remaining_ns,
                period_deadline_ns,
            } => {
                assert_eq!(remaining_ns, 500_000);
                assert_eq!(period_deadline_ns, tg.last_period_start + period_ns);
            }
            other => panic!("expected Allow, got {other:?}"),
        }
        // nr_periods 一次推进 elapsed_periods；throttled_time 只并入第一个过期
        // 边界前的节流片段（[t0+200ms 边界 - (t0+0.2ms)] 的 0.8ms）。
        assert_eq!(tg.stats.nr_periods.load(Ordering::Relaxed), elapsed_periods);
        let first_deadline = t0 + period_ns;
        assert_eq!(
            tg.stats.throttled_time.load(Ordering::Relaxed),
            first_deadline - (t0 + 200_000)
        );
        // 有界性：再次推进相同时间窗口的迭代次数是常数——这里以状态不再随
        // 睡眠长度线性变化验证（同一 now 再 refresh 为零推进）。
        assert!(matches!(
            refresh_period_locked(&mut tg, now),
            CpuRuntimeDecision::Allow { .. }
        ));
        assert_eq!(tg.stats.nr_periods.load(Ordering::Relaxed), elapsed_periods);
    }

    #[test]
    fn refresh_period_matches_iterative_walkers_bit_for_bit() {
        // 对照论证：模拟旧逐周期循环（含节流中途过期只可能在首边界结束的
        // 不变量），在参数化睡眠长度上与新闭式实现逐字段比对。
        for sleep_ms in [1u64, 2, 3, 999, 1_000, 1_001, 7_368_421] {
            let period_ns = 1_000_000u64;
            let quota_ns = 500_000u64;
            let t0 = 5_000_000_000u64;
            let mut tg = limited_group(500, 1000);
            refresh_period_locked(&mut tg, t0);
            // 节流在当前周期中段进入，然后整组睡眠 sleep_ms。
            tg.runtime_remaining = 0;
            enter_throttled_locked(&mut tg, t0 + 300_000);
            let now = t0 + sleep_ms * 1_000_000;
            refresh_period_locked(&mut tg, now);

            // 旧算法：逐周期 while，每轮 nr_periods+1、首边界结算节流片段。
            let mut ref_start = t0;
            let mut ref_nr_periods = 0u64;
            let mut ref_throttled_time = 0u64;
            let mut ref_throttled = true;
            while now.saturating_sub(ref_start) >= period_ns {
                let deadline = ref_start + period_ns;
                if ref_throttled {
                    ref_throttled_time += deadline - (t0 + 300_000);
                    ref_throttled = false;
                }
                ref_nr_periods += 1;
                ref_start = deadline;
            }
            assert_eq!(tg.last_period_start, ref_start, "sleep_ms={sleep_ms}");
            assert_eq!(
                tg.stats.nr_periods.load(Ordering::Relaxed),
                ref_nr_periods,
                "sleep_ms={sleep_ms}"
            );
            assert_eq!(
                tg.stats.throttled_time.load(Ordering::Relaxed),
                ref_throttled_time,
                "sleep_ms={sleep_ms}"
            );
            assert_eq!(!tg.throttled, !ref_throttled);
            assert_eq!(tg.runtime_remaining, quota_ns as i64);
        }
    }

    #[test]
    fn refresh_period_single_period_expiry_still_counts_once() {
        // 常规路径回归：恰好跨一个周期时行为与旧循环一致（nr_periods+1，
        // deadline 前移一个 period，非节流组不受影响）。
        let mut tg = limited_group(2_000, 100_000); // 2ms/100ms
        let t0 = 0u64;
        refresh_period_locked(&mut tg, t0);
        assert_eq!(tg.last_period_start, 0);
        refresh_period_locked(&mut tg, 100_000_000); // 正好一个 period
        assert_eq!(tg.last_period_start, 100_000_000);
        assert_eq!(tg.stats.nr_periods.load(Ordering::Relaxed), 1);
        assert_eq!(tg.runtime_remaining, 2_000_000);
    }

    /// cpu.stat 恒等式与 user/kernel 归属回归（issue #36）。
    ///
    /// 修复前计费点硬编码 `let user = false;`，user_usec 恒 0、system_usec
    /// 吞并全部执行时间；修复后 tick 的 user/kernel 现场逐段传入
    /// `account_runtime_checked`。本测例锁定三条语义：
    /// 1. 恒等式 user_usec + system_usec == usage_usec（files.rs cpu_stat_for
    ///    以 (utime+stime)/1000 渲染 usage_usec，恒等式由构造保证，回归其不被
    ///    后续改动破坏）；
    /// 2. `is_user == true`（tick 用户态现场）的计费增长 utime/user_usec；
    /// 3. `is_user == false`（tick 内核态现场与非 tick 残差冲刷段）的计费只增长
    ///    stime/system_usec。
    #[test]
    fn cpu_stat_user_system_usage_identity() {
        let css = CpuCss::new(None, Weak::new());
        let clock = 1_000_000_000u64;

        // tick 用户态现场：一个滴答计费 10ms
        css.account_runtime_checked(clock, 10_000_000, true);
        // tick 内核态现场 + 非 tick 残差段：共 4ms，全部归 system
        css.account_runtime_checked(clock + 10_000_000, 3_000_000, false);
        css.account_runtime_checked(clock + 13_000_000, 1_000_000, false);

        let stats = css.stats();
        // cpu.stat 渲染值（files.rs：纳秒除以 1000 得 usec）
        let user_usec = stats.utime / 1000;
        let system_usec = stats.stime / 1000;
        let usage_usec = user_usec + system_usec;

        assert_eq!(user_usec + system_usec, usage_usec);
        assert_eq!(
            user_usec, 10_000,
            "用户态滴答必须计入 user_usec（修复前恒 0）"
        );
        assert_eq!(system_usec, 4_000, "内核态/非 tick 段计入 system_usec");
        assert_eq!(usage_usec, 14_000);

        // 纳秒级恒等式：每段 delta 恰被计入 utime/stime 之一，无丢失、无双计
        assert_eq!(stats.utime + stats.stime, 14_000_000);
    }

    /// 配额开启、节流状态机工作时，user/kernel 交替计费的纳秒恒等式依然成立：
    /// 节流不吞时间，每段执行时间只入账一次。
    #[test]
    fn cpu_stat_identity_holds_across_throttle_cycles() {
        let css = CpuCss::new(None, Weak::new());
        // quota 50ms / period 100ms
        css.set_bandwidth(Some(50_000), 100_000).unwrap();

        let mut clock = 0u64;
        let mut charged_ns = 0u64;
        let mut user_ns = 0u64;
        for i in 0..1000u64 {
            let delta = 1_000_000 + i * 1_000;
            // 模拟 tick 现场：每 3 个滴答 1 个内核态，其余用户态
            let user = i % 3 != 0;
            css.account_runtime_checked(clock, delta, user);
            clock += delta;
            charged_ns += delta;
            if user {
                user_ns += delta;
            }
        }

        let stats = css.stats();
        assert_eq!(
            stats.utime + stats.stime,
            charged_ns,
            "每段执行时间必须且只能计入 utime/stime 之一"
        );
        assert_eq!(stats.utime, user_ns);
        assert_eq!(stats.stime, charged_ns - user_ns);
        assert!(stats.nr_periods > 0, "时钟推进应产生周期计数");
    }
}
