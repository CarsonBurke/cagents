//! Explicit disk resume requires completed retirement of the exact old actor.
//! This proves one predecessor closed; ordinary launch/account and concurrent
//! conversation admission still run for the new pane and its new writer.
use super::{Handle, Provider, Session, SharedHost};
use crate::{
    account::scope::LaunchAccountScope,
    daemon::holder_registry::{OriginalRole, Record},
    host::model::{ArchivedHistory, NativeActorOwnership, NativeActorRetirement},
    model::HostSessionProof,
};

struct Evidence<'a> {
    actor: uuid::Uuid,
    owner: &'a Record,
    proof: &'a HostSessionProof,
    source: &'a Session,
    aliases: &'a [Session],
    scope: Result<&'a LaunchAccountScope, &'a str>,
    confirmed: Option<&'a str>,
    paired_holder: Option<&'a Record>,
}

fn same_scope(left: &LaunchAccountScope, right: &LaunchAccountScope) -> bool {
    left.provider == right.provider
        && left.canonical_home == right.canonical_home
        && left.device == right.device
        && left.inode == right.inode
}

// Evidence is projected only from a VerifiedRetirement on the production path.
// A holder awaiting its parent's wait is not executing provider work.
fn validate_retired_identity(
    ownership: &NativeActorOwnership,
    retirement: &NativeActorRetirement,
    evidence: &Evidence<'_>,
) -> Result<(), String> {
    let owns_alias =
        |candidate: &Session| candidate == evidence.source || evidence.aliases.contains(candidate);
    if evidence.actor != ownership.actor()
        || evidence.actor != retirement.actor()
        || evidence.owner != ownership.owner()
        || evidence.owner != retirement.owner()
        || evidence.source.id != evidence.owner.terminal
        || !owns_alias(ownership.source())
        || !owns_alias(retirement.source())
        || retirement.confirmed_session() != evidence.confirmed
        || evidence.owner.root.is_current()
    {
        return Err("The saved Claude actor has no matching completed retirement".into());
    }
    let captured = evidence.scope.map_err(|_| {
        "The retired Claude actor has no captured physical account identity".to_owned()
    })?;
    let registered = ownership.scope().map_err(str::to_owned)?;
    let retired = retirement.scope().map_err(str::to_owned)?;
    if captured.provider != Provider::Claude
        || !same_scope(captured, registered)
        || !same_scope(captured, retired)
    {
        return Err("The Claude retirement changed its captured physical account identity".into());
    }
    Ok(())
}

fn retirement_evidence(verified: &crate::chat::runtime::VerifiedRetirement) -> Evidence<'_> {
    let receipt = verified.receipt();
    Evidence {
        actor: receipt.identity.instance,
        owner: &receipt.identity.record,
        proof: &receipt.proof,
        source: &receipt.source,
        aliases: &receipt.previous_sources,
        scope: receipt
            .launch_account_scope
            .as_ref()
            .map_err(String::as_str),
        confirmed: receipt.confirmed_session.as_deref(),
        paired_holder: receipt.paired_holder.as_ref(),
    }
}

fn unconfirmed_retired_startup(
    ownership: &NativeActorOwnership,
    retirement: &NativeActorRetirement,
    verified: &crate::chat::runtime::VerifiedRetirement,
) -> Result<bool, String> {
    // This actor is not resume evidence. A normal Close has already consumed
    // its semantic journal and authenticated both process and helper teardown.
    // Keep its census/retirement intact, but do not let a failed initialization
    // compete with a later provider-confirmed actor using the same allocation.
    // Pending cleanup intent remains subject to its separate strict witness.
    if ownership.is_pending()
        || retirement.is_pending()
        || verified.receipt().confirmed_session.is_some()
    {
        return Ok(false);
    }
    validate_retired_identity(ownership, retirement, &retirement_evidence(verified))?;
    Ok(true)
}

fn validate_evidence(
    source: &Session,
    ownership: &NativeActorOwnership,
    retirement: &NativeActorRetirement,
    evidence: &Evidence<'_>,
    archived: Option<&ArchivedHistory>,
) -> Result<(), String> {
    if (ownership.is_pending() || retirement.is_pending())
        && !archived.is_some_and(|archived| {
            archived.matches(source, ownership, retirement) && source == evidence.source
        })
    {
        return Err("Pending cleanup intent never grants disk resume authority".into());
    }
    validate_retired_identity(ownership, retirement, evidence)?;
    if source.id != evidence.owner.terminal
        || (source != evidence.source && !evidence.aliases.contains(source))
        || evidence.confirmed != source.launch.session_id.as_deref()
        || evidence.confirmed.is_none()
    {
        return Err(
            "The exact saved Claude pane has no matching completed actor retirement".into(),
        );
    }
    Ok(())
}

