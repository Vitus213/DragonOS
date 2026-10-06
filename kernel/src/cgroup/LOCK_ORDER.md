# cgroup 内存/进程侧锁全序（issue #28 落档）

本文档是 memcg 锁序与中断纪律的唯一权威序图。代码注释（
`kernel/src/mm/memcg.rs` 模块头、`kernel/src/cgroup/controllers/memory.rs`
各获取点）引用本文；两者不一致时以"本文 + 代码实际行为"互审后同步修正。

参考语义：Linux 6.6 `mm/memcontrol.c`（charge 事务锁
`preemption-disabled + local_irq` 域内，`page->memcg_data` 更新不跨
uncharge 持页归属锁）、`kernel/sched/core.c`（`pi_lock → rq_lock` 定序，
`__task_rq_lock` 先 pi 后 rq）。

## 1. 锁清单与中断纪律

| 锁 | 位置 | 纪律（必须） | 可被 hardirq 获取 |
|---|---|---|---|
| `INNER_ALLOCATOR` | arch/{x86_64,riscv64}/mm/mod.rs | `lock_irqsave`（现状即如此） | 是（驱动 IRQ 释放帧） |
| `PAGE_OWNERS` | mm/memcg.rs | **`lock_irqsave`，锁内零嵌套、零分配、绝不跨 uncharge** | 是（free 钩子） |
| `MEMORY_CHARGE_LOCK` | cgroup/controllers/memory.rs | **`lock_irqsave`** | 是（IRQ 释放帧 → uncharge） |
| `MemoryCss::inner` / `flags` | 同上 | **`lock_irqsave`**（charge 锁内层，或单独获取） | 是 |
| `PENDING_MAX_OOM` | mm/memcg.rs | **`lock_irqsave`**，锁内零嵌套 | 是（alloc 拒绝路径可达 charge 链） |
| slab `SLABALLOCATOR` | mm/allocator/kernel_allocator.rs | `lock_irqsave`（现状） | 是 |
| 任务 `pi_lock` | process/sched_info.rs | `pi_lock_irqsave`（现状） | 是 |
| 运行队列 `rq_lock` | sched/mod.rs | `lock_irqsave` 族（现状） | 是 |
| 任务 `task_lock` | process/signal.rs | `with_task_lock_irqsave`（现状） | 是（信号/OOM 路径） |
| freezer `task_lock` | cgroup/controllers/freezer.rs | 仅任务上下文获取（见 §3 交叉点 F） | 否 |
| freezer `wakeable_tasks` | 同上 | 仅任务上下文（freezer 链内层） | 否 |

## 1.1 不变式（全体锁必须遵守）

- **I1（中断纪律）**：硬中断上下文可到达的锁，一律以 `lock_irqsave()`
  获取；禁止任何获取点使用 IRQ-on 的 `lock()`/`lock_bh()`。判据：该锁
  的任一获取路径可从 `LockedFrameAllocator::free`（x86_64 mm/mod.rs
  :843、riscv64 mm/mod.rs:530 每次释放都进 `memcg_free_uncharge`）或
  驱动 IRQ handler 直接/间接到达。
- **I2（临界区纪律）**：irqsave 临界区内禁止睡眠、分配内存、调用回收/
  唤醒/信号原语（即禁止获取锁序图中位于当前锁"下游"之外的任何锁）。
- **I3（摘取-提交分离）**：`PAGE_OWNERS` 与 `MEMORY_CHARGE_LOCK` 互不
  嵌套——free 路径持 `PAGE_OWNERS` 仅摘取归属栈批，放锁后才逐段
  uncharge（两段式，`memcg_free_uncharge`）。
- **I4（新获取点）**：任何后续改动在这两个文件新增的锁获取点，一律
  `lock_irqsave()`；违反 I1 的合并视为回归。

**本次修复消灭的反例（原 vitus/master@23557e57）**：
`mm/memcg.rs:163`（free 侧持 `PAGE_OWNERS.lock_irqsave()` 期间调用
`uncharge_css → MEMORY_CHARGE_LOCK.lock()`）与 `mm/memcg.rs:211-212`
（charge 侧 `record_frame_owners` 用 **IRQ-on** 的 `PAGE_OWNERS.lock()`，
spinlock.rs:74-83 只 preempt_disable 不关中断）。组合后果：task 在
try_charge 事务块（持 MEMORY_CHARGE_LOCK，IRQ-on）被同 CPU 硬中断打断，
IRQ 释放页帧进入 free → `PAGE_OWNERS`（irqsave）→ 对同一
`MEMORY_CHARGE_LOCK` 重进非重入 CAS 自旋 → 持锁者（被中断的 task）
无法推进 → 同 CPU 永久锁死，同链等待者全部拖死。I1（统一 irqsave，
封死同 CPU 重入窗口）与 I3（消除 PAGE_OWNERS→charge 锁嵌套边，封死
跨 CPU ABBA）在修复后同时成立。

