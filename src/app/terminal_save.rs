//! A running automatic terminal whose reservation has not been saved yet.
//! Attachment and persistence settle independently: retrying the latter never
//! replaces the terminal surface or admits a new launch.
use super::*;
use crate::{
    control::{ControlClient, ControlError},
    terminal_ipc::{ProtocolErrorCode, TerminalDescriptor, TerminalSpawnOptions},
    workspace_model::views::{Pane, Scope},
};

const TITLE: &str = "This terminal isn't saved — it won't reopen after a restart";

#[derive(Debug, Clone)]
pub(super) struct Reservation {
    pub scope: Scope,
    pub pane: Pane,
}

#[derive(Debug, Clone)]
pub(super) struct Failure {
    reservation: Reservation,
    proof: HostSessionProof,
    error: SaveError,
}

#[derive(Debug, Clone)]
pub(super) struct SaveError {
    reason: String,
    diagnostics: String,
}

impl SaveError {
    fn control(error: ControlError) -> Self {
        let reason = match &error {
            ControlError::Remote { message, .. } => message.clone(),
            _ => "Could not reach the terminal service. Keep this terminal open and retry saving."
                .into(),
        };
        Self {
            reason,
            diagnostics: error.to_string(),
        }
    }
}

#[derive(Debug)]
struct Entry {
    failure: Failure,
    retry: Option<uuid::Uuid>,
}

#[derive(Debug, Default)]
pub(super) struct State {
    entries: HashMap<CardId, Entry>,
    focus_after_save: Option<(CardId, TerminalId)>,
}

#[derive(Debug, Clone)]
pub(super) enum Action {
    Retry(CardId, TerminalId),
    Finished {
        card: CardId,
        terminal: TerminalId,
        proof: HostSessionProof,
        operation: uuid::Uuid,
        result: Result<crate::workspace_model::WorkspaceModel, SaveError>,
    },
    CopyDetails(CardId, TerminalId),
}

/// A storage or revision failure is a partial success only after the same
/// daemon proves it still hosts this exact launch and the accepted pane is
/// unchanged. Inventory never grants permission to save a changed source.
pub(super) fn complete(
    client: &mut ControlClient,
    reservation: Reservation,
    session: &TerminalSession,
    options: TerminalSpawnOptions,
) -> Result<(TerminalDescriptor, Option<Failure>), String> {
    for _ in 0..100 {
        match client.complete_pending_terminal(reservation.pane.id, session.id, options.clone()) {
            Ok(hosted) => return Ok((hosted, None)),
            Err(ControlError::Remote {
                code: ProtocolErrorCode::TerminalAlreadyExists,
                ..
            }) => {
                // Another start is still publishing. Inventory alone cannot
                // acknowledge that its reservation was saved.
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                if matches!(
                    &error,
                    ControlError::Remote {
                        code: ProtocolErrorCode::Internal | ProtocolErrorCode::ViewConflict,
                        ..
                    }
                ) {
                    let (view, _) = client
                        .read_workspace_view(reservation.scope.clone())
                        .map_err(|e| e.to_string())?;
                    let current = view.as_ref().and_then(|view| {
                        view.panes
                            .iter()
                            .find(|pane| pane.id == reservation.pane.id)
                    });
                    let unchanged = current == Some(&reservation.pane);
                    let saved_by_peer = current.is_some_and(|pane| !pane.pending_terminal_start
                        && matches!(&pane.kind, CardKind::Terminal(saved) if saved.id == session.id
                            && saved.launch == session.launch && saved.continuity.live_proof().as_ref() == Some(client.proof())));
                    if (unchanged || saved_by_peer)
                        && let Some(hosted) = client
                            .list()
                            .map_err(|e| e.to_string())?
                            .into_iter()
                            .find(|t| {
                                t.session.id == session.id
                                    && t.session.launch == session.launch
                                    && (t.session.continuity.live_proof().as_ref()
                                        == Some(client.proof())
                                        || matches!(
                                            t.session.continuity,
                                            TerminalContinuity::Exited { .. }
                                                | TerminalContinuity::RelaunchAvailable {
                                                    last_exit: Some(_),
                                                    ..
                                                }
                                        ))
                            })
                    {
                        let failure = unchanged.then(|| Failure {
                            reservation,
                            proof: client.proof().clone(),
                            error: SaveError::control(error),
                        });
                        return Ok((hosted, failure));
                    }
                }
                return Err(error.to_string());
            }
        }
    }
    Err(SHARED_TERMINAL_DISCONNECTED.into())
}

