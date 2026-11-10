//! The terminal and its tests render the same reviewed outcome record.

use super::{
    impact::{Disposition, Preview, SessionKind},
    source_inspection::SavedOutcome,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
};

const CHAT_RECOVERY: &str = "Choose Resume conversation in the new tab (+) menu";

/// Reconcile observations before grouping: a pane has exactly one outcome.
#[derive(Debug)]
struct Item {
    #[cfg(test)]
    id: crate::model::TerminalId,
    label: String,
    kind: SessionKind,
    outcome: Outcome,
    detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    KeepsRunning,
    Resumes,
    Working,
    Interrupted,
    Paused,
    Unchanged,
    Attention,
}

fn items(preview: &Preview) -> Vec<Item> {
    let sessions: BTreeMap<_, _> = preview
        .daemon
        .iter()
        .flat_map(|daemon| &daemon.sessions)
        .map(|session| (session.id, session))
        .collect();
    let stopped: BTreeMap<_, _> = preview
        .stopped
        .iter()
        .map(|pane| (pane.pane, pane))
        .collect();
    let ids: BTreeSet<_> = preview
        .labels
        .names
        .keys()
        .chain(preview.labels.details.panes.keys())
        .chain(sessions.keys())
        .chain(stopped.keys())
        .copied()
        .collect();
    let mut items: Vec<_> = ids
        .into_iter()
        .map(|id| {
            let live = sessions.get(&id);
            let saved = preview.labels.details.panes.get(&id);
            let stopped = stopped.get(&id);
            let label = preview
                .labels
                .names
                .get(&id)
                .map(String::as_str)
                .or_else(|| live.map(|session| session.title.as_str()))
                .or_else(|| stopped.map(|pane| pane.label.as_str()))
                .unwrap_or("Conversation")
                .to_owned();
            let kind = live
                .map(|session| session.kind)
                .or_else(|| saved.map(|pane| pane.kind))
                .unwrap_or(SessionKind::NativeChat);
            let mut outcome = live
                .map(|session| match session.disposition {
                    Disposition::Continues => Outcome::KeepsRunning,
                    Disposition::Resumes => Outcome::Resumes,
                    Disposition::WaitsForIdle => Outcome::Working,
                    Disposition::Interrupted => Outcome::Interrupted,
                    Disposition::ResumePicker => Outcome::Paused,
                    Disposition::Blocked => Outcome::Attention,
                })
                .unwrap_or_else(|| match saved.map(|pane| pane.outcome) {
                    Some(SavedOutcome::KeepsRunning) => Outcome::KeepsRunning,
                    Some(SavedOutcome::Resumes) => Outcome::Resumes,
                    _ => Outcome::Unchanged,
                });
            let mut detail = saved.map(|pane| pane.detail.clone()).unwrap_or_default();
            // The chat panel can disconnect while its console's process carries
            // on. That is continuity, even if the chat-only source says resume.
            if saved.is_some_and(|pane| {
                pane.outcome == SavedOutcome::KeepsRunning && !pane.detail.is_empty()
            }) && matches!(
                outcome,
                Outcome::Resumes | Outcome::Paused | Outcome::Unchanged
            ) {
                outcome = Outcome::KeepsRunning;
            }
            if !detail.is_empty()
                && matches!(
                    outcome,
                    Outcome::Working | Outcome::Interrupted | Outcome::Attention
                )
                && let Some(live) = live
            {
                detail = format!("{}; {detail}", live_fact(live));
            }

            if let Some(stopped) = stopped {
                // Live work and handoff problems take precedence, with the saved
                // recovery on the same line rather than a second group entry.
                let recovery = recovery_detail(
                    &stopped.reason,
                    &stopped.recovery,
                    matches!(
                        outcome,
                        Outcome::Working | Outcome::Interrupted | Outcome::Attention
                    ),
                );
                if outcome == Outcome::Interrupted {
                    outcome = Outcome::Paused;
                    let interruption = live.map(|session| live_fact(session)).unwrap_or_default();
                    detail = format!(
                        "{interruption}; after the update: {}",
                        recovery_detail(&stopped.reason, &stopped.recovery, false)
                    );
                } else if matches!(outcome, Outcome::Working | Outcome::Attention) {
                    let reason = live.map(|session| live_fact(session)).unwrap_or_default();
                    detail = format!("{reason}; after the update: {recovery}");
                } else {
                    outcome = Outcome::Paused;
                    detail = recovery;
                }
            } else if detail.is_empty() {
                if let Some(live) = live {
                    if outcome == Outcome::Paused {
                        detail = recovery_detail(&live.reason, CHAT_RECOVERY, false);
                    } else if matches!(
                        outcome,
                        Outcome::Attention | Outcome::Working | Outcome::Interrupted
                    ) {
                        detail = live_fact(live);
                    }
                }
            }
            Item {
                #[cfg(test)]
                id,
                label,
                kind,
                outcome,
                detail,
            }
        })
        .collect();
    items.sort_by(|left, right| left.label.cmp(&right.label));
    items
}

