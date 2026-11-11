//! The separate, explicitly destructive part of an update review.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::{self, Read as _},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
};

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
    #[serde(default)]
    pub background_tasks: u64,
    #[serde(default)]
    pub background_identity: String,
    pub items: Vec<StopItem>,
}

impl StopList {
    /// Consent binds owners, source descriptors and recovery, never a count.
    /// Working/idle text is display-only; foreground jobs are in the identity.
    pub fn digest(&self) -> String {
        let mut items: Vec<_> = self.items.iter().collect();
        items.sort_by_key(|item| (item.kind, item.id));
        let identities: Vec<_> = items
            .into_iter()
            .map(|item| {
                (
                    item.id,
                    item.kind,
                    &item.label,
                    &item.identity,
                    &item.recovery,
                )
            })
            .collect();
        let encoded = serde_json::to_vec(&(
            &self.from_version,
            self.background_tasks,
            &self.background_identity,
            identities,
        ))
        .expect("stop lists contain only serializable values");
        format!("{:x}", Sha256::digest(encoded))
    }

    pub fn render(&self) -> String {
        let mut output = self.render_preview();
        output.push_str("\nType \"stop\" to stop these and update. Anything else cancels.\n");
        output
    }

    /// A preview is also shown where typing cannot authorize destruction.
    pub fn render_preview(&self) -> String {
        let mut output = format!(
            "The running terminal service ({}) cannot hand these over.\nUpdating stops them:\n\n",
            inline(&self.from_version)
        );
        if self.background_tasks > 0 {
            let noun = if self.background_tasks == 1 {
                "task"
            } else {
                "tasks"
            };
            writeln!(
                output,
                "  {} running background {noun} also stop; they do not resume automatically.\n",
                self.background_tasks
            )
            .unwrap();
        }
        for item in &self.items {
            writeln!(output, "  {}", inline(&item.label)).unwrap();
            writeln!(output, "      {}", inline(&item.running)).unwrap();
            writeln!(output, "      after the update: {}", inline(&item.recovery)).unwrap();
        }
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
        identity: identity(&(&impact.identity, &impact.interrupted_subagents, chat)),
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
            "the transcript stays visible; start a new conversation"
        }
        .into(),
    }
}

pub(super) fn terminal_item(
    terminal: &crate::terminal_ipc::TerminalDescriptor,
    impact: &impact::SessionImpact,
    label: &str,
    processes: &TerminalProcesses,
) -> StopItem {
    let foreground = processes.foreground(terminal);
    StopItem {
        id: impact.id,
        kind: impact::SessionKind::Terminal,
        label: label.into(),
        identity: identity(&(&impact.identity, &terminal.session, &foreground)),
        running: foreground
            .map(|job| job.description)
            .unwrap_or_else(|| "terminal process unavailable".into()),
        recovery: "screen and directory kept; press Restart in the pane".into(),
    }
}

fn identity(value: &impl Serialize) -> String {
    let encoded = serde_json::to_vec(value).expect("source descriptors are serializable");
    format!("{:x}", Sha256::digest(encoded))
}

/// `--yes` accepts only the ordinary review. A person at a terminal must type
/// this word, even when every other update choice has already been approved.
#[cfg(test)]
pub(super) fn read_stop_choice(
    input: &mut impl io::BufRead,
    is_terminal: bool,
    list: &StopList,
) -> io::Result<UpdateChoice> {
    read_stop_choice_from(|answer| input.read_line(answer), is_terminal, list)
}

pub(super) fn read_stop_choice_from(
    mut read_line: impl FnMut(&mut String) -> io::Result<usize>,
    is_terminal: bool,
    list: &StopList,
) -> io::Result<UpdateChoice> {
    if !is_terminal {
        return Ok(UpdateChoice::Cancel);
    }
    let mut answer = String::new();
    if read_line(&mut answer)? != 0 && answer.trim() == "stop" {
        Ok(UpdateChoice::StopListed {
            items_digest: list.digest(),
        })
    } else {
        Ok(UpdateChoice::Cancel)
    }
}

/// A single same-user census serves the entire stop list. A terminal's marker
/// is inherited by its shell and jobs; never match another user's /proc data.
pub(super) struct TerminalProcesses {
    root: PathBuf,
    rows: BTreeMap<u32, ProcessRow>,
}

