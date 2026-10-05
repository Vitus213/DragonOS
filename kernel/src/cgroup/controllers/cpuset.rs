use alloc::{format, string::{String, ToString}, sync::{Arc, Weak}, vec::Vec};
use core::any::Any;
use system_error::SystemError;

use crate::{
    cgroup::{core::CgroupNode, subsys::{CfType, CgroupSubsys, CgroupSubsysId, CgroupSubsysState, CssFlags}},
    libs::{cpumask::CpuMask, spinlock::SpinLock},
    mm::percpu::PerCpu,
    process::{ProcessControlBlock, ProcessManager},
    smp::cpu::{smp_cpu_manager, ProcessorId},
};

#[derive(Debug)]
pub struct CpusetCss {
    cgroup: Weak<CgroupNode>,
    flags: SpinLock<CssFlags>,
    configured_cpus: SpinLock<Option<CpuMask>>,
    configured_mems: SpinLock<String>,
}

impl CpusetCss {
    pub fn new(cgroup: Weak<CgroupNode>) -> Arc<Self> {
        Arc::new(Self {
            cgroup,
            flags: SpinLock::new(CssFlags::default()),
            configured_cpus: SpinLock::new(None),
            configured_mems: SpinLock::new(String::new()),
        })
    }

    pub fn effective_cpus(&self) -> CpuMask {
        let own = self.configured_cpus.lock().clone();
        let inherited = self
            .cgroup
            .upgrade()
            .and_then(|node| node.parent())
            .and_then(|parent| parent.css(CgroupSubsysId::Cpuset))
            .and_then(|css| {
                css.as_any()
                    .downcast_ref::<CpusetCss>()
                    .map(|parent| parent.effective_cpus())
            });
        let mut effective = match (own, inherited) {
            (Some(mut own), Some(parent)) => {
                own.bitand_assign(&parent);
                own
            }
            (Some(own), None) => own,
            (None, Some(parent)) => parent,
            (None, None) => smp_cpu_manager().possible_cpus().clone(),
        };
        // Linux's effective cpuset follows CPU hotplug state; configured
        // cpuset.cpus remains readable with offline CPUs.
        effective.bitand_assign(&smp_cpu_manager().online_cpus());
        effective
    }

    pub fn configured_cpus_string(&self) -> String {
        self.configured_cpus
            .lock()
            .as_ref()
            .map(encode_cpumask)
            .unwrap_or_default()
    }

    pub fn cpus_string(&self) -> String {
        encode_cpumask(&self.effective_cpus())
    }

    pub fn mems_string(&self) -> String {
        self.configured_mems.lock().clone()
    }

    pub fn set_cpus(&self, value: &str) -> Result<(), SystemError> {
        let value = value.trim();
        let mask = if value.is_empty() {
            None
        } else {
            let mask = parse_cpumask(value)?;
            if mask.is_empty() {
                return Err(SystemError::EINVAL);
            }
            let mut possible = mask.clone();
            possible.bitand_assign(&smp_cpu_manager().possible_cpus());
            if possible.iter_cpu().count() != mask.iter_cpu().count() {
                return Err(SystemError::EINVAL);
            }
            Some(mask)
        };
        if let Some(mask) = &mask {
            let parent_effective = self
                .cgroup
                .upgrade()
                .and_then(|cgroup| cgroup.parent())
                .and_then(|parent| parent.css(CgroupSubsysId::Cpuset))
                .and_then(|css| {
                    css.as_any()
                        .downcast_ref::<CpusetCss>()
                        .map(|parent| parent.effective_cpus())
                });
            if let Some(parent_effective) = parent_effective {
                let mut effective = mask.clone();
                effective.bitand_assign(&parent_effective);
                if effective.is_empty() {
                    return Err(SystemError::EINVAL);
                }
            }
        }
        // DragonOS currently stores only one task affinity mask, rather than
        // Linux's separate user_cpus_ptr and effective cpus_mask. Refuse a
        // policy update that would make any existing task unrunnable instead
        // of widening its explicit request as a fallback.
        let old = core::mem::replace(&mut *self.configured_cpus.lock(), mask);
        if let Err(error) = self.validate_tasks_in_subtree() {
            *self.configured_cpus.lock() = old;
            return Err(error);
        }
        if let Err(error) = self.apply_to_tasks() {
            *self.configured_cpus.lock() = old;
            let _ = self.apply_to_tasks();
            return Err(error);
        }
        Ok(())
    }
    // DragonOS has no NUMA-node allocator yet, so retain the validated Linux
    // node-list spelling for file compatibility without applying placement.
    pub fn set_mems(&self, value: &str) -> Result<(), SystemError> {
        let value = value.trim();
        if !value.is_empty() {
            for part in value.split(',') {
                let mut bounds = part.split('-');
                let start: u32 = bounds.next().ok_or(SystemError::EINVAL)?
                    .parse().map_err(|_| SystemError::EINVAL)?;
                let end: u32 = match bounds.next() {
                    Some(raw) if !raw.is_empty() => raw.parse().map_err(|_| SystemError::EINVAL)?,
                    Some(_) => return Err(SystemError::EINVAL),
                    None => start,
                };
                if bounds.next().is_some() || end < start {
                    return Err(SystemError::EINVAL);
                }
            }
        }
        *self.configured_mems.lock() = value.to_string();
        Ok(())
    }

