//! cgroup v2 io controller.

use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, Ordering};

use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    cgroup::{
        core::CgroupNode,
        subsys::{CgroupSubsys, CgroupSubsysId, CgroupSubsysState, CssFlags},
    },
    libs::spinlock::SpinLock,
    time::{sleep::nanosleep, timekeeping::monotonic_now, PosixTimeSpec},
};

/// Accounting slice for the io throttle: the io counterpart of the cpu
/// controller's 100ms bandwidth period and Linux blk-throttle's
/// `throtl_slice` (linux-6.6.21 block/blk-throttle.c).
const THROTL_SLICE_NS: u64 = 100_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IoDeviceKey {
    pub major: u32,
    pub minor: u32,
}

impl IoDeviceKey {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    fn parse(value: &str) -> Result<Self, SystemError> {
        let (major, minor) = value.split_once(':').ok_or(SystemError::EINVAL)?;
        Ok(Self::new(
            major.parse().map_err(|_| SystemError::EINVAL)?,
            minor.parse().map_err(|_| SystemError::EINVAL)?,
        ))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct IoLimits {
    rbps: Option<u64>,
    wbps: Option<u64>,
    riops: Option<u64>,
    wiops: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
enum IoLimitField {
    ReadBytes,
    WriteBytes,
    ReadIops,
    WriteIops,
}

impl IoLimits {
    fn is_unlimited(self) -> bool {
        self.rbps.is_none() && self.wbps.is_none() && self.riops.is_none() && self.wiops.is_none()
    }
}


#[derive(Debug, Default)]
struct IoStats {
    rbytes: AtomicU64,
    wbytes: AtomicU64,
    rios: AtomicU64,
    wios: AtomicU64,
}

#[derive(Debug, Default)]
struct IoDeviceState {
    limits: IoLimits,
    weight: Option<u16>,
    stats: IoStats,
    /// Token-window accounting for the read direction.
    read_slice: IoSliceState,
    /// Token-window accounting for the write direction.
    write_slice: IoSliceState,
}

/// Accounting window for one (device, direction) pair.
///
/// This mirrors the period settlement of the cpu controller
/// (`refresh_period_locked`) and Linux blk-throttle's `throtl_grp` slice
/// counters: `bytes_disp`/`io_disp` accumulate the amounts dispatched during
/// the current slice and are refilled wholesale when the slice expires.
#[derive(Debug, Clone, Copy, Default)]
struct IoSliceState {
    /// Whether `slice_start_ns` has been observed from the clock yet; the io
    /// counterpart of the cpu controller's `period_initialized`.
    initialized: bool,
    /// Monotonic start of the current slice.
    slice_start_ns: u64,
    bytes_disp: u64,
    io_disp: u64,
}

impl IoSliceState {
    /// Roll expired slices forward in O(1), refilling the dispatch budget on
    /// the new boundary (the io counterpart of the cpu controller's
    /// `refresh_period_locked`).
    fn settle(&mut self, now_ns: u64) {
        if !self.initialized {
            self.initialized = true;
            self.slice_start_ns = now_ns;
            return;
        }
        // 整除一次推进（与 cpu.max 周期追赶同型修复）：`slice_start_ns` 仅在
        // 该 (cgroup, device) 对有 dispatch 时前进，块路径挂起 T 后再回来时，
        // 旧的逐 slice `while` 追赶要跑 T/slice 次迭代。中间 slice 逐个补偿
        // 与最终状态一致（预算整体清零重填），故直接跳到 `now_ns` 所在 slice。
        let elapsed = now_ns.saturating_sub(self.slice_start_ns);
        let slices = elapsed / THROTL_SLICE_NS;
        if slices > 0 {
            self.slice_start_ns = self
                .slice_start_ns
                .saturating_add(slices.saturating_mul(THROTL_SLICE_NS));
            self.bytes_disp = 0;
            self.io_disp = 0;
        }
    }

    fn deadline_ns(&self) -> u64 {
        self.slice_start_ns.saturating_add(THROTL_SLICE_NS)
    }
}

/// Budget a rate limit allows per slice: `rate * THROTL_SLICE_NS / 1s`.
fn slice_allowance(rate: u64) -> u64 {
    ((rate as u128 * THROTL_SLICE_NS as u128) / 1_000_000_000).min(u64::MAX as u128) as u64
}

/// System-wide count of (cgroup, device) pairs carrying at least one io.max
/// limit. While zero, every block dispatch skips the throttle machinery
/// entirely, so devices without an io.max configuration run at zero overhead.
static LIMITED_DEVICE_COUNT: AtomicU64 = AtomicU64::new(0);

/// Whether any io.max limit is configured anywhere in the system.
pub fn any_io_limits_configured() -> bool {
    LIMITED_DEVICE_COUNT.load(Ordering::Relaxed) != 0
}

#[derive(Debug)]
pub struct IoCss {
    cgroup: Weak<CgroupNode>,
    flags: SpinLock<CssFlags>,
    default_weight: SpinLock<u16>,
    devices: SpinLock<HashMap<IoDeviceKey, IoDeviceState>>,
    /// Number of devices of this cgroup carrying at least one io.max limit.
    limited_devices: AtomicU64,
}

impl IoCss {
    pub fn new(cgroup: Weak<CgroupNode>) -> Arc<Self> {
        Arc::new(Self {
            cgroup,
            flags: SpinLock::new(CssFlags::default()),
            default_weight: SpinLock::new(100),
            devices: SpinLock::new(HashMap::new()),
            limited_devices: AtomicU64::new(0),
        })
    }

    pub fn write_max(&self, input: &str) -> Result<(), SystemError> {
        let mut updates = Vec::new();
        for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let mut fields = line.split_whitespace();
            let key = IoDeviceKey::parse(fields.next().ok_or(SystemError::EINVAL)?)?;
            for field in fields {
                let (name, value) = field.split_once('=').ok_or(SystemError::EINVAL)?;
                let field = match name {
                    "rbps" => IoLimitField::ReadBytes,
                    "wbps" => IoLimitField::WriteBytes,
                    "riops" => IoLimitField::ReadIops,
                    "wiops" => IoLimitField::WriteIops,
                    _ => return Err(SystemError::EINVAL),
                };
                updates.push((key, field, parse_limit(value)?));
            }
        }

        let mut devices = self.devices.lock();
        for (key, field, value) in updates {
            let state = devices.entry(key).or_default();
            let was_limited = !state.limits.is_unlimited();
            match field {
                IoLimitField::ReadBytes => state.limits.rbps = value,
                IoLimitField::WriteBytes => state.limits.wbps = value,
                IoLimitField::ReadIops => state.limits.riops = value,
                IoLimitField::WriteIops => state.limits.wiops = value,
            }
            // tg_conf_updated(): restart both direction slices so the new
            // configuration takes effect immediately instead of charging IO
            // already dispatched under the old limits.
            state.read_slice = IoSliceState::default();
            state.write_slice = IoSliceState::default();
            if was_limited != !state.limits.is_unlimited() {
                self.adjust_limited_counts(if was_limited { -1 } else { 1 });
            }
        }
        Ok(())
    }

    /// Track a limited/unlimited device transition in the per-css and
    /// system-wide fast-path counters.
    fn adjust_limited_counts(&self, delta: i64) {
        let step = delta.unsigned_abs();
        if delta < 0 {
            self.limited_devices.fetch_sub(step, Ordering::Relaxed);
            LIMITED_DEVICE_COUNT.fetch_sub(step, Ordering::Relaxed);
        } else {
            self.limited_devices.fetch_add(step, Ordering::Relaxed);
            LIMITED_DEVICE_COUNT.fetch_add(step, Ordering::Relaxed);
        }
    }

    pub fn max_string(&self) -> String {
        let devices = self.devices.lock();
        let mut keys: Vec<IoDeviceKey> = devices.keys().copied().collect();
        keys.sort_unstable();
        let mut out = String::new();
        for key in keys {
            let state = &devices[&key];
            if state.limits.is_unlimited() {
                continue;
            }
            out.push_str(&format!(
                "{}:{} rbps={} wbps={} riops={} wiops={}\n",
                key.major,
                key.minor,
                encode_limit(state.limits.rbps),
                encode_limit(state.limits.wbps),
                encode_limit(state.limits.riops),
                encode_limit(state.limits.wiops),
            ));
        }
        out
    }

    pub fn write_weight(&self, input: &str) -> Result<(), SystemError> {
        let mut devices = self.devices.lock();
        for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let mut fields = line.split_whitespace();
            let target = fields.next().ok_or(SystemError::EINVAL)?;
            let raw_weight = fields.next().ok_or(SystemError::EINVAL)?;
            if fields.next().is_some() {
                return Err(SystemError::EINVAL);
            }
            let raw_weight = raw_weight.strip_prefix("weight=").unwrap_or(raw_weight);
            let weight: u16 = raw_weight.parse().map_err(|_| SystemError::EINVAL)?;
            if !(1..=10_000).contains(&weight) {
                return Err(SystemError::ERANGE);
            }
            if target == "default" {
                *self.default_weight.lock() = weight;
            } else {
                devices.entry(IoDeviceKey::parse(target)?).or_default().weight = Some(weight);
            }
        }
        Ok(())
    }

