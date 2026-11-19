//! Per-attachment presentation. No observer can scroll another client's view.
use super::*;

#[derive(Default)]
pub(super) struct Viewport {
    pub epoch: u64,
    pub offset: Option<u64>,
    pub size: Option<crate::terminal::TerminalSize>,
    pub focused: bool,
    pub source_generation: u64,
    pub revision: u64,
    pub acknowledged_source: u64,
}
impl Viewport {
    /// Numbers a new presentation of `source`, always above the last one.
    pub(super) fn advance(&mut self, source: u64) {
        self.source_generation = source;
        self.revision = source.max(self.revision.saturating_add(1));
    }
}
impl HostState {
    pub(in crate::host) fn presentation_revision(
        &mut self,
        client: ClientId,
        id: TerminalId,
    ) -> u64 {
        let source = self
            .terminals
            .get(&id)
            .map_or(0, |record| record.handle.generation());
        let view = self
            .subscriptions
            .viewports
            .entry((client, id))
            .or_default();
        if view.source_generation < source || view.revision == 0 {
            view.advance(source);
        }
        view.revision
    }
    pub(in crate::host) fn presentation_observed(
        &mut self,
        client: ClientId,
        id: TerminalId,
        source: u64,
    ) -> u64 {
        let view = self
            .subscriptions
            .viewports
            .entry((client, id))
            .or_default();
        if source > view.source_generation || view.revision == 0 {
            view.advance(source);
        }
        view.revision
    }
    pub(in crate::host) fn resume_presentation(
        &mut self,
        client: ClientId,
        id: TerminalId,
        after: u64,
    ) {
        // Replay suppression carries no finite client cursor. Rebase above
        // both departed views and still-connected views: the replacement
        // connection can attach before the old socket unregisters.
        let after = if after == crate::terminal_ipc::SUPPRESS_GRID_REPLAY {
            let retained = self
                .terminals
                .get(&id)
                .map_or(0, |record| record.presentation_watermark);
            self.subscriptions
                .viewports
                .iter()
                .filter(|((_, terminal), _)| *terminal == id)
                .map(|(_, view)| view.revision)
                .fold(retained, u64::max)
        } else {
            after
        };
        let view = self
            .subscriptions
            .viewports
            .entry((client, id))
            .or_default();
        view.revision = view.revision.max(after.saturating_add(1));
    }
    pub(super) fn retain_presentation_watermark(&mut self, client: ClientId, id: TerminalId) {
        if let Some(view) = self.subscriptions.viewports.get(&(client, id))
            && let Some(record) = self.terminals.get_mut(&id)
        {
            record.presentation_watermark = record.presentation_watermark.max(view.revision);
        }
    }
    pub(in crate::host) fn scroll_client(
        &mut self,
        client: ClientId,
        id: TerminalId,
        scroll: crate::terminal::ipc::TerminalScroll,
    ) -> Result<(), ProtocolError> {
        let record = self.attached_terminal(client, id)?;
        let snapshot = record.handle.snapshot();
        let view = self
            .subscriptions
            .viewports
            .entry((client, id))
            .or_default();
        let rows = view.size.map_or(snapshot.rows, |size| size.rows);
        let maximum = snapshot.scroll.total.saturating_sub(u64::from(rows));
        let current = view.offset.unwrap_or(maximum).min(maximum);
        view.offset = match scroll {
            crate::terminal::ipc::TerminalScroll::Bottom => None,
            crate::terminal::ipc::TerminalScroll::Top => Some(0),
            crate::terminal::ipc::TerminalScroll::Row(row) => Some(row.min(maximum)),
            crate::terminal::ipc::TerminalScroll::Lines(delta) if delta < 0 => {
                Some(current.saturating_sub(delta.unsigned_abs()))
            }
            crate::terminal::ipc::TerminalScroll::Lines(delta) => {
                Some(current.saturating_add(delta as u64).min(maximum))
            }
        };
        view.epoch = view.epoch.saturating_add(1);
        view.advance(snapshot.generation);
        self.notify_viewport(client, id);
        Ok(())
    }
    pub(in crate::host) fn resize_client(
        &mut self,
        client: ClientId,
        id: TerminalId,
        size: crate::terminal::TerminalSize,
    ) -> Result<(), ProtocolError> {
        self.attached_terminal(client, id)?;
        let source = self.terminals[&id].handle.generation();
        let view = self
            .subscriptions
            .viewports
            .entry((client, id))
            .or_default();
        if view.size != Some(size) {
            view.size = Some(size);
            view.epoch = view.epoch.saturating_add(1);
            view.advance(source);
        }
        self.refresh_presentation_owner(id);
        self.notify_viewport(client, id);
        Ok(())
    }
    pub(in crate::host) fn focus_client(
        &mut self,
        client: ClientId,
        id: TerminalId,
        focused: bool,
    ) -> Result<(), ProtocolError> {
        self.attached_terminal(client, id)?;
        self.subscriptions
            .viewports
            .entry((client, id))
            .or_default()
            .focused = focused;
        self.refresh_presentation_owner(id);
        Ok(())
    }
    pub(in crate::host) fn refresh_presentation_owner(&mut self, id: TerminalId) {
        let Some(record) = self.terminals.get(&id) else {
            return;
        };
        let handle = record.handle.clone();
        let focused = self
            .subscriptions
            .by_terminal
            .get(&id)
            .into_iter()
            .flat_map(|clients| clients.iter())
            .filter(|(client, visibility)| {
                **visibility == TerminalVisibility::Visible
                    && self
                        .subscriptions
                        .viewports
                        .get(&(**client, id))
                        .is_some_and(|view| view.focused)
            })
            .map(|(client, _)| *client)
            .collect::<Vec<_>>();
        let previous = self.subscriptions.size_owners.get(&id).copied();
        let owner = previous
            .filter(|client| focused.contains(client))
            .or_else(|| focused.first().copied());
        let _ = handle.focus(!focused.is_empty());
        let owner = owner
            .or_else(|| {
                previous.filter(|client| {
                    self.subscriptions
                        .by_client
                        .get(client)
                        .and_then(|terminals| terminals.get(&id))
                        == Some(&TerminalVisibility::Visible)
                })
            })
            .or_else(|| {
                self.subscriptions.by_terminal.get(&id).and_then(|clients| {
                    clients
                        .iter()
                        .find(|(_, visible)| **visible == TerminalVisibility::Visible)
                        .map(|(client, _)| *client)
                })
            });
        if let Some(owner) = owner {
            self.subscriptions.size_owners.insert(id, owner);
            if let Some(size) = self
                .subscriptions
                .viewports
                .get(&(owner, id))
                .and_then(|view| view.size)
            {
                let _ = handle.resize(size);
            }
        } else {
            self.subscriptions.size_owners.remove(&id);
        }
    }
    pub(in crate::host) fn redraw_source(
        &self,
        client: ClientId,
        id: TerminalId,
        generation: u64,
    ) -> u64 {
        if self
            .subscriptions
            .last_sent(client, id)
            .is_some_and(|sent| sent.generation == generation)
        {
            self.subscriptions
                .viewports
                .get(&(client, id))
                .map_or(generation, |view| view.acknowledged_source)
        } else {
            0
        }
    }
    fn notify_viewport(&mut self, client: ClientId, id: TerminalId) {
        let generation = self.presentation_revision(client, id);
        if let Some(delivery) = self.clients.get(&client) {
            let _ = delivery.push(Outbound::broadcast(
                &self.proof_handle(),
                ServerMessage::Redraw {
                    terminal_id: id,
                    generation,
                },
            ));
        }
    }
}