/// Describe what happens to the reviewed panes without consulting live state.
pub fn render_review(preview: &Preview) -> String {
    let mut output = format!(
        "Update cagents {} -> {}\n",
        preview.from_version, preview.candidate.version
    );
    if preview.windows != 0 {
        let (windows, action) = if preview.windows == 1 {
            ("window", "closes and reopens")
        } else {
            ("windows", "close and reopen")
        };
        let panes = if preview.stopped_windows.is_empty() {
            " with the same panes"
        } else {
            ""
        };
        writeln!(output, "{} {windows} {action}{panes}.", preview.windows).unwrap();
    }
    let items = items(preview);
    let mut groups = String::new();
    for (outcome, name, description) in [
        (Outcome::KeepsRunning, "Keep running", String::new()),
        (
            Outcome::Resumes,
            "Resumes",
            "restart on their recorded conversation".into(),
        ),
        (
            Outcome::Unchanged,
            "Unchanged",
            "ended conversations, drafts and inactive panes stay as they are".into(),
        ),
        (
            Outcome::Working,
            "Working now",
            "finishing a turn; the update waits".into(),
        ),
        (
            Outcome::Interrupted,
            "Interrupted and resumed",
            "stop their current turn, then restart on the same conversation".into(),
        ),
        (
            Outcome::Paused,
            "Come back paused",
            "nothing is started; choose Resume conversation in the new tab (+) menu".into(),
        ),
        (Outcome::Attention, "Needs attention", String::new()),
    ] {
        let members: Vec<_> = items
            .iter()
            .filter(|item| item.outcome == outcome)
            .collect();
        let description = if outcome == Outcome::KeepsRunning {
            let terminals = members
                .iter()
                .filter(|item| item.kind == SessionKind::Terminal)
                .count();
            let agents = members.len() - terminals;
            let counts: Vec<_> = [(terminals, "terminal"), (agents, "agent")]
                .into_iter()
                .filter(|(count, _)| *count > 0)
                .map(|(count, kind)| format!("{count} {kind}{}", if count == 1 { "" } else { "s" }))
                .collect();
            format!("{}; nothing restarts", counts.join(", "))
        } else {
            description
        };
        let expanded = matches!(
            outcome,
            Outcome::Working | Outcome::Interrupted | Outcome::Paused | Outcome::Attention
        );
        let lines: Vec<_> = members
            .iter()
            .filter(|item| expanded || !item.detail.is_empty())
            .map(|item| {
                if item.detail.is_empty() {
                    item.label.clone()
                } else {
                    format!("{}: {}", item.label, item.detail)
                }
            })
            .collect();
        group(&mut groups, name, members.len(), &description, &lines);
    }
    let mut notices = preview.labels.details.notices.clone();
    notices.extend(
        preview
            .stopped_windows
            .iter()
            .map(|window| recovery_detail(&window.reason, &window.recovery, true)),
    );
    if let Some(daemon) = &preview.daemon {
        notices.extend(daemon.blockers.iter().cloned());
    }
    if !groups.is_empty() || !notices.is_empty() {
        output.push('\n');
    }
    output.push_str(&groups);
    for notice in notices {
        writeln!(output, "  {notice}").unwrap();
    }
    output
}

