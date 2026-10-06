use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::cmp::Reverse;
use core::sync::atomic::{AtomicUsize, Ordering};
use hashbrown::{HashMap, HashSet};
use system_error::SystemError;

use crate::{
    bpf::prog::{device::DeviceAccess, BpfProg},
    cgroup::subsys::{all_subsys, CgroupSubsysId, CgroupSubsysState},
    include::bindings::linux_bpf::{
        bpf_prog_type, BPF_F_ALLOW_MULTI, BPF_F_ALLOW_OVERRIDE, BPF_F_REPLACE,
    },
    libs::{mutex::Mutex, rwlock::RwLock, spinlock::SpinLock},
    process::RawPid,
};

/// 对无符号计数器做饱和递减：计数已为 0 时停在 0，绝不回绕翻转成
/// `usize::MAX`。
///
/// cgroup 成员/pids 计数在配对正确时与非零；一旦某个语义缺陷造成
/// 单次失配（重复 remove_task/uncharge），裸 `fetch_sub` 会把计数器
/// 翻转为极大值：`pids.current` 变 `usize::MAX` 后 try_charge/can_attach
/// 恒 EAGAIN，整组永久无法 fork；`subtree_task_counter` 翻转则让
/// `is_populated`/rmdir/domain 切换判定全部失真。饱和递减把失配的损害
/// 限制为"该层计数偏低"，可被后续正常配对逐步自愈，且不会制造
/// 永久性的组级 EAGAIN/EBUSY。所有调用点都持有
/// `cgroup_accounting_lock`；顺序对与既有 `fetch_sub(_, Release)` 兼容
/// （成功 Release、失败 Acquire，保守取 AcqRel/Acquire）。
pub(crate) fn saturating_sub(counter: &AtomicUsize) {
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
        if v == 0 {
            None
        } else {
            Some(v - 1)
        }
    });
}

/// `BPF_F_PREORDER` is not generated in the current Linux BPF bindings.
pub const BPF_DEVICE_F_PREORDER: u32 = 1 << 6;
const BPF_CGROUP_MAX_PROGS: usize = 64;
type DeviceSnapshotUpdates = Vec<(Arc<CgroupNode>, Arc<Vec<Arc<BpfProg>>>)>;

#[derive(Debug, Clone)]
struct AttachedDeviceProgram {
    prog: Arc<BpfProg>,
    flags: u32,
}

#[derive(Debug)]
struct DeviceBpfState {
    direct: Vec<AttachedDeviceProgram>,
    flags: u32,
    /// Immutable effective chain; readers only clone this Arc under the node lock.
    effective: Arc<Vec<Arc<BpfProg>>>,
}

impl DeviceBpfState {
    fn empty() -> Self {
        Self {
            direct: Vec::new(),
            flags: 0,
            effective: Arc::new(Vec::new()),
        }
    }
}

/// The cgroup v2 hierarchy mode of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupType {
    Domain,
    Threaded,
    DomainThreaded,
    DomainInvalid,
}

impl CgroupType {
    pub fn name(self) -> &'static str {
        match self {
            Self::Domain => "domain",
            Self::Threaded => "threaded",
            Self::DomainThreaded => "domain threaded",
            Self::DomainInvalid => "domain invalid",
        }
    }
}

/// A deduplicated task CSS-set identity shared by all tasks in one cgroup.
#[derive(Debug)]
pub struct CssSetToken {
    id: usize,
    users: AtomicUsize,
}

impl CssSetToken {
    fn new(id: usize) -> Arc<Self> {
        Arc::new(Self {
            id,
            users: AtomicUsize::new(0),
        })
    }

    fn acquire(&self) {
        self.users.fetch_add(1, Ordering::Relaxed);
    }

    fn release(&self) {
        self.users.fetch_sub(1, Ordering::Release);
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn users(&self) -> usize {
        self.users.load(Ordering::Acquire)
    }
}

fn is_threaded_type(ty: CgroupType) -> bool {
    matches!(ty, CgroupType::Threaded)
}

fn has_threaded_descendant(node: &CgroupNode) -> bool {
    node.children().into_iter().any(|child| {
        is_threaded_type(child.cgroup_type())
            || child.cgroup_type() == CgroupType::DomainThreaded
            || has_threaded_descendant(&child)
    })
}

fn refresh_domain_state(node: &CgroupNode) {
    if node.cgroup_type() == CgroupType::DomainInvalid && !node.has_tasks() {
        *node.type_state.write() = CgroupType::DomainThreaded;
    }
    if node.cgroup_type() == CgroupType::DomainThreaded && !has_threaded_descendant(node) {
        *node.type_state.write() = CgroupType::Domain;
    }
}

#[derive(Debug)]
pub struct CgroupNode {
    id: usize,
    name: String,
    parent: Option<Weak<CgroupNode>>,
    children: RwLock<HashMap<String, Arc<CgroupNode>>>,
    tasks: RwLock<HashSet<RawPid>>,
    subtree_control: RwLock<HashSet<String>>,
    /// 控制器状态数组（对应 Linux 的 cgroup_subsys_state *subsys[]）
    subsys: [RwLock<Option<Arc<dyn CgroupSubsysState>>>; CgroupSubsysId::COUNT],
    /// The hierarchy state exposed through cgroup.type.
    type_state: RwLock<CgroupType>,
    /// Shared identity for this node's complete CSS array.
    css_set: Arc<CssSetToken>,
    /// 全局任务计数（pids 控制器用）
    subtree_task_counter: AtomicUsize,
    device_bpf: RwLock<DeviceBpfState>,
}

impl CgroupNode {
    /// 创建空的控制器状态数组
    fn empty_subsys_array() -> [RwLock<Option<Arc<dyn CgroupSubsysState>>>; CgroupSubsysId::COUNT] {
        [
            RwLock::new(None), // Cpu
            RwLock::new(None), // Memory
            RwLock::new(None), // Pids
            RwLock::new(None), // Io
            RwLock::new(None), // Cpuset
            RwLock::new(None), // Freezer
            RwLock::new(None), // Hugetlb
            RwLock::new(None), // Rdma
            RwLock::new(None), // Misc
        ]
    }

