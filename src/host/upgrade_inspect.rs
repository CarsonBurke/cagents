//! Bounded source qualification after source windows flush, before close.
//! Stores remain read-only. Exact retained providers missing from startup
//! discovery may be authenticated through Resolve; no launch recipe is run.
mod current;
mod v68;
mod v77;
#[cfg(test)]
pub(super) use current::Client as InspectionClient;

use crate::install_guard::SourceInspectionRequest as Request;
use crate::update::source_inspection::{
    InspectedPane, SavedOutcome, SourceInspection, StoppedKind, StoppedWindow,
};
use crate::{
    chat::Session,
    model::{CardKind, HostSessionProof},
    update::source::{self, ClientFamily},
    upgrade::handoff,
};
use std::{
    collections::HashSet,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(super) const ARGUMENT: &str = "--inspect-upgrade-source";

pub(super) fn entry() -> Option<Result<(), String>> {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if arguments
        .first()
        .is_none_or(|argument| argument != ARGUMENT)
    {
        return None;
    }
    Some((|| -> Result<(), String> {
        crate::instance::validate_stage_guard()
            .map_err(|_| "Source inspection requires the active authenticated update fence")?;
        if arguments.len() != 2 {
            return Err("Invalid source inspection arguments".into());
        }
        let argument = arguments[1]
            .to_str()
            .filter(|argument| argument.len() <= 4096)
            .ok_or("Source inspection request exceeds its limit")?;
        let request =
            serde_json::from_str(argument).map_err(|_| "Invalid source inspection request")?;
        let result = inspect(request);
        // Outcomes include only saved display labels and fixed recovery text,
        // never argv, account environment, listener credentials or transcript text.
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, &result)
            .map_err(|_| "Cannot write source inspection receipt")?;
        stdout
            .write_all(b"\n")
            .map_err(|_| "Cannot flush source inspection receipt".to_string())?;
        stdout
            .flush()
            .map_err(|_| "Cannot flush source inspection receipt".to_string())
    })())
}

enum Reader {
    Current(current::Client),
    Legacy(v77::Client),
    Legacy68(v68::Client),
}

impl Reader {
    fn list(&mut self) -> Result<Vec<Session>, String> {
        match self {
            Self::Current(client) => client.list(),
            Self::Legacy(client) => client.list(),
            Self::Legacy68(client) => client.list(),
        }
    }
    fn terminals(&mut self) -> Result<Vec<v77::Terminal>, String> {
        match self {
            Self::Current(client) => client.terminals(),
            Self::Legacy(client) => client.terminals(),
            Self::Legacy68(client) => client.terminals(),
        }
    }
    fn resolve(&mut self, source: &Session) -> Result<Session, String> {
        match self {
            Self::Current(client) => client.resolve(source),
            Self::Legacy(client) => client.resolve(source),
            Self::Legacy68(client) => client.resolve(source),
        }
    }
    fn inspect_saved_history(&mut self, source: &Session) -> Result<(), String> {
        match self {
            Self::Current(client) => client.inspect_saved_history(source),
            _ => Err("Source has no qualified read-only history API".into()),
        }
    }
}

fn connect(request: &Request, deadline: Instant) -> Result<Reader, String> {
    let family = source::client_family(request.protocol).ok_or_else(|| {
        "Source protocol has no qualified migration inspector; source windows remain usable"
            .to_owned()
    })?;
    let proof = request
        .proof
        .as_ref()
        .filter(|proof| proof.is_complete())
        .ok_or("Saved conversations have no authenticated source service")?;
    let socket = crate::terminal::ipc::daemon_socket_path()
        .map_err(|_| "Source service namespace is unavailable")?;
    match family {
        ClientFamily::Current => {
            current::Client::connect(&socket, proof, deadline, request.protocol)
                .map(Reader::Current)
        }
        ClientFamily::V68 => v68::Client::connect(&socket, proof, deadline).map(Reader::Legacy68),
        ClientFamily::V77 => v77::Client::connect(&socket, proof, deadline).map(Reader::Legacy),
    }
}

const UNCHECKED: &str = "could not be checked before the update";
const CHAT_RECOVERY: &str = "Choose Resume conversation in the new tab (+) menu";
const TERMINAL_RECOVERY: &str = "Press Reconnect in the pane to check the saved terminal";
const CONSOLE_REASON: &str = "keeps running in its terminal; its chat panel disconnects";
const CONSOLE_RECOVERY: &str = "Choose Resume conversation in the new tab (+) menu afterwards";

fn stop_unchecked(
    inspection: &mut SourceInspection,
    saved: &[(crate::model::CardId, CardKind)],
    checked: &HashSet<crate::model::TerminalId>,
    current_boot: &crate::model::BootId,
) {
    for (_, kind) in saved {
        let id = match kind {
            CardKind::Chat(source) => source.id,
            CardKind::Terminal(source) => source.id,
            _ => continue,
        };
        if checked.contains(&id)
            || inspection
                .details
                .panes
                .get(&id)
                .is_some_and(|pane| pane.outcome == SavedOutcome::Unchanged)
        {
            continue;
        }
        match kind {
            CardKind::Chat(source) if !source.is_unstarted() => {
                inspection.stop(source.id, StoppedKind::Chat, UNCHECKED, CHAT_RECOVERY)
            }
            CardKind::Terminal(source) if terminal_requires_migration(source, current_boot) => {
                inspection.stop(
                    source.id,
                    StoppedKind::Terminal,
                    UNCHECKED,
                    TERMINAL_RECOVERY,
                )
            }
            _ => {}
        }
    }
}

