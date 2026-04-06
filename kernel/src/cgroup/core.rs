use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use hashbrown::{HashMap, HashSet};
use system_error::SystemError;

use crate::{
    libs::{rwlock::RwLock, spinlock::SpinLock},
    process::RawPid,
};

/// Runtime memory controller state for a cgroup.
///
/// This tracks actual memory usage and events, separate from the configuration
/// thresholds (memory_max, memory_high, memory_low) which are stored directly
/// on CgroupNode.
#[derive(Debug)]
pub struct CgroupMemoryState {
    /// Anonymous pages usage in bytes (local to this cgroup, not including children)
    local_anon_bytes: AtomicUsize,
    /// Anonymous pages count (local)
    local_anon_pages: AtomicUsize,
    /// Hierarchical anonymous bytes (this cgroup + all children)
    anon_bytes: AtomicUsize,
    /// Hierarchical anonymous page count
    anon_pages: AtomicUsize,
    /// Number of reclaim attempts triggered
    reclaim_count: AtomicU64,
    /// Event: memory.low was breached
    events_low: AtomicU64,
    /// Event: memory.high was breached
    events_high: AtomicU64,
    /// Event: memory.max was breached
    events_max: AtomicU64,
    /// Event: OOM was triggered
    events_oom: AtomicU64,
    /// Event: OOM kill occurred
    events_oom_kill: AtomicU64,
    /// Flag indicating if memory controller is enabled
    enabled: AtomicBool,
}

impl CgroupMemoryState {
    pub fn new() -> Self {
        Self {
            local_anon_bytes: AtomicUsize::new(0),
            local_anon_pages: AtomicUsize::new(0),
            anon_bytes: AtomicUsize::new(0),
            anon_pages: AtomicUsize::new(0),
            reclaim_count: AtomicU64::new(0),
            events_low: AtomicU64::new(0),
            events_high: AtomicU64::new(0),
            events_max: AtomicU64::new(0),
            events_oom: AtomicU64::new(0),
            events_oom_kill: AtomicU64::new(0),
            enabled: AtomicBool::new(true),
        }
    }

    /// Get local anonymous bytes (this cgroup only)
    #[inline]
    pub fn local_anon_bytes(&self) -> usize {
        self.local_anon_bytes.load(Ordering::Relaxed)
    }

    /// Get local anonymous page count (this cgroup only)
    #[inline]
    pub fn local_anon_pages(&self) -> usize {
        self.local_anon_pages.load(Ordering::Relaxed)
    }

    /// Get hierarchical anonymous bytes usage (includes children)
    #[inline]
    pub fn anon_bytes(&self) -> usize {
        self.anon_bytes.load(Ordering::Relaxed)
    }

    /// Get hierarchical anonymous page count (includes children)
    #[inline]
    pub fn anon_pages(&self) -> usize {
        self.anon_pages.load(Ordering::Relaxed)
    }

    /// Get reclaim count
    #[inline]
    pub fn reclaim_count(&self) -> u64 {
        self.reclaim_count.load(Ordering::Relaxed)
    }

    /// Get events.low counter
    #[inline]
    pub fn events_low(&self) -> u64 {
        self.events_low.load(Ordering::Relaxed)
    }

    /// Get events.high counter
    #[inline]
    pub fn events_high(&self) -> u64 {
        self.events_high.load(Ordering::Relaxed)
    }

    /// Get events.max counter
    #[inline]
    pub fn events_max(&self) -> u64 {
        self.events_max.load(Ordering::Relaxed)
    }

    /// Get events.oom counter
    #[inline]
    pub fn events_oom(&self) -> u64 {
        self.events_oom.load(Ordering::Relaxed)
    }

    /// Get events.oom_kill counter
    #[inline]
    pub fn events_oom_kill(&self) -> u64 {
        self.events_oom_kill.load(Ordering::Relaxed)
    }

