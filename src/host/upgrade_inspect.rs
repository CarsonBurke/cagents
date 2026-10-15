//! Read-only source qualification after source windows flush, before close.
//! The helper does not take a daemon admission lease, migrate SQLite, attach,
//! restart providers, or publish a replacement descriptor.
mod v77;

use crate::install_guard::SourceInspectionRequest as Request;
use crate::{
    chat::Session,
    model::{CardId, CardKind, HostSessionProof},
    upgrade::handoff,
};
use std::{
    collections::HashSet,
    io::{self, Write},
    path::{Path,PathBuf},
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
    Some((|| {
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
            .map_err(|_| "Cannot flush source inspection receipt")?;
        stdout
            .flush()
            .map_err(|_| "Cannot flush source inspection receipt")
    })())
}

enum Reader {
    V77(v77::Client),
    Current(crate::control::ControlClient),
}
impl Reader {
    fn connect(request: &Request, deadline: Instant) -> Result<Self, String> {
        let proof = request
            .proof
            .as_ref()
            .filter(|proof| proof.is_complete())
            .ok_or("Saved conversations have no authenticated source service")?;
        let socket = crate::terminal::ipc::daemon_socket_path()
            .map_err(|_| "Source service namespace is unavailable")?;
        if request.protocol == 77 {
            return v77::Client::connect(&socket, proof, deadline).map(Self::V77);
        }
        if request.protocol != crate::terminal::ipc::PROTOCOL_VERSION {
            return Err("Source protocol has no qualified migration inspector; source windows remain usable".into());
        }
        let client = crate::control::ControlClient::connect(&socket)
            .map_err(|_| "Cannot authenticate source service")?;
        if client.proof() != proof {
            return Err("Source service ownership changed".into());
        }
        client
            .verify_local_owner(&socket)
            .map_err(|_| "Source service lease does not authenticate its connection")?;
        Ok(Self::Current(client))
    }
    fn list(&mut self) -> Result<Vec<Session>, String> {
        match self {
            Self::V77(client) => client.list(),
            Self::Current(client) => client
                .list_chats()
                .map_err(|_| "Cannot inspect source chat inventory".into()),
        }
    }
    fn resolve(&mut self, source: &Session) -> Result<Session, String> {
        match self {
            Self::V77(client) => client.resolve(source),
            Self::Current(client) => client
                .resolve_upgrade_chat(source.clone())
                .map_err(|_| "Source cannot authenticate saved conversation lineage".into()),
        }
    }
    fn terminals(&mut self) -> Result<Vec<v77::Terminal>, String> {
        match self {
            Self::V77(client) => client.terminals(),
            Self::Current(client) => client
                .list()
                .map(|terminals| terminals.into_iter().map(Into::into).collect())
                .map_err(|_| "Cannot inspect source terminal inventory".into()),
        }
    }
}

