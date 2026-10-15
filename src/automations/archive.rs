//! Qualification for saved automation panes across application replacement.
//! A receipt proves readable immutable history, never a live provider.
use super::ArchivedConversation;
use crate::{agent_execution::HostedSession, chat::Session, model::AgentId};

pub const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
/// First deliberately versioned registry checkpoint defining archive authority.
/// The campaign owner must renumber this floor with its final migration union.
#[cfg(feature = "host")]
pub const ARCHIVE_REGISTRY_SCHEMA_FLOOR: u32 = 13;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedPaneReadiness {
    agent: AgentId,
    session: Session,
    terminal: Option<crate::model::TerminalSession>,
}
impl ArchivedPaneReadiness {
    pub fn qualify(
        agent: AgentId,
        session: &Session,
        archive: &ArchivedConversation,
    ) -> Result<Self, String> {
        let snapshot = &archive.snapshot;
        if archive.agent.id != agent
            || !archive.agent.closed
            || !matches!(
                archive.agent.origin,
                crate::agent_execution::Origin::Automation { .. }
            )
            || !receipt_native_descriptor_matches(archive, session)
            || archive
                .agent
                .session
                .as_ref()
                .and_then(|saved| match saved {
                    HostedSession::Chat(saved) => saved.launch.session_id.as_ref(),
                    _ => None,
                })
                != snapshot.session_id.as_ref()
            || !snapshot.exited
            || snapshot.ready
            || snapshot.busy
            || snapshot.waiting
            || snapshot
                .read_only_reason
                .as_ref()
                .is_none_or(|reason| reason.is_empty())
            || !snapshot.requests.is_empty()
            || !snapshot.queued_messages.is_empty()
            || snapshot.capabilities != Default::default()
            || snapshot.items.len() > 256
            || !snapshot.subagents.is_empty()
        {
            return Err("The saved automation pane has no exact read-only archive receipt".into());
        }
        if serde_json::to_vec(snapshot)
            .map_err(|_| "The saved automation archive is unreadable")?
            .len()
            > MAX_SNAPSHOT_BYTES
        {
            return Err(
                "The saved automation archive exceeds its bounded presentation limit".into(),
            );
        }
        Ok(Self {
            agent,
            session: session.clone(),
            terminal: None,
        })
    }
    pub fn matches(&self, agent: AgentId, session: &Session) -> bool {
        self.agent == agent && &self.session == session
    }
    pub fn matches_terminal(
        &self,
        agent: AgentId,
        terminal: &crate::model::TerminalSession,
    ) -> bool {
        self.agent == agent && self.terminal.as_ref() == Some(terminal)
    }

    pub fn qualify_kind(
        agent: AgentId,
        kind: &crate::model::CardKind,
        archive: &ArchivedConversation,
    ) -> Result<Self, String> {
        if !closed_pane_candidate_matches(&archive.agent, kind) {
            return Err("The saved automation pane has a different exact descriptor".into());
        }
        match kind {
            crate::model::CardKind::Chat(session) => Self::qualify(agent, session, archive),
            crate::model::CardKind::Terminal(terminal) => {
                let Some(HostedSession::Chat(session)) = &archive.agent.session else {
                    return Err("The saved automation has no original native conversation".into());
                };
                let mut receipt = Self::qualify(agent, session, archive)?;
                receipt.terminal = Some(terminal.clone());
                Ok(receipt)
            }
            _ => Err("This pane is not a saved automation conversation".into()),
        }
    }
}

fn supported_archive_continuity(session: &Session) -> bool {
    session.has_valid_native_continuity() && match &session.native_continuity {
        crate::chat::NativeContinuity::ReconnectOnly => false,
        crate::chat::NativeContinuity::Durable { provider_version } => {
            session.launch.provider == crate::agent::Provider::Codex
                && crate::chat::VERIFIED_CODEX_VERSIONS.contains(&provider_version.as_str())
        }
    }
}

