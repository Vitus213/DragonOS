use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::fmt::Debug;
use system_error::SystemError;

use crate::{
    libs::rwlock::RwLock,
    process::ProcessControlBlock,
};

use super::core::CgroupNode;

/// Cgroup 子系统 ID 枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum CgroupSubsysId {
    Cpu = 0,
    Memory = 1,
    Pids = 2,
    Io = 3,
    Cpuset = 4,
    Freezer = 5,
    Hugetlb = 6,
    Rdma = 7,
    Misc = 8,
}

impl CgroupSubsysId {
    pub const COUNT: usize = 9;

    pub fn name(&self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Pids => "pids",
            Self::Io => "io",
            Self::Cpuset => "cpuset",
            Self::Freezer => "freezer",
            Self::Hugetlb => "hugetlb",
            Self::Rdma => "rdma",
            Self::Misc => "misc",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "cpu" => Some(Self::Cpu),
            "memory" => Some(Self::Memory),
            "pids" => Some(Self::Pids),
            "io" => Some(Self::Io),
            "cpuset" => Some(Self::Cpuset),
            "freezer" => Some(Self::Freezer),
            "hugetlb" => Some(Self::Hugetlb),
            "rdma" => Some(Self::Rdma),
            "misc" => Some(Self::Misc),
            _ => None,
        }
    }

    pub fn all() -> &'static [CgroupSubsysId] {
        &[
            Self::Cpu,
            Self::Memory,
            Self::Pids,
            Self::Io,
            Self::Cpuset,
            Self::Freezer,
            Self::Hugetlb,
            Self::Rdma,
            Self::Misc,
        ]
    }
}

/// Cgroup 子系统状态标志
#[derive(Debug, Clone, Copy)]
pub struct CssFlags(u32);

impl CssFlags {
    pub const NO_REF: u32 = 1 << 0; // 不需要引用计数
    pub const ONLINE: u32 = 1 << 1; // 已上线
    pub const RELEASED: u32 = 1 << 2; // 已释放
    pub const VISIBLE: u32 = 1 << 3; // 用户可见
    pub const DYING: u32 = 1 << 4; // 正在销毁

    pub fn new() -> Self {
        Self(0)
    }

    pub fn set(&mut self, flag: u32) {
        self.0 |= flag;
    }

    pub fn clear(&mut self, flag: u32) {
        self.0 &= !flag;
    }

    pub fn contains(&self, flag: u32) -> bool {
        self.0 & flag != 0
    }
}

impl Default for CssFlags {
    fn default() -> Self {
        Self::new()
    }
}

/// Cgroup 子系统状态（每个 cgroup 节点在每个子系统中的状态）
///
/// 对应 Linux 的 `struct cgroup_subsys_state`
pub trait CgroupSubsysState: Debug + Send + Sync {
    /// 获取子系统 ID
    fn subsys_id(&self) -> CgroupSubsysId;

    /// 获取所属 cgroup 节点；节点已被 rmdir 拆除时返回 `None`。
    ///
    /// fail-closed：CSS 可能被离组引用（pending OOM、页面归属记录）
    /// 保留得比它的 cgroup 节点久，此时旧的 `upgrade().expect(...)`
    /// 会在缺页等不可失败路径上 panic。实现必须直接 upgrade 自身的
    /// 弱引用并返回 `Option`；调用方对 `None` 只能丢弃请求或返回
    /// 错误，绝不得 panic。
    fn cgroup_node(&self) -> Option<Arc<CgroupNode>>;

    /// 获取父状态（如果不是根）
    fn parent(&self) -> Option<Arc<dyn CgroupSubsysState>>;

    /// 获取状态标志
    fn flags(&self) -> CssFlags;

    /// 设置状态标志
    fn set_flags(&self, flags: CssFlags);

    /// 在线回调（cgroup 创建后调用）
    fn css_online(&self) -> Result<(), SystemError> {
        Ok(())
    }

    /// 离线回调（cgroup 删除前调用）
    fn css_offline(&self) -> Result<(), SystemError> {
        Ok(())
    }

    /// 释放回调（引用计数归零后调用）
    fn css_released(&self) {}

    /// 重置回调（控制器被禁用时调用）
    fn css_reset(&self) {}

    /// fork 前检查（返回 Err 拒绝 fork）
    fn can_fork(&self, _task: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        Ok(())
    }

    /// fork 取消回调
    fn cancel_fork(&self, _task: &Arc<ProcessControlBlock>) {}

    /// fork 成功回调
    fn fork(&self, _task: &Arc<ProcessControlBlock>) {}

    /// exit 回调
    fn exit(&self, _task: &Arc<ProcessControlBlock>) {}

    /// 任务迁移检查（可失败，对应 Linux `cgroup_subsys->can_attach`）。
    ///
    /// 迁移事务的预演阶段：控制器可为 taskset 整组预取迁移效果（如 pids
    /// 预搬层级计数）。整组要么全迁、要么全不动——本钩子对组内任一任务
    /// 失败时，已施加给前序任务的效果必须在这里就地回退（对照 Linux
    /// `pids_try_charge` 的 revert 循环）；事务核心只对已完整执行过的
    /// can_attach 逐个调用 cancel_attach。
    fn can_attach(&self, _tasks: &[Arc<ProcessControlBlock>]) -> Result<(), SystemError> {
        Ok(())
    }

