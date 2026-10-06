use alloc::{format, string::ToString, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use log::{error, warn};
use system_error::SystemError;

use crate::{
    arch::{ipc::signal::Signal, mm::LockedFrameAllocator, MMArch},
    cgroup::{controllers::memory::MemoryCss, subsys::CgroupSubsysId, CgroupNode},
    ipc::signal_types::{SigCode, SigInfo, SigType},
    libs::{spinlock::SpinLock, wait_queue::WaitQueue},
    mm::{allocator::page_frame::FrameAllocator, MemoryManagementArch},
    process::{pid::PidType, ProcessControlBlock, ProcessFlags, ProcessManager, RawPid},
    time::Duration,
};

use super::ucontext::AddressSpace;

static OOM_WAITQ: WaitQueue = WaitQueue::default();
static OOM_STATE: SpinLock<OomState> = SpinLock::new(OomState::new());
static OOM_FAULT_INJECT: SpinLock<OomFaultInject> = SpinLock::new(OomFaultInject::disabled());
static OOM_KILL_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub struct OomContext {
    pub trigger_pid: RawPid,
    pub trigger_tgid: RawPid,
    pub fault_address: super::VirtAddr,
    pub fault_ip: usize,
    pub order: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OomOutcome {
    Retry,
    CurrentTaskKilled,
    NoVictim,
}

#[derive(Debug, Clone)]
struct OomVictimState {
    generation: u64,
    tgid: RawPid,
    mm_id: u64,
}

#[derive(Debug)]
struct OomState {
    generation: u64,
    selecting: bool,
    inflight: Option<OomVictimState>,
}

#[derive(Debug)]
struct OomFaultInject {
    target_tgid: Option<RawPid>,
    fail_after: usize,
    seen: usize,
    remaining_failures: Option<usize>,
}

impl OomState {
    const fn new() -> Self {
        Self {
            generation: 0,
            selecting: false,
            inflight: None,
        }
    }
}

impl OomFaultInject {
    const fn disabled() -> Self {
        Self {
            target_tgid: None,
            fail_after: 0,
            seen: 0,
            remaining_failures: Some(0),
        }
    }

    fn is_enabled(&self) -> bool {
        self.target_tgid.is_some()
    }
}

#[derive(Debug)]
struct OomCandidate {
    tgid: RawPid,
    mm: Arc<AddressSpace>,
    score: isize,
    resident_pages: usize,
    oom_score_adj: i16,
}

const OOM_SCORE_ADJ_MIN: i16 = -1000;

/// scoped OOM「触发者不可杀且组内无可杀受害者」时的等待上界（毫秒）。
/// Linux `mem_cgroup_oom_synchronize()` 是无界 TASK_KILLABLE 等待；
/// DragonOS 尚无 oom_reaper 与 memcg oom notify，组内回收可能只靠
/// 外部（管理员上调 max / 组内任务退出）推进，因此取有界等待 +
/// charge 重试：等待醒来后无论是否解除越限都返回 Retry，下一次缺页
/// 重新排水；致命信号可随时打断（wait_event_interruptible_timeout
/// 在信号挂起时立即退出，循环头部的 killable 检查随即收敛）。
const SCOPED_OOM_NO_VICTIM_WAIT_MS: u64 = 100;

fn wake_oom_waiters() {
    OOM_WAITQ.wake_all();
}

fn current_is_killed_or_exiting() -> bool {
    let current = ProcessManager::current_pcb();
    Signal::oom_fatal_signal_pending(&current) || current.flags().intersects(ProcessFlags::EXITING)
}

fn leader_of(pcb: Arc<ProcessControlBlock>) -> Arc<ProcessControlBlock> {
    ProcessManager::find(pcb.raw_tgid()).unwrap_or(pcb)
}

fn is_global_init_or_kthread(leader: &Arc<ProcessControlBlock>) -> bool {
    leader.raw_pid().data() == 0
        || leader.raw_pid().data() == 1
        || leader.flags().contains(ProcessFlags::KTHREAD)
}

fn should_skip_candidate(leader: &Arc<ProcessControlBlock>, oom_score_adj: i16) -> bool {
    is_global_init_or_kthread(leader)
        || leader.flags().contains(ProcessFlags::EXITING)
        || leader.is_active_vfork()
        || oom_score_adj == OOM_SCORE_ADJ_MIN
}

fn better_candidate(new: &OomCandidate, old: &OomCandidate) -> bool {
    new.score > old.score
        || (new.score == old.score && new.resident_pages > old.resident_pages)
        || (new.score == old.score
            && new.resident_pages == old.resident_pages
            && new.tgid > old.tgid)
}

fn total_system_pages() -> isize {
    let total_pages = unsafe { LockedFrameAllocator.usage() }.total().bytes() >> MMArch::PAGE_SHIFT;
    total_pages.min(isize::MAX as usize).max(1) as isize
}

fn oom_score(mm: &Arc<AddressSpace>, oom_score_adj: i16, total_pages: isize) -> isize {
    let resident_pages = mm.resident_pages().min(isize::MAX as usize) as isize;
    let adjustment = (oom_score_adj as isize).saturating_mul(total_pages) / 1000;
    resident_pages.saturating_add(adjustment)
}

pub fn proc_oom_score(pcb: &Arc<ProcessControlBlock>) -> usize {
    let leader = leader_of(pcb.clone());
    if is_global_init_or_kthread(&leader) || leader.is_active_vfork() {
        return 0;
    }

    let oom_score_adj = pcb.sig_info_irqsave().oom_score_adj();
    if oom_score_adj == OOM_SCORE_ADJ_MIN {
        return 0;
    }

    let Some(mm) = pcb.basic().user_vm() else {
        return 0;
    };
    let total_pages = total_system_pages();
    let badness = oom_score(&mm, oom_score_adj, total_pages);
    let score = 1000isize
        .saturating_add(badness.saturating_mul(1000) / total_pages)
        .saturating_mul(2)
        / 3;
    score.clamp(0, 2000) as usize
}

pub fn oom_kill_count() -> u64 {
    OOM_KILL_COUNT.load(Ordering::Relaxed)
}

fn count_oom_kill() {
    OOM_KILL_COUNT.fetch_add(1, Ordering::Relaxed);
}

fn task_uses_mm(task: &Arc<ProcessControlBlock>, mm: &Arc<AddressSpace>) -> bool {
    task.basic()
        .user_vm()
        .is_some_and(|task_mm| task_mm.id() == mm.id() || Arc::ptr_eq(&task_mm, mm))
}

fn clear_inflight_for_mm(mm_id: u64) -> bool {
    let mut state = OOM_STATE.lock_irqsave();
    if state
        .inflight
        .as_ref()
        .is_some_and(|victim| victim.mm_id == mm_id)
    {
        state.inflight = None;
        true
    } else {
        false
    }
}

fn rollback_inflight(generation: u64, tgid: RawPid, mm_id: u64) -> bool {
    let mut state = OOM_STATE.lock_irqsave();
    if state.inflight.as_ref().is_some_and(|victim| {
        victim.generation == generation && victim.tgid == tgid && victim.mm_id == mm_id
    }) {
        state.inflight = None;
        true
    } else {
        false
    }
}

/// 全局 OOM 的 pid 快照（选择时重新解引用，不携带缓存语义）。
fn collect_all_task_pids() -> Vec<RawPid> {
    ProcessManager::get_all_processes()
}

/// scoped OOM 的组内成员快照（issue #27：每轮选择重新收集，禁止缓存
/// 上一轮结果——pid 空间会被组外任务复用，静态快照在重试窗口内即失效）。
///
/// 锁序：父节点读锁（children）先于子节点读锁（tasks），仅由父向子
/// 递降；兄弟子树不相交、图无环，不可能构成环等待。沿用原 memcg 侧
/// 收集方式：std RwLock 读锁做成员快照遍历，不新增 IRQ 持锁。
/// pm37（#37）复用本函数做整组击杀遍历。
pub(crate) fn collect_subtree_task_pids(node: &Arc<CgroupNode>) -> Vec<RawPid> {
    let mut pids = node.tasks();
    for child in node.children() {
        pids.extend(collect_subtree_task_pids(&child));
    }
    pids
}

/// 共享受害者 mm 的同组击杀集（issue #27：`scope` 非空时组内成员才
/// 入选）。
///
/// 受害者与其线程组使用同一 mm；vfork 等待父等组外共享者绝不能被
/// 击杀（Linux memcg OOM 只在越限子树内选择）。返回项为线程粒度，
/// 实际发送以 TGID 定向整个线程组。
fn kill_targets_for_mm(
    mm: &Arc<AddressSpace>,
    scope: Option<&Arc<CgroupNode>>,
) -> Vec<Arc<ProcessControlBlock>> {
    let mut seen_tgids = Vec::new();
    let mut targets = Vec::new();

    for pid in collect_all_task_pids() {
        let Some(task) = ProcessManager::find(pid) else {
            continue;
        };
        if !task_uses_mm(&task, mm) {
            continue;
        }

        let leader = leader_of(task.clone());
        let tgid = leader.raw_tgid();
        if seen_tgids.contains(&tgid) {
            continue;
        }
        seen_tgids.push(tgid);

        if is_global_init_or_kthread(&leader) {
            continue;
        }
        if let Some(scope) = scope {
            // 组外共享者不进入 kill 集；发送前 [`scoped_validate_pid`]
            // 还会对入选者复核（双重防线）。
            if !scope.is_ancestor_of(&task_cgroup_node_of(&leader)) {
                continue;
            }
        }
        targets.push(task);
    }

    targets
}

/// 全局 OOM：从全部用户任务中选出最高分受害者。
fn select_victim() -> Option<OomCandidate> {
    select_victim_from(collect_all_task_pids(), None)
}

/// scoped OOM（issue #27）：从越限子树【当前】成员中选出受害者。
/// 每次调用（含 ESRCH 重试与抢占失败后的重选）都重新遍历子树收集
/// pid，绝不复用上一轮的快照——pid 号空间会被组外任务复用，静态
/// 快照在重试窗口内即失效（issue #27 缺陷二）。
fn select_scoped_victim(scope: &Arc<CgroupNode>) -> Option<OomCandidate> {
    select_victim_from(collect_subtree_task_pids(scope), Some(scope))
}

/// 任务当前直接归属的 memory cgroup 节点。
///
/// 迁移路径（`write_procs` / fork 继承）在 `cgroup_accounting_lock` 下
/// 原子地完成 remove_task + add_task + `task_cgroup` 字段写入；字段访问
/// 经 `ArcSwap` 无锁读取，因此这里返回的必然是「迁移前旧节点」或
/// 「迁移后新节点」之一（两个都是稳定 `Arc`），不存在中间态。
fn task_cgroup_node_of(task: &Arc<ProcessControlBlock>) -> Arc<CgroupNode> {
    task.task_cgroup_node()
}

/// scoped 归属复核（SIGKILL 发送前）：受害者必须仍位于越限子树内、
/// 仍使用选定的 mm、且仍是其线程组的 leader。
///
/// 这是 issue #27 的防「pid 复用误杀」闸门：若候选 tgid 对应的进程已
/// 退出且 pid 被组外任务复用，`ProcessManager::find` 返回的是复用者，
/// 其归属节点必然在子树外（或它不是 leader、或已不持有该 mm），
/// 三项校验任一失败即放弃本次击杀并返回 `ESRCH` 触发重选。
fn scoped_validate_pid(
    tgid: RawPid,
    expected_mm: &Arc<AddressSpace>,
    scope: &Arc<CgroupNode>,
) -> Result<Arc<ProcessControlBlock>, SystemError> {
    let task = ProcessManager::find(tgid).ok_or(SystemError::ESRCH)?;
    if !scope.is_ancestor_of(&task_cgroup_node_of(&task)) {
        return Err(SystemError::ESRCH);
    }
    if !task_uses_mm(&task, expected_mm) {
        return Err(SystemError::ESRCH);
    }
    let leader = leader_of(task.clone());
    if leader.raw_pid() != tgid || !scope.is_ancestor_of(&task_cgroup_node_of(&leader)) {
        return Err(SystemError::ESRCH);
    }
    Ok(task)
}

/// 在 `pids` 中选出最高分候选。`scope` 为 `Some` 时是 scoped OOM
/// （`memory.max`，issue #27）：候选 leader 必须直接归属越限子树，
/// 击杀时刻仍会复核组内归属（[`scoped_validate_pid`]）。
fn select_victim_from(pids: Vec<RawPid>, scope: Option<&Arc<CgroupNode>>) -> Option<OomCandidate> {
    let total_pages = total_system_pages();
    let mut seen_tgids = Vec::new();
    let mut best: Option<OomCandidate> = None;

    for pid in pids {
        let Some(task) = ProcessManager::find(pid) else {
            continue;
        };
        let Some(mm) = task.basic().user_vm() else {
            continue;
        };

        let leader = leader_of(task);
        let tgid = leader.raw_tgid();
        if seen_tgids.contains(&tgid) {
            continue;
        }
        seen_tgids.push(tgid);

        let leader_node = task_cgroup_node_of(&leader);
        if let Some(scope) = scope {
            // 组外成员（含 pid 复用后落入本表的组外任务）直接跳过：
            // scoped OOM 的受害者只能来自越限子树。
            if !scope.is_ancestor_of(&leader_node) {
                continue;
            }
        }

        let oom_score_adj = leader.sig_info_irqsave().oom_score_adj();
        if should_skip_candidate(&leader, oom_score_adj) {
            continue;
        }

        let candidate = OomCandidate {
            tgid,
            score: oom_score(&mm, oom_score_adj, total_pages),
            resident_pages: mm.resident_pages(),
            oom_score_adj,
            mm,
        };
        if best
            .as_ref()
            .is_none_or(|current| better_candidate(&candidate, current))
        {
            best = Some(candidate);
        }
    }

    best
}

fn begin_selection() -> Result<u64, ()> {
    let mut state = OOM_STATE.lock_irqsave();
    if state.selecting || state.inflight.is_some() {
        Err(())
    } else {
        state.selecting = true;
        state.generation = state.generation.wrapping_add(1);
        Ok(state.generation)
    }
}

/// 无条件闭合本轮选择。调用前提：调用方仍**独占**自己赢得的那一轮
/// 单飞标记（`begin_selection` 成功后从未让出，`selecting` 必为真且
/// 代际为调用方的）。若槽位可能已经让出（见
/// [`out_of_memory_loop`] 的 SIGKILL 投递失败臂），必须改用
/// [`finish_selection_for`]。
fn finish_selection_none() {
    {
        let mut state = OOM_STATE.lock_irqsave();
        state.selecting = false;
    }
    wake_oom_waiters();
}

/// 按代际闭合选择（issue #27 审计）：`no_victim` 回调的第二处调用点
/// （SIGKILL 投递失败）发生在 `send_oom_sigkill` 内部已把 `selecting`
/// 转登 inflight、又回滚让出槽位**之后**——此刻别的 CPU 可能已赢得
/// 更新的一代际并置起 `selecting`，无条件清标记会把别人的单飞锁抹掉，
/// 放两个选择者并发击杀。代际不符即说明本轮标记早已被自己闭合，
/// 无需也不能再动；唤醒照发（幂等）。
fn finish_selection_for(generation: u64) {
    {
        let mut state = OOM_STATE.lock_irqsave();
        if state.generation == generation {
            state.selecting = false;
        }
    }
    wake_oom_waiters();
}

pub fn note_oom_victim_mm_released(mm_id: u64) {
    if clear_inflight_for_mm(mm_id) {
        wake_oom_waiters();
    }
}

fn send_oom_sigkill(
    generation: u64,
    candidate: &OomCandidate,
    scope: Option<&Arc<CgroupNode>>,
) -> Result<Option<RawPid>, SystemError> {
    let targets = kill_targets_for_mm(&candidate.mm, scope);
    // scoped：候选 tgid 必须在击杀时刻仍然是组内、仍持有该 mm 的
    // leader（[`scoped_validate_pid`]）；全局：保持原有 tgid 反查。
    let victim = match scope {
        Some(scope) => match scoped_validate_pid(candidate.tgid, &candidate.mm, scope) {
            Ok(victim) => victim,
            Err(_) => {
                finish_selection_none();
                return Err(SystemError::ESRCH);
            }
        },
        None => match targets
            .iter()
            .find(|target| target.raw_tgid() == candidate.tgid)
            .or_else(|| {
                targets
                    .iter()
                    .find(|target| task_uses_mm(target, &candidate.mm))
            })
            .cloned()
        {
            Some(victim) => victim,
            None => {
                finish_selection_none();
                return Err(SystemError::ESRCH);
            }
        },
    };
    let victim_tgid = victim.raw_tgid();
    let victim_mm_id = candidate.mm.id();

    let send_sigkill = |target: Arc<ProcessControlBlock>| {
        let mut info = SigInfo::new(
            Signal::SIGKILL,
            0,
            SigCode::Kernel,
            SigType::Kill {
                pid: RawPid::new(0),
                uid: 0,
            },
        );
        Signal::SIGKILL.send_signal_info_to_pcb(Some(&mut info), target, PidType::TGID)
    };

    victim.with_task_lock_irqsave(|| {
        if !task_uses_mm(&victim, &candidate.mm) {
            finish_selection_none();
            return Err(SystemError::ESRCH);
        }
        if let Some(scope) = scope {
            // SIGKILL 前最后一道归属复核（task_lock 下，与迁移的
            // task_lock 互斥）：受害者此刻必须仍在越限子树内。
            if !scope.is_ancestor_of(&task_cgroup_node_of(&victim)) {
                finish_selection_none();
                return Err(SystemError::ESRCH);
            }
        }

        let sighand = victim.sighand();
        sighand.record_oom_victim_mm(victim_tgid, &candidate.mm);
        let mut state = OOM_STATE.lock_irqsave();
        state.selecting = false;
        state.inflight = Some(OomVictimState {
            generation,
            tgid: victim_tgid,
            mm_id: victim_mm_id,
        });
        drop(state);
        match send_sigkill(victim.clone()) {
            Ok(_) => Ok(Some(victim_tgid)),
            Err(err) => {
                sighand.clear_oom_mm_if(victim_tgid, victim_mm_id);
                if rollback_inflight(generation, victim_tgid, victim_mm_id) {
                    wake_oom_waiters();
                }
                Err(err)
            }
        }
    })?;

    for target in targets {
        if target.raw_tgid() == victim_tgid {
            continue;
        }
        if let Some(scope) = scope {
            // 双重防线：发送时刻再复核一次组内归属。
            if scoped_validate_pid(target.raw_tgid(), &candidate.mm, scope).is_err() {
                continue;
            }
        }
        match send_sigkill(target) {
            Ok(_) | Err(SystemError::ESRCH) => {}
            Err(err) => warn!(
                "oom: failed to SIGKILL task group sharing victim mm: {:?}",
                err
            ),
        }
    }

    Ok(Some(victim_tgid))
}

/// Whether the current task is marked as an OOM victim.
pub fn current_is_oom_victim() -> bool {
    if !ProcessManager::initialized() {
        return false;
    }
    let current = ProcessManager::current_pcb();
    current.sighand().oom_victim_mm_matches(current.raw_tgid()) && current_is_killed_or_exiting()
}

fn wait_for_oom_slot() -> Result<(), SystemError> {
    OOM_WAITQ.wait_event_killable(
        || {
            if current_is_killed_or_exiting() {
                return true;
            }
            let state = OOM_STATE.lock_irqsave();
            if state.selecting {
                return false;
            }
            state.inflight.as_ref().is_none()
        },
        None::<fn()>,
    )
}

fn wait_until_recoverable(generation: u64) -> Result<(), SystemError> {
    OOM_WAITQ.wait_event_killable(
        || {
            if current_is_killed_or_exiting() {
                return true;
            }
            let state = OOM_STATE.lock_irqsave();
            if state.selecting {
                return false;
            }
            match state.inflight.as_ref() {
                None => true,
                Some(victim) if victim.generation == generation => false,
                Some(_) => true,
            }
        },
        None::<fn()>,
    )
}

/// Shared body of the OOM state machine: single-flight victim selection,
/// SIGKILL delivery (including group members sharing the victim mm),
/// inflight-victim tracking and killable recovery waits.
///
/// `no_victim` 决定「选不出受害者」时的去向（issue #27 核心）：全局
/// 入口记录错误并返回 NoVictim；scoped 入口绝不逃逸到全局选择，只能
/// 在越限子树内恢复（触发者自杀或等待），对齐 Linux 6.6
/// `out_of_memory()` 中 `is_memcg_oom` 分支不 panic、不升格全局的语义。
/// 选择代际 `generation` 传入回调：scoped 自杀路径要按同一代际登记
/// inflight 受害者（与 [`send_oom_sigkill`] 相同的记账协议），并发
/// 触发者才会等待这次击杀回收内存，而不是抢先另起一轮选择。
fn out_of_memory_loop(
    ctx: OomContext,
    scope: Option<&Arc<CgroupNode>>,
    select: &mut dyn FnMut() -> Option<OomCandidate>,
    no_victim: &mut dyn FnMut(u64) -> OomOutcome,
) -> OomOutcome {
    loop {
        if current_is_killed_or_exiting() {
            return OomOutcome::CurrentTaskKilled;
        }

        let generation = match begin_selection() {
            Ok(generation) => generation,
            Err(()) => {
                let _ = wait_for_oom_slot();
                continue;
            }
        };

        let Some(candidate) = select() else {
            // 选择失败的去向由入口决定；本代际的 selecting 标记由
            // no_victim 负责闭合（自杀路径要把它转登记为 inflight）。
            return no_victim(generation);
        };

        let current = ProcessManager::current_pcb();
        let current_leader = leader_of(current.clone());
        let current_is_victim = candidate.tgid == current_leader.raw_tgid()
            || (task_uses_mm(&current, &candidate.mm)
                && !is_global_init_or_kthread(&current_leader));
        let candidate_tgid = candidate.tgid;
        let victim_score = candidate.score;
        let victim_oom_score_adj = candidate.oom_score_adj;
        let victim_resident_pages = candidate.resident_pages;
        match send_oom_sigkill(generation, &candidate, scope) {
            Ok(killed_tgid) => {
                if let Some(killed_tgid) = killed_tgid {
                    count_oom_kill();
                    error!(
                        "oom-kill: trigger_pid={} trigger_tgid={} victim_tgid={} score={} adj={} rss={} order={} addr={:#x} ip={:#x}",
                        ctx.trigger_pid,
                        ctx.trigger_tgid,
                        killed_tgid,
                        victim_score,
                        victim_oom_score_adj,
                        victim_resident_pages,
                        ctx.order,
                        ctx.fault_address.data(),
                        ctx.fault_ip
                    );
                }
                drop(candidate);
                if current_is_victim {
                    return OomOutcome::CurrentTaskKilled;
                }
                match wait_until_recoverable(generation) {
                    Ok(()) if current_is_killed_or_exiting() => {
                        return OomOutcome::CurrentTaskKilled;
                    }
                    Ok(()) => return OomOutcome::Retry,
                    Err(_) if current_is_killed_or_exiting() => {
                        return OomOutcome::CurrentTaskKilled;
                    }
                    Err(_) => return OomOutcome::Retry,
                }
            }
            Err(SystemError::ESRCH) => {
                continue;
            }
            Err(err) => {
                warn!(
                    "oom: failed to SIGKILL victim tgid={} for trigger pid={} err={:?}",
                    candidate_tgid, ctx.trigger_pid, err
                );
                return no_victim(generation);
            }
        }
    }
}

/// Global (system-wide) OOM entry from the page-fault path.
pub fn pagefault_out_of_memory(ctx: OomContext) -> OomOutcome {
    // A pending cgroup-scoped refusal (memory.max) takes precedence over a
    // global kill: victims inside the offending cgroup relieve the charge
    // without touching unrelated tasks.
    if let Some(outcome) = super::memcg::drain_pending_memcg_oom(ctx) {
        return outcome;
    }
    out_of_memory_loop(ctx, None, &mut select_victim, &mut |generation| {
        finish_selection_for(generation);
        error!(
            "oom: no victim for trigger pid={} tgid={} addr={:#x} ip={:#x}",
            ctx.trigger_pid,
            ctx.trigger_tgid,
            ctx.fault_address.data(),
            ctx.fault_ip
        );
        OomOutcome::NoVictim
    })
}

/// Cgroup-scoped OOM（`memory.max`，issue #27）：受害者只能来自越限子树。
///
/// 与旧实现的两个本质区别：
/// 1. 作用域以 `Arc<CgroupNode>` 传入（而非一次性 pid 快照），每次选择
///    （首轮、ESRCH 重试、inflight 抢占后重选）都重新遍历子树收集成员
///    （[`select_scoped_victim`]）；
/// 2. SIGKILL 发送前对击杀目标复核仍在越限子树内
///    （[`scoped_validate_pid`] + [`send_oom_sigkill`] 的 task_lock 复核），
///    组外任务（含 pid 复用者、组外 mm 共享者）绝不会被击杀。
///
/// 组内选不出受害者时**绝不**逃逸为全局选择（[`scoped_no_victim`]）。
pub fn scoped_out_of_memory(ctx: OomContext, scope: Arc<CgroupNode>) -> OomOutcome {
    out_of_memory_loop(
        ctx,
        Some(&scope),
        &mut || select_scoped_victim(&scope),
        &mut |generation| scoped_no_victim(ctx, &scope, generation),
    )
}

/// 越限子树此刻是否已解除超限（`memory.max` 被上调、残量已被其它路径
/// 释放等）。作用域内任一在线 memory CSS 仍超限即返回 `false`——与
/// `memcg::find_max_exceeded` 的「沿链找第一个超限层」同一判据：超限
/// 已解除时 scoped 等待必须立即结束，否则触发者会白等到超时。
fn scope_pressure_relieved(scope: &Arc<CgroupNode>) -> bool {
    let mut stack = vec![scope.clone()];
    while let Some(node) = stack.pop() {
        if let Some(css) = node.css(CgroupSubsysId::Memory) {
            if let Some(memcg) = css.as_any().downcast_ref::<MemoryCss>() {
                if memcg.max_exceeded_now() {
                    return false;
                }
            }
        }
        stack.extend(node.children());
    }
    true
}

/// 触发者自杀路径的记账核心（issue #27 审计修正）：
///
/// 与 [`send_oom_sigkill`] 走完全相同的受害者登记协议——
/// `record_oom_victim_mm`（等价 Linux `mark_oom_victim` 的 TIF_MEMDIE）
/// + `inflight` 登记 + SIGKILL 投递，失败时成对回滚。缺了登记，被杀
/// 触发者的退出路径会重新落入 memcg 计费拒绝（`memcg_alloc_charge`
/// 依赖 `current_is_oom_victim()` 给受害者放行 reserve 访问，
/// `retry_oom_victim_page_frame_alloc` 同理），退出释放的 mm 也无法
/// 经 `note_oom_victim_mm_released` 解除 inflight、唤醒并发触发者。
///
/// 调用前提：调用方曾以 `generation` 赢得单飞选择。两处调用点的槽位
/// 所有权不同（选择失败=仍独占；SIGKILL 投递失败=已在
/// `send_oom_sigkill` 的回滚中让出），因此本函数必须**先验证书与**
/// 再转移：只有 `OOM_STATE` 仍是本代际且 `selecting` 仍置起时，才把
/// selecting 转登记为 inflight；否则（别的 CPU 已赢得更新的一代际，
/// 或本轮标记已闭合）配对撤销登记并返回 `EBUSY`，绝不劫持别人的单飞
/// 槽位。锁序与 [`send_oom_sigkill`] 一致：先 `sighand` 记账后取
/// `OOM_STATE`，不得倒置。
fn mark_current_oom_victim(generation: u64) -> Result<(), SystemError> {
    let current = ProcessManager::current_pcb();
    current.with_task_lock_irqsave(|| {
        let Some(mm) = current.basic().user_vm() else {
            // 无用户地址空间（理论不可达：越限 charge 来自用户 mm 的
            // 缺页；防御性拒绝自杀登记，调用方退化为等待路径）。
            return Err(SystemError::ESRCH);
        };
        let tgid = current.raw_tgid();
        let mm_id = mm.id();
        let sighand = current.sighand();
        sighand.record_oom_victim_mm(tgid, &mm);
        {
            let mut state = OOM_STATE.lock_irqsave();
            if state.generation != generation || !state.selecting {
                // 槽位已不在手上（见文档注释）：撤销记账后让路。
                drop(state);
                sighand.clear_oom_mm_if(tgid, mm_id);
                return Err(SystemError::EBUSY);
            }
            state.selecting = false;
            state.inflight = Some(OomVictimState {
                generation,
                tgid,
                mm_id,
            });
        }
        let mut info = SigInfo::new(
            Signal::SIGKILL,
            0,
            SigCode::Kernel,
            SigType::Kill {
                pid: RawPid::new(0),
                uid: 0,
            },
        );
        match Signal::SIGKILL.send_signal_info_to_pcb(
            Some(&mut info),
            current.clone(),
            PidType::TGID,
        ) {
            Ok(_) => Ok(()),
            Err(err) => {
                sighand.clear_oom_mm_if(tgid, mm_id);
                if rollback_inflight(generation, tgid, mm_id) {
                    wake_oom_waiters();
                }
                Err(err)
            }
        }
    })
}

/// scoped 无组内受害者时的恢复决策（issue #27 防逃逸核心）。
///
/// 对齐 Linux 6.6 的三段语义，绝不返回 `NoVictim`、绝不全局选受害者：
/// 1. 触发者**可杀**且在越限子树内 → 登记为 OOM 受害者并自杀
///    （`out_of_memory()` 的 `task_will_free_mem(current)` →
///    `mark_oom_victim` + 回收），其 mm 消亡即释放组内额度；
/// 2. 触发者**不可杀**（pid1/kthread/vfork 等待/`oom_score_adj=-1000`
///    ——Linux 的 `oom_unkillable_task` 不会杀它们）且在组内 →
///    killable 等待组内回收或越限解除（`mem_cgroup_oom_synchronize`
///    的 TASK_KILLABLE 等待），绝不升级为击杀不可杀任务；
/// 3. 触发者在组外 → `Retry`，由 charge 重试驱动，行为有界、
///    组外进程零误伤。
fn scoped_no_victim(ctx: OomContext, scope: &Arc<CgroupNode>, generation: u64) -> OomOutcome {
    if current_is_killed_or_exiting() {
        // 本臂只可能来自「选择失败」调用点（此时槽位仍在手上），但
        // 统一走按代际闭合，规则只有一条：no_victim 永不无条件清
        // selecting（见 [`finish_selection_for`]）。
        finish_selection_for(generation);
        return OomOutcome::CurrentTaskKilled;
    }
    let current = ProcessManager::current_pcb();
    let current_leader = leader_of(current.clone());
    let current_node = task_cgroup_node_of(&current_leader);
    if scope.is_ancestor_of(&current_node) {
        let oom_score_adj = current_leader.sig_info_irqsave().oom_score_adj();
        if !should_skip_candidate(&current_leader, oom_score_adj) {
            match mark_current_oom_victim(generation) {
                Ok(()) => {
                    error!(
                        "oom: cgroup scope has no killable victim, killing trigger tgid={} addr={:#x} ip={:#x}",
                        current.raw_tgid(),
                        ctx.fault_address.data(),
                        ctx.fault_ip
                    );
                    count_oom_kill();
                    return OomOutcome::CurrentTaskKilled;
                }
                Err(err) => {
                    // 自杀登记失败：`EBUSY`=槽位已被更新的一代际赢得
                    // （绝不能再动别人的 selecting），`ESRCH`/信号错误
                    // =本轮标记仍在手上。按代际闭合两种情形都正确。
                    warn!(
                        "oom: scoped trigger self-kill bookkeeping failed tgid={} err={:?}",
                        current.raw_tgid(),
                        err
                    );
                    finish_selection_for(generation);
                }
            }
        } else {
            // 触发者不可杀：等待组内其它回收或 max 上调，对齐
            // `mem_cgroup_oom_synchronize` 的 killable 等待。
            finish_selection_for(generation);
            let _ = OOM_WAITQ.wait_event_interruptible_timeout(
                || current_is_killed_or_exiting() || scope_pressure_relieved(scope),
                Some(Duration::from_millis(SCOPED_OOM_NO_VICTIM_WAIT_MS)),
            );
        }
        return OomOutcome::Retry;
    }
    // 组外触发者：不等待（越限解除由组内任务推进），直接让 charge
    // 重试驱动本次缺页前进。
    finish_selection_for(generation);
    OomOutcome::Retry
}

pub fn notify_mm_drop(mm_id: u64) {
    if clear_inflight_for_mm(mm_id) {
        wake_oom_waiters();
    }
}

pub fn should_inject_fault_oom() -> bool {
    let current_tgid = ProcessManager::current_pcb().raw_tgid();
    let mut cfg = OOM_FAULT_INJECT.lock_irqsave();
    if cfg.target_tgid != Some(current_tgid) {
        return false;
    }
    if !cfg.is_enabled() {
        return false;
    }

    let hit = cfg.seen >= cfg.fail_after;
    cfg.seen = cfg.seen.saturating_add(1);
    if !hit {
        return false;
    }

    match cfg.remaining_failures.as_mut() {
        Some(0) => false,
        Some(remaining) => {
            *remaining = remaining.saturating_sub(1);
            true
        }
        None => true,
    }
}

pub fn read_fault_inject_config() -> alloc::string::String {
    let cfg = OOM_FAULT_INJECT.lock_irqsave();
    let target = cfg.target_tgid.map(|pid| pid.data()).unwrap_or(0);
    let remaining = cfg
        .remaining_failures
        .map(|count| count.to_string())
        .unwrap_or_else(|| "persistent".to_string());
    format!(
        "target_tgid={} fail_after={} seen={} remaining={}\n",
        target, cfg.fail_after, cfg.seen, remaining
    )
}

pub fn write_fault_inject_config(data: &[u8]) -> Result<usize, SystemError> {
    let input = core::str::from_utf8(data).map_err(|_| SystemError::EINVAL)?;
    let parts: Vec<&str> = input.split_whitespace().collect();
    if parts.is_empty() {
        return Err(SystemError::EINVAL);
    }

    let target: usize = parts[0].parse().map_err(|_| SystemError::EINVAL)?;
    let mut cfg = OOM_FAULT_INJECT.lock_irqsave();
    if target == 0 {
        *cfg = OomFaultInject::disabled();
        return Ok(data.len());
    }

    let fail_after = parts
        .get(1)
        .copied()
        .unwrap_or("0")
        .parse()
        .map_err(|_| SystemError::EINVAL)?;
    let fail_times: usize = parts
        .get(2)
        .copied()
        .unwrap_or("1")
        .parse()
        .map_err(|_| SystemError::EINVAL)?;

    *cfg = OomFaultInject {
        target_tgid: Some(RawPid::new(target)),
        fail_after,
        seen: 0,
        remaining_failures: if fail_times == 0 {
            None
        } else {
            Some(fail_times)
        },
    };
    Ok(data.len())
}
