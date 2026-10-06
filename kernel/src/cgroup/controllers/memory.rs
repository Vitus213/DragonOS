/// Memory Controller - 基于 Linux 7.0-rc5 mm/memcontrol.c
///
/// 实现 cgroup v2 内存控制，包括：
/// - memory.current/peak/min/low/high/max：用量与限额
/// - memory.events：OOM/low/high/max/oom_kill 事件计数
/// - memory.stat：详细统计信息
/// - memory.swap.current/max/peak/events：swap 用量与限额
///
/// 参照：
/// - Linux: mm/memcontrol.c (6218行)
/// - 结构：struct mem_cgroup (include/linux/memcontrol.h:202)
use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use system_error::SystemError;

use crate::{
    arch::MMArch,
    cgroup::{
        core::CgroupNode,
        subsys::{CfType, CfTypeFlags, CgroupSubsys, CgroupSubsysId, CgroupSubsysState, CssFlags},
    },
    libs::spinlock::SpinLock,
    mm::{page::PageReclaimer, MemoryManagementArch},
};
const PAGE_SIZE: usize = MMArch::PAGE_SIZE;

/// Serializes hierarchical charge transactions.  Holding this lock across the
/// limit checks and updates makes a charge all-or-nothing for the whole CSS
/// ancestry, rather than only for the leaf CSS.
///
/// 中断纪律（issue #28，全序图 `kernel/src/cgroup/LOCK_ORDER.md` §1.1）：
/// 本锁及其内层的 `MemoryCss::inner`/`flags` 一律 `lock_irqsave()` 获取——
/// 硬中断的页帧释放路径会经 `mm::memcg::memcg_free_uncharge` 单独取得
/// 本锁族（free 两段式：PAGE_OWNERS 内只摘归属，放锁后才进到这里），
/// IRQ-on 持有者会被同 CPU 硬中断重进非重入 CAS 自旋锁而死锁。
/// 临界区内禁止睡眠/分配/回调页分配器/获取调度器锁；与 PAGE_OWNERS
/// 互不嵌套（不变式 I3）。
static MEMORY_CHARGE_LOCK: SpinLock<()> = SpinLock::new(());

/// Memory 控制器状态
///
/// 对应 Linux: struct mem_cgroup
#[derive(Debug)]
pub struct MemoryCss {
    /// 父节点
    parent: Option<Weak<dyn CgroupSubsysState>>,
    /// 所属 cgroup 节点
    cgroup: Weak<CgroupNode>,
    /// 状态标志
    flags: SpinLock<CssFlags>,
    /// 内存计数器
    inner: SpinLock<MemoryCssInner>,
    /// Set when a charge crossed `memory.high` on this CSS; cleared by the
    /// fault-path throttle (`mm::memcg::memcg_handle_over_high`).  This keeps
    /// the charge path free of sleeping/reclaim work while still throttling
    /// the cgroup's own tasks on their next fault (Linux: the memcg throttle
    /// runs in `mem_cgroup_handle_over_high()`, not in `try_charge`).
    high_trip: AtomicBool,
}

#[derive(Debug)]
struct MemoryCssInner {
    /// 当前内存用量（页数）
    usage: u64,
    /// 峰值用量（页数）
    peak: u64,
    /// memory.min 限额（页数，None = 0）
    min: Option<u64>,
    /// memory.low 限额（页数，None = 0）
    low: Option<u64>,
    /// memory.high 限额（页数，None = max）
    high: Option<u64>,
    /// memory.max 限额（页数，None = 无限制）
    max: Option<u64>,
    /// swap 用量（页数）
    swap_usage: u64,
    /// swap 峰值（页数）
    swap_peak: u64,
    /// memory.swap.high 限额
    swap_high: Option<u64>,
    /// memory.swap.max 限额
    swap_max: Option<u64>,
    /// memory.events 计数器
    events: MemoryEvents,
    /// memory.oom.group：是否杀死整个组
    oom_group: bool,
}

#[derive(Debug, Default)]
struct MemoryEvents {
    low: AtomicU64,
    high: AtomicU64,
    max: AtomicU64,
    oom: AtomicU64,
    oom_kill: AtomicU64,
    oom_group_kill: AtomicU64,
}