/// Validates a capture taken outside the lock, once the lock is held again.
/// `Ok(None)` asks the caller to capture again. Like `plan_redraw`, this checks
/// worker identity under the lock: a same-ID replacement can reattach the
/// client at epoch 0, which the epoch and generation checks alone would accept.
pub(super) fn accept_pulled_capture(
    state: &mut HostState,
    client: ClientId,
    id: TerminalId,
    handle: &TerminalHandle,
    epoch: u64,
    source: u64,
) -> Result<Option<u64>, ProtocolError> {
    if !state.subscriptions.attached(client, id) {
        return Err(ProtocolError::new(
            ProtocolErrorCode::PermissionDenied,
            "client detached during capture",
            false,
        ));
    }
    if !state
        .terminals
        .get(&id)
        .is_some_and(|record| record.handle.same_worker(handle))
    {
        return Ok(None);
    }
    let revision = state.presentation_observed(client, id, source);
    let view = &state.subscriptions.viewports[&(client, id)];
    if view.epoch != epoch || view.source_generation > source {
        return Ok(None);
    }
    Ok(Some(revision))
}

/// Worker queries wait outside the host mutex: a worker's publication callback
/// may itself acquire that mutex. This also keeps unrelated sessions responsive.
pub(in crate::host) fn snapshot(
    host: &SharedHost,
    client: ClientId,
    id: TerminalId,
    after: u64,
) -> Result<Option<TerminalSnapshot>, ProtocolError> {
    for _ in 0..4 {
        let (handle, offset, rows, epoch, shown, gate) = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let handle = state.history_handle(client, id)?;
            let revision = state.presentation_revision(client, id);
            if after >= revision && after != 0 {
                return Ok(None);
            }
            let view = &state.subscriptions.viewports[&(client, id)];
            (
                handle.clone(),
                view.offset,
                view.size.map(|size| size.rows),
                view.epoch,
                state.subscriptions.visibility(id) == TerminalVisibility::Visible,
                Arc::clone(&state.control_gate),
            )
        };
        // Queue the read as terminal I/O, so no read enters a worker while an
        // exclusive request holds the gate; wait for its frame outside it.
        let capture = handle
            .viewport_snapshot_admitted(offset, rows, shown, || gate.terminal_io())
            .map_err(|message| {
                ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, true)
            })?;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let source = capture.generation;
        let Some(revision) = accept_pulled_capture(&mut state, client, id, &handle, epoch, source)?
        else {
            continue;
        };
        let mut capture = (*capture).clone();
        capture.generation = revision;
        let (capture, wire) = match state.subscriptions.last_sent(client, id) {
            Some(base) if base.generation == after && after != 0 => {
                capture.share_unchanged_rows(base);
                let capture = Arc::new(capture);
                let wire = TerminalSnapshot::diff(&capture, base);
                (capture, wire)
            }
            _ => {
                let capture = Arc::new(capture);
                let wire = TerminalSnapshot::full(&capture);
                (capture, wire)
            }
        };
        state
            .subscriptions
            .viewports
            .get_mut(&(client, id))
            .expect("attached viewport")
            .acknowledged_source = source;
        // The connection queues this reply after the lock is released.
        state.subscriptions.record_sent(client, id, capture, false);
        return Ok(Some(wire));
    }
    Err(ProtocolError::new(
        ProtocolErrorCode::TerminalUnavailable,
        "terminal changed during viewport capture; retry",
        true,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe};

    #[derive(Default)]
    struct ReadTimings {
        capture_time: Duration,
        slowest_capture: Option<(usize, ClientId, Duration)>,
        sleep_time: Duration,
    }

    impl ReadTimings {
        fn record_capture(&mut self, round: usize, client: ClientId, elapsed: Duration) {
            self.capture_time += elapsed;
            if self
                .slowest_capture
                .is_none_or(|(_, _, slowest)| elapsed > slowest)
            {
                self.slowest_capture = Some((round, client, elapsed));
            }
        }

        fn record_sleep(&mut self, elapsed: Duration) {
            self.sleep_time += elapsed;
        }
    }

    impl std::fmt::Display for ReadTimings {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "total capture time: {:?}; slowest capture: ",
                self.capture_time
            )?;
            if let Some((round, client, elapsed)) = self.slowest_capture {
                write!(f, "round {round}, client {client}, {elapsed:?}")?;
            } else {
                write!(f, "none")?;
            }
            write!(
                f,
                "; total sleep time: {:?} (requested 150ms)",
                self.sleep_time
            )?;

            let loadavg = std::fs::read_to_string("/proc/loadavg")
                .unwrap_or_else(|e| format!("unavailable: {e}"));
            let loadavg = if loadavg.starts_with("unavailable: ") {
                loadavg
            } else {
                loadavg
                    .split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let cpu_pressure = std::fs::read_to_string("/proc/pressure/cpu")
                .unwrap_or_else(|e| format!("unavailable: {e}"));
            let cpu_pressure = cpu_pressure
                .lines()
                .find(|line| line.starts_with("some "))
                .unwrap_or(cpu_pressure.trim());
            write!(f, "; loadavg: {loadavg}; CPU pressure: {cpu_pressure}")
        }
    }

    #[test]
    fn read_timings_report_names_the_slowest_capture() {
        let mut timings = ReadTimings::default();
        timings.record_capture(1, 1, Duration::from_millis(4));
        timings.record_capture(2, 2, Duration::from_millis(8));
        timings.record_capture(3, 1, Duration::from_millis(2));
        timings.record_sleep(Duration::from_millis(5));
        timings.record_sleep(Duration::from_millis(7));

        let report = timings.to_string();
        assert!(report.contains("total capture time: 14ms"));
        assert!(report.contains("slowest capture: round 2, client 2, 8ms"));
        assert!(report.contains("total sleep time: 12ms (requested 150ms)"));
    }

    fn fixture() -> (SharedHost, TerminalHandle, TerminalId, tempfile::TempDir) {
        fixture_with(
            r#"i=1; while [ $i -le 100 ]; do printf 'ROW-%03d\n' "$i"; i=$((i+1)); done; exec sleep 600"#,
        )
    }
    fn fixture_with(script: &str) -> (SharedHost, TerminalHandle, TerminalId, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let recipe = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), script.into()],
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
            unset_environment: Default::default(),
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
        let id = session.id;
        let handle =
            TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("view-test"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        });
        state.register_client(1);
        state.register_client(2);
        state.reserve_spawn(id).unwrap();
        state
            .finish_spawn(1, session, handle.clone(), false)
            .unwrap();
        state.attach(2, id).unwrap();
        state
            .set_visibility(1, id, TerminalVisibility::Visible)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        // One hundred newline-terminated rows leave the cursor on row 101.
        // Waiting for row 100 can race the final newline and viewport advance.
        while handle.snapshot().scroll.total < 101 && Instant::now() < deadline {
            handle.acknowledge_redraw(handle.generation());
            thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.snapshot().scroll.total >= 101);
        // Published rows do not mean the worker is settled: it can still hold
        // a later change back for redraw credit, and the next viewport read
        // would capture it as a new generation. A read is ordered after every
        // queued command and replies only once nothing is withheld.
        handle.viewport_snapshot(Some(0), None, true).unwrap();
        (
            Arc::new(crate::host::state::HostLock::new(state)),
            handle,
            id,
            directory,
        )
    }
    #[test]
    fn scrolling_one_client_does_not_move_another_or_the_parser_viewport() {
        let (host, handle, id, _dir) = fixture();
        // Drain the queued startup reveal and its forced capture before taking
        // baselines. An explicit offset requires a worker round trip instead
        // of the published bottom-view fast path; it does not scroll the parser.
        handle
            .viewport_snapshot(Some(handle.snapshot().scroll.max_offset()), None, true)
            .unwrap();
        let parser = handle.snapshot();
        let bottom = snapshot(&host, 2, id, 0).unwrap().unwrap();
        host.lock()
            .unwrap()
            .scroll_client(1, id, crate::terminal::ipc::TerminalScroll::Top)
            .unwrap();
        let top = snapshot(&host, 1, id, 0).unwrap().unwrap();
        let unchanged = snapshot(&host, 2, id, 0).unwrap().unwrap();
        assert_eq!(handle.snapshot().scroll.offset, parser.scroll.offset);
        assert_ne!(top, bottom);
        assert_eq!(unchanged, bottom);
        handle.shutdown().unwrap();
    }
    #[test]
    fn a_scrolled_client_is_sent_only_rows_that_changed() {
        let (host, handle, id, _dir) = fixture_with(
            r#"i=1; while [ $i -le 100 ]; do printf 'ROW-%03d\n' "$i"; i=$((i+1)); done; read line; i=101; while [ $i -le 105 ]; do printf 'ROW-%03d\n' "$i"; i=$((i+1)); done; exec sleep 600"#,
        );
        host.lock()
            .unwrap()
            .scroll_client(1, id, crate::terminal::ipc::TerminalScroll::Top)
            .unwrap();
        let first = snapshot(&host, 1, id, 0).unwrap().unwrap();
        assert!(matches!(
            &first.content,
            crate::terminal::ipc::SnapshotContent::Full { .. }
        ));
        assert_eq!(first.scroll.offset, 0);

        handle.input(b"more\n".to_vec()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if handle.snapshot().lines.iter().any(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
                    .contains("ROW-105")
            }) {
                break;
            }
            assert!(Instant::now() < deadline, "the shell never replied");
            handle.acknowledge_redraw(handle.generation());
            thread::sleep(Duration::from_millis(10));
        }
        let next = snapshot(&host, 1, id, first.generation)
            .unwrap()
            .expect("output below the viewport advances the presentation");
        assert!(next.generation > first.generation);
        assert!(next.scroll.total > first.scroll.total);
        assert_eq!(next.scroll.offset, 0);
        let crate::terminal::ipc::SnapshotContent::Delta {
            base_generation,
            rows,
        } = next.content
        else {
            panic!("the scrolled viewport should receive a delta");
        };
        assert_eq!(base_generation, first.generation);
        assert!(rows.is_empty(), "unchanged history rows were sent again");
        handle.shutdown().unwrap();
    }
    #[test]
    fn focus_elects_one_size_owner_and_observer_resize_is_local() {
        let (host, handle, id, _dir) = fixture();
        let first = crate::terminal::TerminalSize {
            columns: 80,
            rows: 20,
            cell_width_px: 8,
            cell_height_px: 16,
        };
        let second = crate::terminal::TerminalSize {
            columns: 60,
            rows: 10,
            ..first
        };
        {
            let mut state = host.lock().unwrap();
            state
                .set_visibility(1, id, TerminalVisibility::Visible)
                .unwrap();
            state
                .set_visibility(2, id, TerminalVisibility::Visible)
                .unwrap();
            state.focus_client(1, id, true).unwrap();
            state.resize_client(1, id, first).unwrap();
            state.resize_client(2, id, second).unwrap();
            assert_eq!(state.subscriptions.size_owners[&id], 1);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while handle.snapshot().rows != 20 && Instant::now() < deadline {
            handle.acknowledge_redraw(handle.generation());
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(handle.snapshot().columns, 80);
        assert_eq!(handle.snapshot().rows, 20);
        {
            let mut state = host.lock().unwrap();
            state.focus_client(1, id, false).unwrap();
            state.focus_client(2, id, true).unwrap();
            assert_eq!(state.subscriptions.size_owners[&id], 2);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while handle.snapshot().rows != 10 && Instant::now() < deadline {
            handle.acknowledge_redraw(handle.generation());
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(handle.snapshot().columns, 60);
        assert_eq!(handle.snapshot().rows, 10);
        handle.shutdown().unwrap();
    }
    /// A window holding an off-screen pane keeps the terminal hibernated. Each
    /// `terminal snapshot` attaches, asks to see it and reads it at once; the
    /// read must show the screen it revealed, not the dropped picture (#2401).
    #[test]
    fn a_read_after_revealing_a_hibernated_terminal_shows_its_screen() {
        let (host, handle, id, _dir) = fixture();
        {
            let mut state = host.lock().unwrap();
            state
                .set_visibility(1, id, TerminalVisibility::Hibernated)
                .unwrap();
            state.detach(2, id).unwrap();
            state.register_client(3);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.snapshot().detached && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.snapshot().detached);
        // The window that hibernated it still gets the marker, without a
        // trip through the worker.
        assert!(snapshot(&host, 1, id, 0).unwrap().unwrap().detached);
        for read in 0..20 {
            {
                let mut state = host.lock().unwrap();
                state.attach(3, id).unwrap();
                state
                    .set_visibility(3, id, TerminalVisibility::Visible)
                    .unwrap();
            }
            let capture = snapshot(&host, 3, id, 0).unwrap().unwrap();
            assert!(
                !capture.detached && capture.rows > 0 && capture.columns > 0,
                "read {read} returned the hibernated picture: {}x{}",
                capture.columns,
                capture.rows
            );
            let screen = capture.apply(None).unwrap();
            assert!(
                screen.lines.iter().any(|row| row
                    .cells
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
                    .contains("ROW-100")),
                "read {read} is missing the screen"
            );
            let mut state = host.lock().unwrap();
            state
                .set_visibility(3, id, TerminalVisibility::Hidden)
                .unwrap();
            state.detach(3, id).unwrap();
        }
        handle.shutdown().unwrap();
    }
    /// Nothing is drawn while a terminal is hidden, so its published screen
    /// is the one from before. A read that just asked to see it must show
    /// what has arrived since.
    #[test]
    fn a_read_after_revealing_a_hidden_terminal_shows_output_from_while_hidden() {
        let (host, handle, id, _dir) = fixture_with(
            r#"i=1; while [ $i -le 100 ]; do printf 'ROW-%03d\n' "$i"; i=$((i+1)); done; while read line; do echo "GOT-$line"; done"#,
        );
        {
            let mut state = host.lock().unwrap();
            state
                .set_visibility(1, id, TerminalVisibility::Hidden)
                .unwrap();
            state.detach(2, id).unwrap();
            state.register_client(3);
        }
        let shows_reply = |snapshot: &RenderSnapshot| {
            snapshot.lines.iter().any(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
                    .contains("GOT-FRESH")
            })
        };
        handle.input(b"FRESH\n".to_vec()).unwrap();
        // A hidden terminal's parser can still be read with a viewport other
        // than the published one. Wait until it holds the reply.
        let rows = handle.snapshot().rows - 1;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !shows_reply(&handle.viewport_snapshot(None, Some(rows), false).unwrap()) {
            assert!(Instant::now() < deadline, "the shell never replied");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !shows_reply(&handle.snapshot()),
            "a hidden terminal published a screen"
        );
        {
            let mut state = host.lock().unwrap();
            state.attach(3, id).unwrap();
            state
                .set_visibility(3, id, TerminalVisibility::Visible)
                .unwrap();
        }
        let screen = snapshot(&host, 3, id, 0)
            .unwrap()
            .unwrap()
            .apply(None)
            .unwrap();
        assert!(
            screen.lines.iter().any(|row| row
                .cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
                .contains("GOT-FRESH")),
            "the read showed the screen from before the terminal was hidden"
        );
        handle.shutdown().unwrap();
    }
    #[test]
    fn continuously_dirty_output_keeps_two_custom_viewports_responsive() {
        if crate::retained_fixture::run_isolated() {
            return;
        }
        let (host, handle, id, _dir) = fixture_with(
            r#"i=1; while :; do printf 'ROW-%03d\n' "$i"; i=$((i+1)); sleep 0.005; done"#,
        );
        let first = crate::terminal::TerminalSize {
            columns: 80,
            rows: 13,
            cell_width_px: 8,
            cell_height_px: 16,
        };
        let second = crate::terminal::TerminalSize { rows: 17, ..first };
        {
            let mut state = host.lock().unwrap();
            state
                .set_visibility(2, id, TerminalVisibility::Visible)
                .unwrap();
            state.focus_client(1, id, true).unwrap();
            state.resize_client(1, id, first).unwrap();
            state.resize_client(2, id, second).unwrap();
            state
                .scroll_client(1, id, crate::terminal::ipc::TerminalScroll::Top)
                .unwrap();
        }
        let mut timings = ReadTimings::default();
        let started = Instant::now();
        for round in 0..30 {
            for client in [1, 2] {
                let capture_started = Instant::now();
                let capture = snapshot(&host, client, id, 0);
                timings.record_capture(round, client, capture_started.elapsed());
                assert!(capture.unwrap().is_some());
            }
            let sleep_started = Instant::now();
            thread::sleep(Duration::from_millis(5));
            timings.record_sleep(sleep_started.elapsed());
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "60 viewport reads took {elapsed:?}; {timings}"
        );
        handle.shutdown().unwrap();
    }
}
