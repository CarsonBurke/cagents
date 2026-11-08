//! Bounded source qualification after source windows flush, before close.
//! Stores remain read-only. Exact retained providers missing from startup
//! discovery may be authenticated through Resolve; no launch recipe is run.
mod current;
mod v68;
mod v77;
#[cfg(test)]
pub(super) use current::Client as InspectionClient;

use crate::install_guard::SourceInspectionRequest as Request;
use crate::{
    chat::Session,
    model::{CardKind, HostSessionProof},
    upgrade::handoff,
};
use std::{
    collections::HashSet,
    io::{self, Write},
    path::Path,
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
        // Errors are deliberately restricted to card IDs and fixed reasons.
        // The parent can report an actionable deferral without exposing argv,
        // account environment, listener credentials, or transcript contents.
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
    fn reconcile_retained(&mut self, source: &Session) -> Result<(), String> {
        match self {
            Self::Current(client) => client.reconcile_retained(source),
            _ => Ok(()),
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
    if !current::supports_protocol(request.protocol)
        && request.protocol != 77
        && request.protocol != 68
    {
        return Err(
            "Source protocol has no qualified migration inspector; source windows remain usable"
                .into(),
        );
    }
    let proof = request
        .proof
        .as_ref()
        .filter(|proof| proof.is_complete())
        .ok_or("Saved conversations have no authenticated source service")?;
    let socket = crate::terminal::ipc::daemon_socket_path()
        .map_err(|_| "Source service namespace is unavailable")?;
    if current::supports_protocol(request.protocol) {
        current::Client::connect(&socket, proof, deadline, request.protocol).map(Reader::Current)
    } else if request.protocol == 68 {
        v68::Client::connect(&socket, proof, deadline).map(Reader::Legacy68)
    } else {
        v77::Client::connect(&socket, proof, deadline).map(Reader::Legacy)
    }
}

/// Keep the complete classification in one bounded pass. Details are redacted
/// and capped independently of the inventory: a large workspace must not turn
/// one refusal into a sequence of install/close/retry cycles.
#[derive(Default)]
struct InspectionFailures {
    count: usize,
    details: Vec<String>,
    /// Every distinct reason, so one pane retained by several saved windows
    /// is one problem to resolve rather than one per store.
    seen: HashSet<String>,
}

impl InspectionFailures {
    fn push(&mut self, reason: String) {
        if !self.seen.insert(reason.clone()) {
            return;
        }
        self.count += 1;
        if self.details.len() < 40 {
            self.details.push(reason);
        }
    }

    fn finish(self) -> Result<(), String> {
        if self.count == 0 {
            return Ok(());
        }
        let omitted = self.count - self.details.len();
        let mut message = format!(
            "Update needs recovery for {} saved-state problem(s). Source windows remain usable.\n{}",
            self.count,
            self.details.join("\n")
        );
        if omitted > 0 {
            message.push_str(&format!(
                "\n{omitted} additional problems were classified; showing the first 40."
            ));
        }
        Err(message)
    }
}

fn inspect(request: Request) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let current_boot =
        crate::daemon::read_boot_id().map_err(|_| "Cannot verify the current kernel boot")?;
    let paths = crate::instance::window_database_paths()
        .map_err(|_| "Cannot enumerate saved source windows")?;
    let mut failures = InspectionFailures::default();
    let mut budget = crate::persistence::MigrationInspectionBudget::new(deadline);
    let mut saved = Vec::new();
    for (window, path) in paths.into_iter().enumerate() {
        check_deadline(deadline)?;
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                failures.push(format!("Saved window {} cannot be read.", window + 1));
                continue;
            }
            Ok(_) => {}
        }
        match crate::persistence::read_open_migration_sessions_with_budget(&path, &mut budget) {
            Ok(panes) => saved.extend(panes),
            Err(_) => failures.push(format!("Saved window {} has an unreadable or unsupported pane store; its contents were preserved.", window + 1)),
        }
    }
    let evidence = ExecutionEvidence::load(&current_boot);
    let no_live_panes = saved.iter().all(|(_, kind)| match kind {
        CardKind::Chat(session) => {
            session.is_unstarted()
                || evidence
                    .classify(session)
                    .leaves_nothing_to_carry(request.accept_unreopenable)
        }
        CardKind::Terminal(session) => !terminal_requires_migration(session, &current_boot),
        _ => true,
    });
    if no_live_panes && request.proof.is_none() {
        return failures.finish();
    }
    let mut reader = match connect(&request, deadline) {
        Ok(reader) => reader,
        Err(reason) => {
            failures.push(reason);
            return failures.finish();
        }
    };
    // Resolve stale and missing sources first, then sample the roster once.
    // Every cache entry retains the complete issued descriptor; two aliases
    // with the same actor ID cannot borrow each other's authentication.
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
                // Nothing of its old owner remains to adopt or preserve.
                if missing
                    && evidence
                        .classify(source)
                        .leaves_nothing_to_carry(request.accept_unreopenable)
                {
                    continue;
                }
                let result = if missing && request.reconcile_retained {
                    reader
                        .reconcile_retained(source)
                        .and_then(|()| reader.resolve(source))
                } else if missing {
                    Err(
                        "Saved owner changed after preview; review the recovered inventory again"
                            .into(),
                    )
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
        return Err(
            "Source inventory exceeds the qualified migration limit; source windows remain usable"
                .into(),
        );
    }
    let proof = request
        .proof
        .as_ref()
        .ok_or("Source service proof is unavailable")?;
    let matched_cli = crate::companion::Product::Cli
        .beside_current()
        .map_err(|_| "Cannot qualify the destination console companion")?;
    let mut ids = HashSet::new();
    for session in &inventory {
        check_deadline(deadline)?;
        if !ids.insert(session.id) || session.proof.as_ref() != Some(proof) {
            failures.push(format!(
                "Source conversation {} has ambiguous ownership.",
                session.id
            ));
            continue;
        }
        if let Some(terminal) = &session.terminal {
            if let Err(reason) = qualify_console_migration(terminal, &matched_cli) {
                failures.push(format!("Source conversation {}: {reason}.", session.id));
            }
        }
        if validate_live_chat(session, request.require_resumable).is_err() {
            failures.push(format!(
                "Source conversation {} has no qualified immutable migration.",
                session.id
            ));
        }
    }
    for terminal in &terminals {
        check_deadline(deadline)?;
        if terminal_requires_migration(&terminal.session, &current_boot) {
            if let Err(reason) = qualify_console_migration(&terminal.session, &matched_cli) {
                failures.push(format!(
                    "Source terminal {}: {reason}.",
                    terminal.session.id
                ));
            }
        }
    }
    for (card, kind) in saved {
        check_deadline(deadline)?;
        match kind {
            CardKind::Terminal(source) if terminal_requires_migration(&source, &current_boot) => {
                if qualify_terminal(&source, &terminals, proof).is_err() {
                    failures.push(format!(
                        "Saved terminal pane {card} needs exact-owner recovery."
                    ));
                }
            }
            CardKind::Chat(source) if !source.is_unstarted() => {
                if cold_history.iter().any(|issued| issued == &source)
                    || relisted_codex_conversation(
                        &source,
                        &inventory,
                        proof,
                        request.require_resumable,
                    )
                {
                    continue;
                }
                if let Some(problem) = saved_chat_problem(
                    card,
                    &source,
                    &inventory,
                    &evidence,
                    request.accept_unreopenable,
                    || {
                        qualify_with_mode(
                            &source,
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
                    },
                ) {
                    failures.push(problem);
                }
            }
            _ => {}
        }
    }
    failures.finish()
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
    /// A pane that never learned its conversation, every actor of which has
    /// ended, but at least one without the retirement that records what it
    /// confirmed. Nothing still runs, yet nothing durable names a
    /// conversation to reopen: the destination keeps the pane and refuses to
    /// resume or replace it. Carrying it is a loss the person must accept.
    Unrecorded,
}

