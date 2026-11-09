//! Explicit disk resume requires completed retirement of the exact old actor.
//! This proves one predecessor closed; ordinary launch/account and concurrent
//! conversation admission still run for the new pane and its new writer.
use super::{Handle, Provider, Session, SharedHost};
use crate::chat::protocol::SetupError;
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

fn current_admission(
    host: &SharedHost,
    source: &Session,
    expected: &HostSessionProof,
) -> Result<(), SetupError> {
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
        return Err(SetupError::Unavailable(
            "Claude resume has a current owner or unsettled admission; no replacement was prepared"
                .into(),
        ));
    }
    Ok(())
}

/// The successor would select the exact physical account home the old actor
/// captured, so its provider reads the same on-disk conversation.
fn same_account_home(source: &Session, captured: &LaunchAccountScope) -> Result<(), String> {
    same_account_home_for_start(source, captured).map_err(|error| error.to_string())
}

fn same_account_home_for_start(
    source: &Session,
    captured: &LaunchAccountScope,
) -> Result<(), SetupError> {
    captured.revalidate_for_start().map_err(SetupError::from)?;
    if !same_scope(captured, &selected_account_home_for_start(source)?) {
        return Err("Claude disk resume would select a different physical account home".into());
    }
    Ok(())
}

/// The physical Claude account home `source`'s launch would select now.
fn selected_account_home(source: &Session) -> Result<LaunchAccountScope, String> {
    selected_account_home_for_start(source).map_err(|error| error.to_string())
}

fn selected_account_home_for_start(source: &Session) -> Result<LaunchAccountScope, SetupError> {
    let launch = super::prepare_native_process_launch(
        &source.launch,
        Some(crate::model::agent_recovery::fresh_native_environment(
            Provider::Claude,
            false,
        )),
        &Default::default(),
        None,
    )
    .map_err(|e| {
        if e.retryable {
            SetupError::Unavailable(e.message)
        } else {
            SetupError::Invalid(e.message)
        }
    })?;
    LaunchAccountScope::resolve_for_start(
        Provider::Claude,
        &launch.recipe.working_directory,
        |name| {
            launch
                .recipe
                .environment
                .get(name)
                .map(std::ffi::OsString::from)
        },
    )
    .map_err(SetupError::from)
}

/// Whether two captures from different kernel boots name the same physical
/// home. A device number is assigned when its filesystem is mounted, such as
/// an anonymous number for a btrfs subvolume or a device-mapper minor in
/// activation order, so it can differ after a restart while the home is
/// unchanged. Its canonical path and inode on that filesystem cannot. A home
/// that is itself a mount point shares its root inode with any filesystem of
/// that kind, so another volume mounted there in its place is not told apart.
fn same_home_across_boots(left: &LaunchAccountScope, right: &LaunchAccountScope) -> bool {
    left.provider == right.provider
        && left.canonical_home == right.canonical_home
        && left.inode == right.inode
}

/// The refusal when the recorded launch would now resume a conversation from
/// another account home than the one it last ran under before a restart.
pub(super) const PRIOR_BOOT_ACCOUNT_CHANGED: &str = "This conversation last ran under a different Claude account home before the computer restarted, so it was not resumed here. Continue it on another account to choose which account it runs under";

/// A kernel boot change proves an earlier owner of this pane is gone, but not
/// that the recorded launch still selects the account home it ran under. The
/// newest earlier-boot ownership row of the pane names that home; refuse if
/// the launch would now select another one. Rows are never deleted, and an
/// older boot's home may have been changed deliberately since, so only the
/// newest counts. A pane with no such row, or whose newest row captured no
/// home, has nothing to compare; the launch's own account checks still apply.
pub(super) fn ensure_prior_boot_account(host: &SharedHost, source: &Session) -> Result<(), String> {
    ensure_prior_boot_account_for_start(host, source).map_err(|error| error.to_string())
}