## 2. memcg 锁族全序（修复后）

```text
           INNER_ALLOCATOR(irqsave)          ← 分配器本体；charge/uncharge 均在放锁后调用
                 │
      ┌──────────┼────────────────┐
      ▼          ▼                ▼
  PAGE_OWNERS  SLABALLOCATOR   （互不嵌套的三个独立域）
  (irqsave)   (irqsave)
  锁内仅槽位     │
  take/填值     ├─→ PAGE_OWNERS        （slab 补帧 → allocate → memcg_alloc_charge
     ✕无嵌套     │                       → record_frame_owners；无 uncharge）
     ✕无分配     └─→ MEMORY_CHARGE_LOCK（slab 缩容 → free → memcg_free_uncharge
                                         放锁后 uncharge —— 注意：SLABALLOCATOR
                                         域内允许获取 memcg 锁族，因为 memcg
                                         锁族内绝不反向获取 SLABALLOCATOR）

  MEMORY_CHARGE_LOCK(irqsave)
        │
        ▼
  MemoryCss::inner / flags(irqsave)          ← 唯一嵌套边；inner 锁内零分配零嵌套

  PENDING_MAX_OOM(irqsave)                   ← 独立域：锁内仅 Option 读写
```

**关键不变式（本卡修复内容）**：

- `PAGE_OWNERS` 与 `MEMORY_CHARGE_LOCK` **互不嵌套**。原实现 free 侧
  持 `PAGE_OWNERS` 期间进入 `uncharge_css → MEMORY_CHARGE_LOCK`（嵌套
  边 PAGE_OWNERS→MEMORY_CHARGE），charge 侧则顺序使用两锁
  （`try_charge` 释放 charge 锁后 `record_frame_owners` 才取
  PAGE_OWNERS）——同一锁对出现"嵌套/不嵌套"两种形态，叠加
  `record_frame_owners` 用 IRQ-on `lock()`，硬中断落在 charge 侧任一
  临界区并重进 free 链时同 CPU 重入非重入 CAS 自旋锁 → 永久锁死。
  修复：free 两段式（锁内栈批摘 runs、放锁后逐段 uncharge）+ 全锁族
  `lock_irqsave` 统一纪律（mm/memcg.rs、controllers/memory.rs）。
- `PAGE_OWNERS` 临界区内**零堆分配**：分配会经 slab 补帧回调
  `LockedFrameAllocator::allocate`/其拒绝路径的 `free`，同 CPU 重进
  `PAGE_OWNERS`。摘取批因此使用 `FREE_RUN_BATCH=8` 栈数组，满批放锁
  uncharge 后续扫（`take_owner_runs` 的 resume 契约）。
- `MEMORY_CHARGE_LOCK` 域内**零分配**：`try_charge` 的祖先遍历用栈上
  迭代器（`for_each_ancestor` 仅沿 `Weak::upgrade` 的 Arc 链走，不
  collect），`uncharge_chain` 递归深度 = 层级深度（受 max.depth 约束），
  两者都不分配。`PageReclaimer::wakeup_claim_thread()` 在 charge 事务
  块**释放之后**调用（memory.rs try_charge 尾部），避免
  MEMORY_CHARGE_LOCK → pi_lock/rq_lock 边。

## 3. 与进程侧锁（rq / freezer task_lock / pi_lock / task_lock）的交叉点

memcg 锁族与进程侧锁族的全部潜在交叉，逐一论证（序方向：memcg 锁族
一律在进程侧锁**之外或完全不相交**；任何持 memcg 锁的临界区不得调用
唤醒/入队/信号原语）：

- **A. alloc 拒绝路径**（mm/memcg.rs `memcg_alloc_charge` → 
  `PENDING_MAX_OOM.lock_irqsave()`）：只写 Option 槽，不触碰进程侧锁。
  OOM kill / 唤醒由 fault 路径 `drain_pending_memcg_oom` 在无 memcg
  锁状态下执行（`oom.rs:340` 才进 `with_task_lock_irqsave` →
  `OOM_STATE.lock_irqsave` → `send_signal`）。✔ 无边。
- **B. charge 越 high 唤醒**（memory.rs `try_charge` 尾 →
  `PageReclaimer::wakeup_claim_thread` → `ProcessManager::wakeup` →
  `pi_lock → rq_lock`）：调用点在 charge 事务块闭合之后。✔
  MEMORY_CHARGE_LOCK 与 pi/rq 无嵌套边。