    pub fn weight_string(&self) -> String {
        let devices = self.devices.lock();
        let mut keys: Vec<IoDeviceKey> = devices
            .iter()
            .filter_map(|(key, state)| state.weight.map(|_| *key))
            .collect();
        keys.sort_unstable();
        let mut out = format!("default {}\n", *self.default_weight.lock());
        for key in keys {
            out.push_str(&format!("{}:{} {}\n", key.major, key.minor, devices[&key].weight.unwrap()));
        }
        out
    }

    pub fn stat_string(&self) -> String {
        let devices = self.devices.lock();
        let mut keys: Vec<IoDeviceKey> = devices.keys().copied().collect();
        keys.sort_unstable();
        let mut out = String::new();
        for key in keys {
            let state = &devices[&key];
            out.push_str(&format!(
                "{}:{} rbytes={} wbytes={} rios={} wios={}\n",
                key.major,
                key.minor,
                state.stats.rbytes.load(Ordering::Relaxed),
                state.stats.wbytes.load(Ordering::Relaxed),
                state.stats.rios.load(Ordering::Relaxed),
                state.stats.wios.load(Ordering::Relaxed),
            ));
        }
        out
    }

    /// Decide whether `bytes` may be dispatched at the current monotonic
    /// timestamp and, if not, the wait required until the next slice
    /// boundary.
    fn throttle(&self, device: IoDeviceKey, write: bool, bytes: usize) -> u64 {
        self.throttle_at(device, write, bytes, monotonic_now().to_ktime_ns())
    }

