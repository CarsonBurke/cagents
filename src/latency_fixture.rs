//! Opt-in synthetic adjacent-card focus workload. No user data enters reports.
use std::{fs, io, os::unix::fs::OpenOptionsExt, path::PathBuf};

use serde::Serialize;

use crate::{
    performance::{Action, InteractionLatency, Outcome, Sample},
    workspace::FocusDirection,
};

pub const CARD_COUNT: usize = 8;
pub const WARMUP_STEPS: usize = 32;
pub const MEASURED_STEPS: usize = 256;
pub const FIXTURE_VERSION: &str = "adjacent-card-focus-v1";
const SEED: u64 = 0x4341_4745_4e54_5338;
pub const INTERACTIVE_ENV: &str = "CAGENTS_NATIVE_UI_FIXTURE";
pub const SCALE_ENV: &str = "CAGENTS_NATIVE_UI_FIXTURE_SCALE";
pub const STATUS_BAR_ENV: &str = "CAGENTS_NATIVE_UI_FIXTURE_STATUS_BAR";

/// Both diagnostic modes are in-process UI workloads, never service callers.
/// Presence fails closed even when a malformed value is rejected at startup.
pub fn enabled() -> bool {
    std::env::var_os(INTERACTIVE_ENV).is_some()
        || std::env::var_os("CAGENTS_LATENCY_OUTPUT").is_some()
}

