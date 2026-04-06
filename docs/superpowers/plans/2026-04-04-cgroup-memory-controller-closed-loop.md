# cgroup Memory Controller Closed-Loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 DragonOS 中把 cgroup v2 memory controller 从静态文件节点推进到匿名页真实记账、层级统计、动态 `memory.*` 输出，并通过 Nix + QEMU 启动验证基础行为。

**Architecture:** 以 `kernel/src/cgroup/core.rs` 为 memory controller 状态与策略中心，在 `kernel/src/mm/page.rs` 挂页级 memcg 归属，在 `kernel/src/mm/fault.rs` 与 `kernel/src/mm/ucontext.rs` 接入匿名页 charge/uncharge，再由 `kernel/src/filesystem/cgroup2/mod.rs` 动态导出 `memory.current/stat/events`。本轮先完成“真实记账 + 动态观测面”，并把 `high/max/oom` 做成可观测的最小 enforcement 骨架，运行期验证按 `docs/introduction/develop_nix.md` 通过 Nix/QEMU 启动内核。

**Tech Stack:** Rust no_std kernel、cgroup v2、DragonOS mm/page fault 子系统、C 集成测试、Nix、QEMU

---

## File Map

### Core runtime state
- Modify: `kernel/src/cgroup/core.rs`
  - 为 `CgroupNode` 增加 `CgroupMemoryState`
  - 增加层级 charge/uncharge、events/stat 访问接口
  - 增加单元测试
- Modify: `kernel/src/cgroup/mod.rs`
  - 导出新增 memcg 帮助函数/类型

### MM charging / page ownership
- Modify: `kernel/src/mm/page.rs`
  - 给 `InnerPage` 增加 memcg 归属元数据
  - 提供设置/读取/清空页归属的方法
- Modify: `kernel/src/mm/fault.rs`
  - 在匿名缺页与匿名 COW 路径接入 charge
- Modify: `kernel/src/mm/ucontext.rs`
  - 在 unmap / VMA extract / 地址空间 teardown 路径接入 uncharge
- Modify: `kernel/src/process/mod.rs`
  - 进程退出时确保 user vm teardown 触发 uncharge 后再丢弃引用

### cgroup v2 file interface
- Modify: `kernel/src/filesystem/cgroup2/mod.rs`
  - 让 `memory.current/stat/events` 动态输出
  - 在该文件现有 `#[cfg(test)]` 测试块中增加动态输出回归测试
  - 为 `memory.high/max` 命中计数与最小 enforcement 骨架预留接线

### Integration tests
- Modify: `user/apps/c_unitest/test_cgroup_mvp_basic.c`
  - 保留已有 smoke 测试
  - 增加匿名页分配/释放后的 `memory.current`、`memory.stat`、`memory.events` 验证
- Create: `user/apps/c_unitest/test_cgroup_memory_accounting.c`
  - 专门覆盖 charge/uncharge、层级聚合、迁移后“历史页不迁移”语义

### Verification commands
- Build check: `nix develop -c make kernel`
- Userland build: `nix develop -c make -C user/apps/c_unitest test_cgroup_mvp_basic ARCH=x86_64 && nix develop -c make -C user/apps/c_unitest test_cgroup_memory_accounting ARCH=x86_64`
- Rootfs refresh: `nix run .#rootfs-x86_64`
- Boot/run: `nix run .#start-x86_64`
- In-guest verification: `/opt/test_cgroup_mvp_basic` and `/opt/test_cgroup_memory_accounting`

> **Current repo baseline:** `nix develop -c cargo test --manifest-path kernel/Cargo.toml --lib --no-run` 目前会先被仓库内已有的 host-test 问题阻塞（如 `Box`/`Vec` 导入缺失、`duplicate lang item: eh_personality`），因此本计划把内核侧 `#[test]` 代码视为与实现同行的回归用例草图，实际 gating 以 `nix develop -c make kernel` + QEMU 内集成测试为准。

---

### Task 1: 为 CgroupNode 建立 memory runtime state

**Files:**
- Modify: `kernel/src/cgroup/core.rs:16-179`
- Modify: `kernel/src/cgroup/mod.rs:1-8`
- Test: `kernel/src/cgroup/core.rs:499-559`

- [ ] **Step 1: 写失败的 cgroup core 单测，定义 memory state 默认值和层级聚合语义**

