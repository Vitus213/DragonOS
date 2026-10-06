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
    MaxDepth,
    MaxDescendants,
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

const BASE_FILE_SPECS: [CgroupFileSpec; 6] = [
    CgroupFileSpec {
        name: "cgroup.max.descendants",
        ty: CgroupCoreFile::MaxDescendants,
        init: b"max\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::All,
    },
    CgroupFileSpec {
        name: "cgroup.max.depth",
        ty: CgroupCoreFile::MaxDepth,
        init: b"max\n",
        mode: 0o644,
        visibility: CgroupFileVisibility::All,
    },
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
        // cgroup.max.depth / cgroup.max.descendants（#39）：Linux show 语义，
        // usize::MAX 打印 "max"，否则十进制。
        CgroupCoreFile::MaxDepth => encode_hierarchy_limit(cgroup.max_depth()),
        CgroupCoreFile::MaxDescendants => encode_hierarchy_limit(cgroup.max_descendants()),
        CgroupCoreFile::Freeze => {
            format!("{}\n", if cgroup.self_freeze_requested() { 1 } else { 0 }).into_bytes()
        }
        CgroupCoreFile::CpuStat => cpu_stat_for(cgroup),
        CgroupCoreFile::CpuWeight => {
            cpu_bytes(cgroup, |cpu| format!("{}\n", cpu.shares()).into_bytes())
        }
        CgroupCoreFile::CpuMax => cpu_bytes(cgroup, |cpu| {
            let (quota, period) = cpu.bandwidth();
            encode_cpu_max(quota, period)
        }),
        CgroupCoreFile::MemoryCurrent => memory_bytes(cgroup, |memory| {
            format!("{}\n", memory.current()).into_bytes()
        }),
        CgroupCoreFile::MemoryPeak => {
            memory_bytes(cgroup, |memory| format!("{}\n", memory.peak()).into_bytes())
        }
        CgroupCoreFile::MemoryMin => {
            memory_bytes(cgroup, |memory| encode_zero_or_value(memory.min()))
        }
        CgroupCoreFile::MemoryLow => {
            memory_bytes(cgroup, |memory| encode_zero_or_value(memory.low()))
        }
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
        CgroupCoreFile::CpusetCpus
        | CgroupCoreFile::CpusetCpusEffective
        | CgroupCoreFile::CpusetMems => {
            let css = cgroup.css(crate::cgroup::subsys::CgroupSubsysId::Cpuset);
            let Some(css) = css else {
                return b"\n".to_vec();
            };
            let Some(cpuset) = css
                .as_any()
                .downcast_ref::<crate::cgroup::controllers::cpuset::CpusetCss>()
            else {
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
pub(super) fn write_type_file(
    cgroup: &Arc<CgroupNode>,
    input: &str,
) -> Result<Vec<u8>, SystemError> {
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
            let css = cgroup
                .css(crate::cgroup::subsys::CgroupSubsysId::Cpuset)
                .ok_or(SystemError::ENOENT)?;
            let cpuset = css
                .as_any()
                .downcast_ref::<crate::cgroup::controllers::cpuset::CpusetCss>()
                .ok_or(SystemError::EINVAL)?;
            match ty {
                CgroupCoreFile::CpusetCpus => cpuset.set_cpus(input)?,
                CgroupCoreFile::CpusetMems => cpuset.set_mems(input)?,
                _ => unreachable!(),
            }
            Ok(input.trim().as_bytes().to_vec())
        }
        // cgroup.max.depth / cgroup.max.descendants 写入（#39）：Linux write
        // 语义——"max" 复位为 usize::MAX，负值 ERANGE，非法值 EINVAL。
        CgroupCoreFile::MaxDepth | CgroupCoreFile::MaxDescendants => {
            let value = parse_hierarchy_limit(input)?;
            match ty {
                CgroupCoreFile::MaxDepth => cgroup.set_max_depth(value),
                _ => cgroup.set_max_descendants(value),
            }
            Ok(encode_hierarchy_limit(value))
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
                crate::cgroup::core::CgroupType::Threaded
                    | crate::cgroup::core::CgroupType::DomainThreaded
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

fn encode_hierarchy_limit(value: usize) -> Vec<u8> {
    // 对齐 Linux cgroup_max_depth_show/cgroup_max_descendants_show：
    // 无限制打印 "max"，否则十进制整数。usize::MAX 即内部 "max" 表示。
    if value == usize::MAX {
        b"max\n".to_vec()
    } else {
        format!("{}\n", value).into_bytes()
    }
}

fn parse_hierarchy_limit(input: &str) -> Result<usize, SystemError> {
    // 对齐 Linux cgroup_max_*_write（v6.6 cgroup.c:3540/3583）：strstrip 后
    // "max" ⇒ INT_MAX，否则 `kstrtoint(buf, 0, &int)`——base 0（十进制/
    // 0x 十六进制/0 八进制），格式错 EINVAL，数值溢出 int 范围 ERANGE，
    // 显式负值同样 ERANGE。DragonOS 以 usize::MAX 表示无限制，可表达
    // 上界钳到 i32::MAX（超过它在 Linux 上就是 ERANGE，同码拒绝）。
    let trimmed = input.trim();
    if trimmed == "max" {
        return Ok(usize::MAX);
    }
    // kstrtoint 的符号处理：'+' 前缀直接跳过；'-' 解析成功且值为负 ⇒
    // `depth < 0` ⇒ ERANGE（"-0" 数值为 0，Linux 接受，同样落到下方
    // 钳制路径）。溢出（Err(true)）与负值同为 ERANGE，格式非法 ⇒ EINVAL。
    let unsigned = if let Some(rest) = trimmed.strip_prefix('+') {
        rest
    } else {
        trimmed
    };
    let (negative, magnitude_text) = match unsigned.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, unsigned),
    };
    let magnitude = parse_kstrtoint_radix(magnitude_text).map_err(|overflow| {
        if overflow {
            SystemError::ERANGE
        } else {
            SystemError::EINVAL
        }
    })?;
    if negative && magnitude != 0 {
        return Err(SystemError::ERANGE);
    }
    if magnitude > i32::MAX as u64 {
        return Err(SystemError::ERANGE);
    }
    // Linux 以 INT_MAX 兼作 "max" 哨兵：写 2147483647 后 show 打印 "max"。
    // 等价映射到内部 usize::MAX，保持可观测行为一致。
    if magnitude == i32::MAX as u64 {
        return Ok(usize::MAX);
    }
    usize::try_from(magnitude).map_err(|_| SystemError::ERANGE)
}

/// `kstrtoint(buf, 0, …)` 的非负分支：base 0 前缀（0x/0X ⇒ 16 进制，
/// 0 ⇒ 8 进制，其余 ⇒ 10 进制）。返回 `Result<u64, bool>`——Err(true)
/// 表示数字合法但超出 u64（对应 Linux 溢出 ERANGE），Err(false) 表示
/// 格式非法（EINVAL）。
fn parse_kstrtoint_radix(text: &str) -> Result<u64, bool> {
    let (digits, radix) =
        if let Some(rest) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            (rest, 16)
        } else if let Some(rest) = text.strip_prefix('0') {
            // 前导 0 ⇒ 八进制；裸 "0"（去前缀后为空）值即 0。
            if rest.is_empty() {
                return Ok(0);
            }
            (rest, 8)
        } else {
            (text, 10)
        };
    // core::num::ParseIntErrorKind 是 unstable 特性项，内核构建不可用：
    // 先做格式校验（非空且逐字符为该 radix 的合法数字），不合法直接
    // EINVAL；格式合法而 from_str_radix 仍失败 ⇒ 数值超出 u64（溢出，
    // 对应 Linux kstrtoint 的 ERANGE 路径）。
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(false);
    }
    u64::from_str_radix(digits, radix).map_err(|_| true)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knob_encode_decode_matches_linux_show_write() {
        // show（v6.6 cgroup.c:3527/3570）：usize::MAX ⇒ "max"，否则十进制。
        assert_eq!(encode_hierarchy_limit(usize::MAX), b"max\n".to_vec());
        assert_eq!(encode_hierarchy_limit(64), b"64\n".to_vec());
        // write（cgroup.c:3540/3583）："max" ⇒ 无限制；kstrtoint base 0：
        // 0x ⇒ 16 进制、前导 0 ⇒ 8 进制。
        assert_eq!(parse_hierarchy_limit("max\n"), Ok(usize::MAX));
        assert_eq!(parse_hierarchy_limit("  64 "), Ok(64));
        assert_eq!(parse_hierarchy_limit("0x10"), Ok(16));
        assert_eq!(parse_hierarchy_limit("010"), Ok(8));
        // 显式负值 ERANGE（"-0" 数值为 0，Linux 接受）。
        assert_eq!(parse_hierarchy_limit("-1"), Err(SystemError::ERANGE));
        assert_eq!(parse_hierarchy_limit("-0"), Ok(0));
        // 格式非法 EINVAL；超出 int 可表达范围 ERANGE；
        // Linux 的 INT_MAX 哨兵等价映射回 "max"。
        assert_eq!(parse_hierarchy_limit("abc"), Err(SystemError::EINVAL));
        assert_eq!(
            parse_hierarchy_limit("2147483648"),
            Err(SystemError::ERANGE)
        );
        assert_eq!(parse_hierarchy_limit("2147483647"), Ok(usize::MAX));
        // 合法数字但超出 u64：溢出（ERANGE）而非格式错（EINVAL）。
        assert_eq!(
            parse_hierarchy_limit("18446744073709551616"),
            Err(SystemError::ERANGE)
        );
    }
}
