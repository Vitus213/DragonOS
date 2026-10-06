## cgroup v2 审计修复落地说明

本轮收口提交：`6ac0c6eb fix(cgroup): 收口审计发现的 v2 语义缺陷`。

### 已修复

- memcg 内核堆/页表分配走 `allocate_unaccounted`，不把用户 `memory.max` 拒绝传播为内核不可失败分配 panic。
- `memory.max` 拒绝保留强 CSS 引用，fault OOM 继续走 cgroup-scoped victim 选择；`max_exceeded_now` 使用事务拒绝后的 `usage >= max` 语义。
- freezer 父请求继承、SELF-only `cgroup.freeze` 读取、空 cgroup `frozen`、冻结期唤醒保存、跨 cgroup 迁移解冻。
- cgroup v2 控制器列表不再公布 freezer；`memory.min/low` 默认读取 `0`；`cpu.max` finite quota 校验 `1ms <= quota <= period`。
- `make kernel ARCH=x86_64` 通过（最后一次构建：零 error，保留既有 warnings）。

### 未执行

Darwin arm64 无 KVM，TCG guest smoke 启动不可接受；此前连续观察 10 分钟无串口输出，因此本轮不重复启动。剩余边界见 `CONTROLLER_FRAMEWORK.md`。

## issue #34 · 计费入口 panic：JITMem expect 于可拒绝的记账分配

分支 `pm/issue-34`（PR #44），基线 vitus/master@23557e57。

### 已修复

- `JITMem::new()`（`allocate_page_frames(...).expect("JITMem alloc failed")` + `phys_2_virt().unwrap()`）删除；kprobe/tracepoint 的 `PERF_EVENT_IOC_SET_BPF` 迁移到按程序长度定容且可失败的 `try_for_bpf_program(...)?`——memcg `memory.max` 拒绝现以 ENOMEM 返回用户态，不再内核 panic；`JITMem::Drop` 的 `virt_2_phys().expect` 一并去 panic 化。
- JIT 编译失败路径按 uprobe.rs 既有模式 `Box::from_raw` 归还所有权，触发 `memcg_free_uncharge`，消除非缺页拒绝的滞留计费（与 #4 关联项）。
- `IdentPageMapper::create`/`map_phys`/`ident_pt_alloc` → `kexec.rs::init_pgtable` 链（`kexec_load` 系统调用可达）的 `unwrap()` 分配点改 `Result`/ENOMEM，翻译失败归还已计费页帧。
- `BioRequest::new_read/new_write/new_flush` 与 `DmaBuffer::alloc_bytes/alloc_pages` 的 `.expect()` panic 包装删除（调用点唯一，迁移 `try_new_flush()?`）。
- 全仓记账 allocator 后端 `.expect()/.unwrap()` 分配点整改表（9 处修、10 组逐项排除论证）见 issue #34 评论。
- `make kernel ARCH=x86_64` 与 `ARCH=riscv64` 通过；记账拒绝返回 ENOMEM 而非 panic 以 shipped 源码切片的 host 单测验证（新旧行为对照，见 issue #34 评论）。

### 未做

- `dma_alloc_pages_raw`/`E1000EBuffer::new` 的 `.expect`：被外部 crate virtio-drivers `Hal::dma_alloc`（无 Result 签名）与 smoltcp token 接口锁死，DragonOS 侧无法本质传播；属"panic 与全局 OOM handler 同档"的遗留面，整改表已定界，待 fork 上游或 bounce-pool 预取方案另卡处理。

## issue #28 — memcg 锁序与中断纪律倒置修复

- `memcg_free_uncharge` 两段式：持 `PAGE_OWNERS`（irqsave）仅把连续同归属
  页帧摘取进 `FREE_RUN_BATCH=8` 栈批（锁内零分配、零嵌套），放锁后逐段
  `uncharge_css`。消除原实现的 PAGE_OWNERS→MEMORY_CHARGE_LOCK 嵌套边
  （与 charge 侧顺序使用两锁的方向相反）。
- memcg 锁族全部获取点统一 `lock_irqsave()`：`PAGE_OWNERS` 3 处（init /
  `record_frame_owners` / free 摘取循环）、`MEMORY_CHARGE_LOCK` 4 处、
  `MemoryCss::inner`/`flags` 39 处、`PENDING_MAX_OOM` 2 处——硬中断的帧
  释放路径可达整条 charge 锁链，IRQ-on 持有者等于把同 CPU 重入非重入
  CAS 自旋的死锁窗口留给下一次驱动释放。