- **C. IRQ 释放帧 → uncharge**：hardirq 里 free 链摘取 runs 后取
  MEMORY_CHARGE_LOCK/inner（irqsave，安全）。该链从不 wakeup、从不取
  进程侧锁。若 IRQ 恰好打断持有 `pi_lock`/`rq_lock`/`task_lock` 的
  任务：进程侧锁在 IRQ 下的再获取只发生在**另一把锁的 irqsave 域**
  （见 D/E），free 链不含进程侧锁，故无交叉死锁。✔
- **D. freezer**（freezer.rs）：链为 freezer `task_lock` →
  `pi_lock_irqsave` →（同临界区内）`wakeable_tasks.lock()`；全程不
  触碰任何 memcg 锁。反向：memcg 锁域内无 freezer 调用点（全仓
  grep `freeze_task|refrigerator|wakeable` 于 mm/、cgroup/controllers/
  memory.rs 为零）。✔ 两族不相交。
- **E. OOM 选择**（oom.rs）：`with_task_lock_irqsave` 内取
  `OOM_STATE.lock_irqsave` 与 `sighand` 锁；该域不获取 memcg 锁
  （`note_memcg_oom*` 在 task_lock 域**外**调用，memcg.rs
  `drain_pending_memcg_oom`）。反向：memcg 锁域无 OOM 原语调用
  （拒绝只置 `PENDING_MAX_OOM`）。✔ 边方向唯一：task_lock 族 ⊥
  memcg 族，仅顺序衔接。
- **F. freezer `task_lock` 用普通 `lock()`（IRQ-on）**：其临界区含
  `wakeable_tasks.lock()` 与 `task.flags()`，均为任务上下文专用，且
  IRQ 路径不获取 freezer 这两把锁（hardirq 唤醒只经
  sched.rs:88 的 `FROZEN` 标志 + `WAKE_PENDING`，在 `pi_lock_irqsave`
  域内，不触 freezer 锁）。这是 freezer 自身的纪律，与 memcg 锁族正交；
  本文档予以记录，不属本卡改动面。⚠ 若未来 IRQ 路径需要触碰 freezer
  锁，必须先升级为 irqsave（同本卡手法）。

## 4. 全序（拓扑序，外 → 内）

```text
INNER_ALLOCATOR ≺ SLABALLOCATOR
     │                    │
     ├─ PAGE_OWNERS ⫫     ├─ PAGE_OWNERS（经 allocate 拒绝回 free 时）
     │                    └─ MEMORY_CHARGE_LOCK ⫫
     └─ (allocate 放锁后) ── MEMORY_CHARGE_LOCK ≺ MemoryCss::inner/flags
PENDING_MAX_OOM  ⫫ 一切（独立域）
freezer{task_lock ≺ pi_lock ≺ wakeable_tasks}、{task_lock ≺ OOM_STATE}、
pi_lock ≺ rq_lock   —— 与 memcg 族 ⫫（无嵌套边，仅顺序衔接）
```

（`≺` = 允许嵌套；`⫫` = 禁止嵌套/互不可达。）该图上任意两锁的获取
序列都是 DAG 上的路径，无环。

## 5. 核查记录（SOP 3）

`PAGE_OWNERS` 获取点（全仓 grep，共 3 处，全部 `lock_irqsave`）：

1. mm/memcg.rs `memcg_page_owners_init` —— 一次性初始化。
2. mm/memcg.rs `record_frame_owners` —— alloc 侧记名（charge 成功后）。
3. mm/memcg.rs `memcg_free_uncharge` 批摘取循环 —— task/IRQ 两侧可达。

`MEMORY_CHARGE_LOCK` 获取点（全仓 grep，共 4 处，全部 `lock_irqsave`）：

1. memory.rs `try_charge` 事务块 —— 叶+祖先检查与提交。
2. memory.rs `uncharge` —— 层级反向释放链。
3. memory.rs `set_high` —— high 与 usage 同事务。
4. memory.rs `set_max` —— max 与 usage 同事务。

`MemoryCss::inner/flags`、`PENDING_MAX_OOM`：全部获取点已统一
`lock_irqsave`（39 处机械转换 + memcg.rs 2 处），获取点级注释见代码。
hardirq 可达性结论：free 链（`LockedFrameAllocator::free`，
x86_64 mm/mod.rs:842、riscv64 mm/mod.rs:530 每次释放都进
`memcg_free_uncharge`）使整条 memcg 锁族在 IRQ 下可达，因此全部获取点
必须 irqsave——这正是修复采用的纪律。