```rust
#[test]
fn memory_accounting_state_defaults_and_hierarchical_updates() {
    let root = CgroupRoot::new();
    let parent = root.create_child(&root.root(), "parent").unwrap();
    let child = root.create_child(&parent, "child").unwrap();

    assert_eq!(parent.memory_current(), 0);
    assert_eq!(child.memory_current(), 0);
    assert_eq!(child.memory_stat_anon(), 0);
    assert_eq!(child.memory_events().high, 0);

    child.memcg_charge_hierarchy(4096);
    assert_eq!(child.memory_current(), 4096);
    assert_eq!(parent.memory_current(), 4096);
    assert_eq!(child.memory_stat_anon(), 4096);

    child.memcg_uncharge_hierarchy(4096);
    assert_eq!(child.memory_current(), 0);
    assert_eq!(parent.memory_current(), 0);
}
```

- [ ] **Step 2: 运行单测，确认当前失败**

Run: `nix develop -c make kernel`
Expected: PASS，确认内核能编译通过；若同时在当前环境能稳定跑通 host-side `cargo test`，可额外执行，但不作为本轮硬门槛。

- [ ] **Step 3: 在 `CgroupNode` 中增加 runtime state 结构与访问接口**

```rust
#[derive(Debug, Default, Clone)]
pub struct CgroupMemoryEvents {
    pub low: u64,
    pub high: u64,
    pub max: u64,
    pub oom: u64,
    pub oom_kill: u64,
}

#[derive(Debug, Default)]
pub struct CgroupMemoryState {
    anon_bytes: usize,
    anon_pages: usize,
    reclaim_count: u64,
    events: CgroupMemoryEvents,
}

impl CgroupNode {
    pub fn memory_current(&self) -> usize {
        self.memory_state.read().anon_bytes
    }

    pub fn memory_stat_anon(&self) -> usize {
        self.memory_state.read().anon_bytes
    }

    pub fn memory_reclaim_count(&self) -> u64 {
        self.memory_state.read().reclaim_count
    }

    pub fn memory_events(&self) -> CgroupMemoryEvents {
        self.memory_state.read().events.clone()
    }

    pub fn memcg_charge_hierarchy(self: &Arc<Self>, bytes: usize) {
        let pages = bytes / crate::arch::MMArch::PAGE_SIZE;
        let mut cur = Some(self.clone());
        while let Some(node) = cur {
            let mut guard = node.memory_state.write();
            guard.anon_bytes = guard.anon_bytes.saturating_add(bytes);
            guard.anon_pages = guard.anon_pages.saturating_add(pages);
            drop(guard);
            cur = node.parent();
        }
    }

    pub fn memcg_uncharge_hierarchy(self: &Arc<Self>, bytes: usize) {
        let pages = bytes / crate::arch::MMArch::PAGE_SIZE;
        let mut cur = Some(self.clone());
        while let Some(node) = cur {
            let mut guard = node.memory_state.write();
            guard.anon_bytes = guard.anon_bytes.saturating_sub(bytes);
            guard.anon_pages = guard.anon_pages.saturating_sub(pages);
            drop(guard);
            cur = node.parent();
        }
    }
}
```

- [ ] **Step 4: 在 `CgroupNode` 结构里挂上该 state，并导出类型**

```rust
pub struct CgroupNode {
    id: usize,
    name: String,
    parent: Option<Weak<CgroupNode>>,
    children: RwLock<HashMap<String, Arc<CgroupNode>>>,
    tasks: RwLock<HashSet<RawPid>>,
    subtree_control: RwLock<HashSet<String>>,
    pids_max: RwLock<Option<usize>>,
    pids_events_max: AtomicU64,
    memory_max: RwLock<Option<usize>>,
    memory_high: RwLock<Option<usize>>,
    memory_low: RwLock<usize>,
    memory_state: RwLock<CgroupMemoryState>,
}
```

```rust
pub use core::{
    cgroup_accounting_lock, cgroup_can_fork_in, cgroup_common_ancestor,
    cgroup_migrate_vet_dst, cgroup_migrate_vet_dst_with_src, cgroup_path_from_view,
    cgroup_path_relative_to_node, cgroup_root, cgroup_root_node, find_node_by_abs_path,
    find_or_create_node_by_abs_path, CgroupMemoryEvents, CgroupNode, CgroupRoot, TaskCgroupRef,
};
```

- [ ] **Step 5: 运行内核单测，确认新接口通过**