impl SavedExecution {
    /// Whether replacing the source cannot lose this pane's execution: it has
    /// ended, or ended unrecorded and the person accepted it will not reopen.
    fn leaves_nothing_to_carry(self, accept_unreopenable: bool) -> bool {
        self == Self::Ended || (accept_unreopenable && self == Self::Unrecorded)
    }
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
}

/// Every retained helper the durable census registered, by pane.
type Census = Vec<(
    crate::model::TerminalId,
    crate::daemon::holder_registry::Record,
)>;

/// The census is a bounded roster, not a log; a larger one is not inspected.
const MAX_CENSUS_ROWS: usize = 16 * 1024;

impl ExecutionEvidence {
    fn load(boot: &crate::model::BootId) -> Self {
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
        if source.proof.is_none() {
            return self.classify_proofless(source);
        }
        let Some(proof) = source.proof.as_ref().filter(|proof| proof.is_complete()) else {
            return SavedExecution::Unproved;
        };
        if proof.boot_id != self.boot {
            return SavedExecution::Ended;
        }
        if source.launch.provider == crate::agent::Provider::Codex {
            return self.classify_codex(source);
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
            return SavedExecution::Unproved;
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
            Err(_) => return SavedExecution::Unproved,
        };
        owners.extend(&discovered);
        // A pane that never learned its conversation resumes only the one a
        // recorded retirement says its actor confirmed; without that record
        // its ended actor leaves it nothing to reopen.
        let unnamed = source.has_unnamed_retained_claude_actor();
        let mut execution = SavedExecution::Ended;
        let mut unrecorded = false;
        for owner in owners.into_iter().filter(|owner| owner.boot == self.boot) {
            match crate::chat::runtime::retained_actor_abandoned(&owner.holder, &owner.root) {
                Ok(true) => {}
                Ok(false) => execution = SavedExecution::Running,
                Err(_) => return SavedExecution::Unproved,
            }
            match &self.retired {
                Ok(retired) => unrecorded |= unnamed && !retired.contains(owner),
                Err(_) if unnamed => return SavedExecution::Unproved,
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
            return SavedExecution::Unproved;
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
            Err(_) => return SavedExecution::Unproved,
        };
        owners.extend(&discovered);
        let mut execution = SavedExecution::Ended;
        for owner in owners {
            if !retired.contains(owner) {
                return SavedExecution::Unproved;
            }
            if owner.boot != self.boot {
                continue;
            }
            match crate::chat::runtime::retained_actor_abandoned(&owner.holder, &owner.root) {
                Ok(true) => {}
                Ok(false) => execution = SavedExecution::Running,
                Err(_) => return SavedExecution::Unproved,
            }
        }
        execution
    }

    /// A Codex pane of this boot is served by one `codex app-server` that
    /// listens on the pane's own daemon-allocated socket, run either by the
    /// daemon itself or by a native holder. That socket path names the pane,
    /// and the server is the only writer of its conversation. It has ended
    /// when its holder, if one registered, is gone with everything in its
    /// process group and session, no process of this user still names the
    /// pane's listener, and no socket is bound at that path or at the socket
    /// Codex publishes it as a symlink to. Tool work that
    /// left a dead server's process group writes no conversation. A paired
    /// console runs outside the server, so a pane whose console was paired in
    /// this boot stays unproved.
    fn classify_codex(&self, source: &Session) -> SavedExecution {
        use crate::daemon::holder_registry::{self, Kind};
        // A console paired in an earlier kernel boot cannot still run; one of
        // this boot, or with incomplete evidence, is outside what this sees.
        let console_ended = source.terminal.as_ref().is_none_or(|terminal| {
            terminal
                .continuity
                .live_proof()
                .is_some_and(|proof| proof.is_complete() && proof.boot_id != self.boot)
        });
        let Some(listener) = source.owned_codex_listener.as_ref().filter(|_| {
            source.owns_codex_listener() && source.has_valid_native_continuity() && console_ended
        }) else {
            return SavedExecution::Unproved;
        };
        let Ok(native_holders) = &self.native_holders else {
            return SavedExecution::Unproved;
        };
        let control = holder_registry::socket(native_holders, source.id);
        // Discovery refuses a record of another boot.
        let holder = match holder_registry::recovery_candidate(&control, Kind::Native) {
            Ok(holder) => holder,
            Err(_) => return SavedExecution::Unproved,
        };
        if let Some(holder) = holder {
            match crate::chat::runtime::retained_actor_abandoned(&holder.holder, &holder.root) {
                Ok(true) => {}
                Ok(false) => return SavedExecution::Running,
                Err(_) => return SavedExecution::Unproved,
            }
        }
        // Recent Codex releases bind a socket in their own runtime directory
        // and publish the listener path as a symlink to it.
        let published = match std::fs::read_link(&listener.socket) {
            Ok(target) => Some(listener.socket.with_file_name(target)),
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::EINVAL) =>
            {
                None
            }
            Err(_) => return SavedExecution::Unproved,
        };
        // A holder binds its control socket before it starts the server and
        // publishes its record, so a starting holder is seen by that socket.
        let mut sockets = vec![listener.socket.as_path(), control.as_path()];
        sockets.extend(published.as_deref());
        match codex_listener_survives(&self.processes, source.id, &sockets, self.exec_settle) {
            Ok(true) => SavedExecution::Running,
            Ok(false) => SavedExecution::Ended,
            Err(_) => SavedExecution::Unproved,
        }
    }
}