fn validate_resume_account(source: &Session, evidence: &Evidence<'_>) -> Result<(), String> {
    let captured = evidence.scope.map_err(str::to_owned)?;
    captured.revalidate()?;
    let launch = super::prepare_native_process_launch(
        &source.launch,
        Some(crate::model::agent_recovery::fresh_native_environment(
            Provider::Claude,
            false,
        )),
        &Default::default(),
        None,
    )
    .map_err(|error| error.message)?;
    let selected =
        LaunchAccountScope::resolve(Provider::Claude, &launch.recipe.working_directory, |name| {
            launch
                .recipe
                .environment
                .get(name)
                .map(std::ffi::OsString::from)
        })?;
    if !same_scope(captured, &selected) {
        return Err("Claude disk resume would select a different physical account home".into());
    }
    Ok(())
}

/// A retired archived predecessor cannot authorize the requested allocation.
/// Authenticate its own exact history/private receipt before ignoring it, and
/// carry its witness through the final registry recheck with the current one.
fn archived_predecessor(
    registry: &super::super::agent_execution::SharedRegistry,
    requested: &Session,
    ownership: &NativeActorOwnership,
    retirement: &NativeActorRetirement,
    proof: &HostSessionProof,
) -> Result<ArchivedHistory, String> {
    let source = retirement.source();
    let archived = registry
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .archived_history(source, ownership, retirement, &proof.boot_id)?
        .ok_or("Pending cleanup intent never grants disk resume authority")?;
    let verified = match Handle::inspect_retained_retirement(ownership.owner(), proof)? {
        crate::chat::runtime::RetirementInspection::Verified(verified) => verified,
        crate::chat::runtime::RetirementInspection::Running
        | crate::chat::runtime::RetirementInspection::ExitedUnproved => {
            return Err(
                "The archived Claude predecessor has no authenticated completed retirement".into(),
            );
        }
    };
    if !verified.receipt().installed {
        return Err("Archived history requires actual installed actor retirement".into());
    }
    let evidence = retirement_evidence(&verified);
    validate_evidence(source, ownership, retirement, &evidence, Some(&archived))?;
    // ArchivedHistory requires no terminal. The private receipt must agree;
    // no retired predecessor may hide an independently owned original pair.
    if evidence.paired_holder.is_some() {
        return Err("Archived Claude predecessor retained unexpected paired ownership".into());
    }
    validate_resume_account(requested, &evidence)?;
    Ok(archived)
}

fn current_admission(
    host: &SharedHost,
    source: &Session,
    expected: &HostSessionProof,
) -> Result<(), String> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutting_down
        || state.proof() != expected
        || state.native_closing.contains_key(&source.id)
        || state.native_actor_startups.contains_key(&source.id)
        || state.chats.values().any(|record| {
            let mut launch = record.session.launch.clone();
            launch.session_id = record.handle.confirmed_session_id();
            (record.session.id == source.id || super::same_conversation(&launch, &source.launch))
                && record.handle.owns_work()
        })
        || state.chat_spawning.values().any(|pending| {
            pending.id == source.id || super::same_conversation(&pending.launch, &source.launch)
        })
    {
        return Err(
            "Claude resume has a current owner or unsettled admission; no replacement was prepared"
                .into(),
        );
    }
    Ok(())
}