Run: `nix develop -c make kernel`
Expected: PASS，确认新接口可编译进内核；若 host-side 单测环境已被额外修通，再补跑对应 `cargo test`。

- [ ] **Step 6: 提交这一小步**

```bash
git add kernel/src/cgroup/core.rs kernel/src/cgroup/mod.rs
git commit -m "feat: add hierarchical memcg runtime state"
```

### Task 2: 为 cgroup state 增加动态 stat/events 输出所需接口

**Files:**
- Modify: `kernel/src/cgroup/core.rs`
- Test: `kernel/src/cgroup/core.rs`

- [ ] **Step 1: 写失败的 stat/events 单测**

```rust
#[test]
fn memory_stat_and_events_are_reported_from_runtime_state() {
    let root = CgroupRoot::new();
    let cg = root.create_child(&root.root(), "memcg").unwrap();

    cg.memcg_charge_hierarchy(4096);
    cg.memcg_inc_high();
    cg.memcg_inc_max();
    cg.memcg_inc_oom();
    cg.memcg_inc_oom_kill();
    cg.memcg_inc_reclaim(1);

    assert_eq!(cg.memory_stat_anon(), 4096);
    assert_eq!(cg.memory_reclaim_count(), 1);

    let events = cg.memory_events();
    assert_eq!(events.high, 1);
    assert_eq!(events.max, 1);
    assert_eq!(events.oom, 1);
    assert_eq!(events.oom_kill, 1);
}
```

- [ ] **Step 2: 运行单测，确认失败**

Run: `nix develop -c make kernel`
Expected: PASS，确认事件计数接口编译通过；若本地额外修通 host-side 单测环境，再补跑对应 `cargo test`。

- [ ] **Step 3: 实现 events/reclaim 计数接口**

```rust
impl CgroupNode {
    pub fn memcg_inc_low(&self) {
        self.memory_state.write().events.low += 1;
    }

    pub fn memcg_inc_high(&self) {
        self.memory_state.write().events.high += 1;
    }

    pub fn memcg_inc_max(&self) {
        self.memory_state.write().events.max += 1;
    }

    pub fn memcg_inc_oom(&self) {
        self.memory_state.write().events.oom += 1;
    }

    pub fn memcg_inc_oom_kill(&self) {
        self.memory_state.write().events.oom_kill += 1;
    }

    pub fn memcg_inc_reclaim(&self, pages: usize) {
        self.memory_state.write().reclaim_count += pages as u64;
    }
}
```

- [ ] **Step 4: 运行单测确认通过**

Run: `nix develop -c make kernel`
Expected: PASS，确认 events/reclaim 计数接口可编译进内核；行为正确性由后续动态文件读取与 QEMU 集成测试覆盖。

- [ ] **Step 5: 提交这一小步**

```bash
git add kernel/src/cgroup/core.rs
git commit -m "feat: add memcg stat and event counters"
```

### Task 3: 给匿名页增加 memcg 归属元数据

**Files:**
- Modify: `kernel/src/mm/page.rs:620-779`
- Test: `kernel/src/mm/page.rs`（新增 `#[cfg(test)] mod tests`）

- [ ] **Step 1: 先写页归属单测**

```rust
#[test]
fn anonymous_page_tracks_original_memcg_owner() {
    let root = crate::cgroup::core::CgroupRoot::new();
    let cg = root.create_child(&root.root(), "leaf").unwrap();
    let page = Page::new(
        crate::mm::PhysAddr::new(0x1000),
        PageType::Normal,
        PageFlags::empty(),
    );

    page.write().set_memcg(Some(alloc::sync::Arc::downgrade(&cg)));
    assert_eq!(page.read().memcg_id(), Some(cg.id()));

    page.write().clear_memcg();
    assert_eq!(page.read().memcg_id(), None);
}
```

- [ ] **Step 2: 运行内核单测，确认失败**

Run: `nix develop -c make kernel`
Expected: PASS，页归属接口与动态读取测试代码都能编译通过，行为验证留到后续 QEMU 步骤。

- [ ] **Step 3: 在 `InnerPage` 增加 memcg 归属字段与访问器**