fn inspect(request: Request) -> Result<SourceInspection, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let current_boot =
        crate::daemon::read_boot_id().map_err(|_| "Cannot verify the current kernel boot")?;
    let paths = crate::instance::window_database_paths()
        .map_err(|_| "Cannot enumerate saved source windows")?;
    let mut inspection = SourceInspection::default();
    let mut budget = crate::persistence::MigrationInspectionBudget::new(deadline);
    let mut saved = Vec::new();
    for (window, path) in paths.into_iter().enumerate() {
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            _ => {}
        }
        match crate::persistence::read_labeled_migration_sessions_with_budget(&path, &mut budget) {
            Ok(panes) => {
                for crate::persistence::MigrationPane {
                    card,
                    kind,
                    label,
                    inert,
                } in panes
                {
                    let pane = match &kind {
                        CardKind::Chat(source) => source.id,
                        CardKind::Terminal(source) => source.id,
                        _ => continue,
                    };
                    inspection.labels.entry(pane).or_insert(label);
                    record_saved_pane(
                        &mut inspection,
                        &kind,
                        inert,
                        &current_boot,
                        request.require_resumable,
                    );
                    if !inert {
                        saved.push((card, kind));
                    }
                }
            }
            Err(_) => inspection.stopped_windows.push(StoppedWindow {
                window: window + 1,
                reason: format!(
                    "A saved window ({}) could not be read; it may not reopen",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ),
                recovery: "Its saved file stays as it is".into(),
            }),
        }
    }
    let evidence = ExecutionEvidence::load(&current_boot, deadline);
    record_ended_panes(&mut inspection, &saved, &evidence);
    if check_deadline(deadline).is_err() {
        inspection_failure(&mut inspection, "Checking saved panes took too long");
        stop_unchecked(&mut inspection, &saved, &HashSet::new(), &current_boot);
        return Ok(inspection);
    }
    // With no running source service there is still useful per-pane evidence.
    // A saved pane is never a reason to refuse replacing the application.
    if request.proof.is_none() {
        let mut checked = HashSet::new();
        for (_, kind) in &saved {
            if check_deadline(deadline).is_err() {
                inspection_failure(&mut inspection, "Checking saved panes took too long");
                stop_unchecked(&mut inspection, &saved, &checked, &current_boot);
                break;
            }
            match kind {
                CardKind::Chat(source) => {
                    checked.insert(source.id);
                }
                CardKind::Terminal(source) => {
                    checked.insert(source.id);
                }
                _ => {}
            }
            match kind {
                CardKind::Chat(source) if !source.is_unstarted() => {
                    if let Some((reason, recovery)) =
                        saved_chat_problem(source, &[], &evidence, || {
                            Err("No running service".into())
                        })
                    {
                        inspection.stop(source.id, StoppedKind::Chat, &reason, recovery);
                    }
                }
                CardKind::Terminal(source)
                    if terminal_requires_migration(source, &current_boot) =>
                {
                    inspection.stop(
                        source.id,
                        StoppedKind::Terminal,
                        "its terminal could not be matched",
                        TERMINAL_RECOVERY,
                    )
                }
                _ => {}
            }
        }
        return Ok(inspection);
    }
    let mut reader = match connect(&request, deadline) {
        Ok(reader) => reader,
        Err(reason) => {
            inspection_failure(&mut inspection, &reason);
            stop_unchecked(&mut inspection, &saved, &HashSet::new(), &current_boot);
            return Ok(inspection);
        }
    };
    let mut checked_saved = HashSet::new();
    let inspect_live = (|| -> Result<(), String> {
        let initial_inventory = reader.list()?;
        let mut resolutions: Vec<(Session, Result<Session, String>)> = Vec::new();
        let mut cold_history = Vec::new();
        for (_, kind) in &saved {
            check_deadline(deadline)?;
            if let CardKind::Chat(source) = kind {
                if !source.is_unstarted()
                    && (source.proof.as_ref() != request.proof.as_ref()
                        || !initial_inventory.iter().any(|live| live.id == source.id))
                    && !resolutions.iter().any(|(issued, _)| issued == source)
                    && !request.proof.as_ref().is_some_and(|proof| {
                        relisted_codex_conversation(
                            source,
                            &initial_inventory,
                            proof,
                            request.require_resumable,
                        )
                    })
                {
                    let missing = !initial_inventory.iter().any(|live| live.id == source.id);
                    if missing
                        && matches!(
                            evidence.classify(source),
                            SavedExecution::Ended | SavedExecution::Unrecorded
                        )
                    {
                        continue;
                    }
                    let result = if missing && request.reconcile_retained {
                        reader.resolve(source)
                    } else if missing {
                        Err("Saved owner changed after preview".into())
                    } else {
                        reader.resolve(source)
                    };
                    if missing
                        && source.is_saved_claude_history()
                        && !result
                            .as_ref()
                            .is_ok_and(|resolved| resolved.native_continuity.retains_provider())
                        && reader.inspect_saved_history(source).is_ok()
                    {
                        cold_history.push(source.clone());
                    }
                    resolutions.push((source.clone(), result));
                }
            }
        }
        let inventory = reader.list()?;
        let terminals = reader.terminals()?;
        if inventory.len() > handoff::MAX_CHATS || terminals.len() > handoff::MAX_TERMINALS {
            return Err("Source inventory exceeds its inspection limit".into());
        }
        let proof = request
            .proof
            .as_ref()
            .ok_or("Source service is unavailable")?;
        record_live_panes(
            &mut inspection,
            &inventory,
            &terminals,
            proof,
            request.require_resumable,
        );
        let matched_cli = crate::companion::Product::Cli
            .beside_current()
            .map_err(|_| "Cannot inspect the destination console companion")?;
        let mut ids = HashSet::new();
        for session in &inventory {
            check_deadline(deadline)?;
            if !ids.insert(session.id) || session.proof.as_ref() != Some(proof) {
                inspection.stop(
                    session.id,
                    StoppedKind::Chat,
                    "its conversation could not be confirmed",
                    CHAT_RECOVERY,
                );
                continue;
            }
            if request.require_resumable
                && session.terminal.as_ref().is_some_and(|terminal| {
                    terminal.continuity.can_resume_live(proof)
                        && qualify_terminal(terminal, &terminals, proof).is_ok()
                        && qualify_console_migration(terminal, &matched_cli).is_err()
                })
            {
                console_keeps_running(
                    &mut inspection,
                    session.id,
                    crate::update::impact::SessionKind::NativeChat,
                );
            } else if validate_live_chat(session, request.require_resumable).is_err() {
                inspection.stop(
                    session.id,
                    StoppedKind::Chat,
                    "its conversation could not be confirmed",
                    CHAT_RECOVERY,
                );
            }
        }
        for terminal in &terminals {
            check_deadline(deadline)?;
            if request.require_resumable
                && !inventory.iter().any(|chat| chat.id == terminal.session.id)
                && terminal.session.continuity.can_resume_live(proof)
                && qualify_console_migration(&terminal.session, &matched_cli).is_err()
            {
                console_keeps_running(
                    &mut inspection,
                    terminal.session.id,
                    crate::update::impact::SessionKind::Terminal,
                );
            }
        }
        for (_, kind) in &saved {
            check_deadline(deadline)?;
            match kind {
                CardKind::Chat(source) => {
                    checked_saved.insert(source.id);
                }
                CardKind::Terminal(source) => {
                    checked_saved.insert(source.id);
                }
                _ => {}
            }
            match kind {
                CardKind::Terminal(source)
                    if terminal_requires_migration(source, &current_boot) =>
                {
                    if qualify_terminal(source, &terminals, proof).is_err() {
                        inspection.stop(
                            source.id,
                            StoppedKind::Terminal,
                            "its terminal could not be matched",
                            TERMINAL_RECOVERY,
                        );
                    }
                }
                CardKind::Chat(source) if !source.is_unstarted() => {
                    if inspection
                        .details
                        .panes
                        .get(&source.id)
                        .is_some_and(|pane| !pane.detail.is_empty())
                    {
                        continue;
                    }
                    if cold_history.iter().any(|issued| issued == source)
                        || relisted_codex_conversation(
                            source,
                            &inventory,
                            proof,
                            request.require_resumable,
                        )
                    {
                        if cold_history.iter().any(|issued| issued == source) {
                            inspection
                                .details
                                .panes
                                .get_mut(&source.id)
                                .unwrap()
                                .outcome = SavedOutcome::Unchanged;
                        }
                        continue;
                    }
                    if let Some((reason, recovery)) =
                        saved_chat_problem(source, &inventory, &evidence, || {
                            qualify_with_mode(
                                source,
                                &inventory,
                                proof,
                                request.require_resumable,
                                |source| {
                                    resolutions
                                        .iter()
                                        .find(|(issued, _)| issued == source)
                                        .map(|(_, result)| result.clone())
                                        .unwrap_or_else(|| {
                                            Err("No authenticated source resolution".into())
                                        })
                                },
                            )
                        })
                    {
                        inspection.stop(source.id, StoppedKind::Chat, &reason, recovery);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();
    if let Err(reason) = inspect_live {
        inspection_failure(&mut inspection, &reason);
        stop_unchecked(&mut inspection, &saved, &checked_saved, &current_boot);
    }
    Ok(inspection)
}

fn record_saved_pane(
    inspection: &mut SourceInspection,
    kind: &CardKind,
    inert: bool,
    boot: &crate::model::BootId,
    replaces_service: bool,
) {
    use crate::update::impact::SessionKind;
    let (id, kind, outcome) = match kind {
        CardKind::Chat(source) => (
            source.id,
            SessionKind::NativeChat,
            if inert || source.is_unstarted() {
                SavedOutcome::Unchanged
            } else if replaces_service && !source.native_continuity.retains_provider() {
                SavedOutcome::Resumes
            } else {
                SavedOutcome::KeepsRunning
            },
        ),
        CardKind::Terminal(source) => (
            source.id,
            SessionKind::Terminal,
            if terminal_requires_migration(source, boot) {
                SavedOutcome::KeepsRunning
            } else {
                SavedOutcome::Unchanged
            },
        ),
        _ => return,
    };
    inspection.details.panes.entry(id).or_insert(InspectedPane {
        kind,
        outcome,
        detail: String::new(),
    });
}

fn record_ended_panes(
    inspection: &mut SourceInspection,
    saved: &[(crate::model::CardId, CardKind)],
    evidence: &ExecutionEvidence,
) {
    for (_, kind) in saved {
        if let CardKind::Chat(source) = kind
            && !source.is_unstarted()
            && evidence.classify(source) == SavedExecution::Ended
            && let Some(pane) = inspection.details.panes.get_mut(&source.id)
        {
            pane.outcome = SavedOutcome::Unchanged;
        }
    }
}

fn record_live_panes(
    inspection: &mut SourceInspection,
    chats: &[Session],
    terminals: &[v77::Terminal],
    proof: &HostSessionProof,
    replaces_service: bool,
) {
    for session in chats {
        inspection.details.panes.remove(&session.id);
        record_saved_pane(
            inspection,
            &CardKind::Chat(session.clone()),
            false,
            &proof.boot_id,
            replaces_service,
        );
    }
    for terminal in terminals {
        if chats.iter().any(|chat| chat.id == terminal.session.id) {
            continue;
        }
        inspection.details.panes.remove(&terminal.session.id);
        record_saved_pane(
            inspection,
            &CardKind::Terminal(terminal.session.clone()),
            false,
            &proof.boot_id,
            replaces_service,
        );
    }
}

fn console_keeps_running(
    inspection: &mut SourceInspection,
    id: crate::model::TerminalId,
    kind: crate::update::impact::SessionKind,
) {
    inspection.details.panes.insert(
        id,
        InspectedPane {
            kind,
            outcome: SavedOutcome::KeepsRunning,
            detail: format!("{CONSOLE_REASON}. {CONSOLE_RECOVERY}."),
        },
    );
}

fn inspection_failure(inspection: &mut SourceInspection, reason: &str) {
    let lower = reason.to_ascii_lowercase();
    let readable = if lower.contains("console companion") {
        "the console helper installed with this build could not be checked".to_owned()
    } else if lower.contains("timed out")
        || lower.contains("budget")
        || lower.contains("took too long")
    {
        "checking panes took too long".to_owned()
    } else if lower.contains("ownership")
        || lower.contains("owner")
        || lower.contains("proof")
        || lower.contains("authenticate")
    {
        "the running terminal service's identity changed or could not be verified".to_owned()
    } else if lower.contains("connect")
        || lower.contains("namespace")
        || lower.contains("unavailable")
        || lower.contains("send")
    {
        "the running terminal service could not be reached".to_owned()
    } else if lower.contains("limit") || lower.contains("exceeds") || lower.contains("exceeded") {
        "the running terminal service sent more pane data than could be checked".to_owned()
    } else if lower.contains("protocol")
        || lower.contains("response")
        || lower.contains("contract")
        || lower.contains("unsupported")
    {
        "the running terminal service returned pane data this build could not read".to_owned()
    } else {
        reason
            .replace("Source service", "The running terminal service")
            .replace("source service", "running terminal service")
            .replace("Source", "The running terminal service")
    };
    inspection
        .details
        .notices
        .push(format!("Live panes could not all be checked: {readable}."));
}

// Cold terminal presentation is retained unchanged. An actual previous boot
// proves its old process cannot survive; a missing cwd or absent inventory in
// the same boot does not. Never replay a provider hint or synthesize an exit.
fn terminal_requires_migration(
    session: &crate::model::TerminalSession,
    current_boot: &crate::model::BootId,
) -> bool {
    match &session.continuity {
        crate::model::TerminalContinuity::Exited { .. }
        | crate::model::TerminalContinuity::RelaunchAvailable { .. } => false,
        _ => !session
            .continuity
            .live_proof()
            .is_some_and(|proof| proof.is_complete() && &proof.boot_id != current_boot),
    }
}

/// What read-only evidence proves about the old owner of a saved chat pane
/// the source service does not list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SavedExecution {
    /// No process of any owner it ever had can remain. Replacing the source
    /// service cannot lose or strand work it no longer has; the destination
    /// decides separately, through its own resume proof, whether to reopen it.
    Ended,
    /// A registered helper, its provider, or a process in their group or
    /// session still exists outside the source inventory.
    Running,
    /// The pane has execution this evidence does not cover, or the evidence
    /// is absent, foreign or unreadable.
    Unproved,
    /// The bounded census or process inspection could not be completed.
    Unchecked,
    /// A pane that never learned its conversation, every actor of which has
    /// ended, but at least one without the retirement that records what it
    /// confirmed. Nothing still runs, yet nothing durable names a
    /// conversation to reopen: the destination keeps the pane and refuses to
    /// resume or replace it. The update keeps it stopped for explicit recovery.
    Unrecorded,
}

/// The source daemon's own ownership evidence, read without its lease and
/// without changing it. Retirement receipts stay lease-gated in the
/// destination's resume path: this proves only an absence of execution.
struct ExecutionEvidence {
    boot: crate::model::BootId,
    native_holders: Result<std::path::PathBuf, String>,
    census: Result<Census, String>,
    /// The census owners whose retirement is recorded, with what it confirmed.
    retired: Result<Vec<crate::daemon::holder_registry::Record>, String>,
    /// Where process and socket tables are read; `/proc` outside tests.
    processes: std::path::PathBuf,
    /// How long a process that may still run can show no arguments before
    /// it proves nothing.
    exec_settle: Duration,
    deadline: Option<Instant>,
}

/// Every retained helper the durable census registered, by pane.
type Census = Vec<(
    crate::model::TerminalId,
    crate::daemon::holder_registry::Record,
)>;

/// The census is a bounded roster, not a log; a larger one is not inspected.
const MAX_CENSUS_ROWS: usize = 16 * 1024;

impl ExecutionEvidence {
    fn running_pid(&self, source: &Session) -> Option<i32> {
        let owners = self
            .census
            .as_ref()
            .ok()
            .into_iter()
            .flatten()
            .filter(|(pane, _)| *pane == source.id)
            .map(|(_, owner)| owner)
            .collect::<Vec<_>>();
        for owner in &owners {
            for expected in [&owner.root, &owner.holder] {
                let stat = self.processes.join(expected.pid.to_string()).join("stat");
                if std::fs::read(stat)
                    .ok()
                    .and_then(|stat| crate::model::agent_recovery::process_identity(&stat))
                    .is_some_and(|(_, start)| start == expected.start)
                {
                    return Some(expected.pid);
                }
            }
        }
        let marker = format!("cagents-codex-{}", source.id);
        for entry in std::fs::read_dir(&self.processes).ok()? {
            if self
                .deadline
                .is_some_and(|deadline| check_deadline(deadline).is_err())
            {
                return None;
            }
            let Ok(entry) = entry else {
                continue;
            };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            if !owners.is_empty() {
                let Some(stat) = std::fs::read(entry.path().join("stat")).ok() else {
                    continue;
                };
                let Some(close) = stat.iter().rposition(|byte| *byte == b')') else {
                    continue;
                };
                let fields = String::from_utf8_lossy(&stat[close + 1..]);
                let mut fields = fields.split_ascii_whitespace().skip(2);
                let identities = [fields.next(), fields.next()]
                    .map(|field| field.and_then(|field| field.parse::<i32>().ok()));
                if owners.iter().any(|owner| {
                    identities
                        .iter()
                        .flatten()
                        .any(|id| *id == owner.root.pid || *id == owner.holder.pid)
                }) {
                    return Some(pid);
                }
            }
            if source.launch.provider == crate::agent::Provider::Codex {
                if let Ok(Threads::Arguments(arguments)) = process_arguments(&entry.path()) {
                    if arguments
                        .windows(marker.len())
                        .any(|window| window == marker.as_bytes())
                    {
                        return Some(pid);
                    }
                }
            }
        }
        None
    }

    fn load(boot: &crate::model::BootId, deadline: Instant) -> Self {
        let store = crate::instance::project_dirs()
            .ok_or_else(|| "The data directory is unavailable".to_owned())
            .map(|dirs| dirs.data_dir().join("workspace-model.sqlite3"));
        Self {
            boot: boot.clone(),
            native_holders: crate::daemon::holder_registry::directory(
                crate::daemon::holder_registry::Kind::Native,
            ),
            census: store.clone().and_then(|path| read_census(&path)),
            retired: store.and_then(|path| read_retired_owners(&path)),
            processes: "/proc".into(),
            exec_settle: EXEC_SETTLE,
            deadline: Some(deadline),
        }
    }

    /// This daemon's own evidence about a Codex pane of its boot, which
    /// [`Self::classify_codex`] reads without the source census.
    fn codex(boot: &crate::model::BootId) -> Self {
        Self {
            boot: boot.clone(),
            native_holders: crate::daemon::holder_registry::directory(
                crate::daemon::holder_registry::Kind::Native,
            ),
            census: Err("A Codex pane has no census".into()),
            retired: Err("A Codex pane has no census".into()),
            processes: "/proc".into(),
            exec_settle: EXEC_SETTLE,
            deadline: None,
        }
    }

    /// A kernel boot change ends every process of the saved boot. Within this
    /// boot a Codex pane is judged by its own listener (`classify_codex`).
    /// Among Claude panes only a retained actor has an owner roster: each helper
    /// registers in the durable census once it answers, and its discovery
    /// record is published shortly before that. Every owner either names must
    /// be gone with its provider and nothing left in their process group or
    /// session: the process proof the destination requires before it resumes
    /// an abandoned actor, which a verified retirement also satisfies. A
    /// restart still before its record is invisible here; `inspect` looks
    /// twice, before resolution and against the final roster, to narrow it.
    fn classify(&self, source: &Session) -> SavedExecution {
        use crate::daemon::holder_registry::{self, Kind};
        if source.launch.provider == crate::agent::Provider::Codex {
            return self.classify_codex(source);
        }
        if source.proof.is_none() {
            return self.classify_proofless(source);
        }
        let Some(proof) = source.proof.as_ref().filter(|proof| proof.is_complete()) else {
            return SavedExecution::Unproved;
        };
        if proof.boot_id != self.boot {
            return SavedExecution::Ended;
        }
        // A paired console runs outside the actor; this roster cannot see it.
        if !matches!(
            source.native_continuity,
            crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
        ) || !source.has_valid_native_continuity()
            || source.terminal.is_some()
        {
            return SavedExecution::Unproved;
        }
        let (Ok(census), Ok(native_holders)) = (&self.census, &self.native_holders) else {
            return SavedExecution::Unchecked;
        };
        let mut owners = census
            .iter()
            .filter(|(pane, _)| *pane == source.id)
            .map(|(_, owner)| owner)
            .collect::<Vec<_>>();
        // No registered owner is no evidence that one ended.
        if owners.is_empty() {
            return SavedExecution::Unproved;
        }
        let discovered = match holder_registry::recovery_candidate(
            &holder_registry::socket(native_holders, source.id),
            Kind::Native,
        ) {
            Ok(discovered) => discovered,
            Err(_) => return SavedExecution::Unchecked,
        };
        owners.extend(&discovered);
        // A pane that never learned its conversation resumes only the one a
        // recorded retirement says its actor confirmed; without that record
        // its ended actor leaves it nothing to reopen.
        let unnamed = source.has_unnamed_retained_claude_actor();
        let mut execution = SavedExecution::Ended;
        let mut unrecorded = false;
        for owner in owners.into_iter().filter(|owner| owner.boot == self.boot) {
            if self
                .deadline
                .is_some_and(|deadline| check_deadline(deadline).is_err())
            {
                return SavedExecution::Unchecked;
            }
            match crate::chat::runtime::retained_actor_abandoned(&owner.holder, &owner.root) {
                Ok(true) => {}
                Ok(false) => execution = SavedExecution::Running,
                Err(_) => return SavedExecution::Unchecked,
            }
            match &self.retired {
                Ok(retired) => unrecorded |= unnamed && !retired.contains(owner),
                Err(_) if unnamed => return SavedExecution::Unchecked,
                Err(_) => {}
            }
        }
        if execution == SavedExecution::Ended && unrecorded {
            SavedExecution::Unrecorded
        } else {
            execution
        }
    }

    /// A pane with no issued proof names no daemon that owns it. If it once
    /// ran as a retained Claude actor, the census still names every owner it
    /// had under its pane. It has ended only when each of those owners,
    /// including one a discovery record names, has a recorded retirement and
    /// none of this boot's remains with its provider or anything in their
    /// process group or session. Without any census owner nothing proves
    /// what ran it. Its continuity need not still say retained: any later
    /// hosted provider would have issued the pane a proof, so a proofless
    /// pane's only execution is the actors the census registered for it.
    fn classify_proofless(&self, source: &Session) -> SavedExecution {
        use crate::daemon::holder_registry::{self, Kind};
        if source.launch.provider != crate::agent::Provider::Claude
            || source.terminal.is_some()
            || source.owned_codex_listener.is_some()
        {
            return SavedExecution::Unproved;
        }
        let (Ok(census), Ok(native_holders), Ok(retired)) =
            (&self.census, &self.native_holders, &self.retired)
        else {
            return SavedExecution::Unchecked;
        };
        let mut owners = census
            .iter()
            .filter(|(pane, _)| *pane == source.id)
            .map(|(_, owner)| owner)
            .collect::<Vec<_>>();
        if owners.is_empty() {
            return SavedExecution::Unproved;
        }
        let discovered = match holder_registry::recovery_candidate(
            &holder_registry::socket(native_holders, source.id),
            Kind::Native,
        ) {
            Ok(discovered) => discovered,
            Err(_) => return SavedExecution::Unchecked,
        };
        owners.extend(&discovered);
        let mut execution = SavedExecution::Ended;
        for owner in owners {
            if self
                .deadline
                .is_some_and(|deadline| check_deadline(deadline).is_err())
            {
                return SavedExecution::Unchecked;
            }
            if !retired.contains(owner) {
                return SavedExecution::Unproved;
            }
            if owner.boot != self.boot {
                continue;
            }
            match crate::chat::runtime::retained_actor_abandoned(&owner.holder, &owner.root) {
                Ok(true) => {}
                Ok(false) => execution = SavedExecution::Running,
                Err(_) => return SavedExecution::Unchecked,
            }
        }
        execution
    }

    /// Classify execution independently of adoption capabilities. Missing
    /// listener ownership or continuity is not evidence of a surviving writer.
    fn classify_codex(&self, source: &Session) -> SavedExecution {
        match self.codex_execution(source) {
            CodexExecution::Ended => SavedExecution::Ended,
            CodexExecution::Running { .. } | CodexExecution::RetainedProcesses => {
                SavedExecution::Running
            }
            CodexExecution::Unproved { .. } => SavedExecution::Unchecked,
        }
    }

    fn codex_execution(&self, source: &Session) -> CodexExecution {
        self.inspect_codex_execution(source)
            .unwrap_or_else(|reason| CodexExecution::Unproved { reason })
    }

    fn inspect_codex_execution(&self, source: &Session) -> Result<CodexExecution, String> {
        use crate::daemon::holder_registry::{self, Kind};
        let native_holders = self.native_holders.as_ref().map_err(Clone::clone)?;
        let mut controls = vec![holder_registry::socket(native_holders, source.id)];
        if let Some(terminal) = &source.terminal {
            let directory = holder_registry::directory(Kind::Terminal)?;
            controls.push(holder_registry::socket(&directory, terminal.id));
        }
        let holders = controls
            .iter()
            .map(|control| {
                holder_registry::execution_record(control)
                    .map_err(|error| format!("Cannot read holder record: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        // Session.proof records the native execution's boot, including legacy
        // panes. The console, allocator and holder each retain their own boot.
        // Even unanimous earlier-boot descriptors can be stale after a daemon
        // restart. Scan this boot too; the old boots only settle unreadable scans.
        let terminal_proof = source
            .terminal
            .as_ref()
            .and_then(|terminal| terminal.continuity.live_proof());
        let boots = source
            .proof
            .iter()
            .map(|proof| &proof.boot_id)
            .chain(terminal_proof.iter().map(|proof| &proof.boot_id))
            .chain(
                source
                    .owned_codex_listener
                    .iter()
                    .map(|listener| &listener.allocator.boot_id),
            )
            .chain(
                source
                    .terminal
                    .iter()
                    .filter_map(|terminal| terminal.owned_codex_listener.as_ref())
                    .map(|listener| &listener.allocator.boot_id),
            )
            .chain(holders.iter().flatten().map(|holder| &holder.boot))
            .collect::<Vec<_>>();
        let earlier_boot = !boots.is_empty()
            && boots
                .iter()
                .all(|boot| !boot.as_str().is_empty() && **boot != self.boot);
        let mut retained_processes = false;
        for holder in holders
            .iter()
            .flatten()
            .filter(|holder| holder.boot == self.boot)
        {
            if !crate::chat::runtime::codex_execution_abandoned(&holder.holder, &holder.root)? {
                // Surviving registry identities fence replacement, but prove
                // neither an identifiable Codex PID nor a bound server socket.
                retained_processes = true;
            }
        }
        let mut unreadable_listener = None;
        // Legacy panes still record their endpoint in argv. Inspect it even
        // without allocator or continuity evidence, including Codex's published
        // symlink target and sockets unlinked while a server remains bound.
        let mut endpoints = source
            .recorded_codex_socket()
            .into_iter()
            .collect::<Vec<_>>();
        endpoints.extend(
            source
                .owned_codex_listener
                .iter()
                .map(|listener| listener.socket.clone()),
        );
        if let Some(terminal) = &source.terminal {
            endpoints.extend(crate::chat::recorded_codex_socket(&terminal.launch));
            endpoints.extend(
                terminal
                    .owned_codex_listener
                    .iter()
                    .map(|listener| listener.socket.clone()),
            );
        }
        endpoints.sort();
        endpoints.dedup();
        let mut sockets = controls;
        for endpoint in endpoints {
            match std::fs::read_link(&endpoint) {
                Ok(target) => sockets.push(endpoint.with_file_name(target)),
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        || error.raw_os_error() == Some(libc::EINVAL) => {}
                Err(error) => {
                    unreadable_listener =
                        Some(format!("Cannot read Codex listener target: {error}"));
                }
            }
            sockets.push(endpoint);
        }
        let sockets = sockets
            .iter()
            .map(|socket| socket.as_path())
            .collect::<Vec<_>>();
        let scan = codex_listener_survives(
            &self.processes,
            source.id,
            source.launch.session_id.as_deref(),
            &sockets,
            self.exec_settle,
            self.deadline,
        );
        if let Some(deadline) = self.deadline {
            check_deadline(deadline)?;
        }
        match (scan, retained_processes, unreadable_listener) {
            (Ok(running @ CodexExecution::Running { .. }), _, _) => Ok(running),
            (_, true, _) => Ok(CodexExecution::RetainedProcesses),
            (Err(reason), _, _) if reason == CODEX_PROCESS_STARTING => Err(reason),
            (Err(_), _, _) | (_, _, Some(_)) if earlier_boot => Ok(CodexExecution::Ended),
            (_, _, Some(reason)) => Err(reason),
            (result, _, _) => result,
        }
    }
}

pub(super) const CODEX_PROCESS_STARTING: &str = "A process has not published its arguments yet";

/// The same execution proof used by source inspection, close and resume.
/// A socket or surviving process group need not identify a live process ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CodexExecution {
    Ended,
    Running {
        pid: Option<u32>,
        socket: PathBuf,
    },
    /// Holder/group identities survive, without a verified writer or listener.
    RetainedProcesses,
    Unproved {
        reason: String,
    },
}

pub(super) fn codex_execution(source: &Session, boot: &crate::model::BootId) -> CodexExecution {
    if source.launch.provider != crate::agent::Provider::Codex {
        return CodexExecution::Unproved {
            reason: "The pane is not a Codex conversation".into(),
        };
    }
    ExecutionEvidence::codex(boot).codex_execution(source)
}

/// Whether nothing of a Codex pane of `boot` can still run, by the evidence
/// an upgrade requires before it replaces the service that ran it.
pub(super) fn codex_execution_ended(source: &Session, boot: &crate::model::BootId) -> bool {
    codex_execution(source, boot) == CodexExecution::Ended
}

/// What the evidence an upgrade requires before it replaces a service says
/// of what `source`'s execution left behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExecutionEnd {
    /// Nothing of it can still run: its proof names another kernel boot, or
    /// every owner of this boot is gone (see [`ExecutionEvidence::classify`]).
    Ended,
    /// A process or bound socket of it survives.
    Running,
    /// The evidence cannot tell, or does not cover this shape of pane.
    Unproved,
}

/// What remains of `source`'s execution. `owners` is the census of this
/// pane, each owner with whether its retirement is recorded, read from the
/// daemon's own store. An actor that ended without recording what it
/// confirmed has still ended: nothing of it remains to close, only nothing to
/// reopen.
pub(super) fn execution_end(
    source: &Session,
    boot: &crate::model::BootId,
    owners: Result<Vec<(crate::daemon::holder_registry::Record, bool)>, String>,
) -> ExecutionEnd {
    let census = owners.as_ref().map_err(Clone::clone).map(|owners| {
        owners
            .iter()
            .map(|(owner, _)| (source.id, owner.clone()))
            .collect()
    });
    let retired = owners.map(|owners| {
        owners
            .into_iter()
            .filter_map(|(owner, retired)| retired.then_some(owner))
            .collect()
    });
    let evidence = ExecutionEvidence {
        census,
        retired,
        ..ExecutionEvidence::codex(boot)
    };
    match evidence.classify(source) {
        SavedExecution::Ended | SavedExecution::Unrecorded => ExecutionEnd::Ended,
        SavedExecution::Running => ExecutionEnd::Running,
        SavedExecution::Unproved | SavedExecution::Unchecked => ExecutionEnd::Unproved,
    }
}

/// Whether a Codex/cagents process of this user names the pane or conversation
/// in its arguments, or a socket is still bound at one of `sockets`. A
/// process shows no arguments once its group leader has released its memory,
/// and between `execve` and publishing its new ones. It is evidence of
/// absence only when none of its threads can run again, and a live thread
/// shows the arguments its leader no longer does (see [`threads`]). One that
/// still shows none is read again for up to `settle`; a published executable
/// keeps it unproved, while no executable or argv excludes a kernel thread.
fn codex_listener_survives(
    processes: &Path,
    pane: crate::model::TerminalId,
    conversation: Option<&str>,
    sockets: &[&Path],
    settle: Duration,
    inspection_deadline: Option<Instant>,
) -> Result<CodexExecution, String> {
    use std::os::unix::ffi::OsStrExt;
    let marker = format!("cagents-codex-{pane}");
    let marker = marker.as_bytes();
    let uid = crate::daemon::current_uid().to_string();
    // Kernel threads never publish argv or an executable. Retry empty argv
    // within the same settle bound as exec, then exclude only processes with
    // neither argv nor an executable. A readable exe with unpublished argv
    // can still be a starting writer and must remain explicit uncertainty.
    let arguments = |path: &Path| {
        let remaining = inspection_deadline.map_or(settle, |deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .min(settle)
        });
        settled_arguments(path, remaining)
    };
    let mut unreadable = None;
    let entries = std::fs::read_dir(processes)
        .map_err(|error| format!("Cannot enumerate processes: {error}"))?;
    for entry in entries {
        if let Some(deadline) = inspection_deadline {
            check_deadline(deadline)?;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                unreadable.get_or_insert_with(|| format!("Cannot enumerate processes: {error}"));
                continue;
            }
        };
        if entry
            .file_name()
            .to_str()
            .is_none_or(|name| name.parse::<u32>().is_err())
        {
            continue;
        }
        let path = entry.path();
        // The effective user, not the owner of `/proc/<pid>`, which becomes
        // root for a process that made itself non-dumpable.
        let status = match std::fs::read_to_string(path.join("status")) {
            Ok(status) => status,
            Err(error) if inaccessible_process(&error) => continue,
            Err(error) => {
                unreadable.get_or_insert_with(|| format!("Cannot inspect process status: {error}"));
                continue;
            }
        };
        let effective = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|ids| ids.split_whitespace().nth(1));
        match effective {
            Some(effective) if effective != uid => continue,
            Some(_) => {}
            None => {
                unreadable.get_or_insert_with(|| "Cannot read a process's user".into());
                continue;
            }
        }
        let argv = match arguments(&path) {
            Ok(argv) => argv,
            Err(reason) => {
                if reason == CODEX_PROCESS_STARTING {
                    unreadable = Some(reason);
                } else {
                    unreadable.get_or_insert(reason);
                }
                continue;
            }
        };
        let Some(argv) = argv else { continue };
        // Identify the executable as well as its arguments: argv[0] can be
        // rewritten, and a command merely mentioning a path is no writer.
        let executable = match process_executable(&path) {
            Ok(Some(executable)) => executable,
            Ok(None) => continue,
            Err(reason) => {
                unreadable.get_or_insert(reason);
                continue;
            }
        };
        if codex_writer_arguments(&executable, &argv, marker, conversation) {
            return Ok(CodexExecution::Running {
                pid: entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse().ok()),
                socket: sockets
                    .first()
                    .map_or_else(PathBuf::new, |socket| socket.to_path_buf()),
            });
        }
    }
    let bound = std::fs::read(processes.join("net/unix")).map_err(|error| {
        unreadable
            .clone()
            .unwrap_or_else(|| format!("Cannot inspect bound sockets: {error}"))
    })?;
    // Paths may contain spaces: match the complete suffix after the columns.
    for line in bound.split(|byte| *byte == b'\n').skip(1) {
        for socket in sockets {
            let direct = socket.as_os_str().as_bytes();
            let matches = line.len() > direct.len()
                && line.ends_with(direct)
                && line[line.len() - direct.len() - 1] == b' ';
            // Long holder paths bind via a directory FD which closes after
            // bind. /proc/net/unix retains that alias, so resolving it later
            // would be wrong. The exact UUID basename still names this pane.
            let alias = !crate::daemon::holder_registry::socket_fits(socket)
                && line
                    .split(|byte| *byte == b' ')
                    .next_back()
                    .is_some_and(|path| {
                        path.strip_prefix(b"/proc/self/fd/")
                            .and_then(|rest| {
                                let slash = rest.iter().position(|byte| *byte == b'/')?;
                                let (fd, name) = rest.split_at(slash);
                                Some(
                                    !fd.is_empty()
                                        && fd.iter().all(u8::is_ascii_digit)
                                        && socket.file_name().is_some_and(|expected| {
                                            expected.as_bytes() == &name[1..]
                                        }),
                                )
                            })
                            .unwrap_or(false)
                    });
            if matches || alias {
                return Ok(CodexExecution::Running {
                    pid: None,
                    socket: socket.to_path_buf(),
                });
            }
        }
    }
    unreadable.map_or(Ok(CodexExecution::Ended), Err)
}

/// This user's processes started to serve a Codex pane's listener, by the
/// `--listen unix://<socket>` its provider is given: the conversation's
/// provider, which a person closing it can end once its helper has gone (see
/// [`crate::daemon::lost_processes`]). A process that merely names the
/// listener, such as one listing its directory, is not one. A process still
/// starting is not judged: the next look sees it.
pub(super) fn codex_listener_providers(
    pane: crate::model::TerminalId,
    socket: &Path,
) -> Result<Vec<crate::daemon::holder_registry::Process>, String> {
    processes_serving_listener(Path::new("/proc"), pane, socket, EXEC_SETTLE)
}

fn processes_serving_listener(
    processes: &Path,
    pane: crate::model::TerminalId,
    socket: &Path,
    settle: Duration,
) -> Result<Vec<crate::daemon::holder_registry::Process>, String> {
    let marker = format!("cagents-codex-{pane}");
    let uid = crate::daemon::current_uid().to_string();
    let mut serving = Vec::new();
    let entries = std::fs::read_dir(processes)
        .map_err(|error| format!("Cannot enumerate processes: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("Cannot enumerate processes: {error}"))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let path = entry.path();
        let status = match std::fs::read_to_string(path.join("status")) {
            Ok(status) => status,
            Err(error) if inaccessible_process(&error) => continue,
            Err(error) => return Err(format!("Cannot inspect process status: {error}")),
        };
        // The effective user, as in [`codex_listener_survives`].
        if status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|ids| ids.split_whitespace().nth(1))
            != Some(uid.as_str())
        {
            continue;
        }
        let argv = match settled_arguments(&path, settle) {
            Ok(Some(argv)) => argv,
            Ok(None) => continue,
            Err(reason) if reason == CODEX_PROCESS_STARTING => continue,
            Err(reason) => return Err(reason),
        };
        if !argv
            .windows(marker.len())
            .any(|window| window == marker.as_bytes())
            || !serves_listener(socket, &argv)
        {
            continue;
        }
        // Read after the arguments: an identity that changed meanwhile names
        // a later process, which the next scan judges afresh.
        let start = match std::fs::read(path.join("stat")) {
            Ok(stat) => crate::model::agent_recovery::process_identity(&stat)
                .map(|(_, start)| start)
                .ok_or("A process has an unreadable identity")?,
            Err(error) if gone(&error) => continue,
            Err(error) => return Err(format!("Cannot inspect a process: {error}")),
        };
        serving.push(crate::daemon::holder_registry::Process { pid, start });
    }
    Ok(serving)
}

/// Whether `argv` gives exactly `socket` to listen on, as `--listen
/// unix://<socket>` or `--listen=unix://<socket>`.
fn serves_listener(socket: &Path, argv: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let mut endpoint = b"unix://".to_vec();
    endpoint.extend_from_slice(socket.as_os_str().as_bytes());
    let mut joined = b"--listen=".to_vec();
    joined.extend_from_slice(&endpoint);
    let arguments: Vec<&[u8]> = argv.split(|byte| *byte == 0).collect();
    arguments
        .iter()
        .any(|argument| *argument == joined.as_slice())
        || arguments
            .windows(2)
            .any(|pair| pair[0] == b"--listen" && pair[1] == endpoint.as_slice())
}

/// A process's arguments, read again for up to `settle` while it shows none
/// (see [`codex_listener_survives`]). `Ok(None)` is a kernel thread or a
/// process that has ended.
fn settled_arguments(path: &Path, settle: Duration) -> Result<Option<Vec<u8>>, String> {
    let deadline = Instant::now() + settle;
    settled_process_arguments(
        || {
            let observation = process_arguments(path)?;
            if matches!(observation, Threads::Unsettled) && Instant::now() >= deadline {
                if process_executable(path)?.is_none() {
                    // Recheck every thread after exe: publication between
                    // the reads must not look like a kernel thread.
                    return process_arguments_after_settle(path);
                }
            }
            Ok(observation)
        },
        || {
            if Instant::now() >= deadline {
                return Err(CODEX_PROCESS_STARTING.into());
            }
            std::thread::sleep(Duration::from_millis(2));
            Ok(())
        },
    )
}

fn codex_writer_arguments(
    executable: &Path,
    argv: &[u8],
    marker: &[u8],
    conversation: Option<&str>,
) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let named_writer = |name: &std::ffi::OsStr| {
        let name = name.as_bytes();
        let name = name.strip_suffix(b" (deleted)").unwrap_or(name);
        name == b"codex"
            || name == b"cagents"
            || name == b"cagentsd"
            || name.starts_with(b"cagents-")
    };
    let name = executable.file_name().unwrap_or_default();
    // Interpreter installations publish the script as argv[1]. Verify the
    // interpreter too, so `ls /path/codex` or `exec -a codex ls` cannot acquire
    // writer identity. Native Codex is identified even with a rewritten argv[0].
    let interpreter = name
        .to_str()
        .is_some_and(crate::agent::Provider::is_interpreter)
        || matches!(
            name.as_bytes(),
            b"sh" | b"bash" | b"dash" | b"python" | b"python3"
        );
    let script = interpreter
        && argv.split(|byte| *byte == 0).nth(1).is_some_and(|script| {
            Path::new(std::ffi::OsStr::from_bytes(script))
                .file_name()
                .is_some_and(|name| named_writer(name) || name == "codex.js")
        });
    (named_writer(name) || script)
        && argv.split(|byte| *byte == 0).any(|arg| {
            arg.windows(marker.len()).any(|window| window == marker)
                || conversation
                    .filter(|id| !id.is_empty())
                    .is_some_and(|id| arg.windows(id.len()).any(|window| window == id.as_bytes()))
        })
}

/// A leader that called pthread_exit no longer exposes /proc/PID/exe even
/// while a sibling still runs the shared executable. Its thread path does.
fn process_executable(process: &Path) -> Result<Option<PathBuf>, String> {
    let unreadable = |error: io::Error| format!("Cannot inspect process executable: {error}");
    match std::fs::read_link(process.join("exe")) {
        Ok(executable) => return Ok(Some(executable)),
        Err(error) if inaccessible_process(&error) => {}
        Err(error) => return Err(unreadable(error)),
    }
    let threads = match std::fs::read_dir(process.join("task")) {
        Ok(threads) => threads,
        Err(error) if inaccessible_process(&error) => return Ok(None),
        Err(error) => return Err(unreadable(error)),
    };
    for thread in threads {
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) if inaccessible_process(&error) => continue,
            Err(error) => return Err(unreadable(error)),
        };
        match std::fs::read_link(thread.path().join("exe")) {
            Ok(executable) => return Ok(Some(executable)),
            Err(error) if inaccessible_process(&error) => {}
            Err(error) => return Err(unreadable(error)),
        }
    }
    Ok(None)
}

