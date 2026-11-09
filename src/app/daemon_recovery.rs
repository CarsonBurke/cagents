use std::{path::Path, time::Duration};

use crate::{
    control::ControlClient,
    model::{HostSessionProof, TerminalSession},
    terminal::ensure_daemon_ready,
    upgrade::{ensure_no_surviving_process, exact_conversation},
};

pub(super) struct RecoveredTerminal {
    /// An authenticated held terminal may have completed while orphaned. Attach
    /// its final grid and exit status instead of launching its recipe again.
    pub(super) attach_completed: bool,
    pub(super) session: TerminalSession,
    pub(super) expected_host: Option<HostSessionProof>,
    pub(super) recovery_lease: Option<crate::daemon::RecoveryLease>,
}

/// Match the work represented by two terminal records within one proven
/// recovery lineage. An agent needs an exact conversation; a plain terminal
/// needs the same launch recipe. Conflicting direct and hinted agent IDs are
/// never evidence for attachment.
pub(super) fn same_recovery_content(saved: &TerminalSession, hosted: &TerminalSession) -> bool {
    let identity = |session: &TerminalSession| {
        let direct = exact_conversation(&session.launch);
        let resume = session
            .agent_resume_recipe()
            .as_ref()
            .and_then(exact_conversation);
        if direct.is_some() && resume.is_some() && direct != resume {
            return Err(());
        }
        Ok(direct.or(resume))
    };
    match (identity(saved), identity(hosted)) {
        (Ok(Some(saved)), Ok(Some(hosted))) => saved == hosted,
        (Ok(None), Ok(None)) => {
            saved.resume_hint.is_none()
                && hosted.resume_hint.is_none()
                && crate::agent::Provider::from_program(&saved.launch.program).is_none()
                && crate::agent::Provider::from_program(&hosted.launch.program).is_none()
                && saved.launch == hosted.launch
        }
        _ => false,
    }
}

/// Discovery is separate from attachment: an existing handle never changes its
/// proof or replays buffered input into the replacement terminal.
pub(super) fn reconcile(
    session: TerminalSession,
    explicit_retry: bool,
) -> Result<RecoveredTerminal, String> {
    if session.continuity.live_proof().is_none() {
        return Ok(RecoveredTerminal {
            attach_completed: false,
            session,
            expected_host: None,
            recovery_lease: None,
        });
    }
    let socket = ensure_daemon_ready().map_err(|error| error.to_string())?;
    let id = session.id;
    reconcile_with(
        session,
        explicit_retry,
        &socket,
        Path::new("/proc"),
        holder_of_kind,
    )
    .inspect_err(|error| {
        tracing::warn!(terminal = %id, explicit_retry, %error, "terminal recovery refused");
    })
}

/// Resolve saved-pane ownership at its captured endpoint without starting a daemon.
/// The caller may attach the result, but must not execute a returned replay plan.
pub(super) fn inspect_saved_at(
    session: TerminalSession,
    socket: &Path,
) -> Result<RecoveredTerminal, String> {
    reconcile_with(session, false, socket, Path::new("/proc"), holder_of_kind)
}

fn reconcile_with(
    session: TerminalSession,
    explicit_retry: bool,
    socket: &Path,
    proc_root: &Path,
    holder: impl Fn(
        crate::model::TerminalId,
        crate::daemon::holder_registry::Kind,
    ) -> Result<Option<i32>, String>,
) -> Result<RecoveredTerminal, String> {
    let expected = session
        .continuity
        .live_proof()
        .ok_or("terminal has no live proof")?;
    if !expected.is_complete() {
        return Err("terminal ownership proof is incomplete; automatic recovery is unsafe".into());
    }
    // Hold one runtime-wide lease from fresh discovery through successful attachment.
    // A second card or UI must observe the first recovery's newly hosted conversation.
    let recovery_lease = Some(
        crate::daemon::RecoveryLease::acquire(socket, Duration::from_secs(5))
            .map_err(|error| format!("cannot serialize terminal recovery: {error}"))?,
    );
    let mut client = ControlClient::connect(socket).map_err(|error| error.to_string())?;
    let proof = client.proof().clone();
    let terminals = client.list().map_err(|error| error.to_string())?;
    let original = terminals.iter().find(|terminal| {
        terminal.session.id == session.id && terminal.session.launch == session.launch
    });
    if proof == expected {
        if let Some(terminal) = original
            && terminal.session.continuity.can_resume_live(&expected)
        {
            return Ok(RecoveredTerminal {
                attach_completed: false,
                session: terminal.session.clone(),
                expected_host: Some(expected),
                recovery_lease,
            });
        }
    }
    if proof != expected
        && let Some(restored) = terminals.iter().find(|terminal| {
            terminal.upgrade_sources.iter().any(|source| {
                source.id == session.id
                    && source.launch == session.launch
                    && source.continuity.live_proof().as_ref() == Some(&expected)
            }) && (terminal.session.continuity.can_resume_live(&proof)
                || matches!(
                    &terminal.session.continuity,
                    crate::model::TerminalContinuity::RelaunchAvailable { .. }
                ))
        })
    {
        crate::daemon::replacement_owner(socket, &proof)
            .map_err(|error| format!("cannot establish upgraded daemon ownership: {error}"))?;
        // A committed checkpoint or authenticated holder receipt authorizes
        // this exact lineage. Ordinary ID coincidence remains forbidden.
        return Ok(RecoveredTerminal {
            attach_completed: !restored.session.continuity.can_resume_live(&proof),
            session: restored.session.clone(),
            expected_host: Some(proof),
            recovery_lease,
        });
    }
    if proof != expected
        && terminals
            .iter()
            .any(|terminal| terminal.session.id == session.id)
    {
        return Err("a different daemon owns this terminal ID; preserving that session instead of retargeting it".into());
    }
    // Recovery can span more than one daemon replacement. Upgrade receipts
    // describe only the latest handoff, while the stable recovery origin still
    // identifies the saved card's process lineage. Adopt a live descendant
    // only when its exact conversation or plain terminal recipe agrees too.
    let origin = session.recovery_origin.unwrap_or(session.id);
    let mut descendants = terminals.iter().filter(|terminal| {
        terminal.session.recovery_origin == Some(origin)
            && terminal.session.continuity.can_resume_live(&proof)
            && same_recovery_content(&session, &terminal.session)
    });
    if let Some(restored) = descendants.next() {
        if descendants.next().is_some() {
            return Err("multiple live terminals claim this saved conversation".into());
        }
        crate::daemon::replacement_owner(socket, &proof)
            .map_err(|error| format!("cannot establish upgraded daemon ownership: {error}"))?;
        return Ok(RecoveredTerminal {
            attach_completed: false,
            session: restored.session.clone(),
            expected_host: Some(proof),
            recovery_lease,
        });
    }
    if terminals.iter().any(|terminal| {
        terminal.session.recovery_origin == Some(origin)
            && terminal.session.continuity.can_resume_live(&proof)
    }) {
        return Err("a live terminal shares this recovery lineage but its command or conversation differs; preserving it".into());
    }
    // An earlier recovery may already have replaced this terminal in the
    // same daemon. Discover that authenticated live descendant before offering
    // replay of the completed original; replay would duplicate its process.
    if proof == expected {
        let terminal = original.ok_or(
            "the original daemon is live but no matching terminal exists; relaunch is unsafe",
        )?;
        if !matches!(
            terminal.session.continuity,
            crate::model::TerminalContinuity::RelaunchAvailable { .. }
        ) {
            return Err(
                "the original terminal's exit is not established; preserving its identity".into(),
            );
        }
        if !explicit_retry {
            return Err(
                "the terminal exited while disconnected; use Retry to launch it again".into(),
            );
        }
    }
    let owner = crate::daemon::replacement_owner(socket, &proof)
        .map_err(|error| format!("cannot establish replacement daemon ownership: {error}"))?;
    // Explicit replay changes only the recipe policy, never the ownership evidence.
    let plan = crate::upgrade::recovery_plan(&session);
    let planned_recipe = plan
        .as_ref()
        .map_or_else(|_| session.launch.clone(), |(recipe, _)| recipe.clone());
    // A lost Spawn reply is not evidence that Spawn failed. Use one stable
    // candidate per source/daemon and discover it before authorizing a replay.
    let mut replacement_id = recovery_terminal_id(session.id, &proof);
    while let Some(candidate) = terminals
        .iter()
        .find(|terminal| terminal.session.id == replacement_id)
    {
        if candidate.session.launch != planned_recipe {
            return Err(
                "the pending recovery identity belongs to a different command; preserving it"
                    .into(),
            );
        }
        if candidate.session.continuity.can_resume_live(&proof) {
            return Ok(RecoveredTerminal {
                attach_completed: false,
                session: candidate.session.clone(),
                expected_host: Some(proof),
                recovery_lease,
            });
        }
        if !explicit_retry
            || !matches!(
                candidate.session.continuity,
                crate::model::TerminalContinuity::RelaunchAvailable { .. }
            )
        {
            return Err("the recovered command already ran or its state is uncertain; use Retry only after checking its result".into());
        }
        replacement_id = recovery_terminal_id(replacement_id, &proof);
    }
    if session
        .agent_resume_recipe()
        .as_ref()
        .and_then(exact_conversation)
        .is_some_and(|identity| {
            terminals.iter().any(|terminal| {
                terminal.session.continuity.can_resume_live(&proof)
                    && terminal
                        .session
                        .agent_resume_recipe()
                        .as_ref()
                        .and_then(exact_conversation)
                        == Some(identity)
            })
        })
    {
        return Err("the saved agent conversation is already hosted by another terminal; preserving the live session".into());
    }
    // A holder doesn't carry the terminal's markers, so the scan below can't see
    // one. Only a terminal the daemon saw exit keeps a holder with nothing running
    // in it, there to keep the output. A terminal handed over lists without an
    // exit and may still be running. A native provider can outlive its terminal.
    use crate::daemon::holder_registry::Kind;
    for terminal in [session.id, replacement_id] {
        let exited = terminals.iter().any(|listed| {
            listed.session.id == terminal
                && matches!(
                    listed.session.continuity,
                    crate::model::TerminalContinuity::RelaunchAvailable {
                        last_exit: Some(_),
                        ..
                    }
                )
        });
        for kind in [Kind::Terminal, Kind::Native] {
            if exited && kind == Kind::Terminal {
                continue;
            }
            if let Some(pid) = holder(terminal, kind).map_err(|error| {
                tracing::warn!(%error, %terminal, "could not read the holder record for Retry");
                unfinished_check_refusal()
            })? {
                return Err(format!(
                    "This terminal may still be running in process {pid}, which the daemon isn't showing, {NOTHING_STARTED}: a second copy would run beside it. If you no longer need what it was running, stop that process, then use Retry or Close again. Otherwise leave the pane in place."
                ));
            }
        }
    }
    if let Some(survivor) = crate::upgrade::surviving_process(&session, owner, proc_root)
        .map_err(|error| {
            tracing::warn!(%error, terminal = %session.id, "could not finish the process check for Retry");
            unfinished_check_refusal()
        })?
    {
        return Err(survivor_refusal(
            &survivor,
            NOTHING_STARTED,
            "use Retry or Close again",
        ));
    }
    // Recheck the lease after process inspection. If discovery raced replacement,
    // do not authorize a launch against either incarnation.
    if crate::daemon::replacement_owner(socket, &proof)
        .map_err(|error| format!("daemon ownership changed during recovery: {error}"))?
        != owner
    {
        return Err("daemon ownership changed during recovery".into());
    }
    let (recipe, kind) = match plan {
        Ok((recipe, kind)) => (recipe, kind.label()),
        Err(_) if explicit_retry => (session.launch.clone(), "explicit replay"),
        Err(error) => return Err(error),
    };
    tracing::info!(
        terminal = %session.id,
        replacement = %replacement_id,
        program = %recipe.program,
        cwd = %recipe.working_directory.display(),
        recovery = kind,
        "relaunching a terminal whose daemon is gone"
    );
    let mut replacement = super::fresh_terminal_relaunch(&session);
    replacement.id = replacement_id;
    replacement.recovery_origin = Some(session.recovery_origin.unwrap_or(session.id));
    replacement.launch = recipe.clone();
    replacement.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
        recipe,
        last_exit: None,
    };
    Ok(RecoveredTerminal {
        attach_completed: false,
        session: replacement,
        expected_host: Some(proof),
        recovery_lease,
    })
}