fn inspect(request: Request) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let paths = crate::instance::window_database_paths()
        .map_err(|_| "Cannot enumerate saved source windows")?;
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
            crate::persistence::read_open_migration_sessions(&path)
                .map_err(|_| "A saved source pane is unreadable; source windows remain usable")?
                .into_iter().map(|(card,kind)|(path.clone(),card,kind)),
        );
        if saved.len() > 4096 {
            return Err("Saved pane inventory exceeds the migration inspection limit".into());
        }
    }
    let registry=crate::instance::project_dirs().ok_or("Cannot locate the source archive registry")?
        .data_dir().join("workspace-model.sqlite3");
    let saved=require_live_panes(saved,deadline,|database,card,session| {
        crate::automations::archive::inspect_archived_pane(database,&registry,card,session).map(|receipt|receipt.is_some())
    })?;
    let no_live_panes = saved.iter().all(|(_, kind)| match kind {
        CardKind::Chat(session) => session.is_unstarted(),
        CardKind::Terminal(session) => terminal_ended(session),
        _ => true,
    });
    if no_live_panes && request.proof.is_none() {
        return Ok(());
    }
    let mut reader = Reader::connect(&request, deadline)?;
    let inventory = reader.list()?;
    let terminals = reader.terminals()?;
    if inventory.len() > 128 || terminals.len() > 1024 {
        return Err("Source inventory exceeds the qualified migration limit; reduce the inventory before updating. Source windows remain usable".into());
    }
    let proof = request
        .proof
        .as_ref()
        .ok_or("Source service proof is unavailable")?;
    let mut ids = HashSet::new();
    for session in &inventory {
        if !ids.insert(session.id) || session.proof.as_ref() != Some(proof) {
            return Err(format!(
                "Source conversation {} has ambiguous ownership; reconnect or close it before updating. Source windows remain usable",
                session.id
            ));
        }
        validate_live_chat(session, request.require_resumable)
            .map_err(|_| format!("Source conversation {} has no qualified immutable migration; reconnect or close it before updating. Source windows remain usable", session.id))?;
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

fn check_deadline(deadline:Instant)->Result<(),String>{
    if Instant::now()>=deadline {return Err("Source pane inspection exceeded its bounded budget; source windows remain usable".into());}
    Ok(())
}

/// An archive receipt proves a readable, closed conversation. It does not
/// require a daemon or authorize starting/attaching any provider. An absent
/// binding keeps the ordinary exact-live-owner requirements in force.
fn require_live_panes(
    saved:Vec<(PathBuf,CardId,CardKind)>,deadline:Instant,
    mut archive:impl FnMut(&Path,CardId,&Session)->Result<bool,String>,
)->Result<Vec<(CardId,CardKind)>,String>{
    let mut live=Vec::new();
    for (database,card,kind) in saved {
        check_deadline(deadline)?;
        if let CardKind::Chat(session)=&kind && !session.is_unstarted() {
            let qualified=archive(&database,card,session).map_err(|_|format!("Saved archive pane {card} has no qualified readable history. Recover or close that pane before updating; source windows remain usable"))?;
            check_deadline(deadline)?;
            if qualified {continue;}
        }
        live.push((card,kind));
    }
    Ok(live)
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
                let mut normalized = terminal.session.clone();
                normalized.continuity = saved.continuity.clone();
                normalized == *saved
            } else {
                old.boot_id == proof.boot_id && terminal.upgrade_sources.contains(saved)
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
    fn archived_panes_require_typed_history_and_never_hide_unclaimed_live_panes() {
        let archived=CardId::new();let live=CardId::new();let mut draft=session();draft.proof=None;draft.launch.session_id=None;
        let draft_id=CardId::new();
        let saved=vec![(PathBuf::from("/window/a"),archived,CardKind::Chat(session())),(PathBuf::from("/window/b"),live,CardKind::Chat(session())),(PathBuf::from("/window/c"),draft_id,CardKind::Chat(draft))];
        let mut inspected=Vec::new();
        let remaining=require_live_panes(saved,Instant::now()+Duration::from_secs(1),|database,card,_|{inspected.push(database.to_owned());Ok(card==archived)}).unwrap();
        assert_eq!(remaining.iter().map(|(card,_)|*card).collect::<Vec<_>>(),vec![live,draft_id]);
        assert_eq!(inspected,vec![PathBuf::from("/window/a"),PathBuf::from("/window/b")]);
        let saved=vec![(PathBuf::from("/window"),archived,CardKind::Chat(session()))];
        let error=require_live_panes(saved.clone(),Instant::now()+Duration::from_secs(1),|_,_,_|Err("private archive payload must not leak".into())).unwrap_err();
        assert!(error.contains(&archived.to_string()));assert!(!error.contains("private archive payload"));
        assert!(require_live_panes(saved.clone(),Instant::now(),|_,_,_|panic!("expired inspection must not open another database")).is_err());
        assert!(require_live_panes(saved,Instant::now()+Duration::from_secs(1),|_,_,_|Ok(true)).unwrap().is_empty(),"archives-only saved panes require no live source");
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
        descriptor.upgrade_sources.push(saved.clone());
        qualify_terminal(&saved, &[descriptor.into()], &replacement).unwrap();
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
        .find(|live| handoff::issued_chat_matches(live, source))
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
