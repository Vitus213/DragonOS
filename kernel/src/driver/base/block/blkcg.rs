//! Block-cgroup pre-dispatch enforcement and completion accounting hooks.
//!
//! Every path that dispatches or submits block I/O must route through
//! [`throttle_current_io`] (or [`throttle_io`] with an explicit cgroup)
//! *before* handing the request to the device, mirroring how Linux calls
//! `blk_throtl_bio()` from `submit_bio()` for all block I/O. Completions are
//! accounted through [`account_current_io`] or, for asynchronous requests
//! whose waiter runs in another task context, through [`current_io_cgroup`]
//! captured at submit time plus [`account_io_for`].
//!
//! "Current" is deliberately not "whatever task is running":
//! [`set_io_owner`] installs a task-level ownership override, the counterpart
//! of Linux's `kthread_associate_blkcg()`. Kernel-side dispatchers (page cache
//! writeback workers, readahead workers, reclaimers) install it around the
//! requests whose real owner was captured when the data was dirtied, so io.max
//! enforcement and io.stat attribution follow the owner instead of the worker.

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    cgroup::{
        controllers::io::{account_io, any_io_limits_configured, throttle_io, IoDeviceKey},
        CgroupNode,
    },
    driver::base::device::device_number::DeviceNumber,
};

fn io_device_key(device: DeviceNumber) -> IoDeviceKey {
    IoDeviceKey::new(device.major().data(), device.minor())
}

/// Resolve the cgroup the current task's block I/O is attributed to.
///
/// Returns the [`set_io_owner`] override when one is installed, otherwise the
/// executing task's own cgroup. Every enforcement and accounting helper below
/// goes through this function, so a single override covers throttling,
/// completion accounting and submit-time capture alike.
pub fn current_io_cgroup() -> Arc<CgroupNode> {
    let pcb = crate::process::ProcessManager::current_pcb();
    match pcb.blkcg_owner_override() {
        Some(owner) => owner,
        None => pcb.task_cgroup_node(),
    }
}

/// Task-level block I/O ownership override guard.
///
/// Dropping restores the previously installed override (or clears it), so
/// guards nest and an early return or unwinding path can never leak an owner
/// onto a long-lived worker.
#[derive(Debug)]
pub struct IoOwnerGuard {
    previous: Option<Arc<CgroupNode>>,
}

impl Drop for IoOwnerGuard {
    fn drop(&mut self) {
        crate::process::ProcessManager::current_pcb()
            .set_blkcg_owner_override(self.previous.take());
    }
}

/// Attribute the current task's block I/O to `owner` until the returned guard
/// is dropped.
///
/// Mirrors `kthread_associate_blkcg()`: the dispatching task keeps running in
/// its own scheduling and cgroup membership context; only the blkcg identity
/// used by io.max enforcement and io.stat accounting changes.
pub fn set_io_owner(owner: Arc<CgroupNode>) -> IoOwnerGuard {
    IoOwnerGuard {
        previous: crate::process::ProcessManager::current_pcb()
            .set_blkcg_owner_override(Some(owner)),
    }
}

/// Install an owner override only when `owner` is not already the resolved
/// owner.
///
/// Foreground dispatch - a task writing its own I/O - therefore costs one
/// pointer compare and no task-slot write, while a worker dispatching someone
/// else's I/O gets the override. `None` means nothing was installed and no
/// guard is needed.
pub fn try_set_io_owner(owner: &Arc<CgroupNode>) -> Option<IoOwnerGuard> {
    if Arc::ptr_eq(&current_io_cgroup(), owner) {
        return None;
    }
    Some(set_io_owner(owner.clone()))
}

/// Enforce the attributed cgroup's io.max limits before a request reaches hardware.
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
    let cgroup: Arc<CgroupNode> = current_io_cgroup();
    throttle_io(&cgroup, io_device_key(device), write, bytes)
}

/// Account a completed block operation to the attributed cgroup.
pub fn account_current_io(device: DeviceNumber, write: bool, bytes: usize) {
    let cgroup: Arc<CgroupNode> = current_io_cgroup();
    account_io(&cgroup, io_device_key(device), write, bytes);
}

/// Account a completed block operation against a cgroup captured at submit
/// time by [`current_io_cgroup`].
pub fn account_io_for(cgroup: &Arc<CgroupNode>, device: DeviceNumber, write: bool, bytes: usize) {
    account_io(cgroup, io_device_key(device), write, bytes);
}

/// Throttle an explicit cgroup (and its ancestors) before dispatch.
///
/// For callers which already hold the owner identity and do not run under a
/// [`set_io_owner`] override; device numbers are converted with the same key
/// rule as every other enforcement point.
pub fn throttle_for(
    cgroup: &Arc<CgroupNode>,
    device: DeviceNumber,
    write: bool,
    bytes: usize,
) -> Result<(), SystemError> {
    if !any_io_limits_configured() {
        return Ok(());
    }
    throttle_io(cgroup, io_device_key(device), write, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::base::device::device_number::Major;

    /// The device key rule is shared by every enforcement and accounting
    /// point; a partition device must never key limits under its parent disk.
    #[test]
    fn io_device_key_uses_major_minor_of_the_dispatched_device() {
        let disk = DeviceNumber::new(Major::new(254), 0);
        let part = DeviceNumber::new(Major::new(254), 1);
        assert_eq!(io_device_key(disk), IoDeviceKey::new(254, 0));
        assert_eq!(io_device_key(part), IoDeviceKey::new(254, 1));
        assert_ne!(io_device_key(disk), io_device_key(part));
    }
}