```rust
pub struct InnerPage {
    vma_set: HashSet<Arc<LockedVMA>>,
    flags: PageFlags,
    phys_addr: PhysAddr,
    page_type: PageType,
    memcg: Option<Weak<crate::cgroup::CgroupNode>>,
}

impl InnerPage {
    pub fn set_memcg(&mut self, memcg: Option<Weak<crate::cgroup::CgroupNode>>) {
        self.memcg = memcg;
    }

    pub fn memcg(&self) -> Option<Arc<crate::cgroup::CgroupNode>> {
        self.memcg.as_ref().and_then(|cg| cg.upgrade())
    }

    pub fn memcg_id(&self) -> Option<usize> {
        self.memcg().map(|cg| cg.id())
    }

    pub fn clear_memcg(&mut self) {
        self.memcg = None;
    }
}
```

- [ ] **Step 4: 让 `InnerPage::new` / `Page::copy` 初始化该字段**

```rust
pub fn new(phys_addr: PhysAddr, page_type: PageType, flags: PageFlags) -> Self {
    Self {
        vma_set: HashSet::new(),
        flags,
        phys_addr,
        page_type,
        memcg: None,
    }
}
```

- [ ] **Step 5: 运行单测确认通过**

Run: `nix develop -c make kernel`
Expected: PASS，页归属字段与访问器编译通过。

- [ ] **Step 6: 提交这一小步**

```bash
git add kernel/src/mm/page.rs
git commit -m "feat: record original memcg on anonymous pages"
```

### Task 4: 在匿名缺页路径接入 charge

**Files:**
- Modify: `kernel/src/mm/fault.rs:238-300`
- Modify: `kernel/src/cgroup/core.rs`
- Test: `kernel/src/filesystem/cgroup2/mod.rs`

- [ ] **Step 1: 写失败的 cgroup2 动态 `memory.current` 测试**

```rust
#[test]
fn memory_current_reads_runtime_usage() {
    let cg = cgroup_root_node();
    cg.memcg_charge_hierarchy(8192);

    let file = Cgroup2Inode::new_file(
        "memory.current".to_string(),
        cg.clone(),
        CgroupCoreFile::MemoryCurrent,
        b"0\n",
    );

    assert_eq!(read_all(&file), "8192\n");

    cg.memcg_uncharge_hierarchy(8192);
    assert_eq!(read_all(&file), "0\n");
}
```

- [ ] **Step 2: 运行单测确认当前失败**

Run: `nix develop -c make kernel`
Expected: PASS，匿名 charge 路径编译通过；动态 `memory.current` 的行为仍留待下一步接通。

- [ ] **Step 3: 在匿名缺页路径先完成最小 charge 接口调用**

```rust
let current_memcg = ProcessManager::current_pcb().task_cgroup_node();
current_memcg.memcg_charge_hierarchy(MMArch::PAGE_SIZE);

let paddr = mapper.translate(address).unwrap().0;
let mut page_manager_guard = page_manager_lock();
let page = page_manager_guard.get_unwrap(&paddr);
{
    let mut guard = page.write();
    guard.set_memcg(Some(alloc::sync::Arc::downgrade(&current_memcg)));
    guard.insert_vma(vma.clone());
}
```

- [ ] **Step 4: 对匿名共享页路径也写入归属**

```rust
let current_memcg = ProcessManager::current_pcb().task_cgroup_node();
current_memcg.memcg_charge_hierarchy(MMArch::PAGE_SIZE);
let mut guard = page.write();
guard.set_memcg(Some(alloc::sync::Arc::downgrade(&current_memcg)));
guard.insert_vma(vma.clone());
```

- [ ] **Step 5: 运行 cgroup2 单测确认 `memory.current` 仍失败，准备下一步修 fs 读取**

Run: `nix develop -c make kernel`
Expected: PASS，仅 `memory.current` 动态读取尚未接通，行为验证留到下一步。

- [ ] **Step 6: 提交这一小步**

```bash
git add kernel/src/mm/fault.rs kernel/src/cgroup/core.rs kernel/src/filesystem/cgroup2/mod.rs
git commit -m "feat: charge anonymous page faults to current memcg"
```

### Task 5: 让 `memory.current` 改为动态读取 runtime state

**Files:**
- Modify: `kernel/src/filesystem/cgroup2/mod.rs:720-742`
- Test: `kernel/src/filesystem/cgroup2/mod.rs`

- [ ] **Step 1: 修复 `memory.current` 的读取实现**

```rust
CgroupCoreFile::MemoryCurrent => {
    format!("{}\n", cgroup.memory_current()).into_bytes()
}
```

- [ ] **Step 2: 运行单测确认通过**

Run: `nix develop -c make kernel`
Expected: PASS，动态 `memory.current` 读取逻辑编译通过；后续以 QEMU 内测试验证行为。