/// Decides whether Close may retire a saved pane whose terminal is gone, without
/// starting or attaching to anything. `Err` is the reason to show the person,
/// and always means the pane stays: unknown is not dead.
///
/// It stops nothing either, with one exception: a pane that is an open agent's
/// own conversation, which the daemon no longer hosts, is closed with that
/// agent as the conversation's own Close would, and only once every check has
/// let the pane go.
pub(super) fn prove_not_hosted(session: &TerminalSession) -> Result<(), String> {
    // A remote or agent-scoped window sees neither this daemon's lease nor the
    // processes behind it, so nothing it can read here would prove an absence.
    if crate::device::is_remote()
        || crate::agent_execution::Authority::from_environment()
            .token()
            .is_some()
    {
        return Err(
            "This window can't check whether the terminal is still running, so the pane was kept. Close it from a local Cagents window."
                .into(),
        );
    }
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|_| UNREACHABLE)?;
    prove_not_hosted_at(session, &socket, Path::new("/proc"), registered_holder)
}

const UNREACHABLE: &str = "Cagents can't reach its daemon, so this terminal may still be running. The pane was kept. Close it again once the daemon is back.";
const STILL_HOSTED: &str = "The daemon still has this terminal, so the pane was kept. Use Retry or Reconnect on the pane to use it.";
const CONTINUED: &str = "A running terminal continues this one, so the pane was kept. Use Retry or Reconnect on the pane if it offers them, or stop that terminal and close the pane again.";
/// Retry holds the lease through attachment, so waiting longer than this would
/// leave the pane looking unresponsive to a click on Close.
const LEASE_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(5)
};

fn holder_of_kind(
    terminal: crate::model::TerminalId,
    kind: crate::daemon::holder_registry::Kind,
) -> Result<Option<i32>, String> {
    crate::daemon::holder_registry::holding_process(terminal, kind)
        .map_err(|error| error.to_string())
}

/// A holder that this daemon didn't adopt is alive, and unlisted, by design.
fn registered_holder(terminal: crate::model::TerminalId) -> Result<Option<i32>, String> {
    use crate::daemon::holder_registry::Kind;
    for kind in [Kind::Terminal, Kind::Native] {
        if let Some(pid) = holder_of_kind(terminal, kind)? {
            return Ok(Some(pid));
        }
    }
    Ok(None)
}

/// Retry refuses too, so it isn't a way out of this one.
fn unadopted_refusal(pid: i32) -> String {
    format!(
        "This terminal may still be running in process {pid}, which the daemon isn't showing, so the pane was kept. Retry can't reach that process. If you no longer need what it was running, stop it, then close the pane again. Otherwise leave the pane in place."
    )
}

fn prove_not_hosted_at(
    session: &TerminalSession,
    socket: &Path,
    proc_root: &Path,
    holder: impl FnOnce(crate::model::TerminalId) -> Result<Option<i32>, String>,
) -> Result<(), String> {
    session
        .continuity
        .live_proof()
        .filter(HostSessionProof::is_complete)
        .ok_or("This pane's saved identity is incomplete, so Cagents can't confirm its terminal has stopped. The pane was kept, and there's no way to close it yet.")?;
    // Retry holds this from discovery through attachment, so an absence read
    // here cannot be a replacement another window is starting.
    let _lease = crate::daemon::RecoveryLease::acquire(socket, LEASE_WAIT).map_err(|error| {
        match error {
            crate::daemon::PlatformError::Io(error)
                if error.kind() == std::io::ErrorKind::TimedOut =>
            {
                "Another terminal is being recovered, so the pane was kept. Close it again in a moment."
            }
            _ => UNREACHABLE,
        }
    })?;
    let mut client = ControlClient::connect(socket).map_err(|_| UNREACHABLE)?;
    let current = client.proof().clone();
    if !current.is_complete() {
        return Err(UNREACHABLE.into());
    }
    // A daemon stuck in startup recovery still answers inventory reads, and that
    // inventory can be partial. This request is refused until it admits work.
    client.daemon_version().map_err(|_| UNREACHABLE)?;
    let terminals = client.list().map_err(|_| UNREACHABLE)?;
    let chats = client.list_chats().map_err(|_| UNREACHABLE)?;
    let model = client.read_workspace_model().map_err(|_| UNREACHABLE)?;
    if terminals
        .iter()
        .any(|hosted| hosted.session.id == session.id)
    {
        return Err(STILL_HOSTED.into());
    }
    let origin = session.recovery_origin.unwrap_or(session.id);
    if terminals.iter().any(|hosted| {
        hosted
            .upgrade_sources
            .iter()
            .any(|source| source.id == session.id)
            || (hosted.session.recovery_origin == Some(origin)
                && hosted.session.continuity.can_resume_live(&current))
    }) {
        return Err(CONTINUED.into());
    }
    if chats.iter().any(|chat| {
        chat.id == session.id
            || chat
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.id == session.id)
    }) {
        return Err("A conversation still uses this terminal, so the pane was kept. Close that conversation, then close this pane.".into());
    }
    // Removing the pane would leave an open agent with no way to show it,
    // unless the pane is that agent's own conversation, whose provider is gone
    // from the daemon: closing it is then a person closing that conversation,
    // which ends its agent. It is closed last, once nothing below has kept the
    // pane, so a refusal never leaves the agent closed with its pane still open.
    let mut agent_conversation = None;
    if let Some(agent) = model
        .agents
        .iter()
        .find(|agent| !agent.closed && agent_uses_terminal(agent, session.id))
    {
        let Some(conversation) = lost_conversation_of_pane(agent, session.id, &chats) else {
            return Err(open_agent_refusal(agent, &chats, &current.boot_id));
        };
        agent_conversation = Some(conversation.clone());
    }
    // Authenticate the answer: reaching a socket that reports another identity
    // is not evidence that the daemon holding the runtime's lease answered.
    let owner = crate::daemon::replacement_owner(socket, &current).map_err(|_| UNREACHABLE)?;
    let cannot_confirm = || {
        "Cagents can't confirm this terminal has stopped, so the pane was kept. Try Close again in a moment. If this keeps happening, leave the pane in place.".to_owned()
    };
    if let Some(pid) = holder(session.id).map_err(|error| {
        tracing::warn!(%error, terminal = %session.id, "could not read the holder record for Close");
        cannot_confirm()
    })? {
        return Err(unadopted_refusal(pid));
    }
    // Check even when the saved boot is over. No process outlives its boot, but
    // a replacement started on this one carries the saved terminal's lineage
    // and stays unlisted if its daemon failed to adopt it.
    if let Some(survivor) = crate::upgrade::surviving_process(session, owner, proc_root)
        .map_err(|error| {
            tracing::warn!(%error, terminal = %session.id, "could not finish the process check for Close");
            cannot_confirm()
        })?
    {
        return Err(survivor_refusal(
            &survivor,
            "so the pane was kept",
            "close the pane again",
        ));
    }
    if crate::daemon::replacement_owner(socket, &current).map_err(|_| UNREACHABLE)? != owner {
        return Err(UNREACHABLE.into());
    }
    if let Some(conversation) = agent_conversation {
        // The daemon settles it only once nothing of it can still run.
        client
            .close_retained_chat(conversation)
            .map_err(|error| lost_agent_refusal(&error.reason()))?;
        let model = client.read_workspace_model().map_err(|_| UNREACHABLE)?;
        if let Some(agent) = model
            .agents
            .iter()
            .find(|agent| !agent.closed && agent_uses_terminal(agent, session.id))
        {
            return Err(open_agent_refusal(agent, &chats, &current.boot_id));
        }
    }
    Ok(())
}

const NOTHING_STARTED: &str = "so nothing was started";

fn unfinished_check_refusal() -> String {
    format!(
        "Cagents couldn't check every running process, {NOTHING_STARTED}: this terminal may still be running. Try Retry again in a moment. If this keeps happening, leave the pane in place."
    )
}

/// A refusal written for the reader. Any other start error is a raw backend
/// string, which a failed pane doesn't show or copy.
pub(super) fn is_retry_refusal(error: &str) -> bool {
    error.contains(NOTHING_STARTED)
}