/// Whether nothing of a Codex pane of `boot` can still run, by the evidence
/// an upgrade requires before it replaces the service that ran it.
pub(super) fn codex_execution_ended(source: &Session, boot: &crate::model::BootId) -> bool {
    source.launch.provider == crate::agent::Provider::Codex
        && ExecutionEvidence::codex(boot).classify_codex(source) == SavedExecution::Ended
}

/// Whether any process of this user names a Codex pane's listener directory
/// in its arguments, or a socket is still bound at one of `sockets`. A
/// process shows no arguments once its group leader has released its memory,
/// and between `execve` and publishing its new ones. It is evidence of
/// absence only when none of its threads can run again, and a live thread
/// shows the arguments its leader no longer does (see [`threads`]). One that
/// still shows none is read again for up to `settle`, then proves nothing.
fn codex_listener_survives(
    processes: &Path,
    pane: crate::model::TerminalId,
    sockets: &[&Path],
    settle: Duration,
) -> Result<bool, String> {
    use std::os::unix::ffi::OsStrExt;
    let marker = format!("cagents-codex-{pane}");
    let marker = marker.as_bytes();
    let uid = crate::daemon::current_uid().to_string();
    // The arguments a process shows, or None once it can no longer run.
    let arguments = |path: &Path| -> Result<Option<Vec<u8>>, String> {
        let deadline = Instant::now() + settle;
        settled_process_arguments(
            || process_arguments(path),
            || {
                if Instant::now() >= deadline {
                    return Err("A process has not published its arguments yet".into());
                }
                std::thread::sleep(Duration::from_millis(2));
                Ok(())
            },
        )
    };
    let entries = std::fs::read_dir(processes)
        .map_err(|error| format!("Cannot enumerate processes: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("Cannot enumerate processes: {error}"))?;
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
            Err(error) if gone(&error) => continue,
            Err(error) => return Err(format!("Cannot inspect process status: {error}")),
        };
        let effective = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|ids| ids.split_whitespace().nth(1));
        match effective {
            Some(effective) if effective != uid => continue,
            Some(_) => {}
            None => return Err("Cannot read a process's user".into()),
        }
        if arguments(&path)?
            .is_some_and(|argv| argv.windows(marker.len()).any(|window| window == marker))
        {
            return Ok(true);
        }
    }
    let bound = std::fs::read(processes.join("net/unix"))
        .map_err(|error| format!("Cannot inspect bound sockets: {error}"))?;
    // The path is the last field, after the fixed-width columns.
    Ok(bound.split(|byte| *byte == b'\n').skip(1).any(|line| {
        sockets.iter().any(|socket| {
            let socket = socket.as_os_str().as_bytes();
            line.len() > socket.len()
                && line.ends_with(socket)
                && line[line.len() - socket.len() - 1] == b' '
        })
    }))
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
        Err(error) if gone(&error) => Ok(Threads::Ended),
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
            Err(error) if gone(&error) => return Ok(None),
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
            Err(error) if gone(&error) => unsettled = true,
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
        Err(error) if gone(&error) => return Ok(true),
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
        Err(error) if gone(&error) => return Ok(true),
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
fn saved_chat_problem(
    card: crate::model::CardId,
    source: &Session,
    inventory: &[Session],
    evidence: &ExecutionEvidence,
    accept_unreopenable: bool,
    qualify: impl FnOnce() -> Result<(), String>,
) -> Option<String> {
    let execution =
        (!inventory.iter().any(|live| live.id == source.id)).then(|| evidence.classify(source));
    if execution.is_some_and(|execution| execution.leaves_nothing_to_carry(accept_unreopenable)) {
        return None;
    }
    qualify()
        .err()
        .map(|_| unqualified_chat_pane(card, execution))
}