fn process_arguments_after_settle(path: &Path) -> Result<Threads, String> {
    // A leader may show no argv while a live sibling publishes them. Repeat
    // the complete thread-aware observation before excluding this process.
    match process_arguments(path)? {
        Threads::Unsettled => Ok(Threads::Ended),
        observation => Ok(observation),
    }
}

fn inaccessible_process(error: &io::Error) -> bool {
    gone(error) || matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM))
}

/// A process or thread no longer found, or a thread whose id an `execve`
/// from another of its threads took.
fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

/// What the threads of a process showing no arguments say about it.
enum Threads {
    /// None can run again, so the process can write nothing.
    Ended,
    /// A live thread shows the arguments of the memory the threads share,
    /// which a group leader that exited early no longer shows.
    Arguments(Vec<u8>),
    /// One may still run but shows no arguments yet, as during `execve`.
    Unsettled,
}

/// One observation of a process that may be publishing or releasing its memory.
/// Absence of arguments alone does not establish that it has ended.
fn process_arguments(process: &Path) -> Result<Threads, String> {
    match std::fs::read(process.join("cmdline")) {
        Ok(argv) if argv.is_empty() => threads(process),
        Ok(argv) => Ok(Threads::Arguments(argv)),
        Err(error) if inaccessible_process(&error) => Ok(Threads::Ended),
        Err(error) => Err(format!("Cannot inspect process arguments: {error}")),
    }
}

