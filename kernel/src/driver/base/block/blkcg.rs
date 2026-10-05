//! Block-cgroup pre-dispatch enforcement and completion accounting hooks.
//!
//! Every path that dispatches or submits block I/O must route through
//! [`throttle_current_io`] (or [`throttle_io`] with an explicit cgroup)
//! *before* handing the request to the device, mirroring how Linux calls
//! `blk_throtl_bio()` from `submit_bio()` for all block I/O. Completions are
//! accounted through [`account_current_io`] or, for asynchronous requests
//! whose waiter runs in another task context, through [`current_io_cgroup`]
//! captured at submit time plus [`account_io_for`].

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    cgroup::{
        controllers::io::{
            account_io, any_io_limits_configured, throttle_io, IoDeviceKey,
        },
        CgroupNode,
    },
    driver::base::device::device_number::DeviceNumber,
};

fn io_device_key(device: DeviceNumber) -> IoDeviceKey {
    IoDeviceKey::new(device.major().data(), device.minor())
}

/// Enforce the current task's io.max limits before a request reaches hardware.
///
/// While no io.max limit exists anywhere in the system this is a single
/// relaxed atomic load, so devices without an io.max configuration run at
/// zero overhead.
pub fn throttle_current_io(
    device: DeviceNumber,
    write: bool,
    bytes: usize,
) -> Result<(), SystemError> {
    if !any_io_limits_configured() {
        return Ok(());
    }
    let cgroup: Arc<CgroupNode> = crate::process::ProcessManager::current_pcb().task_cgroup_node();
    throttle_io(&cgroup, io_device_key(device), write, bytes)
}

/// Account a completed block operation to the current task's io cgroup.
pub fn account_current_io(device: DeviceNumber, write: bool, bytes: usize) {
    let cgroup: Arc<CgroupNode> = crate::process::ProcessManager::current_pcb().task_cgroup_node();
    account_io(&cgroup, io_device_key(device), write, bytes);
}

/// Capture the submitting task's io cgroup for completion-time accounting.
///
/// Asynchronous requests are often waited for by a different task (for
/// example a workqueue worker); charging that waiter's cgroup would
/// misattribute the I/O, so the cgroup is captured where the request is
/// submitted - the analogue of Linux's `bio_associate_blkg()`.
pub fn current_io_cgroup() -> Arc<CgroupNode> {
    crate::process::ProcessManager::current_pcb().task_cgroup_node()
}

/// Account a completed block operation against a cgroup captured at submit
/// time by [`current_io_cgroup`].
pub fn account_io_for(cgroup: &Arc<CgroupNode>, device: DeviceNumber, write: bool, bytes: usize) {
    account_io(cgroup, io_device_key(device), write, bytes);
}
