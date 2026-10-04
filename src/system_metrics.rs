//! Live CPU, physical memory and network readings, published to themes as
//! `system.*` bindings.
//!
//! Readings are whole units rather than floats so [`SystemMetrics`] stays
//! `Copy + Eq` and can sit inside `ThemeRuntime`, which is compared to decide
//! whether anything changed. Float equality would make that comparison
//! meaningless, and a theme never needs more precision than a whole percent.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::FILETIME;
use windows::Win32::NetworkManagement::IpHelper::{
    GetBestInterface, GetIfEntry2, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211, MIB_IF_ROW2,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

const BYTES_PER_MB: u64 = 1024 * 1024;

/// Any public address works: `GetBestInterface` only consults the routing
/// table, nothing is sent. 8.8.8.8 reads the same in either byte order.
const INTERNET_PROBE_ADDRESS: u32 = u32::from_be_bytes([8, 8, 8, 8]);

/// What kind of link carries traffic to the internet right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetworkKind {
    /// No route out, or the interface could not be read.
    #[default]
    None,
    Ethernet,
    WiFi,
    /// Anything else with a route out: a VPN tunnel, a phone tether, a VM switch.
    Other,
}

impl NetworkKind {
    fn from_interface_type(interface_type: u32) -> Self {
        match interface_type {
            IF_TYPE_ETHERNET_CSMACD => Self::Ethernet,
            IF_TYPE_IEEE80211 => Self::WiFi,
            _ => Self::Other,
        }
    }

    /// Published as `system.network.kind`, so a theme can branch on it.
    pub fn code(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Ethernet => 1,
            Self::WiFi => 2,
            Self::Other => 3,
        }
    }

    /// Published as `system.network.type`. Technical names, left untranslated.
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Ethernet => "Ethernet",
            Self::WiFi => "Wi-Fi",
            Self::Other => "Net",
        }
    }
}

/// One reading of machine load. All-zero means "nothing sampled yet", which
/// renders as an idle machine rather than as an error: a widget that cannot
/// read a counter should stay quiet, not shout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemMetrics {
    /// Share of all logical processors busy since the previous sample, 0 to 100.
    pub cpu_percent: u8,
    /// Share of physical memory in use, 0 to 100, as Windows itself reports it.
    pub memory_percent: u8,
    pub memory_used_mb: u32,
    pub memory_total_mb: u32,
    pub cpu_count: u16,
    /// Receive and send rates on the active interface, in kilobits per second.
    pub network_down_kbps: u32,
    pub network_up_kbps: u32,
    pub network_kind: NetworkKind,
}

/// Kernel CPU counters, in 100-nanosecond ticks summed over every logical
/// processor. Windows folds idle time into kernel time, so `total` is
/// kernel + user and `idle` is a subset of it, never an addition to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTimes {
    idle: u64,
    total: u64,
}

impl CpuTimes {
    pub fn from_kernel_user(idle: u64, kernel: u64, user: u64) -> Self {
        Self {
            idle,
            total: kernel.saturating_add(user),
        }
    }
}

/// CPU load between two readings, rounded to the nearest percent.
///
/// `None` means "no honest answer available": the first reading of the
/// session, a machine that was suspended between samples, or counters that
/// went backwards after a resume. Callers keep the previous value in that
/// case, because a fabricated 0% reads as real to anyone watching the widget.
pub fn cpu_percent_between(previous: CpuTimes, current: CpuTimes) -> Option<u8> {
    let total = current.total.checked_sub(previous.total)?;
    if total == 0 {
        return None;
    }
    // Clamped because idle is reported by a separate counter from kernel time;
    // a scheduling race can make it appear to advance further than the total.
    let idle = current.idle.saturating_sub(previous.idle).min(total);
    let busy = total - idle;
    Some((((busy * 100) + total / 2) / total) as u8)
}

/// Byte counters of one interface, tagged with its index so a switch from
/// Wi-Fi to Ethernet is never differenced as if it were one link.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetworkCounters {
    interface_index: u32,
    in_octets: u64,
    out_octets: u64,
}

impl NetworkCounters {
    pub fn new(interface_index: u32, in_octets: u64, out_octets: u64) -> Self {
        Self {
            interface_index,
            in_octets,
            out_octets,
        }
    }
}

/// Receive and send rates between two readings, in kilobits per second.
///
/// `None` for the same reasons as [`cpu_percent_between`]: no time elapsed,
/// counters that went backwards (an adapter reset), or a different interface,
/// whose counters have nothing to do with the previous ones.
pub fn network_kbps_between(
    previous: NetworkCounters,
    current: NetworkCounters,
    elapsed: Duration,
) -> Option<(u32, u32)> {
    if previous.interface_index != current.interface_index {
        return None;
    }
    let millis = elapsed.as_millis() as u64;
    if millis == 0 {
        return None;
    }
    // bytes * 8 bits, / 1000 per kilobit, / (millis / 1000) per second.
    let rate = |before: u64, after: u64| -> Option<u32> {
        let bytes = after.checked_sub(before)?;
        Some((bytes.saturating_mul(8) / millis).min(u64::from(u32::MAX)) as u32)
    };
    Some((
        rate(previous.in_octets, current.in_octets)?,
        rate(previous.out_octets, current.out_octets)?,
    ))
}