/// A metadata candidate authorizes only a private archive read/Loading fence.
/// It never proves readiness or grants provider attach/start permission. Only
/// a fully qualified private snapshot below can prove the promoted marker.
pub(crate) fn native_archive_candidate_matches(
    agent: &crate::agent_execution::Agent,
    current: &Session,
) -> bool {
    if !agent.closed || !matches!(agent.origin, crate::agent_execution::Origin::Automation { .. }) {
        return false;
    }
    if native_descriptor_matches(agent, current) {
        return true;
    }
    let Some(HostedSession::Chat(canonical)) = &agent.session else { return false; };
    match (&canonical.native_continuity, &current.native_continuity) {
        (crate::chat::NativeContinuity::ReconnectOnly, _) if supported_archive_continuity(current) => {
            let mut promoted = agent.clone();
            let Some(HostedSession::Chat(source)) = &mut promoted.session else { unreachable!() };
            source.native_continuity = current.native_continuity.clone();
            native_descriptor_matches(&promoted, current)
        }
        (_, crate::chat::NativeContinuity::ReconnectOnly) if supported_archive_continuity(canonical) => {
            let mut old = current.clone();
            old.native_continuity = canonical.native_continuity.clone();
            native_descriptor_matches(agent, &old)
        }
        _ => false,
    }
}

fn receipt_native_descriptor_matches(archive: &ArchivedConversation, current: &Session) -> bool {
    let Some(HostedSession::Chat(canonical)) = &archive.agent.session else { return false; };
    let supported = |source: &Session| matches!(source.native_continuity, crate::chat::NativeContinuity::ReconnectOnly)
        || supported_archive_continuity(source);
    let mut reported = canonical.clone();
    reported.native_continuity = archive.snapshot.native_continuity.clone();
    if !supported(canonical) || !supported(current) || !supported(&reported)
        || !matches!(canonical.native_continuity, crate::chat::NativeContinuity::ReconnectOnly)
            && canonical.native_continuity != reported.native_continuity
    {
        return false;
    }
    if native_descriptor_matches(&archive.agent, current) {
        return true;
    }
    if !native_archive_candidate_matches(&archive.agent, current) {
        return false;
    }
    let marker = if matches!(canonical.native_continuity, crate::chat::NativeContinuity::ReconnectOnly) {
        &current.native_continuity
    } else {
        &canonical.native_continuity
    };
    // The writer captures this marker from the exact proved Handle. Metadata
    // candidates and caller-supplied versions cannot substitute for that row.
    &archive.snapshot.native_continuity == marker
}

pub(crate) fn native_descriptor_matches(
    agent: &crate::agent_execution::Agent,
    current: &Session,
) -> bool {
    let Some(HostedSession::Chat(saved)) = &agent.session else {
        return false;
    };
    let Some(thread) = saved
        .launch
        .session_id
        .as_deref()
        .filter(|thread| !thread.trim().is_empty())
    else {
        return false;
    };
    if agent.spec.provider != current.launch.provider
        || saved.id != current.id
        || saved.proof != current.proof
        || !current
            .proof
            .as_ref()
            .is_some_and(crate::model::HostSessionProof::is_complete)
        || saved.launch.provider != current.launch.provider
        || saved.launch.recipe != current.launch.recipe
        || saved.owned_codex_listener != current.owned_codex_listener
        || saved.native_continuity != current.native_continuity
        || current
            .launch
            .session_id
            .as_deref()
            .is_some_and(|current| current != thread)
    {
        return false;
    }
    if saved.terminal == current.terminal {
        return true;
    }
    // Only the exact paired console issued by this same closed owner can be
    // learned from None or added for the local read-only presentation.
    let console_matches = |terminal: &crate::model::TerminalSession| {
        agent.console.as_ref() == Some(terminal)
            && terminal.id == saved.id
            && terminal.launch.arguments
                == [
                    "agent".to_owned(),
                    "console".to_owned(),
                    agent.id.to_string(),
                ]
            && terminal.continuity.live_proof().as_ref() == saved.proof.as_ref()
    };
    match (&saved.terminal, &current.terminal) {
        (None, Some(terminal)) | (Some(terminal), None) => console_matches(terminal),
        _ => false,
    }
}

fn closed_pane_candidate_matches(
    agent: &crate::agent_execution::Agent,
    kind: &crate::model::CardKind,
) -> bool {
    if !agent.closed
        || !matches!(
            agent.origin,
            crate::agent_execution::Origin::Automation { .. }
        )
    {
        return false;
    }
    match kind {
        crate::model::CardKind::Chat(current) => native_archive_candidate_matches(agent, current),
        crate::model::CardKind::Terminal(current) => {
            agent.console.as_ref().is_some_and(|original| {
                let Some(HostedSession::Chat(native)) = &agent.session else {
                    return false;
                };
                if original.id != native.id
                    || !original.continuity.live_proof().is_some_and(|proof| {
                        proof.is_complete() && native.proof.as_ref() == Some(&proof)
                    })
                {
                    return false;
                }
                let mut original = original.clone();
                if matches!(
                    current.continuity,
                    crate::model::TerminalContinuity::Exited { .. }
                        | crate::model::TerminalContinuity::RelaunchAvailable {
                            last_exit: Some(_),
                            ..
                        }
                ) {
                    original.continuity = current.continuity.clone();
                }
                original == *current
            })
        }
        _ => false,
    }
}

