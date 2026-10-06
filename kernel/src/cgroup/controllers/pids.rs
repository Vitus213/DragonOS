use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use system_error::SystemError;

use crate::{
    cgroup::core::CgroupNode,
    libs::{rwlock::RwLock, spinlock::SpinLock},
    process::ProcessControlBlock,
};

use super::super::subsys::{
    CfType, CfTypeFlags, CgroupSubsys, CgroupSubsysId, CgroupSubsysState, CssFlags,
};

/// pids 控制器状态。
#[derive(Debug)]
pub struct PidsCgroupState {
    cgroup: Weak<CgroupNode>,
    flags: SpinLock<CssFlags>,
    /// 最大 pids 数量（None 表示无限制）。
    max: RwLock<Option<usize>>,
    /// 当前 cgroup 的本地任务数。
    local_counter: AtomicUsize,
    /// 当前 cgroup 子树任务数，包含本地任务。
    subtree_counter: AtomicUsize,
    /// pids.events:max 触发次数。
    events_max: AtomicU64,
}

impl PidsCgroupState {
    pub fn new(cgroup: Weak<CgroupNode>) -> Self {
        Self {
            cgroup,
            flags: SpinLock::new(CssFlags::default()),
            max: RwLock::new(None),
            local_counter: AtomicUsize::new(0),
            subtree_counter: AtomicUsize::new(0),
            events_max: AtomicU64::new(0),
        }
    }

    pub fn set_max(&self, max: Option<usize>) {
        *self.max.write() = max;
    }

    pub fn get_max(&self) -> Option<usize> {
        *self.max.read()
    }

    pub fn local_current(&self) -> usize {
        self.local_counter.load(Ordering::Acquire)
    }

    pub fn subtree_current(&self) -> usize {
        self.subtree_counter.load(Ordering::Acquire)
    }

    pub fn events_max(&self) -> u64 {
        self.events_max.load(Ordering::Acquire)
    }

    pub fn inc_events_max(&self) {
        self.events_max.fetch_add(1, Ordering::Relaxed);
    }

    fn ancestors(&self) -> Vec<Arc<CgroupNode>> {
        let mut nodes = Vec::new();
        let Some(cgroup) = self.cgroup.upgrade() else {
            return nodes;
        };
        let mut current = cgroup.parent();
        while let Some(node) = current {
            nodes.push(node.clone());
            current = node.parent();
        }
        nodes
    }

    fn with_state<R>(node: &Arc<CgroupNode>, f: impl FnOnce(&Self) -> R) -> Option<R> {
        let css = node.css(CgroupSubsysId::Pids)?;
        let state = css.as_any().downcast_ref::<Self>()?;
        Some(f(state))
    }

