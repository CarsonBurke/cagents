//! Fixed metadata projection of one existing terminal inventory read.
use super::*;
use crate::model::TerminalExit;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Running,
    Exited,
    RelaunchAvailable,
    Failed,
    Unavailable,
}

#[derive(Serialize)]
struct Entry<'a> {
    id: TerminalId,
    state: State,
    title: Option<&'a str>,
    attached: bool,
    program: &'a str,
    working_directory: &'a Path,
    exit: Option<&'a TerminalExit>,
}

fn entry<'a>(terminal: &'a TerminalDescriptor, proof: &HostSessionProof) -> Entry<'a> {
    let (state, exit) = if terminal.session.continuity.can_resume_live(proof) {
        (State::Running, None)
    } else {
        match &terminal.session.continuity {
            TerminalContinuity::Exited { last_exit }
            | TerminalContinuity::RelaunchAvailable {
                last_exit: Some(last_exit),
                ..
            } => (State::Exited, Some(last_exit)),
            TerminalContinuity::RelaunchAvailable {
                last_exit: None, ..
            } => (State::RelaunchAvailable, None),
            TerminalContinuity::Failed { .. } => (State::Failed, None),
            TerminalContinuity::Live { .. } | TerminalContinuity::DetachedSameBoot { .. } => {
                (State::Unavailable, None)
            }
        }
    };
    Entry {
        id: terminal.session.id,
        state,
        title: terminal.title.as_deref(),
        attached: terminal.attached,
        program: &terminal.session.launch.program,
        working_directory: &terminal.session.launch.working_directory,
        exit,
    }
}

fn state_label(row: &Entry<'_>) -> String {
    match row.state {
        State::Running => "running".into(),
        State::Exited => match row.exit {
            Some(TerminalExit {
                signal: Some(signal),
                ..
            }) => format!("exited (signal {signal})"),
            Some(TerminalExit {
                code: Some(code), ..
            }) => format!("exited ({code})"),
            _ => "exited (unknown status)".into(),
        },
        State::RelaunchAvailable => "relaunch available".into(),
        State::Failed => "failed".into(),
        State::Unavailable => "unavailable".into(),
    }
}

