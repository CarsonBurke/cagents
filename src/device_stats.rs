//! How loaded a device is: its processors, memory and the disk holding its
//! home directory, as the status bar of a window working on it shows them.
//!
//! The daemon on the device samples these while at least one window watches,
//! and sends a reading only when a figure a window shows has moved, so a
//! steady device costs its windows nothing to keep current.

use serde::{Deserialize, Serialize};

/// One reading of a device. A part the device could not read is `None`, and
/// a window shows that part as unknown rather than as zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceStats {
    pub cpu: Option<Cpu>,
    pub memory: Option<Memory>,
    pub disk: Option<Disk>,
}

/// Processor use, smoothed over the last several seconds so a momentary
/// spike does not read as a saturated device.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cpu {
    /// Busy share of every online processor, whole percent from 0 to 100.
    pub percent: u8,
    /// Online logical processors.
    pub cores: u16,
    /// One-minute load average, in hundredths.
    pub load_centi: u32,
}

/// Memory in use, as the kernel counts memory it could not hand to a new
/// program without swapping: total less what is available.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub used: u64,
    pub total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
}

/// The file system holding the device user's home directory, where their
/// projects and agents' worktrees ordinarily live.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Disk {
    pub used: u64,
    pub total: u64,
}

/// Whole percent of `used` in `total`, rounded to nearest, or `None` for an
/// empty total.
pub fn percent(used: u64, total: u64) -> Option<u8> {
    (total > 0).then(|| {
        ((u128::from(used.min(total)) * 100 + u128::from(total) / 2) / u128::from(total)) as u8
    })
}

impl Memory {
    pub fn percent(&self) -> Option<u8> {
        percent(self.used, self.total)
    }
}

impl Disk {
    pub fn percent(&self) -> Option<u8> {
        percent(self.used, self.total)
    }
}

/// A share at which a figure turns amber, and red.
pub const WARNING_PERCENT: u8 = 80;
pub const CRITICAL_PERCENT: u8 = 90;

/// Processor use moves a little with every sample on a busy device; a window
/// is sent a new figure only once it has moved this far, or crossed into
/// another band.
const CPU_STEP: u8 = 2;

fn band(percent: u8) -> u8 {
    match percent {
        percent if percent >= CRITICAL_PERCENT => 2,
        percent if percent >= WARNING_PERCENT => 1,
        _ => 0,
    }
}

impl DeviceStats {
    /// Whether a window showing `self` should be sent `next`: when one of the
    /// status bar's percentages moves, or a part appears or disappears. The
    /// details on hover are as of the last reading sent, which keeps a busy
    /// device from redrawing every window every sample for figures that
    /// mostly sit behind a tooltip.
    pub fn superseded_by(&self, next: &Self) -> bool {
        let cpu = match (self.cpu, next.cpu) {
            (Some(shown), Some(next)) => {
                shown.percent.abs_diff(next.percent) >= CPU_STEP
                    || band(shown.percent) != band(next.percent)
                    || shown.cores != next.cores
            }
            (shown, next) => shown.is_some() != next.is_some(),
        };
        cpu || self.memory.map(|memory| memory.percent())
            != next.memory.map(|memory| memory.percent())
            || self.disk.map(|disk| disk.percent()) != next.disk.map(|disk| disk.percent())
    }
}

/// How often the daemon reads the device while a window watches.
pub const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Reads a device's figures between successive samples. Processor use is a
/// difference between two readings of the kernel's counters, so the first
/// sample has none.
#[cfg(feature = "host")]
#[derive(Default)]
pub struct Sampler {
    previous: Option<platform::CpuTimes>,
    /// Exponentially smoothed busy share, from 0 to 1.
    smoothed: Option<f64>,
    home: Option<std::ffi::CString>,
}

#[cfg(feature = "host")]
impl Sampler {
    /// Weight of the newest interval in the smoothed processor figure: at a
    /// two-second interval a spike fades to under a fifth within six seconds.
    const SMOOTHING: f64 = 0.45;

    pub fn new() -> Self {
        use std::os::unix::ffi::OsStringExt;
        Self {
            previous: None,
            smoothed: None,
            home: std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .and_then(|home| std::ffi::CString::new(home.into_vec()).ok()),
        }
    }