/// Classify this exact durable pane before generic discovery can mistake an
/// exited, unreaped helper for a live owner. Carry the single verified read
/// through the later account/registry checks rather than reopening its files.
pub(super) fn inspect_resume_retirement(
    host: &SharedHost,
    source: &Session,
) -> Result<Option<crate::chat::runtime::VerifiedRetirement>, String> {
    if !matches!(
        source.native_continuity,
        crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
    ) {
        return Ok(None);
    }
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    // Cross-boot resume has its own public path: no old runtime receipt can
    // authenticate current-boot execution, and this classifier must not veto it.
    if source
        .proof
        .as_ref()
        .is_some_and(|saved| saved.boot_id != proof.boot_id)
    {
        return Ok(None);
    }
    let registry = super::super::agent_execution::registry(host)?;
    let mut after = None;
    let mut verified = None;
    let mut running = false;
    loop {
        let page = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownerships(after, 128)?;
        if page.is_empty() {
            // The census is keyed by actor, not pane. A historical retired
            // actor cannot hide a current owner of the same pane allocation.
            // Scan every row first so Running never swallows an unproved exit,
            // receipt error, pending owner or ambiguous verified history.
            return Ok(if running { None } else { verified });
        }
        for ownership in page {
            after = Some(ownership.actor());
            if ownership.source().id != source.id {
                continue;
            }
            if ownership.is_pending() {
                let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
                let retirement = registry.native_actor_retirement(ownership.actor())?;
                let archived = match retirement.as_ref() {
                    Some(retirement) => registry
                        .archived_history(source, &ownership, retirement, &proof.boot_id)?
                        .is_some(),
                    None => false,
                };
                if !archived {
                    return Err("Pending cleanup intent never grants disk resume authority".into());
                }
            }
            match Handle::inspect_retained_retirement(ownership.owner(), &proof)? {
                crate::chat::runtime::RetirementInspection::Running => running = true,
                crate::chat::runtime::RetirementInspection::ExitedUnproved => {
                    return Err("The saved Claude actor exited without verified retirement".into());
                }
                crate::chat::runtime::RetirementInspection::Verified(receipt) => {
                    let retirement = registry
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .native_actor_retirement(ownership.actor())?
                        .ok_or("The saved Claude actor has no committed retirement")?;
                    if unconfirmed_retired_startup(&ownership, &retirement, &receipt)? {
                        continue;
                    }
                    if receipt.receipt().identity.instance != ownership.actor()
                        || verified.replace(receipt).is_some()
                    {
                        return Err(
                            "The saved Claude pane has ambiguous retired actor ownership".into(),
                        );
                    }
                }
            }
        }
    }
}

/// Called only for explicit resume, before the separate Codex close reader.
/// Returning true grants no provider start, account override or input replay.
pub(super) fn prepare_resume(host: &SharedHost, source: &Session) -> Result<bool, String> {
    prepare_resume_verified(host, source, None)
}

/// A read-only visual is not an actor alias. Before leaving a retired managed
/// allocation, independently qualify its exact final actor source through the
/// ordinary private-retirement/history/account join. Never restart from the
/// reader's descriptor or from a closed Agent row alone.
pub(super) fn prepare_history_successor(host: &SharedHost, reader: &Session) -> Result<(), String> {
    let registry = super::super::agent_execution::registry(host)?;
    let mut after = None;
    let mut final_source = None;
    loop {
        let page = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownerships(after, 128)?;
        if page.is_empty() {
            break;
        }
        for ownership in page {
            after = Some(ownership.actor());
            if ownership.source().id != reader.id {
                continue;
            }
            let retirement = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .native_actor_retirement(ownership.actor())?
                .ok_or("The saved-history allocation has an owner without completed retirement")?;
            let source = retirement.source();
            if source.launch != reader.launch
                || source.terminal.is_some()
                || source.owned_codex_listener.is_some()
                || final_source.replace(source.clone()).is_some()
            {
                return Err("The saved-history allocation has ambiguous retired ownership".into());
            }
        }
    }
    let source = final_source
        .ok_or("The retired saved-history allocation has no authenticated actor retirement")?;
    if !prepare_resume(host, &source)? {
        return Err("The retired saved-history allocation has no matching completed actor".into());
    }
    Ok(())
}