fn unqualified_chat_pane(card: crate::model::CardId, execution: Option<SavedExecution>) -> String {
    if execution == Some(SavedExecution::Running) {
        format!(
            "Saved conversation pane {card} still has provider processes the source service does not list; reopen or close that pane in the source app, then update."
        )
    } else if execution == Some(SavedExecution::Unrecorded) {
        format!(
            "Saved conversation pane {card} ended before it recorded which conversation it had, so the update may not be able to reopen it. Nothing of it still runs. To update anyway, run `cagents daemon upgrade --accept-unreopenable` (or `scripts/install-local --accept-unreopenable`): the pane stays in its window, and if its conversation cannot be proved it will not resume or start one. A conversation it began stays on disk and can be reopened with Resume."
        )
    } else {
        format!("Saved conversation pane {card} needs exact conversation recovery.")
    }
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
    fn large_workspace_refusals_are_reported_together_with_bounded_details() {
        let mut failures = InspectionFailures::default();
        for _ in 0..65 {
            failures.push(format!(
                "Saved conversation pane {} needs exact conversation recovery.",
                crate::model::CardId::new()
            ));
        }
        let message = failures.finish().unwrap_err();
        assert!(message.contains("65 saved-state problem(s)"));
        assert!(message.contains("25 additional problems"));
        assert!(message.len() < 16 * 1024);
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
    fn same_boot_execution_without_complete_evidence_is_unproved() {
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
        for card in [paired, unowned, reconnect_only, codex] {
            assert_eq!(
                evidence(holders.path(), ended.clone()).classify(&card),
                SavedExecution::Unproved
            );
        }
        let mut unreadable = evidence(holders.path(), ended.clone());
        unreadable.census = Err("unreadable".into());
        assert_eq!(unreadable.classify(&card), SavedExecution::Unproved);
        // Discovery evidence that cannot be authenticated is never absence.
        std::fs::write(
            crate::daemon::holder_registry::socket(holders.path(), card.id).with_extension("json"),
            b"{\"not\":\"a record\"}",
        )
        .unwrap();
        assert_eq!(
            evidence(holders.path(), ended).classify(&card),
            SavedExecution::Unproved
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
            // Discovery refuses a record of another boot; that proves nothing.
            let mut older = owner(&card, running_process(), running_process());
            older.boot = BootId::new("previous-boot");
            let holders = tempfile::tempdir_in("/var/tmp").unwrap();
            older.create(holders.path()).unwrap();
            assert_eq!(
                live_processes.evidence(holders.path()).classify(&card),
                SavedExecution::Unproved
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
                &[listener.as_path(), control.as_path()],
                Duration::from_secs(2),
            ) {
                Ok(false) => absent += 1,
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
        assert_eq!(evidence.classify(&card), SavedExecution::Unproved);

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
            let mut server = std::process::Command::new("/bin/sh")
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
    fn codex_pane_without_complete_evidence_is_unproved() {
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
                SavedExecution::Unproved
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
        assert_eq!(unreadable.classify(&card), SavedExecution::Unproved);
        // A process table that cannot be read proves nothing.
        let mut absent = live_processes.evidence(holders.path());
        absent.processes = root.path().join("absent");
        assert_eq!(absent.classify(&card), SavedExecution::Unproved);
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
                SavedExecution::Unproved,
            ),
            (
                ('Z', 0, (0, 0)),
                Some(('S', 0, (term, term))),
                SavedExecution::Unproved,
            ),
            (
                ('D', 0x4, (0, 0)),
                Some(('R', 0, (0, 0))),
                SavedExecution::Unproved,
            ),
            (
                ('R', 0, (0, 0)),
                Some(('Z', 0, (0, 0))),
                SavedExecution::Unproved,
            ),
            (('R', 0, (0, 0)), None, SavedExecution::Unproved),
            (('D', 0, (0, 0)), None, SavedExecution::Unproved),
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
        assert_eq!(table.classify(&card), SavedExecution::Unproved);
        // The effective user decides, not the owner of the process directory,
        // which is root for a non-dumpable process of this user.
        thread(4242, 'R', 0, (0, 0));
        std::fs::write(process.join("status"), user(uid.wrapping_add(1))).unwrap();
        assert_eq!(table.classify(&card), SavedExecution::Ended);
        std::fs::write(process.join("status"), b"Name:\tcodex\n").unwrap();
        assert_eq!(table.classify(&card), SavedExecution::Unproved);
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
            SavedExecution::Unproved
        );
    }

    #[test]
    fn ended_panes_neither_hide_nor_join_a_live_refusal() {
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
        let (live_card, held_card) = (crate::model::CardId::new(), crate::model::CardId::new());
        let mut failures = InspectionFailures::default();
        for (card, source) in [
            (crate::model::CardId::new(), &retired),
            (live_card, &live),
            (crate::model::CardId::new(), &previous),
            (held_card, &held),
        ] {
            if let Some(problem) =
                saved_chat_problem(card, source, &inventory, &evidence, false, || {
                    Err("unqualified".into())
                })
            {
                failures.push(problem);
            }
        }
        let message = failures.finish().unwrap_err();
        assert!(message.contains("2 saved-state problem(s)"), "{message}");
        assert!(message.contains(&format!(
            "Saved conversation pane {live_card} still has provider processes the source service does not list; reopen or close that pane in the source app, then update."
        )));
        // A listed chat is the source's live work and always qualifies exactly.
        assert!(message.contains(&format!(
            "Saved conversation pane {held_card} needs exact conversation recovery."
        )));
        assert!(
            saved_chat_problem(live_card, &live, &inventory, &evidence, false, || Ok(())).is_none(),
            "a live actor the source adopted and qualified is not a problem"
        );
    }

    #[test]
    fn an_unnamed_pane_whose_actor_ended_unrecorded_is_carried_only_when_accepted() {
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
        // A surviving provider is live work, which no acceptance waives.
        assert_eq!(evidence.classify(&running), SavedExecution::Running);
        let card = crate::model::CardId::new();
        let refusal = saved_chat_problem(card, &unrecorded, &[], &evidence, false, || {
            Err("unqualified".into())
        })
        .unwrap();
        assert!(
            refusal.contains(&format!(
                "Saved conversation pane {card} ended before it recorded"
            )),
            "{refusal}"
        );
        assert!(refusal.contains("--accept-unreopenable"), "{refusal}");
        assert!(refusal.contains("reopened with Resume"), "{refusal}");
        assert_eq!(
            saved_chat_problem(card, &unrecorded, &[], &evidence, true, || panic!(
                "an accepted unrecorded pane needs no qualification"
            )),
            None
        );
        assert!(
            saved_chat_problem(card, &running, &[], &evidence, true, || Err(
                "unqualified".into()
            ))
            .is_some()
        );
        // Once the source lists it, it is the source's live work again.
        assert!(
            saved_chat_problem(
                card,
                &unrecorded,
                std::slice::from_ref(&unrecorded),
                &evidence,
                true,
                || Err("unqualified".into())
            )
            .is_some()
        );
        // A recorded retirement names what it confirmed; nothing to accept.
        evidence.retired = Ok(vec![ended]);
        assert_eq!(evidence.classify(&unrecorded), SavedExecution::Ended);
        // Unreadable retirements prove nothing either way.
        evidence.retired = Err("unreadable".into());
        assert_eq!(evidence.classify(&unrecorded), SavedExecution::Unproved);
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
    fn a_pane_held_by_several_saved_windows_is_one_problem() {
        let mut failures = InspectionFailures::default();
        let card = crate::model::CardId::new();
        for _ in 0..3 {
            failures.push(format!(
                "Saved conversation pane {card} needs exact conversation recovery."
            ));
        }
        failures.push("Source conversation has no qualified immutable migration.".into());
        let message = failures.finish().unwrap_err();
        assert!(message.contains("2 saved-state problem(s)"), "{message}");
        assert_eq!(message.matches(&card.to_string()).count(), 1, "{message}");
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
        assert_eq!(unreadable.classify(&card), SavedExecution::Unproved);

        // Only a retained Claude pane has an owner roster; an incomplete proof
        // is not the absence of one.
        let mut codex = card.clone();
        codex.launch.provider = Provider::Codex;
        assert_eq!(retired.classify(&codex), SavedExecution::Unproved);
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
