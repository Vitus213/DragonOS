//! Cgroup 控制器初始化
//!
//! 在 cgroup2_init() 之前注册所有控制器

use alloc::sync::Arc;

use crate::cgroup::{
    controllers::{
        cpu::CpuController,
        cpuset::CpusetController,
        freezer::FreezerController,
        io::IoController,
        memory::MemoryController,
        pids::PidsSubsys,
    },
    subsys::register_subsys,
};

/// 初始化所有 cgroup 控制器
///
/// 必须在 cgroup2_init() 之前调用
pub fn init_cgroup_controllers() {
    // 注册 CPU 控制器
    let cpu_controller = Arc::new(CpuController);
    register_subsys(cpu_controller);

    // 注册 Pids 控制器
    let pids_controller = Arc::new(PidsSubsys);
    register_subsys(pids_controller);
    // 注册 Memory 控制器
    let memory_controller = Arc::new(MemoryController);
    register_subsys(memory_controller);

    // 注册 Freezer 控制器
    let freezer_controller = Arc::new(FreezerController);
    register_subsys(freezer_controller);
    // 注册 Cpuset 控制器
    let cpuset_controller = Arc::new(CpusetController);
    register_subsys(cpuset_controller);
    // 注册 IO 控制器
    let io_controller = Arc::new(IoController);
    register_subsys(io_controller);
    log::info!("Cgroup controllers registered: cpu, memory, pids, freezer, cpuset, io");
}