    pub fn sample(&mut self) -> DeviceStats {
        let current = platform::cpu_times();
        let busy = match (self.previous, current) {
            (Some(previous), Some(current)) => current.busy_since(&previous),
            _ => None,
        };
        self.previous = current;
        if let Some(busy) = busy {
            self.smoothed = Some(match self.smoothed {
                Some(smoothed) => smoothed + Self::SMOOTHING * (busy - smoothed),
                None => busy,
            });
        }
        let cores = platform::cores();
        DeviceStats {
            cpu: self.smoothed.zip(cores).map(|(busy, cores)| Cpu {
                percent: (busy.clamp(0.0, 1.0) * 100.0).round() as u8,
                cores,
                load_centi: platform::load_average()
                    .map_or(0, |load| (load.max(0.0) * 100.0).round() as u32),
            }),
            memory: platform::memory(),
            disk: disk(self.home.as_deref().unwrap_or(c"/")),
        }
    }
}

#[cfg(feature = "host")]
fn disk(path: &std::ffi::CStr) -> Option<Disk> {
    let (unit, blocks, free, available) = block_counts(path)?;
    // As `df` counts: the blocks reserved for the superuser are neither used
    // nor available to the user, so they leave the total.
    let used = blocks.saturating_sub(free).saturating_mul(unit);
    let total = used.saturating_add(available.saturating_mul(unit));
    (total > 0).then_some(Disk { used, total })
}

/// The block size and the total, free and user-available blocks of the file
/// system holding `path`.
#[cfg(all(feature = "host", not(target_os = "macos")))]
#[allow(
    clippy::useless_conversion,
    reason = "the field widths differ by platform"
)]
fn block_counts(path: &std::ffi::CStr) -> Option<(u64, u64, u64, u64)> {
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: the path is NUL-terminated and statvfs fills the buffer.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs succeeded, so the buffer is initialized.
    let stat = unsafe { stat.assume_init() };
    Some((
        u64::from(stat.f_frsize),
        u64::from(stat.f_blocks),
        u64::from(stat.f_bfree),
        u64::from(stat.f_bavail),
    ))
}

/// Darwin's `statvfs` reports 32-bit block counts, which a large volume
/// overflows; `statfs` counts in 64 bits.
#[cfg(all(feature = "host", target_os = "macos"))]
fn block_counts(path: &std::ffi::CStr) -> Option<(u64, u64, u64, u64)> {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: the path is NUL-terminated and statfs fills the buffer.
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statfs succeeded, so the buffer is initialized.
    let stat = unsafe { stat.assume_init() };
    Some((
        u64::from(stat.f_bsize),
        stat.f_blocks,
        stat.f_bfree,
        stat.f_bavail,
    ))
}

#[cfg(all(feature = "host", target_os = "linux"))]
mod platform {
    use super::Memory;

    /// Cumulative processor time, in the kernel's ticks, since boot.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct CpuTimes {
        busy: u64,
        total: u64,
    }

    impl CpuTimes {
        pub fn busy_since(&self, previous: &Self) -> Option<f64> {
            let total = self.total.checked_sub(previous.total)?;
            let busy = self.busy.checked_sub(previous.busy)?;
            (total > 0).then(|| busy as f64 / total as f64)
        }
    }

    pub fn cpu_times() -> Option<CpuTimes> {
        let text = read("/proc/stat")?;
        parse_cpu_times(&text)
    }

    /// The aggregate `cpu` line of `/proc/stat`. Guest time is already in
    /// user and nice time, so it is left out; idle and I/O wait are idle.
    pub fn parse_cpu_times(text: &str) -> Option<CpuTimes> {
        let line = text.lines().next()?.strip_prefix("cpu ")?;
        let fields: Vec<u64> = line
            .split_ascii_whitespace()
            .take(8)
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        if fields.len() < 4 {
            return None;
        }
        let total: u64 = fields.iter().sum();
        let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
        Some(CpuTimes {
            busy: total - idle,
            total,
        })
    }

    pub fn cores() -> Option<u16> {
        // SAFETY: sysconf has no preconditions.
        let online = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        u16::try_from(online).ok().filter(|cores| *cores > 0)
    }

    pub fn load_average() -> Option<f64> {
        read("/proc/loadavg")?
            .split_ascii_whitespace()
            .next()?
            .parse()
            .ok()
    }

    pub fn memory() -> Option<Memory> {
        parse_memory(&read("/proc/meminfo")?)
    }

    pub fn parse_memory(text: &str) -> Option<Memory> {
        let (mut total, mut available, mut swap_total, mut swap_free) = (None, None, None, None);
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let slot = match key {
                "MemTotal" => &mut total,
                "MemAvailable" => &mut available,
                "SwapTotal" => &mut swap_total,
                "SwapFree" => &mut swap_free,
                _ => continue,
            };
            *slot = value
                .trim()
                .strip_suffix(" kB")
                .and_then(|kib| kib.trim().parse::<u64>().ok())
                .map(|kib| kib.saturating_mul(1024));
        }
        let total = total?;
        let swap_total = swap_total.unwrap_or(0);
        Some(Memory {
            used: total.saturating_sub(available?),
            total,
            swap_used: swap_total.saturating_sub(swap_free.unwrap_or(swap_total)),
            swap_total,
        })
    }

    /// The small kernel files read here are generated on read; one bounded
    /// read keeps a sample to a few system calls.
    fn read(path: &str) -> Option<String> {
        use std::io::Read;
        let mut text = String::with_capacity(4096);
        std::fs::File::open(path)
            .ok()?
            .take(64 * 1024)
            .read_to_string(&mut text)
            .ok()?;
        Some(text)
    }
}