    fn new_root() -> Arc<Self> {
        Arc::new(Self {
            id: 1,
            name: String::new(),
            parent: None,
            children: RwLock::new(HashMap::new()),
            tasks: RwLock::new(HashSet::new()),
            subtree_control: RwLock::new(HashSet::new()),
            subsys: Self::empty_subsys_array(),
            type_state: RwLock::new(CgroupType::Domain),
            css_set: CssSetToken::new(1),
            subtree_task_counter: AtomicUsize::new(0),
            device_bpf: RwLock::new(DeviceBpfState::empty()),
        })
    }

    fn new_child(id: usize, name: String, parent: &Arc<CgroupNode>) -> Arc<Self> {
        let type_state = if matches!(
            parent.cgroup_type(),
            CgroupType::Threaded | CgroupType::DomainThreaded
        ) {
            CgroupType::Threaded
        } else {
            CgroupType::Domain
        };
        Arc::new(Self {
            id,
            name,
            parent: Some(Arc::downgrade(parent)),
            children: RwLock::new(HashMap::new()),
            tasks: RwLock::new(HashSet::new()),
            subtree_control: RwLock::new(HashSet::new()),
            subsys: Self::empty_subsys_array(),
            type_state: RwLock::new(type_state),
            css_set: CssSetToken::new(id),
            subtree_task_counter: AtomicUsize::new(0),
            device_bpf: RwLock::new(DeviceBpfState::empty()),
        })
    }

    /// 获取指定控制器的状态（对应 Linux 的 cgroup_subsys_state *cgroup_css(cgroup, subsys)）
    pub fn css(&self, id: CgroupSubsysId) -> Option<Arc<dyn CgroupSubsysState>> {
        self.subsys[id as usize].read().clone()
    }

    /// 设置控制器状态（在控制器 online 时调用）
    pub fn set_css(&self, id: CgroupSubsysId, css: Arc<dyn CgroupSubsysState>) {
        *self.subsys[id as usize].write() = Some(css);
    }

    /// 清除控制器状态（在控制器 offline 时调用）
    pub fn clear_css(&self, id: CgroupSubsysId) {
        *self.subsys[id as usize].write() = None;
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn parent(&self) -> Option<Arc<CgroupNode>> {
        self.parent.as_ref().and_then(|p| p.upgrade())
    }

    pub fn cgroup_type(&self) -> CgroupType {
        *self.type_state.read()
    }

    pub fn cgroup_type_name(&self) -> &'static str {
        self.cgroup_type().name()
    }

    pub fn css_set(&self) -> Arc<CssSetToken> {
        self.css_set.clone()
    }

    pub fn css_set_id(&self) -> usize {
        self.css_set.id()
    }

    pub fn has_domain_controllers(&self) -> bool {
        self.subtree_control()
            .iter()
            .any(|name| matches!(name.as_str(), "memory" | "io"))
    }

    /// Whether this node is a member of a threaded subtree.
    pub fn in_threaded_subtree(&self) -> bool {
        if self.cgroup_type() == CgroupType::Threaded {
            return true;
        }
        let mut cur = self.parent();
        while let Some(node) = cur {
            if node.cgroup_type() == CgroupType::Threaded {
                return true;
            }
            cur = node.parent();
        }
        false
    }