    /// Decide whether `bytes` may be dispatched at `now_ns` and, if not, the
    /// wait required until the next slice boundary.
    ///
    /// Modeled on blk-throttle's `tg_may_dispatch` + `throtl_charge_bio`:
    /// expired slices are settled (cpu.max period style), the transfer is
    /// admitted when the current slice still has budget — a first transfer of
    /// an oversized request is always admitted so forward progress is
    /// guaranteed — and the charge is recorded at dispatch time.
    ///
    /// The device-table lock is held only while computing the decision. The
    /// caller performs any sleep after this method has released the guard, so
    /// no throttling wait ever happens under the queue lock.
    fn throttle_at(&self, device: IoDeviceKey, write: bool, bytes: usize, now_ns: u64) -> u64 {
        if bytes == 0 || self.limited_devices.load(Ordering::Relaxed) == 0 {
            return 0;
        }

        let mut devices = self.devices.lock();
        let Some(state) = devices.get_mut(&device) else {
            return 0;
        };
        let (bps_limit, iops_limit) = if write {
            (state.limits.wbps, state.limits.wiops)
        } else {
            (state.limits.rbps, state.limits.riops)
        };
        if bps_limit.is_none() && iops_limit.is_none() {
            return 0;
        }

        let bucket = if write {
            &mut state.write_slice
        } else {
            &mut state.read_slice
        };
        bucket.settle(now_ns);

        let bytes_allowed = bps_limit.map(slice_allowance);
        let ios_allowed = iops_limit.map(slice_allowance);
        let bytes_ok = bytes_allowed.is_none_or(|allowed| {
            bucket.bytes_disp == 0 || bucket.bytes_disp.saturating_add(bytes as u64) <= allowed
        });
        let ios_ok =
            ios_allowed.is_none_or(|allowed| bucket.io_disp == 0 || bucket.io_disp + 1 <= allowed);

        if !bytes_ok || !ios_ok {
            return bucket.deadline_ns().saturating_sub(now_ns).max(1);
        }

        // throtl_charge_bio(): charge the admitted transfer to this slice.
        bucket.bytes_disp = bucket.bytes_disp.saturating_add(bytes as u64);
        bucket.io_disp += 1;
        0
    }