pub fn deny_services() -> io::Result<()> {
    if enabled() {
        #[cfg(test)]
        SERVICE_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Native UI fixtures cannot connect to or start a daemon",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
static SERVICE_ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn service_attempts() -> usize {
    SERVICE_ATTEMPTS.load(std::sync::atomic::Ordering::SeqCst)
}

#[derive(Debug)]
pub struct Fixture {
    output: Option<PathBuf>,
    random: u64,
    index: usize,
    issued: usize,
}

impl Fixture {
    pub fn from_env() -> Option<Self> {
        let interactive = std::env::var_os(INTERACTIVE_ENV);
        let output = std::env::var_os("CAGENTS_LATENCY_OUTPUT").map(PathBuf::from);
        if let Some(value) = interactive {
            assert_eq!(value, "1", "native UI fixture value must be 1");
            assert!(
                output.is_none(),
                "interactive fixtures do not produce latency reports"
            );
        } else {
            assert!(
                output.as_ref()?.is_absolute(),
                "latency output must be absolute"
            );
        }
        Some(Self {
            output,
            random: SEED,
            index: 0,
            issued: 0,
        })
    }

    pub fn automatic(&self) -> bool {
        self.output.is_some()
    }

    /// Appearance choices are intentionally finite and apply only to the
    /// interactive fixture. They never load or save a user's settings file.
    pub fn apply_interactive_appearance(&self, settings: &mut crate::settings::AppSettings) {
        if self.automatic() {
            return;
        }
        settings.ui_scale = match std::env::var(SCALE_ENV).as_deref() {
            Ok("0.8") => 0.8,
            Ok("1") | Err(std::env::VarError::NotPresent) => 1.0,
            _ => panic!("native fixture scale must be 0.8 or 1"),
        };
        settings.show_status_bar = match std::env::var(STATUS_BAR_ENV).as_deref() {
            Ok("off") => false,
            Ok("on") | Err(std::env::VarError::NotPresent) => true,
            _ => panic!("native fixture status bar must be off or on"),
        };
        settings.app_theme = crate::settings::AppTheme::Dark;
    }

    /// Caller starts at card zero and requests another step only after settlement.
    pub fn next_direction(&mut self) -> Option<FocusDirection> {
        if self.issued == WARMUP_STEPS + MEASURED_STEPS {
            return None;
        }
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        let right = self.index == 0 || (self.index + 1 < CARD_COUNT && self.random & 1 != 0);
        self.index = if right {
            self.index + 1
        } else {
            self.index - 1
        };
        self.issued += 1;
        Some(if right {
            FocusDirection::Right
        } else {
            FocusDirection::Left
        })
    }

    pub fn finish(&self, recorder: &mut InteractionLatency, scale: f64) -> io::Result<()> {
        let output = self.output.as_ref().ok_or_else(|| {
            io::Error::other("interactive UI fixtures do not measure redraw latency")
        })?;
        recorder.abandon_pending();
        if cfg!(debug_assertions) {
            return Err(io::Error::other("latency fixture requires a release build"));
        }
        if self.issued != WARMUP_STEPS + MEASURED_STEPS
            || recorder.settled_count() != self.issued as u64
            || !scale.is_finite()
            || scale <= 0.0
        {
            return Err(io::Error::other(
                "incomplete latency fixture or invalid scale",
            ));
        }
        let all: Vec<_> = recorder.samples().copied().collect();
        if all.len() != self.issued
            || all.iter().enumerate().any(|(i, sample)| {
                sample.sequence != i as u64 + 1 || sample.action != Action::CardFocus
            })
        {
            return Err(io::Error::other(
                "fixture generation sequence was contaminated",
            ));
        }
        let samples = all[WARMUP_STEPS..].to_vec();
        let summary = Summary::from_samples(&samples);
        let report = Report {
            schema_version: 1,
            metric: "redraw_scheduling",
            manifest: Manifest::collect(scale)?,
            samples,
            summary,
        };
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(output)?;
        serde_json::to_writer_pretty(&mut file, &report)?;
        file.sync_all()
    }
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    metric: &'static str,
    manifest: Manifest,
    samples: Vec<Sample>,
    summary: Summary,
}

#[derive(Debug, Serialize)]
struct Summary {
    action: Action,
    redraw_scheduled: usize,
    abandoned: usize,
    median_ns: Option<u64>,
    p95_ns: Option<u64>,
    p99_ns: Option<u64>,
    worst_ns: Option<u64>,
}

impl Summary {
    fn from_samples(samples: &[Sample]) -> Self {
        let mut sorted: Vec<_> = samples
            .iter()
            .filter_map(|sample| sample.redraw_scheduling_ns)
            .collect();
        sorted.sort_unstable();
        let percentile = |percent: usize| {
            (!sorted.is_empty()).then(|| sorted[((sorted.len() - 1) * percent).div_ceil(100)])
        };
        Self {
            action: Action::CardFocus,
            redraw_scheduled: sorted.len(),
            abandoned: samples
                .iter()
                .filter(|sample| sample.outcome == Outcome::Abandoned)
                .count(),
            median_ns: percentile(50),
            p95_ns: percentile(95),
            p99_ns: percentile(99),
            worst_ns: sorted.last().copied(),
        }
    }
}

#[derive(Debug, Serialize)]
struct Manifest {
    reference: &'static str,
    revision: String,
    working_tree_dirty: bool,
    fixture_version: &'static str,
    seed: &'static str,
    build_mode: &'static str,
    run: u8,
    session: &'static str,
    compositor: &'static str,
    scale: f64,
    hardware: Hardware,
    budgets_gating: bool,
    provisional_p95_ns: u64,
    provisional_p99_ns: u64,
}

impl Manifest {
    fn collect(scale: f64) -> io::Result<Self> {
        let revision = std::env::var("CAGENTS_LATENCY_REVISION").unwrap_or_default();
        if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(io::Error::other("fixture requires a 40-digit revision"));
        }
        let run = std::env::var("CAGENTS_LATENCY_RUN")
            .ok()
            .and_then(|run| run.parse::<u8>().ok())
            .filter(|run| (1..=5).contains(run))
            .ok_or_else(|| io::Error::other("invalid fixture run"))?;
        let session = match std::env::var("XDG_SESSION_TYPE").as_deref() {
            Ok("wayland") => "wayland",
            Ok("x11") => "x11",
            _ => "unknown",
        };
        // Exact allowlist: never echo arbitrary environment strings into artifacts.
        let compositor = match std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "niri" => "niri",
            "sway" => "sway",
            "hyprland" => "hyprland",
            "cosmic" => "cosmic-comp",
            "gnome" | "ubuntu:gnome" => "mutter",
            "kde" => "kwin",
            _ => "unknown",
        };
        Ok(Self {
            reference: "unnamed-local-non-gating",
            revision,
            fixture_version: FIXTURE_VERSION,
            working_tree_dirty: std::env::var("CAGENTS_LATENCY_DIRTY").as_deref() != Ok("false"),
            seed: "0x434147454e545338",
            build_mode: "release-browserless",
            run,
            session,
            compositor,
            scale,
            hardware: Hardware::collect(),
            budgets_gating: false,
            provisional_p95_ns: 50_000_000,
            provisional_p99_ns: 100_000_000,
        })
    }
}

