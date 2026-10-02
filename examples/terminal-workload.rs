//! Real PTY/backend workload with synthetic consumer demand, not a presentation benchmark.
//! Run: cargo run --release --locked --no-default-features --features host --example terminal-workload -- all
//! `--emit MODE` is also a standalone output generator usable inside the native UI.

#[cfg(not(feature = "host"))]
fn main() {
    eprintln!("terminal-workload requires --no-default-features --features host");
    std::process::exit(2);
}

#[cfg(feature = "host")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use cagents::{
        control::ControlClient,
        model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
        terminal::{HostedTerminalHandle, TerminalConfig, TerminalEvent, TerminalSize},
    };
    use std::{
        collections::BTreeMap,
        io::{self, BufRead, Read, Write},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };
    const STANDARD_LINE: &[u8] =
        b"01234567890123456789012345678901234567890123456789012345678901234567890123456789\r\n";
    const BLOCK_LINES: usize = 512;
    const STANDARD_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let socket = if let Some(index) = args.iter().position(|arg| arg == "--socket") {
        if index + 1 >= args.len() {
            return Err("--socket requires a path".into());
        }
        let path = std::path::PathBuf::from(args.remove(index + 1));
        args.remove(index);
        Some(path)
    } else {
        None
    };
    if args.first().is_some_and(|arg| arg == "--summarize-trace") {
        let path = args.get(1).ok_or("--summarize-trace requires a log path")?;
        let mut stages: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for line in io::BufReader::new(std::fs::File::open(path)?).lines() {
            let line = line?;
            if !line.contains("cagents::terminal_pipeline") {
                continue;
            }
            let mut stage = None;
            let mut elapsed = None;
            for field in line.split_whitespace() {
                if let Some(value) = field.strip_prefix("stage=") {
                    stage = Some(value.trim_matches('"').to_owned());
                }
                if let Some(value) = field.strip_prefix("elapsed_ns=") {
                    elapsed = value.parse::<u64>().ok();
                }
            }
            if let (Some(stage), Some(elapsed)) = (stage, elapsed) {
                stages.entry(stage).or_default().push(elapsed);
            }
        }
        if stages.is_empty() {
            return Err("no terminal stage records found; disable ANSI and enable cagents::terminal_pipeline=trace".into());
        }
        let missing: Vec<_> = [
            "parse",
            "capture",
            "ipc_encode",
            "ipc_decode",
            "snapshot_apply",
            "ui_apply",
            "draw",
        ]
        .into_iter()
        .filter(|stage| !stages.contains_key(*stage))
        .collect();
        let stages: BTreeMap<_, _> = stages.into_iter().map(|(stage, mut samples)| {
            samples.sort_unstable();
            let percentile = |p: usize| samples[(samples.len() * p).div_ceil(100).saturating_sub(1)];
            (stage, serde_json::json!({ "samples": samples.len(), "total_ns": samples.iter().map(|sample| u128::from(*sample)).sum::<u128>(),
                "p50_ns": percentile(50), "p95_ns": percentile(95), "p99_ns": percentile(99) }))
        }).collect();
        println!(
            "{}",
            serde_json::json!({ "schema": "cagents.terminal-stage-summary.v1", "stages": stages,
            "missing_stages": missing, "not_measured": ["compositor_presentation", "physical_scanout", "input_to_present"],
            "interpretation": "stage totals identify instrumented CPU/wire work, not additive end-to-end latency; concurrent stages overlap" })
        );
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--emit") {
        let mode = args.get(1).map(String::as_str).unwrap_or("sustained");
        let mut out = io::stdout().lock();
        if mode == "visual-input" {
            let _raw = RawInput::start()?;
            write!(
                out,
                "\x1b[?1049h\x1b[?25l\x1b[48;2;193;17;37m\x1b[2J\x1b[HRED"
            )?;
            out.flush()?;
            let mut input = io::stdin().lock();
            let mut byte = [0];
            while input.read_exact(&mut byte).is_ok() {
                let (rgb, token) = match byte[0] {
                    b'r' => ("193;17;37", "RED"),
                    b'g' => ("23;181;71", "GREEN"),
                    b'b' => ("31;67;211", "BLUE"),
                    b'q' | 3 => break,
                    _ => continue,
                };
                write!(out, "\x1b[48;2;{rgb}m\x1b[2J\x1b[H{token}")?;
                out.flush()?;
            }
            write!(out, "\x1b[0m\x1b[?25h\x1b[?1049l")?;
        } else if mode == "idle" {
            thread::sleep(Duration::from_secs(3));
        } else if mode == "input" {
            for line in io::stdin().lock().lines() {
                let line = line?;
                if line == "exit" {
                    break;
                }
                write!(out, "\x1b[H\x1b[2KPROBE:{line}\r\n")?;
                out.flush()?;
            }
        } else if mode == "tui" {
            write!(out, "\x1b[?1049h\x1b[?25l")?;
            for frame in 0..180 {
                write!(out, "\x1b[Hframe {frame:06}\r\n")?;
                for row in 0..22 {
                    writeln!(
                        out,
                        "\x1b[{};1H\x1b[38;5;{}m{:078}",
                        row + 2,
                        (frame + row) % 255,
                        frame + row
                    )?;
                }
                out.flush()?;
                thread::sleep(Duration::from_millis(16));
            }
            write!(out, "\x1b[0m\x1b[?25h\x1b[?1049l")?;
        } else {
            let line: &[u8] = if mode == "short-lines" {
                b"x\r\n"
            } else {
                STANDARD_LINE
            };
            let block: Vec<u8> = line
                .iter()
                .copied()
                .cycle()
                .take(line.len() * BLOCK_LINES)
                .collect();
            let target = if mode == "short-lines" {
                32 * 1024 * 1024
            } else {
                STANDARD_OUTPUT_BYTES
            };
            for index in 0..target / block.len() {
                out.write_all(&block)?;
                if matches!(mode, "burst" | "large") && index % 8 == 0 {
                    out.flush()?;
                    thread::sleep(Duration::from_millis(80));
                }
            }
        }
        write!(out, "\r\nWORKLOAD_COMPLETE\r\n")?;
        out.flush()?;
        return Ok(());
    }
    if std::env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(io::stderr)
            .init();
    }
    let selected = args.first().map(String::as_str).unwrap_or("all");
    let modes: Vec<&str> = if selected == "all" {
        vec![
            "idle",
            "burst",
            "short-lines",
            "sustained",
            "tui",
            "large",
            "multiple",
            "input",
            "input-multiple",
        ]
    } else {
        vec![selected]
    };
    let mut control = socket
        .as_ref()
        .map(|socket| ControlClient::connect(socket))
        .transpose()?;
    for mode in modes {
        if ![
            "idle",
            "burst",
            "short-lines",
            "sustained",
            "tui",
            "large",
            "multiple",
            "input",
            "input-multiple",
        ]
        .contains(&mode)
        {
            return Err(format!("unknown workload {mode}").into());
        }
        let input_mode = mode.starts_with("input");
        let count = if matches!(mode, "multiple" | "input-multiple") {
            4
        } else {
            1
        };
        let frame = Duration::from_nanos(1_000_000_000 / 60);
        let start = Instant::now();
        let cpu_start = cpu_ns();
        let wakes = Arc::new(AtomicU64::new(0));
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        let mut handles = Vec::new();
        for index in 0..count {
            let emitted_mode = if mode == "input-multiple" {
                if index == 0 { "input" } else { "sustained" }
            } else {
                mode
            };
            let recipe = LaunchRecipe {
                unset_environment: Default::default(),
                program: std::env::current_exe()?.to_string_lossy().into_owned(),
                arguments: vec!["--emit".into(), emitted_mode.into()],
                working_directory: std::env::current_dir()?,
                environment: BTreeMap::new(),
            };
            let session = TerminalSession {
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                id: TerminalId::new(),
                launch: recipe.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe,
                    last_exit: None,
                },
            };
            let wake_counter = Arc::clone(&wakes);
            let wake_tx = wake_tx.clone();
            let config = TerminalConfig {
                columns: if mode == "large" { 240 } else { 80 },
                rows: if mode == "large" { 100 } else { 24 },
                ..TerminalConfig::default()
            };
            let callback = move |_| {
                wake_counter.fetch_add(1, Ordering::Relaxed);
                let _ = wake_tx.try_send(());
            };
            handles.push(
                if let (Some(socket), Some(control)) = (&socket, &mut control) {
                    BenchTerminal::Hosted(HostedTerminalHandle::attach_or_spawn_at(
                        socket,
                        session,
                        config,
                        Some(control.proof().clone()),
                        callback,
                    )?)
                } else {
                    BenchTerminal::Local(cagents::terminal::TerminalHandle::spawn(
                        session, config, callback,
                    )?)
                },
            );
        }
        let events: Vec<_> = handles.iter().map(BenchTerminal::events).collect();
        let mut exited = vec![false; count];
        let mut events_closed = vec![false; count];
        let mut finished = vec![false; count];
        let mut complete = vec![false; count];
        let mut generations = vec![0; count];
        let mut observations = 0u64;
        let mut resized = false;
        let mut latencies = Vec::new();
        let mut probe = 0;
        let mut sent_at = Instant::now();
        let mut pending = true;
        let mut next_frame = Instant::now();
        if input_mode {
            handles[0].input(b"0\n".to_vec())?;
        }
        while finished.iter().any(|finished| !finished) {
            if start.elapsed() > Duration::from_secs(90) {
                return Err(format!("{mode} exceeded 90s deadline").into());
            }
            // No idle polling: channels arm a single synthetic presentation
            // opportunity. The 90s timeout is a workload failure watchdog.
            let mut select = crossbeam_channel::Select::new();
            let wake_index = select.recv(&wake_rx);
            let registered: Vec<_> = events
                .iter()
                .enumerate()
                .filter(|(index, _)| !events_closed[*index])
                .map(|(index, events)| (select.recv(events), index))
                .collect();
            let timeout = if pending {
                next_frame.saturating_duration_since(Instant::now())
            } else {
                Duration::from_secs(90).saturating_sub(start.elapsed())
            };
            if let Ok(operation) = select.select_timeout(timeout) {
                if operation.index() == wake_index {
                    let _ = operation.recv(&wake_rx);
                    pending = true;
                } else {
                    let index = registered
                        .iter()
                        .find(|(registered, _)| *registered == operation.index())
                        .unwrap()
                        .1;
                    match operation.recv(&events[index]) {
                        Ok(TerminalEvent::Error { operation, message }) => {
                            return Err(format!("{operation}: {message}").into());
                        }
                        Ok(TerminalEvent::Exited { .. }) => exited[index] = true,
                        Ok(_) => {}
                        Err(_) => {
                            events_closed[index] = true;
                            exited[index] = true;
                        }
                    }
                    pending = true;
                }
            }
            if !pending || Instant::now() < next_frame {
                continue;
            }
            pending = false;
            next_frame = Instant::now() + frame;
            if mode == "large" && !resized && start.elapsed() > Duration::from_millis(120) {
                handles[0].resize(TerminalSize {
                    columns: 160,
                    rows: 60,
                    cell_width_px: 9,
                    cell_height_px: 18,
                })?;
                resized = true;
            }
            for (index, handle) in handles.iter().enumerate() {
                // Clearing the hosted semantic edge is independent of draw
                // credit, and does not request another snapshot.
                let _ = handle.events();
                let snapshot = handle.snapshot();
                finished[index] = exited[index] && complete[index];
                if snapshot.generation != generations[index] {
                    let text: String = snapshot
                        .lines
                        .iter()
                        .flat_map(|row| row.cells.iter())
                        .map(|cell| cell.text.as_str())
                        .collect();
                    complete[index] = text.contains("WORKLOAD_COMPLETE");
                    finished[index] = exited[index] && complete[index];
                    observations += 1;
                    generations[index] = snapshot.generation;
                    if input_mode && index == 0 && probe < 30 {
                        if text.contains(&format!("PROBE:{probe}")) {
                            latencies.push(sent_at.elapsed().as_nanos() as u64);
                            probe += 1;
                            sent_at = Instant::now();
                            handle.input(if probe == 30 {
                                b"exit\n".to_vec()
                            } else {
                                format!("{probe}\n").into_bytes()
                            })?;
                        }
                    }
                    handle.acknowledge_redraw(snapshot.generation);
                }
            }
        }
        latencies.sort_unstable();
        let percentile = |numerator: usize| {
            latencies
                .get(
                    (latencies.len() * numerator)
                        .div_ceil(100)
                        .saturating_sub(1),
                )
                .copied()
        };
        let wall_ns = start.elapsed().as_nanos();
        let generations: u64 = handles.iter().map(BenchTerminal::generation).sum();
        let sustained_bytes = matches!(mode, "sustained" | "multiple").then(|| {
            let block_bytes = STANDARD_LINE.len() * BLOCK_LINES;
            (STANDARD_OUTPUT_BYTES / block_bytes * block_bytes * count) as u64
        });
        let sustained_budget_pass = sustained_bytes
            .map(|bytes| u128::from(bytes) * 1_000_000_000 >= 50 * 1024 * 1024 * wall_ns);
        let batches: u64 = if let Some(control) = &mut control {
            let ids: Vec<_> = handles.iter().filter_map(BenchTerminal::id).collect();
            let batches = control
                .list()?
                .iter()
                .filter(|terminal| ids.contains(&terminal.session.id))
                .filter_map(|terminal| terminal.activity)
                .map(|fact| fact.sequence)
                .sum();
            batches
        } else {
            handles
                .iter()
                .filter_map(BenchTerminal::activity)
                .map(|fact| fact.sequence)
                .sum()
        };
        println!(
            "{}",
            serde_json::json!({
                "schema": "cagents.terminal-workload.v1", "workload": mode, "profile": if cfg!(debug_assertions) { "unoptimized-or-dev" } else { "optimized" },
                "terminals": count, "consumer": "synthetic_60hz_snapshot_ack", "transport": if socket.is_some() { "daemon_ipc" } else { "in_process" }, "wall_ns": wall_ns,
                "consumer_process_cpu_ns": cpu_ns().saturating_sub(cpu_start), "output_batches": batches, "resize_exercised": resized,
                "snapshot_generations": generations, "observed_snapshots": observations, "wake_callbacks": wakes.load(Ordering::Relaxed),
                "terminal_ids": handles.iter().filter_map(BenchTerminal::id).map(|id| id.to_string()).collect::<Vec<_>>(),
                "input_samples": latencies.len(), "input_to_snapshot_p50_ns": percentile(50), "input_to_snapshot_p95_ns": percentile(95), "input_to_snapshot_p99_ns": percentile(99),
                "sustained_output_bytes": sustained_bytes,
                "sustained_mib_per_second": sustained_bytes.map(|bytes| bytes as f64 * 1e9 / wall_ns as f64 / (1024.0 * 1024.0)),
                "sustained_budget_pass": sustained_budget_pass,
                "short_lines_budget_pass": (mode == "short-lines").then_some(wall_ns <= 5_000_000_000),
                "budgets": { "input_to_snapshot_p95_ns": 50_000_000, "input_to_snapshot_p99_ns": 100_000_000, "sustained_min_mib_per_second": 50, "short_lines_max_wall_ns": 5_000_000_000u64, "capture_bound": "one per returned generation credit plus initial/reveal/final EOF capture", "idle_continuous_redraws": 0 },
                "input_budget_pass": percentile(95).zip(percentile(99)).map(|(p95,p99)| p95 <= 50_000_000 && p99 <= 100_000_000),
                "stage_timings": "RUST_LOG=cagents::terminal_pipeline=trace writes parse/capture elapsed_ns to stderr; trace overhead is not a release throughput claim",
                "not_measured": if socket.is_some() { vec!["ui_apply", "iced_draw", "compositor_presentation", "input_to_present", "daemon_process_cpu", "child_process_cpu"] } else { vec!["daemon_ipc", "ui_apply", "iced_draw", "compositor_presentation", "input_to_present", "child_process_cpu"] }
            })
        );
    }
    Ok(())
}