- [ ] **Step 3: 提交这一小步**

```bash
git add kernel/src/filesystem/cgroup2/mod.rs
git commit -m "feat: render memory.current from memcg state"
```

### Task 6: 在 unmap / split / exit 路径接入 uncharge

**Files:**
- Modify: `kernel/src/mm/ucontext.rs:1719-1738`
- Modify: `kernel/src/process/mod.rs:726-744`
- Modify: `kernel/src/cgroup/core.rs`
- Test: `kernel/src/filesystem/cgroup2/mod.rs`

- [ ] **Step 1: 写失败的 uncharge 单测**

```rust
#[test]
fn memory_current_drops_after_uncharge() {
    let cg = cgroup_root_node();
    cg.memcg_charge_hierarchy(4096);
    assert_eq!(cg.memory_current(), 4096);

    cg.memcg_uncharge_hierarchy(4096);
    assert_eq!(cg.memory_current(), 0);
}
```

- [ ] **Step 2: 运行单测确认失败或只覆盖纯 state，不覆盖页路径**

Run: `nix develop -c make kernel`
Expected: PASS/FAIL 均可接受；若内核能编译通过，则继续补页级 uncharge 逻辑。

- [ ] **Step 3: 在 `LockedVMA::unmap` 中按页原始归属做 uncharge**

```rust
if let Some(memcg) = page_guard.memcg() {
    memcg.memcg_uncharge_hierarchy(MMArch::PAGE_SIZE);
    page_guard.clear_memcg();
}
page_guard.remove_vma(self);
```

- [ ] **Step 4: 确保进程退出会走地址空间 teardown**

```rust
// 保持现有顺序：先把进程标为退出，再丢掉 user_vm 触发 AddressSpace drop。
pcb.task_cgroup_node().remove_task(raw_pid);
unsafe { pcb.basic_mut().set_user_vm(None) };
```

- [ ] **Step 5: 运行内核单测**

Run: `nix develop -c make kernel`
Expected: PASS，uncharge 路径编译通过且不会引入新的 build breakage；重复 uncharge 等行为在后续 QEMU 集成测试中观察。

- [ ] **Step 6: 提交这一小步**

```bash
git add kernel/src/mm/ucontext.rs kernel/src/process/mod.rs kernel/src/cgroup/core.rs kernel/src/filesystem/cgroup2/mod.rs
git commit -m "feat: uncharge anonymous pages on unmap and exit"
```

### Task 7: 动态导出 `memory.stat` 与 `memory.events`

**Files:**
- Modify: `kernel/src/filesystem/cgroup2/mod.rs:734-741`
- Test: `kernel/src/filesystem/cgroup2/mod.rs`

- [ ] **Step 1: 写失败的 stat/events 单测**

```rust
#[test]
fn memory_stat_and_events_are_rendered_dynamically() {
    let cg = cgroup_root_node();
    cg.memcg_charge_hierarchy(4096);
    cg.memcg_inc_high();
    cg.memcg_inc_max();
    cg.memcg_inc_oom();
    cg.memcg_inc_oom_kill();
    cg.memcg_inc_reclaim(1);

    let stat = Cgroup2Inode::new_file(
        "memory.stat".to_string(),
        cg.clone(),
        CgroupCoreFile::MemoryStat,
        b"anon 0\n",
    );
    let events = Cgroup2Inode::new_file(
        "memory.events".to_string(),
        cg.clone(),
        CgroupCoreFile::MemoryEvents,
        b"low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n",
    );

    assert_eq!(read_all(&stat), "anon 4096\ncurrent 4096\nreclaim 1\n");
    assert_eq!(
        read_all(&events),
        "low 0\nhigh 1\nmax 1\noom 1\noom_kill 1\n"
    );
}
```

- [ ] **Step 2: 运行单测，确认当前失败**

Run: `nix develop -c make kernel`
Expected: PASS，动态 stat/events 读取逻辑编译通过；字段内容在 QEMU 内测试中验证。

- [ ] **Step 3: 在 cgroup2 读取路径动态格式化 stat/events**

```rust
CgroupCoreFile::MemoryStat => {
    format!(
        "anon {}\ncurrent {}\nreclaim {}\n",
        cgroup.memory_stat_anon(),
        cgroup.memory_current(),
        cgroup.memory_reclaim_count(),
    )
    .into_bytes()
}
CgroupCoreFile::MemoryEvents => {
    let events = cgroup.memory_events();
    format!(
        "low {}\nhigh {}\nmax {}\noom {}\noom_kill {}\n",
        events.low, events.high, events.max, events.oom, events.oom_kill,
    )
    .into_bytes()
}
```

