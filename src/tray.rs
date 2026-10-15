mod service {
    use std::{
        os::unix::process::CommandExt,
        process::{Command, Output, Stdio},
        sync::{Arc, LazyLock},
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use ksni::{TrayMethods, menu::StandardItem};
    use tokio::sync::{mpsc, oneshot};

    use crate::{
        control::{ControlClient, StopReport, shutdown_outcome_summary},
        instance,
        terminal_ipc::{ShutdownOutcome, daemon_socket_path},
    };

    /// How long an action's result stays in place of the live status.
    const RESULT_TTL: Duration = Duration::from_secs(6);
    /// A failure stays long enough to be found on a later look at the menu.
    const FAILURE_TTL: Duration = Duration::from_secs(60);
    /// How often the agent counts are read from the daemon.
    const TALLY_INTERVAL: Duration = Duration::from_secs(3);

    /// Hosted agents by what they are doing, for the tray's status lines.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct AgentTally {
        pub working: usize,
        pub waiting: usize,
    }

    /// The exact hosted session a waiting menu item activates.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct WaitingAgent {
        pub session: crate::model::TerminalId,
        pub label: String,
    }

    impl WaitingAgent {
        pub(crate) fn new(
            session: crate::model::TerminalId,
            provider: crate::agent::Provider,
            title: Option<&str>,
            directory: &std::path::Path,
        ) -> Self {
            let bounded = |text: &str| -> String {
                text.chars()
                    .take(256)
                    .filter(|ch| !ch.is_control())
                    .take(100)
                    .collect()
            };
            let title = title.map(bounded).filter(|title| !title.trim().is_empty());
            let text = title.unwrap_or_else(|| {
                bounded(
                    &directory
                        .file_name()
                        .unwrap_or(directory.as_os_str())
                        .to_string_lossy(),
                )
            });
            Self {
                session,
                label: format!("{} · {text}", provider.label()),
            }
        }
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct TraySnapshot {
        pub tally: AgentTally,
        pub waiting_agents: Vec<WaitingAgent>,
    }

    impl AgentTally {
        /// "2 agents working · 1 waiting for you", or `None` when idle.
        fn summary(self) -> Option<String> {
            let agents = |count: usize| match count {
                1 => "1 agent".to_owned(),
                count => format!("{count} agents"),
            };
            match (self.working, self.waiting) {
                (0, 0) => None,
                (working, 0) => Some(format!("{} working", agents(working))),
                (0, waiting) => Some(format!("{} waiting for you", agents(waiting))),
                (working, waiting) => Some(format!(
                    "{} working · {waiting} waiting for you",
                    agents(working)
                )),
            }
        }
    }

    /// What a finished tray action reports.
    type ActionResult = Result<String, String>;

    /// An action's result, shown in place of the live status until `until`.
    struct Outcome {
        text: String,
        until: Instant,
    }

    static ICON: LazyLock<ksni::Icon> = LazyLock::new(|| {
        let mut image = image::load_from_memory_with_format(
            include_bytes!("../packaging/dev.cagents.Cagents.png"),
            image::ImageFormat::Png,
        )
        .expect("embedded tray icon must be a valid PNG")
        .into_rgba8();
        if instance::IS_DEVELOPMENT {
            // The development and installed daemons can run together. Mark the
            // development icon so the two tray actions remain easy to tell apart.
            let (width, height) = image.dimensions();
            let radius = (width.min(height) / 8) as i32;
            let center = (width as i32 - radius - 4, height as i32 - radius - 4);
            for (x, y, pixel) in image.enumerate_pixels_mut() {
                let distance = (x as i32 - center.0).pow(2) + (y as i32 - center.1).pow(2);
                if distance <= radius.pow(2) {
                    *pixel = if distance > (radius - 3).pow(2) {
                        image::Rgba([255, 255, 255, 255])
                    } else {
                        image::Rgba([241, 139, 36, 255])
                    };
                }
            }
        }
        let (width, height) = image.dimensions();
        let mut data = image.into_raw();
        for pixel in data.chunks_exact_mut(4) {
            pixel.rotate_right(1);
        }
        ksni::Icon {
            width: width as i32,
            height: height as i32,
            data,
        }
    });

    static ATTENTION_ICON: LazyLock<ksni::Icon> = LazyLock::new(|| {
        let mut icon = ICON.clone();
        let radius = (icon.width.min(icon.height) / 8).max(1);
        let center = (icon.width - radius - 4, radius + 4);
        for y in 0..icon.height {
            for x in 0..icon.width {
                let distance = (x - center.0).pow(2) + (y - center.1).pow(2);
                if distance <= radius.pow(2) {
                    let index = ((y * icon.width + x) * 4) as usize;
                    icon.data[index..index + 4].copy_from_slice(
                        if distance > (radius - 3).max(0).pow(2) {
                            &[255, 255, 255, 255]
                        } else {
                            &[255, 245, 166, 35]
                        },
                    );
                }
            }
        }
        icon
    });

    type FocusAgent = Arc<dyn Fn(crate::model::TerminalId) -> ActionResult + Send + Sync>;

    struct DaemonTray {
        /// The action in flight, described; actions are refused meanwhile.
        pending: Option<String>,
        outcome: Option<Outcome>,
        tally: AgentTally,
        waiting_agents: Vec<WaitingAgent>,
        focus_agent: FocusAgent,
        updates: mpsc::UnboundedSender<ActionResult>,
    }

    impl DaemonTray {
        fn new(updates: mpsc::UnboundedSender<ActionResult>) -> Self {
            Self {
                pending: None,
                outcome: None,
                tally: AgentTally::default(),
                waiting_agents: Vec::new(),
                focus_agent: Arc::new(|_| Err("The hosted agent is unavailable".into())),
                updates,
            }
        }

        fn run_action(&mut self, stop: bool) {
            let label = if stop {
                "Checking for open terminals…".into()
            } else {
                format!("Opening {}…", instance::APPLICATION_NAME)
            };
            self.begin_action(label, move || {
                if stop {
                    stop_from_tray()
                } else {
                    open_from_tray()
                }
            });
        }

        fn run_agent(&mut self, session: crate::model::TerminalId) {
            let focus = Arc::clone(&self.focus_agent);
            self.begin_action("Opening waiting agent…".into(), move || focus(session));
        }

        fn begin_action(
            &mut self,
            label: String,
            action: impl FnOnce() -> ActionResult + Send + 'static,
        ) {
            if self.pending.is_some() {
                return;
            }
            self.outcome = None;
            self.pending = Some(label);
            let updates = self.updates.clone();
            if let Err(error) = thread::Builder::new()
                .name("cagents-tray-action".into())
                .spawn(move || {
                    let _ = updates.send(action());
                })
            {
                self.pending = None;
                self.finish(
                    Err(format!("Could not start the action: {error}")),
                    Instant::now(),
                );
            }
        }

        /// Shows a finished action's result until it goes stale.
        fn finish(&mut self, result: ActionResult, now: Instant) -> Duration {
            self.pending = None;
            let (text, ttl) = match result {
                Ok(text) => (text, RESULT_TTL),
                Err(text) => (text, FAILURE_TTL),
            };
            self.outcome = Some(Outcome {
                text,
                until: now + ttl,
            });
            ttl
        }

        /// Drops a result that has been shown for long enough.
        fn expire(&mut self, now: Instant) {
            if self
                .outcome
                .as_ref()
                .is_some_and(|outcome| now >= outcome.until)
            {
                self.outcome = None;
            }
        }

        /// The first menu line: an action in flight, its result while it is
        /// fresh, and otherwise that the daemon is running.
        fn status(&self) -> String {
            if let Some(pending) = &self.pending {
                return pending.clone();
            }
            match &self.outcome {
                Some(outcome) => outcome.text.clone(),
                None => "Daemon running".into(),
            }
        }

        /// The tooltip reads like the menu's header: the status, so a fresh
        /// failure is not hidden behind busy agents, then the agent counts.
        fn tool_tip_description(&self) -> String {
            match self.tally.summary() {
                Some(agents) => format!("{}\n{agents}", self.status()),
                None => self.status(),
            }
        }
    }

    fn open_from_tray() -> ActionResult {
        run_child("cagents", &["ui".into()])
            .map(|_| format!("Opened {}", instance::APPLICATION_NAME))
            .map_err(|error| format!("Could not open {}: {error}", instance::APPLICATION_NAME))
    }

    fn run_child(name: &str, arguments: &[String]) -> Result<String, String> {
        let output = spawn_child(name, arguments)?;
        child_outcome(output.status.success(), &output.stdout, &output.stderr)
    }

    fn spawn_child(name: &str, arguments: &[String]) -> Result<Output, String> {
        let mut executable = std::env::current_exe().map_err(|error| error.to_string())?;
        executable.set_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        Command::new(&executable)
            .args(arguments)
            .stdin(Stdio::null())
            // The tray lives in the daemon, which inherits the launcher
            // handshake flag from whichever `cagents ui` started it. Left set,
            // the child appends a startup-protocol line to the stdout this
            // function shows the user as the shutdown result.
            .env_remove(instance::UI_LAUNCH_HANDSHAKE_ENV)
            .process_group(0)
            .output()
            .map_err(|error| {
                format!("could not launch {name}: {error}; no confirmation was performed")
            })
    }

    fn child_outcome(success: bool, stdout: &[u8], stderr: &[u8]) -> Result<String, String> {
        let stdout = String::from_utf8_lossy(stdout);
        let stderr = String::from_utf8_lossy(stderr);
        if !success {
            // Both streams can carry a distinct failure; welded together they
            // read as one nonsensical sentence.
            let detail = [stdout.trim(), stderr.trim()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            let detail = if detail.is_empty() {
                "no output".to_owned()
            } else {
                detail
            };
            return Err(detail);
        }
        Ok(stdout.trim().into())
    }

    fn stop_from_tray() -> ActionResult {
        let failed = |error: String| format!("Could not stop the daemon: {error}");
        let socket = daemon_socket_path().map_err(|error| failed(error.to_string()))?;
        let mut client =
            ControlClient::connect(&socket).map_err(|error| failed(error.to_string()))?;
        match client
            .shutdown_with_policy(true, None)
            .map_err(|error| failed(error.to_string()))?
        {
            // Stopping would close terminals or interrupt agents, which only
            // the counted confirmation may do.
            ShutdownOutcome::EmptyRequired { .. }
            | ShutdownOutcome::ConfirmationRequired { .. } => {}
            outcome => return stop_result(&outcome),
        }
        let preflight = client
            .shutdown_preflight()
            .map_err(|error| failed(error.to_string()))?;
        let capability = serde_json::to_string(&preflight.capability)
            .map_err(|error| failed(error.to_string()))?;
        let output = spawn_child("cagents-ui", &["--confirm-daemon-stop".into(), capability])
            .map_err(failed)?;
        confirmation_outcome(output.status.success(), &output.stdout, &output.stderr)
    }

    /// The dialog's report decides the outcome whatever its exit status: a
    /// refused stop exits unsuccessfully but still reports why. Its streams
    /// are only shown when it reported nothing, so stderr noise cannot bury
    /// the report.
    fn confirmation_outcome(success: bool, stdout: &[u8], stderr: &[u8]) -> ActionResult {
        let report = String::from_utf8_lossy(stdout);
        if !success && stop_report(&report).is_none() {
            let detail = child_outcome(false, stdout, stderr)
                .err()
                .unwrap_or_default();
            return Err(format!("Could not stop the daemon: {detail}"));
        }
        confirmation_result(&report)
    }

    /// The last JSON report line, whatever else a misbehaving child printed
    /// around it.
    fn stop_report(stdout: &str) -> Option<StopReport> {
        stdout
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<StopReport>(line.trim()).ok())
    }

    fn stop_result(outcome: &ShutdownOutcome) -> ActionResult {
        let summary = shutdown_outcome_summary(outcome);
        match outcome {
            ShutdownOutcome::Accepted { .. } | ShutdownOutcome::AlreadyShuttingDown => Ok(summary),
            _ => Err(summary),
        }
    }

    /// Words the confirmation dialog's report; its last JSON line is the
    /// outcome, whatever else a misbehaving child printed around it.
    fn confirmation_result(stdout: &str) -> ActionResult {
        match stop_report(stdout) {
            Some(StopReport::Cancelled) => Ok("Stop cancelled; nothing was closed".into()),
            Some(StopReport::Finished { outcome }) => stop_result(&outcome),
            Some(StopReport::Failed { error }) => {
                Err(format!("Could not stop the daemon: {error}"))
            }
            // A crashed or mismatched dialog must not read as a stop.
            None => Err("The stop confirmation closed without reporting a result".into()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn asynchronous_command_failure_is_visible_and_not_acceptance() {
            assert_eq!(
                child_outcome(false, b"", b"daemon restarted").unwrap_err(),
                "daemon restarted"
            );
            assert_eq!(
                child_outcome(false, b"refused: 2 live\n", b"daemon restarted\n").unwrap_err(),
                "refused: 2 live; daemon restarted"
            );
            assert_eq!(child_outcome(false, b" \n", b"").unwrap_err(), "no output");
            assert!(confirmation_result("").is_err());
            assert!(confirmation_result("ready\n").is_err());
        }

        #[test]
        fn a_refused_stop_reads_its_report_despite_stderr_noise() {
            let report = serde_json::to_string(&StopReport::Finished {
                outcome: ShutdownOutcome::InventoryChanged { current_count: 4 },
            })
            .unwrap();
            let stdout = format!("{report}\n");
            assert_eq!(
                confirmation_outcome(false, stdout.as_bytes(), b"Gdk-WARNING: noise\n"),
                Err("Not stopped: hosted sessions or background tasks changed (4 sessions now). Refresh and try again.".into())
            );
            assert_eq!(
                confirmation_outcome(true, stdout.as_bytes(), b"noise\n"),
                confirmation_result(&report)
            );
            // With no report, the streams explain the failure.
            assert_eq!(
                confirmation_outcome(false, b"", b"dialog crashed\n"),
                Err("Could not stop the daemon: dialog crashed".into())
            );
            assert!(confirmation_outcome(true, b"", b"").is_err());
        }

        #[test]
        fn shutdown_outcomes_read_as_short_plain_copy() {
            let line = |report: StopReport| serde_json::to_string(&report).unwrap();
            assert_eq!(
                confirmation_result(&line(StopReport::Finished {
                    outcome: ShutdownOutcome::Accepted {
                        live_terminals_before: 3,
                        termination_requested: true,
                    },
                })),
                Ok("Stopping… 3 hosted sessions will close".into())
            );
            assert_eq!(
                confirmation_result(&line(StopReport::Cancelled)),
                Ok("Stop cancelled; nothing was closed".into())
            );
            assert_eq!(
                confirmation_result(&line(StopReport::Finished {
                    outcome: ShutdownOutcome::InventoryChanged { current_count: 4 },
                })),
                Err("Not stopped: hosted sessions or background tasks changed (4 sessions now). Refresh and try again.".into())
            );
            // No outcome is worded with the CLI's field names.
            for outcome in [
                ShutdownOutcome::Accepted {
                    live_terminals_before: 1,
                    termination_requested: true,
                },
                ShutdownOutcome::EmptyRequired { current_count: 2 },
                ShutdownOutcome::InventoryChanged { current_count: 2 },
                ShutdownOutcome::Expired,
                ShutdownOutcome::DaemonRestarted,
                ShutdownOutcome::InvalidCapability,
                ShutdownOutcome::AlreadyShuttingDown,
                ShutdownOutcome::ConfirmationRequired { current_count: 2 },
            ] {
                let copy = shutdown_outcome_summary(&outcome);
                assert!(!copy.contains('_') && !copy.contains('='), "{copy}");
            }
        }

        #[test]
        fn pending_activation_is_not_a_second_click_confirmation() {
            let (updates, mut results) = mpsc::unbounded_channel();
            let mut tray = DaemonTray::new(updates);
            tray.pending = Some("Checking for open terminals…".into());
            tray.run_action(true);
            assert_eq!(tray.status(), "Checking for open terminals…");
            assert!(results.try_recv().is_err());
        }

        #[test]
        fn an_action_result_gives_way_to_the_live_status() {
            let (updates, _results) = mpsc::unbounded_channel();
            let mut tray = DaemonTray::new(updates);
            let now = Instant::now();
            let ttl = tray.finish(Ok("Opened cagents".into()), now);
            assert_eq!(tray.status(), "Opened cagents");
            tray.expire(now + ttl / 2);
            assert_eq!(tray.status(), "Opened cagents");
            tray.expire(now + ttl);
            assert_eq!(tray.status(), "Daemon running");
            // A failure outlasts a success, so a later look still finds it.
            assert!(tray.finish(Err("Could not open".into()), now) > ttl);
        }

        #[test]
        fn the_tooltip_keeps_a_failure_beside_the_agent_counts() {
            let (updates, _results) = mpsc::unbounded_channel();
            let mut tray = DaemonTray::new(updates);
            assert_eq!(tray.tool_tip_description(), "Daemon running");
            tray.tally = AgentTally {
                working: 2,
                waiting: 0,
            };
            tray.finish(Err("Could not open".into()), Instant::now());
            assert_eq!(
                tray.tool_tip_description(),
                "Could not open\n2 agents working"
            );
        }

        #[test]
        fn waiting_menu_activates_the_exact_session_and_restores_the_icon() {
            let (updates, _results) = mpsc::unbounded_channel();
            let (selected, received) = std::sync::mpsc::channel();
            let mut tray = DaemonTray::new(updates);
            let idle_icon = ksni::Tray::icon_pixmap(&tray);
            let session = crate::model::TerminalId::new();
            tray.tally.waiting = 1;
            tray.waiting_agents.push(WaitingAgent {
                session,
                label: "Claude · feature_one".into(),
            });
            tray.focus_agent = Arc::new(move |id| {
                selected.send(id).unwrap();
                Ok("Opened".into())
            });
            assert_eq!(ksni::Tray::icon_name(&tray), "");
            assert_ne!(ksni::Tray::icon_pixmap(&tray)[0].data, idle_icon[0].data);
            let mut menu = ksni::Tray::menu(&tray);
            let ksni::MenuItem::Standard(item) = &mut menu[2] else {
                panic!("waiting agent item");
            };
            assert_eq!(item.label, "Claude · feature__one");
            (item.activate)(&mut tray);
            assert_eq!(
                received.recv_timeout(Duration::from_secs(2)).unwrap(),
                session
            );
            // Pending actions are guarded even if a stale menu is activated again.
            (item.activate)(&mut tray);
            assert!(received.recv_timeout(Duration::from_millis(30)).is_err());
            assert_eq!(tray.status(), "Opening waiting agent…");
            tray.tally.waiting = 0;
            tray.waiting_agents.clear();
            assert_eq!(ksni::Tray::icon_pixmap(&tray)[0].data, idle_icon[0].data);
        }

        #[test]
        fn waiting_labels_are_bounded_and_do_not_contain_control_rows() {
            let label = WaitingAgent::new(
                crate::model::TerminalId::new(),
                crate::agent::Provider::Claude,
                Some(&format!("hello\n\t{}", "x".repeat(2000))),
                std::path::Path::new("/project"),
            );
            assert!(!label.label.chars().any(char::is_control));
            assert!(label.label.chars().count() <= 120);
            let fallback = WaitingAgent::new(
                label.session,
                crate::agent::Provider::Claude,
                Some(" "),
                std::path::Path::new("/repo/feature"),
            );
            assert_eq!(fallback.label, "Claude Code · feature");
        }

        #[test]
        fn agent_counts_read_as_one_line() {
            let tally = |working, waiting| AgentTally { working, waiting }.summary();
            assert_eq!(tally(0, 0), None);
            assert_eq!(tally(1, 0).as_deref(), Some("1 agent working"));
            assert_eq!(tally(0, 2).as_deref(), Some("2 agents waiting for you"));
            assert_eq!(
                tally(3, 1).as_deref(),
                Some("3 agents working · 1 waiting for you")
            );
        }
    }

    impl ksni::Tray for DaemonTray {
        fn id(&self) -> String {
            instance::APPLICATION_ID.into()
        }

        fn title(&self) -> String {
            format!("{} daemon", instance::APPLICATION_NAME)
        }

        fn icon_name(&self) -> String {
            if instance::IS_DEVELOPMENT || self.tally.waiting > 0 {
                // The attention variant must not be replaced by a theme icon.
                // No icon-theme entry is installed for source-tree builds.
                String::new()
            } else {
                instance::APPLICATION_ID.into()
            }
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            vec![if self.tally.waiting > 0 {
                ATTENTION_ICON.clone()
            } else {
                ICON.clone()
            }]
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            self.run_action(false);
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip {
                title: self.title(),
                description: self.tool_tip_description(),
                ..Default::default()
            }
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            let idle = self.pending.is_none();
            // Status first, as a header the actions below act on.
            let mut menu: Vec<ksni::MenuItem<Self>> = vec![
                StandardItem {
                    label: self.status(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            ];
            if let Some(agents) = self.tally.summary() {
                menu.push(
                    StandardItem {
                        label: agents,
                        enabled: false,
                        ..Default::default()
                    }
                    .into(),
                );
            }
            for agent in &self.waiting_agents {
                let session = agent.session;
                menu.push(
                    StandardItem {
                        // DBusMenu treats a single underscore as a mnemonic marker.
                        label: agent.label.replace('_', "__"),
                        enabled: idle,
                        activate: Box::new(move |tray: &mut DaemonTray| tray.run_agent(session)),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            menu.extend([
                ksni::MenuItem::Separator,
                StandardItem {
                    label: format!("Open {}", instance::APPLICATION_NAME),
                    icon_name: self.icon_name(),
                    enabled: idle,
                    activate: Box::new(|tray: &mut DaemonTray| tray.run_action(false)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Stop daemon…".into(),
                    icon_name: "application-exit".into(),
                    enabled: idle,
                    activate: Box::new(|tray: &mut DaemonTray| tray.run_action(true)),
                    ..Default::default()
                }
                .into(),
            ]);
            menu
        }
    }

    pub struct TrayGuard {
        shutdown: Option<oneshot::Sender<()>>,
        worker: Option<JoinHandle<()>>,
    }

    impl Drop for TrayGuard {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(worker) = self.worker.take() {
                let deadline = Instant::now() + Duration::from_millis(250);
                while !worker.is_finished() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                if worker.is_finished() {
                    if worker.join().is_err() {
                        tracing::warn!("cagents tray thread panicked during shutdown");
                    }
                } else {
                    tracing::warn!("cagents tray shutdown exceeded 250 ms; detaching it");
                }
            }
        }
    }

    /// Starts the tray; `tally` reads the daemon's agent counts, or `None`
    /// once the daemon is gone.
    pub fn start(
        tally: impl Fn() -> Option<TraySnapshot> + Send + Sync + 'static,
        focus_agent: impl Fn(crate::model::TerminalId) -> ActionResult + Send + Sync + 'static,
    ) -> TrayGuard {
        let tally = Arc::new(tally);
        let focus_agent: FocusAgent = Arc::new(focus_agent);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let worker = thread::Builder::new()
            .name("cagents-tray".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::warn!(%error, "could not create cagents tray runtime");
                        return;
                    }
                };
                runtime.block_on(async move {
                    let (updates, mut results) = mpsc::unbounded_channel();
                    let mut tray = DaemonTray::new(updates);
                    let initial = tally().unwrap_or_default();
                    tray.tally = initial.tally;
                    tray.waiting_agents = initial.waiting_agents;
                    tray.focus_agent = focus_agent;
                    let handle = match tray.assume_sni_available(true).spawn().await {
                        Ok(handle) => handle,
                        Err(error) => {
                            tracing::warn!(%error, "could not register cagents tray icon");
                            return;
                        }
                    };
                    let handle = Arc::new(handle);
                    let update_handle = Arc::clone(&handle);
                    let updater = tokio::spawn(async move {
                        while let Some(result) = results.recv().await {
                            let Some(ttl) = update_handle
                                .update(move |tray| tray.finish(result, Instant::now()))
                                .await
                            else {
                                break;
                            };
                            let expiring = Arc::clone(&update_handle);
                            tokio::spawn(async move {
                                tokio::time::sleep(ttl).await;
                                expiring.update(|tray| tray.expire(Instant::now())).await;
                            });
                        }
                    });
                    let tally_handle = Arc::clone(&handle);
                    let counter = tokio::spawn(async move {
                        let mut interval = tokio::time::interval(TALLY_INTERVAL);
                        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        let mut last = None;
                        loop {
                            interval.tick().await;
                            // Off the tray's own thread: the daemon's lock can
                            // be held across slow work.
                            let read = Arc::clone(&tally);
                            let Ok(current) = tokio::task::spawn_blocking(move || read()).await
                            else {
                                continue;
                            };
                            // Only a change is worth a menu update on the bus.
                            if current.is_some() && current != last {
                                last = current.clone();
                                let current = current.unwrap_or_default();
                                tally_handle
                                    .update(move |tray| {
                                        tray.tally = current.tally;
                                        tray.waiting_agents = current.waiting_agents;
                                    })
                                    .await;
                            }
                        }
                    });
                    let _ = shutdown_rx.await;
                    updater.abort();
                    counter.abort();
                    handle.shutdown().await;
                });
            })
            .map_err(|error| tracing::warn!(%error, "could not start cagents tray thread"))
            .ok();
        TrayGuard {
            shutdown: Some(shutdown_tx),
            worker,
        }
    }
}

pub use service::{AgentTally, TrayGuard, TraySnapshot, WaitingAgent, start};