/// A window closed before completion has not saved its archive map yet. The
/// exact canonical closed owner and its immutable descriptor can establish that
/// binding read-only, including an original paired terminal presentation.
#[cfg(feature = "host")]
pub fn inspect_completed_automation_pane(
    database: &std::path::Path,
    registry: &std::path::Path,
    card: crate::model::CardId,
    kind: &crate::model::CardKind,
) -> Result<Option<ArchivedPaneReadiness>, String> {
    use crate::model::{CardKind, Existence};
    use rusqlite::{Connection, OpenFlags, OptionalExtension};
    if !matches!(kind, CardKind::Chat(_) | CardKind::Terminal(_)) {
        return Ok(None);
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut source = Connection::open_with_flags(database, flags)
        .map_err(|_| "The source automation pane is unreadable")?;
    source
        .busy_timeout(std::time::Duration::from_millis(100))
        .map_err(|_| "The source automation pane is unavailable")?;
    let tx = source
        .transaction()
        .map_err(|_| "Cannot inspect the source automation pane")?;
    let row:Option<(String,String,String,Option<String>,String)>=tx.query_row(
        "SELECT c.workspace_id,CASE WHEN length(CAST(c.existence_json AS BLOB))<=4096 THEN c.existence_json ELSE 'null' END,c.kind_tag,CASE WHEN length(CAST(c.content_json AS BLOB))<=4194304 THEN c.content_json ELSE NULL END,CASE WHEN length(CAST(w.existence_json AS BLOB))<=4096 THEN w.existence_json ELSE 'null' END FROM cards c JOIN workspaces w ON w.workspace_id=c.workspace_id WHERE c.card_id=?1",
        [card.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).optional().map_err(|_|"The source automation pane is unreadable")?;
    let Some((workspace, existence, tag, content, workspace_existence)) = row else {
        return Ok(None);
    };
    let bindings:Option<Option<String>>=tx.query_row("SELECT CASE WHEN length(CAST(value AS BLOB))<=1048576 THEN value ELSE NULL END FROM metadata WHERE key=?1",[format!("workspace:{workspace}:navigation:automation_archives")],|row|row.get(0)).optional().map_err(|_|"The source archive binding is unreadable")?;
    let claim = match bindings {
        None => None,
        Some(value) => {
            let value = value.ok_or("The source archive binding exceeds its inspection limit")?;
            let bindings: std::collections::HashMap<crate::model::CardId, AgentId> =
                serde_json::from_str(&value)
                    .map_err(|_| "The source archive binding is invalid")?;
            bindings.get(&card).copied()
        }
    };
    let expected = AgentId::from_uuid(card.as_uuid());
    if claim.is_some_and(|id| id != expected) {
        return Err("The source archive binding changed".into());
    }
    let existence: Existence =
        serde_json::from_str(&existence).map_err(|_| "The source pane existence is invalid")?;
    let workspace_existence: Existence = serde_json::from_str(&workspace_existence)
        .map_err(|_| "The source workspace existence is invalid")?;
    let saved = crate::persistence::read_migration_session_at(&tx, card, &tag, content.as_deref())
        .map_err(
            |_| "The source automation descriptor is invalid or exceeds its inspection limit",
        )?;
    if !existence.is_open() || !workspace_existence.is_open() || &saved != kind {
        return Err("The source automation pane changed".into());
    }
    match std::fs::symlink_metadata(registry) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && claim.is_none() => {
            return Ok(None);
        }
        Err(_) => return Err("The claimed automation archive is unavailable".into()),
        Ok(_) => {}
    }
    let model = Connection::open_with_flags(registry, flags)
        .map_err(|_| "The source automation registry is unreadable")?;
    model
        .busy_timeout(std::time::Duration::from_millis(100))
        .map_err(|_| "The source automation registry is unavailable")?;
    let registry_version: u32 = model
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|_| "The source automation registry version is unreadable")?;
    if registry_version > crate::host::WORKSPACE_REGISTRY_SCHEMA_VERSION {
        return Err("The source automation registry requires a newer archive inspector".into());
    }
    let schema:bool=model.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='automation_snapshots')",[],|row|row.get(0)).map_err(|_|"The source automation registry is unreadable")?;
    if !schema {
        return if claim.is_some() {
            Err("The claimed automation archive schema is missing".into())
        } else {
            Ok(None)
        };
    }
    if registry_version < ARCHIVE_REGISTRY_SCHEMA_FLOOR {
        return Err("The source automation archive has no supported versioned authority".into());
    }
    let owner:Option<String>=model.query_row("SELECT record FROM agents WHERE id=?1 AND closed=1 AND json_type(record,'$.origin.Automation')='object' AND length(CAST(record AS BLOB))<=4194304",[expected.to_string()],|row|row.get(0)).optional().map_err(|_|"The source automation owner is unreadable")?;
    let Some(owner) = owner else {
        return if claim.is_some() {
            Err("The claimed closed automation owner is missing".into())
        } else {
            Ok(None)
        };
    };
    let agent =
        serde_json::from_str(&owner).map_err(|_| "The source automation owner is invalid")?;
    if !closed_pane_candidate_matches(&agent, kind) {
        return if claim.is_some() {
            Err("The claimed automation descriptor changed".into())
        } else {
            Ok(None)
        };
    }
    let snapshot:String=model.query_row("SELECT snapshot FROM automation_snapshots WHERE agent=?1 AND length(CAST(snapshot AS BLOB))<=4194304 ORDER BY rowid DESC LIMIT 1",[expected.to_string()],|row|row.get(0)).map_err(|_|"The completed automation archive is missing or unreadable")?;
    let archive = ArchivedConversation {
        agent,
        snapshot: serde_json::from_str(&snapshot)
            .map_err(|_| "The completed automation archive payload is invalid")?,
    };
    ArchivedPaneReadiness::qualify_kind(expected, kind, &archive).map(Some)
}

