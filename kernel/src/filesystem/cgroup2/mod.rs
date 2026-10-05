use alloc::sync::Arc;

use linkme::distributed_slice;
use system_error::SystemError;

use crate::{
    cgroup::{cgroup_root, CgroupNode},
    filesystem::{
        sysfs::sysfs_instance,
        vfs::{mount::MountFlags, FileSystem, FileSystemMakerData, MountableFileSystem, FSMAKER},
    },
    libs::once::Once,
    process::ProcessManager,
    register_mountable_fs,
};

use self::{inode::Cgroup2Inode, mount::Cgroup2Fs};

mod files;
mod inode;
mod mount;

pub(super) const CGROUP2_MAX_NAMELEN: usize = 255;
pub(super) const CGROUP2_BLOCK_SIZE: u64 = 512;
/// cgroup.controllers 列出的可启用控制器。
///
/// 不含 freezer：Linux 6.6 的 v2 freezer 是 cgroup 核心功能而非
/// cgroup_subsys（kernel/cgroup/freezer.c，cgroup.freeze/cgroup.events 为
/// 基础文件）；legacy v1 的 freezer_cgrp_subsys（legacy_freezer.c）没有
/// dfl_cftypes，被 cgroup_init() 计入 cgrp_dfl_inhibit_ss_mask，由
/// cgroup_control() 从默认层级根的列表中过滤掉，故任何组都不会列出。
pub(super) const AVAILABLE_CONTROLLERS: [&str; 5] =
    ["cpu", "memory", "pids", "cpuset", "io"];
pub(super) const DOMAIN_CONTROLLERS: [&str; 2] = ["memory", "io"];

pub fn cgroup2_init() -> Result<(), SystemError> {
    static INIT: Once = Once::new();
    let mut result = None;
    INIT.call_once(|| {
        result = Some((|| -> Result<(), SystemError> {
            sysfs_instance().ensure_mount_point_path(
                &["fs", "cgroup"],
                crate::filesystem::vfs::InodeMode::from_bits_truncate(0o755),
            )?;

            let root_inode = ProcessManager::current_mntns().root_inode();
            let sys = root_inode.find("sys")?;
            let fs_dir = sys.find("fs")?;
            let cgroup_dir = fs_dir.find("cgroup")?;

            let cgroup_fs = Cgroup2Fs::new(cgroup_root().root(), false);
            cgroup_dir.mount(cgroup_fs, MountFlags::empty())?;

            ::log::info!("Cgroup2 mounted at /sys/fs/cgroup");
            Ok(())
        })());
    });
    result.unwrap_or(Ok(()))
}

pub fn cgroup2_check_attach_permissions(
    fs_root: Arc<dyn crate::filesystem::vfs::IndexNode>,
    src_cgroup: &Arc<CgroupNode>,
    dst_cgroup: &Arc<CgroupNode>,
) -> Result<(), SystemError> {
    Cgroup2Inode::check_attach_permissions(fs_root, src_cgroup, dst_cgroup)
}

pub fn cgroup2_inode_to_node(
    inode: &Arc<dyn crate::filesystem::vfs::IndexNode>,
) -> Result<Arc<CgroupNode>, SystemError> {
    let cgroup_inode = inode
        .as_any_ref()
        .downcast_ref::<Cgroup2Inode>()
        .ok_or(SystemError::EINVAL)?;
    cgroup_inode.cgroup().ok_or(SystemError::ENOTDIR)
}

register_mountable_fs!(Cgroup2Fs, CGROUP2FSMAKER, "cgroup2");
