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
    fn advance(&mut self, source: u64) {
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
        let view = self
            .subscriptions
            .viewports
            .entry((client, id))
            .or_default();
        view.revision = view.revision.max(after.saturating_add(1));
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

/// Worker queries wait outside the host mutex: a worker's publication callback
/// may itself acquire that mutex. This also keeps unrelated sessions responsive.
pub(in crate::host) fn snapshot(
    host: &SharedHost,
    client: ClientId,
    id: TerminalId,
    after: u64,
) -> Result<Option<TerminalSnapshot>, ProtocolError> {
    for _ in 0..4 {
        let (handle, offset, rows, epoch) = {
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
                view.size
                    .map_or(handle.snapshot().rows.max(1), |size| size.rows),
                view.epoch,
            )
        };
        let capture = handle.viewport_snapshot(offset, rows).map_err(|message| {
            ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, true)
        })?;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !state.subscriptions.attached(client, id) {
            return Err(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                "client detached during capture",
                false,
            ));
        }
        let source = capture.generation;
        let revision = state.presentation_observed(client, id, source);
        let view = &state.subscriptions.viewports[&(client, id)];
        if view.epoch != epoch || view.source_generation > source {
            continue;
        }
        let mut capture = (*capture).clone();
        capture.generation = revision;
        let capture = Arc::new(capture);
        let wire = match state.subscriptions.last_sent(client, id) {
            Some(base) if base.generation == after && after != 0 => {
                TerminalSnapshot::diff(&capture, base)
            }
            _ => TerminalSnapshot::full(&capture),
        };
        state
            .subscriptions
            .viewports
            .get_mut(&(client, id))
            .expect("attached viewport")
            .acknowledged_source = source;
        state.subscriptions.record_sent(client, id, capture);
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
    #[test]
    fn continuously_dirty_output_keeps_two_custom_viewports_responsive() {
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
        let started = Instant::now();
        for _ in 0..30 {
            for client in [1, 2] {
                assert!(snapshot(&host, client, id, 0).unwrap().is_some());
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(started.elapsed() < Duration::from_secs(3));
        handle.shutdown().unwrap();
    }
}
