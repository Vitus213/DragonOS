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

---

## issue #35：io.max 对异步回写逃逸的修复落地说明

问题：pagecache 工作线程（`pagecache-wb-*`/`pagecache-io-*`/`events`/`page_reclaim`）上下文派发块 I/O 时，限速与 io.stat 按执行线程（恒为根组）的 cgroup 解析，容器 buffered write 的脏页落盘既不受 io.max 约束也不计入其 io.stat。

### 已修复

- `blkcg` 增加任务级块 I/O 归属覆写（Linux `kthread_associate_blkcg()` 等价物）：`set_io_owner`/`try_set_io_owner` + RAII guard（Drop 恢复、可嵌套），`current_io_cgroup()` 覆写优先；`throttle_current_io`/`account_current_io` 全部改走该解析，既有 GenDisk/ext4 适配器/MBR 扫描 hook 零改动即被覆盖。
- 脏发布捕获：`PageEntry::dirty_owner` 在页进入新脏生命周期（NewlyDirty/RedirtiedDuringWriteback）时记录发布任务的归属 cgroup（Linux `inode_switch_wbs()` 语义）；合并进既有脏页保留原归属，一个脏生命周期一个属主。`inner` 串行化读写。
- 回写批次认领时聚合归属（`pick_batch_io_owner` 取首个已知脏主），`submit_writeback_batch` 在 token submission/legacy write_pages/inode 直写三个派发分支前安装覆写，异步 completion 路径（ext4 `account_io_for` 提交时捕获）随之前移解析到脏主。
- 页回收单页写回（`mm/page.rs::page_writeback`）认领后经 `PageCache::dirty_io_owner` 读回归属并围绕 `write_page`/`write_direct` 安装覆写。
- 异步读补齐：page cache 预读 work（`start_async_read`）与默认批量读兼容路径（`submit_default_read_batch`）在调度者上下文解析归属、工作线程内安装覆写。
- 直调点核对（SOP3）：BlockDevice `submit_bio*`/`read_at_sync`/`write_at_sync` 的驱动实现体（virtio_blk/ahcidisk/pmem/mmc/loop_device）均在 GenDisk/适配层 hook 之下；`GenDisk::sync`/`sync_file`/ext4 `flush` 携带 0 字节、按 bytes==0 短路不参与限速（与 Linux blk-flush 不计 io.max 一致）；FATFsInfo::update、LoopDevice 裸 IndexNode 读写为死代码路径。边界如实记入 `CONTROLLER_FRAMEWORK.md`（混合归属批次取首主、ext4 journal 元数据仍按线程归属）。
- `make kernel ARCH=x86_64` 通过；归属聚合与设备 key 规则有宿主机可运行的 `#[cfg(test)]` 单测。