    /// fork 前为当前 cgroup 及其所有祖先预留一个计数。
    pub fn try_charge(&self) -> Result<(), SystemError> {
        let Some(cgroup) = self.cgroup.upgrade() else {
            return Err(SystemError::ENOENT);
        };
        let ancestors = self.ancestors();

        if let Some(max) = self.get_max() {
            if self.local_current().saturating_add(1) > max {
                self.inc_events_max();
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        }
        for ancestor in &ancestors {
            let exceeded = Self::with_state(ancestor, |state| {
                state
                    .get_max()
                    .is_some_and(|max| state.subtree_current().saturating_add(1) > max)
            })
            .unwrap_or(false);
            if exceeded {
                Self::with_state(ancestor, |state| state.inc_events_max());
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        }

        self.local_counter.fetch_add(1, Ordering::AcqRel);
        self.subtree_counter.fetch_add(1, Ordering::AcqRel);
        for ancestor in ancestors {
            Self::with_state(&ancestor, |state| {
                state.subtree_counter.fetch_add(1, Ordering::AcqRel);
            });
        }
        let _ = cgroup;
        Ok(())
    }

    /// 无条件增加层级计数，用于 Linux 语义下不受 pids.max 阻塞的任务迁移。
    ///
    /// 任务迁移是组织操作；Linux 允许迁入后暂时超过 pids.max，只有 fork/clone
    /// 受限。调用方必须持有 cgroup accounting lock。
    pub fn charge_unchecked(&self) {
        self.local_counter.fetch_add(1, Ordering::AcqRel);
        self.subtree_counter.fetch_add(1, Ordering::AcqRel);
        for ancestor in self.ancestors() {
            Self::with_state(&ancestor, |state| {
                state.subtree_counter.fetch_add(1, Ordering::AcqRel);
            });
        }
    }

    /// 释放一个任务的层级计数。
    ///
    /// 全部递减走饱和路径：pids 计数与任务配对在正确语义下非零，但一旦
    /// 任何失配（历史缺陷、重复释放）发生，裸 `fetch_sub` 会把
    /// `pids.current`/`pids.events` 依赖的计数翻转成 `usize::MAX`，
    /// try_charge/can_attach 从此恒 EAGAIN——整个 cgroup 永久无法 fork。
    /// 饱和停在 0 把损害限制为计数偏低（下次正常 charge 即可恢复）。
    pub fn uncharge(&self) {
        crate::cgroup::core::saturating_sub(&self.local_counter);
        crate::cgroup::core::saturating_sub(&self.subtree_counter);
        for ancestor in self.ancestors() {
            Self::with_state(&ancestor, |state| {
                crate::cgroup::core::saturating_sub(&state.subtree_counter);
            });
        }
    }

    /// 把 `count` 个任务的层级 pids 计数从 `src` 组搬入本组（forward=true），
    /// 或反向搬回（forward=false）。
    ///
    /// 对应 Linux 6.6 `pids_can_attach` 的 `pids_charge(dst)+pids_uncharge(src)`
    /// 与 `pids_cancel_attach` 的反向搬运。迁移是组织操作，Linux 允许迁入后
    /// 暂时超过 `pids.max`，只有 fork/clone 受限，因此这里无条件搬运、不做
    /// max 检查（max 检查只属于 `try_charge`/fork 路径）。
    ///
    /// 源组 pids 状态缺失时整体不动作并返回 `false`，调用方无需按前缀回退；
    /// 调用方必须持有 `cgroup_accounting_lock`，与 rmdir/冻结请求串行化。
    pub fn transfer_for_migration(&self, src: &CgroupNode, count: usize, forward: bool) -> bool {
        if count == 0 {
            return true;
        }
        let Some(src_css) = src.css(CgroupSubsysId::Pids) else {
            return false;
        };
        let Some(src_state) = src_css.as_any().downcast_ref::<Self>() else {
            return false;
        };
        for _ in 0..count {
            if forward {
                self.charge_unchecked();
                src_state.uncharge();
            } else {
                self.uncharge();
                src_state.charge_unchecked();
            }
        }
        true
    }

    /// 迁移预演核心：逐任务把层级计数从 `srcs[i]` 搬入本组。
    ///
    /// 整组要么全预演成功、要么全部撤销：第 `idx` 个任务的源组 pids
    /// 状态缺失（异常拓扑）时，先把已施加于前 `idx` 个任务的搬运逐一
    /// 反向撤销（与 Linux `pids_try_charge` 失败时的 revert 循环同构），
    /// 再返回错误；因此失败方无需迁移事务再为本控制器调用
    /// cancel_attach。抽出为不依赖任务对象的形状（源组数组），使其可在
    /// 宿主单测中直接构造层级计数验证（issue #38）。
    pub fn precharge_migration(&self, srcs: &[Arc<CgroupNode>]) -> Result<(), SystemError> {
        for (idx, src) in srcs.iter().enumerate() {
            if !self.transfer_for_migration(src, 1, true) {
                for prev in &srcs[..idx] {
                    self.transfer_for_migration(prev, 1, false);
                }
                return Err(SystemError::ENOENT);
            }
        }
        Ok(())
    }

    /// 迁移回退核心：把预搬入本组的层级计数逐任务搬回各自旧组。
    ///
    /// 对应 Linux 6.6 `pids_cancel_attach`；与 `precharge_migration` 严格
    /// 互逆（搬运顺序不影响原子性：每对操作各自闭合）。
    pub fn revert_migration(&self, srcs: &[Arc<CgroupNode>]) {
        for src in srcs {
            self.transfer_for_migration(src, 1, false);
        }
    }
}

impl CgroupSubsysState for PidsCgroupState {
    fn subsys_id(&self) -> CgroupSubsysId {
        CgroupSubsysId::Pids
    }

    fn cgroup_node(&self) -> Option<Arc<CgroupNode>> {
        // fail-closed（issue #30）：与 subsys trait 一致，节点已随
        // rmdir 拆除时返回 None，绝不 panic。
        self.cgroup.upgrade()
    }

    fn parent(&self) -> Option<Arc<dyn CgroupSubsysState>> {
        self.cgroup
            .upgrade()
            .and_then(|cgroup| cgroup.parent())
            .and_then(|parent| parent.css(CgroupSubsysId::Pids))
    }

    fn flags(&self) -> CssFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, flags: CssFlags) {
        *self.flags.lock() = flags;
    }

    /// 迁移预演：逐任务把层级 pids 计数从任务旧组搬入本组
    /// （委托 `precharge_migration`）。
    ///
    /// 对应 Linux 6.6 `pids_can_attach`。提交点尚未到达，任务归属仍为
    /// 旧组，逐任务 `task_cgroup_node()` 取源，与 Linux 在 can_attach
    /// 与 cancel_attach 时刻重读 `task_css(task, PID)` 的行为一致。
    fn can_attach(&self, tasks: &[Arc<ProcessControlBlock>]) -> Result<(), SystemError> {
        let srcs: Vec<Arc<CgroupNode>> = tasks.iter().map(|task| task.task_cgroup_node()).collect();
        self.precharge_migration(&srcs)
    }

    /// 迁移失败回退：把预搬入本组的层级 pids 计数逐任务搬回旧组
    /// （委托 `revert_migration`）。
    ///
    /// 对应 Linux 6.6 `pids_cancel_attach`；由迁移事务在后续控制器的
    /// can_attach 失败时对已完整执行过 can_attach 的前序控制器调用。
    fn cancel_attach(&self, tasks: &[Arc<ProcessControlBlock>]) {
        let srcs: Vec<Arc<CgroupNode>> = tasks.iter().map(|task| task.task_cgroup_node()).collect();
        self.revert_migration(&srcs);
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// pids 子系统。
#[derive(Debug)]
pub struct PidsSubsys;

impl CgroupSubsys for PidsSubsys {
    fn id(&self) -> CgroupSubsysId {
        CgroupSubsysId::Pids
    }

    fn name(&self) -> &'static str {
        "pids"
    }

    fn css_alloc(
        &self,
        _parent: Option<&Arc<dyn CgroupSubsysState>>,
        cgroup: &Arc<CgroupNode>,
    ) -> Result<Arc<dyn CgroupSubsysState>, SystemError> {
        Ok(Arc::new(PidsCgroupState::new(Arc::downgrade(cgroup))))
    }

    fn css_free(&self, _css: &Arc<dyn CgroupSubsysState>) {}

    fn dfl_cftypes(&self) -> Vec<CfType> {
        vec![
            CfType {
                name: "pids.max".to_string(),
                flags: CfTypeFlags::new(),
                max_write_len: 64,
                read: Some(pids_max_read),
                write: Some(pids_max_write),
            },
            CfType {
                name: "pids.current".to_string(),
                flags: CfTypeFlags::new(),
                max_write_len: 0,
                read: Some(pids_current_read),
                write: None,
            },
            CfType {
                name: "pids.events".to_string(),
                flags: CfTypeFlags::new(),
                max_write_len: 0,
                read: Some(pids_events_read),
                write: None,
            },
        ]
    }
}

fn pids_state(css: &Arc<dyn CgroupSubsysState>) -> Result<&PidsCgroupState, SystemError> {
    css.as_any().downcast_ref().ok_or(SystemError::EINVAL)
}

fn pids_max_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    Ok(match pids_state(css)?.get_max() {
        Some(max) => format!("{}\n", max),
        None => "max\n".to_string(),
    })
}

fn pids_max_write(css: &Arc<dyn CgroupSubsysState>, input: &str) -> Result<(), SystemError> {
    let value = input.trim();
    let max = if value == "max" {
        None
    } else {
        Some(value.parse::<usize>().map_err(|_| SystemError::EINVAL)?)
    };
    pids_state(css)?.set_max(max);
    Ok(())
}

fn pids_current_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    Ok(format!("{}\n", pids_state(css)?.local_current()))
}