fn survivor_refusal(
    survivor: &crate::upgrade::SurvivingProcess,
    outcome: &str,
    next: &str,
) -> String {
    use crate::upgrade::SurvivorLink;
    let process = match &survivor.name {
        Some(name) => format!("{name} (PID {})", survivor.pid),
        None => format!("A process (PID {})", survivor.pid),
    };
    match &survivor.link {
        SurvivorLink::Lineage => format!(
            "{process}, started by this terminal, is still running, {outcome}. Stop it, then {next}."
        ),
        SurvivorLink::Conversation => format!(
            "{process} is still running this terminal's conversation, {outcome}. Stop it, then {next}."
        ),
        SurvivorLink::Unit(unit) => format!(
            "{process} is running in {unit}, the systemd unit this terminal was launched in, {outcome}. If it came from this terminal, stop it, then {next}. Otherwise the pane stays until that process exits."
        ),
    }
}

/// The conversation of an open agent that this pane is, when the daemon no
/// longer hosts it: what the pane's Close closes, as the conversation's own
/// Close would. A scheduled agent is its automation's to end.
fn lost_conversation_of_pane<'a>(
    agent: &'a crate::agent_execution::Agent,
    terminal: crate::model::TerminalId,
    hosted: &[crate::chat::Session],
) -> Option<&'a crate::chat::Session> {
    use crate::agent_execution::{HostedSession, Origin};
    match &agent.session {
        Some(HostedSession::Chat(chat))
            if chat.id == terminal
                && !matches!(agent.origin, Origin::Automation { .. })
                && !hosted.iter().any(|hosted| hosted.id == chat.id) =>
        {
            Some(chat)
        }
        _ => None,
    }
}

/// Why closing the agent conversation this pane is was refused, in the
/// daemon's words.
fn lost_agent_refusal(reason: &str) -> String {
    let reason = reason.trim_end_matches('.');
    format!("This pane is an open agent's conversation, which Cagents couldn't close. {reason}.")
}

/// Never names a route the daemon is certain to refuse.
fn open_agent_refusal(
    agent: &crate::agent_execution::Agent,
    hosted: &[crate::chat::Session],
    boot: &crate::model::BootId,
) -> String {
    use crate::agent_execution::Origin;
    if !agent_close_can_release(agent, hosted, boot) {
        return "An open agent still uses this terminal and Cagents can't confirm it has stopped, so the pane was kept. There's no way to close this pane yet.".into();
    }
    // The daemon only lets an agent's owner close it. A person's own shell owns
    // the agents a person started, but a shell in a Cagents terminal counts as
    // that terminal's session and is refused.
    match agent.origin {
        Origin::User => format!(
            "An open agent still uses this terminal, so the pane was kept. Close the agent first by running `cagents agent close {}` in a terminal outside Cagents, then close the pane again.",
            agent.id
        ),
        Origin::Child { .. } | Origin::Automation { .. } => "An open agent started by another session or automation still uses this terminal, so the pane was kept. Only what started it can close it. Close this pane again after that.".into(),
    }
}

/// Whether the daemon has a way to close this agent: something to stop its
/// session with, or a changed kernel boot proving that session has gone. A
/// launch still pending isn't visible from here, and neither is what the daemon
/// checks before relying on the boot, so this can be optimistic.
fn agent_close_can_release(
    agent: &crate::agent_execution::Agent,
    hosted: &[crate::chat::Session],
    boot: &crate::model::BootId,
) -> bool {
    use crate::agent_execution::{HostedSession, Origin};
    match &agent.session {
        None => true,
        Some(HostedSession::Terminal(_)) => false,
        Some(HostedSession::Chat(session)) => {
            session.closes_without_hosted_actor()
                // The daemon closes a retained actor that died without its
                // retirement once nothing of it can still run.
                || matches!(
                    session.native_continuity,
                    crate::chat::NativeContinuity::RetainedClaudeActor { .. }
                )
                || hosted.iter().any(|chat| chat.id == session.id)
                // A scheduled agent is its automation's to end, so the daemon
                // doesn't take this route for one.
                || (!matches!(agent.origin, Origin::Automation { .. })
                    && session
                        .proof
                        .as_ref()
                        .is_some_and(|saved| saved.is_complete() && saved.boot_id != *boot))
        }
    }
}

fn agent_uses_terminal(
    agent: &crate::agent_execution::Agent,
    terminal: crate::model::TerminalId,
) -> bool {
    use crate::agent_execution::HostedSession;
    agent
        .console
        .as_ref()
        .is_some_and(|console| console.id == terminal)
        || match &agent.session {
            Some(HostedSession::Terminal(session)) => session.id == terminal,
            Some(HostedSession::Chat(chat)) => {
                chat.id == terminal
                    || chat
                        .terminal
                        .as_ref()
                        .is_some_and(|source| source.id == terminal)
            }
            None => false,
        }
}

