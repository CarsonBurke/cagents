//! Read-only source qualification after source windows flush, before close.
//! The helper does not take a daemon admission lease, migrate SQLite, attach,
//! restart providers, or publish a replacement descriptor.
mod current;
mod v68;
mod v77;

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
    fn resolve(&mut self, source: &Session) -> Result<Session, String> {
        match self {
            Self::Current(client) => client.resolve(source),
            Self::Legacy(client) => client.resolve(source),
            Self::Legacy68(client) => client.resolve(source),
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

fn inspect(request: Request) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let paths = crate::instance::window_database_paths()
        .map_err(|_| "Cannot enumerate saved source windows")?;
    let mut budget = crate::persistence::MigrationInspectionBudget::new(deadline);
    let mut saved = Vec::new();
    for path in paths {
        check_deadline(deadline)?;
        if !path
            .try_exists()
            .map_err(|_| "Cannot verify a saved source database")?
        {
            continue;
        }
        saved.extend(
            crate::persistence::read_open_migration_sessions_with_budget(&path, &mut budget)
                .map_err(|_| "A saved source pane is unreadable; source windows remain usable")?
                .into_iter()
                .map(|(card, kind)| (path.clone(), card, kind)),
        );
        if saved.len() > handoff::MAX_SESSIONS {
            return Err("Saved pane inventory exceeds the migration inspection limit".into());
        }
    }
    let saved = saved
        .into_iter()
        .map(|(_, card, kind)| (card, kind))
        .collect::<Vec<_>>();
    let no_live_panes = saved.iter().all(|(_, kind)| match kind {
        CardKind::Chat(session) => session.is_unstarted(),
        CardKind::Terminal(session) => terminal_ended(session),
        _ => true,
    });
    if no_live_panes && request.proof.is_none() {
        return Ok(());
    }
    let mut reader = connect(&request, deadline)?;
    let inventory = reader.list()?;
    let terminals = reader.terminals()?;
    if inventory.len() > handoff::MAX_CHATS || terminals.len() > handoff::MAX_TERMINALS {
        return Err("Source inventory exceeds the qualified migration limit; reduce the inventory before updating. Source windows remain usable".into());
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
        if !ids.insert(session.id) || session.proof.as_ref() != Some(proof) {
            return Err(format!(
                "Source conversation {} has ambiguous ownership; reconnect or close it before updating. Source windows remain usable",
                session.id
            ));
        }
        if let Some(terminal) = &session.terminal {
            qualify_console_migration(terminal, &matched_cli).map_err(|reason| {
                format!(
                    "Source conversation {}: {reason}. Source windows remain usable",
                    session.id
                )
            })?;
        }
        validate_live_chat(session, request.require_resumable)
            .map_err(|_| format!("Source conversation {} has no qualified immutable migration; reconnect or close it before updating. Source windows remain usable", session.id))?;
    }
    for terminal in &terminals {
        if !terminal_ended(&terminal.session) {
            qualify_console_migration(&terminal.session, &matched_cli).map_err(|reason| {
                format!(
                    "Source terminal {}: {reason}. Source windows remain usable",
                    terminal.session.id
                )
            })?;
        }
    }
    for (card, kind) in saved {
        check_deadline(deadline)?;
        if let CardKind::Terminal(source) = &kind {
            if !terminal_ended(source) {
                qualify_terminal(source, &terminals, proof)
                    .map_err(|_| format!("Saved terminal pane {card} has no qualified exact source owner. Recover or close it before updating; source windows remain usable"))?;
            }
            continue;
        }
        let CardKind::Chat(source) = kind else {
            continue;
        };
        if source.is_unstarted() {
            continue;
        }
        qualify_with_mode(&source, &inventory, proof, request.require_resumable, |source| reader.resolve(source))
            .map_err(|_| format!("Saved conversation pane {card} has no qualified exact migration. Reconnect, recover, or close that pane before updating; source windows remain usable"))?;
    }
    Ok(())
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

fn terminal_ended(session: &crate::model::TerminalSession) -> bool {
    matches!(
        session.continuity,
        crate::model::TerminalContinuity::Exited { .. }
            | crate::model::TerminalContinuity::RelaunchAvailable {
                last_exit: Some(_),
                ..
            }
    )
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
    let source = if saved.proof.as_ref() == Some(proof) {
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