fn retry_save(
    card: CardId,
    terminal: TerminalId,
    expected: HostSessionProof,
) -> Result<crate::workspace_model::WorkspaceModel, SaveError> {
    let socket = crate::terminal::ensure_daemon_ready().map_err(|e| SaveError {
        reason: "The terminal service is unavailable. Keep this terminal open and retry saving."
            .into(),
        diagnostics: e.to_string(),
    })?;
    let mut client = ControlClient::connect(&socket).map_err(SaveError::control)?;
    if client.proof() != &expected {
        return Err(SaveError {
            reason: "The terminal service changed. This terminal has not been saved.".into(),
            diagnostics: "daemon proof changed before retry save".into(),
        });
    }
    client
        .retry_pending_terminal_save(card, terminal)
        .map_err(SaveError::control)?;
    client.read_workspace_model().map_err(SaveError::control)
}

impl Cagents {
    pub(super) fn terminal_save_reservation(&self, card: CardId) -> Option<Reservation> {
        self.workspace_model.views.iter().find_map(|view| {
            view.panes
                .iter()
                .find(|pane| pane.id == card && pane.pending_terminal_start)
                .map(|pane| Reservation {
                    scope: view.scope.clone(),
                    pane: pane.clone(),
                })
        })
    }

    pub(super) fn record_terminal_save_failure(&mut self, card: CardId, failure: Failure) {
        let CardKind::Terminal(session) = &failure.reservation.pane.kind else {
            return;
        };
        let terminal = session.id;
        // A peer may have completed the save while this attachment was in flight.
        if self.workspace_model.views.iter().flat_map(|view| &view.panes).any(|pane| pane.id == card
            && !pane.pending_terminal_start
            && matches!(&pane.kind, CardKind::Terminal(current) if current.id == terminal && current.continuity.live_proof().as_ref() == Some(&failure.proof))) {
            return;
        }
        self.status = Some(
            TransientNotice::error(TITLE)
                .with_details(failure.error.reason.clone())
                .with_key(NoticeKey::TerminalSave(card, terminal)),
        );
        self.terminal_save.entries.insert(
            card,
            Entry {
                failure,
                retry: None,
            },
        );
    }

    fn resolve_terminal_save(&mut self, card: CardId) {
        if let Some(entry) = self.terminal_save.entries.remove(&card)
            && let CardKind::Terminal(session) = entry.failure.reservation.pane.kind
        {
            self.resolve_notice(NoticeKey::TerminalSave(card, session.id));
        }
    }

    pub(super) fn prune_terminal_save_notices(&mut self) {
        let closed: Vec<_> = self
            .terminal_save
            .entries
            .iter()
            .filter_map(|(&card, entry)| {
                let CardKind::Terminal(accepted) = &entry.failure.reservation.pane.kind else {
                    return Some(card);
                };
                let current = self
                    .card_state(card)
                    .filter(|state| state.existence.is_open());
                let same = current.is_some_and(|state| {
                    matches!(&state.kind, CardKind::Terminal(session)
                if session.id == accepted.id)
                });
                (!same).then_some(card)
            })
            .collect();
        for card in closed {
            self.resolve_terminal_save(card);
        }
    }

    /// Hold the exact canonical reservation until the completion request saves
    /// it. Runtime titles, proof and resume metadata remain local meanwhile.
    pub(super) fn hold_unsaved_terminal_pane(&self, scope: &Scope, pane: &mut Pane) {
        if self.terminal_save.entries.contains_key(&pane.id)
            && let Some(canonical) = self
                .workspace_model
                .views
                .iter()
                .find(|view| &view.scope == scope)
                .and_then(|view| {
                    view.panes
                        .iter()
                        .find(|current| current.id == pane.id && current.pending_terminal_start)
                })
        {
            *pane = canonical.clone();
        }
    }