impl MemoryCss {
    pub fn new(parent: Option<Arc<dyn CgroupSubsysState>>, cgroup: Weak<CgroupNode>) -> Arc<Self> {
        Arc::new(Self {
            parent: parent.map(|p| Arc::downgrade(&p) as Weak<dyn CgroupSubsysState>),
            cgroup,
            flags: SpinLock::new(CssFlags::default()),
            inner: SpinLock::new(MemoryCssInner {
                usage: 0,
                peak: 0,
                min: None,
                low: None,
                high: None,
                max: None,
                swap_usage: 0,
                swap_peak: 0,
                swap_high: None,
                swap_max: None,
                events: MemoryEvents::default(),
                oom_group: false,
            }),
            high_trip: AtomicBool::new(false),
        })
    }

    /// Visit every memory CSS ancestor without allocating on the charge path.
    fn for_each_ancestor(&self, mut visit: impl FnMut(&MemoryCss)) {
        let mut parent = self.parent();
        while let Some(css) = parent {
            parent = css.parent();
            if let Some(memcg) = css.as_any().downcast_ref::<MemoryCss>() {
                visit(memcg);
            }
        }
    }

    pub(crate) fn high_limit_exceeded(&self) -> bool {
        let mut exceeded = {
            let inner = self.inner.lock_irqsave();
            inner.high.is_some_and(|high| inner.usage > high)
        };
        self.for_each_ancestor(|memcg| {
            if exceeded {
                return;
            }
            let inner = memcg.inner.lock_irqsave();
            exceeded = inner.high.is_some_and(|high| inner.usage > high);
        });
        exceeded
    }
    fn uncharge_chain(&self, pages: u64) {
        if let Some(parent) = self.parent() {
            if let Some(memcg) = parent.as_any().downcast_ref::<MemoryCss>() {
                memcg.uncharge_chain(pages);
            }
        }
        let mut inner = self.inner.lock_irqsave();
        inner.usage = inner.usage.saturating_sub(pages);
    }

    /// Take and clear the `memory.high` trip flag set by the last refused
    /// over-high charge (see [`Self::high_trip`]).
    pub(crate) fn take_high_trip(&self) -> bool {
        self.high_trip.swap(false, Ordering::Relaxed)
    }