    pub fn account(&self, device: IoDeviceKey, write: bool, bytes: usize) {
        let mut devices = self.devices.lock();
        let state = devices.entry(device).or_default();
        if write {
            state.stats.wbytes.fetch_add(bytes as u64, Ordering::Relaxed);
            state.stats.wios.fetch_add(1, Ordering::Relaxed);
        } else {
            state.stats.rbytes.fetch_add(bytes as u64, Ordering::Relaxed);
            state.stats.rios.fetch_add(1, Ordering::Relaxed);
        }
    }
}
fn parse_limit(value: &str) -> Result<Option<u64>, SystemError> {
    if value == "max" {
        return Ok(None);
    }
    let value = value.parse::<u64>().map_err(|_| SystemError::EINVAL)?;
    if value == 0 {
        return Err(SystemError::ERANGE);
    }
    Ok(Some(value))
}

fn encode_limit(value: Option<u64>) -> String {
    value.map(|value| value.to_string()).unwrap_or_else(|| "max".to_string())
}

impl CgroupSubsysState for IoCss {
    fn subsys_id(&self) -> CgroupSubsysId {
        CgroupSubsysId::Io
    }

    fn cgroup(&self) -> Arc<CgroupNode> {
        self.cgroup.upgrade().expect("io cgroup dropped")
    }