/// Compatibility entry point for a native Chat. Keep schema admission and
/// exact source qualification on the same read-only path as Terminal panes.
#[cfg(feature = "host")]
pub fn inspect_archived_pane(
    database: &std::path::Path,
    registry: &std::path::Path,
    card: crate::model::CardId,
    session: &Session,
) -> Result<Option<ArchivedPaneReadiness>, String> {
    inspect_completed_automation_pane(database, registry, card, &crate::model::CardKind::Chat(session.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Agent, Origin, Placement, Spec, Surface, Target},
        chat::Snapshot,
        model::{CardKind, TerminalId},
    };
    fn fixture() -> (AgentId, Session, ArchivedConversation) {
        let agent = AgentId::new();
        let path = std::path::PathBuf::from("/fixture");
        let mut session = crate::chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            path.clone(),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        session.proof = Some(crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("archive-host"),
            boot_id: crate::model::BootId::new("archive-boot"),
            host_incarnation: crate::model::HostIncarnation::new("archive-incarnation"),
        });
        session.launch.session_id = Some("confirmed-archive-thread".into());
        let record = Agent {
            name: None,
            id: agent,
            origin: Origin::Automation {
                id: uuid::Uuid::new_v4(),
            },
            spec: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository: path.clone(),
                    directory: path.clone(),
                    placement: None,
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: Placement::Worktree {
                repository: path.clone(),
                path,
            },
            session: Some(HostedSession::Chat(session.clone())),
            console: None,
            created_ms: 1,
            closed: true,
        };
        let snapshot = Snapshot {
            session_id: session.launch.session_id.clone(),
            exited: true,
            read_only_reason: Some("Completed scheduled conversation".into()),
            ..Default::default()
        };
        (
            agent,
            session,
            ArchivedConversation {
                agent: record,
                snapshot,
            },
        )
    }
    fn promoted_archive_fixture() -> (AgentId, Session, Session, ArchivedConversation) {
        let (id, original, mut archive) = fixture();
        let mut source = crate::chat::fresh_session(Provider::Codex, original.id,
            original.launch.recipe.working_directory.clone(), &crate::account::Account::system(Provider::Codex)).unwrap();
        source.proof = original.proof;
        let proof = source.proof.clone().unwrap();
        assert!(source.claim_owned_codex_listener(&std::env::temp_dir(), &proof));
        source.launch.session_id = original.launch.session_id;
        archive.agent.spec.provider = Provider::Codex;
        archive.agent.session = Some(HostedSession::Chat(source.clone()));
        archive.snapshot.native_continuity = crate::chat::NativeContinuity::Durable { provider_version: "0.162.0".into() };
        let mut promoted = source.clone();
        promoted.native_continuity = archive.snapshot.native_continuity.clone();
        (id,source,promoted,archive)
    }

    #[test]
    fn archived_native_promotion_requires_the_exact_supported_private_snapshot_marker() {
        let (id,source,promoted,archive) = promoted_archive_fixture();
        assert!(!native_descriptor_matches(&archive.agent, &promoted), "global strict matcher stays unchanged");
        for saved in [&source,&promoted] {
            assert!(native_archive_candidate_matches(&archive.agent, saved));
            assert!(ArchivedPaneReadiness::qualify(id,saved,&archive).unwrap().matches(id,saved));
        }
        let mut missing = archive.clone();
        missing.snapshot.native_continuity = crate::chat::NativeContinuity::ReconnectOnly;
        assert!(ArchivedPaneReadiness::qualify(id,&promoted,&missing).is_err(), "metadata candidate alone is not a receipt");
        let mut different = promoted.clone();
        different.native_continuity = crate::chat::NativeContinuity::Durable { provider_version: "0.160.1".into() };
        assert!(native_archive_candidate_matches(&archive.agent,&different));
        assert!(ArchivedPaneReadiness::qualify(id,&different,&archive).is_err());
        let mut canonical = archive.clone();
        canonical.agent.session = Some(HostedSession::Chat(promoted.clone()));
        assert!(ArchivedPaneReadiness::qualify(id,&source,&canonical).is_ok(), "an issued default source can learn the same snapshot-proved marker");
        let mut missing_known = canonical.clone();
        missing_known.snapshot.native_continuity = crate::chat::NativeContinuity::ReconnectOnly;
        for saved in [&source,&promoted] {
            assert!(ArchivedPaneReadiness::qualify(id,saved,&missing_known).is_err(), "a known canonical marker requires the same private snapshot marker, including strict-descriptor matches");
        }
        assert!(!native_archive_candidate_matches(&canonical.agent,&different), "different known markers never match");
        canonical.snapshot.native_continuity = different.native_continuity;
        assert!(ArchivedPaneReadiness::qualify(id,&source,&canonical).is_err());
        assert!(ArchivedPaneReadiness::qualify(id,&promoted,&canonical).is_err(), "snapshot and known canonical markers cannot disagree");
        for field in 0..5 {
            let mut changed = promoted.clone();
            match field {
                0 => changed.owned_codex_listener.as_mut().unwrap().socket = "/unrelated/control.sock".into(),
                1 => changed.launch.session_id = Some("unrelated-confirmed-thread".into()),
                2 => changed.proof = None,
                3 => { changed.launch.recipe.environment.insert("CHANGED_ACCOUNT".into(),"other".into()); }
                _ => changed.native_continuity = crate::chat::NativeContinuity::Durable { provider_version: "999.999.999".into() },
            }
            let mut forged = archive.clone();
            forged.snapshot.native_continuity = changed.native_continuity.clone();
            assert!(ArchivedPaneReadiness::qualify(id,&changed,&forged).is_err(), "invalid source field {field}");
        }
    }

    #[cfg(feature = "host")]
    #[test]
    fn persisted_default_and_promoted_archive_panes_qualify_without_rewriting_source() {
        use rusqlite::{Connection,params};
        let directory = tempfile::tempdir().unwrap();
        let registry = directory.path().join("registry.sqlite3");
        let (id,source,promoted,archive) = promoted_archive_fixture();
        let model = Connection::open(&registry).unwrap();
        model.execute_batch("CREATE TABLE agents(id TEXT,closed INTEGER,record TEXT);CREATE TABLE automation_snapshots(agent TEXT,snapshot TEXT);").unwrap();
        model.pragma_update(None,"user_version",ARCHIVE_REGISTRY_SCHEMA_FLOOR).unwrap();
        model.execute("INSERT INTO agents VALUES(?1,1,?2)",params![id.to_string(),serde_json::to_string(&archive.agent).unwrap()]).unwrap();
        model.execute("INSERT INTO automation_snapshots VALUES(?1,?2)",params![id.to_string(),serde_json::to_string(&archive.snapshot).unwrap()]).unwrap();
        drop(model);
        let card = crate::model::CardId::from_uuid(id.as_uuid());
        for (index,saved) in [source,promoted].into_iter().enumerate() {
            for claim in [false,true] {
                let path = directory.path().join(format!("source-{index}-{claim}.sqlite3"));
                let store = crate::persistence::StateStore::open(&path,crate::model::BootId::new("archive-boot")).unwrap();
                let mut workspace = crate::model::WorkspaceState::default();
                workspace.cards.push(crate::model::CardState {
                    id: card,generation:0,title:"Saved native result".into(),width_fraction:0.6,
                    existence:crate::model::Existence::Open,exposure:crate::model::Exposure::Offscreen,
                    residency:crate::model::Residency::Cold,kind:CardKind::Chat(saved.clone()),
                });
                if claim { workspace.automation_archives.insert(card,id); }
                store.save_workspace(&workspace,crate::persistence::SnapshotBase { workspace:0,cards:&Default::default() }).unwrap();
                let before = (std::fs::read(&path).unwrap(),std::fs::read(&registry).unwrap());
                assert!(inspect_completed_automation_pane(&path,&registry,card,&CardKind::Chat(saved.clone())).unwrap().unwrap().matches(id,&saved));
                assert!(inspect_archived_pane(&path,&registry,card,&saved).unwrap().unwrap().matches(id,&saved));
                assert_eq!(before,(std::fs::read(&path).unwrap(),std::fs::read(&registry).unwrap()),"read-only source qualification");
            }
        }
    }

    #[test]
    fn archive_receipt_requires_exact_closed_read_only_history() {
        let (id, session, archive) = fixture();
        let receipt = ArchivedPaneReadiness::qualify(id, &session, &archive).unwrap();
        assert!(receipt.matches(id, &session));
        let mut changed = session.clone();
        changed.id = TerminalId::new();
        assert!(!receipt.matches(id, &changed));
        let mut unissued = archive.clone();
        let Some(HostedSession::Chat(unissued_session)) = unissued.agent.session.as_mut() else {
            unreachable!()
        };
        unissued_session.proof = None;
        let unissued_session = unissued_session.clone();
        assert!(ArchivedPaneReadiness::qualify(id, &unissued_session, &unissued).is_err());
        assert!(ArchivedPaneReadiness::qualify(id, &changed, &archive).is_err());
        let mut stale = session.clone();
        stale.launch.session_id = None;
        assert!(
            ArchivedPaneReadiness::qualify(id, &stale, &archive).is_ok(),
            "stale None learns only the canonical confirmed thread"
        );
        let mut wrong_thread = archive.clone();
        wrong_thread.snapshot.session_id = Some("different-confirmed-thread".into());
        assert!(ArchivedPaneReadiness::qualify(id, &stale, &wrong_thread).is_err());
        let mut listener = session.clone();
        listener.owned_codex_listener = Some(crate::chat::OwnedCodexListener {
            socket: "/invented/control.sock".into(),
            allocator: session.proof.clone().unwrap(),
        });
        assert!(ArchivedPaneReadiness::qualify(id, &listener, &archive).is_err());
        let mut continuity = session.clone();
        continuity.native_continuity = crate::chat::NativeContinuity::Durable {
            provider_version: "invented".into(),
        };
        assert!(ArchivedPaneReadiness::qualify(id, &continuity, &archive).is_err());
        let mut paired = session.clone();
        paired.terminal = Some(crate::model::TerminalSession {
            id: session.id,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: session.launch.recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe: session.launch.recipe.clone(),
                last_exit: None,
            },
        });
        assert!(ArchivedPaneReadiness::qualify(id, &paired, &archive).is_err());
        for field in 0..5 {
            let mut invalid = archive.clone();
            match field {
                0 => invalid.agent.closed = false,
                1 => invalid.snapshot.ready = true,
                2 => invalid.snapshot.busy = true,
                3 => invalid.snapshot.read_only_reason = None,
                _ => invalid.snapshot.detail = "x".repeat(MAX_SNAPSHOT_BYTES + 1),
            }
            assert!(
                ArchivedPaneReadiness::qualify(id, &session, &invalid).is_err(),
                "invalid field {field}"
            );
        }
    }
    #[cfg(feature = "host")]
    #[test]
    fn source_archive_inspection_is_read_only_and_needs_no_service() {
        use rusqlite::{Connection, params};
        let directory = tempfile::tempdir().unwrap();
        let window = directory.path().join("window.sqlite3");
        let registry = directory.path().join("workspace-model.sqlite3");
        let (id, session, archive) = fixture();
        let card = crate::model::CardId::from_uuid(id.as_uuid());
        let db = Connection::open(&window).unwrap();
        db.execute_batch("CREATE TABLE workspaces(workspace_id TEXT,existence_json TEXT);CREATE TABLE cards(card_id TEXT,workspace_id TEXT,existence_json TEXT,kind_tag TEXT,content_json TEXT);CREATE TABLE metadata(key TEXT,value TEXT);").unwrap();
        db.execute(
            "INSERT INTO workspaces VALUES('window',?1)",
            [serde_json::to_string(&crate::model::Existence::Open).unwrap()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO cards VALUES(?1,'window',?2,'chat',?3)",
            params![
                card.to_string(),
                serde_json::to_string(&crate::model::Existence::Open).unwrap(),
                serde_json::to_string(&session).unwrap()
            ],
        )
        .unwrap();
        assert!(
            inspect_archived_pane(&window, &registry, card, &session)
                .unwrap()
                .is_none(),
            "ordinary live pane still needs normal source qualification"
        );
        db.execute(
            "INSERT INTO metadata VALUES('workspace:window:navigation:automation_archives',?1)",
            [serde_json::to_string(&std::collections::HashMap::from([(card, id)])).unwrap()],
        )
        .unwrap();
        drop(db);
        assert!(
            inspect_archived_pane(&window, &registry, card, &session).is_err(),
            "a claimed missing archive is never silently exempted"
        );
        let model = Connection::open(&registry).unwrap();
        model.execute_batch("CREATE TABLE agents(id TEXT,closed INTEGER,record TEXT);CREATE TABLE automation_snapshots(agent TEXT,snapshot TEXT);").unwrap();
        model
            .pragma_update(None, "user_version", ARCHIVE_REGISTRY_SCHEMA_FLOOR)
            .unwrap();
        model
            .execute(
                "INSERT INTO agents VALUES(?1,1,?2)",
                params![
                    id.to_string(),
                    serde_json::to_string(&archive.agent).unwrap()
                ],
            )
            .unwrap();
        model
            .execute(
                "INSERT INTO automation_snapshots VALUES(?1,?2)",
                params![
                    id.to_string(),
                    serde_json::to_string(&archive.snapshot).unwrap()
                ],
            )
            .unwrap();
        drop(model);
        let before = (
            std::fs::read(&window).unwrap(),
            std::fs::read(&registry).unwrap(),
        );
        assert!(
            inspect_archived_pane(&window, &registry, card, &session)
                .unwrap()
                .unwrap()
                .matches(id, &session)
        );
        assert_eq!(
            before,
            (
                std::fs::read(&window).unwrap(),
                std::fs::read(&registry).unwrap()
            ),
            "no migration or source writes"
        );
        let versioned = Connection::open(&registry).unwrap();
        for version in [
            ARCHIVE_REGISTRY_SCHEMA_FLOOR - 1,
            crate::host::WORKSPACE_REGISTRY_SCHEMA_VERSION + 1,
        ] {
            versioned
                .pragma_update(None, "user_version", version)
                .unwrap();
            let before_version = std::fs::read(&registry).unwrap();
            assert!(
                inspect_archived_pane(&window, &registry, card, &session).is_err(),
                "unsupported archive authority version {version}"
            );
            assert_eq!(
                std::fs::read(&registry).unwrap(),
                before_version,
                "inspection must not rewrite unsupported schemas"
            );
        }
        versioned
            .pragma_update(None, "user_version", ARCHIVE_REGISTRY_SCHEMA_FLOOR)
            .unwrap();
        drop(versioned);
        #[cfg(unix)]
        {
            let alias = directory.path().join("alias");
            std::os::unix::fs::symlink(&registry, &alias).unwrap();
            assert!(inspect_archived_pane(&window, &alias, card, &session).is_err());
        }
        // The window may have closed before its daemon completed the turn and
        // therefore never saved the archive map or the presentation change.
        let source = Connection::open(&window).unwrap();
        source.execute("DELETE FROM metadata", []).unwrap();
        drop(source);
        assert!(
            inspect_completed_automation_pane(
                &window,
                &directory.path().join("missing.sqlite3"),
                card,
                &CardKind::Chat(session.clone())
            )
            .unwrap()
            .is_none()
        );
        let legacy = directory.path().join("legacy.sqlite3");
        Connection::open(&legacy)
            .unwrap()
            .execute_batch("CREATE TABLE metadata(id TEXT); PRAGMA user_version=12;")
            .unwrap();
        assert!(
            inspect_completed_automation_pane(
                &window,
                &legacy,
                card,
                &CardKind::Chat(session.clone())
            )
            .unwrap()
            .is_none()
        );
        assert!(
            inspect_completed_automation_pane(
                &window,
                &registry,
                card,
                &CardKind::Chat(session.clone())
            )
            .unwrap()
            .unwrap()
            .matches(id, &session)
        );
        let proof = session.proof.clone().unwrap();
        let mut terminal = crate::model::TerminalSession {
            id: session.id,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: crate::model::LaunchRecipe {
                program: "cagents".into(),
                arguments: vec!["agent".into(), "console".into(), id.to_string()],
                working_directory: "/fixture".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::Live {
                host_session_id: proof.host_session_id,
                boot_id: proof.boot_id,
                host_incarnation: proof.host_incarnation,
            },
        };
        let mut terminal_archive = archive.clone();
        terminal_archive.agent.console = Some(terminal.clone());
        let model = Connection::open(&registry).unwrap();
        model
            .execute(
                "UPDATE agents SET record=?1",
                [serde_json::to_string(&terminal_archive.agent).unwrap()],
            )
            .unwrap();
        drop(model);
        let terminal_window = directory.path().join("normalized-terminal.sqlite3");
        let store = crate::persistence::StateStore::open(
            &terminal_window,
            crate::model::BootId::new("archive-boot"),
        )
        .unwrap();
        let mut workspace = crate::model::WorkspaceState::default();
        workspace.cards.push(crate::model::CardState {
            id: card,
            generation: 0,
            title: "Original scheduled console".into(),
            width_fraction: 0.6,
            existence: crate::model::Existence::Open,
            exposure: crate::model::Exposure::Offscreen,
            residency: crate::model::Residency::Cold,
            kind: CardKind::Terminal(terminal.clone()),
        });
        let mut receipt = store
            .save_workspace(
                &workspace,
                crate::persistence::SnapshotBase {
                    workspace: 0,
                    cards: &Default::default(),
                },
            )
            .unwrap();
        let actual = Connection::open(&terminal_window).unwrap();
        let content: Option<String> = actual
            .query_row(
                "SELECT content_json FROM cards WHERE card_id=?1",
                [card.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            content.is_none(),
            "production Terminal descriptors use normalized rows"
        );
        drop(actual);
        assert!(
            inspect_completed_automation_pane(
                &terminal_window,
                &legacy,
                card,
                &CardKind::Terminal(terminal.clone())
            )
            .unwrap()
            .is_none(),
            "an ordinary baseline terminal with NULL content is not an archive"
        );
        for ended in [false, true] {
            if ended {
                terminal.continuity = crate::model::TerminalContinuity::Exited {
                    last_exit: crate::model::TerminalExit {
                        code: Some(0),
                        signal: None,
                        occurred_at_unix_ms: 1,
                    },
                };
            }
            workspace.cards[0].kind = CardKind::Terminal(terminal.clone());
            receipt = store
                .save_workspace(
                    &workspace,
                    crate::persistence::SnapshotBase {
                        workspace: receipt.generation,
                        cards: &receipt.card_generations,
                    },
                )
                .unwrap();
            let before = (
                std::fs::read(&terminal_window).unwrap(),
                std::fs::read(&registry).unwrap(),
            );
            assert!(
                inspect_completed_automation_pane(
                    &terminal_window,
                    &registry,
                    card,
                    &CardKind::Terminal(terminal.clone())
                )
                .unwrap()
                .unwrap()
                .matches_terminal(id, &terminal)
            );
            assert_eq!(
                before,
                (
                    std::fs::read(&terminal_window).unwrap(),
                    std::fs::read(&registry).unwrap()
                )
            );
        }
        terminal.launch.working_directory = "/explicit-replacement".into();
        let actual = Connection::open(&terminal_window).unwrap();
        actual
            .execute(
                "UPDATE terminal_recipes SET recipe_json=?1 WHERE card_id=?2",
                params![
                    serde_json::to_string(&terminal.launch).unwrap(),
                    card.to_string()
                ],
            )
            .unwrap();
        drop(actual);
        assert!(
            inspect_completed_automation_pane(
                &terminal_window,
                &registry,
                card,
                &CardKind::Terminal(terminal.clone())
            )
            .unwrap()
            .is_none(),
            "canonical card identity cannot authorize a replacement descriptor"
        );
    }
}