    /// Whether this CSS's usage is still at or above its own `memory.max`.
    pub(crate) fn max_exceeded_now(&self) -> bool {
        let inner = self.inner.lock_irqsave();
        inner.max.is_some_and(|max| inner.usage >= max)
    }
    /// Count a `memory.events` `oom` event (scoped OOM machinery entered).
    pub(crate) fn note_memcg_oom(&self) {
        self.inner
            .lock_irqsave()
            .events
            .oom
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Count a `memory.events` `oom_kill` event (victim selected and killed).
    pub(crate) fn note_memcg_oom_kill(&self) {
        self.inner
            .lock_irqsave()
            .events
            .oom_kill
            .fetch_add(1, Ordering::Relaxed);
    }

    /// 尝试对指定页数计费
    ///
    /// 对 leaf 与全部祖先 CSS 做事务式检查与更新：先在全局事务锁下检查
    /// 每一层的 `memory.max`，任何一层超限则完全不更新（无需回滚），
    /// 全部通过后才逐层累加 usage 并更新 peak。对应 Linux 的
    /// `try_charge`/`commit_charge`。
    ///
    /// 本函数运行在页分配器内部（可能持有 page manager 锁），因此绝不
    /// 睡眠、不回收、不执行 OOM 选择：
    /// - 越过 `memory.high` 时只设置 trip 标志并唤醒回收线程，节流由
    ///   缺页路径的 `mm::memcg::memcg_handle_over_high()` 完成；
    /// - 被 `memory.max` 拒绝时由调用方（`mm::memcg`）记录 pending，
    ///   由 `oom::pagefault_out_of_memory` 排水到 `oom.rs` 状态机执行
    ///   cgroup 范围内的 OOM kill。
    pub fn try_charge(&self, pages: u64) -> Result<(), SystemError> {
        let mut high_exceeded = false;
        let mut max_exceeded = false;

        {
            let _charge_guard = MEMORY_CHARGE_LOCK.lock_irqsave();

            // Check every limit before changing any usage. The global
            // transaction lock prevents another charger from consuming the
            // capacity between these checks and the updates below.
            {
                let inner = self.inner.lock_irqsave();
                if inner
                    .max
                    .is_some_and(|max| inner.usage.saturating_add(pages) > max)
                {
                    inner.events.max.fetch_add(1, Ordering::Relaxed);
                    max_exceeded = true;
                }
            }
            if !max_exceeded {
                self.for_each_ancestor(|memcg| {
                    if max_exceeded {
                        return;
                    }
                    let inner = memcg.inner.lock_irqsave();
                    if inner
                        .max
                        .is_some_and(|max| inner.usage.saturating_add(pages) > max)
                    {
                        inner.events.max.fetch_add(1, Ordering::Relaxed);
                        max_exceeded = true;
                    }
                });
            }
            if !max_exceeded {
                {
                    let mut inner = self.inner.lock_irqsave();
                    inner.usage = inner.usage.saturating_add(pages);
                    if inner.usage > inner.peak {
                        inner.peak = inner.usage;
                    }
                    if inner.high.is_some_and(|high| inner.usage > high) {
                        inner.events.high.fetch_add(1, Ordering::Relaxed);
                        high_exceeded = true;
                        // Trip this CSS so its own tasks throttle on their
                        // next fault; repeated trips only re-set the flag.
                        self.high_trip.store(true, Ordering::Relaxed);
                    }
                }
                self.for_each_ancestor(|memcg| {
                    let mut inner = memcg.inner.lock_irqsave();
                    inner.usage = inner.usage.saturating_add(pages);
                    if inner.usage > inner.peak {
                        inner.peak = inner.usage;
                    }
                    if inner.high.is_some_and(|high| inner.usage > high) {
                        inner.events.high.fetch_add(1, Ordering::Relaxed);
                        high_exceeded = true;
                        memcg.high_trip.store(true, Ordering::Relaxed);
                    }
                });
            }
        }

        if max_exceeded {
            // The charge was never applied to any CSS, so there is nothing
            // to roll back. The refusal itself (and the scoped OOM kill) is
            // handled by the caller on the fault path.
            return Err(SystemError::ENOMEM);
        }
        if high_exceeded {
            // memory.high is a throttle, not an allocation failure: ask the
            // background reclaimer to trim caches. This is allocation-free
            // and lock-hierarchy-safe, so it may run from the charge path.
            PageReclaimer::wakeup_claim_thread();
        }
        Ok(())
    }

    /// 释放指定页数的计费
    pub fn uncharge(&self, pages: u64) {
        let _charge_guard = MEMORY_CHARGE_LOCK.lock_irqsave();

        // Recursing to the root before decrementing the leaf gives the
        // exact inverse order of the charge transaction.
        self.uncharge_chain(pages);
    }

    /// 读取当前用量（字节）
    pub fn current(&self) -> u64 {
        let inner = self.inner.lock_irqsave();
        inner.usage * PAGE_SIZE as u64
    }

    /// 读取峰值用量（字节）
    pub fn peak(&self) -> u64 {
        let inner = self.inner.lock_irqsave();
        inner.peak * PAGE_SIZE as u64
    }

    /// 设置 memory.min（字节）
    pub fn set_min(&self, bytes: Option<u64>) -> Result<(), SystemError> {
        let mut inner = self.inner.lock_irqsave();
        inner.min = bytes.map(|b| b / PAGE_SIZE as u64);
        Ok(())
    }

    /// 设置 memory.low（字节）
    pub fn set_low(&self, bytes: Option<u64>) -> Result<(), SystemError> {
        let mut inner = self.inner.lock_irqsave();
        inner.low = bytes.map(|b| b / PAGE_SIZE as u64);
        Ok(())
    }

    /// 设置 memory.high（字节）
    pub fn set_high(&self, bytes: Option<u64>) -> Result<(), SystemError> {
        let _charge_guard = MEMORY_CHARGE_LOCK.lock_irqsave();
        let mut inner = self.inner.lock_irqsave();
        inner.high = bytes.map(|b| b / PAGE_SIZE as u64);
        Ok(())
    }

    /// 设置 memory.max（字节）
    pub fn set_max(&self, bytes: Option<u64>) -> Result<(), SystemError> {
        let _charge_guard = MEMORY_CHARGE_LOCK.lock_irqsave();
        let mut inner = self.inner.lock_irqsave();
        inner.max = bytes.map(|b| b / PAGE_SIZE as u64);
        Ok(())
    }
    pub fn set_swap_high(&self, bytes: Option<u64>) -> Result<(), SystemError> {
        let mut inner = self.inner.lock_irqsave();
        inner.swap_high = bytes.map(|b| b / PAGE_SIZE as u64);
        Ok(())
    }

    pub fn set_swap_max(&self, bytes: Option<u64>) -> Result<(), SystemError> {
        let mut inner = self.inner.lock_irqsave();
        inner.swap_max = bytes.map(|b| b / PAGE_SIZE as u64);
        Ok(())
    }
    pub fn min(&self) -> Option<u64> {
        self.inner
            .lock_irqsave()
            .min
            .map(|pages| pages * PAGE_SIZE as u64)
    }

    pub fn low(&self) -> Option<u64> {
        self.inner
            .lock_irqsave()
            .low
            .map(|pages| pages * PAGE_SIZE as u64)
    }

    pub fn high(&self) -> Option<u64> {
        self.inner
            .lock_irqsave()
            .high
            .map(|pages| pages * PAGE_SIZE as u64)
    }

    pub fn max(&self) -> Option<u64> {
        self.inner
            .lock_irqsave()
            .max
            .map(|pages| pages * PAGE_SIZE as u64)
    }

    pub fn swap_current(&self) -> u64 {
        self.inner.lock_irqsave().swap_usage * PAGE_SIZE as u64
    }

    pub fn swap_peak(&self) -> u64 {
        self.inner.lock_irqsave().swap_peak * PAGE_SIZE as u64
    }

    pub fn swap_high(&self) -> Option<u64> {
        self.inner
            .lock_irqsave()
            .swap_high
            .map(|pages| pages * PAGE_SIZE as u64)
    }

    pub fn swap_max(&self) -> Option<u64> {
        self.inner
            .lock_irqsave()
            .swap_max
            .map(|pages| pages * PAGE_SIZE as u64)
    }

    /// 读取 memory.events
    pub fn events(&self) -> String {
        let inner = self.inner.lock_irqsave();
        format!(
            "low {}\nhigh {}\nmax {}\noom {}\noom_kill {}\noom_group_kill {}\n",
            inner.events.low.load(Ordering::Relaxed),
            inner.events.high.load(Ordering::Relaxed),
            inner.events.max.load(Ordering::Relaxed),
            inner.events.oom.load(Ordering::Relaxed),
            inner.events.oom_kill.load(Ordering::Relaxed),
            inner.events.oom_group_kill.load(Ordering::Relaxed)
        )
    }

    /// 读取 memory.stat（简化版）
    pub fn stat(&self) -> String {
        let inner = self.inner.lock_irqsave();
        format!(
            "anon {}\nfile 0\nkernel 0\npagetables 0\nslab 0\nsock 0\n\
             file_mapped 0\nfile_dirty 0\nfile_writeback 0\n\
             inactive_anon 0\nactive_anon {}\ninactive_file 0\nactive_file 0\n\
             unevictable 0\nslab_reclaimable 0\nslab_unreclaimable 0\n\
             pgfault 0\npgmajfault 0\n",
            inner.usage * PAGE_SIZE as u64,
            inner.usage * PAGE_SIZE as u64
        )
    }
}

impl CgroupSubsysState for MemoryCss {
    fn subsys_id(&self) -> CgroupSubsysId {
        CgroupSubsysId::Memory
    }