#[cfg(feature = "host")]
fn cpu_ns() -> u64 {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime initializes the writable timespec on success.
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, time.as_mut_ptr()) } != 0 {
        return 0;
    }
    let time = unsafe { time.assume_init() };
    (time.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(time.tv_nsec as u64)
}

#[cfg(feature = "host")]
enum BenchTerminal {
    Local(cagents::terminal::TerminalHandle),
    Hosted(cagents::terminal::HostedTerminalHandle),
}

#[cfg(feature = "host")]
impl BenchTerminal {
    fn id(&self) -> Option<cagents::model::TerminalId> {
        match self {
            Self::Local(_) => None,
            Self::Hosted(handle) => Some(handle.terminal_id()),
        }
    }
    fn snapshot(&self) -> std::sync::Arc<cagents::terminal::RenderSnapshot> {
        match self {
            Self::Local(handle) => handle.snapshot(),
            Self::Hosted(handle) => handle.snapshot(),
        }
    }
    fn generation(&self) -> u64 {
        match self {
            Self::Local(handle) => handle.generation(),
            Self::Hosted(handle) => handle.generation(),
        }
    }
    fn activity(&self) -> Option<cagents::model::TerminalOutputActivity> {
        match self {
            Self::Local(handle) => handle.activity(),
            Self::Hosted(handle) => handle.activity(),
        }
    }
    fn events(&self) -> crossbeam_channel::Receiver<cagents::terminal::TerminalEvent> {
        match self {
            Self::Local(handle) => handle.events(),
            Self::Hosted(handle) => handle.events(),
        }
    }
    fn acknowledge_redraw(&self, generation: u64) {
        match self {
            Self::Local(handle) => handle.acknowledge_redraw(generation),
            Self::Hosted(handle) => handle.acknowledge_redraw(generation),
        }
    }
    fn input(&self, bytes: Vec<u8>) -> Result<(), cagents::terminal::TerminalSendError> {
        match self {
            Self::Local(handle) => handle.input(bytes),
            Self::Hosted(handle) => handle.input(bytes),
        }
    }
    fn resize(
        &self,
        size: cagents::terminal::TerminalSize,
    ) -> Result<(), cagents::terminal::TerminalSendError> {
        match self {
            Self::Local(handle) => handle.resize(size),
            Self::Hosted(handle) => handle.resize(size),
        }
    }
}

#[cfg(feature = "host")]
struct RawInput(libc::termios);

#[cfg(feature = "host")]
impl RawInput {
    fn start() -> std::io::Result<Self> {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: stdin is an existing descriptor and original is writable.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        // SAFETY: raw is initialized; tcsetattr borrows it only for this call.
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(original))
    }
}

#[cfg(feature = "host")]
impl Drop for RawInput {
    fn drop(&mut self) {
        // SAFETY: the saved settings remain initialized for the lifetime of self.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}
