//! Bounded source qualification after source windows flush, before close.
//! Stores remain read-only. Exact retained providers missing from startup
//! discovery may be authenticated through Resolve; no launch recipe is run.
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

/// Keep the complete classification in one bounded pass. Details are redacted
/// and capped independently of the inventory: a large workspace must not turn
/// one refusal into a sequence of install/close/retry cycles.
#[derive(Default)]
struct InspectionFailures {
    count: usize,
    details: Vec<String>,
}

impl InspectionFailures {
    fn push(&mut self, reason: String) {
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
        match path.try_exists() {
            Ok(false) => continue,
            Err(_) => {
                failures.push(format!("Saved window {} cannot be read.", window + 1));
                continue;
            }
            Ok(true) => {}
        }
        match crate::persistence::read_open_migration_sessions_with_budget(&path, &mut budget) {
            Ok(panes) => saved.extend(panes),
            Err(_) => failures.push(format!("Saved window {} has an unreadable or unsupported pane store; its contents were preserved.", window + 1)),
        }
    }
    let no_live_panes = saved.iter().all(|(_, kind)| match kind {
        CardKind::Chat(session) => session.is_unstarted(),
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
    for (_, kind) in &saved {
        check_deadline(deadline)?;
        if let CardKind::Chat(source) = kind {
            if !source.is_unstarted()
                && (source.proof.as_ref() != request.proof.as_ref()
                    || !initial_inventory.iter().any(|live| live.id == source.id))
                && !resolutions.iter().any(|(issued, _)| issued == source)
            {
                resolutions.push((source.clone(), reader.resolve(source)));
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
                if qualify_with_mode(
                    &source,
                    &inventory,
                    proof,
                    request.require_resumable,
                    |source| {
                        resolutions
                            .iter()
                            .find(|(issued, _)| issued == source)
                            .map(|(_, result)| result.clone())
                            .unwrap_or_else(|| Err("No authenticated source resolution".into()))
                    },
                )
                .is_err()
                {
                    failures.push(format!(
                        "Saved conversation pane {card} needs exact conversation recovery."
                    ));
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