/// Read again only when a runnable process has not published its arguments.
/// The caller owns the wait budget; exhausting it cannot establish absence.
fn settled_process_arguments(
    mut observe: impl FnMut() -> Result<Threads, String>,
    mut wait: impl FnMut() -> Result<(), String>,
) -> Result<Option<Vec<u8>>, String> {
    loop {
        match observe()? {
            Threads::Ended => return Ok(None),
            Threads::Arguments(argv) => return Ok(Some(argv)),
            Threads::Unsettled => wait()?,
        }
    }
}

/// Reads every thread of a process. It has ended once none can run again:
/// each is a zombie, exiting (`PF_EXITING`) or has `SIGKILL` pending, which a
/// group exit sends every other thread before they run again. The last one
/// out tears down the shared memory, which takes a while for a large process
/// under load. A thread that cannot run cannot create another, so the
/// threads listed again afterwards must all be ones already read. The leader
/// is read last: a thread that runs `execve` takes the leader's id, and stops
/// being found under its own, only once the old leader has gone.
fn threads(process: &Path) -> Result<Threads, String> {
    let unreadable = |error: io::Error| format!("Cannot inspect process threads: {error}");
    let list = || -> Result<Option<Vec<std::ffi::OsString>>, String> {
        let threads = match std::fs::read_dir(process.join("task")) {
            Ok(threads) => threads,
            Err(error) if inaccessible_process(&error) => return Ok(None),
            Err(error) => return Err(unreadable(error)),
        };
        threads
            .map(|thread| thread.map(|thread| thread.file_name()).map_err(unreadable))
            .collect::<Result<_, _>>()
            .map(Some)
    };
    let Some(mut read) = list()? else {
        return Ok(Threads::Ended);
    };
    let leader = process.file_name();
    read.sort_by_key(|thread| Some(thread.as_os_str()) == leader);
    let mut unsettled = false;
    for thread in &read {
        let thread = process.join("task").join(thread);
        if thread_ended(&thread)? {
            continue;
        }
        match std::fs::read(thread.join("cmdline")) {
            Ok(argv) if !argv.is_empty() => return Ok(Threads::Arguments(argv)),
            Ok(_) => unsettled = true,
            Err(error) if inaccessible_process(&error) => unsettled = true,
            Err(error) => return Err(unreadable(error)),
        }
    }
    if unsettled {
        return Ok(Threads::Unsettled);
    }
    match list()? {
        None => return Ok(Threads::Ended),
        Some(listed) if listed.iter().any(|thread| !read.contains(thread)) => {
            return Ok(Threads::Unsettled);
        }
        Some(_) => {}
    }
    // A thread created after the first listing can still have taken the
    // leader's id by `execve` before the second; only the leader's id can
    // change hands, so reading it once more settles that.
    let leader = match leader {
        Some(leader) => process.join("task").join(leader),
        None => return Ok(Threads::Unsettled),
    };
    Ok(if thread_ended(&leader)? {
        Threads::Ended
    } else {
        Threads::Unsettled
    })
}

/// Whether one thread can never run again: reaped, a zombie, exiting
/// (`PF_EXITING`), or with `SIGKILL` pending. A status it cannot read
/// whole says it can.
fn thread_ended(thread: &Path) -> Result<bool, String> {
    let unreadable = |error: io::Error| format!("Cannot inspect process exit state: {error}");
    let stat = match std::fs::read_to_string(thread.join("stat")) {
        Ok(stat) => stat,
        Err(error) if inaccessible_process(&error) => return Ok(true),
        Err(error) => return Err(unreadable(error)),
    };
    // Fields after the command name begin at state (3); flags is field 9.
    let fields = stat
        .rsplit_once(')')
        .map(|(_, fields)| fields.split_whitespace().collect::<Vec<_>>())
        .unwrap_or_default();
    let Some(flags) = fields.get(6).and_then(|flags| flags.parse::<u64>().ok()) else {
        return Ok(false);
    };
    if matches!(fields[0], "Z" | "X") || flags & PF_EXITING != 0 {
        return Ok(true);
    }
    let status = match std::fs::read_to_string(thread.join("status")) {
        Ok(status) => status,
        Err(error) if inaccessible_process(&error) => return Ok(true),
        Err(error) => return Err(unreadable(error)),
    };
    Ok(status
        .lines()
        .filter_map(|line| {
            line.strip_prefix("SigPnd:")
                .or_else(|| line.strip_prefix("ShdPnd:"))
        })
        .any(|mask| {
            u64::from_str_radix(mask.trim(), 16)
                .is_ok_and(|mask| mask & 1 << (libc::SIGKILL - 1) != 0)
        }))
}

/// How long one process may take to publish its arguments after `execve`.
const EXEC_SETTLE: Duration = Duration::from_millis(100);

/// `PF_EXITING`, set once a thread enters `do_exit`.
const PF_EXITING: u64 = 0x4;

/// The census owners whose retirement the store records, read like the census.
fn read_retired_owners(path: &Path) -> Result<Vec<crate::daemon::holder_registry::Record>, String> {
    use rusqlite::{Connection, OpenFlags, config::DbConfig};
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| error.to_string())?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .map_err(|error| error.to_string())?;
    db.busy_timeout(Duration::from_secs(2))
        .map_err(|error| error.to_string())?;
    let mut statement = db
        .prepare(
            "SELECT o.ownership FROM native_actor_ownerships o \
             JOIN native_actor_retirements r ON r.actor = o.actor ORDER BY o.actor LIMIT ?1",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([MAX_CENSUS_ROWS as i64 + 1], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    let mut retired = Vec::new();
    for (index, row) in rows.enumerate() {
        if index >= MAX_CENSUS_ROWS {
            return Err("The retained actor census exceeds its inspection bound".into());
        }
        let ownership: crate::host::model::NativeActorOwnership =
            serde_json::from_str(&row.map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        retired.push(ownership.owner().clone());
    }
    Ok(retired)
}

/// One read transaction over the census, opened read-only: no schema
/// admission, migration, checkpoint or lock beyond SQLite's shared read.
fn read_census(path: &Path) -> Result<Census, String> {
    use rusqlite::{Connection, OpenFlags, config::DbConfig};
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| error.to_string())?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .map_err(|error| error.to_string())?;
    db.busy_timeout(Duration::from_secs(2))
        .map_err(|error| error.to_string())?;
    let mut statement = db
        .prepare("SELECT ownership FROM native_actor_ownerships ORDER BY actor LIMIT ?1")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([MAX_CENSUS_ROWS as i64 + 1], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    let mut census = Vec::new();
    for (index, row) in rows.enumerate() {
        if index >= MAX_CENSUS_ROWS {
            return Err("The retained actor census exceeds its inspection bound".into());
        }
        let ownership: crate::host::model::NativeActorOwnership =
            serde_json::from_str(&row.map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        let owner = ownership.owner().clone();
        if ownership.source().id != owner.terminal {
            census.push((ownership.source().id, owner.clone()));
        }
        census.push((owner.terminal, owner));
    }
    Ok(census)
}

/// A live Codex conversation can outlast more daemon generations than its
/// source keeps upgrade aliases for, so a pane its window never reattached
/// still holds an older issued proof that the source no longer resolves. When
/// the source lists that conversation under its current proof and every other
/// issued field is identical, including the listener its original allocator
/// issued, the pane names exactly that conversation. The conversation migrates
/// as the source's live work and is qualified as such above; the update does
/// not change whether this pane can reattach, which the source already refuses.
fn relisted_codex_conversation(
    saved: &Session,
    inventory: &[Session],
    proof: &HostSessionProof,
    require_resumable: bool,
) -> bool {
    if !saved.owns_codex_listener()
        || !saved.has_valid_native_continuity()
        || saved
            .proof
            .as_ref()
            .is_none_or(|issued| !issued.is_complete() || issued == proof)
    {
        return false;
    }
    let mut current = saved.clone();
    current.proof = Some(proof.clone());
    let mut listed = inventory.iter().filter(|live| live.id == saved.id);
    match (listed.next(), listed.next()) {
        (Some(live), None) => {
            handoff::issued_chat_matches(live, &current)
                && validate_live_chat(live, require_resumable).is_ok()
        }
        _ => false,
    }
}

/// Classify again against the final roster: a pane adopted since the first
/// look is held and qualifies like any live chat, and an owner that started
/// since then is no longer Ended. Only a pane the source does not list and
/// whose execution has ended is exempt from exact qualification.
fn provider_name(provider: crate::agent::Provider) -> &'static str {
    match provider {
        crate::agent::Provider::Claude => "Claude",
        crate::agent::Provider::Codex => "Codex",
        crate::agent::Provider::Omp => "OMP",
    }
}

fn saved_chat_problem(
    source: &Session,
    inventory: &[Session],
    evidence: &ExecutionEvidence,
    qualify: impl FnOnce() -> Result<(), String>,
) -> Option<(String, &'static str)> {
    let execution =
        (!inventory.iter().any(|live| live.id == source.id)).then(|| evidence.classify(source));
    if execution == Some(SavedExecution::Ended) {
        return None;
    }
    if execution == Some(SavedExecution::Unrecorded) {
        return Some((
            "ended before it recorded its conversation".into(),
            "Choose Resume conversation in the new tab (+) menu to find a saved conversation, or start a new conversation",
        ));
    }
    qualify().err().map(|_| {
        if execution == Some(SavedExecution::Running) {
            let pid = evidence.running_pid(source);
            (
                match pid {
                    Some(pid) => format!(
                        "{} is still running outside cagents (pid {pid})",
                        provider_name(source.launch.provider)
                    ),
                    None => format!(
                        "{} is still running outside cagents",
                        provider_name(source.launch.provider)
                    ),
                },
                "Stop that process, then choose Resume conversation in the new tab (+) menu",
            )
        } else if execution == Some(SavedExecution::Unchecked) {
            (UNCHECKED.into(), CHAT_RECOVERY)
        } else {
            (
                "its conversation could not be confirmed".into(),
                CHAT_RECOVERY,
            )
        }
    })
}

fn check_deadline(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err(
            "Source pane inspection exceeded its bounded budget; source windows remain usable"
                .into(),
        );
    }
    Ok(())
}

/// A console's issued recipe is not permission to replay a different installed
/// CLI. Even an unchanged path cannot prove that its existing ControlClient
/// survives the source daemon stopping. Defer until an exact, typed migration
/// route qualifies both the accepted console and its retained provider.
fn qualify_console_migration(
    terminal: &crate::model::TerminalSession,
    matched_cli: &Path,
) -> Result<(), &'static str> {
    if crate::agent_execution::console_agent(&terminal.launch).is_none() {
        return Ok(());
    }
    if Path::new(&terminal.launch.program) != matched_cli {
        return Err(
            "its accepted console belongs to a different matched CLI installation; this pairing cannot migrate yet",
        );
    }
    Err(
        "its accepted console has no qualified reconnection across daemon replacement; this pairing cannot migrate yet",
    )
}

// A foreground process hint is an observation of the retained PTY, not an
// issued launch or a choice of provider/thread to resume. Match the exact
// complete proof and all immutable fields across Live/Detached presentation.
fn issued_terminal_matches(
    authoritative: &crate::model::TerminalSession,
    saved: &crate::model::TerminalSession,
) -> bool {
    let Some(proof) = authoritative
        .continuity
        .live_proof()
        .filter(|proof| proof.is_complete())
    else {
        return false;
    };
    if saved.continuity.live_proof().as_ref() != Some(&proof) {
        return false;
    }
    let mut normalized = authoritative.clone();
    normalized.continuity = saved.continuity.clone();
    normalized.resume_hint = saved.resume_hint.clone();
    normalized == *saved
}