    /// A model update can report another window's successful Retry save.
    pub(super) fn observe_terminal_save(&mut self, pane: &Pane) -> bool {
        let Some(entry) = self.terminal_save.entries.get(&pane.id) else {
            return false;
        };
        let CardKind::Terminal(accepted) = &entry.failure.reservation.pane.kind else {
            return false;
        };
        let same_worker =
            matches!(&pane.kind, CardKind::Terminal(session) if session.id == accepted.id);
        if !pane.pending_terminal_start
            && same_worker
            && matches!(&pane.kind, CardKind::Terminal(session) if session.continuity.live_proof().as_ref() == Some(&entry.failure.proof))
        {
            if entry.retry.is_some() {
                self.terminal_save.focus_after_save = Some((pane.id, accepted.id));
            }
            self.resolve_terminal_save(pane.id);
            return true;
        }
        // Keep the authenticated live local source for input and Close while
        // the canonical pending source remains the publication authority.
        pane.pending_terminal_start && same_worker
            && self.card_state(pane.id).is_some_and(|state| matches!(&state.kind, CardKind::Terminal(session) if session.id == accepted.id))
    }

    pub(super) fn refocus_saved_terminal(&mut self) -> Task<Message> {
        let Some((card, terminal)) = self.terminal_save.focus_after_save.take() else {
            return Task::none();
        };
        if self.card_state(card).is_some_and(|state| {
            state.existence.is_open()
                && matches!(&state.kind, CardKind::Terminal(session) if session.id == terminal)
        }) {
            self.refocus_after_terminal_save(card)
        } else {
            Task::none()
        }
    }

    pub(super) fn has_terminal_save_notices(&self) -> bool {
        !self.terminal_save.entries.is_empty()
    }

