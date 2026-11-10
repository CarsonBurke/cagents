//! The terminal and its tests render the same reviewed outcome record.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
};

use super::impact::{Disposition, Preview};

/// Describe what happens to the reviewed panes without consulting live state.
pub fn render_review(preview: &Preview) -> String {
    let mut output = format!(
        "Update cagents {} -> {}\n",
        preview.from_version, preview.candidate.version
    );
    if preview.windows != 0 {
        writeln!(
            output,
            "{} windows close and reopen with the same panes.",
            preview.windows
        )
        .unwrap();
    }
    for editor in &preview.editors {
        if editor.protocol != Some(preview.candidate.protocol) {
            writeln!(
                output,
                "{} ({}) keeps running; its agent features need {} to be updated as well.",
                editor.name,
                editor.version.as_deref().unwrap_or("version unknown"),
                editor.name
            )
            .unwrap();
        }
    }

    let stopped_ids: BTreeSet<_> = preview.stopped.iter().map(|pane| pane.pane).collect();
    let impact_titles: BTreeMap<_, _> = preview
        .daemon
        .iter()
        .flat_map(|daemon| &daemon.sessions)
        .map(|session| (session.id, &session.title))
        .collect();
    let mut continues = 0;
    let mut resumes = 0;
    let mut working = Vec::new();
    let mut interrupted = Vec::new();
    let mut stopped = Vec::new();
    let mut attention = Vec::new();
    for pane in &preview.stopped {
        let label = preview
            .labels
            .get(&pane.pane)
            .or_else(|| impact_titles.get(&pane.pane).copied())
            .unwrap_or(&pane.label);
        stopped.push(with_recovery(label, &pane.reason, &pane.recovery));
    }
    for window in &preview.stopped_windows {
        stopped.push(with_recovery("", &window.reason, &window.recovery));
    }
    if let Some(daemon) = &preview.daemon {
        for session in &daemon.sessions {
            let label = preview.labels.get(&session.id).unwrap_or(&session.title);
            // A stopped result supersedes routine continuity. A source that
            // still has live work or refuses handoff must also remain visible:
            // the inspector's outcome does not settle that source's work.
            if stopped_ids.contains(&session.id)
                && matches!(
                    session.disposition,
                    Disposition::Continues | Disposition::Resumes | Disposition::ResumePicker
                )
            {
                continue;
            }
            match session.disposition {
                Disposition::Continues => continues += 1,
                Disposition::Resumes => resumes += 1,
                Disposition::WaitsForIdle => working.push(label.clone()),
                Disposition::Interrupted => interrupted.push(label.clone()),
                Disposition::ResumePicker => stopped.push(with_recovery(
                    label,
                    &session.reason,
                    "Pick its conversation from Resume in the pane",
                )),
                Disposition::Blocked => attention.push(format!("{label}: {}", session.reason)),
            }
        }
        attention.extend(daemon.blockers.iter().cloned());
    }

    let mut groups = String::new();
    group(
        &mut groups,
        "Continues",
        continues,
        "keep running, nothing restarts",
        &[],
    );
    group(
        &mut groups,
        "Resumes",
        resumes,
        "restart on their recorded conversation",
        &[],
    );
    group(
        &mut groups,
        "Working now",
        working.len(),
        "finishing a turn",
        &working,
    );
    group(
        &mut groups,
        "Interrupted and resumed",
        interrupted.len(),
        "",
        &interrupted,
    );
    group(
        &mut groups,
        "Reopens stopped",
        stopped.len(),
        "nothing is started; each can be reopened from its pane",
        &stopped,
    );
    group(
        &mut groups,
        "Needs attention",
        attention.len(),
        "",
        &attention,
    );
    if !groups.is_empty() {
        output.push('\n');
        output.push_str(&groups);
    }
    output
}

fn with_recovery(label: &str, reason: &str, recovery: &str) -> String {
    let prefix = if label.is_empty() {
        String::new()
    } else {
        format!("{label}: ")
    };
    let separator = if reason.ends_with(['.', '!', '?']) {
        " "
    } else {
        ". "
    };
    format!("{prefix}{reason}{separator}{recovery}")
}