    fn cgroup(&self) -> Arc<CgroupNode> {
        self.cgroup.upgrade().expect("cgroup node dropped")
    }

    fn parent(&self) -> Option<Arc<dyn CgroupSubsysState>> {
        self.parent.as_ref().and_then(|p| p.upgrade())
    }

    fn flags(&self) -> CssFlags {
        *self.flags.lock_irqsave()
    }

    fn set_flags(&self, flags: CssFlags) {
        *self.flags.lock_irqsave() = flags;
    }

    fn css_online(&self) -> Result<(), SystemError> {
        Ok(())
    }

    fn css_offline(&self) -> Result<(), SystemError> {
        // 确保所有内存已释放
        let inner = self.inner.lock_irqsave();
        if inner.usage > 0 {
            log::warn!(
                "memcg offline with {} bytes still charged",
                inner.usage * PAGE_SIZE as u64
            );
        }
        Ok(())
    }

    fn can_fork(
        &self,
        _task: &Arc<crate::process::ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        // Memory controller 不阻止 fork
        Ok(())
    }

    fn fork(&self, _task: &Arc<crate::process::ProcessControlBlock>) {}

    fn exit(&self, _task: &Arc<crate::process::ProcessControlBlock>) {
        // 进程退出时，mm 子系统会调用 uncharge
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// Memory 控制器定义
#[derive(Debug)]
pub struct MemoryController;

impl MemoryController {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl CgroupSubsys for MemoryController {
    fn id(&self) -> CgroupSubsysId {
        CgroupSubsysId::Memory
    }

    fn name(&self) -> &'static str {
        "memory"
    }

    fn css_alloc(
        &self,
        parent: Option<&Arc<dyn CgroupSubsysState>>,
        cgroup: &Arc<CgroupNode>,
    ) -> Result<Arc<dyn CgroupSubsysState>, SystemError> {
        Ok(MemoryCss::new(parent.cloned(), Arc::downgrade(cgroup)))
    }

    fn css_free(&self, _css: &Arc<dyn CgroupSubsysState>) {}

    fn dfl_cftypes(&self) -> Vec<CfType> {
        vec![
            CfType::new("memory.current")
                .with_read(memory_current_read)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.peak")
                .with_read(memory_peak_read)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.min")
                .with_read(memory_min_read)
                .with_write(memory_min_write)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.low")
                .with_read(memory_low_read)
                .with_write(memory_low_write)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.high")
                .with_read(memory_high_read)
                .with_write(memory_high_write)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.max")
                .with_read(memory_max_read)
                .with_write(memory_max_write)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.events")
                .with_read(memory_events_read)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.stat")
                .with_read(memory_stat_read)
                .with_flags(CfTypeFlags::new()),
        ]
    }
}

// ========== 文件读写函数 ==========

fn memory_current_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    Ok(format!("{}\n", mem.current()))
}

fn memory_peak_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    Ok(format!("{}\n", mem.peak()))
}

fn memory_min_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let inner = mem.inner.lock_irqsave();
    Ok(format!(
        "{}\n",
        inner.min.map(|p| p * PAGE_SIZE as u64).unwrap_or(0)
    ))
}