pub(super) fn ensure_prior_boot_account_for_start(
    host: &SharedHost,
    source: &Session,
) -> Result<(), SetupError> {
    let boot = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .boot_id
        .clone();
    let newest = super::super::agent_execution::registry(host)
        .map_err(SetupError::Unavailable)?
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .newest_native_actor_ownership_for_start(source.id, &boot)?;
    let Some(captured) = newest.as_ref().and_then(|ownership| ownership.scope().ok()) else {
        return Ok(());
    };
    // The launch's own failure to select a home is its own refusal, not an
    // account change.
    let selected = selected_account_home_for_start(source)?;
    if same_home_across_boots(captured, &selected) {
        return Ok(());
    }
    tracing::warn!(
        chat = %source.id,
        captured = %captured.canonical_home.display(),
        selected = %selected.canonical_home.display(),
        "earlier-boot Claude account home changed"
    );
    Err(SetupError::Invalid(PRIOR_BOOT_ACCOUNT_CHANGED.to_owned()))
}

const SURVIVOR: &str = "A process from this conversation's earlier Claude session is still running, so nothing was started in its place. Retry once it has exited";

/// Also the answer for a pane whose conversation had an open CLI view, since
/// that view's terminal could still hold the conversation.
const UNMATCHED: &str = "This conversation's earlier Claude process ended without recording its close, and cagents cannot prove that nothing else still holds the conversation, so nothing was started in its place. It can be resumed after this computer restarts";

/// An actor that ended without retirement grants disk resume only when no
/// writer of it can survive: its exact helper and provider are gone in this
/// boot with nothing left in their process group or session, it was never
/// cleanup intent, it had no paired CLI terminal, and it ran this pane's exact
/// launch under the account home the successor would select. Whatever it had
/// not acknowledged is lost with it and is never resubmitted; the successor
/// starts on the saved conversation with no input.
fn abandoned(
    ownership: &NativeActorOwnership,
    source: &Session,
    proof: &HostSessionProof,
) -> Result<(), String> {
    let issued = ownership.source();
    if ownership.is_pending()
        || issued.id != source.id
        || ownership.owner().terminal != source.id
        || issued.launch.provider != Provider::Claude
        || issued.launch.recipe != source.launch.recipe
        || issued
            .launch
            .session_id
            .as_ref()
            .is_some_and(|id| Some(id) != source.launch.session_id.as_ref())
        || issued.terminal.is_some()
        || source.terminal.is_some()
    {
        return Err(UNMATCHED.into());
    }
    if !Handle::inspect_abandoned_actor(ownership.owner(), proof)? {
        return Err(SURVIVOR.into());
    }
    let captured = ownership.scope().map_err(|_| {
        "The ended Claude actor has no captured physical account identity".to_owned()
    })?;
    same_account_home(source, captured)
}

/// How the pane's same-boot actors ended, as far as one census scan proves.
pub(super) enum ResumeRetirement {
    /// Nothing classified: not a retained pane, another boot, or an actor
    /// still running. Generic owner discovery decides.
    Unclassified,
    /// The single verified retirement, carried through the later checks
    /// rather than reopening its files.
    Verified(crate::chat::runtime::VerifiedRetirement),
    /// An actor that ended without retirement and left no surviving group
    /// or session. Its stale runtime record is not a live owner; the full
    /// ownership check still runs before any successor is prepared.
    Abandoned,
}

impl ResumeRetirement {
    pub(super) fn is_classified(&self) -> bool {
        !matches!(self, Self::Unclassified)
    }
    pub(super) fn verified(self) -> Option<crate::chat::runtime::VerifiedRetirement> {
        match self {
            Self::Verified(verified) => Some(verified),
            Self::Unclassified | Self::Abandoned => None,
        }
    }
}

/// Classify this exact durable pane before generic discovery can mistake an
/// exited, unreaped helper for a live owner.
pub(super) fn inspect_resume_retirement(
    host: &SharedHost,
    source: &Session,
) -> Result<ResumeRetirement, String> {
    inspect_resume_retirement_for_start(host, source).map_err(|error| error.to_string())
}