    fn validate_tasks_in_subtree(&self) -> Result<(), SystemError> {
        let Some(cgroup) = self.cgroup.upgrade() else {
            return Ok(());
        };
        let mut pending = vec![cgroup];
        while let Some(node) = pending.pop() {
            let Some(css) = node.css(CgroupSubsysId::Cpuset) else {
                continue;
            };
            let Some(cpuset) = css.as_any().downcast_ref::<CpusetCss>() else {
                continue;
            };
            let mask = cpuset.effective_cpus();
            // Linux permits an empty effective cpuset while it has no tasks;
            // attachment itself rejects such a destination.
            let task_ids = node.tasks();
            if mask.is_empty() && !task_ids.is_empty() {
                return Err(SystemError::EINVAL);
            }
            for pid in task_ids {
                if let Some(task) = ProcessManager::find(pid) {
                    let mut task_mask = task.sched_info().cpus_allowed();
                    task_mask.bitand_assign(&mask);
                    if task_mask.is_empty() {
                        return Err(SystemError::EINVAL);
                    }
                }
            }
            pending.extend(node.children());
        }
        Ok(())
    }

    fn apply_to_tasks(&self) -> Result<(), SystemError> {
        let Some(cgroup) = self.cgroup.upgrade() else {
            return Ok(());
        };
        let mut pending = vec![cgroup];
        while let Some(node) = pending.pop() {
            let Some(css) = node.css(CgroupSubsysId::Cpuset) else {
                continue;
            };
            let Some(cpuset) = css.as_any().downcast_ref::<CpusetCss>() else {
                continue;
            };
            let mask = cpuset.effective_cpus();
            for pid in node.tasks() {
                if let Some(task) = ProcessManager::find(pid) {
                    let mut task_mask = task.sched_info().cpus_allowed();
                    task_mask.bitand_assign(&mask);
                    if !task_mask.is_empty() {
                        ProcessManager::set_cpus_allowed(&task, task_mask)?;
                    }
                }
            }
            pending.extend(node.children());
        }
        Ok(())
    }
}

impl CgroupSubsysState for CpusetCss {
    fn subsys_id(&self) -> CgroupSubsysId { CgroupSubsysId::Cpuset }
    fn cgroup(&self) -> Arc<CgroupNode> { self.cgroup.upgrade().expect("cpuset cgroup dropped") }
    fn parent(&self) -> Option<Arc<dyn CgroupSubsysState>> {
        self.cgroup
            .upgrade()
            .and_then(|cgroup| cgroup.parent())
            .and_then(|parent| parent.css(CgroupSubsysId::Cpuset))
    }
    fn flags(&self) -> CssFlags { *self.flags.lock() }
    fn set_flags(&self, flags: CssFlags) { *self.flags.lock() = flags; }
    fn can_attach(&self, tasks: &[Arc<ProcessControlBlock>]) -> Result<(), SystemError> {
        let effective = self.effective_cpus();
        if effective.is_empty() {
            return Err(SystemError::EINVAL);
        }
        for task in tasks {
            let mut allowed = task.sched_info().cpus_allowed();
            allowed.bitand_assign(&effective);
            if allowed.is_empty() {
                return Err(SystemError::EXDEV);
            }
        }
        Ok(())
    }

    fn fork(&self, task: &Arc<ProcessControlBlock>) {
        let mut task_mask = task.sched_info().cpus_allowed();
        task_mask.bitand_assign(&self.effective_cpus());
        if !task_mask.is_empty() {
            let _ = ProcessManager::set_cpus_allowed(task, task_mask);
        }
    }
    fn attach(&self, tasks: &[Arc<ProcessControlBlock>]) {
        let mask = self.effective_cpus();
        for task in tasks {
            let mut task_mask = task.sched_info().cpus_allowed();
            task_mask.bitand_assign(&mask);
            // can_attach rejects empty intersections. Avoid silently replacing
            // a user's affinity if policy changed between validation and commit.
            if !task_mask.is_empty() {
                let _ = ProcessManager::set_cpus_allowed(task, task_mask);
            }
        }
    }
    fn as_any(&self) -> &dyn Any { self }
}

