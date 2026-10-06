use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::Ordering;

use hashbrown::{HashMap, HashSet};
use system_error::SystemError;

use crate::cgroup::CgroupNode;

use super::{AVAILABLE_CONTROLLERS, DOMAIN_CONTROLLERS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CgroupCoreFile {
    Procs,
    Controllers,
    SubtreeControl,
    Events,
    Type,
    Freeze,
    CpuStat,
    CpuWeight,
    CpuMax,
    MemoryCurrent,
    MemoryPeak,
    MemoryMin,
    MemoryLow,
    MemoryHigh,
    MemoryMax,
    MemoryEvents,
    MemoryStat,
    MemorySwapCurrent,
    MemorySwapPeak,
    MemorySwapHigh,
    MemorySwapMax,
    MemorySwapEvents,
    PidsCurrent,
    PidsMax,
    PidsEvents,
    CpusetCpus,
    CpusetCpusEffective,
    CpusetMems,
    IoMax,
    IoWeight,
    IoStat,
}

#[derive(Clone, Copy)]
pub(super) struct CgroupFileSpec {
    pub(super) name: &'static str,
    pub(super) ty: CgroupCoreFile,
    pub(super) init: &'static [u8],
    pub(super) mode: u16,
    visibility: CgroupFileVisibility,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CgroupFileVisibility {
    All,
    NotOnRoot,
}

impl CgroupFileSpec {
    fn visible_on(self, cgroup: &Arc<CgroupNode>) -> bool {
        match self.visibility {
            CgroupFileVisibility::All => true,
            CgroupFileVisibility::NotOnRoot => cgroup.parent().is_some(),
        }
    }
}

const BASE_FILE_SPECS: [CgroupFileSpec; 4] = [
    CgroupFileSpec {
        name: "cgroup.procs",
        ty: CgroupCoreFile::Procs,
        init: b"",
        mode: 0o644,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "cgroup.controllers",
        ty: CgroupCoreFile::Controllers,
        init: b"\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "cgroup.subtree_control",
        ty: CgroupCoreFile::SubtreeControl,
        init: b"\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "cpu.stat",
        ty: CgroupCoreFile::CpuStat,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::All,
    },
];

const NON_ROOT_CORE_FILE_SPECS: [CgroupFileSpec; 3] = [
    CgroupFileSpec {
        name: "cgroup.events",
        ty: CgroupCoreFile::Events,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "cgroup.type",
        ty: CgroupCoreFile::Type,
        init: b"domain\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "cgroup.freeze",
        ty: CgroupCoreFile::Freeze,
        init: b"0\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
];

const CPU_FILE_SPECS: [CgroupFileSpec; 2] = [
    CgroupFileSpec {
        name: "cpu.weight",
        ty: CgroupCoreFile::CpuWeight,
        init: b"100\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "cpu.max",
        ty: CgroupCoreFile::CpuMax,
        init: b"max 100000\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
];

const MEMORY_FILE_SPECS: [CgroupFileSpec; 13] = [
    CgroupFileSpec {
        name: "memory.current",
        ty: CgroupCoreFile::MemoryCurrent,
        init: b"0\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.peak",
        ty: CgroupCoreFile::MemoryPeak,
        init: b"0\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.min",
        ty: CgroupCoreFile::MemoryMin,
        init: b"0\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.low",
        ty: CgroupCoreFile::MemoryLow,
        init: b"0\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.high",
        ty: CgroupCoreFile::MemoryHigh,
        init: b"max\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.max",
        ty: CgroupCoreFile::MemoryMax,
        init: b"max\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.events",
        ty: CgroupCoreFile::MemoryEvents,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.stat",
        ty: CgroupCoreFile::MemoryStat,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "memory.swap.current",
        ty: CgroupCoreFile::MemorySwapCurrent,
        init: b"0\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.swap.peak",
        ty: CgroupCoreFile::MemorySwapPeak,
        init: b"0\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.swap.high",
        ty: CgroupCoreFile::MemorySwapHigh,
        init: b"max\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.swap.max",
        ty: CgroupCoreFile::MemorySwapMax,
        init: b"max\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "memory.swap.events",
        ty: CgroupCoreFile::MemorySwapEvents,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
];

const PIDS_FILE_SPECS: [CgroupFileSpec; 3] = [
    CgroupFileSpec {
        name: "pids.current",
        ty: CgroupCoreFile::PidsCurrent,
        init: b"0\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "pids.max",
        ty: CgroupCoreFile::PidsMax,
        init: b"max\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "pids.events",
        ty: CgroupCoreFile::PidsEvents,
        init: b"max 0\n",
        mode: 0o444,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
];
const CPUSET_FILE_SPECS: [CgroupFileSpec; 3] = [
    CgroupFileSpec {
        name: "cpuset.cpus",
        ty: CgroupCoreFile::CpusetCpus,
        init: b"",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "cpuset.cpus.effective",
        ty: CgroupCoreFile::CpusetCpusEffective,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "cpuset.mems",
        ty: CgroupCoreFile::CpusetMems,
        init: b"",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
];
const IO_FILE_SPECS: [CgroupFileSpec; 3] = [
    CgroupFileSpec {
        name: "io.max",
        ty: CgroupCoreFile::IoMax,
        init: b"",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "io.weight",
        ty: CgroupCoreFile::IoWeight,
        init: b"default 100\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::NotOnRoot,
    },
    CgroupFileSpec {
        name: "io.stat",
        ty: CgroupCoreFile::IoStat,
        init: b"",
        mode: 0o444,
        visibility: CgroupFileVisibility::All,
    },
];


pub(super) fn desired_file_specs(cgroup: &Arc<CgroupNode>) -> Vec<CgroupFileSpec> {
    let mut specs = Vec::new();
    push_visible_specs(&mut specs, cgroup, &BASE_FILE_SPECS);
    push_visible_specs(&mut specs, cgroup, &NON_ROOT_CORE_FILE_SPECS);
    for controller in available_controllers_for(cgroup) {
        push_visible_specs(&mut specs, cgroup, controller_specs(controller));
    }
    specs
}

pub(super) fn desired_file_names(cgroup: &Arc<CgroupNode>) -> HashSet<&'static str> {
    desired_file_specs(cgroup)
        .into_iter()
        .map(|spec| spec.name)
        .collect()
}

fn push_visible_specs(
    out: &mut Vec<CgroupFileSpec>,
    cgroup: &Arc<CgroupNode>,
    specs: &'static [CgroupFileSpec],
) {
    out.extend(specs.iter().copied().filter(|spec| spec.visible_on(cgroup)));
}

fn controller_specs(name: &str) -> &'static [CgroupFileSpec] {
    match name {
        "cpu" => &CPU_FILE_SPECS,
        "memory" => &MEMORY_FILE_SPECS,
        "pids" => &PIDS_FILE_SPECS,
        "cpuset" => &CPUSET_FILE_SPECS,
        "io" => &IO_FILE_SPECS,
        _ => &[],
    }
}

fn available_controllers_for(cgroup: &Arc<CgroupNode>) -> Vec<&'static str> {
    let Some(parent) = cgroup.parent() else {
        return AVAILABLE_CONTROLLERS.to_vec();
    };
    let parent_enabled: HashSet<String> = parent.subtree_control().into_iter().collect();
    AVAILABLE_CONTROLLERS
        .iter()
        .copied()
        .filter(|name| parent_enabled.contains(*name))
        .collect()
}

fn is_known_controller(name: &str) -> bool {
    AVAILABLE_CONTROLLERS.contains(&name)
}

pub(super) fn read_file(cgroup: &Arc<CgroupNode>, ty: CgroupCoreFile) -> Vec<u8> {
    match ty {
        CgroupCoreFile::Procs => {
            let mut lines = String::new();
            for pid in cgroup.tasks() {
                lines.push_str(&format!("{}\n", pid.data()));
            }
            lines.into_bytes()
        }
        CgroupCoreFile::Controllers => {
            let items: Vec<String> = available_controllers_for(cgroup)
                .into_iter()
                .map(|s| s.to_string())
                .collect();
            encode_controller_list(&items)
        }
        CgroupCoreFile::SubtreeControl => {
            let items = cgroup.subtree_control();
            encode_controller_list(&items)
        }
        CgroupCoreFile::Events => {
            let populated = if is_populated(cgroup) { 1 } else { 0 };
            let frozen = if cgroup.is_frozen() { 1 } else { 0 };
            format!("populated {}\nfrozen {}\n", populated, frozen).into_bytes()
        }
        CgroupCoreFile::Type => format!("{}\n", cgroup.cgroup_type_name()).into_bytes(),
        CgroupCoreFile::Freeze => format!(
            "{}\n",
            if cgroup.self_freeze_requested() { 1 } else { 0 }
        )
        .into_bytes(),
        CgroupCoreFile::CpuStat => cpu_stat_for(cgroup),
        CgroupCoreFile::CpuWeight => cpu_bytes(cgroup, |cpu| {
            format!("{}\n", cpu.shares()).into_bytes()
        }),
        CgroupCoreFile::CpuMax => cpu_bytes(cgroup, |cpu| {
            let (quota, period) = cpu.bandwidth();
            encode_cpu_max(quota, period)
        }),
        CgroupCoreFile::MemoryCurrent => memory_bytes(cgroup, |memory| {
            format!("{}\n", memory.current()).into_bytes()
        }),
        CgroupCoreFile::MemoryPeak => memory_bytes(cgroup, |memory| {
            format!("{}\n", memory.peak()).into_bytes()
        }),
        CgroupCoreFile::MemoryMin => {
            memory_bytes(cgroup, |memory| encode_zero_or_value(memory.min()))
        }
        CgroupCoreFile::MemoryLow => {
            memory_bytes(cgroup, |memory| encode_zero_or_value(memory.low()))
        },
        CgroupCoreFile::MemoryHigh => memory_bytes(cgroup, |memory| encode_max_u64(memory.high())),
        CgroupCoreFile::MemoryMax => memory_bytes(cgroup, |memory| encode_max_u64(memory.max())),
        CgroupCoreFile::MemoryEvents => memory_bytes(cgroup, |memory| memory.events().into_bytes()),
        CgroupCoreFile::MemoryStat => memory_bytes(cgroup, |memory| memory.stat().into_bytes()),
        CgroupCoreFile::MemorySwapCurrent => memory_bytes(cgroup, |memory| {
            format!("{}\n", memory.swap_current()).into_bytes()
        }),
        CgroupCoreFile::MemorySwapPeak => memory_bytes(cgroup, |memory| {
            format!("{}\n", memory.swap_peak()).into_bytes()
        }),
        CgroupCoreFile::MemorySwapHigh => {
            memory_bytes(cgroup, |memory| encode_max_u64(memory.swap_high()))
        }
        CgroupCoreFile::MemorySwapMax => {
            memory_bytes(cgroup, |memory| encode_max_u64(memory.swap_max()))
        }
        CgroupCoreFile::MemorySwapEvents => memory_swap_events(),
        CgroupCoreFile::PidsCurrent => format!("{}\n", cgroup.pids_current_count()).into_bytes(),
        CgroupCoreFile::PidsMax => encode_pids_max(cgroup.pids_max()),
        CgroupCoreFile::PidsEvents => format!("max {}\n", cgroup.pids_events_max()).into_bytes(),
        CgroupCoreFile::CpusetCpus | CgroupCoreFile::CpusetCpusEffective | CgroupCoreFile::CpusetMems => {
            let css = cgroup.css(crate::cgroup::subsys::CgroupSubsysId::Cpuset);
            let Some(css) = css else { return b"\n".to_vec(); };
            let Some(cpuset) = css.as_any().downcast_ref::<crate::cgroup::controllers::cpuset::CpusetCss>() else {
                return b"\n".to_vec();
            };
            match ty {
                CgroupCoreFile::CpusetCpus => {
                    format!("{}\n", cpuset.configured_cpus_string()).into_bytes()
                }
                CgroupCoreFile::CpusetCpusEffective => {
                    format!("{}\n", cpuset.cpus_string()).into_bytes()
                }
                CgroupCoreFile::CpusetMems => format!("{}\n", cpuset.mems_string()).into_bytes(),
                _ => unreachable!(),
            }
        }
        CgroupCoreFile::IoMax | CgroupCoreFile::IoWeight | CgroupCoreFile::IoStat => {
            let Some(css) = cgroup.css(crate::cgroup::subsys::CgroupSubsysId::Io) else {
                return b"\n".to_vec();
            };
            let Some(io) = css
                .as_any()
                .downcast_ref::<crate::cgroup::controllers::io::IoCss>()
            else {
                return b"\n".to_vec();
            };
            match ty {
                CgroupCoreFile::IoMax => io.max_string().into_bytes(),
                CgroupCoreFile::IoWeight => io.weight_string().into_bytes(),
                CgroupCoreFile::IoStat => io.stat_string().into_bytes(),
                _ => unreachable!(),
            }
        }
    }
}
/// 写 cgroup.type 的入口。全程持有 cgroup_accounting_lock：
/// 类型变更的 vet→写入必须与任务迁移（write_procs/fork/exit）、
/// mkdir/rmdir 串行，才能杜绝与并发迁移交错产生的非法层级组合
/// （如 threaded 子树内含域控制器任务）。set_cgroup_type 的
/// "调用者须持锁"不变量在此处满足。
pub(super) fn write_type_file(
    cgroup: &Arc<CgroupNode>,
    input: &str,
) -> Result<Vec<u8>, SystemError> {
    let _accounting_guard = crate::cgroup::core::cgroup_accounting_lock().lock();
    cgroup.set_cgroup_type(input)?;
    Ok(format!("{}\n", cgroup.cgroup_type_name()).into_bytes())
}

pub(super) fn write_controller_file(
    cgroup: &Arc<CgroupNode>,
    ty: CgroupCoreFile,
    input: &str,
) -> Result<Vec<u8>, SystemError> {
    match ty {
        CgroupCoreFile::Freeze => {
            let value = input
                .trim()
                .parse::<u32>()
                .map_err(|_| SystemError::EINVAL)?;
            if value > 1 {
                return Err(SystemError::ERANGE);
            }
            cgroup.set_freeze_requested(value == 1);
            Ok(format!("{}\n", value).into_bytes())
        }
        CgroupCoreFile::CpuWeight => {
            let weight = input
                .trim()
                .parse::<u64>()
                .map_err(|_| SystemError::EINVAL)?;
            if !(1..=10_000).contains(&weight) {
                return Err(SystemError::ERANGE);
            }
            cgroup.set_cpu_weight(weight)?;
            Ok(format!("{}\n", weight).into_bytes())
        }
        CgroupCoreFile::CpuMax => {
            let (_, current_period) = cgroup.cpu_bandwidth();
            let (quota, period) = parse_cpu_max(input, current_period)?;
            cgroup.set_cpu_max(quota, period)?;
            Ok(encode_cpu_max(quota, period))
        }
        CgroupCoreFile::MemoryMin
        | CgroupCoreFile::MemoryLow
        | CgroupCoreFile::MemoryHigh
        | CgroupCoreFile::MemoryMax
        | CgroupCoreFile::MemorySwapHigh
        | CgroupCoreFile::MemorySwapMax => {
            let value = parse_max_u64(input)?;
            match ty {
                CgroupCoreFile::MemoryMin => cgroup.set_memory_min(value)?,
                CgroupCoreFile::MemoryLow => cgroup.set_memory_low(value)?,
                CgroupCoreFile::MemoryHigh => cgroup.set_memory_high(value)?,
                CgroupCoreFile::MemoryMax => cgroup.set_memory_max(value)?,
                CgroupCoreFile::MemorySwapHigh => cgroup.set_memory_swap_high(value)?,
                CgroupCoreFile::MemorySwapMax => cgroup.set_memory_swap_max(value)?,
                _ => unreachable!(),
            }
            Ok(encode_max_u64(value))
        }
        CgroupCoreFile::PidsMax => {
            let new_limit = parse_pids_max(input)?;
            cgroup.set_pids_max(new_limit)?;
            Ok(encode_pids_max(new_limit))
        }
        CgroupCoreFile::IoMax | CgroupCoreFile::IoWeight => {
            let css = cgroup
                .css(crate::cgroup::subsys::CgroupSubsysId::Io)
                .ok_or(SystemError::ENOENT)?;
            let io = css
                .as_any()
                .downcast_ref::<crate::cgroup::controllers::io::IoCss>()
                .ok_or(SystemError::EINVAL)?;
            match ty {
                CgroupCoreFile::IoMax => io.write_max(input)?,
                CgroupCoreFile::IoWeight => io.write_weight(input)?,
                _ => unreachable!(),
            }
            let output = match ty {
                CgroupCoreFile::IoMax => io.max_string(),
                CgroupCoreFile::IoWeight => io.weight_string(),
                _ => unreachable!(),
            };
            Ok(output.into_bytes())
        }
        CgroupCoreFile::CpusetCpus | CgroupCoreFile::CpusetMems => {
            let css = cgroup.css(crate::cgroup::subsys::CgroupSubsysId::Cpuset).ok_or(SystemError::ENOENT)?;
            let cpuset = css.as_any().downcast_ref::<crate::cgroup::controllers::cpuset::CpusetCss>().ok_or(SystemError::EINVAL)?;
            match ty {
                CgroupCoreFile::CpusetCpus => cpuset.set_cpus(input)?,
                CgroupCoreFile::CpusetMems => cpuset.set_mems(input)?,
                _ => unreachable!(),
            }
            Ok(input.trim().as_bytes().to_vec())
        }
        CgroupCoreFile::Controllers
        | CgroupCoreFile::Events
        | CgroupCoreFile::Type
        | CgroupCoreFile::CpuStat
        | CgroupCoreFile::MemoryCurrent
        | CgroupCoreFile::MemoryPeak
        | CgroupCoreFile::MemoryEvents
        | CgroupCoreFile::MemoryStat
        | CgroupCoreFile::MemorySwapCurrent
        | CgroupCoreFile::MemorySwapPeak
        | CgroupCoreFile::MemorySwapEvents
        | CgroupCoreFile::PidsCurrent
        | CgroupCoreFile::PidsEvents
        | CgroupCoreFile::CpusetCpusEffective
        | CgroupCoreFile::IoStat => Err(SystemError::EPERM),
        CgroupCoreFile::Procs | CgroupCoreFile::SubtreeControl => Err(SystemError::EINVAL),
    }
}

pub(super) fn apply_subtree_control(
    cgroup: &Arc<CgroupNode>,
    input: &str,
) -> Result<Vec<u8>, SystemError> {
    let ops = fold_subtree_control_ops(input)?;
    let mut enabled: HashSet<String> = cgroup.subtree_control().into_iter().collect();

    for (name, is_enable) in ops {
        if is_enable {
            if enabled.contains(&name) {
                continue;
            }
            validate_enable_controller(cgroup, &name)?;
            enabled.insert(name);
        } else {
            for child in cgroup.children() {
                if child.subtree_control().iter().any(|ctrl| ctrl == &name) {
                    return Err(SystemError::EBUSY);
                }
            }
            enabled.remove(&name);
        }
    }

    cgroup.set_subtree_control(enabled.clone());
    let mut out: Vec<String> = enabled.into_iter().collect();
    out.sort();
    Ok(encode_controller_list(&out))
}

fn validate_enable_controller(cgroup: &Arc<CgroupNode>, name: &str) -> Result<(), SystemError> {
    let available = available_controllers_for(cgroup);
    if !available.contains(&name) {
        return Err(SystemError::ENOENT);
    }
    if DOMAIN_CONTROLLERS.contains(&name)
        && (cgroup.in_threaded_subtree()
            || matches!(
                cgroup.cgroup_type(),
                crate::cgroup::core::CgroupType::Threaded | crate::cgroup::core::CgroupType::DomainThreaded
            ))
    {
        // Linux cgroup_vet_subtree_control_enable()：thread root
        // （DomainThreaded）同样禁止启用域控制器。
        return Err(SystemError::EBUSY);
    }
    if DOMAIN_CONTROLLERS.contains(&name) && cgroup.parent().is_some() && cgroup.has_tasks() {
        return Err(SystemError::EBUSY);
    }
    Ok(())
}

fn parse_subtree_control_ops(input: &str) -> Result<Vec<(bool, &str)>, SystemError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let mut ops = Vec::new();
    for token in trimmed.split_whitespace() {
        let mut chars = token.chars();
        let op = chars.next().ok_or(SystemError::EINVAL)?;
        let enable = match op {
            '+' => true,
            '-' => false,
            _ => return Err(SystemError::EINVAL),
        };
        let name = chars.as_str();
        if name.is_empty() || name.contains('/') {
            return Err(SystemError::EINVAL);
        }
        ops.push((enable, name));
    }
    Ok(ops)
}

fn fold_subtree_control_ops(input: &str) -> Result<HashMap<String, bool>, SystemError> {
    let mut folded = HashMap::new();
    for (enable, name) in parse_subtree_control_ops(input)? {
        if !is_known_controller(name) {
            return Err(SystemError::EINVAL);
        }
        folded.insert(name.to_string(), enable);
    }
    Ok(folded)
}

fn encode_controller_list(items: &[String]) -> Vec<u8> {
    if items.is_empty() {
        return b"\n".to_vec();
    }
    let mut sorted = items.to_vec();
    sorted.sort();
    let mut line = sorted.join(" ");
    line.push('\n');
    line.into_bytes()
}

fn encode_pids_max(limit: Option<usize>) -> Vec<u8> {
    match limit {
        Some(v) => format!("{}\n", v).into_bytes(),
        None => b"max\n".to_vec(),
    }
}

fn parse_pids_max(input: &str) -> Result<Option<usize>, SystemError> {
    let trimmed = input.trim();
    if trimmed == "max" {
        return Ok(None);
    }
    let value = trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?;
    let value = usize::try_from(value).map_err(|_| SystemError::EINVAL)?;
    Ok(Some(value))
}

fn encode_max_u64(value: Option<u64>) -> Vec<u8> {
    match value {
        Some(v) => format!("{}\n", v).into_bytes(),
        None => b"max\n".to_vec(),
    }
}
fn encode_zero_or_value(value: Option<u64>) -> Vec<u8> {
    match value {
        Some(v) => format!("{}\n", v).into_bytes(),
        None => b"0\n".to_vec(),
    }

}
fn parse_max_u64(input: &str) -> Result<Option<u64>, SystemError> {
    let trimmed = input.trim();
    if trimmed == "max" {
        return Ok(None);
    }
    let value = trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?;
    Ok(Some(value))
}

fn encode_cpu_max(quota: Option<u64>, period_us: u64) -> Vec<u8> {
    match quota {
        Some(quota) => format!("{} {}\n", quota, period_us).into_bytes(),
        None => format!("max {}\n", period_us).into_bytes(),
    }
}

fn parse_cpu_max(input: &str, current_period_us: u64) -> Result<(Option<u64>, u64), SystemError> {
    let mut parts = input.split_whitespace();
    let quota_raw = parts.next().ok_or(SystemError::EINVAL)?;
    let quota = if quota_raw == "max" {
        None
    } else {
        Some(quota_raw.parse::<u64>().map_err(|_| SystemError::EINVAL)?)
    };
    let period = match parts.next() {
        Some(raw) => raw.parse::<u64>().map_err(|_| SystemError::EINVAL)?,
        None => current_period_us,
    };
    if parts.next().is_some() || period == 0 {
        return Err(SystemError::EINVAL);
    }
    Ok((quota, period))
}

fn cpu_bytes(
    cgroup: &Arc<CgroupNode>,
    f: impl FnOnce(&crate::cgroup::controllers::cpu::CpuCss) -> Vec<u8>,
) -> Vec<u8> {
    let Some(css) = cgroup.css(crate::cgroup::subsys::CgroupSubsysId::Cpu) else {
        return b"0\n".to_vec();
    };
    let Some(cpu) = css
        .as_any()
        .downcast_ref::<crate::cgroup::controllers::cpu::CpuCss>()
    else {
        return b"0\n".to_vec();
    };
    f(cpu)
}

fn cpu_stat_for(cgroup: &Arc<CgroupNode>) -> Vec<u8> {
    cpu_bytes(cgroup, |cpu| {
        let stats = cpu.stats();
        let user = stats.utime / 1000;
        let system = stats.stime / 1000;
        format!(
            "usage_usec {}\nuser_usec {}\nsystem_usec {}\nnr_periods {}\nnr_throttled {}\nthrottled_usec {}\n",
            user + system,
            user,
            system,
            stats.nr_periods,
            stats.nr_throttled,
            stats.throttled_time / 1000,
        )
        .into_bytes()
    })
}

fn memory_bytes(
    cgroup: &Arc<CgroupNode>,
    f: impl FnOnce(&crate::cgroup::controllers::memory::MemoryCss) -> Vec<u8>,
) -> Vec<u8> {
    let Some(css) = cgroup.css(crate::cgroup::subsys::CgroupSubsysId::Memory) else {
        return b"0\n".to_vec();
    };
    let Some(memory) = css
        .as_any()
        .downcast_ref::<crate::cgroup::controllers::memory::MemoryCss>()
    else {
        return b"0\n".to_vec();
    };
    f(memory)
}

fn memory_swap_events() -> Vec<u8> {
    b"high 0\nmax 0\nfail 0\n".to_vec()
}

fn is_populated(cgroup: &Arc<CgroupNode>) -> bool {
    cgroup.has_tasks() || cgroup.subtree_task_counter().load(Ordering::Acquire) > 0
}
