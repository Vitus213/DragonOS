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

## issue #32 — __refrigerator 信号逃逸（计数虚增 + FROZEN 残留吞唤醒）

- `__refrigerator()` 引入纯裁决 `classify_refrigerator_entry()`：FROZEN 位、
  nr_frozen_tasks、wakeable 登记三者只在裁决完成的转移里于同一 pi_lock
  临界区一落账——Runnable→EnterBlocked（转冰箱阻塞态+登记 wakeable）、
  Blocked→EnterInPlace（状态原样、唤醒归原事件）、Stopped/Exited→Defer
  （不置位不计数，FREEZING 留待下轮收敛）。旧实现对已睡眠任务先置 FROZEN、
  无条件计数并返回 true 而状态不动，随后被 `signal_pending_state` 抬回
  Runnable：计数虚增使 `is_frozen()` 谎报冻结完成，残留 FROZEN 把之后
  每次 wakeup 吞成 WAKE_PENDING 挂死到解冻。
- 冻结态不可被信号恢复（Linux TASK_FROZEN 无唤醒位的等价收口）三守卫：
  `__schedule()` signal_wake 加 `!FROZEN`；`undo_mark_sleep()` 对
  Blocked+FROZEN 改置 WAKE_PENDING 暂存（不再抬回 Runnable）；wakeup()
  原有 FROZEN 暂存语义保持不变，由解冻路径（unfreeze_task/迁移解冻）重放。
- 计数不变式：FROZEN ⟺ 已计数 ⟺ 处于阻塞集合，构造性成立；解冻/迁移/exit
  清理对"从未入冰箱的任务"幂等（dec 只随 FROZEN、标志全清、双次解冻早退）。
- 验证：`make kernel ARCH=x86_64` 通过（worktree@aa81b900 基线）；裁决
  函数宿主切片单测 4 例全绿；旧行为复现模型（缺陷 3 断言全命中）与修复后
  收敛/幂等模型（8 场景断言）见 issue #32 步骤评论。
