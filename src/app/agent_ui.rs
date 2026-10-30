use super::*;
use crate::agent::Provider;

/// Whether a hosted terminal needs an attachment for background agent metadata.
/// Claude/Codex use daemon observers; Omp publishes foreground-verified title
/// activity. Both reach the sidebar through the same hidden attachment.
pub(super) fn observes_terminal_agent(session: &TerminalSession) -> bool {
    if session.continuity.live_proof().is_none() {
        return false;
    }
    let provider = session
        .resume_hint
        .as_deref()
        .and_then(crate::model::agent_recovery::observed_launch_from_hint)
        .as_ref()
        .and_then(Provider::recognize_launch)
        .or_else(|| Provider::recognize_launch(&session.launch));
    matches!(
        provider,
        Some(Provider::Claude | Provider::Codex | Provider::Omp)
    )
}

#[cfg(test)]
mod tests;
#[derive(Debug, Clone)]
pub(super) enum CapacityAction {
    Refresh,
    CloseInterrupted,
    Received {
        request: u64,
        result: Arc<Result<crate::agent_execution::Reply, String>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapacityRequest {
    Report,
    Close(Option<crate::model::AgentId>),
}

pub(super) struct CapacityState {
    // The app's original authority is never replaced by a User default on a poll.
    authority: crate::agent_execution::Authority,
    report: Option<crate::agent_execution::CapacityReport>,
    pending: Option<(u64, CapacityRequest)>,
    sequence: u64,
    closed: usize,
    failed: usize,
    cleanup_note: Option<String>,
    unavailable: bool,
}
impl CapacityState {
    pub(super) fn installation() -> Self {
        Self {
            authority: crate::agent_execution::Authority::from_environment(),
            report: None,
            pending: None,
            sequence: 0,
            closed: 0,
            failed: 0,
            cleanup_note: None,
            unavailable: false,
        }
    }

    fn can_close_all(&self) -> bool {
        self.authority.token().is_none()
    }
}
impl Default for CapacityState {
    fn default() -> Self {
        Self::installation()
    }
}

/// A page acknowledges at most four distinct attempts. Never infer a successful
/// close, skip failures, or follow an unproved cursor after a partial reply.
fn cleanup_page_valid(
    page: &crate::agent_execution::CloseInterrupted,
    after: Option<crate::model::AgentId>,
) -> bool {
    let ids = page
        .closed
        .iter()
        .copied()
        .chain(page.failed.iter().map(|failure| failure.id))
        .collect::<std::collections::BTreeSet<_>>();
    let count = page.closed.len() + page.failed.len();
    count <= 4
        && ids.len() == count
        && ids.iter().all(|id| after.is_none_or(|prior| *id > prior))
        && page
            .next
            .is_none_or(|next| count == 4 && ids.last() == Some(&next))
}

impl Cagents {
    pub(super) fn agent_capacity_subscription(&self) -> Subscription<Message> {
        if self.settings.show_status_bar && self.refresh_ui.focused {
            cosmic::iced::time::every(Duration::from_secs(5))
                .map(|_| Message::AgentCapacity(CapacityAction::Refresh))
        } else {
            Subscription::none()
        }
    }

    fn request_agent_capacity(&mut self, request: CapacityRequest) -> Task<Message> {
        use crate::agent_execution::Operation;
        if self.agent_capacity.pending.is_some() {
            return Task::none();
        }
        let Some(sequence) = self.agent_capacity.sequence.checked_add(1) else {
            return Task::none();
        };
        self.agent_capacity.sequence = sequence;
        self.agent_capacity.pending = Some((sequence, request));
        let authority = self.agent_capacity.authority.clone();
        let operation = match request {
            CapacityRequest::Report => Operation::Capacity,
            CapacityRequest::Close(after) => Operation::CloseInterrupted { all: true, after },
        };
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                // Local observation and recovery never start a daemon.
                let socket =
                    crate::terminal_ipc::daemon_socket_path().map_err(|error| error.to_string())?;
                let mut client = crate::control::ControlClient::connect(&socket)
                    .map_err(|error| error.to_string())?;
                client
                    .agent(authority, operation)
                    .map_err(|error| error.to_string())
            })
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
            Message::AgentCapacity(CapacityAction::Received {
                request: sequence,
                result: Arc::new(result),
            })
        })
    }

    pub(super) fn update_agent_capacity(&mut self, action: CapacityAction) -> Task<Message> {
        use crate::agent_execution::Reply;
        match action {
            CapacityAction::Refresh => {
                if !self.settings.show_status_bar || !self.refresh_ui.focused {
                    return Task::none();
                }
                self.request_agent_capacity(CapacityRequest::Report)
            }
            CapacityAction::CloseInterrupted => {
                if !self.agent_capacity.can_close_all() || self.agent_capacity.pending.is_some() {
                    return Task::none();
                }
                self.agent_capacity.closed = 0;
                self.agent_capacity.failed = 0;
                self.agent_capacity.cleanup_note =
                    Some("Closing interrupted agents across all callers…".into());
                self.request_agent_capacity(CapacityRequest::Close(None))
            }
            CapacityAction::Received { request, result } => {
                let Some((current, kind)) = self.agent_capacity.pending else {
                    return Task::none();
                };
                if request != current {
                    return Task::none();
                }
                self.agent_capacity.pending = None;
                match (kind, result.as_ref()) {
                    (CapacityRequest::Report, Ok(Reply::Capacity(report))) => {
                        self.agent_capacity.report = Some(report.clone());
                        self.agent_capacity.unavailable = false;
                    }
                    (CapacityRequest::Close(after), Ok(Reply::InterruptedClosed(page)))
                        if cleanup_page_valid(page, after) =>
                    {
                        self.agent_capacity.closed += page.closed.len();
                        self.agent_capacity.failed += page.failed.len();
                        for failure in &page.failed {
                            self.notices.record(TransientNotice::warning(format!(
                                "Could not close interrupted agent {}: {}",
                                failure.id, failure.message
                            )));
                        }
                        if let Some(next) = page.next {
                            return self.request_agent_capacity(CapacityRequest::Close(Some(next)));
                        }
                        self.agent_capacity.cleanup_note = Some(format!(
                            "{} interrupted agents closed; {} could not be closed. Retry checks remaining interrupted agents across all callers.",
                            self.agent_capacity.closed, self.agent_capacity.failed,
                        ));
                        // Only a new authoritative report changes the displayed count.
                        // Complete this read even if the app lost focus during cleanup.
                        return self.request_agent_capacity(CapacityRequest::Report);
                    }
                    (kind, result) => {
                        self.agent_capacity.unavailable = true;
                        let error = match result {
                            Err(error) => error.as_str(),
                            Ok(_) => "Invalid capacity response; no further cleanup was attempted",
                        };
                        if matches!(kind, CapacityRequest::Close(_)) {
                            self.agent_capacity.cleanup_note = Some(format!(
                                "{} closes confirmed; {} failed. Cleanup stopped: {error}. Retry checks current interrupted agents across all callers; unacknowledged outcomes remain unknown.",
                                self.agent_capacity.closed, self.agent_capacity.failed,
                            ));
                            self.notices.record(TransientNotice::warning(
                                self.agent_capacity.cleanup_note.clone().unwrap(),
                            ));
                            return self.request_agent_capacity(CapacityRequest::Report);
                        }
                        tracing::debug!(%error, "Local agent capacity unavailable");
                    }
                }
                Task::none()
            }
        }
    }

    pub(super) fn agent_capacity_control(&self) -> Element<'_, Message> {
        use crate::agent_execution::State;
        let Some(report) = &self.agent_capacity.report else {
            return crate::interaction::tooltip(
                design::caption("Local slots unavailable"),
                "Capacity is read from the existing local daemon; no daemon is started. It will be checked again while this window is focused.",
            ).into();
        };
        let capacity = &report.global;
        let interrupted = capacity
            .holders
            .iter()
            .filter(|holder| holder.state == State::Interrupted)
            .count();
        let compact = self.window_width < design::OVERLAY_DOCK_BREAKPOINT;
        let label = if compact {
            format!(
                "{}/{} local slots · {} interrupted",
                capacity.running, capacity.limit, interrupted
            )
        } else if interrupted == 0 {
            format!(
                "{} of {} local agent slots",
                capacity.running, capacity.limit
            )
        } else {
            format!(
                "{} of {} local slots · {} interrupted",
                capacity.running, capacity.limit, interrupted
            )
        };
        let label = if self.agent_capacity.unavailable {
            format!("{label} · last count")
        } else {
            label
        };
        let holders = capacity
            .holders
            .iter()
            .filter(|holder| holder.state == State::Interrupted)
            .chain(
                capacity
                    .holders
                    .iter()
                    .filter(|holder| holder.state != State::Interrupted),
            )
            .take(12)
            .map(|holder| format!("{} {:?}", holder.id, holder.state))
            .collect::<Vec<_>>()
            .join("\n");
        let more = capacity.holders.len().saturating_sub(12);
        let freshness = if self.agent_capacity.unavailable {
            "Latest read unavailable; showing the last accepted count. "
        } else if self.agent_capacity.pending.is_some() {
            "Checking local capacity. "
        } else {
            ""
        };
        let scope = if self.agent_capacity.can_close_all() {
            "Close interrupted affects all callers on this local daemon, including automations. Active agents are kept."
        } else {
            "This app has session authority. All-caller cleanup is available only to an ordinary User app; the original authority is preserved."
        };
        let detail = format!(
            "{freshness}Accepted, running, waiting and interrupted agents reserve slots. Interrupted agents retain their reservation until closed. {scope}\n{holders}\n{more} additional reservations\n{}",
            self.agent_capacity.cleanup_note.as_deref().unwrap_or(""),
        );
        let mut controls: Vec<Element<'_, Message>> =
            vec![crate::interaction::tooltip(design::caption(label), detail)];
        if interrupted > 0 && self.agent_capacity.can_close_all() {
            controls.push(
                widget::button::custom(design::caption("Close interrupted · all callers"))
                    .name("Close interrupted agents across all callers on this local daemon")
                    .class(crate::ui_controls::control_style(false, None))
                    .height(20)
                    .padding(Padding::from([0.0, space::XS]))
                    .on_press_maybe(
                        self.agent_capacity
                            .pending
                            .is_none()
                            .then_some(Message::AgentCapacity(CapacityAction::CloseInterrupted)),
                    )
                    .into(),
            );
        }
        widget::row::with_children(controls)
            .spacing(space::XS)
            .align_y(Alignment::Center)
            .into()
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;
    use crate::agent_execution::{Capacity, CapacityHolder, CapacityReport, CapacityScope, State};
    #[test]
    fn capacity_control_exposes_count_and_interrupted_cleanup() {
        use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.agent_capacity.authority = crate::agent_execution::Authority::default();
        let holder = CapacityHolder {
            id: crate::model::AgentId::new(),
            run: crate::model::RunId::new(),
            state: State::Interrupted,
        };
        let global = Capacity {
            running: 1,
            limit: 32,
            scope: CapacityScope::Global,
            holders: vec![holder],
        };
        app.agent_capacity.report = Some(CapacityReport {
            caller: Capacity {
                running: 0,
                limit: 8,
                scope: CapacityScope::Caller,
                holders: vec![],
            },
            global,
        });
        for (width, count) in [(1280.0, "1 of 32"), (800.0, "1/32")] {
            app.window_width = width;
            let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let mut element = app.agent_capacity_control();
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(1000.0, 80.0)),
            );
            assert!(
                node.size().height <= 20.0,
                "capacity control must fit the 24px status bar"
            );
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                mouse::Cursor::Unavailable,
            );
            let labels = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .filter_map(|node| node.node().label())
                .collect::<Vec<_>>();
            assert!(
                labels
                    .iter()
                    .any(|label| label.contains(count) && label.contains("1 interrupted")),
                "{labels:?}"
            );
            assert!(
                labels
                    .iter()
                    .any(|label| label.contains("Close interrupted agents across all callers")),
                "{labels:?}"
            );
        }
    }
    #[test]
    fn capacity_cleanup_correlates_replies_and_refreshes_partial_results() {
        use crate::agent_execution::{CloseFailure, CloseInterrupted, Reply};
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.agent_capacity.authority = crate::agent_execution::Authority::default();
        let closed = crate::model::AgentId::new();
        let failed = crate::model::AgentId::new();
        app.agent_capacity.pending = Some((1, CapacityRequest::Close(None)));
        app.agent_capacity.sequence = 1;
        let page = CloseInterrupted {
            closed: vec![closed],
            failed: vec![CloseFailure::new(failed, "owner still active")],
            next: None,
        };
        let _ = app.update_agent_capacity(CapacityAction::Received {
            request: 0,
            result: Arc::new(Ok(Reply::InterruptedClosed(page.clone()))),
        });
        assert_eq!(
            app.agent_capacity.pending,
            Some((1, CapacityRequest::Close(None)))
        );
        assert_eq!(app.agent_capacity.closed, 0);
        let _ = app.update_agent_capacity(CapacityAction::Received {
            request: 1,
            result: Arc::new(Ok(Reply::InterruptedClosed(page))),
        });
        assert_eq!(app.agent_capacity.closed, 1);
        assert_eq!(app.agent_capacity.failed, 1);
        assert_eq!(
            app.agent_capacity.pending,
            Some((2, CapacityRequest::Report))
        );
        assert!(
            app.agent_capacity.report.is_none(),
            "a close reply is not a capacity snapshot"
        );
        let report = CapacityReport {
            caller: Capacity {
                running: 1,
                limit: 8,
                scope: CapacityScope::Caller,
                holders: vec![],
            },
            global: Capacity {
                running: 2,
                limit: 32,
                scope: CapacityScope::Global,
                holders: vec![],
            },
        };
        let _ = app.update_agent_capacity(CapacityAction::Received {
            request: 2,
            result: Arc::new(Ok(Reply::Capacity(report))),
        });
        assert_eq!(
            app.agent_capacity.report.as_ref().unwrap().global.running,
            2
        );
        assert!(app.agent_capacity.pending.is_none());
        let _ = app.update_agent_capacity(CapacityAction::CloseInterrupted);
        assert_eq!(
            app.agent_capacity.pending,
            Some((3, CapacityRequest::Close(None))),
            "retry must start a fresh checked scan"
        );
    }

    #[test]
    fn capacity_cleanup_rejects_duplicate_or_unproved_cursor_pages() {
        use crate::agent_execution::{CloseFailure, CloseInterrupted};
        let mut ids = (0..5)
            .map(|_| crate::model::AgentId::new())
            .collect::<Vec<_>>();
        ids.sort();
        let valid = CloseInterrupted {
            closed: ids[..3].to_vec(),
            failed: vec![CloseFailure::new(ids[3], "not closed")],
            next: Some(ids[3]),
        };
        assert!(cleanup_page_valid(&valid, None));
        let mut duplicate = valid.clone();
        duplicate.failed[0].id = ids[0];
        assert!(!cleanup_page_valid(&duplicate, None));
        let mut cursor = valid.clone();
        cursor.next = Some(ids[4]);
        assert!(!cleanup_page_valid(&cursor, None));
        assert!(!cleanup_page_valid(&valid, Some(ids[0])));
        let oversized = CloseInterrupted {
            closed: ids,
            failed: vec![],
            next: None,
        };
        assert!(!cleanup_page_valid(&oversized, None));
        let short = CloseInterrupted {
            closed: valid.closed[..1].to_vec(),
            failed: vec![],
            next: valid.next,
        };
        assert!(!cleanup_page_valid(&short, None));
    }

    #[test]
    fn capacity_cleanup_lost_reply_keeps_confirmed_progress_and_requires_new_report() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.agent_capacity.closed = 4;
        app.agent_capacity.failed = 1;
        app.agent_capacity.pending = Some((
            7,
            CapacityRequest::Close(Some(crate::model::AgentId::new())),
        ));
        app.agent_capacity.sequence = 7;
        let _ = app.update_agent_capacity(CapacityAction::Received {
            request: 7,
            result: Arc::new(Err("reply unavailable".into())),
        });
        assert_eq!(app.agent_capacity.closed, 4);
        assert_eq!(app.agent_capacity.failed, 1);
        assert_eq!(
            app.agent_capacity.pending,
            Some((8, CapacityRequest::Report))
        );
        assert!(
            app.agent_capacity
                .cleanup_note
                .as_deref()
                .unwrap()
                .contains("unacknowledged outcomes remain unknown")
        );
    }

    #[test]
    fn capacity_cleanup_preserves_scoped_authority_and_modal_admission() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        // Even an empty issued value is Some: it must never become User.
        app.agent_capacity.authority = crate::agent_execution::Authority::session(String::new());
        assert!(!app.agent_capacity.can_close_all());
        let _ = app.update_agent_capacity(CapacityAction::CloseInterrupted);
        assert!(app.agent_capacity.pending.is_none());
        assert!(user_workspace_message_blocked_while_modal(
            &Message::AgentCapacity(CapacityAction::CloseInterrupted)
        ));
        assert!(user_message_blocked_during_update(&Message::AgentCapacity(
            CapacityAction::CloseInterrupted
        )));
        for action in [
            CapacityAction::Refresh,
            CapacityAction::Received {
                request: 1,
                result: Arc::new(Err("fixture".into())),
            },
        ] {
            assert!(!user_workspace_message_blocked_while_modal(
                &Message::AgentCapacity(action.clone())
            ));
            assert!(!user_message_blocked_during_update(
                &Message::AgentCapacity(action)
            ));
        }
    }

    #[test]
    fn capacity_settings_use_shared_validation_and_preserve_unsaved_drafts() {
        let settings = AppSettings::default();
        let mut draft = SettingsDraft::from_settings(&settings);
        assert_eq!(draft.agent_caller_limit, "8");
        assert_eq!(draft.agent_global_limit, "32");
        draft.set(SettingsField::AgentCallerLimit, "not yet valid".into());
        assert_eq!(draft.agent_caller_limit, "not yet valid");
        assert_eq!(draft.agent_global_limit, "32");
        for field in [
            SettingsField::AgentCallerLimit,
            SettingsField::AgentGlobalLimit,
        ] {
            assert_eq!(field.category(), SettingsCategory::Agents);
            for value in ["0", "4097", "-1", "3.5", ""] {
                assert!(
                    matches!(
                        settings_update(&settings, field, value),
                        SettingsDecision::Invalid(_)
                    ),
                    "{field:?} {value}"
                );
            }
            for value in ["1", "4096"] {
                let SettingsDecision::Persist(next) = settings_update(&settings, field, value)
                else {
                    panic!("valid limit rejected: {field:?} {value}")
                };
                assert_eq!(
                    next.terminal_scrollback_mib,
                    settings.terminal_scrollback_mib
                );
                match field {
                    SettingsField::AgentCallerLimit => {
                        assert_eq!(next.agent_concurrency.caller.to_string(), value);
                        assert_eq!(next.agent_concurrency.global, 32);
                    }
                    _ => {
                        assert_eq!(next.agent_concurrency.global.to_string(), value);
                        assert_eq!(next.agent_concurrency.caller, 8);
                    }
                }
            }
        }
    }
    #[test]
    fn capacity_poll_requires_visible_focus_and_coalesces_inflight_requests() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.settings.show_status_bar = true;
        app.refresh_ui.focused = false;
        let _ = app.update_agent_capacity(CapacityAction::Refresh);
        assert!(app.agent_capacity.pending.is_none());
        app.refresh_ui.focused = true;
        app.settings.show_status_bar = false;
        let _ = app.update_agent_capacity(CapacityAction::Refresh);
        assert!(app.agent_capacity.pending.is_none());
        app.settings.show_status_bar = true;
        let _ = app.update_agent_capacity(CapacityAction::Refresh);
        assert_eq!(
            app.agent_capacity.pending,
            Some((1, CapacityRequest::Report))
        );
        let _ = app.update_agent_capacity(CapacityAction::Refresh);
        let _ = app.update_agent_capacity(CapacityAction::CloseInterrupted);
        assert_eq!(app.agent_capacity.sequence, 1);
        assert_eq!(
            app.agent_capacity.pending,
            Some((1, CapacityRequest::Report))
        );
    }
}
