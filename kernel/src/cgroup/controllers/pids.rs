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
    pub fn uncharge(&self) {
        self.local_counter.fetch_sub(1, Ordering::AcqRel);
        self.subtree_counter.fetch_sub(1, Ordering::AcqRel);
        for ancestor in self.ancestors() {
            Self::with_state(&ancestor, |state| {
                state.subtree_counter.fetch_sub(1, Ordering::AcqRel);
            });
        }
    }

    pub fn can_attach(&self, count: usize) -> Result<(), SystemError> {
        if self
            .get_max()
            .is_some_and(|max| self.local_current().saturating_add(count) > max)
        {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        for ancestor in self.ancestors() {
            let exceeded = Self::with_state(&ancestor, |state| {
                state
                    .get_max()
                    .is_some_and(|max| state.subtree_current().saturating_add(count) > max)
            })
            .unwrap_or(false);
            if exceeded {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        }
        Ok(())
    }
}

impl CgroupSubsysState for PidsCgroupState {
    fn subsys_id(&self) -> CgroupSubsysId {
        CgroupSubsysId::Pids
    }

    fn cgroup(&self) -> Arc<CgroupNode> {
        self.cgroup.upgrade().expect("pids cgroup dropped")
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
        Some(
            value
                .parse::<usize>()
                .map_err(|_| SystemError::EINVAL)?,
        )
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