/// A source reason can combine live activity with its old destination plan.
/// When merging outcomes, carry only activity; this review owns the destination.
fn live_fact(session: &super::impact::SessionImpact) -> String {
    match session.disposition {
        Disposition::WaitsForIdle => "finishing its turn".into(),
        Disposition::Interrupted => {
            let tasks = session.interrupted_subagents.len();
            if tasks == 0 {
                "its current turn is interrupted".into()
            } else {
                let noun = if tasks == 1 { "task" } else { "tasks" };
                format!(
                    "its current turn and {tasks} background {noun} stop; background work will not resume automatically"
                )
            }
        }
        _ => clause(&session.reason),
    }
}

fn clause(reason: &str) -> String {
    let reason = reason.trim_end_matches(['.', ' ', '\n']);
    if [
        "Its ",
        "The ",
        "Could ",
        "Ended ",
        "Keeps ",
        "Finishing ",
        "Restarts ",
    ]
    .iter()
    .any(|prefix| reason.starts_with(prefix))
    {
        let mut chars = reason.chars();
        format!("{}{}", chars.next().unwrap().to_lowercase(), chars.as_str())
    } else {
        reason.into()
    }
}

fn recovery_detail(reason: &str, recovery: &str, include_default: bool) -> String {
    let reason = clause(reason);
    if recovery == CHAT_RECOVERY && !include_default {
        return reason;
    }
    format!("{reason}. {recovery}")
}