#[cfg(all(feature = "host", target_os = "macos"))]
mod platform {
    use super::Memory;

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct CpuTimes {
        busy: u64,
        total: u64,
    }

    impl CpuTimes {
        pub fn busy_since(&self, previous: &Self) -> Option<f64> {
            let total = self.total.checked_sub(previous.total)?;
            let busy = self.busy.checked_sub(previous.busy)?;
            (total > 0).then(|| busy as f64 / total as f64)
        }
    }

    /// The host's aggregate processor ticks, from the Mach host statistics.
    #[allow(
        deprecated,
        reason = "libc's Mach host calls are the dependency-free route"
    )]
    pub fn cpu_times() -> Option<CpuTimes> {
        // Each `mach_host_self` adds a reference to the host port that is
        // never released, so the port is asked for once.
        static HOST: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
        // SAFETY: mach_host_self has no preconditions.
        let host = *HOST.get_or_init(|| unsafe { libc::mach_host_self() });
        let mut info = std::mem::MaybeUninit::<libc::host_cpu_load_info>::zeroed();
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        // SAFETY: the buffer holds `count` integers of the requested flavor.
        let status = unsafe {
            libc::host_statistics(
                host,
                libc::HOST_CPU_LOAD_INFO,
                info.as_mut_ptr().cast(),
                &mut count,
            )
        };
        if status != libc::KERN_SUCCESS {
            return None;
        }
        // SAFETY: the call succeeded and filled the structure.
        let ticks = unsafe { info.assume_init() }.cpu_ticks.map(u64::from);
        let total: u64 = ticks.iter().sum();
        Some(CpuTimes {
            busy: total - ticks[libc::CPU_STATE_IDLE as usize],
            total,
        })
    }

    pub fn cores() -> Option<u16> {
        // SAFETY: sysconf has no preconditions.
        let online = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        u16::try_from(online).ok().filter(|cores| *cores > 0)
    }

    pub fn load_average() -> Option<f64> {
        let mut load = [0.0f64; 1];
        // SAFETY: the buffer holds the one figure asked for.
        (unsafe { libc::getloadavg(load.as_mut_ptr(), 1) } == 1).then_some(load[0])
    }

    /// Memory in use as Activity Monitor counts it: total less free,
    /// speculative, file-backed and purgeable pages, which the system hands
    /// back without swapping.
    pub fn memory() -> Option<Memory> {
        let total = sysctl_u64(c"hw.memsize")?;
        let page = sysctl_u64(c"vm.pagesize")?;
        let reclaimable = [
            c"vm.page_free_count",
            c"vm.page_speculative_count",
            c"vm.page_pageable_external_count",
            c"vm.page_purgeable_count",
        ]
        .into_iter()
        .map(|name| sysctl_u64(name).unwrap_or(0))
        .sum::<u64>()
        .saturating_mul(page);
        let mut swap = std::mem::MaybeUninit::<libc::xsw_usage>::zeroed();
        let mut length = std::mem::size_of::<libc::xsw_usage>();
        // SAFETY: the name is NUL-terminated and the buffer is as long as said.
        let swap = (unsafe {
            libc::sysctlbyname(
                c"vm.swapusage".as_ptr(),
                swap.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } == 0)
            // SAFETY: the call succeeded and filled the structure.
            .then(|| unsafe { swap.assume_init() });
        Some(Memory {
            used: total.saturating_sub(reclaimable),
            total,
            swap_used: swap.map_or(0, |swap| swap.xsu_used),
            swap_total: swap.map_or(0, |swap| swap.xsu_total),
        })
    }

    /// An unsigned sysctl of either width.
    fn sysctl_u64(name: &std::ffi::CStr) -> Option<u64> {
        let mut buffer = [0u8; 8];
        let mut length = buffer.len();
        // SAFETY: the name is NUL-terminated and the buffer is as long as said.
        if unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        match length {
            4 => Some(u64::from(u32::from_ne_bytes(buffer[..4].try_into().ok()?))),
            8 => Some(u64::from_ne_bytes(buffer)),
            _ => None,
        }
    }
}