#[derive(Debug)]
pub struct CpusetController;
impl CgroupSubsys for CpusetController {
    fn id(&self) -> CgroupSubsysId { CgroupSubsysId::Cpuset }
    fn name(&self) -> &'static str { "cpuset" }
    fn css_alloc(
        &self,
        _parent: Option<&Arc<dyn CgroupSubsysState>>,
        cgroup: &Arc<CgroupNode>,
    ) -> Result<Arc<dyn CgroupSubsysState>, SystemError> {
        Ok(CpusetCss::new(Arc::downgrade(cgroup)))
    }
    fn css_free(&self, _css: &Arc<dyn CgroupSubsysState>) {}
    fn dfl_cftypes(&self) -> Vec<CfType> {
        vec![
            CfType::new("cpuset.cpus").with_read(cpuset_cpus_read).with_write(cpuset_cpus_write),
            CfType::new("cpuset.cpus.effective").with_read(cpuset_cpus_effective_read),
            CfType::new("cpuset.mems").with_read(cpuset_mems_read).with_write(cpuset_mems_write),
        ]
    }
}

fn cpuset(css: &Arc<dyn CgroupSubsysState>) -> Result<&CpusetCss, SystemError> { css.as_any().downcast_ref().ok_or(SystemError::EINVAL) }
fn cpuset_cpus_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    Ok(format!("{}\n", cpuset(css)?.configured_cpus_string()))
}
fn cpuset_cpus_effective_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    Ok(format!("{}\n", cpuset(css)?.cpus_string()))
}
fn cpuset_cpus_write(css: &Arc<dyn CgroupSubsysState>, value: &str) -> Result<(), SystemError> { cpuset(css)?.set_cpus(value) }
fn cpuset_mems_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> { Ok(format!("{}\n", cpuset(css)?.mems_string())) }
fn cpuset_mems_write(css: &Arc<dyn CgroupSubsysState>, value: &str) -> Result<(), SystemError> { cpuset(css)?.set_mems(value) }

fn parse_cpumask(value: &str) -> Result<CpuMask, SystemError> {
    let mut mask = CpuMask::new();
    for part in value.trim().split(',') {
        if part.is_empty() {
            return Err(SystemError::EINVAL);
        }
        let mut bounds = part.split('-');
        let start: u32 = bounds
            .next()
            .ok_or(SystemError::EINVAL)?
            .parse()
            .map_err(|_| SystemError::EINVAL)?;
        let end: u32 = match bounds.next() {
            Some(raw) if !raw.is_empty() => raw.parse().map_err(|_| SystemError::EINVAL)?,
            Some(_) => return Err(SystemError::EINVAL),
            None => start,
        };
        if bounds.next().is_some() || end < start || end >= PerCpu::MAX_CPU_NUM {
            return Err(SystemError::EINVAL);
        }
        for cpu in start..=end {
            mask.set(ProcessorId::new(cpu), true);
        }
    }
    Ok(mask)
}

fn encode_cpumask(mask: &CpuMask) -> String {
    let cpus: Vec<u32> = mask.iter_cpu().map(|cpu| cpu.data()).collect();
    if cpus.is_empty() { return String::new(); }
    let mut out = String::new(); let mut i = 0;
    while i < cpus.len() { let start = cpus[i]; let mut end = start; while i + 1 < cpus.len() && cpus[i + 1] == end + 1 { i += 1; end = cpus[i]; } if !out.is_empty() { out.push(','); } if start == end { out.push_str(&format!("{}", start)); } else { out.push_str(&format!("{}-{}", start, end)); } i += 1; }
    out
}

pub fn init_cpuset_controller() { crate::cgroup::subsys::register_subsys(Arc::new(CpusetController)); }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpumask_parser_and_encoder_preserve_ranges() {
        let mask = parse_cpumask("0-2,4,6-7").unwrap();
        assert_eq!(encode_cpumask(&mask), "0-2,4,6-7");
    }

    #[test]
    fn cpumask_parser_rejects_invalid_ranges() {
        assert_eq!(parse_cpumask("3-1"), Err(SystemError::EINVAL));
        assert_eq!(parse_cpumask("0-1-2"), Err(SystemError::EINVAL));
        assert_eq!(parse_cpumask("1-"), Err(SystemError::EINVAL));
    }
}