    /// 任务迁移取消回调（对应 Linux `cgroup_subsys->cancel_attach`）。
    ///
    /// 仅在某个控制器的 can_attach 失败后，由迁移事务对已完整执行过
    /// can_attach 的前序控制器逐个调用，回退其预演的效果。
    fn cancel_attach(&self, _tasks: &[Arc<ProcessControlBlock>]) {}

    /// 任务迁移完成回调（不可失败，对应 Linux `cgroup_subsys->attach`）。
    ///
    /// 仅在迁移事务提交（任务归属已切换）之后调用。
    fn attach(&self, _tasks: &[Arc<ProcessControlBlock>]) {}

    /// 用于类型转换的 Any trait
    fn as_any(&self) -> &dyn core::any::Any;
}

/// Cgroup 子系统（控制器）定义
///
/// 对应 Linux 的 `struct cgroup_subsys`
pub trait CgroupSubsys: Debug + Send + Sync {
    /// 获取子系统 ID
    fn id(&self) -> CgroupSubsysId;

    /// 获取子系统名称
    fn name(&self) -> &'static str {
        self.id().name()
    }

    /// 是否为隐式控制器（不在 cgroup.controllers 中显示，自动启用）
    fn implicit_on_dfl(&self) -> bool {
        false
    }

    /// 是否支持线程模式
    fn threaded(&self) -> bool {
        false
    }

    /// 分配新的 css 状态（cgroup 创建时调用）
    fn css_alloc(
        &self,
        parent: Option<&Arc<dyn CgroupSubsysState>>,
        cgroup: &Arc<CgroupNode>,
    ) -> Result<Arc<dyn CgroupSubsysState>, SystemError>;

    /// 释放 css 状态（cgroup 销毁时调用）
    fn css_free(&self, css: &Arc<dyn CgroupSubsysState>);

    /// 获取默认的 cgroup 文件定义
    fn dfl_cftypes(&self) -> Vec<CfType> {
        Vec::new()
    }
}

/// Cgroup 文件类型标志
#[derive(Debug, Clone, Copy)]
pub struct CfTypeFlags(u32);

impl CfTypeFlags {
    pub const ONLY_ON_ROOT: u32 = 1 << 0; // 只在根 cgroup 创建
    pub const NOT_ON_ROOT: u32 = 1 << 1; // 不在根 cgroup 创建
    pub const NS_DELEGATABLE: u32 = 1 << 2; // 命名空间可委托写入

    pub fn new() -> Self {
        Self(0)
    }

    pub fn contains(&self, flag: u32) -> bool {
        self.0 & flag != 0
    }
}

impl Default for CfTypeFlags {
    fn default() -> Self {
        Self::new()
    }
}

/// Cgroup 文件定义
///
/// 对应 Linux 的 `struct cftype`
#[derive(Debug, Clone)]
pub struct CfType {
    pub name: String,
    pub flags: CfTypeFlags,
    pub max_write_len: usize,
    pub read: Option<fn(&Arc<dyn CgroupSubsysState>) -> Result<String, SystemError>>,
    pub write: Option<fn(&Arc<dyn CgroupSubsysState>, &str) -> Result<(), SystemError>>,
}

impl CfType {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            flags: CfTypeFlags::new(),
            max_write_len: 4096,
            read: None,
            write: None,
        }
    }

    pub fn with_read(
        mut self,
        read: fn(&Arc<dyn CgroupSubsysState>) -> Result<String, SystemError>,
    ) -> Self {
        self.read = Some(read);
        self
    }

    pub fn with_write(
        mut self,
        write: fn(&Arc<dyn CgroupSubsysState>, &str) -> Result<(), SystemError>,
    ) -> Self {
        self.write = Some(write);
        self
    }

    pub fn with_flags(mut self, flags: CfTypeFlags) -> Self {
        self.flags = flags;
        self
    }
}

lazy_static! {
    static ref SUBSYS_REGISTRY: RwLock<
        [Option<Arc<dyn CgroupSubsys>>; CgroupSubsysId::COUNT],
    > = RwLock::new([None, None, None, None, None, None, None, None, None]);
}


/// 注册子系统
pub fn register_subsys(subsys: Arc<dyn CgroupSubsys>) {
    let id = subsys.id() as usize;
    let mut registry = SUBSYS_REGISTRY.write();
    if registry[id].is_some() {
        panic!("cgroup subsys {} already registered", subsys.name());
    }
    registry[id] = Some(subsys);
}

/// 获取已注册的子系统
pub fn get_subsys(id: CgroupSubsysId) -> Option<Arc<dyn CgroupSubsys>> {
    SUBSYS_REGISTRY.read()[id as usize].clone()
}

/// 获取所有已注册的子系统
pub fn all_subsys() -> Vec<Arc<dyn CgroupSubsys>> {
    SUBSYS_REGISTRY
        .read()
        .iter()
        .filter_map(|s| s.clone())
        .collect()
}
