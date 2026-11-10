//! The separate, explicitly destructive part of an update review.

use std::{fmt::Write as _, io, path::Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{UpdateChoice, impact};
use crate::model::TerminalId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopItem {
    pub id: TerminalId,
    pub kind: impact::SessionKind,
    pub label: String,
    pub identity: String,
    pub running: String,
    pub recovery: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopList {
    pub from_version: String,
    pub items: Vec<StopItem>,
}

impl StopList {
    /// The listed identities and words are the authority, never their count.
    /// Inventory ordering can change without changing that authority.
    pub fn digest(&self) -> String {
        let mut items: Vec<_> = self.items.iter().collect();
        items.sort_by_key(|item| (item.kind, item.id));
        let encoded = serde_json::to_vec(&(&self.from_version, items))
            .expect("stop lists contain only serializable values");
        format!("{:x}", Sha256::digest(encoded))
    }

    pub fn render(&self) -> String {
        let mut output = format!(
            "The running terminal service ({}) cannot hand these over.\nUpdating stops them:\n\n",
            inline(&self.from_version)
        );
        for item in &self.items {
            writeln!(output, "  {}", inline(&item.label)).unwrap();
            writeln!(output, "      {}", inline(&item.running)).unwrap();
            writeln!(output, "      after the update: {}", inline(&item.recovery)).unwrap();
        }
        output.push_str("\nType \"stop\" to stop these and update. Anything else cancels.\n");
        output
    }
}

fn inline(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

pub(super) fn chat_item_with_state(
    chat: &crate::chat::Session,
    impact: &impact::SessionImpact,
    label: &str,
    state: &str,
) -> StopItem {
    let recorded = chat
        .launch
        .session_id
        .as_ref()
        .is_some_and(|id| !id.is_empty());
    StopItem {
        id: impact.id,
        kind: impact::SessionKind::NativeChat,
        label: label.into(),
        identity: impact.identity.clone(),
        running: format!(
            "{} chat, {state}{}",
            match chat.launch.provider {
                crate::agent::Provider::Claude => "Claude",
                provider => provider.label(),
            },
            if recorded { "" } else { " (can't be resumed)" }
        ),
        recovery: if recorded {
            "press Resume in the pane"
        } else {
            "the transcript stays visible; Start fresh in the pane"
        }
        .into(),
    }
}

pub(super) fn terminal_item(
    _terminal: &crate::terminal_ipc::TerminalDescriptor,
    impact: &impact::SessionImpact,
    label: &str,
) -> StopItem {
    StopItem {
        id: impact.id,
        kind: impact::SessionKind::Terminal,
        label: label.into(),
        identity: impact.identity.clone(),
        // The frozen List descriptor has no child PID. Do not substitute a
        // launch recipe: that would name the shell rather than its live job.
        running: terminal_running(None),
        recovery: "screen and directory kept; press Restart in the pane".into(),
    }
}

/// `--yes` accepts only the ordinary review. A person at a terminal must type
/// this word, even when every other update choice has already been approved.
pub(super) fn read_stop_choice(
    input: &mut impl io::BufRead,
    is_terminal: bool,
    _yes: bool,
    list: &StopList,
) -> io::Result<UpdateChoice> {
    if !is_terminal {
        return Ok(UpdateChoice::Cancel);
    }
    let mut answer = String::new();
    if input.read_line(&mut answer)? != 0 && answer.trim() == "stop" {
        Ok(UpdateChoice::StopListed {
            items_digest: list.digest(),
        })
    } else {
        Ok(UpdateChoice::Cancel)
    }
}

pub(super) fn terminal_running(pid: Option<u32>) -> String {
    pid.and_then(|pid| foreground(Path::new("/proc"), pid))
        .unwrap_or_else(|| "terminal session".into())
}

#[derive(PartialEq, Eq)]
struct Stat {
    group: u32,
    foreground: u32,
    start: u64,
}

fn stat(bytes: &str) -> Option<Stat> {
    // comm can contain whitespace and parentheses; the closing delimiter is
    // the last one before the remaining numeric fields.
    let fields: Vec<_> = bytes.rsplit_once(") ")?.1.split_whitespace().collect();
    Some(Stat {
        group: fields.get(2)?.parse().ok()?,
        foreground: fields.get(5)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
    })
}

fn command(root: &Path, pid: u32) -> Option<Vec<String>> {
    let bytes = std::fs::read(root.join(pid.to_string()).join("cmdline")).ok()?;
    let arguments: Vec<_> = bytes
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| inline(&String::from_utf8_lossy(argument)))
        .collect();
    (!arguments.is_empty()).then_some(arguments)
}

fn foreground(root: &Path, pid: u32) -> Option<String> {
    let child_path = root.join(pid.to_string()).join("stat");
    let child = stat(&std::fs::read_to_string(&child_path).ok()?)?;
    if child.foreground == 0 {
        return None;
    }
    let leader_path = root.join(child.foreground.to_string()).join("stat");
    let leader = stat(&std::fs::read_to_string(&leader_path).ok()?)?;
    let arguments = command(root, child.foreground)?;
    let program = Path::new(&arguments[0])
        .file_name()?
        .to_str()?
        .trim_start_matches('-');
    let idle = child.foreground == child.group
        && matches!(
            program,
            "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "nu"
        )
        && arguments[1..]
            .iter()
            .all(|argument| matches!(argument.as_str(), "-l" | "--login" | "-i" | "--interactive"));
    let mut description = if idle {
        "idle shell".into()
    } else {
        format!("shell running `{}`", arguments.join(" "))
    };
    // btime is a recorded boot epoch; process starttime is in clock ticks
    // since boot. Neither a transcript mtime nor a guessed launch time is used.
    let started = std::fs::read_to_string(root.join("stat"))
        .ok()
        .and_then(|contents| {
            contents
                .lines()
                .find_map(|line| line.strip_prefix("btime ")?.trim().parse::<i64>().ok())
        })
        .and_then(|boot| {
            // SAFETY: sysconf accepts a scalar constant and owns no memory.
            let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            (ticks > 0)
                .then(|| i64::try_from(leader.start / ticks as u64).ok())
                .flatten()
                .and_then(|seconds| boot.checked_add(seconds))
        })
        .and_then(crate::chat::local_time);
    if let Some(started) = started {
        write!(description, " (since {})", started.clock()).unwrap();
    }
    if stat(&std::fs::read_to_string(child_path).ok()?)? != child
        || stat(&std::fs::read_to_string(leader_path).ok()?)? != leader
    {
        return None;
    }
    Some(description)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::Provider, chat, model::LaunchRecipe};

    fn item(kind: impact::SessionKind, label: &str, running: &str, recovery: &str) -> StopItem {
        StopItem {
            id: TerminalId::new(),
            kind,
            label: label.into(),
            identity: "reviewed owner".into(),
            running: running.into(),
            recovery: recovery.into(),
        }
    }

    fn representative() -> StopList {
        StopList {
            from_version: "0.1.3+local.991daa43e".into(),
            items: vec![
                item(
                    impact::SessionKind::Terminal,
                    "build · Release Details: rollout · alpha",
                    "shell running `cargo test --workspace` (since 13:40)",
                    "screen and directory kept; press Restart in the pane",
                ),
                item(
                    impact::SessionKind::NativeChat,
                    "Untitled conversation in beta, started 9 Nov 14:02",
                    "Claude chat, working (can't be resumed)",
                    "the transcript stays visible; Start fresh in the pane",
                ),
            ],
        }
    }

    #[test]
    fn confirmation_names_every_item_once_and_preserves_sidebar_punctuation() {
        assert_eq!(
            representative().render(),
            concat!(
                "The running terminal service (0.1.3+local.991daa43e) cannot hand these over.\n",
                "Updating stops them:\n\n",
                "  build · Release Details: rollout · alpha\n",
                "      shell running `cargo test --workspace` (since 13:40)\n",
                "      after the update: screen and directory kept; press Restart in the pane\n",
                "  Untitled conversation in beta, started 9 Nov 14:02\n",
                "      Claude chat, working (can't be resumed)\n",
                "      after the update: the transcript stays visible; Start fresh in the pane\n\n",
                "Type \"stop\" to stop these and update. Anything else cancels.\n"
            )
        );
    }

    #[test]
    fn only_a_persons_stop_authorizes_the_list_even_with_yes() {
        let list = representative();
        for yes in [false, true] {
            assert!(matches!(
                read_stop_choice(&mut b"stop\n".as_slice(), true, yes, &list).unwrap(),
                UpdateChoice::StopListed { items_digest } if items_digest == list.digest()
            ));
            for answer in ["", "\n", "yes\n", "y\n", "STOP\n", "stop these\n", "n\n"] {
                assert!(matches!(
                    read_stop_choice(&mut answer.as_bytes(), true, yes, &list).unwrap(),
                    UpdateChoice::Cancel
                ));
            }
            assert!(matches!(
                read_stop_choice(&mut b"stop\n".as_slice(), false, yes, &list).unwrap(),
                UpdateChoice::Cancel
            ));
        }
    }

    #[test]
    fn consent_digest_covers_every_displayed_and_owned_fact() {
        let original = representative();
        let mut reordered = original.clone();
        reordered.items.reverse();
        assert_eq!(original.digest(), reordered.digest());
        for change in 0..8 {
            let mut changed = original.clone();
            match change {
                0 => changed.items[0].id = TerminalId::new(),
                1 => changed.items[0].identity.push('x'),
                2 => changed.items[0].label.push('x'),
                3 => changed.items[0].running.push('x'),
                4 => changed.items[0].recovery.push('x'),
                5 => changed.items[0].kind = impact::SessionKind::NativeChat,
                6 => changed.from_version.push('x'),
                _ => changed.items.push(changed.items[0].clone()),
            }
            assert_ne!(original.digest(), changed.digest());
        }
    }

    fn chat(recorded: bool) -> (chat::Session, impact::SessionImpact) {
        let id = TerminalId::new();
        (
            chat::Session {
                id,
                launch: chat::Launch {
                    provider: Provider::Claude,
                    recipe: LaunchRecipe {
                        program: "claude".into(),
                        arguments: Vec::new(),
                        working_directory: "/work".into(),
                        environment: Default::default(),
                        unset_environment: Default::default(),
                    },
                    session_id: recorded.then(|| "recorded-conversation".into()),
                },
                proof: None,
                terminal: None,
                owned_codex_listener: None,
                native_continuity: Default::default(),
            },
            impact::SessionImpact {
                id,
                kind: impact::SessionKind::NativeChat,
                title: "Claude chat".into(),
                identity: "source identity".into(),
                disposition: impact::Disposition::WaitsForIdle,
                reason: "Finishing its turn".into(),
                interrupted_subagents: Vec::new(),
            },
        )
    }

    #[test]
    fn chat_confirmation_distinguishes_recorded_conversations() {
        let (source, impact) = chat(true);
        let recorded = chat_item_with_state(&source, &impact, "Review Details: alpha", "working");
        assert_eq!(recorded.label, "Review Details: alpha");
        assert_eq!(recorded.running, "Claude chat, working");
        assert_eq!(recorded.recovery, "press Resume in the pane");
        let (source, impact) = chat(false);
        let unrecorded =
            chat_item_with_state(&source, &impact, "Untitled conversation in beta", "working");
        assert_eq!(
            unrecorded.running,
            "Claude chat, working (can't be resumed)"
        );
        assert_eq!(
            unrecorded.recovery,
            "the transcript stays visible; Start fresh in the pane"
        );
        assert_eq!(terminal_running(None), "terminal session");
    }

    fn proc_stat(pid: u32, group: u32, foreground: u32) -> String {
        let mut fields = vec!["0".to_owned(); 50];
        fields[0] = "S".into();
        fields[2] = group.to_string();
        fields[5] = foreground.to_string();
        fields[19] = "0".into();
        format!("{pid} (shell ) with spaces) {}", fields.join(" "))
    }

    #[test]
    fn terminal_confirmation_reads_foreground_group_command_and_idle_shell() {
        let proc = tempfile::tempdir().unwrap();
        for pid in [10, 20] {
            std::fs::create_dir(proc.path().join(pid.to_string())).unwrap();
        }
        std::fs::write(proc.path().join("10/stat"), proc_stat(10, 10, 20)).unwrap();
        std::fs::write(proc.path().join("10/cmdline"), b"/bin/bash\0").unwrap();
        std::fs::write(proc.path().join("20/stat"), proc_stat(20, 20, 20)).unwrap();
        std::fs::write(
            proc.path().join("20/cmdline"),
            b"cargo\0test\0--workspace\0",
        )
        .unwrap();
        std::fs::write(proc.path().join("stat"), b"btime 1794258000\n").unwrap();
        let clock = crate::chat::local_time(1_794_258_000).unwrap().clock();
        assert_eq!(
            foreground(proc.path(), 10).unwrap(),
            format!("shell running `cargo test --workspace` (since {clock})")
        );
        std::fs::write(proc.path().join("10/stat"), proc_stat(10, 10, 10)).unwrap();
        assert_eq!(
            foreground(proc.path(), 10).unwrap(),
            format!("idle shell (since {clock})")
        );
        std::fs::write(
            proc.path().join("10/cmdline"),
            b"/bin/bash\0-c\0cargo test\0",
        )
        .unwrap();
        assert_eq!(
            foreground(proc.path(), 10).unwrap(),
            format!("shell running `/bin/bash -c cargo test` (since {clock})")
        );
        std::fs::write(proc.path().join("10/cmdline"), b"/bin/bash\0").unwrap();
        std::fs::remove_file(proc.path().join("stat")).unwrap();
        assert_eq!(foreground(proc.path(), 10).unwrap(), "idle shell");
    }
}