pub(super) fn inspect_resume_retirement_for_start(
    host: &SharedHost,
    source: &Session,
) -> Result<ResumeRetirement, SetupError> {
    if !matches!(
        source.native_continuity,
        crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
    ) {
        return Ok(ResumeRetirement::Unclassified);
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
        return Ok(ResumeRetirement::Unclassified);
    }
    let registry =
        super::super::agent_execution::registry(host).map_err(SetupError::Unavailable)?;
    let mut after = None;
    let mut verified = None;
    let mut running = false;
    let mut abandoned = false;
    loop {
        let page = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownerships_for_start(after, 128)?;
        if page.is_empty() {
            // The census is keyed by actor, not pane. A historical retired
            // actor cannot hide a current owner of the same pane allocation.
            // Scan every row first so Running never swallows an unproved exit,
            // receipt error, pending owner or ambiguous verified history.
            return Ok(match (running, verified) {
                (true, _) => ResumeRetirement::Unclassified,
                (false, Some(verified)) => ResumeRetirement::Verified(verified),
                (false, None) if abandoned => ResumeRetirement::Abandoned,
                (false, None) => ResumeRetirement::Unclassified,
            });
        }
        for ownership in page {
            after = Some(ownership.actor());
            if ownership.source().id != source.id {
                continue;
            }
            if ownership.is_pending() {
                let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
                let retirement = registry.native_actor_retirement_for_start(ownership.actor())?;
                let archived = match retirement.as_ref() {
                    Some(retirement) => registry
                        .archived_history_for_start(source, &ownership, retirement, &proof.boot_id)?
                        .is_some(),
                    None => false,
                };
                if !archived {
                    return Err("Pending cleanup intent never grants disk resume authority".into());
                }
            }
            match Handle::inspect_retained_retirement(ownership.owner(), &proof)
                .map_err(SetupError::Unavailable)?
            {
                crate::chat::runtime::RetirementInspection::Running => running = true,
                crate::chat::runtime::RetirementInspection::ExitedUnproved => {
                    // Disk resume decides separately whether this exit left
                    // no surviving writer; it is never retirement evidence.
                    if ownership.is_pending()
                        || !Handle::inspect_abandoned_actor(ownership.owner(), &proof)
                            .map_err(SetupError::Unavailable)?
                    {
                        return Err(
                            "The saved Claude actor exited without verified retirement".into()
                        );
                    }
                    abandoned = true;
                }
                crate::chat::runtime::RetirementInspection::Verified(receipt) => {
                    let retirement = registry
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .native_actor_retirement_for_start(ownership.actor())?
                        .ok_or("The saved Claude actor has no committed retirement")?;
                    if unconfirmed_retired_startup(&ownership, &retirement, &receipt)
                        .map_err(SetupError::Invalid)?
                    {
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

/// A resume successor the daemon prepared but that has no ownership proof yet.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Unstarted {
    /// Preparation recorded it in a lineage and it never started, so a second
    /// resume of it replaces no process and keeps that lineage, with the agent
    /// it was prepared to take over. Only a start issues an ownership proof,
    /// and every Claude start registers its retained actor before it runs;
    /// neither exists for this allocation.
    InLineage,
    /// No lineage records it here, so it resumes as a conversation of its own.
    Unrecorded,
}

const STARTED_HERE: &str = "This pane's earlier Claude process may still be running, so nothing was started in its place. Retry once it has exited";

const OPEN_ELSEWHERE: &str = "Another pane may still be writing to this Claude conversation, so nothing was started. Close that pane, then resume this conversation again";

/// Whether `source` is a resume successor that never started, and how it
/// resumes. Nothing in this daemon may run, start or admit its conversation,
/// and every other registered owner of that conversation must be proved
/// ended, since a second process on it would be a second writer.
pub(super) fn unstarted(host: &SharedHost, source: &Session) -> Result<Option<Unstarted>, String> {
    unstarted_for_start(host, source).map_err(|error| error.to_string())
}

pub(super) fn unstarted_for_start(
    host: &SharedHost,
    source: &Session,
) -> Result<Option<Unstarted>, SetupError> {
    if source.launch.provider != Provider::Claude
        || source.native_continuity != crate::chat::NativeContinuity::ReconnectOnly
        || source.proof.is_some()
        || source.terminal.is_some()
        || source.launch.session_id.is_none()
    {
        return Ok(None);
    }
    let proof = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.chats.contains_key(&source.id) || state.chat_handoffs.contains_key(&source.id) {
            return Err(SetupError::Unavailable(
                "This conversation is already open, so nothing was started".into(),
            ));
        }
        // An idle process on the conversation can still write to it.
        if state.chats.values().any(|record| {
            let mut launch = record.session.launch.clone();
            launch.session_id = record.handle.confirmed_session_id();
            super::same_conversation(&launch, &source.launch)
                || super::same_conversation(&record.session.launch, &source.launch)
        }) {
            return Err(SetupError::Unavailable(OPEN_ELSEWHERE.into()));
        }
        state.proof().clone()
    };
    current_admission(host, source, &proof)?;
    let registry =
        super::super::agent_execution::registry(host).map_err(SetupError::Unavailable)?;
    let mut after = None;
    loop {
        let page = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownerships_for_start(after, 128)?;
        if page.is_empty() {
            break;
        }
        for ownership in page {
            after = Some(ownership.actor());
            // An owner registered for this very pane started it before the
            // card saved its proof, possibly before naming the conversation,
            // so it counts whatever conversation it records. A kernel boot
            // change ended every process of an earlier boot.
            let this_pane = ownership.source().id == source.id;
            if !(this_pane || super::same_conversation(&ownership.source().launch, &source.launch))
                || ownership.owner().boot != proof.boot_id
            {
                continue;
            }
            let retirement = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .native_actor_retirement_for_start(ownership.actor())?;
            let ended = match retirement {
                Some(retirement) => !ownership.is_pending() && !retirement.is_pending(),
                None => {
                    !ownership.is_pending()
                        && Handle::inspect_abandoned_actor(ownership.owner(), &proof)
                            .map_err(SetupError::Unavailable)?
                }
            };
            if !ended {
                return Err(SetupError::Unavailable(
                    if this_pane {
                        STARTED_HERE
                    } else {
                        OPEN_ELSEWHERE
                    }
                    .into(),
                ));
            }
        }
    }
    let recorded = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .session_lineage(source.id)
        .map_err(SetupError::Unavailable)?
        .is_some();
    Ok(Some(if recorded {
        Unstarted::InLineage
    } else {
        Unstarted::Unrecorded
    }))
}

const UNRECORDED: &str = "The saved Claude actor ended without a retirement that records which conversation it confirmed, so nothing was resumed or started in its place. Its conversation, if it began one, can be found and reopened with Resume";

/// What a retained Claude pane that never learned its conversation ID
/// resumes. Every actor of the pane must be proven ended (a kernel boot
/// change, a verified retirement, or an exit that left no surviving writer)
/// and retired, since only a retirement records the conversation an actor
/// confirmed. `Some` is the one conversation those actors confirmed. `None`
/// proves that none confirmed one, so the pane starts a conversation in
/// place: Claude names a conversation it was given before it answers
/// anything, so an actor that never confirmed one never had a prompt
/// accepted.
pub(super) fn unnamed_conversation(
    host: &SharedHost,
    source: &Session,
) -> Result<Option<String>, String> {
    unnamed_conversation_for_start(host, source).map_err(|error| error.to_string())
}

pub(super) fn unnamed_conversation_for_start(
    host: &SharedHost,
    source: &Session,
) -> Result<Option<String>, SetupError> {
    if !source.has_unnamed_retained_claude_actor() {
        return Err("Only an unnamed retained Claude pane resumes without its conversation".into());
    }
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    let Some(saved) = source.proof.as_ref().filter(|saved| saved.is_complete()) else {
        return Err("The unnamed Claude pane has no complete ownership proof".into());
    };
    let previous_boot = saved.boot_id != proof.boot_id;
    let registry =
        super::super::agent_execution::registry(host).map_err(SetupError::Unavailable)?;
    let mut after = None;
    let mut found = false;
    let mut learned: Option<String> = None;
    loop {
        let page = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownerships_for_start(after, 128)?;
        if page.is_empty() {
            break;
        }
        for ownership in page {
            after = Some(ownership.actor());
            if ownership.source().id != source.id {
                continue;
            }
            found = true;
            if ownership.is_pending() {
                return Err("Pending cleanup intent never grants disk resume authority".into());
            }
            let retirement = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .native_actor_retirement_for_start(ownership.actor())?;
            if retirement.as_ref().is_some_and(|r| r.is_pending()) {
                return Err("Pending cleanup retirement never grants disk resume authority".into());
            }
            // Only a retirement records what the actor confirmed. An actor
            // that ended without one may have confirmed a conversation that
            // nothing durable names, so it never proves there is none.
            let recorded = retirement
                .as_ref()
                .map(|retirement| retirement.confirmed_session().map(str::to_owned))
                .ok_or(UNRECORDED);
            let confirmed = if ownership.owner().boot != proof.boot_id {
                recorded?
            } else {
                match Handle::inspect_retained_retirement(ownership.owner(), &proof)
                    .map_err(SetupError::Unavailable)?
                {
                    crate::chat::runtime::RetirementInspection::Running => {
                        return Err(SetupError::Unavailable(
                            "The saved Claude actor still executes; no replacement was prepared"
                                .into(),
                        ));
                    }
                    crate::chat::runtime::RetirementInspection::ExitedUnproved => {
                        if !Handle::inspect_abandoned_actor(ownership.owner(), &proof)
                            .map_err(SetupError::Unavailable)?
                        {
                            return Err(SURVIVOR.into());
                        }
                        recorded?
                    }
                    crate::chat::runtime::RetirementInspection::Verified(verified) => {
                        verified.receipt().confirmed_session.clone()
                    }
                }
            };
            let Some(confirmed) = confirmed else {
                continue;
            };
            if uuid::Uuid::parse_str(&confirmed).is_err()
                || learned
                    .replace(confirmed.clone())
                    .is_some_and(|earlier| earlier != confirmed)
            {
                return Err(
                    "The unnamed Claude pane's actors confirmed different conversations".into(),
                );
            }
        }
    }
    // An actor is registered before it starts, so a same-boot pane without
    // one has no ended actor to vouch for; a live record decides instead.
    if !found && !previous_boot {
        return Err("cagents has no record of this conversation's earlier Claude process, so it cannot prove that process ended. Nothing was started in its place".into());
    }
    Ok(learned)
}

/// Called only for explicit resume, before the separate Codex close reader.
/// Returning true grants no provider start, account override or input replay.
pub(super) fn prepare_resume(host: &SharedHost, source: &Session) -> Result<bool, String> {
    prepare_resume_verified(host, source, None)
}

pub(super) fn prepare_resume_verified(
    host: &SharedHost,
    source: &Session,
    verified: Option<crate::chat::runtime::VerifiedRetirement>,
) -> Result<bool, String> {
    prepare_resume_verified_for_start(host, source, verified).map_err(|error| error.to_string())
}

pub(super) fn prepare_resume_verified_for_start(
    host: &SharedHost,
    source: &Session,
    mut verified: Option<crate::chat::runtime::VerifiedRetirement>,
) -> Result<bool, SetupError> {
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
    let paths = crate::daemon::RuntimePaths::discover()
        .map_err(|e| SetupError::Unavailable(e.to_string()))?;
    if !proof.is_complete()
        || crate::daemon::lease_owner(&paths.lock, &proof)
            .map_err(|e| SetupError::Unavailable(e.to_string()))?
            != std::process::id()
        || source
            .proof
            .as_ref()
            .is_none_or(|old| old.boot_id != proof.boot_id)
    {
        return Err("Claude resume reader does not own this boot's current daemon lease".into());
    }
    super::super::account_lifecycle::ensure_idle(host).map_err(|failure| {
        if failure.retryable {
            SetupError::Unavailable(failure.message)
        } else {
            SetupError::Invalid(failure.message)
        }
    })?;
    current_admission(host, source, &proof)?;
    let registry =
        super::super::agent_execution::registry(host).map_err(SetupError::Unavailable)?;
    let mut after = None;
    let mut matched = false;
    let mut archived_history = None;
    loop {
        let page = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_actor_ownerships_for_start(after, 128)?;
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
            if ownership.is_pending() && ownership.source().id != source.id {
                return Err("Pending cleanup intent never grants disk resume authority".into());
            }
            let retirement = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .native_actor_retirement_for_start(ownership.actor())?;
            if retirement
                .as_ref()
                .is_some_and(|receipt| receipt.is_pending())
                && ownership.source().id != source.id
            {
                return Err("Pending cleanup retirement never grants disk resume authority".into());
            }
            if ownership.source().id != source.id {
                // A known second writer is never made safe by this actor's
                // receipt. Unknown provider identities remain the ordinary
                // launch admission's responsibility, not an inferred absence.
                if super::same_conversation(&ownership.source().launch, &source.launch)
                    && retirement.is_none()
                    && (ownership.is_pending()
                        || !Handle::inspect_abandoned_actor(ownership.owner(), &proof)
                            .map_err(SetupError::Unavailable)?)
                {
                    return Err(SetupError::Unavailable(
                        "Another registered Claude owner has no completed retirement".into(),
                    ));
                }
                continue;
            }
            let Some(retirement) = retirement else {
                abandoned(&ownership, source, &proof).map_err(SetupError::Invalid)?;
                if matched {
                    return Err("The saved Claude pane matches more than one ended actor".into());
                }
                matched = true;
                continue;
            };
            let archived = if ownership.is_pending() || retirement.is_pending() {
                Some(
                    registry
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .archived_history_for_start(
                            source,
                            &ownership,
                            &retirement,
                            &proof.boot_id,
                        )?
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
                Handle::inspect_retained_retirement(ownership.owner(), &proof)
                    .map_err(SetupError::Unavailable)?
            };
            let verified = match inspection {
                crate::chat::runtime::RetirementInspection::Verified(verified) => verified,
                crate::chat::runtime::RetirementInspection::Running => {
                    return Err(SetupError::Unavailable(
                        "The saved Claude actor still executes; no replacement was prepared".into(),
                    ));
                }
                crate::chat::runtime::RetirementInspection::ExitedUnproved => {
                    if retirement.is_pending() {
                        return Err(
                            "The saved Claude actor has no authenticated final retirement proof"
                                .into(),
                        );
                    }
                    abandoned(&ownership, source, &proof).map_err(SetupError::Invalid)?;
                    if matched {
                        return Err(
                            "The saved Claude pane matches more than one ended actor".into()
                        );
                    }
                    matched = true;
                    continue;
                }
            };
            if unconfirmed_retired_startup(&ownership, &retirement, &verified)
                .map_err(SetupError::Invalid)?
            {
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
            )
            .map_err(SetupError::Invalid)?;
            if matched {
                return Err("The saved Claude pane matches more than one retired actor".into());
            }
            let captured = evidence
                .scope
                .map_err(|message| SetupError::Invalid(message.into()))?;
            same_account_home_for_start(source, captured)?;
            match (&evidence.source.terminal, evidence.paired_holder) {
                (None, None) => {}
                (Some(terminal), Some(pair))
                    if terminal.id == pair.terminal && pair.terminal == source.id =>
                {
                    let role = OriginalRole::authenticated(pair, evidence.proof, &proof)
                        .map_err(SetupError::Invalid)?;
                    if !crate::terminal::holder::cleanup::confirmed_owner_with_role(
                        pair,
                        &proof,
                        role.as_ref(),
                    )
                    .map_err(SetupError::Invalid)?
                    {
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
            archived_history = archived;
        }
    }
    if let Some(archived) = &archived_history {
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .recheck_archived_history_for_start(archived)?;
    }
    current_admission(host, source, &proof)?;
    // This is a fresh current-lease check after all disk and account reads.
    if crate::daemon::lease_owner(&paths.lock, &proof)
        .map_err(|e| SetupError::Unavailable(e.to_string()))?
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