fn recovery_terminal_id(
    source: crate::model::TerminalId,
    proof: &HostSessionProof,
) -> crate::model::TerminalId {
    let mut hash = blake3::Hasher::new_derive_key("cagents terminal recovery identity v1");
    hash.update(source.as_uuid().as_bytes());
    hash.update(&serde_json::to_vec(proof).expect("host proof contains only strings"));
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
    crate::model::TerminalId::from_uuid(uuid::Uuid::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{HostIncarnation, HostSessionId, LaunchRecipe, TerminalContinuity, TerminalId},
        terminal::ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
            ServerResponse, TerminalDescriptor, read_frame, write_frame,
        },
    };
    use std::{
        fs,
        fs::{File, OpenOptions},
        io,
        os::{
            fd::AsRawFd,
            unix::{fs::OpenOptionsExt, net::UnixListener},
        },
        thread,
    };

    fn proof(incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new(incarnation),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }

    fn session(path: &Path, proof: &HostSessionProof) -> TerminalSession {
        TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: TerminalId::new(),
            launch: LaunchRecipe::shell(path.to_owned()).unwrap(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            },
        }
    }

    fn lease(directory: &Path, proof: &HostSessionProof) -> File {
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(directory.join("cagentsd.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        serde_json::to_writer(
            &file,
            &serde_json::json!({
                "pid": std::process::id(), "proof": proof,
            }),
        )
        .unwrap();
        file
    }

    /// A recovery that returns before the connection a test expected must fail
    /// that test, not leave it waiting on the listener.
    fn accept_within(listener: &UnixListener) -> std::os::unix::net::UnixStream {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        listener.set_nonblocking(true).unwrap();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    return stream;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "recovery never made the connection this test expected"
                    );
                    thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        }
    }

    fn discovery(
        socket: &Path,
        proof: HostSessionProof,
        terminals: Vec<TerminalDescriptor>,
        probes: usize,
    ) -> thread::JoinHandle<()> {
        let listener = UnixListener::bind(socket).unwrap();
        thread::spawn(move || {
            let mut stream = accept_within(&listener);
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(
                matches!(connect.request, ClientRequest::Connect { expected_host: None, ref resume } if resume.is_empty())
            );
            write_frame(
                &mut stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(connect.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: Vec::new(),
                        },
                    },
                },
            )
            .unwrap();
            let list: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert_eq!(list.proof, Some(proof.clone()));
            assert_eq!(list.request, ClientRequest::List);
            write_frame(
                &mut stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(list.request_id),
                    proof,
                    message: ServerMessage::Response {
                        response: ServerResponse::Listed { terminals },
                    },
                },
            )
            .unwrap();
            // Recovery is read-only: no Spawn, Attach, Shutdown, or Input reaches
            // this connection. A newly returned plan must use a fresh handle.
            assert_eq!(read_frame::<ClientEnvelope>(&mut stream).unwrap(), None);
            for _ in 0..probes {
                accept_within(&listener);
            }
        })
    }

    #[test]
    fn replaced_daemon_recovers_a_shell_with_fresh_identity_and_pinned_owner() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let original = session(directory.path(), &proof("old"));
        let replacement_proof = proof("replacement");
        let _lease = lease(directory.path(), &replacement_proof);
        let server = discovery(&socket, replacement_proof.clone(), Vec::new(), 2);
        let recovered = reconcile_at(original.clone(), false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_ne!(recovered.session.id, original.id);
        assert_eq!(recovered.session.recovery_origin, Some(original.id));
        assert_eq!(recovered.session.launch, original.launch);
        assert_eq!(recovered.expected_host, Some(replacement_proof));
        assert!(matches!(
            recovered.session.continuity,
            TerminalContinuity::RelaunchAvailable { .. }
        ));
        assert!(matches!(
            crate::daemon::RecoveryLease::acquire(&socket, Duration::ZERO),
            Err(crate::daemon::PlatformError::Io(error)) if error.kind() == io::ErrorKind::TimedOut
        ));
        drop(recovered);
        let _next_recovery =
            crate::daemon::RecoveryLease::acquire(&socket, Duration::ZERO).unwrap();
    }

    #[test]
    fn exact_live_terminal_keeps_identity_without_relaunching() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("live");
        let original = session(directory.path(), &owner);
        let mut descriptor =
            TerminalDescriptor::relaunchable(original.id, original.launch.clone(), None);
        descriptor.session = original.clone();
        let server = discovery(&socket, owner.clone(), vec![descriptor], 0);
        let recovered = reconcile_at(original.clone(), false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session, original);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn same_daemon_adopts_live_recovery_descendant_before_replay() {
        for explicit_retry in [false, true] {
            for retain_original in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let socket = directory.path().join("daemon.sock");
                let owner = proof("live");
                let original = session(directory.path(), &owner);
                let mut descendant = session(directory.path(), &owner);
                descendant.recovery_origin = Some(original.id);
                let mut descriptor = TerminalDescriptor::relaunchable(
                    descendant.id,
                    descendant.launch.clone(),
                    None,
                );
                descriptor.session = descendant.clone();
                let mut terminals = vec![descriptor];
                if retain_original {
                    terminals.push(TerminalDescriptor::relaunchable(
                        original.id,
                        original.launch.clone(),
                        None,
                    ));
                }
                let _lease = lease(directory.path(), &owner);
                let server = discovery(&socket, owner.clone(), terminals, 1);
                let recovered =
                    reconcile_at(original, explicit_retry, &socket, directory.path()).unwrap();
                server.join().unwrap();
                assert_eq!(recovered.session, descendant);
                assert_eq!(recovered.expected_host, Some(owner));
                assert!(!recovered.attach_completed);
            }
        }
    }

    #[test]
    fn same_daemon_recovery_rejects_conflicting_live_descendants() {
        for ambiguous in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("daemon.sock");
            let owner = proof("live");
            let original = session(directory.path(), &owner);
            let mut descendant = session(directory.path(), &owner);
            descendant.recovery_origin = Some(original.id);
            if !ambiguous {
                descendant
                    .launch
                    .arguments
                    .push("--different-command".into());
            }
            let mut descriptor =
                TerminalDescriptor::relaunchable(descendant.id, descendant.launch.clone(), None);
            descriptor.session = descendant;
            let mut terminals = vec![descriptor.clone()];
            if ambiguous {
                descriptor.session.id = crate::model::TerminalId::new();
                terminals.push(descriptor);
            }
            let server = discovery(&socket, owner, terminals, 0);
            let error = reconcile_at(original, true, &socket, directory.path())
                .err()
                .unwrap();
            server.join().unwrap();
            assert!(error.contains(if ambiguous {
                "multiple live terminals"
            } else {
                "command or conversation differs"
            }));
        }
    }

    #[test]
    fn committed_upgrade_lineage_adopts_restored_process_without_replaying() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("upgraded");
        let original = session(directory.path(), &proof("old"));
        let _lease = lease(directory.path(), &owner);
        let mut restored = session(directory.path(), &owner);
        restored.id = original.id;
        restored.launch.working_directory = directory.path().join("current-directory");
        let mut descriptor =
            TerminalDescriptor::relaunchable(restored.id, restored.launch.clone(), None);
        descriptor.session = restored.clone();
        let mut intermediate = original.clone();
        intermediate.continuity = session(directory.path(), &proof("intermediate")).continuity;
        descriptor.upgrade_sources = vec![original.clone(), intermediate];
        let server = discovery(&socket, owner.clone(), vec![descriptor], 1);
        let recovered = reconcile_at(original, false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session, restored);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn completed_holder_lineage_attaches_the_result_instead_of_replaying_the_recipe() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("replacement");
        let original = session(directory.path(), &proof("old"));
        let _lease = lease(directory.path(), &owner);
        let mut descriptor =
            TerminalDescriptor::relaunchable(original.id, original.launch.clone(), None);
        descriptor.upgrade_sources = vec![original.clone()];
        // The final worker grid may precede the asynchronously delivered exit
        // event. Its completed descriptor still authorizes attachment only.
        let server = discovery(&socket, owner.clone(), vec![descriptor], 1);
        let recovered = reconcile_at(original, false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert!(recovered.attach_completed);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn saved_agent_adopts_a_live_descendant_after_multiple_handoffs() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("latest");
        let mut original = session(directory.path(), &proof("old"));
        original.launch.program = "omp".into();
        original.launch.arguments = vec![
            "--resume".into(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        ];
        let mut descendant = session(directory.path(), &owner);
        descendant.recovery_origin = Some(original.id);
        descendant.launch = original.launch.clone();
        let mut descriptor =
            TerminalDescriptor::relaunchable(descendant.id, descendant.launch.clone(), None);
        descriptor.session = descendant.clone();
        // The latest upgrade receipt knows only its immediate predecessor.
        descriptor.upgrade_sources = vec![session(directory.path(), &proof("intermediate"))];
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner.clone(), vec![descriptor], 1);
        let recovered = reconcile_at(original, false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session, descendant);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn saved_shell_adopts_a_live_descendant_after_multiple_handoffs() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("latest");
        let original = session(directory.path(), &proof("old"));
        let mut descendant = session(directory.path(), &owner);
        descendant.recovery_origin = Some(original.id);
        let mut descriptor =
            TerminalDescriptor::relaunchable(descendant.id, descendant.launch.clone(), None);
        descriptor.session = descendant.clone();
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner.clone(), vec![descriptor], 1);
        let recovered = reconcile_at(original, false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session, descendant);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn generic_agent_and_ambiguous_shell_hint_do_not_identify_a_descendant() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = session(directory.path(), &proof("old"));
        let mut hosted = session(directory.path(), &proof("latest"));
        hosted.recovery_origin = Some(saved.id);
        saved.launch.program = "claude".into();
        hosted.launch = saved.launch.clone();
        assert!(!same_recovery_content(&saved, &hosted));

        saved.launch.program = "/bin/sh".into();
        hosted.launch = saved.launch.clone();
        saved.resume_hint = Some("unconfirmed agent identity".into());
        assert!(!same_recovery_content(&saved, &hosted));
    }

    #[test]
    fn recovery_origin_without_the_exact_conversation_cannot_retarget_an_agent() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("latest");
        let mut original = session(directory.path(), &proof("old"));
        original.launch.program = "omp".into();
        original.launch.arguments = vec![
            "--resume".into(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        ];
        let mut descendant = session(directory.path(), &owner);
        descendant.recovery_origin = Some(original.id);
        descendant.launch.program = "omp".into();
        descendant.launch.arguments = vec![
            "--resume".into(),
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into(),
        ];
        let mut descriptor =
            TerminalDescriptor::relaunchable(descendant.id, descendant.launch.clone(), None);
        descriptor.session = descendant;
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner, vec![descriptor], 0);
        let result = reconcile_at(original, false, &socket, directory.path());
        server.join().unwrap();
        assert!(result.err().unwrap().contains("conversation differs"));
    }

    #[test]
    fn upgrade_id_without_matching_source_proof_cannot_retarget_card() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("upgraded");
        let original = session(directory.path(), &proof("old"));
        let mut restored = session(directory.path(), &owner);
        restored.id = original.id;
        let mut wrong_source = session(directory.path(), &proof("unrelated"));
        wrong_source.id = original.id;
        let mut descriptor =
            TerminalDescriptor::relaunchable(restored.id, restored.launch.clone(), None);
        descriptor.session = restored;
        descriptor.upgrade_sources = vec![wrong_source];
        let server = discovery(&socket, owner, vec![descriptor], 0);
        assert!(reconcile_at(original, false, &socket, directory.path()).is_err());
        server.join().unwrap();
    }

    #[test]
    fn lost_spawn_reply_rediscovers_the_same_recovery_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("replacement");
        let original = session(directory.path(), &proof("old"));
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner.clone(), Vec::new(), 2);
        let planned = reconcile_at(original.clone(), false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        let mut running = session(directory.path(), &owner);
        running.id = planned.session.id;
        drop(planned);
        fs::remove_file(&socket).unwrap();
        let mut descriptor =
            TerminalDescriptor::relaunchable(running.id, running.launch.clone(), None);
        descriptor.session = running.clone();
        let server = discovery(&socket, owner.clone(), vec![descriptor], 1);
        let recovered = reconcile_at(original, false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session, running);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn agent_without_exact_identity_reopens_its_resume_picker_not_its_shell() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("replacement");
        let mut original = session(directory.path(), &proof("old"));
        // The shell's argv names nothing; the observation saw Claude inside it
        // without establishing which conversation it held.
        let agent = LaunchRecipe {
            unset_environment: Default::default(),
            program: "claude".into(),
            arguments: vec![
                "--model".into(),
                "sonnet".into(),
                "--resume".into(),
                "12345678-1234-4234-8234-123456789abc".into(),
            ],
            working_directory: directory.path().to_owned(),
            environment: Default::default(),
        };
        original.resume_hint =
            Some(serde_json::json!({ "version": 2, "recipe": agent }).to_string());
        assert!(original.agent_resume_recipe().is_none());
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner.clone(), Vec::new(), 2);
        let recovered = reconcile_at(original.clone(), false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session.launch.program, "claude");
        assert_eq!(
            recovered.session.launch.arguments,
            ["--model", "sonnet", "--resume"]
        );
        assert_eq!(recovered.session.resume_hint, None);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn authenticated_exit_allows_only_explicit_relaunch() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("same-owner");
        let original = session(directory.path(), &owner);
        let _lease = lease(directory.path(), &owner);
        let descriptor =
            TerminalDescriptor::relaunchable(original.id, original.launch.clone(), None);
        let server = discovery(&socket, owner.clone(), vec![descriptor.clone()], 0);
        assert!(reconcile_at(original.clone(), false, &socket, directory.path()).is_err());
        server.join().unwrap();
        fs::remove_file(&socket).unwrap();
        let server = discovery(&socket, owner.clone(), vec![descriptor], 2);
        let recovered = reconcile_at(original.clone(), true, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_ne!(recovered.session.id, original.id);
        assert_eq!(recovered.session.launch, original.launch);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn replacement_cannot_adopt_a_matching_id_from_another_owner() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let original = session(directory.path(), &proof("old"));
        let mut descriptor =
            TerminalDescriptor::relaunchable(original.id, original.launch.clone(), None);
        descriptor.session = session(directory.path(), &proof("replacement"));
        descriptor.session.id = original.id;
        let server = discovery(&socket, proof("replacement"), vec![descriptor], 0);
        assert!(reconcile_at(original, false, &socket, directory.path()).is_err());
        server.join().unwrap();
    }

    #[test]
    fn socket_response_without_exclusive_ownership_cannot_authorize_relaunch() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let original = session(directory.path(), &proof("old"));
        let server = discovery(&socket, proof("replacement"), Vec::new(), 0);
        assert!(reconcile_at(original, false, &socket, directory.path()).is_err());
        server.join().unwrap();
    }

    #[test]
    fn side_effecting_commands_require_explicit_retry() {
        let directory = tempfile::tempdir().unwrap();
        let mut original = session(directory.path(), &proof("old"));
        original.launch.arguments = vec!["-c".into(), "touch important-file".into()];
        assert!(crate::upgrade::recovery_plan(&original).is_err());
    }

    #[test]
    fn explicit_retry_replays_command_only_after_replacement_is_authenticated() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let mut original = session(directory.path(), &proof("old"));
        original.launch.arguments = vec!["-c".into(), "touch important-file".into()];
        let replacement_proof = proof("replacement");
        let _lease = lease(directory.path(), &replacement_proof);
        let server = discovery(&socket, replacement_proof.clone(), Vec::new(), 2);
        let recovered = reconcile_at(original.clone(), true, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session.launch, original.launch);
        assert_ne!(recovered.session.id, original.id);
        assert_eq!(recovered.expected_host, Some(replacement_proof));
    }

    /// Close points away from Retry here because Retry would start a second
    /// copy beside the process it can't see.
    #[test]
    fn retry_starts_nothing_beside_a_live_holder() {
        // The last is a replacement whose spawn reply was lost, with a holder of
        // its own.
        let cases: [fn(bool) -> Result<Option<i32>, String>; 3] = [
            |original| Ok(original.then_some(4242)),
            |original| {
                if original {
                    Err("the record can't be read".to_owned())
                } else {
                    Ok(None)
                }
            },
            |original| Ok((!original).then_some(4242)),
        ];
        for holder in cases {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("daemon.sock");
            let original = session(directory.path(), &proof("old"));
            let id = original.id;
            let replacement_proof = proof("replacement");
            let _lease = lease(directory.path(), &replacement_proof);
            let server = discovery(&socket, replacement_proof, Vec::new(), 1);
            let result =
                reconcile_with(original, true, &socket, directory.path(), |terminal, _| {
                    holder(terminal == id)
                });
            server.join().unwrap();
            assert!(result.is_err());
        }
    }

    /// A finished terminal's holder stays up to keep its output. The daemon lists
    /// that terminal as ready to run again, so its holder isn't a hidden process.
    #[test]
    fn retry_restarts_a_finished_terminal_the_daemon_lists_despite_its_holder() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("live");
        let original = session(directory.path(), &owner);
        let id = original.id;
        let listed = exited(id, &original.launch);
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner, vec![listed], 2);
        let recovered = reconcile_with(
            original,
            true,
            &socket,
            directory.path(),
            |terminal, kind| {
                Ok(
                    (terminal == id && kind == crate::daemon::holder_registry::Kind::Terminal)
                        .then_some(4242),
                )
            },
        );
        // Checked first: a refusal never makes the probes the server waits for.
        let recovered = recovered.unwrap();
        server.join().unwrap();
        assert_ne!(recovered.session.id, id);
    }

    /// A terminal handed over to another owner lists as ready to run again with
    /// no recorded exit, while its process is still running.
    #[test]
    fn retry_refuses_beside_the_holder_of_a_terminal_with_no_recorded_exit() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("live");
        let original = session(directory.path(), &owner);
        let id = original.id;
        let listed = TerminalDescriptor::relaunchable(id, original.launch.clone(), None);
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner, vec![listed], 1);
        let recovered = reconcile_with(original, true, &socket, directory.path(), |terminal, _| {
            Ok((terminal == id).then_some(4242))
        });
        server.join().unwrap();
        let Err(reason) = recovered else {
            panic!("nothing should start beside the holder");
        };
        assert!(reason.contains("process 4242"), "{reason}");
    }

    /// An unreadable holder record is unknown, and its OS error isn't for the reader.
    #[test]
    fn retry_says_plainly_when_it_cannot_read_a_holder_record() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let original = session(directory.path(), &proof("old"));
        let replacement_proof = proof("replacement");
        let _lease = lease(directory.path(), &replacement_proof);
        let server = discovery(&socket, replacement_proof, Vec::new(), 1);
        let recovered = reconcile_with(original, true, &socket, directory.path(), |_, _| {
            Err("Permission denied (os error 13)".to_owned())
        });
        server.join().unwrap();
        let Err(reason) = recovered else {
            panic!("an unreadable record is not an absent one");
        };
        assert!(is_retry_refusal(&reason), "{reason}");
        assert!(!reason.contains("os error"), "{reason}");
    }

    /// A completed terminal can still own a native provider, under the same ID.
    #[test]
    fn retry_refuses_beside_a_native_holder_of_a_terminal_that_exited() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("live");
        let original = session(directory.path(), &owner);
        let id = original.id;
        let listed = exited(id, &original.launch);
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner, vec![listed], 1);
        let recovered = reconcile_with(
            original,
            true,
            &socket,
            directory.path(),
            |terminal, kind| {
                Ok(
                    (terminal == id && kind == crate::daemon::holder_registry::Kind::Native)
                        .then_some(4242),
                )
            },
        );
        server.join().unwrap();
        let Err(reason) = recovered else {
            panic!("nothing should start beside the holder");
        };
        assert!(reason.contains("process 4242"), "{reason}");
    }

    /// A terminal listed with no exit was handed over and may still be running in
    /// its holder, so only a recorded exit lets Retry pass over that holder.
    #[test]
    fn retry_refuses_beside_the_holder_of_a_terminal_listed_without_an_exit() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("live");
        let original = session(directory.path(), &owner);
        let id = original.id;
        let listed = TerminalDescriptor::relaunchable(id, original.launch.clone(), None);
        assert!(matches!(
            listed.session.continuity,
            TerminalContinuity::RelaunchAvailable {
                last_exit: None,
                ..
            }
        ));
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner, vec![listed], 1);
        let recovered = reconcile_with(
            original,
            true,
            &socket,
            directory.path(),
            |terminal, kind| {
                Ok(
                    (terminal == id && kind == crate::daemon::holder_registry::Kind::Terminal)
                        .then_some(4242),
                )
            },
        );
        server.join().unwrap();
        let Err(reason) = recovered else {
            panic!("nothing should start beside the holder");
        };
        assert!(reason.contains("process 4242"), "{reason}");
    }

    /// The refusal is shown on the pane, so it names the process to stop and not
    /// the identifiers behind it.
    #[test]
    fn retry_refuses_beside_a_marked_process_in_words_a_reader_can_act_on() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let original = session(directory.path(), &proof("old"));
        let id = original.id;
        let process = directory.path().join("42");
        fs::create_dir(&process).unwrap();
        fs::write(process.join("stat"), b"42 (sleep) S 1 42 42").unwrap();
        fs::write(process.join("cmdline"), b"/bin/sleep\0").unwrap();
        fs::write(
            process.join("environ"),
            format!("CAGENTS_TERMINAL_ID={id}\0"),
        )
        .unwrap();
        std::os::unix::fs::symlink(directory.path(), process.join("cwd")).unwrap();
        let replacement_proof = proof("replacement");
        let _lease = lease(directory.path(), &replacement_proof);
        let server = discovery(&socket, replacement_proof, Vec::new(), 1);
        let error = reconcile_at(original, true, &socket, directory.path())
            .err()
            .unwrap();
        server.join().unwrap();
        assert!(is_retry_refusal(&error), "{error}");
        assert!(error.contains("PID 42"), "{error}");
        assert!(!error.contains(&id.to_string()), "{error}");
    }

    /// A process that stays unreadable is unknown, and the reader gets words about
    /// what to do, not the scan's own error.
    #[test]
    fn retry_says_plainly_when_it_cannot_check_every_process() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let original = session(directory.path(), &proof("old"));
        let process = directory.path().join("42");
        fs::create_dir(&process).unwrap();
        fs::write(process.join("stat"), b"42 (sleep) R 1 42 42").unwrap();
        fs::write(process.join("cmdline"), b"").unwrap();
        let replacement_proof = proof("replacement");
        let _lease = lease(directory.path(), &replacement_proof);
        let server = discovery(&socket, replacement_proof, Vec::new(), 1);
        let error = reconcile_at(original, true, &socket, directory.path())
            .err()
            .unwrap();
        server.join().unwrap();
        assert!(is_retry_refusal(&error), "{error}");
        assert!(!error.contains("cannot inspect"), "{error}");
        assert!(!error.contains("42"), "{error}");
    }

    #[test]
    fn explicit_retry_does_not_override_missing_daemon_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let mut original = session(directory.path(), &proof("old"));
        original.launch.arguments = vec!["-c".into(), "touch important-file".into()];
        let server = discovery(&socket, proof("replacement"), Vec::new(), 0);
        let result = reconcile_at(original, true, &socket, directory.path());
        server.join().unwrap();
        assert!(
            result
                .err()
                .unwrap()
                .contains("cannot establish replacement daemon ownership")
        );
    }

    #[test]
    fn explicit_retry_does_not_override_uncertain_process_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let mut original = session(directory.path(), &proof("old"));
        original.launch.arguments = vec!["-c".into(), "touch important-file".into()];
        let replacement_proof = proof("replacement");
        let _lease = lease(directory.path(), &replacement_proof);
        let process = directory.path().join("42");
        fs::create_dir(&process).unwrap();
        fs::write(process.join("stat"), b"unknown process identity").unwrap();
        let server = discovery(&socket, replacement_proof, Vec::new(), 1);
        let result = reconcile_at(original, true, &socket, directory.path());
        server.join().unwrap();
        assert!(
            result
                .err()
                .unwrap()
                .contains("couldn't check every running process")
        );
    }

    #[test]
    fn orphaned_terminal_process_blocks_recovery_without_being_killed() {
        use std::io::Read;
        use std::process::Stdio;

        let directory = tempfile::tempdir().unwrap();
        let original = session(directory.path(), &proof("old"));
        for marker in [
            None,
            Some(recovery_terminal_id(original.id, &proof("intermediate")).to_string()),
        ] {
            let mut command = std::process::Command::new("/bin/sh");
            command
                // Publish readiness from the executed child before scanning its
                // proc files. Keep it alive on an open input pipe: neither a
                // scheduling delay nor the suite's duration ends the fixture.
                .args(["-c", "printf ready; read -r release"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .current_dir(directory.path())
                .env("CAGENTS_RECOVERY_ORIGIN", original.id.to_string())
                .env("TERM_PROGRAM", "cagents");
            if let Some(marker) = marker {
                command.env("CAGENTS_TERMINAL_ID", marker);
            } else {
                command.env_remove("CAGENTS_TERMINAL_ID");
            }
            let mut child = command.spawn().unwrap();
            let mut ready = [0; 5];
            child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"ready");
            std::os::unix::fs::symlink(
                format!("/proc/{}", child.id()),
                directory.path().join(child.id().to_string()),
            )
            .unwrap();
            let result = ensure_no_surviving_process(&original, u32::MAX, directory.path());
            let alive = child.try_wait().unwrap().is_none();
            child.kill().unwrap();
            child.wait().unwrap();
            assert!(result.is_err());
            assert!(alive, "discovery must never signal an existing process");
            fs::remove_file(directory.path().join(child.id().to_string())).unwrap();
        }
    }

    #[test]
    fn incomplete_process_evidence_does_not_authorize_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let pid_directory = directory.path().join("42");
        fs::create_dir(&pid_directory).unwrap();
        fs::write(pid_directory.join("stat"), b"unreadable process identity").unwrap();
        let launch = LaunchRecipe::shell(directory.path().to_owned()).unwrap();
        let session = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: crate::model::TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        };
        assert!(ensure_no_surviving_process(&session, u32::MAX, directory.path()).is_err());
    }

    #[test]
    fn exact_terminal_marker_protects_a_survivor_after_directory_change() {
        let directory = tempfile::tempdir().unwrap();
        let launch = LaunchRecipe::shell(directory.path().join("old-directory")).unwrap();
        let session = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: crate::model::TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        };
        let process = directory.path().join("42");
        fs::create_dir(&process).unwrap();
        fs::write(process.join("stat"), b"42 (sleep) S 1 42 42").unwrap();
        fs::write(process.join("cmdline"), b"/bin/sleep\0").unwrap();
        fs::write(process.join("environ"), b"PATH=/bin\0").unwrap();
        std::os::unix::fs::symlink(directory.path(), process.join("cwd")).unwrap();
        assert!(ensure_no_surviving_process(&session, u32::MAX, directory.path()).is_ok());
        fs::write(
            process.join("environ"),
            format!("CAGENTS_TERMINAL_ID={}\0", session.id),
        )
        .unwrap();
        assert!(ensure_no_surviving_process(&session, u32::MAX, directory.path()).is_err());
    }

    #[test]
    fn distinct_cards_cannot_resume_the_same_hosted_conversation_with_different_flags() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let mut original = session(directory.path(), &proof("old"));
        original.launch.program = "codex".into();
        original.launch.arguments = vec![
            "resume".into(),
            id.into(),
            "--model".into(),
            "old-model".into(),
        ];
        let mut hosted = session(directory.path(), &proof("replacement"));
        hosted.launch.program = "/usr/bin/codex".into();
        hosted.launch.arguments = vec![
            "resume".into(),
            id.to_uppercase(),
            "--model".into(),
            "new-model".into(),
        ];
        let mut descriptor =
            TerminalDescriptor::relaunchable(hosted.id, hosted.launch.clone(), None);
        descriptor.session = hosted;
        let _lease = lease(directory.path(), &proof("replacement"));
        let server = discovery(&socket, proof("replacement"), vec![descriptor], 1);
        let result = reconcile_at(original, true, &socket, directory.path());
        server.join().unwrap();
        assert!(
            result
                .err()
                .unwrap()
                .contains("conversation is already hosted")
        );
    }

    #[test]
    fn unrelated_uuid_shaped_flags_do_not_identify_a_hosted_conversation() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let other_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let mut original = session(directory.path(), &proof("old"));
        original.launch.program = "codex".into();
        original.launch.arguments = vec!["resume".into(), id.into()];
        let replacement_proof = proof("replacement");
        let mut hosted = session(directory.path(), &replacement_proof);
        hosted.launch.program = "codex".into();
        hosted.launch.arguments = vec![
            "resume".into(),
            other_id.into(),
            "--model".into(),
            id.into(),
        ];
        let mut descriptor =
            TerminalDescriptor::relaunchable(hosted.id, hosted.launch.clone(), None);
        descriptor.session = hosted;
        let _lease = lease(directory.path(), &replacement_proof);
        let server = discovery(&socket, replacement_proof, vec![descriptor], 2);
        let recovered = reconcile_at(original.clone(), false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(
            recovered.session.agent_resume_recipe(),
            original.agent_resume_recipe()
        );
        assert_ne!(recovered.session.id, original.id);
    }

    #[test]
    fn exact_omp_session_filepath_protects_an_orphan_writer() {
        let directory = tempfile::tempdir().unwrap();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let mut original = session(directory.path(), &proof("old"));
        original.launch.program = "omp".into();
        original.launch.arguments = vec!["--resume".into(), id.into()];
        let process = directory.path().join("42");
        fs::create_dir(&process).unwrap();
        fs::write(process.join("stat"), b"42 (omp) S 1").unwrap();
        fs::write(
            process.join("cmdline"),
            format!("omp\0--session\0/tmp/2026-09-09_{id}.jsonl\0"),
        )
        .unwrap();
        let result = ensure_no_surviving_process(&original, u32::MAX, directory.path());
        assert!(
            result
                .unwrap_err()
                .contains("agent session still has a live process (42)")
        );
    }

    /// What a Close proof sees. It only ever reads: Spawn, Attach, Input or any
    /// close request panics here instead.
    #[derive(Default)]
    struct Inventory {
        terminals: Vec<TerminalDescriptor>,
        chats: Vec<crate::chat::Session>,
        agents: Vec<crate::agent_execution::Agent>,
        /// A daemon in startup recovery answers inventory reads but not this.
        recovery_only: bool,
        /// Owner authentication connects once before and once after the scan.
        owner_probes: usize,
        /// How a pane's Close of a conversation it hosts no longer is
        /// answered: accepted, which closes its agents, or refused with this.
        retained_close: Option<Result<(), String>>,
    }

    fn inventory(
        socket: &Path,
        proof: HostSessionProof,
        mut inventory: Inventory,
    ) -> thread::JoinHandle<()> {
        let listener = UnixListener::bind(socket).unwrap();
        thread::spawn(move || {
            let mut stream = accept_within(&listener);
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            while let Some(request) = read_frame::<ClientEnvelope>(&mut stream).unwrap() {
                let message = match request.request {
                    ClientRequest::BuildInfo if inventory.recovery_only => {
                        ServerMessage::Error(crate::terminal::ipc::ProtocolError::new(
                            crate::terminal::ipc::ProtocolErrorCode::TerminalUnavailable,
                            "Startup recovery retained original ownership",
                            false,
                        ))
                    }
                    ClientRequest::ChatCloseRetained { .. } => {
                        match inventory
                            .retained_close
                            .take()
                            .expect("a Close proof closed a conversation")
                        {
                            Ok(()) => {
                                for agent in &mut inventory.agents {
                                    agent.closed = true;
                                }
                                ServerMessage::Response {
                                    response: ServerResponse::Accepted,
                                }
                            }
                            Err(refusal) => {
                                ServerMessage::Error(crate::terminal::ipc::ProtocolError::new(
                                    crate::terminal::ipc::ProtocolErrorCode::TerminalUnavailable,
                                    refusal,
                                    true,
                                ))
                            }
                        }
                    }
                    other => ServerMessage::Response {
                        response: match other {
                            ClientRequest::Connect { .. } => ServerResponse::Connected {
                                proof: proof.clone(),
                                resumed: Vec::new(),
                            },
                            ClientRequest::BuildInfo => ServerResponse::BuildInfo {
                                version: "fixture".into(),
                            },
                            ClientRequest::List => ServerResponse::Listed {
                                terminals: inventory.terminals.clone(),
                            },
                            ClientRequest::ChatList => ServerResponse::ChatListed {
                                sessions: inventory.chats.clone(),
                            },
                            ClientRequest::ReadWorkspaceModel => ServerResponse::WorkspaceModel {
                                model: crate::workspace_model::WorkspaceModel {
                                    agents: inventory.agents.clone(),
                                    ..Default::default()
                                },
                            },
                            other => panic!("a Close proof sent {other:?}"),
                        },
                    },
                };
                write_frame(
                    &mut stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request.request_id),
                        proof: proof.clone(),
                        message,
                    },
                )
                .unwrap();
            }
            for _ in 0..inventory.owner_probes {
                accept_within(&listener);
            }
        })
    }

    fn no_holder(_: crate::model::TerminalId) -> Result<Option<i32>, String> {
        Ok(None)
    }

    fn reconcile_at(
        session: TerminalSession,
        explicit_retry: bool,
        socket: &Path,
        proc_root: &Path,
    ) -> Result<RecoveredTerminal, String> {
        reconcile_with(session, explicit_retry, socket, proc_root, |_, _| Ok(None))
    }

    fn other_boot(incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            boot_id: crate::model::BootId::new("a-previous-boot"),
            ..proof(incarnation)
        }
    }

    fn exited(id: crate::model::TerminalId, launch: &LaunchRecipe) -> TerminalDescriptor {
        let mut listed = TerminalDescriptor::relaunchable(id, launch.clone(), None);
        listed.session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: launch.clone(),
            last_exit: Some(crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            }),
        };
        listed
    }

    fn descriptor(session: &TerminalSession) -> TerminalDescriptor {
        let mut descriptor =
            TerminalDescriptor::relaunchable(session.id, session.launch.clone(), None);
        descriptor.session = session.clone();
        descriptor
    }

    /// One process outside the daemon's tree that carries this marker.
    fn process_with(proc_root: &Path, marker: String) {
        let process = proc_root.join("4242");
        fs::create_dir(&process).unwrap();
        fs::write(process.join("stat"), "4242 (sh) S 1 4242 4242 0").unwrap();
        fs::write(process.join("cmdline"), "sh\0").unwrap();
        fs::write(process.join("environ"), format!("{marker}\0")).unwrap();
    }

    /// Serves `served`, and returns what a Close proof says about `saved`.
    fn prove(
        saved: &TerminalSession,
        current: HostSessionProof,
        served: Inventory,
        proc_root: &Path,
    ) -> Result<(), String> {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let _lease = lease(directory.path(), &current);
        let server = inventory(&socket, current, served);
        let result = prove_not_hosted_at(saved, &socket, proc_root, no_holder);
        server.join().unwrap();
        result
    }

    #[test]
    fn a_reboot_proves_an_unlisted_terminal_gone() {
        let directory = tempfile::tempdir().unwrap();
        let saved = session(directory.path(), &other_boot("old"));
        let served = Inventory {
            owner_probes: 2,
            ..Default::default()
        };
        assert_eq!(
            prove(&saved, proof("current"), served, directory.path()),
            Ok(())
        );
    }

    #[test]
    fn a_reboot_does_not_excuse_a_replacement_running_on_this_boot() {
        let directory = tempfile::tempdir().unwrap();
        let saved = session(directory.path(), &other_boot("old"));
        // Started on this boot, then lost by a daemon that didn't adopt it.
        process_with(
            directory.path(),
            format!("CAGENTS_RECOVERY_ORIGIN={}", saved.id),
        );
        let served = Inventory {
            owner_probes: 1,
            ..Default::default()
        };
        let refusal = prove(&saved, proof("current"), served, directory.path()).unwrap_err();
        assert!(refusal.contains("4242"), "{refusal}");
    }

    #[test]
    fn a_daemon_restart_on_the_same_boot_is_not_proof_while_a_process_survives() {
        let directory = tempfile::tempdir().unwrap();
        // New session and incarnation IDs, the same kernel boot.
        let saved = session(directory.path(), &proof("before-restart"));
        process_with(
            directory.path(),
            format!("CAGENTS_TERMINAL_ID={}", saved.id),
        );
        let current = proof("after-restart");
        assert_ne!(saved.continuity.live_proof().unwrap(), current);
        assert_eq!(
            saved.continuity.live_proof().unwrap().boot_id,
            current.boot_id
        );
        let served = Inventory {
            owner_probes: 1,
            ..Default::default()
        };
        let refusal = prove(&saved, current, served, directory.path()).unwrap_err();
        assert!(refusal.contains("PID 4242"), "{refusal}");
        assert!(refusal.contains("Stop it"), "{refusal}");
        assert!(!refusal.contains(&saved.id.to_string()), "{refusal}");
        assert!(!refusal.contains("automatic recovery"), "{refusal}");
    }

    /// The launch a recovery terminal saves: a named scope wrapping the provider.
    fn in_named_scope(saved: &mut TerminalSession) {
        saved.launch.program = "/usr/bin/systemd-run".into();
        saved.launch.arguments = [
            "--user",
            "--scope",
            "--quiet",
            "--unit=recovered-1",
            "--slice=app.slice",
            "env",
            "claude",
        ]
        .map(String::from)
        .into();
    }

    #[test]
    fn a_process_in_the_saved_scope_is_not_proof_even_with_its_markers_cleared() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = session(directory.path(), &proof("before-restart"));
        in_named_scope(&mut saved);
        process_with(directory.path(), "PATH=/usr/bin".into());
        fs::write(
            directory.path().join("4242/cgroup"),
            "0::/user.slice/app.slice/recovered-1.scope\n",
        )
        .unwrap();
        let served = Inventory {
            owner_probes: 1,
            ..Default::default()
        };
        fs::write(directory.path().join("4242/comm"), "claude\n").unwrap();
        let refusal = prove(&saved, proof("after-restart"), served, directory.path()).unwrap_err();
        assert!(refusal.starts_with("claude (PID 4242)"), "{refusal}");
        // The unit name is the only link here, and another launch can reuse it.
        assert!(refusal.contains("recovered-1"), "{refusal}");
        assert!(
            refusal.contains("If it came from this terminal"),
            "{refusal}"
        );
    }

    #[test]
    fn a_marked_process_in_the_saved_scope_is_named_as_this_terminals_own() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = session(directory.path(), &proof("before-restart"));
        in_named_scope(&mut saved);
        process_with(
            directory.path(),
            format!("CAGENTS_TERMINAL_ID={}", saved.id),
        );
        fs::write(
            directory.path().join("4242/cgroup"),
            "0::/user.slice/app.slice/recovered-1.scope\n",
        )
        .unwrap();
        let served = Inventory {
            owner_probes: 1,
            ..Default::default()
        };
        let refusal = prove(&saved, proof("after-restart"), served, directory.path()).unwrap_err();
        assert!(refusal.contains("started by this terminal"), "{refusal}");
        assert!(!refusal.contains("If it came from"), "{refusal}");
    }

    #[test]
    fn a_process_that_cannot_be_read_is_not_called_a_survivor() {
        let directory = tempfile::tempdir().unwrap();
        let saved = session(directory.path(), &proof("before-restart"));
        process_with(directory.path(), "PATH=/usr/bin".into());
        // An exec in progress briefly publishes no arguments.
        fs::write(directory.path().join("4242/cmdline"), "").unwrap();
        let served = Inventory {
            owner_probes: 1,
            ..Default::default()
        };
        let refusal = prove(&saved, proof("after-restart"), served, directory.path()).unwrap_err();
        assert!(refusal.contains("was kept"), "{refusal}");
        assert!(refusal.contains("in a moment"), "{refusal}");
        assert!(!refusal.contains("Stop"), "{refusal}");
    }

    #[test]
    fn a_process_in_another_scope_does_not_keep_a_scoped_pane() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = session(directory.path(), &proof("before-restart"));
        in_named_scope(&mut saved);
        process_with(directory.path(), "PATH=/usr/bin".into());
        fs::write(
            directory.path().join("4242/cgroup"),
            "0::/user.slice/app.slice/recovered-10.scope\n",
        )
        .unwrap();
        let served = Inventory {
            owner_probes: 2,
            ..Default::default()
        };
        assert_eq!(
            prove(&saved, proof("after-restart"), served, directory.path()),
            Ok(())
        );
    }

    #[test]
    fn a_daemon_restart_on_the_same_boot_proves_it_gone_once_nothing_survives() {
        let directory = tempfile::tempdir().unwrap();
        let saved = session(directory.path(), &proof("before-restart"));
        let served = Inventory {
            owner_probes: 2,
            ..Default::default()
        };
        assert_eq!(
            prove(&saved, proof("after-restart"), served, directory.path()),
            Ok(())
        );
    }

    #[test]
    fn an_unadopted_holder_is_not_proof_even_with_no_visible_marker() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let saved = session(directory.path(), &proof("before-restart"));
        let current = proof("after-restart");
        let boot = current.boot_id.clone();
        let id = saved.id;
        let _lease = lease(directory.path(), &current);
        let served = Inventory {
            owner_probes: 1,
            ..Default::default()
        };
        let server = inventory(&socket, current, served);
        let refusal = prove_not_hosted_at(&saved, &socket, directory.path(), |terminal| {
            assert_eq!(terminal, id);
            Ok(Some(4242))
        })
        .unwrap_err();
        server.join().unwrap();
        assert_eq!(refusal, unadopted_refusal(4242));

        // Unreadable holder evidence is unknown, not absent.
        let socket = directory.path().join("unreadable.sock");
        let served = Inventory {
            owner_probes: 1,
            ..Default::default()
        };
        let server = inventory(&socket, proof("after-restart"), served);
        let refusal = prove_not_hosted_at(&saved, &socket, directory.path(), |_| {
            Err("unreadable".into())
        })
        .unwrap_err();
        server.join().unwrap();
        assert!(refusal.contains("can't confirm"), "{refusal}");
    }

    #[test]
    fn a_daemon_in_startup_recovery_cannot_prove_an_absence() {
        let directory = tempfile::tempdir().unwrap();
        let saved = session(directory.path(), &other_boot("old"));
        let served = Inventory {
            recovery_only: true,
            ..Default::default()
        };
        assert_eq!(
            prove(&saved, proof("current"), served, directory.path()),
            Err(UNREACHABLE.into())
        );
    }

    #[test]
    fn the_same_daemon_forgetting_a_terminal_still_needs_no_surviving_process() {
        let directory = tempfile::tempdir().unwrap();
        let owner = proof("same");
        let saved = session(directory.path(), &owner);
        process_with(
            directory.path(),
            format!("CAGENTS_TERMINAL_ID={}", saved.id),
        );
        let served = Inventory {
            owner_probes: 1,
            ..Default::default()
        };
        assert!(prove(&saved, owner, served, directory.path()).is_err());
    }

    #[test]
    fn any_listed_identity_keeps_the_pane_whatever_proof_or_boot_it_reports() {
        let directory = tempfile::tempdir().unwrap();
        let saved = session(directory.path(), &other_boot("old"));
        let served = Inventory {
            terminals: vec![descriptor(&saved)],
            ..Default::default()
        };
        assert_eq!(
            prove(&saved, proof("current"), served, directory.path()),
            Err(STILL_HOSTED.into())
        );
    }

    #[test]
    fn a_running_continuation_keeps_the_pane_across_a_reboot() {
        let current = proof("current");
        let directory = tempfile::tempdir().unwrap();
        let saved = session(directory.path(), &other_boot("old"));

        let mut upgraded = session(directory.path(), &current);
        upgraded.id = TerminalId::new();
        let mut handed_off = descriptor(&upgraded);
        handed_off.upgrade_sources = vec![saved.clone()];

        let mut replaced = session(directory.path(), &current);
        replaced.id = TerminalId::new();
        replaced.recovery_origin = Some(saved.id);

        for hosted in [handed_off, descriptor(&replaced)] {
            let served = Inventory {
                terminals: vec![hosted],
                ..Default::default()
            };
            assert_eq!(
                prove(&saved, current.clone(), served, directory.path()),
                Err(CONTINUED.into())
            );
        }
    }

    #[test]
    fn a_finished_descendant_does_not_keep_the_pane() {
        let current = proof("current");
        let directory = tempfile::tempdir().unwrap();
        let saved = session(directory.path(), &other_boot("old"));
        let mut replaced = session(directory.path(), &other_boot("older"));
        replaced.id = TerminalId::new();
        replaced.recovery_origin = Some(saved.id);
        let served = Inventory {
            terminals: vec![descriptor(&replaced)],
            owner_probes: 2,
            ..Default::default()
        };
        assert_eq!(prove(&saved, current, served, directory.path()), Ok(()));
    }

    fn chat(
        id: TerminalId,
        launch: &LaunchRecipe,
        terminal: Option<TerminalSession>,
    ) -> crate::chat::Session {
        crate::chat::Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Omp,
                recipe: launch.clone(),
                session_id: None,
            },
            proof: None,
            terminal,
        }
    }

    #[test]
    fn a_conversation_using_the_terminal_keeps_the_pane_across_a_reboot() {
        let directory = tempfile::tempdir().unwrap();
        let saved = session(directory.path(), &other_boot("old"));
        for chat in [
            chat(saved.id, &saved.launch, None),
            chat(TerminalId::new(), &saved.launch, Some(saved.clone())),
        ] {
            let served = Inventory {
                chats: vec![chat],
                ..Default::default()
            };
            let refusal = prove(&saved, proof("current"), served, directory.path()).unwrap_err();
            assert!(refusal.contains("conversation"), "{refusal}");
        }
    }

    #[test]
    fn only_an_open_agent_using_the_terminal_keeps_the_pane() {
        use crate::agent_execution::{
            Agent, HostedSession, Origin, Placement, Spec, Surface, Target,
        };
        let directory = tempfile::tempdir().unwrap();
        let saved = session(directory.path(), &other_boot("old"));
        let agent = |session, console, closed| Agent {
            name: None,
            id: crate::model::AgentId::new(),
            origin: Origin::User,
            spec: Spec {
                provider: crate::agent::Provider::Claude,
                account: None,
                target: Target {
                    repository: "/execution/.git".into(),
                    directory: "/execution/cwd".into(),
                    placement: None,
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: Placement::Channel(crate::model::ChannelId::new()),
            session,
            console,
            created_ms: 1,
            closed,
        };
        for open in [
            agent(Some(HostedSession::Terminal(saved.clone())), None, false),
            agent(None, Some(saved.clone()), false),
            agent(
                Some(HostedSession::Chat(chat(saved.id, &saved.launch, None))),
                None,
                false,
            ),
            agent(
                Some(HostedSession::Chat(chat(
                    TerminalId::new(),
                    &saved.launch,
                    Some(saved.clone()),
                ))),
                None,
                false,
            ),
        ] {
            let served = Inventory {
                agents: vec![open],
                // Only the pane that is the agent's conversation asks.
                retained_close: Some(Err(
                    "Cagents couldn't confirm this conversation stopped".into()
                )),
                ..Default::default()
            };
            let refusal = prove(&saved, proof("current"), served, directory.path()).unwrap_err();
            assert!(
                refusal.contains("was kept") || refusal.contains("which Cagents couldn't close"),
                "{refusal}"
            );
        }
        let served = Inventory {
            agents: vec![agent(
                Some(HostedSession::Terminal(saved.clone())),
                None,
                true,
            )],
            owner_probes: 2,
            ..Default::default()
        };
        assert_eq!(
            prove(&saved, proof("current"), served, directory.path()),
            Ok(())
        );
    }

    /// #2102: the pane is an open agent's conversation whose provider the
    /// daemon no longer hosts. Its Close closes that conversation, and its
    /// agent with it, as the conversation's own Close does; the daemon keeps
    /// both when it can't prove nothing of it still runs, and says why.
    #[test]
    fn closing_a_lost_agents_own_pane_closes_its_conversation() {
        use crate::agent_execution::{
            Agent, HostedSession, Origin, Placement, Spec, Surface, Target,
        };
        let directory = tempfile::tempdir().unwrap();
        let saved = session(directory.path(), &proof("old"));
        let agent = |origin| Agent {
            name: None,
            id: crate::model::AgentId::new(),
            origin,
            spec: Spec {
                provider: crate::agent::Provider::Claude,
                account: None,
                target: Target {
                    repository: "/execution/.git".into(),
                    directory: "/execution/cwd".into(),
                    placement: None,
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: Placement::Channel(crate::model::ChannelId::new()),
            session: Some(HostedSession::Chat(crate::chat::Session {
                native_continuity: crate::chat::NativeContinuity::RetainedClaudeActor {
                    protocol: 2,
                },
                ..chat(saved.id, &saved.launch, None)
            })),
            console: Some(saved.clone()),
            created_ms: 1,
            closed: false,
        };
        let served = Inventory {
            agents: vec![agent(Origin::User)],
            retained_close: Some(Ok(())),
            owner_probes: 2,
            ..Default::default()
        };
        assert_eq!(
            prove(&saved, proof("current"), served, directory.path()),
            Ok(())
        );

        let reason =
            "Cagents couldn't confirm this conversation stopped. Its Cagents helper is gone";
        let served = Inventory {
            agents: vec![agent(Origin::User)],
            retained_close: Some(Err(reason.into())),
            owner_probes: 2,
            ..Default::default()
        };
        let refusal = prove(&saved, proof("current"), served, directory.path()).unwrap_err();
        assert!(
            refusal.contains("which Cagents couldn't close"),
            "{refusal}"
        );
        assert!(refusal.contains(reason), "{refusal}");

        // A scheduled agent is its automation's to end: nothing is asked.
        let served = Inventory {
            agents: vec![agent(Origin::Automation {
                id: uuid::Uuid::new_v4(),
            })],
            ..Default::default()
        };
        let refusal = prove(&saved, proof("current"), served, directory.path()).unwrap_err();
        assert!(refusal.contains("Only what started it"), "{refusal}");

        // A pane kept for another reason keeps its agent too: nothing is
        // closed until every check has let the pane go. The inventory panics
        // on any conversation Close.
        process_with(
            directory.path(),
            format!("CAGENTS_TERMINAL_ID={}", saved.id),
        );
        let served = Inventory {
            agents: vec![agent(Origin::User)],
            owner_probes: 1,
            ..Default::default()
        };
        let refusal = prove(&saved, proof("current"), served, directory.path()).unwrap_err();
        assert!(refusal.contains("4242"), "{refusal}");
    }

    #[test]
    fn the_agent_refusal_names_the_close_command_only_when_it_can_work() {
        use crate::agent_execution::{
            Agent, HostedSession, Origin, Placement, Spec, Surface, Target,
        };
        use crate::chat::NativeContinuity;
        let launch = LaunchRecipe::shell("/execution/cwd".into()).unwrap();
        let chat_with = |continuity| crate::chat::Session {
            native_continuity: continuity,
            ..chat(TerminalId::new(), &launch, None)
        };
        let agent = |session| Agent {
            name: None,
            id: crate::model::AgentId::new(),
            origin: Origin::User,
            spec: Spec {
                provider: crate::agent::Provider::Claude,
                account: None,
                target: Target {
                    repository: "/execution/.git".into(),
                    directory: "/execution/cwd".into(),
                    placement: None,
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: Placement::Channel(crate::model::ChannelId::new()),
            session,
            console: None,
            created_ms: 1,
            closed: false,
        };
        let boot = proof("current").boot_id;
        // A retained actor that died in this boot closes once the daemon
        // proves nothing of it runs; a conversation with no owner to prove
        // that by has no route.
        assert!(agent_close_can_release(
            &agent(Some(HostedSession::Chat(chat_with(
                NativeContinuity::RetainedClaudeActor { protocol: 2 }
            )))),
            &[],
            &boot
        ));
        let lost = chat_with(NativeContinuity::ReconnectOnly);
        assert!(!agent_close_can_release(
            &agent(Some(HostedSession::Chat(lost.clone()))),
            &[],
            &boot
        ));
        assert!(agent_close_can_release(
            &agent(Some(HostedSession::Chat(lost.clone()))),
            std::slice::from_ref(&lost),
            &boot
        ));
        assert!(!agent_close_can_release(
            &agent(Some(HostedSession::Chat(chat_with(
                NativeContinuity::ReconnectOnly
            )))),
            &[],
            &boot
        ));
        assert!(agent_close_can_release(
            &agent(Some(HostedSession::Chat(chat_with(
                NativeContinuity::Durable {
                    provider_version: "1".into()
                }
            )))),
            &[],
            &boot
        ));
        assert!(agent_close_can_release(&agent(None), &[], &boot));

        let command = |agent: &Agent| format!("cagents agent close {}", agent.id);
        let own = agent(None);
        assert!(open_agent_refusal(&own, &[], &boot).contains(&command(&own)));
        let lost_session = lost.clone();
        let lost_agent = agent(Some(HostedSession::Chat(lost)));
        assert!(!open_agent_refusal(&lost_agent, &[], &boot).contains(&command(&lost_agent)));
        assert!(open_agent_refusal(&lost_agent, &[], &boot).contains("no way to close"));

        // A changed kernel boot gives the daemon a way to close a lost agent.
        let left_behind = crate::chat::Session {
            proof: Some(other_boot("old")),
            ..lost_session.clone()
        };
        let earlier = agent(Some(HostedSession::Chat(left_behind.clone())));
        let named = open_agent_refusal(&earlier, &[], &boot);
        assert!(named.contains(&command(&earlier)), "{named}");
        assert!(named.contains("outside Cagents"), "{named}");
        // Not from this boot's own proof, or from one that isn't complete.
        let this_boot = crate::chat::Session {
            proof: Some(proof("old")),
            ..lost_session.clone()
        };
        let incomplete = crate::chat::Session {
            proof: Some(HostSessionProof {
                host_incarnation: HostIncarnation::new(""),
                ..other_boot("old")
            }),
            ..lost_session
        };
        for session in [this_boot, incomplete] {
            let held = agent(Some(HostedSession::Chat(session)));
            assert!(open_agent_refusal(&held, &[], &boot).contains("no way to close"));
        }
        // Whoever started it decides, and an automation's agent has no route.
        let child = Agent {
            origin: Origin::Child {
                parent: crate::agent_execution::Lineage(TerminalId::new()),
            },
            ..earlier.clone()
        };
        let refusal = open_agent_refusal(&child, &[], &boot);
        assert!(!refusal.contains("cagents agent close"), "{refusal}");
        assert!(refusal.contains("Only what started it"), "{refusal}");
        let scheduled = Agent {
            origin: Origin::Automation {
                id: uuid::Uuid::new_v4(),
            },
            ..earlier
        };
        assert!(open_agent_refusal(&scheduled, &[], &boot).contains("no way to close"));

        // The daemon refuses a shell closing an agent it doesn't own.
        for origin in [
            Origin::Child {
                parent: crate::agent_execution::Lineage(TerminalId::new()),
            },
            Origin::Automation {
                id: uuid::Uuid::new_v4(),
            },
        ] {
            let other = Agent {
                origin,
                ..agent(None)
            };
            let refusal = open_agent_refusal(&other, &[], &boot);
            assert!(!refusal.contains("cagents agent close"), "{refusal}");
            assert!(refusal.contains("Only what started it"), "{refusal}");
        }
    }

    #[test]
    fn an_unreachable_daemon_is_not_proof() {
        let directory = tempfile::tempdir().unwrap();
        let saved = session(directory.path(), &other_boot("old"));
        assert_eq!(
            prove_not_hosted_at(
                &saved,
                &directory.path().join("daemon.sock"),
                directory.path(),
                no_holder,
            ),
            Err(UNREACHABLE.into())
        );
    }

    #[test]
    fn a_socket_that_does_not_hold_the_daemon_lease_is_not_proof() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let saved = session(directory.path(), &other_boot("old"));
        // Nothing holds cagentsd.lock, whatever this listener claims to be.
        let server = inventory(&socket, proof("impostor"), Inventory::default());
        assert_eq!(
            prove_not_hosted_at(&saved, &socket, directory.path(), no_holder),
            Err(UNREACHABLE.into())
        );
        server.join().unwrap();
    }

    #[test]
    fn a_recovery_in_progress_holds_the_proof_back_until_it_has_finished() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let saved = session(directory.path(), &other_boot("old"));
        let _recovering = crate::daemon::RecoveryLease::acquire(&socket, Duration::ZERO).unwrap();
        let refusal =
            prove_not_hosted_at(&saved, &socket, directory.path(), no_holder).unwrap_err();
        assert!(refusal.contains("being recovered"), "{refusal}");
    }

    #[test]
    fn an_incomplete_saved_identity_is_not_proof() {
        let directory = tempfile::tempdir().unwrap();
        let mut incomplete = other_boot("old");
        incomplete.host_incarnation = HostIncarnation::new("");
        let saved = session(directory.path(), &incomplete);
        let refusal = prove_not_hosted_at(
            &saved,
            &directory.path().join("daemon.sock"),
            directory.path(),
            no_holder,
        )
        .unwrap_err();
        assert!(refusal.contains("incomplete"), "{refusal}");
    }
}