fn pids_events_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    Ok(format!("max {}\n", pids_state(css)?.events_max()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单次计数失配（重复 uncharge）不得把 pids.current 翻转为
    /// usize::MAX。修复前 `fetch_sub` 从 0 减 1 溢出为极大值，
    /// try_charge 从此恒 EAGAIN——整个 cgroup 永久无法
    /// fork（issue #29 SOP-3 的组级雪崩）。饱和递减停在 0。
    /// （issue #38 收编后，原 `can_attach(count)` 预检函数已删除：
    /// Linux 6.6 语义下组织迁移不受 pids.max 阻塞，计数搬运改由
    /// trait can_attach/cancel_attach 事务钩子无条件执行，fork 门槛
    /// 判定即下方 saturating_add 表达式。）
    #[test]
    fn over_uncharge_saturates_and_keeps_fork_gate_open() {
        let state = PidsCgroupState::new(Weak::new());
        state.set_max(Some(1));

        // charge → uncharge → 再 uncharge（模拟双移/重复释放的失配）。
        state.charge_unchecked();
        state.uncharge();
        state.uncharge();

        assert_eq!(state.local_current(), 0);
        assert_eq!(state.subtree_current(), 0);
        // 关键回归：计数未翻转为 usize::MAX，try_charge/cgroup_can_fork_in
        // 的门槛判定 `local_current().saturating_add(1) > max` 仍然放行
        // （修复前 local_current 溢出后该判定恒 EAGAIN）。
        let fork_gate_ok = state.local_current().saturating_add(1) <= state.get_max().unwrap();
        assert!(fork_gate_ok);
    }

    #[test]
    fn charge_uncharge_pairs_track_counts() {
        let state = PidsCgroupState::new(Weak::new());
        state.charge_unchecked();
        state.charge_unchecked();
        assert_eq!(state.local_current(), 2);
        assert_eq!(state.subtree_current(), 2);
        state.uncharge();
        assert_eq!(state.local_current(), 1);
        state.uncharge();
        assert_eq!(state.local_current(), 0);
    }

    /// 给节点装 Pids css 并返回状态句柄。宿主测试环境的
    /// `SUBSYS_REGISTRY` 为空，`create_child` 不自动挂 css，正好允许手动
    /// 构造「有/无 Pids 控制器」的层级拓扑；每个用例用唯一节点名在
    /// 全局 `cgroup_root()` 下挂私有子链，互不串扰。
    fn install_pids(node: &Arc<CgroupNode>) -> Arc<PidsCgroupState> {
        let state = Arc::new(PidsCgroupState::new(Arc::downgrade(node)));
        node.set_css(CgroupSubsysId::Pids, state.clone());
        state
    }

    /// 搭 `mnt ─ {src, dst}` 子链，三节点全装 Pids；orphan 参数指定
    /// 一个不装 Pids 的旁支节点名（缺 css 异常拓扑用例用）。
    struct TxnFixture {
        mnt: Arc<CgroupNode>,
        src: Arc<CgroupNode>,
        dst: Arc<CgroupNode>,
        src_state: Arc<PidsCgroupState>,
        dst_state: Arc<PidsCgroupState>,
        mnt_state: Arc<PidsCgroupState>,
    }

    fn txn_fixture(tag: &str) -> TxnFixture {
        let root_mgr = crate::cgroup::core::cgroup_root().clone();
        let mnt = root_mgr
            .create_child(&root_mgr.root(), &format!("i38-{tag}-mnt"))
            .unwrap();
        let src = root_mgr
            .create_child(&mnt, &format!("i38-{tag}-src"))
            .unwrap();
        let dst = root_mgr
            .create_child(&mnt, &format!("i38-{tag}-dst"))
            .unwrap();
        TxnFixture {
            mnt_state: install_pids(&mnt),
            src_state: install_pids(&src),
            dst_state: install_pids(&dst),
            mnt,
            src,
            dst,
        }
    }

    /// 迁移预演/回退的层级计数事务核心（issue #38 SOP-1/SOP-3）：
    /// `precharge_migration` 把 2 个任务的计数从 src 搬入 dst 子链
    /// （共同祖先 mnt 总量守恒），`revert_migration` 与之严格互逆——
    /// 回退后各级 local/subtree 逐字段回到迁移前。
    #[test]
    fn precharge_and_revert_transfer_counts_across_levels_atomically() {
        let f = txn_fixture("tx");
        let snap = |s: &PidsCgroupState| (s.local_current(), s.subtree_current());

        f.src_state.charge_unchecked();
        f.src_state.charge_unchecked();
        let before = (snap(&f.src_state), snap(&f.dst_state), snap(&f.mnt_state));
        assert_eq!(before.0, (2, 2));
        assert_eq!(before.2, (0, 2)); // mnt 只见 src 的 2 个层级任务

        f.dst_state
            .precharge_migration(&[f.src.clone(), f.src.clone()])
            .unwrap();
        assert_eq!(snap(&f.src_state), (0, 0));
        assert_eq!(snap(&f.dst_state), (2, 2));
        assert_eq!(snap(&f.mnt_state), before.2); // 迁移只改分布，不改层级总数

        // cancel_attach 的底层核心：与预演严格互逆。
        f.dst_state
            .revert_migration(&[f.src.clone(), f.src.clone()]);
        assert_eq!(
            (snap(&f.src_state), snap(&f.dst_state), snap(&f.mnt_state)),
            before,
            "回退后各级 pids 计数必须逐字段回到迁移前"
        );
    }

    /// 预演内部的前缀自撤（Linux `pids_try_charge` 失败 revert 循环同构）：
    /// 第 2 个任务的源组缺 Pids css（异常拓扑）时，第 1 个任务已施加的
    /// 搬运必须就地撤销并返回 ENOENT——失败方无需再为本控制器 cancel。
    #[test]
    fn precharge_reverts_prefix_when_a_src_lacks_pids_css() {
        let f = txn_fixture("pf");
        let root_mgr = crate::cgroup::core::cgroup_root().clone();
        let orphan = root_mgr.create_child(&f.mnt, "i38-pf-orphan").unwrap(); // 不装 Pids css

        f.src_state.charge_unchecked();
        let err = f
            .dst_state
            .precharge_migration(&[f.src.clone(), orphan])
            .expect_err("缺 Pids css 的源组必须使预演失败");
        assert_eq!(err, SystemError::ENOENT);
        // 前缀（任务 #0）搬运已自撤：dst 无残留，src 计数复原。
        assert_eq!(f.dst_state.local_current(), 0);
        assert_eq!(f.dst_state.subtree_current(), 0);
        assert_eq!(f.src_state.local_current(), 1);
        assert_eq!(f.src_state.subtree_current(), 1);
        assert_eq!(f.mnt_state.subtree_current(), 1);
    }

    /// 语义取舍锁定（issue #38）：迁移是组织操作，`precharge_migration`
    /// 无条件搬运、不受 `pids.max` 阻塞（Linux `pids_can_attach` 无 max
    /// 检查，只有 fork/clone 受限）；与被删除的死代码 max 门形态相反。
    #[test]
    fn migration_precharge_ignores_pids_max_and_revert_restores() {
        let f = txn_fixture("mx");
        f.dst_state.set_max(Some(1)); // dst 上限 1，迁入 2 也必须放行

        f.src_state.charge_unchecked();
        f.src_state.charge_unchecked();
        f.dst_state
            .precharge_migration(&[f.src.clone(), f.src.clone()])
            .expect("迁入不受 pids.max 阻塞");
        assert_eq!(f.dst_state.local_current(), 2);
        f.dst_state
            .revert_migration(&[f.src.clone(), f.src.clone()]);
        assert_eq!(f.dst_state.local_current(), 0);
        assert_eq!(f.src_state.local_current(), 2);
    }
}