#[cfg(all(feature = "host", not(any(target_os = "linux", target_os = "macos"))))]
mod platform {
    use super::Memory;

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct CpuTimes;

    impl CpuTimes {
        pub fn busy_since(&self, _previous: &Self) -> Option<f64> {
            None
        }
    }

    pub fn cpu_times() -> Option<CpuTimes> {
        None
    }

    pub fn cores() -> Option<u16> {
        None
    }

    pub fn load_average() -> Option<f64> {
        None
    }

    pub fn memory() -> Option<Memory> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages_round_and_refuse_an_empty_total() {
        assert_eq!(percent(1, 3), Some(33));
        assert_eq!(percent(2, 3), Some(67));
        assert_eq!(percent(5, 4), Some(100));
        assert_eq!(percent(0, 0), None);
        assert_eq!(percent(u64::MAX, u64::MAX), Some(100));
    }

    #[test]
    fn only_a_change_the_bar_shows_is_sent() {
        let reading = DeviceStats {
            cpu: Some(Cpu {
                percent: 12,
                cores: 16,
                load_centi: 190,
            }),
            memory: Some(Memory {
                used: 9_300_000_000,
                total: 16_000_000_000,
                swap_used: 0,
                swap_total: 0,
            }),
            disk: Some(Disk {
                used: 238_000_000_000,
                total: 512_000_000_000,
            }),
        };
        let with = |change: fn(&mut DeviceStats)| {
            let mut next = reading;
            change(&mut next);
            reading.superseded_by(&next)
        };
        assert!(!with(|next| next.memory.as_mut().unwrap().used += 4096));
        assert!(!with(|next| next.disk.as_mut().unwrap().used += 1_000_000));
        assert!(!with(|next| next.cpu.as_mut().unwrap().load_centi = 250));
        // A processor figure that only jitters by a point is held.
        assert!(!with(|next| next.cpu.as_mut().unwrap().percent = 13));
        assert!(with(|next| next.cpu.as_mut().unwrap().percent = 14));
        assert!(with(
            |next| next.memory.as_mut().unwrap().used = 10_000_000_000
        ));
        assert!(with(|next| next.disk = None));
        // Crossing into amber is sent even within the step.
        let mut warm = reading;
        warm.cpu.as_mut().unwrap().percent = 79;
        let mut hot = warm;
        hot.cpu.as_mut().unwrap().percent = 80;
        assert!(warm.superseded_by(&hot));
    }

    #[cfg(all(feature = "host", target_os = "linux"))]
    #[test]
    fn linux_counters_parse() {
        let stat = "cpu  100 5 50 800 20 0 5 0 0 0\ncpu0 1 2 3 4\n";
        let times = platform::parse_cpu_times(stat).unwrap();
        let later = platform::parse_cpu_times("cpu  150 5 70 860 20 0 5 0 0 0\n").unwrap();
        // 70 busy ticks of 130.
        assert!((later.busy_since(&times).unwrap() - 70.0 / 130.0).abs() < 1e-9);
        assert_eq!(
            times.busy_since(&later),
            None,
            "counters never run backwards"
        );
        assert_eq!(platform::parse_cpu_times("intr 1 2 3\n"), None);

        let memory = platform::parse_memory(
            "MemTotal:       16000000 kB\nMemFree:  1 kB\nMemAvailable:    6000000 kB\n\
             SwapTotal:       4000000 kB\nSwapFree:        3000000 kB\n",
        )
        .unwrap();
        assert_eq!(memory.total, 16_000_000 * 1024);
        assert_eq!(memory.used, 10_000_000 * 1024);
        assert_eq!(memory.swap_used, 1_000_000 * 1024);
        assert_eq!(memory.percent(), Some(63));
        assert_eq!(platform::parse_memory("MemTotal: 1 kB\n"), None);
    }

    #[cfg(feature = "host")]
    #[test]
    fn a_device_reads_itself() {
        let mut sampler = Sampler::new();
        let first = sampler.sample();
        assert_eq!(first.cpu, None, "processor use needs two readings");
        assert!(first.disk.is_some_and(|disk| disk.total > 0));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(first.memory.is_some_and(|memory| memory.total > 0));
    }
}