fn group(output: &mut String, name: &str, count: usize, description: &str, members: &[String]) {
    if count == 0 {
        return;
    }
    let heading = format!("{name} ({count})");
    if description.is_empty() {
        writeln!(output, "  {heading}").unwrap();
    } else {
        writeln!(output, "  {heading:<21}{description}").unwrap();
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

    #[test]
    fn review_groups_outcomes_and_preserves_the_sidebar_names() {
        let mut preview = preview();
        let titled = session("Claude", Disposition::WaitsForIdle, "Finishing its turn");
        preview.labels.insert(
            titled.id,
            "chat-alpha · Release Details: rollout · alpha".into(),
        );
        let untitled = session("Codex", Disposition::WaitsForIdle, "Finishing its turn");
        preview.labels.insert(
            untitled.id,
            "Untitled conversation in beta, started 9 Nov 14:02".into(),
        );
        preview.daemon.as_mut().unwrap().sessions = vec![
            session("counted terminal", Disposition::Continues, "Keeps running"),
            session("counted chat", Disposition::Resumes, "Restarts"),
            titled,
            untitled,
            session(
                "Review · General · beta",
                Disposition::Interrupted,
                "Restarts",
            ),
            session(
                "Notes · General · beta",
                Disposition::ResumePicker,
                "its conversation needs to be selected",
            ),
            session(
                "Build · Release · alpha",
                Disposition::Blocked,
                "its running work needs to finish",
            ),
        ];
        preview.stopped.push(StoppedPane {
            pane: TerminalId::new(),
            kind: StoppedKind::Chat,
            label: "console-alpha · General · alpha".into(),
            reason: "ended before it recorded its conversation".into(),
            recovery: "Start fresh in the pane; anything it began can be reopened from Resume"
                .into(),
        });
        preview.stopped_windows.push(StoppedWindow {
            window: 3,
            reason: "Window 3's saved panes could not be read".into(),
            recovery: "The saved file is kept unchanged".into(),
        });

        assert_eq!(
            render_review(&preview),
            concat!(
                "Update cagents 0.1.3+local.991daa43e -> 0.1.3+local.d2509365\n",
                "2 windows close and reopen with the same panes.\n",
                "\n",
                "  Continues (1)        keep running, nothing restarts\n",
                "  Resumes (1)          restart on their recorded conversation\n",
                "  Working now (2)      finishing a turn\n",
                "    chat-alpha · Release Details: rollout · alpha\n",
                "    Untitled conversation in beta, started 9 Nov 14:02\n",
                "  Interrupted and resumed (1)\n",
                "    Review · General · beta\n",
                "  Reopens stopped (3)  nothing is started; each can be reopened from its pane\n",
                "    console-alpha · General · alpha: ended before it recorded its conversation. Start fresh in the pane; anything it began can be reopened from Resume\n",
                "    Window 3's saved panes could not be read. The saved file is kept unchanged\n",
                "    Notes · General · beta: its conversation needs to be selected. Pick its conversation from Resume in the pane\n",
                "  Needs attention (1)\n",
                "    Build · Release · alpha: its running work needs to finish\n",
            )
        );
    }

    #[test]
    fn review_omits_empty_groups_and_the_absent_window_line() {
        let mut preview = preview();
        preview.windows = 0;
        assert_eq!(
            render_review(&preview),
            "Update cagents 0.1.3+local.991daa43e -> 0.1.3+local.d2509365\n"
        );
        preview.daemon = None;
        assert_eq!(
            render_review(&preview),
            "Update cagents 0.1.3+local.991daa43e -> 0.1.3+local.d2509365\n"
        );
    }

    #[test]
    fn stopped_panes_are_not_also_counted_as_continuing_or_resuming() {
        for disposition in [
            Disposition::Continues,
            Disposition::Resumes,
            Disposition::ResumePicker,
        ] {
            let mut preview = preview();
            let source = session("source title", disposition, "source reason");
            preview.stopped.push(StoppedPane {
                pane: source.id,
                kind: StoppedKind::Chat,
                label: "saved label".into(),
                reason: "its conversation could not be confirmed".into(),
                recovery: "Press Resume in the pane to reopen it".into(),
            });
            preview.daemon.as_mut().unwrap().sessions.push(source);
            let review = render_review(&preview);
            assert!(review.contains("Reopens stopped (1)"), "{review}");
            assert!(!review.contains("Continues ("), "{review}");
            assert!(!review.contains("Resumes ("), "{review}");
            assert_eq!(review.matches("source title").count(), 1, "{review}");
        }
    }

    #[test]
    fn stopped_daemon_sessions_use_sidebar_labels_then_impact_titles() {
        let mut preview = preview();
        let source = session(
            "Live conversation Details: rollout",
            Disposition::Continues,
            "",
        );
        let id = source.id;
        preview.daemon.as_mut().unwrap().sessions.push(source);
        preview.stopped.push(StoppedPane {
            pane: id,
            kind: StoppedKind::Chat,
            label: "Conversation".into(),
            reason: "its conversation could not be confirmed".into(),
            recovery: "Press Resume in the pane to reopen it".into(),
        });
        let review = render_review(&preview);
        assert!(
            review.contains(
                "    Live conversation Details: rollout: its conversation could not be confirmed."
            ),
            "{review}"
        );

        preview
            .labels
            .insert(id, "Saved title · General · alpha".into());
        let review = render_review(&preview);
        assert!(
            review.contains(
                "    Saved title · General · alpha: its conversation could not be confirmed."
            ),
            "{review}"
        );
        assert!(!review.contains("Live conversation"), "{review}");

        preview.labels.clear();
        preview.daemon = None;
        preview.stopped[0].label = "Original pane label".into();
        let review = render_review(&preview);
        assert!(
            review.contains("    Original pane label: its conversation could not be confirmed."),
            "{review}"
        );
    }

    #[test]
    fn stopped_outcomes_do_not_hide_live_work_or_handoff_blockers() {
        let mut preview = preview();
        for disposition in [Disposition::WaitsForIdle, Disposition::Blocked] {
            let source = session("live source", disposition, "needs attention");
            preview.stopped.push(StoppedPane {
                pane: source.id,
                kind: StoppedKind::Chat,
                label: "saved label".into(),
                reason: "its conversation could not be confirmed".into(),
                recovery: "Press Resume in the pane to reopen it".into(),
            });
            preview.daemon.as_mut().unwrap().sessions.push(source);
        }
        preview
            .daemon
            .as_mut()
            .unwrap()
            .blockers
            .push("The terminal service needs attention".into());
        let review = render_review(&preview);
        assert!(review.contains("Working now (1)"), "{review}");
        assert!(review.contains("Reopens stopped (2)"), "{review}");
        assert!(review.contains("Needs attention (2)"), "{review}");
        assert!(
            review.contains("The terminal service needs attention"),
            "{review}"
        );
    }
}