fn qualify_terminal(
    saved: &crate::model::TerminalSession,
    terminals: &[v77::Terminal],
    proof: &HostSessionProof,
) -> Result<(), String> {
    let old = saved
        .continuity
        .live_proof()
        .filter(|proof| proof.is_complete())
        .ok_or("Saved terminal has no complete source ownership")?;
    let matches = terminals
        .iter()
        .filter(|terminal| {
            if !terminal.session.continuity.can_resume_live(proof) {
                return false;
            }
            if &old == proof {
                issued_terminal_matches(&terminal.session, saved)
            } else {
                old.boot_id == proof.boot_id
                    && terminal
                        .upgrade_sources
                        .iter()
                        .any(|source| issued_terminal_matches(source, saved))
            }
        })
        .count();
    if matches != 1 {
        return Err("Saved terminal source is absent or ambiguous".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        chat::{Launch, NativeContinuity},
        model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe, TerminalId},
    };

    #[test]
    fn inspector_and_lifecycle_admit_exactly_the_same_source_protocols() {
        let deadline = Instant::now() + Duration::from_secs(5);
        for protocol in 0..=u16::MAX {
            let request = Request {
                protocol,
                proof: None,
                require_resumable: true,
                reconcile_retained: false,
            };
            // Qualified sources reach the ownership check. Unqualified sources
            // are refused before reading any service namespace or opening a socket.
            let error = connect(&request, deadline).err().unwrap();
            assert_eq!(
                error == "Saved conversations have no authenticated source service",
                source::is_qualified_source(protocol),
                "protocol {protocol}: {error}"
            );
            assert_eq!(
                current::supports_protocol(protocol),
                source::client_family(protocol) == Some(ClientFamily::Current),
                "protocol {protocol}"
            );
        }
        assert!(current::supports_protocol(156));
        assert_eq!(
            source::protocols().collect::<Vec<_>>(),
            [68, 77, 128, 156, crate::terminal_ipc::PROTOCOL_VERSION]
        );
    }

    #[test]
    fn cold_terminal_history_never_requires_replaying_a_launch_or_inventing_an_exit() {
        let mut source = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            TerminalId::new(),
            session().launch.recipe,
            None,
        )
        .session;
        source.launch.working_directory = "/deleted-worktree".into();
        source.resume_hint = Some("historical-provider-observation".into());
        let original = source.clone();
        assert!(!terminal_requires_migration(
            &source,
            &BootId::new("current")
        ));
        assert_eq!(source, original);
        source.continuity = crate::model::TerminalContinuity::Live {
            host_session_id: HostSessionId::new("original"),
            boot_id: BootId::new("previous"),
            host_incarnation: HostIncarnation::new("original"),
        };
        assert!(!terminal_requires_migration(
            &source,
            &BootId::new("current")
        ));
        assert!(
            terminal_requires_migration(&source, &BootId::new("previous")),
            "missing cwd and inventory are not evidence that a same-boot owner died"
        );
        source.continuity = crate::model::TerminalContinuity::Live {
            host_session_id: HostSessionId::new(""),
            boot_id: BootId::new("previous"),
            host_incarnation: HostIncarnation::new("original"),
        };
        assert!(
            terminal_requires_migration(&source, &BootId::new("current")),
            "incomplete proof cannot classify obsolete execution"
        );
    }

    #[test]
    fn drafts_and_inactive_panes_are_counted_without_starting_them() {
        let mut draft = session();
        draft.launch.session_id = None;
        draft.proof = None;
        draft.terminal = None;
        draft.owned_codex_listener = None;
        draft.native_continuity = NativeContinuity::ReconnectOnly;
        assert!(draft.is_unstarted());
        let inactive = session();
        let mut inspection = SourceInspection::default();
        for (source, inert) in [(&draft, false), (&inactive, true)] {
            record_saved_pane(
                &mut inspection,
                &CardKind::Chat(source.clone()),
                inert,
                &current_boot(),
                true,
            );
            assert_eq!(
                inspection.details.panes[&source.id].outcome,
                SavedOutcome::Unchanged
            );
        }
        assert_eq!(inspection.details.panes.len(), 2);
        assert!(inspection.stopped.is_empty());
    }

    #[test]
    fn compatible_updates_count_live_chats_and_terminals_including_relisted_panes() {
        let source = session();
        let proof = source.proof.clone().unwrap();
        let recipe = crate::model::LaunchRecipe {
            program: "bash".into(),
            arguments: vec![],
            working_directory: "/alpha".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let mut terminal =
            crate::terminal_ipc::TerminalDescriptor::relaunchable(TerminalId::new(), recipe, None);
        terminal.session.continuity = crate::model::TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        let mut inspection = SourceInspection::default();
        record_saved_pane(
            &mut inspection,
            &CardKind::Chat(source.clone()),
            true,
            &proof.boot_id,
            false,
        );
        assert_eq!(
            inspection.details.panes[&source.id].outcome,
            SavedOutcome::Unchanged
        );
        record_live_panes(
            &mut inspection,
            &[source.clone()],
            &[terminal.into()],
            &proof,
            false,
        );
        assert_eq!(inspection.details.panes.len(), 2);
        assert!(
            inspection
                .details
                .panes
                .values()
                .all(|pane| pane.outcome == SavedOutcome::KeepsRunning)
        );
    }

    #[test]
    fn live_inspection_failure_keeps_one_readable_reason() {
        let mut inspection = SourceInspection::default();
        inspection_failure(&mut inspection, "Cannot connect to the source service");
        assert_eq!(
            inspection.details.notices,
            [
                "Live panes could not all be checked: the running terminal service could not be reached."
            ]
        );
        assert!(inspection.stopped.is_empty());
    }

    #[test]
    fn large_workspaces_report_every_stopped_pane() {
        let mut inspection = SourceInspection::default();
        for _ in 0..65 {
            inspection.stop(
                TerminalId::new(),
                StoppedKind::Chat,
                "its conversation could not be confirmed",
                CHAT_RECOVERY,
            );
        }
        assert_eq!(inspection.stopped.len(), 65);
        assert!(
            inspection
                .stopped
                .iter()
                .all(|pane| pane.reason == "its conversation could not be confirmed")
        );
    }

    #[test]
    fn accepted_console_migration_defers_even_when_the_cli_path_is_unchanged() {
        let matched = Path::new("/installed/new/bin/cagents");
        let recipe = crate::model::LaunchRecipe {
            program: matched.to_string_lossy().into_owned(),
            arguments: vec![
                "agent".into(),
                "console".into(),
                crate::model::AgentId::new().to_string(),
            ],
            working_directory: "/private-directory".into(),
            environment: [("PRIVATE_ACCOUNT".into(), "never-print-this".into())].into(),
            unset_environment: Default::default(),
        };
        let mut terminal =
            crate::terminal_ipc::TerminalDescriptor::relaunchable(TerminalId::new(), recipe, None)
                .session;
        let same_path = qualify_console_migration(&terminal, matched).unwrap_err();
        assert!(same_path.contains("no qualified reconnection"));
        terminal.launch.program = "/installed/old/bin/cagents".into();
        let older = qualify_console_migration(&terminal, matched).unwrap_err();
        assert!(older.contains("different matched CLI installation"));
        for error in [same_path, older] {
            let mut inspection = SourceInspection::default();
            console_keeps_running(
                &mut inspection,
                terminal.id,
                crate::update::impact::SessionKind::Terminal,
            );
            assert!(inspection.stopped.is_empty());
            let pane = &inspection.details.panes[&terminal.id];
            assert_eq!(pane.outcome, SavedOutcome::KeepsRunning);
            assert_eq!(
                pane.detail,
                "keeps running in its terminal; its chat panel disconnects. Choose Resume conversation in the new tab (+) menu afterwards."
            );
            assert!(!error.contains("private-directory"));
            assert!(!error.contains("never-print-this"));
            assert!(!error.contains("/installed"));
        }
        terminal.launch.arguments = vec!["--login".into()];
        assert!(qualify_console_migration(&terminal, matched).is_ok());
        terminal.launch.arguments = vec![
            "--remote".into(),
            "unix://owned-listener".into(),
            "resume".into(),
            "exact-thread".into(),
        ];
        assert!(qualify_console_migration(&terminal, matched).is_ok());
    }

    pub(super) fn session() -> Session {
        Session {
            id: TerminalId::new(),
            launch: Launch {
                provider: Provider::Claude,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: vec![],
                    working_directory: "/tmp".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some("11111111-1111-4111-8111-111111111111".into()),
            },
            proof: Some(HostSessionProof {
                host_session_id: HostSessionId::new("source"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("actor"),
            }),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: NativeContinuity::ReconnectOnly,
        }
    }

    fn retained_card(boot: BootId) -> Session {
        let mut card = session();
        card.native_continuity = NativeContinuity::RetainedClaudeActor { protocol: 2 };
        card.proof.as_mut().unwrap().boot_id = boot;
        card
    }

    fn current_boot() -> BootId {
        crate::daemon::read_boot_id().unwrap()
    }

    fn running_process() -> crate::daemon::holder_registry::Process {
        crate::daemon::holder_registry::Process::read(std::process::id() as i32).unwrap()
    }

    /// The exact identity of a reaped child that led no process group or
    /// session: an incarnation that no longer exists and left nothing behind.
    fn ended_process() -> crate::daemon::holder_registry::Process {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let identity = crate::daemon::holder_registry::Process::read(child.id() as i32).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        identity
    }

    /// The helper identity a retained actor registers and publishes.
    fn owner(
        card: &Session,
        holder: crate::daemon::holder_registry::Process,
        root: crate::daemon::holder_registry::Process,
    ) -> crate::daemon::holder_registry::Record {
        use crate::daemon::holder_registry::{BuildIdentity, Kind, Record};
        Record {
            version: 4,
            build: Some(BuildIdentity::current(Kind::Native)),
            kind: Kind::Native,
            application: crate::instance::APPLICATION_ID.into(),
            boot: current_boot(),
            terminal: card.id,
            holder,
            root,
        }
    }

    fn evidence(
        holders: &Path,
        census: Vec<(TerminalId, crate::daemon::holder_registry::Record)>,
    ) -> ExecutionEvidence {
        ExecutionEvidence {
            boot: current_boot(),
            native_holders: Ok(holders.to_owned()),
            census: Ok(census),
            retired: Ok(Vec::new()),
            processes: "/proc".into(),
            exec_settle: EXEC_SETTLE,
            deadline: None,
        }
    }

    /// Real kernel process and socket data, restricted to this fixture's
    /// explicitly owned processes. An unrelated exec is not part of its input.
    struct LiveProcesses(tempfile::TempDir);

    impl LiveProcesses {
        fn new() -> Self {
            let processes = Self(tempfile::tempdir_in("/var/tmp").unwrap());
            std::fs::create_dir(processes.0.path().join("net")).unwrap();
            std::os::unix::fs::symlink("/proc/net/unix", processes.0.path().join("net/unix"))
                .unwrap();
            processes.include(std::process::id());
            processes
        }

        fn include(&self, pid: u32) {
            std::os::unix::fs::symlink(format!("/proc/{pid}"), self.0.path().join(pid.to_string()))
                .unwrap();
        }

        fn remove(&self, pid: u32) {
            std::fs::remove_file(self.0.path().join(pid.to_string())).unwrap();
        }

        fn evidence(&self, holders: &Path) -> ExecutionEvidence {
            ExecutionEvidence {
                processes: self.0.path().into(),
                ..evidence(holders, Vec::new())
            }
        }
    }

    /// A Codex pane served on its own daemon-allocated listener under `root`.
    fn codex_card(root: &Path, continuity: NativeContinuity) -> Session {
        let mut card = session();
        card.launch.provider = Provider::Codex;
        card.launch.recipe.program = "codex".into();
        card.native_continuity = continuity;
        card.proof.as_mut().unwrap().boot_id = current_boot();
        let socket = root
            .join(format!("cagents-codex-{}", card.id))
            .join("control.sock");
        card.launch.recipe.arguments =
            vec!["--remote".into(), format!("unix://{}", socket.display())];
        card.owned_codex_listener = Some(crate::chat::OwnedCodexListener {
            socket,
            allocator: card.proof.clone().unwrap(),
        });
        assert!(card.owns_codex_listener() && card.has_valid_native_continuity());
        card
    }

    fn codex_cards(root: &Path) -> [Session; 2] {
        [
            codex_card(root, NativeContinuity::ReconnectOnly),
            codex_card(
                root,
                NativeContinuity::Durable {
                    provider_version: "0.157.1".into(),
                },
            ),
        ]
    }

    #[test]
    fn verified_retired_pane_read_from_the_real_census_has_nothing_to_preserve() {
        use crate::account::scope::LaunchAccountScope;
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut issued = retained_card(current_boot());
        issued.launch.session_id = None;
        issued.launch.recipe.working_directory = root.path().into();
        let mut card = issued.clone();
        card.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        let retired = owner(&card, ended_process(), ended_process());
        let metadata = std::fs::metadata(root.path()).unwrap();
        let scope = LaunchAccountScope {
            provider: Provider::Claude,
            canonical_home: root.path().canonicalize().unwrap(),
            device: metadata.dev(),
            inode: metadata.ino(),
            registered_id: None,
            purge_ids: vec![],
        };
        let path = root.path().join("registry.sqlite3");
        let actor = uuid::Uuid::new_v4();
        let mut registry = crate::host::model::Registry::at(&path).unwrap();
        registry
            .register_native_actor_ownership(actor, &retired, &issued, Ok(&scope))
            .unwrap();
        registry
            .commit_native_actor_retirement(
                actor,
                &retired,
                &card,
                Ok(&scope),
                card.launch.session_id.as_deref(),
            )
            .unwrap();
        // The census is read while its writer still holds the store open.
        let census = read_census(&path).unwrap();
        assert_eq!(census, vec![(card.id, retired.clone())]);
        // Verified retirement removed the discovery record.
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut evidence = evidence(holders.path(), census);
        assert_eq!(evidence.classify(&card), SavedExecution::Ended);
        assert_eq!(
            evidence.classify(&issued),
            SavedExecution::Unrecorded,
            "a pre-ID card reopens only what a recorded retirement confirmed"
        );
        evidence.retired = read_retired_owners(&path);
        assert_eq!(evidence.retired, Ok(vec![retired.clone()]));
        assert_eq!(
            evidence.classify(&issued),
            SavedExecution::Ended,
            "a pre-ID card whose actor retired holds no live work either"
        );
        drop(registry);
        assert!(read_census(&root.path().join("absent.sqlite3")).is_err());
        assert!(
            !root.path().join("absent.sqlite3").exists(),
            "inspection never creates a store"
        );
    }

    #[test]
    fn previous_boot_pane_has_nothing_to_preserve_without_any_census() {
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut evidence = evidence(holders.path(), Vec::new());
        evidence.census = Err("unreadable".into());
        let mut card = retained_card(BootId::new("previous-boot"));
        assert_eq!(evidence.classify(&card), SavedExecution::Ended);
        card.native_continuity = NativeContinuity::ReconnectOnly;
        card.launch.provider = Provider::Codex;
        assert_eq!(
            evidence.classify(&card),
            SavedExecution::Ended,
            "a kernel boot change ends every provider"
        );
    }

    #[test]
    fn abandoned_actor_is_ended_only_when_no_owner_process_survives() {
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let card = retained_card(current_boot());
        let abandoned = owner(&card, ended_process(), ended_process());
        // A stale discovery record of the same ended helper.
        abandoned.create(holders.path()).unwrap();
        let mut older_boot = abandoned.clone();
        older_boot.boot = BootId::new("previous-boot");
        older_boot.holder = running_process();
        let census = vec![(card.id, abandoned.clone()), (card.id, older_boot)];
        assert_eq!(
            evidence(holders.path(), census).classify(&card),
            SavedExecution::Ended
        );
        for (holder, root) in [
            (running_process(), ended_process()),
            (ended_process(), running_process()),
        ] {
            let card = retained_card(current_boot());
            let census = vec![(card.id, owner(&card, holder, root))];
            assert_eq!(
                evidence(holders.path(), census).classify(&card),
                SavedExecution::Running,
                "a surviving helper or provider is live work"
            );
        }
    }

    #[test]
    fn a_helper_not_yet_in_the_census_is_live_work() {
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let card = retained_card(current_boot());
        // An earlier actor ended; a restart published its record but has not
        // answered the census registration yet.
        owner(&card, running_process(), running_process())
            .create(holders.path())
            .unwrap();
        let census = vec![(card.id, owner(&card, ended_process(), ended_process()))];
        assert_eq!(
            evidence(holders.path(), census).classify(&card),
            SavedExecution::Running
        );
    }

    #[test]
    fn same_boot_claude_execution_without_complete_evidence_is_unproved() {
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let card = retained_card(current_boot());
        let ended = vec![(card.id, owner(&card, ended_process(), ended_process()))];
        assert_eq!(
            evidence(holders.path(), Vec::new()).classify(&card),
            SavedExecution::Unproved,
            "no registered owner is no evidence that one ended"
        );
        let mut paired = card.clone();
        paired.terminal = Some(
            crate::terminal_ipc::TerminalDescriptor::relaunchable(
                paired.id,
                paired.launch.recipe.clone(),
                None,
            )
            .session,
        );
        let mut unowned = card.clone();
        unowned.proof = None;
        let mut reconnect_only = card.clone();
        reconnect_only.native_continuity = NativeContinuity::ReconnectOnly;
        let mut codex = reconnect_only.clone();
        codex.launch.provider = Provider::Codex;
        assert_eq!(
            LiveProcesses::new()
                .evidence(holders.path())
                .classify(&codex),
            SavedExecution::Ended,
            "Codex uses process and socket evidence, not the Claude census"
        );
        for card in [paired, unowned, reconnect_only] {
            assert_eq!(
                evidence(holders.path(), ended.clone()).classify(&card),
                SavedExecution::Unproved
            );
        }
        let mut unreadable = evidence(holders.path(), ended.clone());
        unreadable.census = Err("unreadable".into());
        assert_eq!(unreadable.classify(&card), SavedExecution::Unchecked);
        // Discovery evidence that cannot be authenticated is never absence.
        std::fs::write(
            crate::daemon::holder_registry::socket(holders.path(), card.id).with_extension("json"),
            b"{\"not\":\"a record\"}",
        )
        .unwrap();
        assert_eq!(
            evidence(holders.path(), ended).classify(&card),
            SavedExecution::Unchecked
        );
    }

    #[test]
    fn codex_pane_whose_server_is_gone_has_nothing_to_preserve() {
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let live_processes = LiveProcesses::new();
        for card in codex_cards(root.path()) {
            assert_eq!(
                live_processes.evidence(holders.path()).classify(&card),
                SavedExecution::Ended,
                "{:?}",
                card.native_continuity
            );
            // A stale record of a holder that ended with its server.
            owner(&card, ended_process(), ended_process())
                .create(holders.path())
                .unwrap();
            assert_eq!(
                live_processes.evidence(holders.path()).classify(&card),
                SavedExecution::Ended
            );
            // A prior-boot record cannot name a surviving holder, even when
            // its PID and start time happen to match a process of this boot.
            let mut older = owner(&card, running_process(), running_process());
            older.boot = BootId::new("previous-boot");
            let holders = tempfile::tempdir_in("/var/tmp").unwrap();
            older.create(holders.path()).unwrap();
            assert_eq!(
                live_processes.evidence(holders.path()).classify(&card),
                SavedExecution::Ended
            );
        }
    }

    /// Keep whole-host kernel coverage alongside the controlled state fixture.
    /// An unresolved exec is explicit uncertainty; every other error must stay
    /// visible instead of being collapsed into SavedExecution::Unproved.
    #[test]
    fn real_exec_churn_reports_only_explicit_argument_settlement_uncertainty() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let [card, _] = codex_cards(root.path());
        let listener = &card.owned_codex_listener.as_ref().unwrap().socket;
        let control = crate::daemon::holder_registry::socket(holders.path(), card.id);
        let stop = Arc::new(AtomicBool::new(false));
        let (started, ready) = std::sync::mpsc::sync_channel(0);
        let churn = {
            let stop = stop.clone();
            std::thread::spawn(move || -> io::Result<usize> {
                let mut spawned = 0;
                while !stop.load(Ordering::Relaxed) {
                    let status = std::process::Command::new("/usr/bin/true").status()?;
                    if !status.success() {
                        return Err(io::Error::other(format!("exec churn failed: {status}")));
                    }
                    spawned += 1;
                    if spawned == 1 {
                        started.send(()).map_err(io::Error::other)?;
                    }
                }
                Ok(spawned)
            })
        };
        if let Err(error) = ready.recv() {
            let result = churn.join().unwrap();
            panic!("exec churn did not start: {error}; {result:?}");
        }
        let mut absent = 0;
        let mut unpublished = 0;
        let mut unexpected = std::collections::BTreeMap::<String, usize>::new();
        for _ in 0..300 {
            match codex_listener_survives(
                Path::new("/proc"),
                card.id,
                card.launch.session_id.as_deref(),
                &[listener.as_path(), control.as_path()],
                Duration::from_secs(2),
                None,
            ) {
                Ok(CodexExecution::Ended) => absent += 1,
                Err(error) if error == "A process has not published its arguments yet" => {
                    unpublished += 1;
                }
                result => *unexpected.entry(format!("{result:?}")).or_default() += 1,
            }
        }
        stop.store(true, Ordering::Relaxed);
        let spawned = churn.join().unwrap().unwrap();
        eprintln!(
            "CODEX_KERNEL_CENSUS scans=300 absent={absent} unpublished={unpublished} spawned={spawned} unexpected={unexpected:?}"
        );
        assert!(spawned > 0, "the kernel exec interleaving was exercised");
        assert!(
            unexpected.is_empty(),
            "a whole-host inspection may only report absence or the exact unpublished-arguments condition: {unexpected:?}"
        );
        assert_eq!(absent + unpublished, 300);
    }

    #[test]
    fn process_publication_and_exit_preserve_exact_codex_execution_evidence() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let processes = tempfile::tempdir_in("/var/tmp").unwrap();
        std::fs::create_dir(processes.path().join("net")).unwrap();
        std::fs::write(
            processes.path().join("net/unix"),
            "Num       RefCount Protocol Flags    Type St Inode Path\n",
        )
        .unwrap();
        let process = processes.path().join("4242");
        let thread = process.join("task/4242");
        std::fs::create_dir_all(&thread).unwrap();
        let uid = crate::daemon::current_uid();
        std::fs::write(
            process.join("status"),
            format!("Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
        std::os::unix::fs::symlink("/usr/bin/codex", process.join("exe")).unwrap();
        std::fs::write(process.join("cmdline"), b"").unwrap();
        std::fs::write(thread.join("cmdline"), b"").unwrap();
        std::fs::write(
            thread.join("stat"),
            "4242 (true) R 1 4242 4242 0 0 0 0 0 0 0 0 0 0 0 20 0 1 0 1",
        )
        .unwrap();
        std::fs::write(
            thread.join("status"),
            "SigPnd:\t0000000000000000\nShdPnd:\t0000000000000000\n",
        )
        .unwrap();
        let [card, _] = codex_cards(root.path());
        let evidence = ExecutionEvidence {
            processes: processes.path().into(),
            // A runnable process is allowed to stay between exec and argument
            // publication indefinitely. Exercise the exhausted budget directly,
            // without assuming anything about how soon the host schedules it.
            exec_settle: Duration::ZERO,
            ..evidence(holders.path(), Vec::new())
        };
        assert!(matches!(
            process_arguments(&process).unwrap(),
            Threads::Unsettled
        ));
        assert_eq!(evidence.classify(&card), SavedExecution::Unchecked);

        // Publish an unrelated process's actual arguments. The exact Ended
        // assertion must hold once that process no longer needs settlement.
        let unrelated = b"true\0";
        let mut waits = 0;
        let argv = settled_process_arguments(
            || process_arguments(&process),
            || {
                assert_eq!(
                    waits, 0,
                    "publication needs exactly one further observation"
                );
                waits += 1;
                std::fs::write(process.join("cmdline"), unrelated).unwrap();
                Ok(())
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(waits, 1);
        assert_eq!(argv, unrelated);
        let Threads::Arguments(argv) = process_arguments(&process).unwrap() else {
            panic!("published arguments were not observed");
        };
        assert_eq!(argv, unrelated);
        assert_eq!(evidence.classify(&card), SavedExecution::Ended);

        let listener = &card.owned_codex_listener.as_ref().unwrap().socket;
        let server = format!(
            "codex\0app-server\0--listen\0unix://{}\0",
            listener.display()
        );
        std::fs::write(process.join("cmdline"), &server).unwrap();
        assert_eq!(evidence.classify(&card), SavedExecution::Running);

        // A leader that released its memory can still have a live thread.
        // Publication through that thread provides the same exact evidence.
        std::fs::write(process.join("cmdline"), b"").unwrap();
        std::fs::write(
            thread.join("stat"),
            "4242 (true) Z 1 4242 4242 0 0 4 0 0 0 0 0 0 0 0 20 0 1 0 1",
        )
        .unwrap();
        let worker = process.join("task/4243");
        std::fs::create_dir(&worker).unwrap();
        // A zombie leader also stops exposing exe. A live sibling retains
        // both the executable and argv needed to identify the writer.
        std::fs::remove_file(process.join("exe")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/codex", worker.join("exe")).unwrap();
        std::fs::write(
            worker.join("stat"),
            "4243 (true) R 1 4242 4242 0 0 0 0 0 0 0 0 0 0 0 20 0 1 0 1",
        )
        .unwrap();
        std::fs::copy(thread.join("status"), worker.join("status")).unwrap();
        std::fs::write(worker.join("cmdline"), unrelated).unwrap();
        assert_eq!(evidence.classify(&card), SavedExecution::Ended);
        std::fs::write(worker.join("cmdline"), &server).unwrap();
        assert_eq!(evidence.classify(&card), SavedExecution::Running);

        // Exit makes an empty argument list conclusive, unlike the live exec.
        std::fs::remove_dir_all(worker).unwrap();
        assert!(matches!(
            process_arguments(&process).unwrap(),
            Threads::Ended
        ));
        assert_eq!(evidence.classify(&card), SavedExecution::Ended);
        std::fs::remove_dir_all(&process).unwrap();
        assert!(matches!(
            process_arguments(&process).unwrap(),
            Threads::Ended
        ));
        assert_eq!(evidence.classify(&card), SavedExecution::Ended);
    }

    #[test]
    fn codex_pane_with_a_surviving_server_or_holder_is_live_work() {
        use std::io::Read;
        // Other tests must not fork while this test owns a listener whose
        // final descriptor close is meant to remove its /proc/net/unix row.
        #[cfg(target_os = "linux")]
        if crate::retained_fixture::run_isolated() {
            return;
        }
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        for card in codex_cards(root.path()) {
            let socket = card.owned_codex_listener.clone().unwrap().socket;
            let holders = tempfile::tempdir_in("/var/tmp").unwrap();
            let live_processes = LiveProcesses::new();
            // A server still naming the pane's listener, as `codex app-server
            // --listen unix://<socket>` does.
            let program = holders.path().join("codex");
            std::fs::copy("/bin/sh", &program).unwrap();
            let mut server = std::process::Command::new(&program)
                .args([
                    "-c",
                    "printf ready; IFS= read -r release",
                    "server",
                    "app-server",
                    "--listen",
                ])
                .arg(format!("unix://{}", socket.display()))
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut ready = [0; 5];
            server
                .stdout
                .as_mut()
                .unwrap()
                .read_exact(&mut ready)
                .unwrap();
            assert_eq!(&ready, b"ready");
            live_processes.include(server.id());
            let named = live_processes.evidence(holders.path()).classify(&card);
            server
                .stdin
                .take()
                .unwrap()
                .write_all(b"release\n")
                .unwrap();
            assert!(server.wait().unwrap().success());
            live_processes.remove(server.id());
            assert_eq!(named, SavedExecution::Running);
            // A server under another name still bound to the listener path.
            std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
            let bound = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            assert_eq!(
                live_processes.evidence(holders.path()).classify(&card),
                SavedExecution::Running
            );
            drop(bound);
            assert_eq!(
                live_processes.evidence(holders.path()).classify(&card),
                SavedExecution::Ended,
                "a socket file nobody listens on is no server"
            );
            std::fs::remove_file(&socket).unwrap();
            // Recent Codex binds in its own runtime directory and publishes
            // the listener path as a symlink to that socket.
            let runtime = tempfile::tempdir_in("/var/tmp").unwrap();
            let target = runtime.path().join("served");
            std::os::unix::fs::symlink(&target, &socket).unwrap();
            let bound = std::os::unix::net::UnixListener::bind(&target).unwrap();
            assert_eq!(
                live_processes.evidence(holders.path()).classify(&card),
                SavedExecution::Running,
                "a server bound behind the published listener"
            );
            drop(bound);
            assert_eq!(
                live_processes.evidence(holders.path()).classify(&card),
                SavedExecution::Ended
            );
            std::fs::remove_dir_all(socket.parent().unwrap()).unwrap();
            for (holder, provider) in [
                (running_process(), ended_process()),
                (ended_process(), running_process()),
            ] {
                let holders = tempfile::tempdir_in("/var/tmp").unwrap();
                owner(&card, holder, provider)
                    .create(holders.path())
                    .unwrap();
                assert_eq!(
                    live_processes.evidence(holders.path()).classify(&card),
                    SavedExecution::Running,
                    "a registered holder or its server survives"
                );
            }
        }
    }

    #[test]
    fn codex_writer_identity_uses_executable_and_recognizes_script_launchers() {
        let marker = b"cagents-codex-pane";
        let thread = "12345678-1234-4234-8234-123456789abc";
        for (exe, argv, expected) in [
            (
                "/usr/bin/ls",
                "codex\0cagents-codex-pane\0".to_owned(),
                false,
            ),
            (
                "/usr/bin/codex (deleted)",
                "custom-name\0app-server\0cagents-codex-pane\0".to_owned(),
                true,
            ),
            (
                "/usr/bin/bash",
                "bash\0-c\0ls /path/codex cagents-codex-pane\0".to_owned(),
                false,
            ),
            (
                "/usr/bin/bash",
                format!("bash\0/path/codex\0resume\0{thread}\0"),
                true,
            ),
            (
                "/usr/bin/node",
                format!("node\0/path/codex.js\0resume\0{thread}\0"),
                true,
            ),
            (
                "/usr/bin/cagents",
                format!("cagents\0--holder\0{{\"threadId\":\"{thread}\"}}\0"),
                true,
            ),
        ] {
            assert_eq!(
                codex_writer_arguments(Path::new(exe), argv.as_bytes(), marker, Some(thread)),
                expected,
                "{exe}: {argv:?}"
            );
        }
    }

    #[test]
    fn codex_supported_script_launcher_is_a_writer_before_a_server_exists() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let [card, _] = codex_cards(root.path());
        let processes = tempfile::tempdir_in("/var/tmp").unwrap();
        std::fs::create_dir(processes.path().join("net")).unwrap();
        std::fs::write(processes.path().join("net/unix"), "header\n").unwrap();
        let process = processes.path().join("4242");
        std::fs::create_dir(&process).unwrap();
        let uid = crate::daemon::current_uid();
        std::fs::write(
            process.join("status"),
            format!("Uid: {uid} {uid} {uid} {uid}\n"),
        )
        .unwrap();
        for interpreter in ["node", "nodejs", "bun"] {
            let executable = process.join("exe");
            std::os::unix::fs::symlink(format!("/usr/bin/{interpreter}"), &executable).unwrap();
            std::fs::write(
                process.join("cmdline"),
                format!(
                    "{interpreter}\0/path/codex.js\0resume\0{}\0",
                    card.launch.session_id.as_deref().unwrap()
                ),
            )
            .unwrap();
            let table = ExecutionEvidence {
                processes: processes.path().into(),
                ..evidence(holders.path(), Vec::new())
            };
            assert!(
                matches!(
                    table.codex_execution(&card),
                    CodexExecution::Running {
                        pid: Some(4242),
                        ..
                    }
                ),
                "{interpreter} launcher alone must fence a replacement"
            );
            std::fs::remove_file(executable).unwrap();
        }
    }

    #[test]
    fn codex_live_holder_without_a_bound_control_retains_processes() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let [card, _] = codex_cards(root.path());
        let record = owner(&card, running_process(), running_process());
        record.create(holders.path()).unwrap();
        let processes = LiveProcesses::new();
        let table = processes.evidence(holders.path());
        assert!(!crate::daemon::holder_registry::socket(holders.path(), card.id).exists());
        assert_eq!(
            table.codex_execution(&card),
            CodexExecution::RetainedProcesses
        );
        record.remove(holders.path());
        assert_eq!(table.codex_execution(&card), CodexExecution::Ended);
    }

    #[test]
    fn codex_long_holder_socket_alias_blocks_until_listener_exits() {
        // A concurrent fork can retain a listener until its child execs. Keep
        // final-close assertions in a child where other tests cannot fork it.
        #[cfg(target_os = "linux")]
        if crate::retained_fixture::run_isolated() {
            return;
        }
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let directory = root.path().join("holders".repeat(20));
        crate::secure_file::prepare_private_directory(&directory).unwrap();
        let pane = TerminalId::new();
        let socket = crate::daemon::holder_registry::socket(&directory, pane);
        assert!(!crate::daemon::holder_registry::socket_fits(&socket));
        let listener = crate::daemon::holder_registry::bind(&socket).unwrap();
        let processes = LiveProcesses::new();
        assert_eq!(
            codex_listener_survives(
                processes.0.path(),
                pane,
                None,
                &[&socket],
                Duration::ZERO,
                None
            )
            .unwrap(),
            CodexExecution::Running {
                pid: None,
                socket: socket.clone()
            }
        );
        drop(listener);
        assert_eq!(
            codex_listener_survives(
                processes.0.path(),
                pane,
                None,
                &[&socket],
                Duration::ZERO,
                None
            )
            .unwrap(),
            CodexExecution::Ended
        );
    }

    #[test]
    fn codex_process_scan_skips_inaccessible_and_unpublished_kernel_processes() {
        for errno in [libc::EACCES, libc::EPERM, libc::ESRCH, libc::ENOENT] {
            assert!(inaccessible_process(&io::Error::from_raw_os_error(errno)));
        }
        assert!(!inaccessible_process(&io::Error::from_raw_os_error(
            libc::EIO
        )));
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let [card, _] = codex_cards(root.path());
        let processes = tempfile::tempdir_in("/var/tmp").unwrap();
        let process = processes.path().join("4242");
        let thread = process.join("task/4242");
        std::fs::create_dir_all(&thread).unwrap();
        std::fs::create_dir(processes.path().join("net")).unwrap();
        std::fs::write(processes.path().join("net/unix"), "header\n").unwrap();
        let uid = crate::daemon::current_uid();
        std::fs::write(
            process.join("status"),
            format!("Uid: {uid} {uid} {uid} {uid}\n"),
        )
        .unwrap();
        std::fs::write(process.join("cmdline"), b"").unwrap();
        std::fs::write(thread.join("cmdline"), b"").unwrap();
        std::fs::write(thread.join("stat"), "4242 (kernel) R 1 4242 4242 0 0 0 0").unwrap();
        std::fs::write(thread.join("status"), "SigPnd: 0\nShdPnd: 0\n").unwrap();
        let table = ExecutionEvidence {
            processes: processes.path().into(),
            exec_settle: Duration::ZERO,
            ..evidence(holders.path(), Vec::new())
        };
        assert_eq!(table.codex_execution(&card), CodexExecution::Ended);
        std::os::unix::fs::symlink("/usr/bin/codex", process.join("exe")).unwrap();
        assert!(
            matches!(table.codex_execution(&card), CodexExecution::Unproved { reason } if reason == "A process has not published its arguments yet")
        );
        let mut stale = card.clone();
        stale.proof.as_mut().unwrap().boot_id = BootId::new("earlier-boot");
        stale
            .owned_codex_listener
            .as_mut()
            .unwrap()
            .allocator
            .boot_id = BootId::new("earlier-boot");
        assert!(
            matches!(table.codex_execution(&stale), CodexExecution::Unproved { reason } if reason == CODEX_PROCESS_STARTING)
        );
        // Permission-denied process entries must not poison the entire census.
        std::fs::set_permissions(&process, std::os::unix::fs::PermissionsExt::from_mode(0))
            .unwrap();
        if crate::daemon::current_uid() != 0 {
            assert_eq!(table.codex_execution(&card), CodexExecution::Ended);
        }
        std::fs::set_permissions(
            &process,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
    }

    #[test]
    fn codex_earlier_boot_descriptors_still_scan_for_current_writers() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let [mut card, _] = codex_cards(root.path());
        let prior_boot = BootId::new("prior-boot");
        card.proof.as_mut().unwrap().boot_id = prior_boot.clone();
        card.owned_codex_listener
            .as_mut()
            .unwrap()
            .allocator
            .boot_id = prior_boot.clone();
        let mut old_holder = owner(&card, running_process(), running_process());
        old_holder.boot = prior_boot.clone();
        old_holder.create(holders.path()).unwrap();
        // Stale descriptors cannot hide an orphaned writer of this boot.
        let processes = tempfile::tempdir_in("/var/tmp").unwrap();
        let process = processes.path().join("4242");
        std::fs::create_dir(&process).unwrap();
        let uid = crate::daemon::current_uid();
        std::fs::write(
            process.join("status"),
            format!("Uid: {uid} {uid} {uid} {uid}\n"),
        )
        .unwrap();
        std::fs::write(
            process.join("cmdline"),
            format!(
                "codex\0app-server\0--listen\0unix://{}\0",
                card.owned_codex_listener.as_ref().unwrap().socket.display()
            ),
        )
        .unwrap();
        std::os::unix::fs::symlink("/usr/bin/codex", process.join("exe")).unwrap();
        let current_writer = ExecutionEvidence {
            processes: processes.path().into(),
            ..evidence(holders.path(), Vec::new())
        };
        assert!(matches!(
            current_writer.codex_execution(&card),
            CodexExecution::Running {
                pid: Some(4242),
                ..
            }
        ));
        // An exhausted inspection budget cannot establish absence, even when
        // every saved descriptor belongs to an earlier boot.
        let expired = ExecutionEvidence {
            deadline: Some(Instant::now()),
            ..current_writer
        };
        assert_eq!(expired.classify(&card), SavedExecution::Unchecked);
        // Earlier boots only settle scans whose kernel evidence is unreadable.
        // Matching PIDs alone cannot revive an earlier holder identity.
        let unreadable_kernel = ExecutionEvidence {
            processes: root.path().join("unavailable-proc"),
            ..evidence(holders.path(), Vec::new())
        };
        assert_eq!(
            unreadable_kernel.codex_execution(&card),
            CodexExecution::Ended
        );
        // The same applies to the original legacy shape: only Session.proof
        // records the boot, and no allocator/native capability ever existed.
        let mut legacy = card.clone();
        legacy.owned_codex_listener = None;
        assert_eq!(
            unreadable_kernel.codex_execution(&legacy),
            CodexExecution::Ended
        );

        // Each independent record can disprove that shortcut. With this boot
        // represented, an unreadable process table must remain an explicit
        // read failure rather than being masked by Session.proof.
        card.owned_codex_listener
            .as_mut()
            .unwrap()
            .allocator
            .boot_id = current_boot();
        assert!(
            matches!(unreadable_kernel.codex_execution(&card), CodexExecution::Unproved { reason } if reason.contains("Cannot enumerate processes"))
        );
        card.owned_codex_listener
            .as_mut()
            .unwrap()
            .allocator
            .boot_id = prior_boot;
        let mut console = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            TerminalId::new(),
            card.launch.recipe.clone(),
            None,
        )
        .session;
        console.continuity = crate::model::TerminalContinuity::Live {
            host_session_id: "console".into(),
            boot_id: current_boot(),
            host_incarnation: "console".into(),
        };
        card.terminal = Some(console);
        assert!(
            matches!(unreadable_kernel.codex_execution(&card), CodexExecution::Unproved { reason } if reason.contains("Cannot enumerate processes"))
        );
        // The console's listener allocator is an independent execution record.
        let terminal = card.terminal.as_mut().unwrap();
        if let crate::model::TerminalContinuity::Live { boot_id, .. } = &mut terminal.continuity {
            *boot_id = BootId::new("prior-boot");
        }
        terminal.owned_codex_listener = card.owned_codex_listener.clone();
        terminal
            .owned_codex_listener
            .as_mut()
            .unwrap()
            .allocator
            .boot_id = current_boot();
        assert!(
            matches!(unreadable_kernel.codex_execution(&card), CodexExecution::Unproved { reason } if reason.contains("Cannot enumerate processes"))
        );
        card.terminal = None;
        old_holder.remove(holders.path());
        old_holder.boot = current_boot();
        old_holder.create(holders.path()).unwrap();
        assert!(matches!(
            unreadable_kernel.codex_execution(&card),
            CodexExecution::RetainedProcesses
        ));
    }

    #[test]
    fn codex_pane_missing_capabilities_uses_execution_evidence() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let [card, _] = codex_cards(root.path());
        let live_processes = LiveProcesses::new();
        let mut paired = card.clone();
        paired.terminal = Some(
            crate::terminal_ipc::TerminalDescriptor::relaunchable(
                paired.id,
                paired.launch.recipe.clone(),
                None,
            )
            .session,
        );
        let mut unowned = card.clone();
        unowned.owned_codex_listener = None;
        let mut foreign = card.clone();
        foreign.launch.recipe.arguments[1] = "unix:///elsewhere/control.sock".into();
        let mut unproved = card.clone();
        unproved.proof = None;
        // A console paired in this boot may still run outside the server.
        let mut console = paired.clone();
        let mut console_proof = card.proof.clone().unwrap();
        console.terminal.as_mut().unwrap().continuity = crate::model::TerminalContinuity::Live {
            host_session_id: console_proof.host_session_id.clone(),
            boot_id: console_proof.boot_id.clone(),
            host_incarnation: console_proof.host_incarnation.clone(),
        };
        for card in [paired, unowned, foreign, unproved, console.clone()] {
            assert_eq!(
                live_processes.evidence(holders.path()).classify(&card),
                SavedExecution::Ended
            );
        }
        // One paired in an earlier boot cannot, so the server alone decides.
        console_proof.boot_id = BootId::new("previous-boot");
        console.terminal.as_mut().unwrap().continuity = crate::model::TerminalContinuity::Live {
            host_session_id: console_proof.host_session_id,
            boot_id: console_proof.boot_id,
            host_incarnation: console_proof.host_incarnation,
        };
        assert_eq!(
            live_processes.evidence(holders.path()).classify(&console),
            SavedExecution::Ended
        );
        let mut unreadable = live_processes.evidence(holders.path());
        unreadable.native_holders = Err("unavailable".into());
        assert_eq!(unreadable.classify(&card), SavedExecution::Unchecked);
        // A process table that cannot be read proves nothing.
        let mut absent = live_processes.evidence(holders.path());
        absent.processes = root.path().join("absent");
        assert_eq!(absent.classify(&card), SavedExecution::Unchecked);
        // A process that has not published its arguments might be the
        // server, unless none of its threads can run again: each is a zombie,
        // exiting (PF_EXITING, like a PID-namespace init waiting in do_exit)
        // or has SIGKILL pending. A group exit leaves killed or exiting
        // threads behind a zombie leader. A live thread can still exec.
        let processes = tempfile::tempdir_in("/var/tmp").unwrap();
        std::fs::create_dir_all(processes.path().join("net")).unwrap();
        std::fs::write(
            processes.path().join("net/unix"),
            "Num       RefCount Protocol Flags    Type St Inode Path\n",
        )
        .unwrap();
        let process = processes.path().join("4242");
        std::fs::create_dir(&process).unwrap();
        std::os::unix::fs::symlink("/usr/bin/codex", process.join("exe")).unwrap();
        std::fs::write(process.join("cmdline"), b"").unwrap();
        let uid = crate::daemon::current_uid();
        let user = |uid: u32| format!("Name:\tcodex\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n");
        std::fs::write(process.join("status"), user(uid)).unwrap();
        let mut table = live_processes.evidence(holders.path());
        table.processes = processes.path().into();
        let thread = |tid: u32, state: char, flags: u64, pending: (u64, u64)| {
            let thread = process.join("task").join(tid.to_string());
            std::fs::create_dir_all(&thread).unwrap();
            std::fs::write(
                thread.join("stat"),
                format!("{tid} (a) b) {state} 1 4242 4242 0 0 {flags} 0 0 0 0 0 0 0 0 20 0 1 0 1"),
            )
            .unwrap();
            std::fs::write(
                thread.join("status"),
                format!(
                    "Name:\ta\nSigPnd:\t{:016x}\nShdPnd:\t{:016x}\n",
                    pending.0, pending.1
                ),
            )
            .unwrap();
            std::fs::write(thread.join("cmdline"), b"").unwrap();
        };
        let kill = 1 << (libc::SIGKILL - 1);
        let term = 1 << (libc::SIGTERM - 1);
        for (leader, other, expected) in [
            (('Z', 0, (0, 0)), None, SavedExecution::Ended),
            (('X', 0, (0, 0)), None, SavedExecution::Ended),
            (('D', 0x4, (0, 0)), None, SavedExecution::Ended),
            (('R', 0, (kill, 0)), None, SavedExecution::Ended),
            (('R', 0, (0, kill)), None, SavedExecution::Ended),
            (
                ('Z', 0x4, (0, 0)),
                Some(('R', 0x4, (0, 0))),
                SavedExecution::Ended,
            ),
            (
                ('Z', 0x4, (0, 0)),
                Some(('S', 0, (kill, 0))),
                SavedExecution::Ended,
            ),
            (
                ('Z', 0x4, (0, 0)),
                Some(('Z', 0, (0, 0))),
                SavedExecution::Ended,
            ),
            (
                ('Z', 0, (0, 0)),
                Some(('S', 0, (0, 0))),
                SavedExecution::Unchecked,
            ),
            (
                ('Z', 0, (0, 0)),
                Some(('S', 0, (term, term))),
                SavedExecution::Unchecked,
            ),
            (
                ('D', 0x4, (0, 0)),
                Some(('R', 0, (0, 0))),
                SavedExecution::Unchecked,
            ),
            (
                ('R', 0, (0, 0)),
                Some(('Z', 0, (0, 0))),
                SavedExecution::Unchecked,
            ),
            (('R', 0, (0, 0)), None, SavedExecution::Unchecked),
            (('D', 0, (0, 0)), None, SavedExecution::Unchecked),
        ] {
            let _ = std::fs::remove_dir_all(process.join("task"));
            thread(4242, leader.0, leader.1, leader.2);
            if let Some(other) = other {
                thread(4243, other.0, other.1, other.2);
            }
            assert_eq!(table.classify(&card), expected, "{leader:?} {other:?}");
        }
        // A live thread behind a zombie leader shows the arguments they share.
        let served = card.owned_codex_listener.clone().unwrap().socket;
        for (argv, expected) in [
            ("python\0worker\0".to_owned(), SavedExecution::Ended),
            (
                format!("codex\0app-server\0--listen\0unix://{}\0", served.display()),
                SavedExecution::Running,
            ),
        ] {
            std::fs::remove_dir_all(process.join("task")).unwrap();
            thread(4242, 'Z', 0x4, (0, 0));
            thread(4243, 'S', 0, (0, 0));
            std::fs::write(process.join("task/4243/cmdline"), &argv).unwrap();
            assert_eq!(table.classify(&card), expected, "{argv:?}");
        }
        // A thread already reaped is gone; the group is gone with its last one.
        std::fs::remove_dir_all(process.join("task")).unwrap();
        thread(4243, 'Z', 0, (0, 0));
        std::fs::create_dir_all(process.join("task/4242")).unwrap();
        assert_eq!(table.classify(&card), SavedExecution::Ended);
        std::fs::remove_dir_all(process.join("task")).unwrap();
        assert_eq!(table.classify(&card), SavedExecution::Ended);
        // A truncated status line proves nothing.
        thread(4242, 'Z', 0, (0, 0));
        std::fs::write(
            process.join("task/4242/stat"),
            b"4242 (codex) Z 1 4242 4242",
        )
        .unwrap();
        assert_eq!(table.classify(&card), SavedExecution::Unchecked);
        // The effective user decides, not the owner of the process directory,
        // which is root for a non-dumpable process of this user.
        thread(4242, 'R', 0, (0, 0));
        std::fs::write(process.join("status"), user(uid.wrapping_add(1))).unwrap();
        assert_eq!(table.classify(&card), SavedExecution::Ended);
        std::fs::write(process.join("status"), b"Name:\tcodex\n").unwrap();
        assert_eq!(table.classify(&card), SavedExecution::Unchecked);
        std::fs::write(process.join("status"), user(uid)).unwrap();
        let named = card.owned_codex_listener.clone().unwrap().socket;
        std::fs::write(
            process.join("cmdline"),
            format!("codex\0app-server\0--listen\0unix://{}\0", named.display()),
        )
        .unwrap();
        assert_eq!(table.classify(&card), SavedExecution::Running);
        std::fs::remove_dir_all(&process).unwrap();
        assert_eq!(table.classify(&card), SavedExecution::Ended);
        // A socket bound at the listener, or at a starting holder's control
        // endpoint, is live work; a different path that ends alike is not.
        let listener = card.owned_codex_listener.clone().unwrap().socket;
        let control = crate::daemon::holder_registry::socket(holders.path(), card.id);
        for (path, expected) in [
            (listener.clone(), SavedExecution::Running),
            (control, SavedExecution::Running),
            (
                format!("/elsewhere{}", listener.display()).into(),
                SavedExecution::Ended,
            ),
        ] {
            std::fs::write(
                processes.path().join("net/unix"),
                format!(
                    "Num       RefCount Protocol Flags    Type St Inode Path\n\
                     0000000000000000: 00000002 00000000 00010000 0001 01 4242 {}\n",
                    std::path::Path::new(&path).display()
                ),
            )
            .unwrap();
            assert_eq!(table.classify(&card), expected, "{}", path.display());
        }
        // Discovery evidence that cannot be authenticated is never absence.
        std::fs::write(
            crate::daemon::holder_registry::socket(holders.path(), card.id).with_extension("json"),
            b"{\"not\":\"a record\"}",
        )
        .unwrap();
        assert_eq!(
            live_processes.evidence(holders.path()).classify(&card),
            SavedExecution::Unchecked
        );
    }

    #[test]
    fn ended_panes_stay_unchanged_and_live_unlisted_panes_report_recovery() {
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let retired = retained_card(current_boot());
        let previous = retained_card(BootId::new("previous-boot"));
        let live = retained_card(current_boot());
        let held = retained_card(current_boot());
        let census = vec![
            (
                retired.id,
                owner(&retired, ended_process(), ended_process()),
            ),
            (live.id, owner(&live, running_process(), running_process())),
            (held.id, owner(&held, ended_process(), ended_process())),
        ];
        let evidence = evidence(holders.path(), census);
        let inventory = [held.clone()];
        let mut inspection = SourceInspection::default();
        let saved: Vec<_> = [&retired, &live, &previous, &held]
            .into_iter()
            .map(|source| {
                let kind = CardKind::Chat(source.clone());
                record_saved_pane(&mut inspection, &kind, false, &current_boot(), true);
                (crate::model::CardId::new(), kind)
            })
            .collect();
        record_ended_panes(&mut inspection, &saved, &evidence);
        assert_eq!(inspection.details.panes.len(), 4);
        for source in [&retired, &previous] {
            assert_eq!(
                inspection.details.panes[&source.id].outcome,
                SavedOutcome::Unchanged
            );
        }
        for source in [&retired, &live, &previous, &held] {
            if let Some((reason, recovery)) =
                saved_chat_problem(source, &inventory, &evidence, || Err("unqualified".into()))
            {
                inspection.stop(source.id, StoppedKind::Chat, &reason, recovery);
            }
        }
        assert_eq!(inspection.stopped.len(), 2);
        let running = inspection
            .stopped
            .iter()
            .find(|pane| pane.pane == live.id)
            .unwrap();
        assert_eq!(
            running.reason,
            format!(
                "Claude is still running outside cagents (pid {})",
                std::process::id()
            )
        );
        assert_eq!(
            running.recovery,
            "Stop that process, then choose Resume conversation in the new tab (+) menu"
        );
        let held = inspection
            .stopped
            .iter()
            .find(|pane| pane.pane == held.id)
            .unwrap();
        assert_eq!(held.reason, "its conversation could not be confirmed");
        assert_eq!(held.recovery, CHAT_RECOVERY);
        assert!(saved_chat_problem(&live, &inventory, &evidence, || Ok(())).is_none());
    }

    #[test]
    fn an_unnamed_pane_whose_actor_ended_unrecorded_reopens_stopped() {
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut unrecorded = retained_card(current_boot());
        unrecorded.launch.session_id = None;
        let mut running = retained_card(current_boot());
        running.launch.session_id = None;
        let ended = owner(&unrecorded, ended_process(), ended_process());
        let census = vec![
            (unrecorded.id, ended.clone()),
            (
                running.id,
                owner(&running, running_process(), running_process()),
            ),
        ];
        let mut evidence = evidence(holders.path(), census);
        assert_eq!(evidence.classify(&unrecorded), SavedExecution::Unrecorded);
        assert_eq!(evidence.classify(&running), SavedExecution::Running);
        let (reason, recovery) = saved_chat_problem(&unrecorded, &[], &evidence, || {
            panic!("an ended unrecorded pane needs no qualification")
        })
        .unwrap();
        let mut inspection = SourceInspection::default();
        inspection.stop(unrecorded.id, StoppedKind::Chat, &reason, recovery);
        assert_eq!(
            inspection.stopped[0].reason,
            "ended before it recorded its conversation"
        );
        assert_eq!(
            inspection.stopped[0].recovery,
            "Choose Resume conversation in the new tab (+) menu to find a saved conversation, or start a new conversation"
        );
        assert!(
            saved_chat_problem(&running, &[], &evidence, || Err("unqualified".into())).is_some()
        );
        assert_eq!(
            saved_chat_problem(
                &unrecorded,
                std::slice::from_ref(&unrecorded),
                &evidence,
                || Err("unqualified".into())
            )
            .unwrap()
            .0,
            "its conversation could not be confirmed"
        );
        evidence.retired = Ok(vec![ended]);
        assert_eq!(evidence.classify(&unrecorded), SavedExecution::Ended);
        evidence.retired = Err("unreadable".into());
        assert_eq!(evidence.classify(&unrecorded), SavedExecution::Unchecked);
    }

    #[test]
    fn exhausted_budget_omits_checked_panes_and_cold_terminals() {
        let live = retained_card(current_boot());
        let checked = retained_card(current_boot());
        let cold = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            TerminalId::new(),
            live.launch.recipe.clone(),
            None,
        )
        .session;
        let panes = vec![
            (crate::model::CardId::new(), CardKind::Chat(live.clone())),
            (crate::model::CardId::new(), CardKind::Chat(checked.clone())),
            (crate::model::CardId::new(), CardKind::Terminal(cold)),
        ];
        let mut inspection = SourceInspection::default();
        stop_unchecked(
            &mut inspection,
            &panes,
            &HashSet::from([checked.id]),
            &current_boot(),
        );
        assert_eq!(inspection.stopped.len(), 1);
        assert_eq!(inspection.stopped[0].pane, live.id);
        assert_eq!(inspection.stopped[0].reason, UNCHECKED);
    }

    #[test]
    fn census_and_process_scan_failures_reopen_only_the_unchecked_pane_stopped() {
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let card = retained_card(current_boot());
        for reason in [
            "Cannot enumerate processes",
            "The retained actor census exceeds its inspection bound",
            "exceeded its bounded budget",
        ] {
            let mut evidence = evidence(holders.path(), Vec::new());
            evidence.census = Err(reason.into());
            let (reason, recovery) =
                saved_chat_problem(&card, &[], &evidence, || Err("unqualified".into())).unwrap();
            let mut inspection = SourceInspection::default();
            inspection.stop(card.id, StoppedKind::Chat, &reason, recovery);
            assert_eq!(inspection.stopped[0].pane, card.id);
            assert_eq!(inspection.stopped[0].reason, UNCHECKED);
        }
    }

    #[test]
    fn replacing_only_the_ui_does_not_require_resuming_a_live_actor() {
        let mut live = session();
        live.launch.session_id = None;
        validate_live_chat(&live, false).unwrap();
        assert!(validate_live_chat(&live, true).is_err());
        qualify_with_mode(
            &live,
            std::slice::from_ref(&live),
            live.proof.as_ref().unwrap(),
            false,
            |_| panic!("an exact current actor needs no replacement"),
        )
        .unwrap();
    }

    #[test]
    fn paired_codex_issued_recipe_and_exact_execution_are_qualified_separately() {
        let mut live = session();
        live.launch.provider = Provider::Codex;
        live.launch.recipe.program = "codex".into();
        let recipe = live.launch.terminal_recipe().unwrap();
        let proof = live.proof.clone().unwrap();
        let mut paired =
            crate::terminal_ipc::TerminalDescriptor::relaunchable(live.id, recipe.clone(), None)
                .session;
        paired.continuity = crate::model::TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        live.terminal = Some(paired);
        validate_live_chat(&live, true).unwrap();
        qualify(&live, std::slice::from_ref(&live), &proof, |_| {
            panic!("current pair")
        })
        .unwrap();
        for launch in [
            live.launch.recipe.clone(),
            LaunchRecipe::shell("/tmp".into()).unwrap(),
        ] {
            live.terminal.as_mut().unwrap().launch = launch;
            validate_live_chat(&live, true).unwrap();
            qualify(&live, std::slice::from_ref(&live), &proof, |_| {
                panic!("current pair")
            })
            .unwrap();
            let mut changed = live.clone();
            changed
                .terminal
                .as_mut()
                .unwrap()
                .launch
                .environment
                .insert("CODEX_HOME".into(), "/another".into());
            assert!(qualify(&live, &[changed], &proof, |_| panic!("same daemon source")).is_err());
        }
    }

    #[test]
    fn exact_live_actor_can_supply_a_previously_unobserved_thread_identity() {
        let live = session();
        let mut saved = live.clone();
        saved.launch.session_id = None;
        qualify(
            &saved,
            std::slice::from_ref(&live),
            live.proof.as_ref().unwrap(),
            |_| panic!("current exact actor needs no lineage resolution"),
        )
        .unwrap();
        let mut changed = live.clone();
        changed
            .launch
            .recipe
            .environment
            .insert("ACCOUNT_SECRET".into(), "never print this".into());
        let error = qualify(&saved, &[changed], live.proof.as_ref().unwrap(), |_| {
            panic!("not a recovery request")
        })
        .unwrap_err();
        assert!(!error.contains("never print this"));
    }

    #[test]
    fn supplied_thread_must_match_the_authenticated_live_actor() {
        let live = session();
        let mut supplied = live.clone();
        supplied.launch.session_id = Some("22222222-2222-4222-8222-222222222222".into());
        assert!(
            qualify(
                &supplied,
                std::slice::from_ref(&live),
                live.proof.as_ref().unwrap(),
                |_| panic!("same-proof requests cannot invent a thread identity"),
            )
            .is_err()
        );
    }

    #[test]
    fn known_thread_cannot_be_qualified_by_unidentified_inventory() {
        let saved = session();
        let mut unidentified = saved.clone();
        unidentified.launch.session_id = None;
        assert!(
            qualify(
                &saved,
                &[unidentified],
                saved.proof.as_ref().unwrap(),
                |_| panic!("a current known identity cannot be erased")
            )
            .is_err()
        );
    }

    #[test]
    fn old_proof_requires_authenticated_resolution_and_exact_current_inventory() {
        let saved = session();
        let mut current = saved.clone();
        current.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("new-actor");
        qualify(
            &saved,
            std::slice::from_ref(&current),
            current.proof.as_ref().unwrap(),
            |_| Ok(current.clone()),
        )
        .unwrap();
        let mut altered = current.clone();
        altered
            .launch
            .recipe
            .arguments
            .push("--different-account".into());
        assert!(
            qualify(
                &saved,
                std::slice::from_ref(&altered),
                current.proof.as_ref().unwrap(),
                |_| Ok(altered.clone())
            )
            .is_err()
        );
    }

    #[test]
    fn already_corrupted_77_published_resume_argv_cannot_qualify_a_saved_pane() {
        let saved = session();
        let mut current = saved.clone();
        current.launch.recipe.arguments.extend([
            "--resume".into(),
            "11111111-1111-4111-8111-111111111111".into(),
        ]);
        assert!(
            qualify(
                &saved,
                &[current],
                saved.proof.as_ref().unwrap(),
                |_| panic!("same proof cannot authorize recipe drift")
            )
            .is_err()
        );
    }

    #[test]
    fn terminal_source_requires_exact_recipe_and_authenticated_lineage() {
        let chat = session();
        let proof = chat.proof.unwrap();
        let mut descriptor = crate::terminal::ipc::TerminalDescriptor::relaunchable(
            chat.id,
            chat.launch.recipe,
            None,
        );
        descriptor.session.continuity = crate::model::TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        let saved = descriptor.session.clone();
        qualify_terminal(&saved, &[descriptor.clone().into()], &proof).unwrap();
        descriptor.session.resume_hint = Some("observed-agent-inside-original-shell".into());
        qualify_terminal(&saved, &[descriptor.clone().into()], &proof).unwrap();
        let mut detached = saved.clone();
        detached.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        qualify_terminal(&detached, &[descriptor.clone().into()], &proof).unwrap();
        for field in 0..5 {
            let mut changed = descriptor.clone();
            match field {
                0 => changed.session.launch.working_directory = "/another".into(),
                1 => {
                    changed
                        .session
                        .launch
                        .unset_environment
                        .insert("ACCOUNT".into());
                }
                2 => changed.session.transcript_path = Some("/another/transcript".into()),
                3 => changed.session.id = crate::model::TerminalId::new(),
                _ => {
                    changed.session.owned_codex_listener = Some(crate::chat::OwnedCodexListener {
                        socket: "/another/socket".into(),
                        allocator: proof.clone(),
                    })
                }
            }
            assert!(
                qualify_terminal(&saved, &[changed.into()], &proof).is_err(),
                "immutable field {field}"
            );
        }
        descriptor
            .session
            .launch
            .environment
            .insert("ACCOUNT".into(), "different".into());
        assert!(qualify_terminal(&saved, &[descriptor.clone().into()], &proof).is_err());
        descriptor.session.launch = saved.launch.clone();
        let mut replacement = proof.clone();
        replacement.host_incarnation = HostIncarnation::new("replacement");
        descriptor.session.continuity = crate::model::TerminalContinuity::Live {
            host_session_id: replacement.host_session_id.clone(),
            boot_id: replacement.boot_id.clone(),
            host_incarnation: replacement.host_incarnation.clone(),
        };
        assert!(qualify_terminal(&saved, &[descriptor.clone().into()], &replacement).is_err());
        descriptor.upgrade_sources.push(detached);
        qualify_terminal(&saved, &[descriptor.clone().into()], &replacement).unwrap();
        descriptor.upgrade_sources[0]
            .launch
            .environment
            .insert("ACCOUNT".into(), "another".into());
        assert!(qualify_terminal(&saved, &[descriptor.into()], &replacement).is_err());
    }

    #[test]
    fn a_pane_held_by_several_saved_windows_is_one_outcome() {
        let mut inspection = SourceInspection::default();
        let pane = TerminalId::new();
        for _ in 0..3 {
            inspection.stop(
                pane,
                StoppedKind::Chat,
                "its conversation could not be confirmed",
                CHAT_RECOVERY,
            );
        }
        assert_eq!(inspection.stopped.len(), 1);
        assert_eq!(inspection.stopped[0].pane, pane);
    }

    #[test]
    fn a_proofless_pane_ends_only_when_every_owner_it_had_is_retired() {
        let holders = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut card = session();
        card.proof = None;
        let ended = owner(&card, ended_process(), ended_process());
        let mut earlier = ended.clone();
        earlier.boot = BootId::new("previous-boot");
        let running = owner(&card, running_process(), running_process());

        let mut retired = evidence(
            holders.path(),
            vec![(card.id, ended.clone()), (card.id, earlier.clone())],
        );
        retired.retired = Ok(vec![ended.clone(), earlier.clone()]);
        assert_eq!(retired.classify(&card), SavedExecution::Ended);

        // A recorded retirement is required for every owner, in any boot.
        let mut unrecorded = evidence(
            holders.path(),
            vec![(card.id, ended.clone()), (card.id, earlier.clone())],
        );
        unrecorded.retired = Ok(vec![ended.clone()]);
        assert_eq!(unrecorded.classify(&card), SavedExecution::Unproved);

        // A retirement record does not outweigh a process that remains.
        let mut alive = evidence(holders.path(), vec![(card.id, running.clone())]);
        alive.retired = Ok(vec![running]);
        assert_eq!(alive.classify(&card), SavedExecution::Running);

        // No census owner is no evidence of what ran the pane.
        let mut unknown = evidence(holders.path(), Vec::new());
        unknown.retired = Ok(vec![ended.clone()]);
        assert_eq!(unknown.classify(&card), SavedExecution::Unproved);

        // An unreadable retirement table proves nothing.
        let mut unreadable = evidence(holders.path(), vec![(card.id, ended.clone())]);
        unreadable.retired = Err("unreadable".into());
        assert_eq!(unreadable.classify(&card), SavedExecution::Unchecked);

        // Only a retained Claude pane has an owner roster; an incomplete proof
        // is not the absence of one.
        let mut codex = card.clone();
        codex.launch.provider = Provider::Codex;
        assert_eq!(
            LiveProcesses::new()
                .evidence(holders.path())
                .classify(&codex),
            SavedExecution::Ended,
            "a proofless Codex pane uses kernel evidence without a Claude census"
        );
        let mut incomplete = card.clone();
        incomplete.proof = Some(HostSessionProof {
            host_session_id: HostSessionId::new(""),
            boot_id: current_boot(),
            host_incarnation: HostIncarnation::new("actor"),
        });
        assert_eq!(retired.classify(&incomplete), SavedExecution::Unproved);
    }

    #[test]
    fn a_codex_pane_relisted_under_a_newer_proof_names_that_live_conversation() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let saved = codex_card(root.path(), NativeContinuity::ReconnectOnly);
        let current = HostSessionProof {
            host_session_id: HostSessionId::new("current"),
            boot_id: current_boot(),
            host_incarnation: HostIncarnation::new("current"),
        };
        let mut live = saved.clone();
        live.proof = Some(current.clone());
        let inventory = [live.clone()];
        assert!(relisted_codex_conversation(
            &saved, &inventory, &current, false
        ));

        // The pane's own proof, an incomplete proof or a missing one is not
        // an older issued identity of a relisted conversation.
        assert!(!relisted_codex_conversation(
            &live, &inventory, &current, false
        ));
        let mut proofless = saved.clone();
        proofless.proof = None;
        assert!(!relisted_codex_conversation(
            &proofless, &inventory, &current, false
        ));

        // Every other issued field must be identical.
        for change in 0..4 {
            let mut other = live.clone();
            match change {
                0 => other.launch.session_id = Some(uuid::Uuid::new_v4().to_string()),
                1 => other.launch.recipe.working_directory = "/elsewhere".into(),
                2 => other.owned_codex_listener.as_mut().unwrap().allocator = current.clone(),
                _ => {
                    other.terminal = Some(
                        crate::terminal_ipc::TerminalDescriptor::relaunchable(
                            other.id,
                            other.launch.recipe.clone(),
                            None,
                        )
                        .session,
                    )
                }
            }
            assert!(
                !relisted_codex_conversation(&saved, &[other], &current, false),
                "change {change}"
            );
        }
        // Two listings are ambiguous, and a listing under another proof is
        // not the source's current work.
        assert!(!relisted_codex_conversation(
            &saved,
            &[live.clone(), live.clone()],
            &current,
            false
        ));
        let mut foreign = live.clone();
        foreign.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("foreign");
        assert!(!relisted_codex_conversation(
            &saved,
            &[foreign],
            &current,
            false
        ));
        // A retained Claude pane keeps its exact recovery contract.
        let claude = retained_card(current_boot());
        let mut relisted = claude.clone();
        relisted.proof = Some(current.clone());
        assert!(!relisted_codex_conversation(
            &claude,
            &[relisted],
            &current,
            false
        ));
    }

    #[test]
    fn only_a_process_given_the_listener_to_serve_is_its_provider() {
        let socket = Path::new("/tmp/cagents-codex-x/control.sock");
        for provider in [
            &b"codex\0app-server\0--listen\0unix:///tmp/cagents-codex-x/control.sock\0"[..],
            b"codex\0app-server\0--listen=unix:///tmp/cagents-codex-x/control.sock\0",
        ] {
            assert!(serves_listener(socket, provider));
        }
        // Naming the directory, or even the socket, is not serving it.
        for other in [
            &b"ls\0/tmp/cagents-codex-x\0"[..],
            b"tail\0unix:///tmp/cagents-codex-x/control.sock\0",
            b"codex\0--listen\0unix:///tmp/cagents-codex-x/control.sock.old\0",
        ] {
            assert!(!serves_listener(socket, other));
        }
    }
}