    fn parent(&self) -> Option<Arc<dyn CgroupSubsysState>> {
        self.cgroup
            .upgrade()
            .and_then(|cgroup| cgroup.parent())
            .and_then(|parent| parent.css(CgroupSubsysId::Io))
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

#[derive(Debug)]
pub struct IoController;

impl CgroupSubsys for IoController {
    fn id(&self) -> CgroupSubsysId {
        CgroupSubsysId::Io
    }

    fn name(&self) -> &'static str {
        "io"
    }

    fn css_alloc(
        &self,
        _parent: Option<&Arc<dyn CgroupSubsysState>>,
        cgroup: &Arc<CgroupNode>,
    ) -> Result<Arc<dyn CgroupSubsysState>, SystemError> {
        Ok(IoCss::new(Arc::downgrade(cgroup)))
    }

    fn css_free(&self, _css: &Arc<dyn CgroupSubsysState>) {}
}

pub fn account_io(cgroup: &Arc<CgroupNode>, device: IoDeviceKey, write: bool, bytes: usize) {
    let Some(css) = cgroup.css(CgroupSubsysId::Io) else {
        return;
    };
    let Some(io) = css.as_any().downcast_ref::<IoCss>() else {
        return;
    };
    io.account(device, write, bytes);
}
/// Enforce all configured limits in the current cgroup and its ancestors.
///
/// Mirrors `blk_throtl_bio()`'s climb from the bio's group towards the root:
/// each level admits the transfer against its own slice budget. The wait is
/// always performed after the level's device-table lock has been released,
/// and the level is re-checked after waking so concurrent dispatches that
/// consumed the refilled budget are handled without busy-waiting.
pub fn throttle_io(
    cgroup: &Arc<CgroupNode>,
    device: IoDeviceKey,
    write: bool,
    bytes: usize,
) -> Result<(), SystemError> {
    let mut current = Some(cgroup.clone());
    while let Some(node) = current {
        if let Some(css) = node.css(CgroupSubsysId::Io) {
            if let Some(io) = css.as_any().downcast_ref::<IoCss>() {
                loop {
                    let wait_ns = io.throttle(device, write, bytes);
                    if wait_ns == 0 {
                        break;
                    }
                    nanosleep(PosixTimeSpec::from_ns(wait_ns.max(500_000)))?;
                }
            }
        }
        current = node.parent();
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that adjust the process-wide LIMITED_DEVICE_COUNT.
    static COUNTER_TEST_LOCK: SpinLock<()> = SpinLock::new(());

    #[test]
    fn io_control_files_round_trip_and_account() {
        let _guard = COUNTER_TEST_LOCK.lock();
        let io = IoCss::new(Weak::new());
        io.write_max("8:0 rbps=1048576 wbps=max riops=100\n")
            .unwrap();
        assert_eq!(
            io.max_string(),
            "8:0 rbps=1048576 wbps=max riops=100 wiops=max\n"
        );

        io.write_weight("default 200\n8:0 500\n").unwrap();
        assert_eq!(io.weight_string(), "default 200\n8:0 500\n");

        io.account(IoDeviceKey::new(8, 0), false, 4096);
        io.account(IoDeviceKey::new(8, 0), true, 8192);
        assert_eq!(
            io.stat_string(),
            "8:0 rbytes=4096 wbytes=8192 rios=1 wios=1\n"
        );

        // Drop the limits again so the shared fast-path counter is restored
        // for tests running after this one.
        io.write_max("8:0 rbps=max riops=max").unwrap();
        assert!(io.max_string().is_empty());
    }

    #[test]
    fn io_rejects_invalid_limits_and_weights() {
        let io = IoCss::new(Weak::new());
        assert_eq!(io.write_max("8:0 unknown=1"), Err(SystemError::EINVAL));
        assert_eq!(io.write_max("8:0 rbps=0"), Err(SystemError::ERANGE));
        assert_eq!(
            io.write_max("8:0 rbps=1\n8:1 unknown=1"),
            Err(SystemError::EINVAL)
        );
        assert!(io.max_string().is_empty());
        assert_eq!(io.write_weight("default 0"), Err(SystemError::ERANGE));
    }

    #[test]
    fn io_slice_admits_within_budget_and_waits_at_boundary() {
        let _guard = COUNTER_TEST_LOCK.lock();
        let io = IoCss::new(Weak::new());
        // 4096 B/s over a 100ms slice => 409 bytes per slice.
        io.write_max("8:0 rbps=4096").unwrap();
        let device = IoDeviceKey::new(8, 0);
        let t0 = 5_000_000_000u64;

        // The first dispatch of a slice is admitted even when larger than
        // the whole slice budget, so oversized requests still make forward
        // progress.
        assert_eq!(io.throttle_at(device, false, 512, t0), 0);
        // The slice is now charged; a further transfer must wait until the
        // boundary.
        let wait = io.throttle_at(device, false, 1, t0 + 1_000_000);
        assert!(wait > 0 && wait <= THROTL_SLICE_NS);

        // After the boundary the budget is refilled: 409 bytes fit again.
        let boundary = t0 + THROTL_SLICE_NS;
        assert_eq!(io.throttle_at(device, false, 409, boundary), 0);
        assert!(io.throttle_at(device, false, 1, boundary) > 0);

        // Unlimited directions and unknown devices pass through untouched.
        assert_eq!(io.throttle_at(device, true, 4096, boundary), 0);
        assert_eq!(io.throttle_at(IoDeviceKey::new(9, 9), false, 4096, boundary), 0);

        // Removing the last limit restores the zero-overhead fast path.
        io.write_max("8:0 rbps=max").unwrap();
        assert_eq!(io.limited_devices.load(Ordering::Relaxed), 0);
        assert_eq!(io.throttle_at(device, false, 4096, boundary), 0);
    }

    #[test]
    fn io_slice_settles_refill_style() {
        // 10 KiB/s over 100ms slices => 1024 bytes per slice.
        let mut bucket = IoSliceState::default();
        let t0 = 10_000_000_000u64;
        bucket.settle(t0);
        bucket.bytes_disp = 1024;
        bucket.io_disp = 4;
        // Half a slice later the budget is not refilled yet.
        bucket.settle(t0 + THROTL_SLICE_NS / 2);
        assert_eq!(bucket.bytes_disp, 1024);
        assert_eq!(bucket.io_disp, 4);
        // Two slices later the counters were refilled twice and the window
        // slid to the slice containing `now`.
        let now = t0 + 2 * THROTL_SLICE_NS + 1;
        bucket.settle(now);
        assert_eq!(bucket.bytes_disp, 0);
        assert_eq!(bucket.io_disp, 0);
        assert_eq!(bucket.slice_start_ns, t0 + 2 * THROTL_SLICE_NS);
        assert!(bucket.deadline_ns() > now);

        assert_eq!(slice_allowance(10_240), 1024);
        assert_eq!(slice_allowance(u64::MAX), 1_844_674_407_370_955_161);
    }

    #[test]
    fn io_slice_settle_advances_in_one_step_after_long_idle() {
        // 与 cpu.max 同型的有界性验证：块路径挂起 10 分钟（6000 个 100ms
        // slice）后首次 dispatch，旧逐 slice while 追赶要跑 6000 次迭代；
        // 现在一次整除推进到 `now` 所在 slice 并整体重填预算。
        let mut bucket = IoSliceState::default();
        let t0 = 1_000_000_000u64;
        bucket.settle(t0);
        bucket.bytes_disp = 1024;
        bucket.io_disp = 4;
        let idle = 600_000_000_000u64; // 10 分钟 = 6000 slices
        let now = t0 + idle + 3;
        bucket.settle(now);
        assert_eq!(bucket.slice_start_ns, t0 + 6000 * THROTL_SLICE_NS);
        assert!(bucket.slice_start_ns <= now && now - bucket.slice_start_ns < THROTL_SLICE_NS);
        assert_eq!(bucket.bytes_disp, 0);
        assert_eq!(bucket.io_disp, 0);
        // 同一时刻再次 settle 为零推进（迭代次数与睡眠长度无关）。
        bucket.settle(now);
        assert_eq!(bucket.slice_start_ns, t0 + 6000 * THROTL_SLICE_NS);
    }

    #[test]
    fn io_limit_transitions_track_fast_path_counters() {
        let _guard = COUNTER_TEST_LOCK.lock();
        let io = IoCss::new(Weak::new());
        let before = LIMITED_DEVICE_COUNT.load(Ordering::Relaxed);

        io.write_max("8:0 rbps=1000\n8:1 wiops=10").unwrap();
        assert_eq!(io.limited_devices.load(Ordering::Relaxed), 2);
        assert_eq!(LIMITED_DEVICE_COUNT.load(Ordering::Relaxed), before + 2);
        assert!(any_io_limits_configured());

        // Re-writing one field of an already limited device must not bump
        // the counters again, and clearing the last limit of a device must
        // restore the previous counts.
        io.write_max("8:0 rbps=2000").unwrap();
        assert_eq!(io.limited_devices.load(Ordering::Relaxed), 2);
        io.write_max("8:0 rbps=max").unwrap();
        assert_eq!(io.limited_devices.load(Ordering::Relaxed), 1);
        io.write_max("8:1 wiops=max").unwrap();
        assert_eq!(io.limited_devices.load(Ordering::Relaxed), 0);
        assert_eq!(LIMITED_DEVICE_COUNT.load(Ordering::Relaxed), before);
    }
}