/// Turns raw `MEMORYSTATUSEX` numbers into the published reading.
///
/// `load` is Windows' own in-use percentage, which is what Task Manager
/// shows; deriving it from total/available instead would drift from the
/// figure users compare against.
fn memory_reading(total_bytes: u64, available_bytes: u64, load: u32) -> (u8, u32, u32) {
    let used_bytes = total_bytes.saturating_sub(available_bytes);
    (
        load.min(100) as u8,
        (used_bytes / BYTES_PER_MB) as u32,
        (total_bytes / BYTES_PER_MB) as u32,
    )
}

/// Holds the previous CPU counters so successive samples can be differenced.
#[derive(Debug, Default)]
pub struct SystemSampler {
    previous: Option<(CpuTimes, Instant)>,
    previous_network: Option<(NetworkCounters, Instant)>,
    latest: SystemMetrics,
}

impl SystemSampler {
    pub fn new() -> Self {
        Self {
            previous: None,
            previous_network: None,
            latest: SystemMetrics {
                cpu_count: logical_processor_count(),
                ..SystemMetrics::default()
            },
        }
    }

    pub fn latest(&self) -> SystemMetrics {
        self.latest
    }

    /// Takes a fresh reading, differencing the CPU counters against the
    /// previous one unless that one is older than `max_age`.
    ///
    /// The age check is what keeps a stale baseline from being reported as
    /// current load: the row being switched back on after an hour, a machine
    /// resuming from sleep, or a starved timer would otherwise average the
    /// whole gap and present it as the last second. Best effort throughout —
    /// a failed Win32 call leaves the previous figures in place rather than
    /// flashing zeros.
    pub fn sample(&mut self, max_age: Duration) -> SystemMetrics {
        if let Some(current) = read_cpu_times() {
            if let Some((previous, taken_at)) = self.previous {
                if taken_at.elapsed() <= max_age {
                    if let Some(percent) = cpu_percent_between(previous, current) {
                        self.latest.cpu_percent = percent;
                    }
                }
            }
            self.previous = Some((current, Instant::now()));
        }
        if let Some((percent, used_mb, total_mb)) = read_memory() {
            self.latest.memory_percent = percent;
            self.latest.memory_used_mb = used_mb;
            self.latest.memory_total_mb = total_mb;
        }
        match read_network() {
            Some((current, kind)) => {
                if let Some((previous, taken_at)) = self.previous_network {
                    let elapsed = taken_at.elapsed();
                    if previous.interface_index != current.interface_index {
                        // The old link's rate says nothing about the new one.
                        self.latest.network_down_kbps = 0;
                        self.latest.network_up_kbps = 0;
                    } else if elapsed <= max_age {
                        if let Some((down, up)) = network_kbps_between(previous, current, elapsed) {
                            self.latest.network_down_kbps = down;
                            self.latest.network_up_kbps = up;
                        }
                    }
                }
                self.latest.network_kind = kind;
                self.previous_network = Some((current, Instant::now()));
            }
            None => {
                // No route out is a real zero, not a failed read to paper over.
                self.latest.network_kind = NetworkKind::None;
                self.latest.network_down_kbps = 0;
                self.latest.network_up_kbps = 0;
                self.previous_network = None;
            }
        }
        if self.latest.cpu_count == 0 {
            self.latest.cpu_count = logical_processor_count();
        }
        self.latest
    }
}

/// Reading for callers with no sampler of their own, such as the Theme Studio
/// preview. Rate-limited because a preview repaints far more often than the
/// machine's load actually changes, and a sample that spans a few milliseconds
/// measures scheduling noise rather than load.
pub fn shared_sample() -> SystemMetrics {
    static SHARED: Mutex<Option<(SystemSampler, Instant)>> = Mutex::new(None);
    const MIN_INTERVAL: Duration = Duration::from_millis(500);

    let mut guard = SHARED.lock().unwrap_or_else(|error| error.into_inner());
    match guard.as_mut() {
        Some((sampler, taken_at)) if taken_at.elapsed() < MIN_INTERVAL => sampler.latest(),
        Some((sampler, taken_at)) => {
            *taken_at = Instant::now();
            sampler.sample(MIN_INTERVAL * 4)
        }
        None => {
            let mut sampler = SystemSampler::new();
            let metrics = sampler.sample(MIN_INTERVAL * 4);
            *guard = Some((sampler, Instant::now()));
            metrics
        }
    }
}

fn logical_processor_count() -> u16 {
    std::thread::available_parallelism()
        .map(|count| count.get().min(u16::MAX as usize) as u16)
        .unwrap_or(0)
}

fn read_cpu_times() -> Option<CpuTimes> {
    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()?;
    }
    Some(CpuTimes::from_kernel_user(
        filetime_ticks(idle),
        filetime_ticks(kernel),
        filetime_ticks(user),
    ))
}

fn read_memory() -> Option<(u8, u32, u32)> {
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe {
        GlobalMemoryStatusEx(&mut status).ok()?;
    }
    Some(memory_reading(
        status.ullTotalPhys,
        status.ullAvailPhys,
        status.dwMemoryLoad,
    ))
}

fn read_network() -> Option<(NetworkCounters, NetworkKind)> {
    let mut index = 0u32;
    if unsafe { GetBestInterface(INTERNET_PROBE_ADDRESS, &mut index) } != 0 {
        return None;
    }
    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: index,
        ..Default::default()
    };
    unsafe { GetIfEntry2(&mut row) }.ok().ok()?;
    Some((
        NetworkCounters::new(index, row.InOctets, row.OutOctets),
        NetworkKind::from_interface_type(row.Type),
    ))
}

fn filetime_ticks(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

#[cfg(test)]
mod tests;