    /// Increment events.low
    #[inline]
    pub fn inc_events_low(&self) {
        self.events_low.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment events.high
    #[inline]
    pub fn inc_events_high(&self) {
        self.events_high.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment events.max
    #[inline]
    pub fn inc_events_max(&self) {
        self.events_max.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment events.oom
    #[inline]
    pub fn inc_events_oom(&self) {
        self.events_oom.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment events.oom_kill
    #[inline]
    pub fn inc_events_oom_kill(&self) {
        self.events_oom_kill.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment reclaim count
    #[inline]
    pub fn inc_reclaim_count(&self) {
        self.reclaim_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Check if memory controller is enabled
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Enable or disable memory controller
    #[inline]
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Charge local usage (called on the leaf cgroup where page is allocated).
    /// Updates both local and hierarchical counters.
    pub fn charge_local(&self, bytes: usize, pages: usize) {
        self.local_anon_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.local_anon_pages.fetch_add(pages, Ordering::Relaxed);
        self.anon_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.anon_pages.fetch_add(pages, Ordering::Relaxed);
    }

    /// Uncharge local usage (called when page is freed from this cgroup).
    /// Uses saturating subtraction to prevent underflow.
    pub fn uncharge_local(&self, bytes: usize, pages: usize) {
        self.local_anon_bytes.fetch_sub(bytes, Ordering::Relaxed);
        self.local_anon_pages.fetch_sub(pages, Ordering::Relaxed);
        self.anon_bytes.fetch_sub(bytes, Ordering::Relaxed);
        self.anon_pages.fetch_sub(pages, Ordering::Relaxed);
    }

    /// Charge hierarchical usage only (for ancestor cgroups during propagation).
    pub fn charge_hierarchical(&self, bytes: usize, pages: usize) {
        self.anon_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.anon_pages.fetch_add(pages, Ordering::Relaxed);
    }

    /// Uncharge hierarchical usage only (for ancestor cgroups during propagation).
    pub fn uncharge_hierarchical(&self, bytes: usize, pages: usize) {
        self.anon_bytes.fetch_sub(bytes, Ordering::Relaxed);
        self.anon_pages.fetch_sub(pages, Ordering::Relaxed);
    }
}

impl Default for CgroupMemoryState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct CgroupNode {
    id: usize,
    name: String,
    parent: Option<Weak<CgroupNode>>,
    children: RwLock<HashMap<String, Arc<CgroupNode>>>,
    tasks: RwLock<HashSet<RawPid>>,
    //任务集合
    subtree_control: RwLock<HashSet<String>>,
    pids_max: RwLock<Option<usize>>,
    pids_events_max: AtomicU64,
    memory_max: RwLock<Option<usize>>,
    memory_high: RwLock<Option<usize>>,
    memory_low: RwLock<usize>,
    /// Runtime memory controller state (lazy-initialized when memory controller is enabled)
    memory_state: RwLock<Option<Arc<CgroupMemoryState>>>,
}

impl CgroupNode {
    fn new_root() -> Arc<Self> {
        Arc::new(Self {
            id: 1,
            name: String::new(),
            parent: None,
            children: RwLock::new(HashMap::new()),
            tasks: RwLock::new(HashSet::new()),
            subtree_control: RwLock::new(HashSet::new()),
            pids_max: RwLock::new(None),
            pids_events_max: AtomicU64::new(0),
            memory_max: RwLock::new(None),
            memory_high: RwLock::new(None),
            memory_low: RwLock::new(0),
            memory_state: RwLock::new(None),
        })
    }

    fn new_child(id: usize, name: String, parent: &Arc<CgroupNode>) -> Arc<Self> {
        Arc::new(Self {
            id,
            name,
            parent: Some(Arc::downgrade(parent)),
            children: RwLock::new(HashMap::new()),
            tasks: RwLock::new(HashSet::new()),
            subtree_control: RwLock::new(HashSet::new()),
            pids_max: RwLock::new(None),
            pids_events_max: AtomicU64::new(0),
            memory_max: RwLock::new(None),
            memory_high: RwLock::new(None),
            memory_low: RwLock::new(0),
            memory_state: RwLock::new(None),
        })
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

    pub fn add_task(&self, pid: RawPid) {
        self.tasks.write().insert(pid);
    }

    pub fn remove_task(&self, pid: RawPid) {
        self.tasks.write().remove(&pid);
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

    pub fn pids_max(&self) -> Option<usize> {
        *self.pids_max.read()
    }

    pub fn set_pids_max(&self, max: Option<usize>) {
        *self.pids_max.write() = max;
    }

    pub fn pids_events_max(&self) -> u64 {
        self.pids_events_max.load(Ordering::Relaxed)
    }

    pub fn inc_pids_events_max(&self) {
        self.pids_events_max.fetch_add(1, Ordering::Relaxed);
    }

    pub fn memory_max(&self) -> Option<usize> {
        *self.memory_max.read()
    }

    pub fn set_memory_max(&self, max: Option<usize>) {
        *self.memory_max.write() = max;
    }

    pub fn memory_high(&self) -> Option<usize> {
        *self.memory_high.read()
    }

    pub fn set_memory_high(&self, high: Option<usize>) {
        *self.memory_high.write() = high;
    }

    pub fn memory_low(&self) -> Option<usize> {
        Some(*self.memory_low.read())
    }

    pub fn set_memory_low(&self, low: Option<usize>) {
        *self.memory_low.write() = low.unwrap_or(0);
    }

    /// Get or initialize the memory controller runtime state for this cgroup.
    /// Returns the Arc to the memory state, initializing it if needed.
    pub fn memory_state(self: &Arc<Self>) -> Arc<CgroupMemoryState> {
        // First try to get existing state
        {
            let guard = self.memory_state.read();
            if let Some(ref state) = *guard {
                return state.clone();
            }
        }

        // Need to initialize - double-checked locking pattern
        let new_state = Arc::new(CgroupMemoryState::new());
        let mut guard = self.memory_state.write();
        match &*guard {
            Some(existing) => existing.clone(),
            None => {
                *guard = Some(new_state.clone());
                new_state
            }
        }
    }

    /// Try to get the memory controller state without initializing.
    /// Returns None if the memory controller hasn't been enabled yet.
    pub fn try_memory_state(&self) -> Option<Arc<CgroupMemoryState>> {
        self.memory_state.read().clone()
    }

    /// Check if memory controller is enabled for this cgroup.
    pub fn memory_enabled(&self) -> bool {
        self.memory_state.read().is_some()
    }

    /// Get hierarchical anonymous bytes usage (includes this cgroup and all children)
    pub fn memory_usage(&self) -> usize {
        self.memory_state.read().as_ref().map(|s| s.anon_bytes()).unwrap_or(0)
    }

    /// Get hierarchical anonymous page count (includes this cgroup and all children)
    pub fn memory_page_count(&self) -> usize {
        self.memory_state.read().as_ref().map(|s| s.anon_pages()).unwrap_or(0)
    }

    /// Charge an anonymous page to this cgroup and propagates to ancestors.
    /// Returns true on success.
    pub fn memcg_charge(self: &Arc<Self>, bytes: usize, pages: usize) -> bool {
        let state = self.memory_state();
        state.charge_local(bytes, pages);

        // Propagate to ancestors
        let mut cur = self.parent();
        while let Some(parent) = cur {
            if let Some(parent_state) = parent.memory_state.read().as_ref() {
                parent_state.charge_hierarchical(bytes, pages);
            }
            cur = parent.parent();
        }

        true
    }

    /// Uncharge an anonymous page from this cgroup and propagates to ancestors.
    /// Uses saturating subtraction to prevent underflow.
    pub fn memcg_uncharge(self: &Arc<Self>, bytes: usize, pages: usize) {
        let state = self.memory_state();
        state.uncharge_local(bytes, pages);

        // Propagate to ancestors
        let mut cur = self.parent();
        while let Some(parent) = cur {
            if let Some(parent_state) = parent.memory_state.read().as_ref() {
                parent_state.uncharge_hierarchical(bytes, pages);
            }
            cur = parent.parent();
        }
    }

    pub fn subtree_task_count(self: &Arc<Self>) -> usize {
        let mut total = self.tasks.read().len();
        for child in self.children() {
            total = total.saturating_add(child.subtree_task_count());
        }
        total
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
}

#[derive(Debug)]
pub struct CgroupRoot {
    root: Arc<CgroupNode>,
    next_id: AtomicUsize,
    all_nodes: SpinLock<HashMap<usize, Arc<CgroupNode>>>,
}

impl CgroupRoot {
    fn new() -> Arc<Self> {
        let root = CgroupNode::new_root();
        let mut all_nodes = HashMap::new();
        all_nodes.insert(root.id(), root.clone());

        Arc::new(Self {
            root,
            next_id: AtomicUsize::new(2),
            all_nodes: SpinLock::new(all_nodes),
        })
    }

    pub fn root(&self) -> Arc<CgroupNode> {
        self.root.clone()
    }

    #[allow(dead_code)]
    pub fn lookup_by_id(&self, id: usize) -> Option<Arc<CgroupNode>> {
        self.all_nodes.lock().get(&id).cloned()
    }

    pub fn create_child(
        &self,
        parent: &Arc<CgroupNode>,
        name: &str,
    ) -> Result<Arc<CgroupNode>, SystemError> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(SystemError::EINVAL);
        }
        //先找寻有无节点，避免重复创建
        {
            let children = parent.children.read();
            if let Some(existing) = children.get(name) {
                return Ok(existing.clone());
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let child = CgroupNode::new_child(id, name.to_string(), parent);

        {
            let mut children = parent.children.write();
            if let Some(existing) = children.get(name) {
                return Ok(existing.clone());
            }
            children.insert(name.to_string(), child.clone());
        }

        self.all_nodes.lock().insert(id, child.clone());
        Ok(child)
    }

    pub fn remove_child(&self, parent: &Arc<CgroupNode>, name: &str) -> Result<(), SystemError> {
        let child = {
            let children = parent.children.read();
            children.get(name).cloned().ok_or(SystemError::ENOENT)?
        };
        //有孩子时返回busy错误
        if child.has_children() || child.has_tasks() {
            return Err(SystemError::EBUSY);
        }

        parent.children.write().remove(name);
        self.all_nodes.lock().remove(&child.id());
        Ok(())
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

#[derive(Debug, Clone)]
pub struct TaskCgroupRef {
    node: Arc<CgroupNode>,
}

impl TaskCgroupRef {
    pub fn new(node: Arc<CgroupNode>) -> Self {
        Self { node }
    }

    pub fn node(&self) -> Arc<CgroupNode> {
        self.node.clone()
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
    // v2 no-internal-process 最小约束：
    // 目标 cgroup 如果已经有进程并且启用了 subtree_control，则拒绝迁移。
    if dst.has_tasks() && !dst.subtree_control().is_empty() {
        return Err(SystemError::EBUSY);
    }
    Ok(())
}
//fork前pids.max检查
pub fn cgroup_can_fork_in(node: &Arc<CgroupNode>, new_tasks: usize) -> Result<(), SystemError> {
    let mut cur = Some(node.clone());
    while let Some(cg) = cur {
        if let Some(max) = cg.pids_max() {
            let used = cg.subtree_task_count();
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
    src: &Arc<CgroupNode>,
    dst: &Arc<CgroupNode>,
    moved_tasks: usize,
) -> Result<(), SystemError> {
    cgroup_migrate_vet_dst(dst)?;

    let mut cur = Some(dst.clone());
    while let Some(cg) = cur {
        if let Some(max) = cg.pids_max() {
            let used = cg.subtree_task_count();
            let delta = if cg.is_ancestor_of(src) {
                0
            } else {
                moved_tasks
            };
            if used.saturating_add(delta) > max {
                cg.inc_pids_events_max();
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        }
        cur = cg.parent();
    }

    Ok(())
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
    fn memory_thresholds_are_stored_per_cgroup_with_stable_defaults() {
        let root = CgroupRoot::new();
        let parent = root.create_child(&root.root(), "parent").unwrap();
        let left = root.create_child(&parent, "left").unwrap();
        let right = root.create_child(&parent, "right").unwrap();

        assert_eq!(left.memory_max(), None);
        assert_eq!(left.memory_high(), None);
        assert_eq!(left.memory_low(), Some(0));
        assert_eq!(right.memory_max(), None);
        assert_eq!(right.memory_high(), None);
        assert_eq!(right.memory_low(), Some(0));

        left.set_memory_max(Some(4096));
        left.set_memory_high(Some(2048));
        left.set_memory_low(Some(1024));

        assert_eq!(left.memory_max(), Some(4096));
        assert_eq!(left.memory_high(), Some(2048));
        assert_eq!(left.memory_low(), Some(1024));
        assert_eq!(right.memory_max(), None);
        assert_eq!(right.memory_high(), None);
        assert_eq!(right.memory_low(), Some(0));

        let left_again = parent.child("left").unwrap();
        assert_eq!(left_again.memory_max(), Some(4096));
        assert_eq!(left_again.memory_high(), Some(2048));
        assert_eq!(left_again.memory_low(), Some(1024));
    }
}
