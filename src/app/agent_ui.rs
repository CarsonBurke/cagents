use super::*;
use crate::agent::Provider;

/// Whether a hosted terminal runs an agent whose activity the daemon can
/// observe, which is what the sidebar's spinner for that terminal needs.
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
    matches!(provider, Some(Provider::Claude | Provider::Codex))
}

#[derive(Debug, Clone)]
pub(super) enum CapacityAction {
    Refresh,
    CloseInterrupted,
    ContinueClosing(crate::model::AgentId),
    Received(Arc<Result<crate::agent_execution::Reply, String>>),
}
#[derive(Default)]
pub(super) struct CapacityState {
    report: Option<crate::agent_execution::CapacityReport>,
    pending: bool,
    closing: bool,
    cleanup_cursor: Option<crate::model::AgentId>,
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

    pub(super) fn update_agent_capacity(&mut self, action: CapacityAction) -> Task<Message> {
        use crate::agent_execution::{Operation, Reply};
        let operation = match action {
            CapacityAction::Received(result) => {
                self.agent_capacity.pending = false;
                match result.as_ref() {
                    Ok(Reply::Capacity(report)) => {
                        self.agent_capacity.report = Some(report.clone())
                    }
                    Ok(Reply::InterruptedClosed(result)) => {
                        for failure in &result.failed {
                            self.notices.record(TransientNotice::warning(format!("Could not close interrupted agent {}: {}", failure.id, failure.message)));
                        }
                        if let Some(report) = &mut self.agent_capacity.report {
                            for capacity in [&mut report.caller, &mut report.global] {
                                capacity.holders.retain(|holder| !result.closed.contains(&holder.id));
                                capacity.running = capacity.holders.len();
                            }
                        }
                        if let Some(next) = result.next {
                            if self.agent_capacity.cleanup_cursor.is_some_and(|previous| next <= previous) {
                                self.notices.record(TransientNotice::warning("Interrupted cleanup did not advance; remaining agents were left open"));
                            } else {
                                self.agent_capacity.cleanup_cursor = Some(next);
                                return self.update_agent_capacity(CapacityAction::ContinueClosing(next));
                            }
                        }
                        return self.update_agent_capacity(CapacityAction::Refresh);
                    }
                    Err(error) => {
                        if self.agent_capacity.closing {
                            self.notices.record(TransientNotice::warning(format!(
                                "Could not close interrupted agents: {error}"
                            )));
                        }
                        self.agent_capacity.report = None;
                        tracing::debug!(%error, "Agent capacity unavailable");
                    }
                    _ => {}
                }
                return Task::none();
            }
            CapacityAction::Refresh
                if !self.settings.show_status_bar || !self.refresh_ui.focused =>
            {
                return Task::none();
            }
            _ if self.agent_capacity.pending => return Task::none(),
            CapacityAction::Refresh => Operation::Capacity,
            CapacityAction::CloseInterrupted => {
                self.agent_capacity.cleanup_cursor = None;
                Operation::CloseInterrupted { all: true, after: None }
            }
            CapacityAction::ContinueClosing(after) => Operation::CloseInterrupted { all: true, after: Some(after) },
        };
        self.agent_capacity.closing = matches!(operation, Operation::CloseInterrupted { .. });
        self.agent_capacity.pending = true;
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                // Observing capacity never starts a daemon.
                let socket =
                    crate::terminal_ipc::daemon_socket_path().map_err(|error| error.to_string())?;
                let mut client = crate::control::ControlClient::connect(&socket)
                    .map_err(|error| error.to_string())?;
                client
                    .agent(crate::agent_execution::Authority::default(), operation)
                    .map_err(|error| error.to_string())
            })
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
            Message::AgentCapacity(CapacityAction::Received(Arc::new(result)))
        })
    }

    pub(super) fn agent_capacity_control(&self) -> Element<'_, Message> {
        use crate::agent_execution::State;
        let Some(report) = &self.agent_capacity.report else {
            return widget::row::with_children(Vec::new()).into();
        };
        let capacity = &report.global;
        let interrupted = capacity
            .holders
            .iter()
            .filter(|holder| holder.state == State::Interrupted)
            .count();
        let label = if interrupted == 0 {
            format!("{} of {} agents running", capacity.running, capacity.limit)
        } else {
            format!(
                "{} of {} agent slots · {} interrupted",
                capacity.running, capacity.limit, interrupted
            )
        };
        // Keep tooltip layout bounded even when the configured cap is large.
        // Interrupted ownership is the actionable part, so show it first.
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
        let remaining = capacity.holders.len().saturating_sub(12);
        let more = if remaining > 0 {
            format!("\n{remaining} additional reservations")
        } else {
            String::new()
        };
        let detail = format!(
            "{} reservations; interrupted agents retain capacity until closed.\n{}{}",
            capacity.running, holders, more
        );
        let mut controls: Vec<Element<'_, Message>> =
            vec![crate::interaction::tooltip(design::body(label), detail)];
        if interrupted > 0 {
            controls.push(
                crate::ui_controls::icon_button(
                    Icon::Close,
                    "Close interrupted agents across all callers",
                )
                .on_press_maybe(
                    (!self.agent_capacity.pending)
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
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut element = app.agent_capacity_control();
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(1000.0, 80.0)),
        );
        let accessible =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let labels = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter_map(|node| node.node().label())
            .collect::<Vec<_>>();
        assert!(
            labels
                .iter()
                .any(|label| label.contains("1 of 32") && label.contains("1 interrupted")),
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