struct ProcessRow {
    stat: Stat,
    terminal: Option<TerminalId>,
}

impl TerminalProcesses {
    pub(super) fn observe() -> Self {
        Self::at(Path::new("/proc"), crate::daemon::current_uid())
    }

    fn at(root: &Path, uid: u32) -> Self {
        let mut rows = BTreeMap::new();
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse().ok())
                else {
                    continue;
                };
                if !entry.metadata().is_ok_and(|metadata| metadata.uid() == uid) {
                    continue;
                }
                let Some(stat) = read_stat(root, pid) else {
                    continue;
                };
                let terminal = bounded_file(&entry.path().join("environ"))
                    .split(|byte| *byte == 0)
                    .find_map(|entry| {
                        let value = entry.strip_prefix(b"CAGENTS_TERMINAL_ID=")?;
                        std::str::from_utf8(value).ok()?.parse().ok()
                    });
                rows.insert(pid, ProcessRow { stat, terminal });
            }
        }
        Self {
            root: root.to_owned(),
            rows,
        }
    }

    fn foreground(&self, terminal: &crate::terminal_ipc::TerminalDescriptor) -> Option<Foreground> {
        let id = terminal.session.id;
        let origin = terminal.session.recovery_origin.unwrap_or(id);
        let matches = |row: &ProcessRow| {
            row.terminal
                .is_some_and(|marker| marker == id || marker == origin)
        };
        // The session leader is the shell even when it has exec'd a command.
        // Older holders may use an existing session. The shell then marks a
        // boundary from its daemon/holder parent's terminal marker.
        let (&pid, row) = self
            .rows
            .iter()
            .filter(|(_, row)| matches(row))
            .filter(|(pid, row)| {
                **pid == row.stat.session
                    || self
                        .rows
                        .get(&row.stat.parent)
                        .is_none_or(|parent| parent.terminal != row.terminal)
            })
            // A restarted pane can coexist with an old unattached process that
            // has its recovery origin. Its exact current marker wins; origin
            // matching is only a fallback for holders carrying the old marker.
            .min_by_key(|(pid, row)| {
                (
                    row.terminal != Some(id),
                    **pid != row.stat.session,
                    row.stat.start,
                    **pid,
                )
            })?;
        if read_stat(&self.root, pid)? != row.stat {
            return None;
        }
        foreground(&self.root, pid)
    }
}

fn bounded_file(path: &Path) -> Vec<u8> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    const LIMIT: u64 = 1024 * 1024;
    if file.take(LIMIT + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > LIMIT {
        return Vec::new();
    }
    bytes
}

#[derive(Clone, PartialEq, Eq)]
struct Stat {
    parent: u32,
    session: u32,
    group: u32,
    foreground: u32,
    start: u64,
}

fn stat(bytes: &str) -> Option<Stat> {
    // comm can contain whitespace and parentheses; the closing delimiter is
    // the last one before the remaining numeric fields.
    let fields: Vec<_> = bytes.rsplit_once(") ")?.1.split_whitespace().collect();
    if matches!(fields.first(), Some(&"Z" | &"X")) {
        return None;
    }
    Some(Stat {
        parent: fields.get(1)?.parse().ok()?,
        session: fields.get(3)?.parse().ok()?,
        group: fields.get(2)?.parse().ok()?,
        foreground: fields.get(5)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
    })
}

fn read_stat(root: &Path, pid: u32) -> Option<Stat> {
    stat(std::str::from_utf8(&bounded_file(&root.join(pid.to_string()).join("stat"))).ok()?)
}

fn command(root: &Path, pid: u32) -> Option<Vec<Vec<u8>>> {
    let bytes = bounded_file(&root.join(pid.to_string()).join("cmdline"));
    let arguments: Vec<_> = bytes
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    (!arguments.is_empty()).then_some(arguments)
}

#[derive(Serialize)]
struct Foreground {
    shell_pid: u32,
    shell_start: u64,
    pid: u32,
    start: u64,
    argv: Vec<Vec<u8>>,
    #[serde(skip)]
    description: String,
}