- [ ] **Step 4: 运行单测确认通过**

Run: `nix develop -c make kernel`
Expected: PASS，动态 stat/events 测试代码与实现可编译进内核；运行时内容以 QEMU 内结果为准。

- [ ] **Step 5: 提交这一小步**

```bash
git add kernel/src/filesystem/cgroup2/mod.rs
git commit -m "feat: render memory stat and events dynamically"
```

### Task 8: 接入最小 `memory.high` / `memory.max` 命中计数与失败骨架

**Files:**
- Modify: `kernel/src/cgroup/core.rs`
- Modify: `kernel/src/mm/fault.rs`
- Test: `kernel/src/cgroup/core.rs`

- [ ] **Step 1: 写失败的阈值判定单测**

```rust
#[test]
fn memcg_threshold_check_reports_high_and_max_hits() {
    let root = CgroupRoot::new();
    let cg = root.create_child(&root.root(), "leaf").unwrap();

    cg.set_memory_high(Some(4096));
    cg.set_memory_max(Some(8192));
    cg.memcg_charge_hierarchy(4096);

    let verdict = cg.memcg_check_limits(4096);
    assert!(verdict.high_hit);
    assert!(verdict.max_hit);
}
```

- [ ] **Step 2: 运行单测确认失败**

Run: `nix develop -c make kernel`
Expected: PASS，`memcg_check_limits` 与 fault 路径改动可编译进内核。

- [ ] **Step 3: 实现最小 limit 判定结构和祖先链检查**

```rust
#[derive(Debug, Default)]
pub struct MemcgLimitVerdict {
    pub high_hit: bool,
    pub max_hit: bool,
}

impl CgroupNode {
    pub fn memcg_check_limits(self: &Arc<Self>, bytes: usize) -> MemcgLimitVerdict {
        let mut verdict = MemcgLimitVerdict::default();
        let mut cur = Some(self.clone());
        while let Some(node) = cur {
            let current = node.memory_current();
            if node.memory_high().is_some_and(|high| current.saturating_add(bytes) >= high) {
                verdict.high_hit = true;
            }
            if node.memory_max().is_some_and(|max| current.saturating_add(bytes) >= max) {
                verdict.max_hit = true;
            }
            cur = node.parent();
        }
        verdict
    }
}
```

- [ ] **Step 4: 在匿名缺页路径接入命中计数与最小失败分支**

```rust
let verdict = current_memcg.memcg_check_limits(MMArch::PAGE_SIZE);
if verdict.high_hit {
    current_memcg.memcg_inc_high();
}
if verdict.max_hit {
    current_memcg.memcg_inc_max();
    current_memcg.memcg_inc_oom();
    return VmFaultReason::VM_FAULT_OOM;
}
```

- [ ] **Step 5: 运行内核单测**

Run: `nix develop -c make kernel`
Expected: PASS，limit 判定单测思路已落到代码设计；实际 gating 以 `nix develop -c make kernel` 和 QEMU 内行为验证为准。

- [ ] **Step 6: 提交这一小步**

```bash
git add kernel/src/cgroup/core.rs kernel/src/mm/fault.rs
git commit -m "feat: add minimal memcg high and max limit checks"
```

### Task 9: 扩展用户态 smoke 测试验证动态 `memory.*`

**Files:**
- Modify: `user/apps/c_unitest/test_cgroup_mvp_basic.c`

- [ ] **Step 1: 先在现有 smoke 测试中增加辅助函数**

```c
static long read_long(const char *path) {
    char buf[128];
    if (read_text(path, buf, sizeof(buf)) != 0) {
        fail(path);
    }
    return strtol(buf, NULL, 10);
}

static void touch_anon_pages(size_t bytes) {
    volatile char *p = malloc(bytes);
    size_t i;
    if (!p) {
        fail("malloc anon pages");
    }
    for (i = 0; i < bytes; i += 4096) {
        p[i] = 1;
    }
    free((void *)p);
}
```

- [ ] **Step 2: 在测试主体中加 `memory.current` 与 `memory.stat` 断言**