    pub(super) fn terminal_save_notice(
        &self,
        card: CardId,
        popup: bool,
    ) -> Option<Element<'_, Message>> {
        let entry = self.terminal_save.entries.get(&card)?;
        let CardKind::Terminal(session) = &entry.failure.reservation.pane.kind else {
            return None;
        };
        let terminal = session.id;
        let suffix = if popup { "popup" } else { "pane" };
        let severity = NoticeSeverity::Error;
        let retry = if entry.retry.is_some() {
            notice_surface_detail(severity, "Retrying save…")
        } else {
            notice_surface_action(
                severity,
                "Retry save",
                "Retry saving this terminal",
                card_control_id(card, &format!("terminal-save.retry.{suffix}")),
                Message::TerminalSave(Action::Retry(card, terminal)),
            )
        };
        let copy = crate::interaction::tooltip(
            widget::button::icon(Icon::Copy)
                .id(card_control_id(
                    card,
                    &format!("terminal-save.copy.{suffix}"),
                ))
                .name("Copy terminal save details")
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .class(notice_surface_button(severity))
                .on_press(Message::TerminalSave(Action::CopyDetails(card, terminal))),
            "Copy details",
        );
        Some(notice_surface(
            severity,
            widget::column::with_children(vec![
                notice_body(if popup {
                    format!(
                        "{}: {TITLE}",
                        self.notice_pane_name(card)
                            .unwrap_or_else(|| "Terminal".into())
                    )
                } else {
                    TITLE.into()
                }),
                notice_surface_details(severity, &entry.failure.error.reason),
            ])
            .spacing(space::XS)
            .width(Length::Fill)
            .into(),
            Some(
                widget::row::with_children(vec![retry, copy.into()])
                    .spacing(space::XS)
                    .align_y(Alignment::Center)
                    .into(),
            ),
            false,
        ))
    }

    pub(super) fn terminal_save_notice_views(&self) -> Vec<Element<'_, Message>> {
        self.terminal_save
            .entries
            .keys()
            .filter_map(|card| self.terminal_save_notice(*card, true))
            .collect()
    }

    pub(super) fn update_terminal_save(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::CopyDetails(card, terminal) => {
                let Some(entry) = self.terminal_save.entries.get(&card) else {
                    return Task::none();
                };
                if !matches!(&entry.failure.reservation.pane.kind, CardKind::Terminal(session) if session.id == terminal)
                {
                    return Task::none();
                }
                return iced::clipboard::write(format!(
                    "Terminal save failed\nPane: {card}\nTerminal: {terminal}\n{}",
                    entry.failure.error.diagnostics
                ));
            }
            Action::Retry(card, terminal) => {
                let Some(entry) = self.terminal_save.entries.get(&card) else {
                    return Task::none();
                };
                let current = self.card_state(card).is_some_and(|state| state.existence.is_open()
                    && matches!(&state.kind, CardKind::Terminal(session) if session.id == terminal));
                if entry.retry.is_some() || !current {
                    return Task::none();
                }
                let proof = entry.failure.proof.clone();
                let operation = uuid::Uuid::new_v4();
                self.terminal_save.entries.get_mut(&card).unwrap().retry = Some(operation);
                cosmic::task::future(async move {
                    let expected = proof.clone();
                    let result =
                        tokio::task::spawn_blocking(move || retry_save(card, terminal, expected))
                            .await
                            .map_err(|e| SaveError {
                                reason: "The save task could not finish. Retry saving.".into(),
                                diagnostics: e.to_string(),
                            })
                            .and_then(|r| r);
                    Message::TerminalSave(Action::Finished {
                        card,
                        terminal,
                        proof,
                        operation,
                        result,
                    })
                })
            }
            Action::Finished {
                card,
                terminal,
                proof,
                operation,
                result,
            } => {
                let Some(entry) = self.terminal_save.entries.get_mut(&card) else {
                    return Task::none();
                };
                if entry.retry != Some(operation)
                    || entry.failure.proof != proof
                    || !matches!(&entry.failure.reservation.pane.kind, CardKind::Terminal(session) if session.id == terminal)
                {
                    return Task::none();
                }
                match result {
                    Ok(model) => {
                        // Keep the operation until canonical completion can
                        // return focus from the retiring Retry save control.
                        let applied = self.apply_workspace_model_update(
                            workspace_model_sync::ModelUpdate::Read(model),
                        );
                        if let Some(entry) = self.terminal_save.entries.get_mut(&card)
                            && entry.retry == Some(operation)
                        {
                            // An older/unconfirmed read still permits retry.
                            entry.retry = None;
                        }
                        applied
                    }
                    Err(error) => {
                        // The shell stays mounted even if the retry fails.
                        entry.retry = None;
                        entry.failure.error = error;
                        let failure = entry.failure.clone();
                        self.record_terminal_save_failure(card, failure);
                        Task::none()
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{BootId, ChannelId, HostIncarnation, HostSessionId, ProjectChannel},
        workspace_model::{ModelChannel, views::View},
    };

    fn fixture() -> (Cagents, CardId, TerminalId, tempfile::TempDir) {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.durable.cards[0].id;
        let directory = tempfile::tempdir().unwrap();
        let channel = ChannelId::new();
        let project = ProjectId::new();
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.launch.working_directory = directory.path().into();
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: session.launch.clone(),
            last_exit: None,
        };
        let terminal = session.id;
        let mut pane = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
        pane.pending_terminal_start = true;
        let scope = Scope::Channel(channel);
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("fixture"),
            boot_id: BootId::new("fixture"),
            host_incarnation: HostIncarnation::new("fixture"),
        };
        let failure = Failure {
            reservation: Reservation {
                scope: scope.clone(),
                pane: pane.clone(),
            },
            proof: proof.clone(),
            error: SaveError {
                reason: "The session database is read-only.".into(),
                diagnostics: "daemon rejected the request (Internal): fixture save refused".into(),
            },
        };
        let durable = Arc::make_mut(&mut app.durable);
        durable.active_channel = Some(channel);
        durable.channels.push(ProjectChannel {
            id: channel,
            project_id: project,
            title: "Fixture".into(),
            title_source: Default::default(),
            ordinal: 1,
            shared: true,
            cards: vec![card],
            focused: Some(card),
        });
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.workspace_model.channels.push(ModelChannel {
            id: channel,
            repository: directory.path().into(),
            title: "Fixture".into(),
            title_source: Default::default(),
            ordinal: 1,
        });
        app.workspace_model.views.push(View {
            scope: scope.clone(),
            revision: 1,
            panes: vec![pane],
            retired: vec![],
        });
        app.shared_views.attached.insert(card);
        app.initialize_terminal_save_test_scope(scope);
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.continuity = TerminalContinuity::Live {
            host_session_id: proof.host_session_id,
            boot_id: proof.boot_id,
            host_incarnation: proof.host_incarnation,
        };
        let handle = super::super::tests::unconnected_surface(directory.path(), terminal);
        app.cards.insert(card, CardRuntime::Terminal(handle));
        app.lifecycle
            .register(card, Exposure::Visible, Residency::Hot, Instant::now());
        app.record_terminal_save_failure(card, failure);
        // Match the application update boundary's status→history transfer.
        app.notices.record(app.status.take().unwrap());
        (app, card, terminal, directory)
    }

    fn retry(
        app: &mut Cagents,
        card: CardId,
        terminal: TerminalId,
    ) -> (uuid::Uuid, HostSessionProof) {
        drop(app.update_terminal_save(Action::Retry(card, terminal)));
        let entry = &app.terminal_save.entries[&card];
        (entry.retry.unwrap(), entry.failure.proof.clone())
    }

    fn saved_model(app: &Cagents, card: CardId) -> crate::workspace_model::WorkspaceModel {
        let mut model = app.workspace_model.clone();
        model.sequence += 1;
        let pane = &mut model.views[0].panes[0];
        pane.pending_terminal_start = false;
        pane.kind = app.card_state(card).unwrap().kind.clone();
        model.views[0].revision += 1;
        model
    }

    #[test]
    fn successful_retry_save_clears_its_notice_and_preserves_the_live_surface() {
        let (mut app, card, terminal, _directory) = fixture();
        assert_eq!(app.notice_control_severity(), Some(NoticeSeverity::Error));
        assert!(app.terminal_save_notice(card, false).is_some());
        let (operation, proof) = retry(&mut app, card, terminal);
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Terminal(handle)) if handle.terminal_id() == terminal)
        );
        let result = Ok(saved_model(&app, card));
        drop(app.update(Message::TerminalSave(Action::Finished {
            card,
            terminal,
            proof,
            operation,
            result,
        })));
        assert!(!app.notice_stands(NoticeKey::TerminalSave(card, terminal)));
        assert!(!app.has_terminal_save_notices());
        assert_eq!(
            app.notice_control_severity(),
            None,
            "saving retires the red header indicator"
        );
        assert!(app.terminal_save_notice(card, false).is_none());
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Terminal(handle)) if handle.terminal_id() == terminal)
        );
        assert!(app.pending_terminal_reservation(card).is_none());
    }

    #[test]
    fn successful_retry_restores_keyboard_focus_in_both_completion_orders() {
        for watcher_first in [false, true] {
            let (mut app, card, terminal, _directory) = fixture();
            let (operation, proof) = retry(&mut app, card, terminal);
            let model = saved_model(&app, card);
            if watcher_first {
                drop(
                    app.apply_workspace_model_update(workspace_model_sync::ModelUpdate::Read(
                        model.clone(),
                    )),
                );
            }
            drop(app.update_terminal_save(Action::Finished {
                card,
                terminal,
                proof,
                operation,
                result: Ok(model),
            }));
            assert_eq!(
                app.terminal_save.focus_after_save,
                Some((card, terminal)),
                "save completion must return focus after retiring Retry save (watcher_first={watcher_first})"
            );
            assert!(!app.has_terminal_save_notices());
            assert!(iced::runtime::task::into_stream(app.refocus_saved_terminal()).is_some());
            assert!(app.terminal_save.focus_after_save.is_none());
        }
    }

    #[test]
    fn unconfirmed_retry_save_keeps_its_notice_and_reenables_retry() {
        let (mut app, card, terminal, _directory) = fixture();
        let (operation, proof) = retry(&mut app, card, terminal);
        let model = app.workspace_model.clone();
        drop(app.update_terminal_save(Action::Finished {
            card,
            terminal,
            proof,
            operation,
            result: Ok(model),
        }));
        assert!(app.notice_stands(NoticeKey::TerminalSave(card, terminal)));
        assert!(app.terminal_save.entries[&card].retry.is_none());
        assert!(app.terminal_save.focus_after_save.is_none());
        assert_ne!(retry(&mut app, card, terminal).0, operation);
    }

    #[test]
    fn failed_retry_save_keeps_the_surface_and_one_notice_then_success_preserves_unrelated_errors()
    {
        let (mut app, card, terminal, _directory) = fixture();
        app.notices
            .record(TransientNotice::error("Unrelated failure"));
        let (operation, proof) = retry(&mut app, card, terminal);
        drop(app.update(Message::TerminalSave(Action::Finished {
            card,
            terminal,
            proof,
            operation,
            result: Err(SaveError {
                reason: "The session database is still read-only.".into(),
                diagnostics: "Internal: fixture save refused again".into(),
            }),
        })));
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Terminal(handle)) if handle.terminal_id() == terminal)
        );
        assert_eq!(
            app.notices
                .recent()
                .filter(|n| n.notice.key == Some(NoticeKey::TerminalSave(card, terminal)))
                .count(),
            1
        );
        assert!(!app.notices.recent().any(|n| {
            n.notice
                .details
                .as_deref()
                .is_some_and(|s| s.contains("Internal"))
        }));
        let (operation, proof) = retry(&mut app, card, terminal);
        let result = Ok(saved_model(&app, card));
        drop(app.update(Message::TerminalSave(Action::Finished {
            card,
            terminal,
            proof,
            operation,
            result,
        })));
        assert!(!app.notice_stands(NoticeKey::TerminalSave(card, terminal)));
        assert!(
            app.notices
                .recent()
                .any(|n| n.notice.message == "Unrelated failure")
        );
        assert_eq!(app.notice_control_severity(), Some(NoticeSeverity::Error));
    }

    #[test]
    fn pending_save_holds_the_full_reservation_during_runtime_metadata_changes() {
        let (mut app, card, terminal, _directory) = fixture();
        let canonical = app.workspace_model.views[0].panes[0].clone();
        let scope = app.workspace_model.views[0].scope.clone();
        let state = app.card_state_mut(card).unwrap();
        state.title = "Shell title changed".into();
        state.width_fraction = 0.8;
        let CardKind::Terminal(session) = &mut state.kind else {
            unreachable!()
        };
        session.resume_hint = Some("runtime metadata".into());
        let mut local = Pane::from_card(state, None).unwrap();
        app.hold_unsaved_terminal_pane(&scope, &mut local);
        assert_eq!(local, canonical);
        // Both full and selected-scope projections must preserve the same source.
        let projected = app.local_views_for_terminal_save_test(&scope);
        assert_eq!(projected, canonical);
        let mut refreshed = app.workspace_model.clone();
        refreshed.sequence += 1;
        refreshed.views[0].revision += 1;
        drop(app.update(Message::WorkspaceModelObserved(
            workspace_model_sync::ModelUpdate::Read(refreshed),
        )));
        let local = app.card_state(card).unwrap();
        assert_eq!(local.title, "Shell title changed");
        assert_eq!(local.width_fraction, 0.8);
        assert!(
            matches!(&local.kind, CardKind::Terminal(session) if session.resume_hint.as_deref() == Some("runtime metadata"))
        );
        let mut echo = canonical.clone();
        assert!(
            app.observe_terminal_save(&echo),
            "the pending echo retains the live proof for Close"
        );
        echo.pending_terminal_start = false;
        echo.kind = app.card_state(card).unwrap().kind.clone();
        assert!(app.observe_terminal_save(&echo));
        assert!(
            !app.notice_stands(NoticeKey::TerminalSave(card, terminal)),
            "another window can complete this save"
        );
    }

    #[test]
    fn changed_pending_source_is_not_mistaken_for_a_successful_save() {
        let (mut app, card, terminal, _directory) = fixture();
        let mut model = app.workspace_model.clone();
        model.sequence += 1;
        model.views[0].revision += 1;
        let CardKind::Terminal(source) = &mut model.views[0].panes[0].kind else {
            unreachable!()
        };
        source.resume_hint = Some("new canonical intent".into());
        drop(app.update(Message::WorkspaceModelObserved(
            workspace_model_sync::ModelUpdate::Read(model),
        )));
        assert!(app.notice_stands(NoticeKey::TerminalSave(card, terminal)));
        assert!(
            matches!(&app.card_state(card).unwrap().kind, CardKind::Terminal(session) if session.continuity.live_proof().is_some())
        );
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Terminal(handle)) if handle.terminal_id() == terminal)
        );
    }

    #[test]
    fn worker_exit_keeps_the_save_failure_and_retry_does_not_relaunch() {
        let (mut app, card, terminal, _directory) = fixture();
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            unreachable!()
        };
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: session.launch.clone(),
            last_exit: None,
        };
        app.prune_terminal_save_notices();
        assert!(app.notice_stands(NoticeKey::TerminalSave(card, terminal)));
        let (operation, _) = retry(&mut app, card, terminal);
        assert_eq!(app.terminal_save.entries[&card].retry, Some(operation));
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Terminal(handle)) if handle.terminal_id() == terminal)
        );
        assert_eq!(
            app.terminal_starts_in_flight(),
            0,
            "Retry save never schedules a launch"
        );
    }

    #[test]
    fn stale_save_callbacks_cannot_clear_or_replace_a_newer_save_attempt() {
        let (mut app, card, terminal, _directory) = fixture();
        let (operation, proof) = retry(&mut app, card, terminal);
        let result = Ok(saved_model(&app, card));
        let stale = Action::Finished {
            card,
            terminal,
            proof,
            operation: uuid::Uuid::new_v4(),
            result,
        };
        drop(app.update_terminal_save(stale));
        assert!(app.notice_stands(NoticeKey::TerminalSave(card, terminal)));
        assert_eq!(app.terminal_save.entries[&card].retry, Some(operation));
        app.card_state_mut(card).unwrap().existence = Existence::Closing {
            operation_id: crate::model::OperationId::new(),
        };
        app.prune_terminal_save_notices();
        assert!(!app.has_terminal_save_notices());
        assert!(!app.notice_stands(NoticeKey::TerminalSave(card, terminal)));
    }
    #[test]
    fn save_error_fallback_attaches_only_the_accepted_running_or_completed_worker() {
        use crate::terminal_ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ServerEnvelope,
            ServerMessage, ServerResponse, read_frame, write_frame,
        };
        use std::os::unix::net::UnixStream;
        for case in [
            "pending",
            "exited",
            "peer-saved",
            "revision-conflict",
            "source-changed",
            "source-conflict",
            "wrong-worker",
        ] {
            let (app, card, terminal, _directory) = fixture();
            let failure = app.terminal_save.entries[&card].failure.clone();
            let reservation = failure.reservation.clone();
            let CardKind::Terminal(accepted) = reservation.pane.kind.clone() else {
                unreachable!()
            };
            let CardKind::Terminal(live) = app.card_state(card).unwrap().kind.clone() else {
                unreachable!()
            };
            let mut view = app.workspace_model.views[0].clone();
            if case == "peer-saved" {
                view.panes[0].pending_terminal_start = false;
                view.panes[0].kind = CardKind::Terminal(live.clone());
            }
            if matches!(case, "source-changed" | "source-conflict") {
                view.panes[0].title = "Changed elsewhere".into();
            }
            let mut hosted =
                TerminalDescriptor::relaunchable(terminal, accepted.launch.clone(), None);
            hosted.session = live;
            if case == "exited" {
                hosted.session.continuity = TerminalContinuity::Exited {
                    last_exit: crate::model::TerminalExit {
                        code: Some(0),
                        signal: None,
                        occurred_at_unix_ms: 1,
                    },
                };
            }
            if case == "wrong-worker" {
                hosted.session.id = TerminalId::new();
            }
            let (client, mut server) = UnixStream::pair().unwrap();
            let proof = failure.proof.clone();
            let serving = std::thread::spawn(move || {
                let mut requests = Vec::new();
                while let Ok(Some(request)) = read_frame::<ClientEnvelope>(&mut server) {
                    let message = match request.request {
                        ClientRequest::Connect { .. } => ServerMessage::Response {
                            response: ServerResponse::Connected {
                                proof: proof.clone(),
                                resumed: vec![],
                            },
                        },
                        ClientRequest::CompletePendingTerminal { .. } => {
                            requests.push("complete");
                            ServerMessage::Error(ProtocolError::new(
                                if matches!(case, "source-conflict" | "revision-conflict") {
                                    ProtocolErrorCode::ViewConflict
                                } else {
                                    ProtocolErrorCode::Internal
                                },
                                "fixture refused save",
                                false,
                            ))
                        }
                        ClientRequest::ReadWorkspaceView { .. } => ServerMessage::Response {
                            response: ServerResponse::WorkspaceView {
                                view: Some(view.clone()),
                                sequence: 1,
                            },
                        },
                        ClientRequest::List => ServerMessage::Response {
                            response: ServerResponse::Listed {
                                terminals: vec![hosted.clone()],
                            },
                        },
                        _ => {
                            panic!("fallback must not issue any spawn, input, or unrelated request")
                        }
                    };
                    if write_frame(
                        &mut server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(request.request_id),
                            proof: proof.clone(),
                            message,
                        },
                    )
                    .is_err()
                    {
                        break;
                    }
                }
                requests
            });
            let mut client = ControlClient::from_stream(client).unwrap();
            let result = complete(
                &mut client,
                reservation,
                &accepted,
                TerminalConfig::default().into(),
            );
            match case {
                "pending" | "exited" | "revision-conflict" => {
                    let (hosted, notice) = result.unwrap();
                    assert_eq!(hosted.session.id, terminal);
                    assert!(notice.is_some());
                }
                "peer-saved" => assert!(result.unwrap().1.is_none()),
                _ => assert!(result.is_err(), "{case} is not save-only partial success"),
            }
            drop(client);
            assert_eq!(serving.join().unwrap(), ["complete"]);
        }
    }
}