fn foreground(root: &Path, pid: u32) -> Option<Foreground> {
    let child = read_stat(root, pid)?;
    if child.foreground == 0 {
        return None;
    }
    let leader = read_stat(root, child.foreground)?;
    let arguments = command(root, child.foreground)?;
    let executable = String::from_utf8_lossy(&arguments[0]);
    let program = Path::new(executable.as_ref())
        .file_name()?
        .to_str()?
        .trim_start_matches('-');
    let idle = child.foreground == child.group
        && matches!(
            program,
            "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "nu"
        )
        && arguments[1..].iter().all(|argument| {
            matches!(
                argument.as_slice(),
                b"-l" | b"--login" | b"-i" | b"--interactive"
            )
        });
    let mut description = if idle {
        "idle shell".into()
    } else {
        let command = arguments
            .iter()
            .map(|argument| String::from_utf8_lossy(argument))
            .collect::<Vec<_>>()
            .join(" ");
        format!("shell running `{}`", inline(&command))
    };
    // btime is the boot epoch; starttime is in clock ticks since boot.
    let started = fs::read_to_string(root.join("stat"))
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
    if read_stat(root, pid)? != child
        || read_stat(root, child.foreground)? != leader
        || command(root, child.foreground)? != arguments
    {
        return None;
    }
    Some(Foreground {
        shell_pid: pid,
        shell_start: child.start,
        pid: child.foreground,
        start: leader.start,
        argv: arguments,
        description,
    })
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
            background_tasks: 0,
            background_identity: String::new(),
            from_version: "0.1.3+local.991daa43e".into(),
            items: vec![
                item(
                    impact::SessionKind::Terminal,
                    "build · Release Details: rollout · alpha",
                    "shell running `cargo test` (since 13:40)",
                    "screen and directory kept; press Restart in the pane",
                ),
                item(
                    impact::SessionKind::Terminal,
                    "console · General · alpha",
                    "idle shell (since 13:35)",
                    "screen and directory kept; press Restart in the pane",
                ),
                item(
                    impact::SessionKind::NativeChat,
                    "Untitled conversation in beta, started 9 Nov 14:02",
                    "Claude chat, working (can't be resumed)",
                    "the transcript stays visible; start a new conversation",
                ),
                item(
                    impact::SessionKind::NativeChat,
                    "review · General · alpha",
                    "Claude chat, idle",
                    "press Resume in the pane",
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
                "      shell running `cargo test` (since 13:40)\n",
                "      after the update: screen and directory kept; press Restart in the pane\n",
                "  console · General · alpha\n",
                "      idle shell (since 13:35)\n",
                "      after the update: screen and directory kept; press Restart in the pane\n",
                "  Untitled conversation in beta, started 9 Nov 14:02\n",
                "      Claude chat, working (can't be resumed)\n",
                "      after the update: the transcript stays visible; start a new conversation\n",
                "  review · General · alpha\n",
                "      Claude chat, idle\n",
                "      after the update: press Resume in the pane\n\n",
                "Type \"stop\" to stop these and update. Anything else cancels.\n"
            )
        );
    }

    #[test]
    fn only_a_persons_exact_stop_authorizes_the_list() {
        let list = representative();
        assert!(matches!(
            read_stop_choice(&mut b"stop\n".as_slice(), true, &list).unwrap(),
            UpdateChoice::StopListed { items_digest } if items_digest == list.digest()
        ));
        for answer in ["", "\n", "yes\n", "y\n", "STOP\n", "stop these\n", "n\n"] {
            assert!(matches!(
                read_stop_choice(&mut answer.as_bytes(), true, &list).unwrap(),
                UpdateChoice::Cancel
            ));
        }
        assert!(matches!(
            read_stop_choice(&mut b"stop\n".as_slice(), false, &list).unwrap(),
            UpdateChoice::Cancel
        ));
    }

    #[test]
    fn consent_digest_binds_descriptors_but_not_display_only_activity() {
        let original = representative();
        let mut reordered = original.clone();
        reordered.items.reverse();
        assert_eq!(original.digest(), reordered.digest());
        for change in 0..7 {
            let mut changed = original.clone();
            match change {
                0 => changed.items[0].id = TerminalId::new(),
                1 => changed.items[0].identity.push('x'),
                2 => changed.items[0].label.push('x'),
                3 => changed.items[0].recovery.push('x'),
                4 => changed.items[0].kind = impact::SessionKind::NativeChat,
                5 => changed.from_version.push('x'),
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
            "the transcript stays visible; start a new conversation"
        );
        let mut idle = recorded.clone();
        idle.running = "Claude chat, idle".into();
        let list = |item| StopList {
            background_tasks: 0,
            background_identity: String::new(),
            from_version: "old".into(),
            items: vec![item],
        };
        assert_eq!(list(recorded).digest(), list(idle).digest());
    }

    #[test]
    fn chat_digest_binds_source_descriptor_and_ignores_work_state() {
        let (mut source, impact) = chat(false);
        let list = |item| StopList {
            background_tasks: 0,
            background_identity: String::new(),
            from_version: "old".into(),
            items: vec![item],
        };
        let working = list(chat_item_with_state(&source, &impact, "chat", "working")).digest();
        assert_eq!(
            working,
            list(chat_item_with_state(&source, &impact, "chat", "idle")).digest()
        );
        source.launch.recipe.arguments.push("--resume".into());
        assert_ne!(
            working,
            list(chat_item_with_state(&source, &impact, "chat", "working")).digest()
        );
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
            foreground(proc.path(), 10).unwrap().description,
            format!("shell running `cargo test --workspace` (since {clock})")
        );
        std::fs::write(proc.path().join("10/stat"), proc_stat(10, 10, 10)).unwrap();
        assert_eq!(
            foreground(proc.path(), 10).unwrap().description,
            format!("idle shell (since {clock})")
        );
        std::fs::write(
            proc.path().join("10/cmdline"),
            b"/bin/bash\0-c\0cargo test\0",
        )
        .unwrap();
        assert_eq!(
            foreground(proc.path(), 10).unwrap().description,
            format!("shell running `/bin/bash -c cargo test` (since {clock})")
        );
        std::fs::write(proc.path().join("10/cmdline"), b"/bin/bash\0").unwrap();
        std::fs::remove_file(proc.path().join("stat")).unwrap();
        assert_eq!(
            foreground(proc.path(), 10).unwrap().description,
            "idle shell"
        );
    }
    #[test]
    fn preview_has_no_unusable_typed_prompt() {
        let preview = representative().render_preview();
        assert!(preview.contains("shell running `cargo test`"));
        assert!(!preview.contains("Type"));
        assert!(!preview.contains("Anything else cancels"));
    }

    fn terminal() -> (
        crate::terminal_ipc::TerminalDescriptor,
        impact::SessionImpact,
    ) {
        let (chat, mut impact) = chat(true);
        impact.kind = impact::SessionKind::Terminal;
        (
            crate::terminal_ipc::TerminalDescriptor::relaunchable(
                chat.id,
                chat.launch.recipe,
                None,
            ),
            impact,
        )
    }

    fn process(
        root: &Path,
        pid: u32,
        parent: u32,
        session: u32,
        group: u32,
        foreground: u32,
        start: u64,
        marker: Option<TerminalId>,
        argv: &[u8],
    ) {
        let directory = root.join(pid.to_string());
        fs::create_dir_all(&directory).unwrap();
        let mut fields = vec!["0".to_owned(); 50];
        fields[0] = "S".into();
        fields[1] = parent.to_string();
        fields[2] = group.to_string();
        fields[3] = session.to_string();
        fields[5] = foreground.to_string();
        fields[19] = start.to_string();
        fs::write(
            directory.join("stat"),
            format!("{pid} (shell ) with spaces) {}", fields.join(" ")),
        )
        .unwrap();
        fs::write(
            directory.join("environ"),
            marker
                .map(|id| format!("CAGENTS_TERMINAL_ID={id}\0"))
                .unwrap_or_default(),
        )
        .unwrap();
        fs::write(directory.join("cmdline"), argv).unwrap();
    }

    #[test]
    fn terminal_marker_finds_shell_and_binds_foreground_epoch_and_arguments() {
        let root = tempfile::tempdir().unwrap();
        let (terminal, impact) = terminal();
        let id = terminal.session.id;
        let write_job = |pid, start, argv: &[u8]| {
            process(
                root.path(),
                10,
                1,
                10,
                10,
                pid,
                50,
                Some(id),
                b"/bin/bash\0",
            );
            process(root.path(), pid, 10, 10, pid, pid, start, Some(id), argv);
        };
        write_job(20, 100, b"cargo\0test\0");
        // This unrelated session would win a naive oldest-shell search.
        process(
            root.path(),
            2,
            1,
            2,
            2,
            2,
            1,
            Some(TerminalId::new()),
            b"/bin/bash\0",
        );
        let observed = || TerminalProcesses::at(root.path(), crate::daemon::current_uid());
        let before = terminal_item(&terminal, &impact, "build", &observed());
        assert_eq!(before.running, "shell running `cargo test`");
        let list = |item| StopList {
            background_tasks: 0,
            background_identity: String::new(),
            from_version: "old".into(),
            items: vec![item],
        };
        let digest = list(before).digest();
        for (pid, start, argv) in [
            (21, 100, b"cargo\0test\0".as_slice()),
            (20, 101, b"cargo\0test\0".as_slice()),
            (20, 100, b"cargo\0build\0".as_slice()),
        ] {
            write_job(pid, start, argv);
            assert_ne!(
                digest,
                list(terminal_item(&terminal, &impact, "build", &observed())).digest()
            );
        }
        write_job(20, 100, b"cargo\0test\0");
        let mut changed = terminal.clone();
        changed.session.launch.arguments.push("-l".into());
        assert_ne!(
            digest,
            list(terminal_item(&changed, &impact, "build", &observed())).digest()
        );
        assert!(
            TerminalProcesses::at(root.path(), crate::daemon::current_uid().wrapping_add(1))
                .rows
                .is_empty()
        );
    }

    #[test]
    fn current_terminal_marker_wins_over_an_older_surviving_recovery_origin() {
        let root = tempfile::tempdir().unwrap();
        let (mut terminal, impact) = terminal();
        let origin = TerminalId::new();
        terminal.session.recovery_origin = Some(origin);
        process(
            root.path(),
            10,
            1,
            10,
            10,
            10,
            1,
            Some(origin),
            b"/bin/bash\0",
        );
        process(
            root.path(),
            20,
            1,
            20,
            20,
            30,
            100,
            Some(terminal.session.id),
            b"/bin/bash\0",
        );
        process(
            root.path(),
            30,
            20,
            20,
            30,
            30,
            200,
            Some(terminal.session.id),
            b"cargo\0test\0",
        );
        let observed = TerminalProcesses::at(root.path(), crate::daemon::current_uid());
        assert_eq!(
            terminal_item(&terminal, &impact, "build", &observed).running,
            "shell running `cargo test`"
        );
        // A holder can keep its origin marker while issuing a fresh child.
        process(
            root.path(),
            20,
            10,
            2,
            20,
            30,
            100,
            Some(terminal.session.id),
            b"/bin/bash\0",
        );
        let observed = TerminalProcesses::at(root.path(), crate::daemon::current_uid());
        assert_eq!(
            terminal_item(&terminal, &impact, "build", &observed).running,
            "shell running `cargo test`"
        );
    }

    #[test]
    fn terminal_marker_finds_shell_beneath_holder_and_recovery_origin() {
        let root = tempfile::tempdir().unwrap();
        let (mut terminal, impact) = terminal();
        let origin = TerminalId::new();
        terminal.session.recovery_origin = Some(origin);
        process(
            root.path(),
            10,
            3,
            2,
            10,
            10,
            100,
            Some(origin),
            b"/bin/bash\0",
        );
        process(root.path(), 3, 1, 2, 3, 0, 50, None, b"holder\0");
        process(
            root.path(),
            20,
            10,
            2,
            20,
            20,
            200,
            Some(origin),
            b"sleep\0100\0",
        );
        let processes = TerminalProcesses::at(root.path(), crate::daemon::current_uid());
        assert_eq!(
            terminal_item(&terminal, &impact, "console", &processes).running,
            "idle shell"
        );
        fs::write(root.path().join("10/stat"), proc_stat(10, 10, 20)).unwrap();
        assert!(
            processes.foreground(&terminal).is_none(),
            "a shell that changed after the census is not identified by stale evidence"
        );
    }
    #[test]
    fn background_scope_is_disclosed_and_bound_even_when_the_count_is_unchanged() {
        let mut list = representative();
        let original = list.digest();
        list.background_tasks = 2;
        list.background_identity = "first task;second task".into();
        assert_ne!(list.digest(), original);
        let shown = list.digest();
        assert!(
            list.render().contains(
                "2 running background tasks also stop; they do not resume automatically."
            )
        );
        list.background_identity = "replacement task;second task".into();
        assert_ne!(list.digest(), shown);
    }
}