```c
const char *mem_current = "/sys/fs/cgroup/mvp_basic/memory.current";
const char *mem_stat = "/sys/fs/cgroup/mvp_basic/memory.stat";
long before = read_long(mem_current);
touch_anon_pages(8192);
long after = read_long(mem_current);
if (after < before) {
    printf("[FAIL] memory.current did not grow: before=%ld after=%ld\n", before, after);
    return 1;
}
if (read_text(mem_stat, buf, sizeof(buf)) != 0 || strstr(buf, "anon ") == NULL) {
    fail("read memory.stat");
}
```

- [ ] **Step 3: 在测试中对 `memory.events` 做字段稳定性校验**

```c
const char *mem_events = "/sys/fs/cgroup/mvp_basic/memory.events";
if (read_text(mem_events, buf, sizeof(buf)) != 0) {
    fail("read memory.events");
}
if (strstr(buf, "high ") == NULL || strstr(buf, "max ") == NULL || strstr(buf, "oom ") == NULL) {
    printf("[FAIL] unexpected memory.events content: %s\n", buf);
    return 1;
}
```

- [ ] **Step 4: 本地编译 C 测试程序**

Run: `nix develop -c make -C user/apps/c_unitest test_cgroup_mvp_basic ARCH=x86_64`
Expected: PASS，生成 `user/apps/c_unitest/test_cgroup_mvp_basic`。

- [ ] **Step 5: 提交这一小步**

```bash
git add user/apps/c_unitest/test_cgroup_mvp_basic.c
git commit -m "test: extend cgroup memory smoke assertions"
```

### Task 10: 新增专门的 cgroup memory accounting 集成测试

**Files:**
- Create: `user/apps/c_unitest/test_cgroup_memory_accounting.c`

- [ ] **Step 1: 新建失败中的最小测试程序骨架**

```c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

static void fail(const char *step) {
    printf("[FAIL] %s: %s\n", step, strerror(errno));
    exit(1);
}

int main(void) {
    printf("[FAIL] not implemented\n");
    return 1;
}
```

- [ ] **Step 2: 本地编译新测试程序，确认可生成**

Run: `nix develop -c make -C user/apps/c_unitest test_cgroup_memory_accounting ARCH=x86_64`
Expected: PASS，生成 `user/apps/c_unitest/test_cgroup_memory_accounting`。

- [ ] **Step 3: 填入 charge/uncharge + 层级聚合 + 迁移语义测试主体**

```c
int main(void) {
    const char *root_sub = "/sys/fs/cgroup/cgroup.subtree_control";
    const char *parent = "/sys/fs/cgroup/mem_parent";
    const char *child_a = "/sys/fs/cgroup/mem_parent/a";
    const char *child_b = "/sys/fs/cgroup/mem_parent/b";
    const char *procs_a = "/sys/fs/cgroup/mem_parent/a/cgroup.procs";
    const char *procs_b = "/sys/fs/cgroup/mem_parent/b/cgroup.procs";

    if (write_text(root_sub, "+memory\n") != 0) {
        fail("enable memory controller");
    }
    if (mkdir(parent, 0755) < 0 && errno != EEXIST) fail("mkdir parent");
    if (mkdir(child_a, 0755) < 0 && errno != EEXIST) fail("mkdir child a");
    if (mkdir(child_b, 0755) < 0 && errno != EEXIST) fail("mkdir child b");

    if (write_text(procs_a, "0\n") != 0) fail("join child a");
    touch_anon_pages(8192);
    long a_before = read_long("/sys/fs/cgroup/mem_parent/a/memory.current");
    long parent_before = read_long("/sys/fs/cgroup/mem_parent/memory.current");

    if (write_text(procs_b, "0\n") != 0) fail("join child b");
    touch_anon_pages(4096);
    long a_after = read_long("/sys/fs/cgroup/mem_parent/a/memory.current");
    long b_after = read_long("/sys/fs/cgroup/mem_parent/b/memory.current");
    long parent_after = read_long("/sys/fs/cgroup/mem_parent/memory.current");

    if (a_after < a_before) {
        printf("[FAIL] historical pages moved unexpectedly\n");
        return 1;
    }
    if (b_after == 0) {
        printf("[FAIL] new pages were not charged to child b\n");
        return 1;
    }
    if (parent_after < parent_before) {
        printf("[FAIL] parent current lost child accounting\n");
        return 1;
    }

    printf("[PASS] cgroup_memory_accounting\n");
    return 0;
}
```

- [ ] **Step 4: 本地编译新测试程序**