fn memory_min_write(css: &Arc<dyn CgroupSubsysState>, buf: &str) -> Result<(), SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let trimmed = buf.trim();
    let bytes = if trimmed == "0" {
        None
    } else {
        Some(trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?)
    };
    mem.set_min(bytes)
}

fn memory_low_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let inner = mem.inner.lock_irqsave();
    Ok(format!(
        "{}\n",
        inner.low.map(|p| p * PAGE_SIZE as u64).unwrap_or(0)
    ))
}

fn memory_low_write(css: &Arc<dyn CgroupSubsysState>, buf: &str) -> Result<(), SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let trimmed = buf.trim();
    let bytes = if trimmed == "0" {
        None
    } else {
        Some(trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?)
    };
    mem.set_low(bytes)
}

fn memory_high_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let inner = mem.inner.lock_irqsave();
    Ok(match inner.high {
        Some(pages) => format!("{}\n", pages * PAGE_SIZE as u64),
        None => "max\n".to_string(),
    })
}

fn memory_high_write(css: &Arc<dyn CgroupSubsysState>, buf: &str) -> Result<(), SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let trimmed = buf.trim();
    let bytes = if trimmed == "max" {
        None
    } else {
        Some(trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?)
    };
    mem.set_high(bytes)
}

fn memory_max_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let inner = mem.inner.lock_irqsave();
    Ok(match inner.max {
        Some(pages) => format!("{}\n", pages * PAGE_SIZE as u64),
        None => "max\n".to_string(),
    })
}

fn memory_max_write(css: &Arc<dyn CgroupSubsysState>, buf: &str) -> Result<(), SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let trimmed = buf.trim();
    let bytes = if trimmed == "max" {
        None
    } else {
        Some(trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?)
    };
    mem.set_max(bytes)
}

fn memory_events_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    Ok(mem.events())
}

fn memory_stat_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    Ok(mem.stat())
}

/// 初始化 Memory 控制器
pub fn init_memory_controller() {
    let controller = MemoryController::new();
    crate::cgroup::subsys::register_subsys(controller);
}