fn group(output: &mut String, name: &str, count: usize, description: &str, members: &[String]) {
    if count == 0 {
        return;
    }
    let heading = format!("{name} ({count})");
    if description.is_empty() {
        writeln!(output, "  {heading}").unwrap();
    } else {
        writeln!(
            output,
            "  {heading:width$}{description}",
            width = 24.max(heading.len() + 3)
        )
        .unwrap();
    }
    for member in members {
        writeln!(output, "    {member}").unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, TerminalId},
        terminal_ipc::ShutdownCapability,
        update::{
            impact::{Candidate, DaemonImpact, Policy, SessionImpact, SessionKind},
            source_inspection::{StoppedKind, StoppedPane, StoppedWindow},
        },
    };

    fn preview() -> Preview {
        Preview {
            candidate: Candidate {
                version: "0.1.3+local.d2509365".into(),
                protocol: 160,
                digest: "reviewed-build".into(),
            },
            from_version: "0.1.3+local.991daa43e".into(),
            windows: 2,
            replaces_daemon: true,
            daemon: Some(DaemonImpact {
                capability: ShutdownCapability {
                    proof: HostSessionProof {
                        host_session_id: HostSessionId::new("host"),
                        boot_id: BootId::new("boot"),
                        host_incarnation: HostIncarnation::new("daemon"),
                    },
                    token: "review".into(),
                    inventory_generation: 1,
                    live_terminals: 1,
                    background_subagents: 0,
                    background_identity: String::new(),
                    expires_at_unix_ms: 100,
                },
                policy: Policy::WhenIdle,
                sessions: Vec::new(),
                blockers: Vec::new(),
            }),
            stopped: Vec::new(),
            stopped_windows: Vec::new(),
            labels: Default::default(),
        }
    }

    fn session(title: &str, disposition: Disposition, reason: &str) -> SessionImpact {
        SessionImpact {
            id: TerminalId::new(),
            kind: SessionKind::NativeChat,
            title: title.into(),
            identity: "reviewed-session".into(),
            disposition,
            reason: reason.into(),
            interrupted_subagents: Vec::new(),
        }
    }

    fn saved(
        preview: &mut Preview,
        title: &str,
        outcome: SavedOutcome,
    ) -> crate::model::TerminalId {
        let id = crate::model::TerminalId::new();
        preview.labels.names.insert(id, title.into());
        preview.labels.details.panes.insert(
            id,
            super::super::source_inspection::InspectedPane {
                kind: SessionKind::NativeChat,
                outcome,
                detail: String::new(),
            },
        );
        id
    }

    fn representative() -> Preview {
        let mut preview = preview();
        for title in ["shell", "build", "logs", "tests", "watch"] {
            let mut terminal = session(title, Disposition::Continues, "Keeps running");
            terminal.kind = SessionKind::Terminal;
            preview.daemon.as_mut().unwrap().sessions.push(terminal);
        }
        // Two provider-default consoles in one project need distinct labels.
        for started in [1_794_232_920, 1_794_233_820] {
            let title = crate::update::source_inspection::pane_label(
                "Claude Code",
                None,
                "alpha",
                true,
                Some(started),
            );
            let id = saved(&mut preview, &title, SavedOutcome::KeepsRunning);
            assert!(crate::update::source_inspection::is_untitled(
                "Claude Code",
                true
            ));
            preview.labels.details.panes.get_mut(&id).unwrap().detail =
                "keeps running in its terminal; its chat panel disconnects. Choose Resume conversation in the new tab (+) menu afterwards.".into();
            let mut console = session(&title, Disposition::Continues, "Keeps running");
            console.id = id;
            preview.daemon.as_mut().unwrap().sessions.push(console);
        }
        let working = session(
            "rollout · Release · alpha",
            Disposition::WaitsForIdle,
            "Finishing its turn",
        );
        preview
            .labels
            .names
            .insert(working.id, working.title.clone());
        preview.daemon.as_mut().unwrap().sessions.push(working);
        let stopped = saved(
            &mut preview,
            "parser · General · beta",
            SavedOutcome::Resumes,
        );
        preview.stopped.push(StoppedPane {
            pane: stopped,
            kind: StoppedKind::Chat,
            label: "old title".into(),
            reason: "its conversation could not be confirmed".into(),
            recovery: CHAT_RECOVERY.into(),
        });
        saved(
            &mut preview,
            "ended · General · beta",
            SavedOutcome::Unchanged,
        );
        saved(
            &mut preview,
            "draft · General · beta",
            SavedOutcome::Unchanged,
        );
        preview.stopped_windows.push(StoppedWindow {
            window: 3,
            reason: "A saved window (window-3.sqlite) could not be read; it may not reopen".into(),
            recovery: "Its saved file stays as it is".into(),
        });
        preview
    }

    #[test]
    fn representative_review_counts_routine_panes_and_names_recovery() {
        let review = render_review(&representative());
        println!("{review}");
        assert!(
            review.starts_with(concat!(
                "Update cagents 0.1.3+local.991daa43e -> 0.1.3+local.d2509365\n",
                "2 windows close and reopen.\n\n",
                "  Keep running (7)        5 terminals, 2 agents; nothing restarts\n",
            )),
            "{review}"
        );
        assert!(review.contains("  Unchanged (2)           ended conversations, drafts and inactive panes stay as they are\n"), "{review}");
        assert!(
            review.contains("  Working now (1)         finishing a turn; the update waits\n"),
            "{review}"
        );
        assert!(review.contains("  Come back paused (1)    nothing is started; choose Resume conversation in the new tab (+) menu\n"), "{review}");
        assert!(
            review
                .contains("    parser · General · beta: its conversation could not be confirmed\n"),
            "{review}"
        );
        assert!(review.contains("  A saved window (window-3.sqlite) could not be read; it may not reopen. Its saved file stays as it is\n"), "{review}");
        assert_eq!(
            review
                .matches("    Untitled conversation in alpha, started ")
                .count(),
            2,
            "{review}"
        );
        assert_eq!(
            review
                .matches(": keeps running in its terminal; its chat panel disconnects.")
                .count(),
            2,
            "{review}"
        );
    }

    #[test]
    fn every_inspected_pane_and_daemon_session_is_rendered_once() {
        let mut preview = representative();
        // Exercise every collision, including live work and handoff trouble.
        for disposition in [
            Disposition::Continues,
            Disposition::Resumes,
            Disposition::ResumePicker,
            Disposition::WaitsForIdle,
            Disposition::Interrupted,
            Disposition::Blocked,
        ] {
            let live = session(
                &format!("collision {disposition:?}"),
                disposition,
                "Its running work needs attention",
            );
            preview.labels.names.insert(live.id, live.title.clone());
            preview.stopped.push(StoppedPane {
                pane: live.id,
                kind: StoppedKind::Chat,
                label: "old label".into(),
                reason: "its conversation could not be confirmed".into(),
                recovery: CHAT_RECOVERY.into(),
            });
            preview.daemon.as_mut().unwrap().sessions.push(live);
        }
        let expected: BTreeSet<_> = preview
            .labels
            .details
            .panes
            .keys()
            .chain(preview.labels.names.keys())
            .chain(preview.stopped.iter().map(|pane| &pane.pane))
            .chain(
                preview
                    .daemon
                    .as_ref()
                    .unwrap()
                    .sessions
                    .iter()
                    .map(|session| &session.id),
            )
            .copied()
            .collect();
        let items = items(&preview);
        assert_eq!(
            items.iter().map(|item| item.id).collect::<BTreeSet<_>>(),
            expected
        );
        assert_eq!(items.len(), expected.len());
        let review = render_review(&preview);
        // Routine panes are collapsed, so their one appearance is in the
        // group count; actionable panes also have exactly one named line.
        let rendered_count: usize = review
            .lines()
            .filter(|line| line.starts_with("  ") && !line.starts_with("    "))
            .filter_map(|line| {
                line.split_once(" (")
                    .and_then(|(_, rest)| rest.split_once(')'))
            })
            .filter_map(|(count, _)| count.parse::<usize>().ok())
            .sum();
        assert_eq!(rendered_count, expected.len(), "{review}");
        for item in items {
            let expanded = matches!(
                item.outcome,
                Outcome::Paused | Outcome::Working | Outcome::Interrupted | Outcome::Attention
            ) || !item.detail.is_empty();
            assert_eq!(
                review.matches(&format!("    {}:", item.label)).count(),
                usize::from(expanded),
                "{review}"
            );
        }
        for disposition in [
            Disposition::WaitsForIdle,
            Disposition::Interrupted,
            Disposition::Blocked,
        ] {
            let line = review
                .lines()
                .find(|line| line.contains(&format!("collision {disposition:?}:")))
                .unwrap();
            assert!(
                line.contains("after the update: its conversation could not be confirmed"),
                "{line}"
            );
        }
    }

    #[test]
    fn a_console_carries_its_terminal_when_its_chat_only_source_says_resume() {
        for disposition in [Disposition::Resumes, Disposition::ResumePicker] {
            let mut preview = preview();
            let live = session(
                "console · alpha",
                disposition,
                "The chat cannot reconnect automatically",
            );
            preview.labels.details.panes.insert(
                live.id,
                super::super::source_inspection::InspectedPane {
                    kind: SessionKind::NativeChat,
                    outcome: SavedOutcome::KeepsRunning,
                    detail: "keeps running in its terminal; its chat panel disconnects".into(),
                },
            );
            preview.daemon.as_mut().unwrap().sessions.push(live);
            let review = render_review(&preview);
            assert!(review.contains("Keep running (1)"), "{review}");
            assert!(!review.contains("Come back paused"), "{review}");
            assert!(!review.contains("Resumes ("), "{review}");
            assert_eq!(
                review.matches("    console · alpha:").count(),
                1,
                "{review}"
            );
        }
    }

    #[test]
    fn a_known_paused_interruption_never_promises_automatic_resume() {
        for tasks in [0, 2] {
            let mut preview = preview();
            let warning = if tasks == 0 {
                "The active turn will be interrupted; the saved conversation will resume."
            } else {
                "The active turn and 2 running background subagents will stop; background work will not resume automatically."
            };
            let mut live = session("interrupted · alpha", Disposition::Interrupted, warning);
            live.interrupted_subagents = (0..tasks)
                .map(|index| format!("background-task-{index}"))
                .collect();
            preview.stopped.push(StoppedPane {
                pane: live.id,
                kind: StoppedKind::Chat,
                label: live.title.clone(),
                reason: "its conversation could not be confirmed".into(),
                recovery: CHAT_RECOVERY.into(),
            });
            preview.daemon.as_mut().unwrap().sessions.push(live);
            let review = render_review(&preview);
            assert!(review.contains("Come back paused (1)"), "{review}");
            if tasks > 0 {
                assert!(review.contains("its current turn and 2 background tasks stop; background work will not resume automatically; after the update:"), "{review}");
            } else {
                assert!(
                    review.contains("its current turn is interrupted; after the update:"),
                    "{review}"
                );
            }
            assert!(
                !review.contains("the saved conversation will resume"),
                "{review}"
            );
            assert!(!review.contains("Interrupted and resumed"), "{review}");
        }
    }

    #[test]
    fn working_panes_with_manual_recovery_do_not_repeat_the_sources_resume_promise() {
        let mut preview = preview();
        let live = session(
            "working · alpha",
            Disposition::WaitsForIdle,
            "The update will wait for this conversation to become idle, then resume it.",
        );
        preview.stopped.push(StoppedPane {
            pane: live.id,
            kind: StoppedKind::Chat,
            label: live.title.clone(),
            reason: "its conversation could not be confirmed".into(),
            recovery: CHAT_RECOVERY.into(),
        });
        preview.daemon.as_mut().unwrap().sessions.push(live);
        let review = render_review(&preview);
        assert!(review.contains("Working now (1)"), "{review}");
        assert!(review.contains("working · alpha: finishing its turn; after the update: its conversation could not be confirmed"), "{review}");
        assert!(!review.contains("then resume it"), "{review}");
        assert_eq!(
            review.matches("    working · alpha:").count(),
            1,
            "{review}"
        );
    }

    #[test]
    fn review_omits_empty_groups_and_pluralizes_one_window() {
        let mut preview = preview();
        preview.windows = 0;
        preview.daemon = None;
        assert_eq!(
            render_review(&preview),
            "Update cagents 0.1.3+local.991daa43e -> 0.1.3+local.d2509365\n"
        );
        preview.windows = 1;
        assert!(render_review(&preview).contains("1 window closes and reopens"));
    }

    #[test]
    fn pane_specific_controls_only_appear_when_different_from_group_guidance() {
        let mut preview = preview();
        let mut terminal = session("terminal · beta", Disposition::Continues, "");
        terminal.kind = SessionKind::Terminal;
        preview.stopped.push(StoppedPane {
            pane: terminal.id,
            kind: StoppedKind::Terminal,
            label: "saved label".into(),
            reason: "Its terminal could not be matched".into(),
            recovery: "Press Reconnect in the pane to check the saved terminal".into(),
        });
        preview.daemon.as_mut().unwrap().sessions.push(terminal);
        let review = render_review(&preview);
        assert!(review.contains("    terminal · beta: its terminal could not be matched. Press Reconnect in the pane to check the saved terminal"), "{review}");
        assert!(!review.contains("Keep running ("), "{review}");
    }
}