- 与进程侧锁（rq_lock/freezer task_lock/pi_lock/task_lock）无嵌套边：
  `try_charge` 的 `wakeup_claim_thread` 调用点在事务块之外、freezer/OOM
  链不含 memcg 锁（全序图与逐交叉点论证：`kernel/src/cgroup/LOCK_ORDER.md`）。
- 验证：`make kernel ARCH=x86_64` 通过；`take_owner_runs` 纯函数宿主
  单测 6 例全绿（摘取归并/缝隙/钳制/满批续扫收敛/边界不越批）。

## issue #39 · cgroup.max.depth / cgroup.max.descendants 与递归链定界

### Linux 6.6 实码语义对照（v6.6 `kernel/cgroup/cgroup.c`，经 v4.14 同码交叉验证）

- 文件归属：`cgroup.max.descendants` / `cgroup.max.depth` 是 `cgroup_base_files[]`
  的成员（cgroup.c:5237-5246），两个条目均无 `CFTYPE_NOT_ON_ROOT` 标志 ⇒
  默认层级下**每个 cgroup（含 root 与非 root）都有这两个读写 knob**，约束各自子树。
- 默认值：`init_cgroup_housekeeping()` 将 `max_descendants` 与 `max_depth` 均初始化为
  `INT_MAX`（cgroup.c:2000-2001）；`Documentation/admin-guide/cgroup-v2.rst`（"cgroup.max.depth:
  A read-write single value files. The default is max"）确认默认 max。issue 背景中的
  "默认 max.depth=64、max.descendants=1000"与 6.6 实码不符，本卡按实码取 max。
- 读写语义：show（cgroup.c:3527-3537、3570-3580）打印 `max` 或十进制整数；
  write（cgroup.c:3540-3568、3583-3608）接受 `max` 或整数，负值返回 `-ERANGE`，
  非数字由 `kstrtoint` 报错（EINVAL）。
- mkdir 越限检查：`cgroup_mkdir()`（cgroup.c:5722）在 `cgroup_lock` 内调用
  `cgroup_check_hierarchy_limits(parent)`（cgroup.c:5699-5718）：自 parent 向 root 逐层，
  新子组的相对深度 `level` 从 1 起算；任一层 `nr_descendants >= max_descendants`
  （cgroup.c:5708）或 `level > max_depth`（cgroup.c:5711）即失败。越限 errno 实码为
  **`-EAGAIN`**（cgroup.c:5736-5737）——不是 EMLINK；本卡按"以实码为准"引用 EAGAIN。
- 计数器维护：创建时（cgroup.c:5645-5649）对 parent 及其全部祖先（不含自身）
  `nr_descendants++`；rmdir（`cgroup_destroy_locked`，cgroup.c:5930-5932）对 parent 及
  其祖先 `nr_descendants--`、`nr_dying_descendants++`；dying cgroup 彻底释放
  （cgroup.c:5425-5427）时 `nr_dying_descendants--`。DragonOS 的 rmdir 要求组内无子组、
  无任务且同步彻底删除（无 dying 态），因此只镜像 `nr_descendants` 的增删；
  dying 计数在 DragonOS 没有对应物，此处显式声明省略。

### DragonOS 落地（本卡）

- `CgroupNode` 新增 `nr_descendants`/`max_depth`/`max_descendants`（AtomicUsize，
  `usize::MAX` 即 "max"）；`create_child` 在 accounting lock 内做 parent→root 逐层检查
  （纯函数 seam `hierarchy_limits_breach`，越限 `EAGAIN_OR_EWOULDBLOCK`），成功后沿同链
  `nr_descendants+1`；`remove_child` 成功路径 `-1`。
- `cgroup2/files.rs` 暴露 `cgroup.max.depth` / `cgroup.max.descendants`（root 与全部子组
  均可见，mode 0644；`max` ↔ `usize::MAX`，负值 `ERANGE`，非法值 `EINVAL`）。
- 递归链定界：`collect_subtree_tasks`（mm/memcg.rs）、freezer 的
  `freeze_tasks↔propagate_parent_freezing` / `unfreeze_tasks↔retract_parent_freezing`
  互递归与 `update_ancestor_counts` 上溯递归、`has_threaded_descendant`（cgroup/core.rs）、
  `MemoryCss::uncharge_chain`（controllers/memory.rs）全部改为迭代遍历。
  cpuset / io / pids / `add_task` 的祖先链与 `prepare_device_snapshots` 的子树遍历
  原本已是迭代式，核查后保持不变。递归深度消耗栈的路径清零；层级深度由 knobs
  显式定界（Linux 语义：默认 max）。