#[cfg(test)]
fn qualify(
    saved: &Session,
    inventory: &[Session],
    proof: &HostSessionProof,
    resolve: impl FnMut(&Session) -> Result<Session, String>,
) -> Result<(), String> {
    qualify_with_mode(saved, inventory, proof, true, resolve)
}

fn qualify_with_mode(
    saved: &Session,
    inventory: &[Session],
    proof: &HostSessionProof,
    require_resumable: bool,
    mut resolve: impl FnMut(&Session) -> Result<Session, String>,
) -> Result<(), String> {
    let resolved;
    let source = if saved.proof.as_ref() == Some(proof)
        && inventory.iter().any(|live| live.id == saved.id)
    {
        saved
    } else {
        resolved = resolve(saved)?;
        if !crate::chat::is_upgrade_recovery(saved, &resolved)
            && !crate::chat::is_upgrade_identity_learning(saved, &resolved)
        {
            return Err("Saved conversation resolution changed its issued identity".into());
        }
        &resolved
    };
    let live = inventory
        .iter()
        .find(|live| {
            if handoff::issued_chat_matches(live, source) {
                return true;
            }
            // A current pane may not have observed the live actor's first
            // identity yet. Inspection can take that identity from exact,
            // authenticated current inventory. Recovery aliases remain exact
            // and cannot erase a previously issued known identity.
            if saved.proof.as_ref() == Some(proof) && source.launch.session_id.is_none() {
                let mut observed = source.clone();
                observed.launch.session_id = live.launch.session_id.clone();
                return handoff::issued_chat_matches(live, &observed);
            }
            false
        })
        .ok_or("Saved conversation has no exact current source owner")?;
    if require_resumable {
        handoff::validate_chat_source(source, &live.launch)
    } else {
        Ok(())
    }
}

fn validate_live_chat(session: &Session, require_resumable: bool) -> Result<(), String> {
    if !session.has_valid_native_continuity()
        || session.owned_codex_listener.is_some() && !session.owns_codex_listener()
        || session.terminal.as_ref().is_some_and(|terminal| {
            terminal.id != session.id
                || terminal.continuity.live_proof().as_ref() != session.proof.as_ref()
        })
    {
        return Err("Source conversation ownership evidence is inconsistent".into());
    }
    if require_resumable {
        handoff::validate_chat_source(session, &session.launch)
    } else {
        Ok(())
    }
}