    /// Apply a cgroup.type write. The parent is promoted to the appropriate
    /// domain state when a new threaded subtree is established.
    ///
    /// 调用者必须持有 `cgroup_accounting_lock`：cgroup.type 的
    /// vet→写入是"读拓扑态（subtree_task_count/域控制器/父类型）→改
    /// 类型"的读改写序列，必须与任务迁移（write_procs/fork/exit）以及
    /// mkdir/rmdir 串行。否则并发迁移恰好插在 vet 与写入之间时，可固化
    /// "threaded 子树内存在域控制器任务"等非法组合，后续 vet/rmdir 把
    /// 非法态当真，导致持久 EBUSY 或 no-internal-process 规则绕过。
    /// 入口 `write_type_file` 负责取锁。
    pub fn set_cgroup_type(&self, requested: &str) -> Result<(), SystemError> {
        // 锁纪律自检：SpinLock::is_locked 为弱判定（并发持锁者可能使
        // 漏取锁的调用侥幸通过），但足以在单线程复现路径与调试期稳定
        // 捕获"未持锁即变更类型"的违约。
        debug_assert!(
            cgroup_accounting_lock().is_locked(),
            "set_cgroup_type 必须在持有 cgroup_accounting_lock 的临界区内调用"
        );
        match requested.trim() {
            "threaded" => {
                if self.parent().is_none() || self.cgroup_type() != CgroupType::Domain {
                    return Err(SystemError::EINVAL);
                }
                // Linux cgroup_enable_threaded()：
                //   - cgroup_is_populated(cgrp)（本组或后代仍有任务）→ EOPNOTSUPP
                //   - 父组的 subtree_control 已启用域控制器（memory/io）→ EOPNOTSUPP
                if self.subtree_task_count() != 0 {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                let parent = self.parent().unwrap();
                if parent.has_domain_controllers() {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                if self.has_domain_controllers() {
                    return Err(SystemError::EBUSY);
                }
                if self.children().into_iter().any(|child| {
                    !matches!(
                        child.cgroup_type(),
                        CgroupType::Threaded | CgroupType::DomainThreaded
                    )
                }) {
                    return Err(SystemError::EBUSY);
                }
                match parent.cgroup_type() {
                    CgroupType::Threaded | CgroupType::DomainThreaded => {}
                    CgroupType::Domain if parent.has_tasks() => {
                        *parent.type_state.write() = CgroupType::DomainInvalid;
                    }
                    CgroupType::Domain => {
                        *parent.type_state.write() = CgroupType::DomainThreaded;
                    }
                    CgroupType::DomainInvalid => return Err(SystemError::EBUSY),
                }
                *self.type_state.write() = CgroupType::Threaded;
                Ok(())
            }
            "domain" => {
                if self.cgroup_type() != CgroupType::Threaded {
                    return Err(SystemError::EINVAL);
                }
                if !self.children().is_empty() {
                    return Err(SystemError::EBUSY);
                }
                *self.type_state.write() = CgroupType::Domain;
                if let Some(parent) = self.parent() {
                    refresh_domain_state(&parent);
                }
                Ok(())
            }
            "domain threaded" | "domain invalid" => Err(SystemError::EINVAL),
            _ => Err(SystemError::EINVAL),
        }
    }

    pub fn add_task(&self, pid: RawPid) {
        if !self.tasks.write().insert(pid) {
            debug_assert!(false, "cgroup task {:?} already exists", pid);
            return;
        }
        let mut cur = self.parent();
        while let Some(node) = cur {
            node.subtree_task_counter.fetch_add(1, Ordering::Release);
            cur = node.parent();
        }
    }

    pub fn remove_task(&self, pid: RawPid) {
        if !self.tasks.write().remove(&pid) {
            debug_assert!(false, "cgroup task {:?} does not exist", pid);
            return;
        }
        let mut cur = self.parent();
        while let Some(node) = cur {
            // 饱和递减：成员集合命中与计数严格配对，但防御任何未来失配
            // （如重复 remove）把祖先计数翻转成 usize::MAX——那会让
            // is_populated/rmdir 与 domain 切换判定永久失真（EBUSY）。
            saturating_sub(node.subtree_task_counter());
            refresh_domain_state(&node);
            cur = node.parent();
        }
        refresh_domain_state(self);
    }

    pub fn rename_task(&self, old_pid: RawPid, new_pid: RawPid) {
        if old_pid == new_pid {
            return;
        }

        let mut tasks = self.tasks.write();
        if !tasks.remove(&old_pid) {
            debug_assert!(false, "cgroup task {:?} does not exist", old_pid);
            return;
        }
        let inserted = tasks.insert(new_pid);
        debug_assert!(inserted, "cgroup task {:?} already exists", new_pid);
    }

    pub fn tasks(&self) -> Vec<RawPid> {
        self.tasks.read().iter().cloned().collect()
    }

    pub fn children_names(&self) -> Vec<String> {
        self.children.read().keys().cloned().collect()
    }

    pub fn children(&self) -> Vec<Arc<CgroupNode>> {
        self.children.read().values().cloned().collect()
    }

    pub fn child(&self, name: &str) -> Option<Arc<CgroupNode>> {
        self.children.read().get(name).cloned()
    }

    pub fn has_children(&self) -> bool {
        !self.children.read().is_empty()
    }

    pub fn has_tasks(&self) -> bool {
        !self.tasks.read().is_empty()
    }

    pub fn subtree_control(&self) -> Vec<String> {
        self.subtree_control.read().iter().cloned().collect()
    }

    pub fn set_subtree_control(&self, controllers: HashSet<String>) {
        *self.subtree_control.write() = controllers;
    }


    pub fn subtree_task_counter(&self) -> &AtomicUsize {
        &self.subtree_task_counter
    }

    pub fn subtree_task_count(&self) -> usize {
        self.tasks
            .read()
            .len()
            .saturating_add(self.subtree_task_counter.load(Ordering::Acquire))
    }


    pub fn is_ancestor_of(self: &Arc<Self>, other: &Arc<Self>) -> bool {
        if Arc::ptr_eq(self, other) {
            return true;
        }

        let mut cur = other.parent();
        while let Some(node) = cur {
            if Arc::ptr_eq(self, &node) {
                return true;
            }
            cur = node.parent();
        }

        false
    }

    // ==================== Pids 控制器辅助方法 ====================
    
    /// 获取 pids.max（通过 css 访问）
    pub fn pids_max(&self) -> Option<usize> {
        self.css(CgroupSubsysId::Pids).and_then(|css| {
            css.as_any()
                .downcast_ref::<crate::cgroup::controllers::pids::PidsCgroupState>()
                .and_then(|state| state.get_max())
        })
    }

    /// 获取 pids.current（当前 cgroup 及其子树的层级计数）
    pub fn pids_current_count(&self) -> usize {
        self.css(CgroupSubsysId::Pids)
            .and_then(|css| {
                css.as_any()
                    .downcast_ref::<crate::cgroup::controllers::pids::PidsCgroupState>()
                    .map(|state| state.subtree_current())
            })
            .unwrap_or(0)
    }

    /// 获取 pids.events max 计数
    pub fn pids_events_max(&self) -> u64 {
        self.css(CgroupSubsysId::Pids)
            .and_then(|css| {
                css.as_any()
                    .downcast_ref::<crate::cgroup::controllers::pids::PidsCgroupState>()
                    .map(|state| state.events_max())
            })
            .unwrap_or(0)
    }

    /// 增加 pids.events max（fork 失败时调用）
    pub fn inc_pids_events_max(&self) {
        if let Some(css) = self.css(CgroupSubsysId::Pids) {
            if let Some(state) = css.as_any()
                .downcast_ref::<crate::cgroup::controllers::pids::PidsCgroupState>() {
                state.inc_events_max();
            }
        }
    }
    /// 为 fork 预留 pids 计数。
    pub fn charge_pids(&self, count: usize) -> Result<(), SystemError> {
        let css = self.css(CgroupSubsysId::Pids).ok_or(SystemError::ENOENT)?;
        let state = css
            .as_any()
            .downcast_ref::<crate::cgroup::controllers::pids::PidsCgroupState>()
            .ok_or(SystemError::EINVAL)?;
        let mut charged = 0;
        while charged < count {
            if let Err(error) = state.try_charge() {
                for _ in 0..charged {
                    state.uncharge();
                }
                return Err(error);
            }
            charged += 1;
        }
        Ok(())
    }

    /// 释放任务的 pids 计数。
    pub fn uncharge_pids(&self, count: usize) {
        let Some(css) = self.css(CgroupSubsysId::Pids) else {
            return;
        };
        let Some(state) = css
            .as_any()
            .downcast_ref::<crate::cgroup::controllers::pids::PidsCgroupState>()
        else {
            return;
        };
        for _ in 0..count {
            state.uncharge();
        }
    }

    /// 在 cgroup 迁移时转移层级 pids 计数。
    ///
    /// Linux 将迁移视为组织操作，不受 `pids.max` 阻塞；因此目标计数
    /// 必须无条件增加，不能在更新任务归属后再执行可能失败的 charge。
    pub fn transfer_pids_charge(
        src: &Arc<CgroupNode>,
        dst: &Arc<CgroupNode>,
        count: usize,
    ) {
        src.uncharge_pids(count);
        if let Some(css) = dst.css(CgroupSubsysId::Pids) {
            if let Some(state) = css
                .as_any()
                .downcast_ref::<crate::cgroup::controllers::pids::PidsCgroupState>()
            {
                for _ in 0..count {
                    state.charge_unchecked();
                }
            }
        }
    }
    pub fn set_pids_max(
        &self,
        max: Option<usize>,
    ) -> Result<(), SystemError> {
        let css = self.css(CgroupSubsysId::Pids).ok_or(SystemError::ENOENT)?;
        let state = css
            .as_any()
            .downcast_ref::<crate::cgroup::controllers::pids::PidsCgroupState>()
            .ok_or(SystemError::EINVAL)?;
        state.set_max(max);
        Ok(())
    }

    pub fn set_cpu_weight(&self, weight: u64) -> Result<(), SystemError> {
        let css = self.css(CgroupSubsysId::Cpu).ok_or(SystemError::ENOENT)?;
        let state = css
            .as_any()
            .downcast_ref::<crate::cgroup::controllers::cpu::CpuCss>()
            .ok_or(SystemError::EINVAL)?;
        state.set_shares(weight)
    }

    pub fn set_cpu_max(
        &self,
        quota: Option<u64>,
        period_us: u64,
    ) -> Result<(), SystemError> {
        let css = self.css(CgroupSubsysId::Cpu).ok_or(SystemError::ENOENT)?;
        let state = css
            .as_any()
            .downcast_ref::<crate::cgroup::controllers::cpu::CpuCss>()
            .ok_or(SystemError::EINVAL)?;
        state.set_bandwidth(quota, period_us)
    }

    fn with_memory_css<R>(
        &self,
        f: impl FnOnce(&crate::cgroup::controllers::memory::MemoryCss) -> Result<R, SystemError>,
    ) -> Result<R, SystemError> {
        let css = self.css(CgroupSubsysId::Memory).ok_or(SystemError::ENOENT)?;
        let state = css
            .as_any()
            .downcast_ref::<crate::cgroup::controllers::memory::MemoryCss>()
            .ok_or(SystemError::EINVAL)?;
        f(state)
    }

    pub fn set_memory_min(&self, value: Option<u64>) -> Result<(), SystemError> {
        self.with_memory_css(|state| state.set_min(value))
    }

    pub fn set_memory_low(&self, value: Option<u64>) -> Result<(), SystemError> {
        self.with_memory_css(|state| state.set_low(value))
    }

    pub fn set_memory_high(&self, value: Option<u64>) -> Result<(), SystemError> {
        self.with_memory_css(|state| state.set_high(value))
    }

    pub fn set_memory_max(&self, value: Option<u64>) -> Result<(), SystemError> {
        self.with_memory_css(|state| state.set_max(value))
    }

    pub fn set_memory_swap_high(&self, value: Option<u64>) -> Result<(), SystemError> {
        self.with_memory_css(|state| state.set_swap_high(value))
    }

    pub fn set_memory_swap_max(&self, value: Option<u64>) -> Result<(), SystemError> {
        self.with_memory_css(|state| state.set_swap_max(value))
    }
    pub fn cpu_bandwidth(&self) -> (Option<u64>, u64) {
        self.css(CgroupSubsysId::Cpu)
            .and_then(|css| {
                css.as_any()
                    .downcast_ref::<crate::cgroup::controllers::cpu::CpuCss>()
                    .map(|cpu| cpu.bandwidth())
            })
            .unwrap_or((None, 100_000))
    }


    /// Apply the complete effective chain to one device operation. Linux does
    /// not short-circuit this chain when a program denies access.
    pub fn allows_device_access(&self, access: DeviceAccess) -> bool {
        let programs = self.device_bpf.read().effective.clone();
        let mut allowed = true;
        for program in programs.iter() {
            if !program.run_device(access) {
                allowed = false;
            }
        }
        allowed
    }


    /// Get whether this cgroup itself requested freezing, for cgroup.freeze.
    /// 获取有效冻结请求（包括祖先传播的请求）。
    pub fn freeze_requested(&self) -> bool {
        if let Some(css) = self.css(CgroupSubsysId::Freezer) {
            if let Some(freezer) = css
                .as_any()
                .downcast_ref::<crate::cgroup::controllers::freezer::FreezerCss>()
            {
                return freezer.freeze_requested();
            }
        }
        false
    }

    pub fn self_freeze_requested(&self) -> bool {
        if let Some(css) = self.css(CgroupSubsysId::Freezer) {
            if let Some(freezer) = css
                .as_any()
                .downcast_ref::<crate::cgroup::controllers::freezer::FreezerCss>()
            {
                return freezer.self_freeze_requested();
            }
        }
        false

    }
    /// 设置 freeze 请求
    pub fn set_freeze_requested(&self, freeze: bool) {
        if let Some(freezer_css) = self.css(CgroupSubsysId::Freezer) {
            if let Some(freezer) = freezer_css.as_any().downcast_ref::<crate::cgroup::controllers::freezer::FreezerCss>() {
                let _ = freezer.set_freeze_requested(freeze);
            }
        }
    }

    /// 检查 cgroup 是否已冻结
    /// The kernel v2 `cgroup.events` frozen bit reports whether this
    /// cgroup's own freeze request has completed for its subtree.
    pub fn is_frozen(&self) -> bool {
        if let Some(freezer_css) = self.css(CgroupSubsysId::Freezer) {
            if let Some(freezer) = freezer_css
                .as_any()
                .downcast_ref::<crate::cgroup::controllers::freezer::FreezerCss>()
            {
                return freezer.is_frozen();
            }
        }
        false
    }
}

#[derive(Debug)]
pub struct CgroupRoot {
    root: Arc<CgroupNode>,
    next_id: AtomicUsize,
    all_nodes: SpinLock<HashMap<usize, Arc<CgroupNode>>>,
    /// Serializes hierarchy changes and device-program state transitions. An
    /// accounting-lock holder must never acquire this sleeping lock.
    structure_lock: Mutex<()>,
}

impl CgroupRoot {
    fn initialize_css(
        node: &Arc<CgroupNode>,
        parent: Option<&Arc<CgroupNode>>,
    ) -> Result<(), SystemError> {
        for subsys in all_subsys() {
            let parent_css = parent.and_then(|parent| parent.css(subsys.id()));
            let css = subsys.css_alloc(parent_css.as_ref(), node)?;
            css.css_online()?;
            node.set_css(subsys.id(), css);
        }
        Ok(())
    }

    fn new() -> Arc<Self> {
        let root = CgroupNode::new_root();
        Self::initialize_css(&root, None).expect("cgroup root CSS initialization failed");
        let mut all_nodes = HashMap::new();
        all_nodes.insert(root.id(), root.clone());

        Arc::new(Self {
            root,
            next_id: AtomicUsize::new(2),
            all_nodes: SpinLock::new(all_nodes),
            structure_lock: Mutex::new(()),
        })
    }

    pub fn root(&self) -> Arc<CgroupNode> {
        self.root.clone()
    }

    #[allow(dead_code)]
    pub fn lookup_by_id(&self, id: usize) -> Option<Arc<CgroupNode>> {
        self.all_nodes.lock().get(&id).cloned()
    }

    pub fn is_online(&self, node: &Arc<CgroupNode>) -> bool {
        self.all_nodes
            .lock()
            .get(&node.id())
            .is_some_and(|online| Arc::ptr_eq(online, node))
    }

    pub fn create_child(
        &self,
        parent: &Arc<CgroupNode>,
        name: &str,
    ) -> Result<Arc<CgroupNode>, SystemError> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(SystemError::EINVAL);
        }
        let _structure_guard = self.structure_lock.lock();
        if !self.is_online(parent) {
            return Err(SystemError::ENOENT);
        }
        if let Some(existing) = parent.children.read().get(name) {
            return Ok(existing.clone());
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // Freeze requests and CSS publication share the accounting lock so a
        // child cannot miss an ancestor freeze racing with mkdir.
        let _accounting_guard = cgroup_accounting_lock().lock();
        let child = CgroupNode::new_child(id, name.to_string(), parent);
        Self::initialize_css(&child, Some(parent))?;
        child.device_bpf.write().effective = parent.device_bpf.read().effective.clone();
        parent
            .children
            .write()
            .insert(name.to_string(), child.clone());

        self.all_nodes.lock().insert(id, child.clone());
        Ok(child)
    }

    pub fn remove_child(
        &self,
        parent: &Arc<CgroupNode>,
        name: &str,
        expected: &Arc<CgroupNode>,
    ) -> Result<(), SystemError> {
        let _structure_guard = self.structure_lock.lock();
        if !self.is_online(parent) {
            return Err(SystemError::ENOENT);
        }
        let child = parent
            .children
            .read()
            .get(name)
            .cloned()
            .ok_or(SystemError::ENOENT)?;
        if !Arc::ptr_eq(&child, expected) {
            return Err(SystemError::ENOENT);
        }

        // A fork reserves a pids charge before publishing task membership.
        // Keep the accounting lock through both the emptiness check and the
        // online-registry removal so migration/fork cannot target a dying node.
        let accounting_guard = cgroup_accounting_lock().lock();
        if child.has_children() {
            return Err(SystemError::ENOTEMPTY);
        }
        if child.has_tasks() || child.pids_current_count() != 0 {
            return Err(SystemError::EBUSY);
        }

        // Take stable references to every CSS before beginning teardown.  A
        // controller's offline callback may inspect the still-online cgroup.
        let mut css_states = Vec::new();
        for subsys in all_subsys() {
            if let Some(css) = child.css(subsys.id()) {
                css.css_offline()?;
                css_states.push((subsys, css));
            }
        }

        let removed_child = parent.children.write().remove_entry(name);
        let removed = self.all_nodes.lock().remove(&child.id());
        // 最后一个 threaded 子节点移除后，父节点需要从 DomainThreaded 回退到
        // Domain（对应 Linux cgroup_rmwb 之后的域状态重算），否则 cgroup.type
        // 卡在 "domain threaded"，后续 mkdir 会继承错误的类型。
        refresh_domain_state(parent);
        drop(accounting_guard);

        // Open directory FDs may keep this node alive; they do not keep its
        // attachments installed after rmdir. Drop program refs outside locks.
        let empty_state = DeviceBpfState::empty();
        let old_state = core::mem::replace(&mut *child.device_bpf.write(), empty_state);
        for (subsys, css) in css_states {
            child.clear_css(subsys.id());
            subsys.css_free(&css);
            css.css_released();
        }
        drop(_structure_guard);
        drop(old_state);
        drop(removed);
        drop(removed_child);
        Ok(())
    }

    /// Legacy `BPF_PROG_ATTACH` for `BPF_CGROUP_DEVICE`. All descendants are
    /// prepared before any visible policy is changed.
    pub fn attach_device_program(
        &self,
        node: &Arc<CgroupNode>,
        prog: Arc<BpfProg>,
        flags: u32,
        replace: Option<Arc<BpfProg>>,
    ) -> Result<(), SystemError> {
        if prog.prog_type() != bpf_prog_type::BPF_PROG_TYPE_CGROUP_DEVICE
            || replace
                .as_ref()
                .is_some_and(|old| old.prog_type() != bpf_prog_type::BPF_PROG_TYPE_CGROUP_DEVICE)
        {
            return Err(SystemError::EINVAL);
        }
        let allowed_flags =
            BPF_F_ALLOW_OVERRIDE | BPF_F_ALLOW_MULTI | BPF_F_REPLACE | BPF_DEVICE_F_PREORDER;
        if flags & !allowed_flags != 0
            || flags & BPF_F_ALLOW_OVERRIDE != 0 && flags & BPF_F_ALLOW_MULTI != 0
            || flags & BPF_F_REPLACE != 0 && flags & BPF_F_ALLOW_MULTI == 0
            || (flags & BPF_F_REPLACE != 0) != replace.is_some()
        {
            return Err(SystemError::EINVAL);
        }

        let _structure_guard = self.structure_lock.lock();
        if !self.is_online(node) {
            return Err(SystemError::ENOENT);
        }
        if !Self::hierarchy_allows_device_attach(node) {
            return Err(SystemError::EPERM);
        }

        let current = node.device_bpf.read();
        let mode = flags & (BPF_F_ALLOW_OVERRIDE | BPF_F_ALLOW_MULTI);
        if !current.direct.is_empty() && current.flags != mode {
            return Err(SystemError::EPERM);
        }
        if current.direct.len() >= BPF_CGROUP_MAX_PROGS {
            return Err(SystemError::E2BIG);
        }
        let mut direct = Vec::new();
        direct
            .try_reserve(current.direct.len() + 1)
            .map_err(|_| SystemError::ENOMEM)?;
        direct.extend(current.direct.iter().cloned());
        drop(current);

        if mode & BPF_F_ALLOW_MULTI == 0 {
            let entry = AttachedDeviceProgram { prog, flags };
            if direct.is_empty() {
                direct.push(entry);
            } else {
                direct[0] = entry;
            }
        } else {
            if direct.iter().any(|entry| {
                Arc::ptr_eq(&entry.prog, &prog)
                    && replace.as_ref().is_none_or(|old| !Arc::ptr_eq(old, &prog))
            }) {
                return Err(SystemError::EINVAL);
            }
            let entry = AttachedDeviceProgram { prog, flags };
            if let Some(replace) = replace {
                let old = direct
                    .iter_mut()
                    .find(|candidate| Arc::ptr_eq(&candidate.prog, &replace))
                    .ok_or(SystemError::ENOENT)?;
                *old = entry;
            } else {
                direct.push(entry);
            }
        }

        let mut updates = self.prepare_device_snapshots(node, &direct, mode)?;
        let old_direct = {
            let mut state = node.device_bpf.write();
            state.flags = mode;
            core::mem::replace(&mut state.direct, direct)
        };
        Self::publish_device_snapshots(&mut updates);
        drop(_structure_guard);
        drop(old_direct);
        Ok(())
    }

    pub fn detach_device_program(
        &self,
        node: &Arc<CgroupNode>,
        prog: Option<&Arc<BpfProg>>,
    ) -> Result<(), SystemError> {
        let _structure_guard = self.structure_lock.lock();
        if !self.is_online(node) {
            return Err(SystemError::ENOENT);
        }
        let current = node.device_bpf.read();
        if current.direct.is_empty() {
            return Err(SystemError::ENOENT);
        }
        let index = if current.flags & BPF_F_ALLOW_MULTI != 0 {
            let prog = prog.ok_or(SystemError::EINVAL)?;
            current
                .direct
                .iter()
                .position(|entry| Arc::ptr_eq(&entry.prog, prog))
                .ok_or(SystemError::ENOENT)?
        } else {
            // Legacy NONE and OVERRIDE modes ignore a supplied program FD.
            0
        };
        let mut direct = Vec::new();
        direct
            .try_reserve(current.direct.len().saturating_sub(1))
            .map_err(|_| SystemError::ENOMEM)?;
        direct.extend(current.direct.iter().enumerate().filter_map(|(i, entry)| {
            if i == index {
                None
            } else {
                Some(entry.clone())
            }
        }));
        let mode = if direct.is_empty() { 0 } else { current.flags };
        drop(current);

        let mut updates = self.prepare_device_snapshots(node, &direct, mode)?;
        let old_direct = {
            let mut state = node.device_bpf.write();
            state.flags = mode;
            core::mem::replace(&mut state.direct, direct)
        };
        Self::publish_device_snapshots(&mut updates);
        drop(_structure_guard);
        drop(old_direct);
        Ok(())
    }

    /// Return a stable copy of direct/effective IDs and direct per-program
    /// flags. The caller performs all user copies after releasing this lock.
    pub fn query_device_programs(
        &self,
        node: &Arc<CgroupNode>,
        effective: bool,
    ) -> Result<(u32, Vec<u32>, Vec<u32>), SystemError> {
        let _structure_guard = self.structure_lock.lock();
        if !self.is_online(node) {
            return Err(SystemError::ENOENT);
        }
        let state = node.device_bpf.read();
        let programs = if effective {
            state.effective.as_slice()
        } else {
            &[]
        };
        let count = if effective {
            programs.len()
        } else {
            state.direct.len()
        };
        let mut ids = Vec::new();
        let mut attach_flags = Vec::new();
        ids.try_reserve_exact(count)
            .map_err(|_| SystemError::ENOMEM)?;
        if !effective {
            attach_flags
                .try_reserve_exact(count)
                .map_err(|_| SystemError::ENOMEM)?;
        }
        if effective {
            ids.extend(programs.iter().map(|prog| prog.id()));
        } else {
            ids.extend(state.direct.iter().map(|entry| entry.prog.id()));
            attach_flags.resize(count, state.flags);
        }
        Ok((if effective { 0 } else { state.flags }, ids, attach_flags))
    }

    fn hierarchy_allows_device_attach(node: &Arc<CgroupNode>) -> bool {
        let mut parent = node.parent();
        while let Some(ancestor) = parent {
            let state = ancestor.device_bpf.read();
            if state.flags & BPF_F_ALLOW_MULTI != 0 {
                return true;
            }
            if !state.direct.is_empty() {
                return state.flags & BPF_F_ALLOW_OVERRIDE != 0;
            }
            parent = ancestor.parent();
        }
        true
    }

    /// Iterate the Linux effective-chain candidates, retaining the original
    /// per-cgroup FIFO index for the global PREORDER ordering.
    fn visit_effective_device_programs<F>(
        node: &Arc<CgroupNode>,
        changed: &Arc<CgroupNode>,
        direct: &[AttachedDeviceProgram],
        flags: u32,
        mut visit: F,
    ) where
        F: FnMut(usize, usize, &AttachedDeviceProgram),
    {
        let mut current = Some(node.clone());
        let mut count = 0;
        let mut depth = 0;
        while let Some(ancestor) = current {
            let state = ancestor.device_bpf.read();
            let (entries, mode) = if Arc::ptr_eq(&ancestor, changed) {
                (direct, flags)
            } else {
                (state.direct.as_slice(), state.flags)
            };
            if count == 0 || mode & BPF_F_ALLOW_MULTI != 0 {
                for (index, entry) in entries.iter().enumerate() {
                    visit(depth, index, entry);
                }
                count += entries.len();
            }
            current = ancestor.parent();
            depth += 1;
        }
    }

    fn prepare_device_snapshots(
        &self,
        changed: &Arc<CgroupNode>,
        direct: &[AttachedDeviceProgram],
        flags: u32,
    ) -> Result<DeviceSnapshotUpdates, SystemError> {
        let mut nodes = Vec::new();
        nodes.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        nodes.push(changed.clone());
        let mut index = 0;
        while index < nodes.len() {
            let current = nodes[index].clone();
            let child_count = current.children.read().len();
            nodes
                .try_reserve(child_count)
                .map_err(|_| SystemError::ENOMEM)?;
            nodes.extend(current.children.read().values().cloned());
            index += 1;
        }

        let mut snapshots = Vec::new();
        snapshots
            .try_reserve_exact(nodes.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for node in nodes {
            let mut preorder_count = 0usize;
            let mut normal_count = 0usize;
            Self::visit_effective_device_programs(&node, changed, direct, flags, |_, _, entry| {
                if entry.flags & BPF_DEVICE_F_PREORDER != 0 {
                    preorder_count += 1;
                } else {
                    normal_count += 1;
                }
            });

            let mut preorder = Vec::new();
            let mut normal = Vec::new();
            preorder
                .try_reserve_exact(preorder_count)
                .map_err(|_| SystemError::ENOMEM)?;
            normal
                .try_reserve_exact(normal_count)
                .map_err(|_| SystemError::ENOMEM)?;
            Self::visit_effective_device_programs(
                &node,
                changed,
                direct,
                flags,
                |depth, local_index, entry| {
                    if entry.flags & BPF_DEVICE_F_PREORDER != 0 {
                        preorder.push((Reverse(depth), local_index, entry.prog.clone()));
                    } else {
                        normal.push(entry.prog.clone());
                    }
                },
            );
            preorder.sort_unstable_by_key(|(depth, index, _)| (*depth, *index));
            let mut effective = Vec::new();
            effective
                .try_reserve_exact(preorder_count + normal_count)
                .map_err(|_| SystemError::ENOMEM)?;
            effective.extend(preorder.into_iter().map(|(_, _, prog)| prog));
            effective.extend(normal);
            snapshots.push((node, Arc::new(effective)));
        }
        Ok(snapshots)
    }

    fn publish_device_snapshots(updates: &mut DeviceSnapshotUpdates) {
        // Swapping leaves all old snapshots in `updates`, to be dropped only
        // after the structure lock is released by the caller.
        for (node, snapshot) in updates {
            core::mem::swap(&mut node.device_bpf.write().effective, snapshot);
        }
    }

    #[allow(dead_code)]
    pub fn find_or_create_path(&self, path: &str) -> Result<Arc<CgroupNode>, SystemError> {
        let rel = normalize_cgroup_abs_path(path)?;
        let mut cur = self.root();

        if rel.is_empty() {
            return Ok(cur);
        }

        for comp in rel.split('/') {
            if comp.is_empty() {
                continue;
            }
            cur = self.create_child(&cur, comp)?;
        }

        Ok(cur)
    }

    #[allow(dead_code)]
    pub fn find_path(&self, path: &str) -> Result<Arc<CgroupNode>, SystemError> {
        let rel = normalize_cgroup_abs_path(path)?;
        let mut cur = self.root();

        if rel.is_empty() {
            return Ok(cur);
        }

        for comp in rel.split('/') {
            if comp.is_empty() {
                continue;
            }
            let next = cur
                .children
                .read()
                .get(comp)
                .cloned()
                .ok_or(SystemError::ENOENT)?;
            cur = next;
        }

        Ok(cur)
    }
}

#[derive(Debug)]
pub struct TaskCgroupRef {
    node: Arc<CgroupNode>,
    css_set: Arc<CssSetToken>,
}

impl Clone for TaskCgroupRef {
    fn clone(&self) -> Self {
        self.css_set.acquire();
        Self {
            node: self.node.clone(),
            css_set: self.css_set.clone(),
        }
    }
}

impl Drop for TaskCgroupRef {
    fn drop(&mut self) {
        self.css_set.release();
    }
}

impl TaskCgroupRef {
    pub fn new(node: Arc<CgroupNode>) -> Self {
        let css_set = node.css_set();
        css_set.acquire();
        Self { node, css_set }
    }

    pub fn node(&self) -> Arc<CgroupNode> {
        self.node.clone()
    }

    pub fn css_set_id(&self) -> usize {
        self.css_set.id()
    }
}

lazy_static! {
    static ref CGROUP_ROOT: Arc<CgroupRoot> = CgroupRoot::new();
    static ref CGROUP_ACCOUNTING_LOCK: SpinLock<()> = SpinLock::new(());
}

pub fn cgroup_root() -> &'static Arc<CgroupRoot> {
    &CGROUP_ROOT
}

pub fn cgroup_root_node() -> Arc<CgroupNode> {
    CGROUP_ROOT.root()
}

pub fn cgroup_accounting_lock() -> &'static SpinLock<()> {
    &CGROUP_ACCOUNTING_LOCK
}

pub fn cgroup_path_relative_to_node(node: &Arc<CgroupNode>, view_root: &Arc<CgroupNode>) -> String {
    if !view_root.is_ancestor_of(node) {
        return "/".to_string();
    }

    let node_path = cgroup_path_components(node);
    let root_path = cgroup_path_components(view_root);

    let down = &node_path[root_path.len()..];

    if down.is_empty() {
        return "/".to_string();
    }

    format!("/{}", down.join("/"))
}

fn cgroup_path_projected_from_view(node: &Arc<CgroupNode>, view_root: &Arc<CgroupNode>) -> String {
    let node_path = cgroup_path_components(node);
    let root_path = cgroup_path_components(view_root);
    let common = cgroup_common_ancestor(node, view_root);
    let common_depth = cgroup_path_components(&common).len();

    let up = root_path.len().saturating_sub(common_depth);
    let down = &node_path[common_depth..];

    if up == 0 && down.is_empty() {
        return "/".to_string();
    }

    let mut parts = Vec::with_capacity(up + down.len());
    for _ in 0..up {
        parts.push("..".to_string());
    }
    parts.extend(down.iter().cloned());

    format!("/{}", parts.join("/"))
}

pub fn cgroup_path_from_view(node: &Arc<CgroupNode>, view_root: &Arc<CgroupNode>) -> String {
    cgroup_path_projected_from_view(node, view_root)
}

pub fn cgroup_common_ancestor(left: &Arc<CgroupNode>, right: &Arc<CgroupNode>) -> Arc<CgroupNode> {
    let mut cur = Some(left.clone());
    while let Some(node) = cur {
        if node.is_ancestor_of(right) {
            return node;
        }
        cur = node.parent();
    }
    cgroup_root_node()
}
//一个已经作为管理节点的node不能同时作为迁移目的地承载普通节点
pub fn cgroup_migrate_vet_dst(dst: &Arc<CgroupNode>) -> Result<(), SystemError> {
    // Callers hold CGROUP_ACCOUNTING_LOCK. rmdir takes the same lock before
    // removing the node from the online registry, so a successful migration
    // cannot attach a task to a directory which has already been removed.
    if !cgroup_root().is_online(dst) {
        return Err(SystemError::ENOENT);
    }
    // Domain controllers cannot manage tasks in a threaded subtree.
    if dst.in_threaded_subtree()
        && dst
            .subtree_control()
            .iter()
            .any(|ctrl| matches!(ctrl.as_str(), "memory" | "io"))
    {
        return Err(SystemError::EBUSY);
    }
    // Linux's no-internal-process rule applies to non-threaded domain
    // controllers. pids/cpu/cpuset are threaded; memory and io are domains.
    if dst.parent().is_some() && dst.has_domain_controllers() {
        return Err(SystemError::EBUSY);
    }
    Ok(())
}
//fork前pids.max检查
pub fn cgroup_can_fork_in(node: &Arc<CgroupNode>, new_tasks: usize) -> Result<(), SystemError> {
    if !cgroup_root().is_online(node) {
        return Err(SystemError::ENOENT);
    }
    let mut cur = Some(node.clone());
    while let Some(cg) = cur {
        if let Some(max) = cg.pids_max() {
            let used = cg.pids_current_count();
            if used.saturating_add(new_tasks) > max {
                cg.inc_pids_events_max();
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        }
        cur = cg.parent();
    }
    Ok(())
}

pub fn cgroup_migrate_vet_dst_with_src(
    _src: &Arc<CgroupNode>,
    dst: &Arc<CgroupNode>,
    _moved_tasks: usize,
) -> Result<(), SystemError> {
    // pids.max constrains fork/clone, not organizational migration.
    cgroup_migrate_vet_dst(dst)
}

#[allow(dead_code)]
pub fn find_or_create_node_by_abs_path(path: &str) -> Result<Arc<CgroupNode>, SystemError> {
    cgroup_root().find_or_create_path(path)
}

#[allow(dead_code)]
pub fn find_node_by_abs_path(path: &str) -> Result<Arc<CgroupNode>, SystemError> {
    cgroup_root().find_path(path)
}

fn cgroup_path_components(node: &Arc<CgroupNode>) -> Vec<String> {
    let mut rev = Vec::new();
    let mut cur = Some(node.clone());

    while let Some(n) = cur {
        if !n.name().is_empty() {
            rev.push(n.name().to_string());
        }
        cur = n.parent();
    }

    rev.reverse();
    rev
}

fn normalize_cgroup_abs_path(path: &str) -> Result<String, SystemError> {
    // 支持两种形式：
    // 1) cgroup v2 路径："/foo/bar"
    // 2) 绝对挂载路径："/sys/fs/cgroup/foo/bar"
    let rel = if let Some(stripped) = path.strip_prefix("/sys/fs/cgroup") {
        stripped
    } else {
        path
    };

    if rel.is_empty() {
        return Ok(String::new());
    }

    if !rel.starts_with('/') {
        return Err(SystemError::EINVAL);
    }

    let mut out = Vec::new();
    //单调栈处理..和.
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            if out.pop().is_none() {
                return Err(SystemError::EINVAL);
            }
            continue;
        }
        out.push(comp);
    }