pub(super) fn prepare_resume_verified(
    host: &SharedHost,
    source: &Session,
    mut verified: Option<crate::chat::runtime::VerifiedRetirement>,
) -> Result<bool, String> {
    if source.launch.provider != Provider::Claude {
        return Ok(false);
    }
    if !matches!(
        source.native_continuity,
        crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
    ) || !source.has_valid_native_continuity()
        || source.launch.session_id.is_none()
    {
        return Err("Saved Claude resume requires supported exact retained-actor evidence".into());
    }
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    let paths = crate::daemon::RuntimePaths::discover().map_err(|e| e.to_string())?;
    if !proof.is_complete()
        || crate::daemon::lease_owner(&paths.lock, &proof).map_err(|e| e.to_string())?
            != std::process::id()
        || source
            .proof
            .as_ref()
            .is_none_or(|old| old.boot_id != proof.boot_id)
    {
        return Err("Claude resume reader does not own this boot's current daemon lease".into());
    }
    super::super::account_lifecycle::ensure_idle(host).map_err(|e| e.message)?;
    current_admission(host, source, &proof)?;
    let registry = super::super::agent_execution::registry(host)?;
    let mut after = None;
    let mut matched = false;
    let mut archived_history = Vec::new();
    loop {
        let page = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownerships(after, 128)?;
        if page.is_empty() {
            break;
        }
        for ownership in page {
            after = Some(ownership.actor());
            if ownership.source().id != source.id
                && !super::same_conversation(&ownership.source().launch, &source.launch)
            {
                continue;
            }
            let retirement = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .native_actor_retirement(ownership.actor())?;
            if ownership.source().id != source.id {
                if ownership.is_pending()
                    || retirement
                        .as_ref()
                        .is_some_and(|receipt| receipt.is_pending())
                {
                    let retirement = retirement
                        .as_ref()
                        .ok_or("Pending cleanup intent never grants disk resume authority")?;
                    archived_history.push(archived_predecessor(
                        &registry, source, &ownership, retirement, &proof,
                    )?);
                    continue;
                }
                // A known second writer is never made safe by this actor's
                // receipt. Unknown provider identities remain the ordinary
                // launch admission's responsibility, not an inferred absence.
                if super::same_conversation(&ownership.source().launch, &source.launch)
                    && retirement.is_none()
                {
                    return Err(
                        "Another registered Claude owner has no completed retirement".into(),
                    );
                }
                continue;
            }
            let retirement =
                retirement.ok_or("The saved Claude actor has no committed retirement")?;
            let archived = if ownership.is_pending() || retirement.is_pending() {
                Some(
                    registry
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .archived_history(source, &ownership, &retirement, &proof.boot_id)?
                        .ok_or("Pending cleanup intent never grants disk resume authority")?,
                )
            } else {
                None
            };
            let inspection = if verified
                .as_ref()
                .is_some_and(|receipt| receipt.receipt().identity.instance == ownership.actor())
            {
                crate::chat::runtime::RetirementInspection::Verified(verified.take().unwrap())
            } else {
                Handle::inspect_retained_retirement(ownership.owner(), &proof)?
            };
            let verified = match inspection {
                crate::chat::runtime::RetirementInspection::Verified(verified) => verified,
                crate::chat::runtime::RetirementInspection::Running => {
                    return Err(
                        "The saved Claude actor still executes; no replacement was prepared".into(),
                    );
                }
                crate::chat::runtime::RetirementInspection::ExitedUnproved => {
                    return Err(
                        "The saved Claude actor has no authenticated final retirement proof".into(),
                    );
                }
            };
            if unconfirmed_retired_startup(&ownership, &retirement, &verified)? {
                continue;
            }
            let receipt = verified.receipt();
            if archived.is_some() && !receipt.installed {
                return Err("Archived history requires actual installed actor retirement".into());
            }
            let evidence = retirement_evidence(&verified);
            validate_evidence(
                source,
                &ownership,
                &retirement,
                &evidence,
                archived.as_ref(),
            )?;
            if matched {
                return Err("The saved Claude pane matches more than one retired actor".into());
            }
            validate_resume_account(source, &evidence)?;
            match (&evidence.source.terminal, evidence.paired_holder) {
                (None, None) => {}
                (Some(terminal), Some(pair))
                    if terminal.id == pair.terminal && pair.terminal == source.id =>
                {
                    let role = OriginalRole::authenticated(pair, evidence.proof, &proof)?;
                    if !crate::terminal::holder::cleanup::confirmed_owner_with_role(
                        pair,
                        &proof,
                        role.as_ref(),
                    )? {
                        return Err("The retired Claude actor's exact paired terminal has no confirmed close proof".into());
                    }
                }
                _ => {
                    return Err(
                        "Claude retirement lost its exact original paired terminal evidence".into(),
                    );
                }
            }
            matched = true;
            if let Some(archived) = archived {
                archived_history.push(archived);
            }
        }
    }
    for archived in &archived_history {
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .recheck_archived_history(archived)?;
    }
    current_admission(host, source, &proof)?;
    // This is a fresh current-lease check after all disk and account reads.
    if crate::daemon::lease_owner(&paths.lock, &proof).map_err(|e| e.to_string())?
        != std::process::id()
    {
        return Err("Daemon authority changed during Claude resume preparation".into());
    }
    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::{Launch, NativeContinuity},
        daemon::holder_registry::{Kind, Process},
        host::model::Registry,
        model::{HostIncarnation, HostSessionId, LaunchRecipe, TerminalId},
    };
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn disk_resume_joins_exact_store_retirement_to_private_issued_aliases() {
        // Real typed store writes/readback; this exercises the authority join,
        // not the separate kernel-lease/private-file/helper qualification.
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("resume-fixture"),
            host_incarnation: HostIncarnation::new("original"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
        };
        let issued = Session {
            id: TerminalId::new(),
            launch: Launch {
                provider: Provider::Claude,
                session_id: None,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: vec![],
                    working_directory: root.path().into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: NativeContinuity::RetainedClaudeActor { protocol: 2 },
        };
        let mut source = issued.clone();
        source.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        let mut dead_incarnation = Process::read(std::process::id() as i32).unwrap();
        dead_incarnation.start += 1;
        let owner = Record {
            version: 4,
            build: None,
            kind: Kind::Native,
            application: crate::instance::APPLICATION_ID.into(),
            boot: proof.boot_id.clone(),
            terminal: issued.id,
            holder: dead_incarnation.clone(),
            root: dead_incarnation,
        };
        let metadata = std::fs::metadata(root.path()).unwrap();
        let scope = LaunchAccountScope {
            provider: Provider::Claude,
            canonical_home: root.path().canonicalize().unwrap(),
            device: metadata.dev(),
            inode: metadata.ino(),
            registered_id: None,
            purge_ids: vec![],
        };
        let actor = uuid::Uuid::new_v4();
        let path = root.path().join("registry.sqlite3");
        let mut registry = Registry::at(&path).unwrap();
        registry
            .register_native_actor_ownership(actor, &owner, &issued, Ok(&scope))
            .unwrap();
        assert!(registry.native_actor_retirement(actor).unwrap().is_none());
        registry
            .commit_native_actor_retirement(
                actor,
                &owner,
                &source,
                Ok(&scope),
                source.launch.session_id.as_deref(),
            )
            .unwrap();
        drop(registry);
        let registry = Registry::at(&path).unwrap();
        let ownership = registry
            .native_actor_ownerships(None, 128)
            .unwrap()
            .remove(0);
        let retirement = registry.native_actor_retirement(actor).unwrap().unwrap();
        let aliases = [issued];
        let mut evidence = Evidence {
            actor,
            owner: &owner,
            proof: &proof,
            source: &source,
            aliases: &aliases,
            scope: Ok(&scope),
            confirmed: source.launch.session_id.as_deref(),
            paired_holder: None,
        };
        validate_evidence(&source, &ownership, &retirement, &evidence, None).unwrap();
        let mut changed = source.clone();
        changed
            .launch
            .recipe
            .arguments
            .push("--different-recipe".into());
        assert!(validate_evidence(&changed, &ownership, &retirement, &evidence, None).is_err());
        changed = source.clone();
        changed.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("forged-alias");
        assert!(validate_evidence(&changed, &ownership, &retirement, &evidence, None).is_err());
        evidence.actor = uuid::Uuid::new_v4();
        assert!(validate_evidence(&source, &ownership, &retirement, &evidence, None).is_err());
        evidence.actor = actor;
        evidence.confirmed = None;
        assert!(validate_evidence(&source, &ownership, &retirement, &evidence, None).is_err());
        evidence.confirmed = source.launch.session_id.as_deref();
        let mut other_scope = scope.clone();
        other_scope.inode += 1;
        evidence.scope = Ok(&other_scope);
        assert!(validate_evidence(&source, &ownership, &retirement, &evidence, None).is_err());
        evidence.scope = Err("unconfirmed scope");
        assert!(validate_evidence(&source, &ownership, &retirement, &evidence, None).is_err());
        // Even an otherwise exact confirmed retirement cannot turn cleanup
        // intent into account/history read or disk-resume permission.
        let pending_actor = uuid::Uuid::new_v4();
        let mut registry = Registry::at(&path).unwrap();
        registry
            .register_pending_native_actor_ownership(pending_actor, &owner, &source, Ok(&scope))
            .unwrap();
        registry
            .commit_native_actor_retirement(
                pending_actor,
                &owner,
                &source,
                Ok(&scope),
                source.launch.session_id.as_deref(),
            )
            .unwrap();
        let pending_owner = registry
            .native_actor_ownership(pending_actor)
            .unwrap()
            .unwrap();
        let pending_retirement = registry
            .native_actor_retirement(pending_actor)
            .unwrap()
            .unwrap();
        evidence.actor = pending_actor;
        evidence.scope = Ok(&scope);
        assert!(
            validate_evidence(
                &source,
                &pending_owner,
                &pending_retirement,
                &evidence,
                None
            )
            .unwrap_err()
            .contains("Pending cleanup intent")
        );
    }
}