#[derive(Debug, Serialize)]
struct Hardware {
    architecture: &'static str,
    logical_cpus: usize,
    cpu_vendor: &'static str,
    cpu_family: Option<u32>,
    cpu_model: Option<u32>,
    memory_bytes: Option<u64>,
    gpus: Vec<Gpu>,
}

#[derive(Debug, Serialize)]
struct Gpu {
    vendor: u32,
    device: u32,
}

impl Hardware {
    fn collect() -> Self {
        let cpu = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let field = |name: &str| {
            cpu.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == name).then_some(value.trim())
            })
        };
        let cpu_vendor = match field("vendor_id") {
            Some("AuthenticAMD") => "amd",
            Some("GenuineIntel") => "intel",
            _ => "other",
        };
        let memory_bytes = fs::read_to_string("/proc/meminfo").ok().and_then(|text| {
            text.lines()
                .find_map(|line| {
                    line.strip_prefix("MemTotal:")?
                        .split_whitespace()
                        .next()?
                        .parse::<u64>()
                        .ok()
                })
                .and_then(|kb| kb.checked_mul(1024))
        });
        let mut gpus = Vec::new();
        if let Ok(devices) = fs::read_dir("/sys/bus/pci/devices") {
            for device in devices.flatten() {
                let number = |name| {
                    fs::read_to_string(device.path().join(name))
                        .ok()
                        .and_then(|text| {
                            u32::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
                        })
                };
                if number("class").is_some_and(|class| class >> 16 == 3)
                    && let (Some(vendor), Some(device)) = (number("vendor"), number("device"))
                {
                    gpus.push(Gpu { vendor, device });
                }
            }
        }
        gpus.sort_by_key(|gpu| (gpu.vendor, gpu.device));
        Self {
            architecture: std::env::consts::ARCH,
            logical_cpus: std::thread::available_parallelism().map_or(0, usize::from),
            cpu_vendor,
            cpu_family: field("cpu family").and_then(|value| value.parse().ok()),
            cpu_model: field("model").and_then(|value| value.parse().ok()),
            memory_bytes,
            gpus,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_seed_workload_is_reproducible_adjacent_and_bounded() {
        let new = || Fixture {
            output: Some(PathBuf::new()),
            random: SEED,
            index: 0,
            issued: 0,
        };
        let (mut first, mut second) = (new(), new());
        let mut position = 0usize;
        let mut visited = [false; CARD_COUNT];
        for _ in 0..WARMUP_STEPS + MEASURED_STEPS {
            let direction = first.next_direction().unwrap();
            assert_eq!(Some(direction), second.next_direction());
            position = match direction {
                FocusDirection::Left => position.checked_sub(1).unwrap(),
                FocusDirection::Right => position + 1,
            };
            assert!(position < CARD_COUNT);
            visited[position] = true;
        }
        assert!(visited.into_iter().all(|value| value));
        assert_eq!(first.next_direction(), None);
        assert_eq!(second.next_direction(), None);
    }

    #[test]
    fn abandoned_generations_are_not_zero_latency_samples() {
        let samples = [
            Sample {
                sequence: 1,
                action: Action::CardFocus,
                outcome: Outcome::Abandoned,
                redraw_scheduling_ns: None,
            },
            Sample {
                sequence: 2,
                action: Action::CardFocus,
                outcome: Outcome::RedrawScheduled,
                redraw_scheduling_ns: Some(12),
            },
        ];
        let summary = Summary::from_samples(&samples);
        assert_eq!(summary.redraw_scheduled, 1);
        assert_eq!(summary.abandoned, 1);
        assert_eq!(summary.median_ns, Some(12));
    }
}