fn human(rows: &[Entry<'_>], empty: &str) -> String {
    super::format::table(
        ["TERMINAL", "STATE", "TITLE", "DIRECTORY"],
        rows.iter()
            .map(|row| {
                [
                    row.id.to_string(),
                    state_label(row),
                    row.title.unwrap_or("unknown").into(),
                    row.working_directory.display().to_string(),
                ]
            })
            .collect(),
        empty,
    )
}

pub(super) fn list(
    terminals: &[TerminalDescriptor],
    proof: &HostSessionProof,
    all: bool,
    exited: bool,
) -> CommandOutput {
    let rows = terminals
        .iter()
        .map(|terminal| entry(terminal, proof))
        .filter(|row| {
            all || if exited {
                row.state == State::Exited
            } else {
                row.state == State::Running
            }
        })
        .collect::<Vec<_>>();
    let empty = if all {
        "No terminals"
    } else if exited {
        "No terminals with a recorded exit"
    } else {
        "No running terminals"
    };
    CommandOutput {
        command: "terminal.list",
        human: human(&rows, empty),
        data: json!({"terminals": rows, "proof": proof}),
    }
}

pub(super) fn show(terminal: &TerminalDescriptor, proof: &HostSessionProof) -> CommandOutput {
    CommandOutput {
        command: "terminal.show",
        human: human(&[entry(terminal, proof)], "No terminals"),
        data: json!({"terminal": terminal, "proof": proof}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("terminal-list"),
            boot_id: crate::model::BootId::new("private-boot"),
            host_incarnation: crate::model::HostIncarnation::new("private-incarnation"),
        }
    }

    fn terminal(continuity: TerminalContinuity) -> TerminalDescriptor {
        let mut row = TerminalDescriptor::relaunchable(
            TerminalId::new(),
            LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec!["-l".into()],
                working_directory: "/fixture".into(),
                environment: BTreeMap::from([("FIXTURE_VALUE".into(), "show only".into())]),
                unset_environment: Default::default(),
            },
            None,
        );
        row.title = Some("Native title".into());
        row.session.resume_hint = Some("fixture recovery metadata".into());
        row.session.continuity = continuity;
        row
    }

    #[test]
    fn terminal_list_filters_use_current_live_proof_and_recorded_exit_only() {
        let current = proof();
        let running = terminal(TerminalContinuity::DetachedSameBoot {
            host_session_id: current.host_session_id.clone(),
            boot_id: current.boot_id.clone(),
            host_incarnation: current.host_incarnation.clone(),
        });
        let mut stale = running.clone();
        stale.session.id = TerminalId::new();
        stale.session.continuity = TerminalContinuity::Live {
            host_session_id: current.host_session_id.clone(),
            boot_id: current.boot_id.clone(),
            host_incarnation: crate::model::HostIncarnation::new("different"),
        };
        let mut exited = running.clone();
        exited.session.id = TerminalId::new();
        exited.session.continuity = TerminalContinuity::Exited {
            last_exit: TerminalExit {
                code: Some(3),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
        let unknown = TerminalDescriptor::relaunchable(
            TerminalId::new(),
            running.session.launch.clone(),
            None,
        );
        let rows = [running, stale, exited, unknown];
        let live = list(&rows, &current, false, false);
        assert_eq!(live.data["terminals"].as_array().unwrap().len(), 1);
        assert_eq!(live.data["terminals"][0]["state"], "running");
        assert!(live.human.starts_with("TERMINAL"));
        let ended = list(&rows, &current, false, true);
        assert_eq!(ended.data["terminals"].as_array().unwrap().len(), 1);
        assert!(ended.human.contains("exited (3)"));
        let all = list(&rows, &current, true, false);
        assert_eq!(all.data["terminals"].as_array().unwrap().len(), 4);
        assert_eq!(all.data["terminals"][1]["state"], "unavailable");
        assert_eq!(all.data["terminals"][3]["state"], "relaunch_available");
        assert!(
            !all.human.contains("hibernated"),
            "absence of an exit does not establish hibernation"
        );
    }

    #[test]
    fn terminal_list_json_is_lean_while_show_keeps_the_full_descriptor() {
        let current = proof();
        let row = terminal(TerminalContinuity::Exited {
            last_exit: TerminalExit {
                code: None,
                signal: Some("Killed".into()),
                occurred_at_unix_ms: 1,
            },
        });
        let shown = show(&row, &current);
        assert_eq!(
            shown.data["terminal"]["session"]["launch"]["environment"]["FIXTURE_VALUE"],
            "show only"
        );
        assert_eq!(
            shown.data["terminal"]["session"]["resume_hint"],
            "fixture recovery metadata"
        );
        let output = list(&[row], &current, true, false);
        let entry = &output.data["terminals"][0];
        assert_eq!(entry["state"], "exited");
        assert_eq!(entry["title"], "Native title");
        assert_eq!(entry["exit"]["signal"], "Killed");
        for field in ["session", "launch", "environment", "resume_hint", "recipe"] {
            assert!(entry.get(field).is_none());
        }
        assert!(output.human.contains("exited (signal Killed)"));
    }

    #[test]
    fn terminal_list_has_explicit_empty_states_and_grammar() {
        let current = proof();
        assert_eq!(
            list(&[], &current, false, false).human,
            "No running terminals"
        );
        assert_eq!(list(&[], &current, true, false).human, "No terminals");
        assert_eq!(
            list(&[], &current, false, true).human,
            "No terminals with a recorded exit"
        );
        for flag in ["--all", "--exited"] {
            assert!(Cli::try_parse_from(["cagents", "terminal", "list", flag]).is_ok());
        }
        assert!(Cli::try_parse_from(["cagents", "terminal", "list", "--all", "--exited"]).is_err());
        assert!(
            Cli::try_parse_from([
                "cagents",
                "terminal",
                "show",
                "00000000-0000-4000-8000-000000000001"
            ])
            .is_ok()
        );
    }
}