    Ok(out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_path_from_view_same_node_is_root() {
        let root = CgroupRoot::new();
        let node = root.create_child(&root.root(), "same").unwrap();

        assert_eq!(cgroup_path_from_view(&node, &node), "/");
    }

    #[test]
    fn cgroup_path_from_view_descendant_stays_relative() {
        let root = CgroupRoot::new();
        let parent = root.create_child(&root.root(), "parent").unwrap();
        let child = root.create_child(&parent, "child").unwrap();

        assert_eq!(cgroup_path_from_view(&child, &parent), "/child");
    }

    #[test]
    fn cgroup_path_from_view_sibling_uses_parent_segments() {
        let root = CgroupRoot::new();
        let left = root.create_child(&root.root(), "left").unwrap();
        let right = root.create_child(&root.root(), "right").unwrap();

        assert_eq!(cgroup_path_from_view(&right, &left), "/../right");
    }

    #[test]
    fn add_remove_pair_keeps_subtree_counter_exact() {
        let root = CgroupRoot::new();
        let parent = root.create_child(&root.root(), "p").unwrap();
        let child = root.create_child(&parent, "c").unwrap();

        child.add_task(RawPid::new(101));
        assert_eq!(parent.subtree_task_counter().load(Ordering::Acquire), 1);
        child.remove_task(RawPid::new(101));
        assert_eq!(parent.subtree_task_counter().load(Ordering::Acquire), 0);
    }

    /// 固定住幽灵迁移路径的第一半：do_exit 的 remove_task 先摘除成员后，
    /// 任何对同一 pid 的二次 remove_task（修复前 write_procs 对正退出任务
    /// 执行 `set_task_cgroup_node` 时对 src 的重复摘除即此形态）在 debug
    /// 内核必须命中 debug_assert panic——这正是 issue #29 的崩溃点；
    /// 锁内二次复核把该路径在到达这里之前剔除。
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "does not exist")]
    fn repeated_remove_task_of_dead_member_panics_under_debug_kernel() {
        let root = CgroupRoot::new();
        let src = root.create_child(&root.root(), "src").unwrap();
        src.add_task(RawPid::new(7));
        src.remove_task(RawPid::new(7)); // do_exit 锁内摘除
        src.remove_task(RawPid::new(7)); // 迁移侧迟到的重复摘除 → panic
    }

    /// release 内核（debug_assert 被编译掉）下同一失配不 panic，成员集
    /// 未命中提前返回，祖先 subtree_task_counter 保持 0，不翻转成
    /// usize::MAX——修复前的翻转会让 is_populated/rmdir 与 domain 判定
    /// 永久失真（EBUSY）。
    #[cfg(not(debug_assertions))]
    #[test]
    fn repeated_remove_task_does_not_wrap_subtree_counter() {
        let root = CgroupRoot::new();
        let parent = root.create_child(&root.root(), "p").unwrap();
        let src = root.create_child(&parent, "src").unwrap();
        src.add_task(RawPid::new(7));
        src.remove_task(RawPid::new(7));
        src.remove_task(RawPid::new(7));
        assert_eq!(parent.subtree_task_counter().load(Ordering::Acquire), 0);
    }

    #[test]
    fn saturating_sub_stops_at_zero() {
        let counter = AtomicUsize::new(2);
        saturating_sub(&counter);
        saturating_sub(&counter);
        saturating_sub(&counter);
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }
}