Run: `nix develop -c make -C user/apps/c_unitest test_cgroup_memory_accounting ARCH=x86_64`
Expected: PASS。

- [ ] **Step 5: 提交这一小步**

```bash
git add user/apps/c_unitest/test_cgroup_memory_accounting.c
git commit -m "test: add cgroup memory accounting integration coverage"
```

### Task 11: 通过 Nix/QEMU 启动内核验证真实行为

**Files:**
- Modify as needed from previous tasks only
- Test: `user/apps/c_unitest/test_cgroup_mvp_basic.c`, `user/apps/c_unitest/test_cgroup_memory_accounting.c`

- [ ] **Step 1: 先跑仓库当前可用的 host-side 构建检查，记录已知限制**

Run: `nix develop -c cargo test --manifest-path kernel/Cargo.toml --lib --no-run`
Expected: 目前会因仓库已有 host-test 问题失败（例如 `Box`/`Vec` 导入缺失、`duplicate lang item: eh_personality`）；把这些视为 baseline，不在本任务范围内修复。

- [ ] **Step 2: 再跑内核编译，确认本轮改动没有引入新的 build breakage**

Run: `nix develop -c make kernel`
Expected: PASS，内核镜像成功生成。


- [ ] **Step 3: 刷新 rootfs**

Run: `nix run .#rootfs-x86_64`
Expected: PASS，生成最新 `bin/qemu-system-x86_64.img`。

- [ ] **Step 4: 按 Nix 流程启动 DragonOS**

Run: `nix run .#start-x86_64`
Expected: QEMU 成功进入 DragonOS 控制台。

- [ ] **Step 5: 在 QEMU 内运行 smoke 测试**

```sh
/opt/test_cgroup_mvp_basic
```

Expected: 输出 `[PASS] cgroup_mvp_basic`。

- [ ] **Step 6: 在 QEMU 内运行 memory accounting 测试**

```sh
/opt/test_cgroup_memory_accounting
```

Expected: 输出 `[PASS] cgroup_memory_accounting`。

- [ ] **Step 7: 如需 root 权限，交互输入 sudo 密码，不要把密码写入脚本或仓库文件**

```sh
# 只在宿主机构建/启动命令提示 sudo 时交互输入
sudo -v
```

Expected: 权限预热成功；后续继续 `nix run` / QEMU 调试流程。密码必须保持在交互输入中，不能固化到测试脚本、文档、代码或 commit 历史。

- [ ] **Step 8: 记录串口/控制台上的关键证据**

```text
- 最后一个成功事件：进入 /sys/fs/cgroup 并成功创建测试 cgroup
- 第一条异常证据：若 memory.current/stat/events 输出与预期不一致，记录具体文件内容
- 若发生 OOM fault：记录 fault 路径和返回的 memory.events 计数
```

- [ ] **Step 9: 提交验证通过后的整体验证提交**

```bash
git add kernel/src/cgroup/core.rs kernel/src/cgroup/mod.rs kernel/src/mm/page.rs kernel/src/mm/fault.rs kernel/src/mm/ucontext.rs kernel/src/process/mod.rs kernel/src/filesystem/cgroup2/mod.rs user/apps/c_unitest/test_cgroup_mvp_basic.c user/apps/c_unitest/test_cgroup_memory_accounting.c
git commit -m "feat: implement memcg accounting and dynamic memory stats"
```

## Spec Coverage Check

- 匿名页真实记账：Task 1、Task 3、Task 4、Task 6
- `memory.current` 动态输出：Task 4、Task 5
- `memory.stat` / `memory.events` 动态输出：Task 2、Task 7
- 层级聚合：Task 1、Task 10
- 迁移后历史页不迁移：Task 3、Task 6、Task 10
- `memory.high` / `memory.max` 最小可观测 enforcement：Task 8
- 用户态与 QEMU 验证：Task 9、Task 10、Task 11

## Notes

- 本计划刻意把 reclaim/OOM 的“完整 Linux 语义”拆成最小命中计数与 fault 返回骨架，先保证真实 charge/uncharge、动态观测面、层级归属和 QEMU 验证闭环。
- 如果 Task 11 中发现 `PageReclaimer` 当前只支持 `PageType::File` 导致匿名 reclaim 无法继续推进，则下一轮计划应把匿名 reclaim/OOM 继续拆分到 `kernel/src/mm/page.rs` 的 `PageReclaimer` 路径中。
