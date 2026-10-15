//! Qualification for saved automation panes across application replacement.
//! A receipt proves readable immutable history, never a live provider.
use super::ArchivedConversation;
use crate::{agent_execution::HostedSession, chat::Session, model::AgentId};

pub const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedPaneReadiness {
    agent: AgentId,
    session: Session,
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
            || archive.agent.spec.provider != session.launch.provider
            || !archive
                .agent
                .session
                .as_ref()
                .is_some_and(|saved| saved.same_process(&HostedSession::Chat(session.clone())))
            || session
                .launch
                .session_id
                .as_ref()
                .is_some_and(|id| snapshot.session_id.as_ref() != Some(id))
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
        })
    }
    pub fn matches(&self, agent: AgentId, session: &Session) -> bool {
        self.agent == agent && &self.session == session
    }
}

/// Qualifies a source pane without opening a service or migrating either DB.
/// No claim is `None`; a broken claimed archive is an error, never a live-pane
/// exemption. Both reads refuse symlinks and preserve source files unchanged.
#[cfg(feature = "host")]
pub fn inspect_archived_pane(
    database: &std::path::Path,
    registry: &std::path::Path,
    card: crate::model::CardId,
    session: &Session,
) -> Result<Option<ArchivedPaneReadiness>, String> {
    use crate::model::{CardKind, Existence};
    use rusqlite::{Connection, OpenFlags, OptionalExtension};
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut source = Connection::open_with_flags(database, flags)
        .map_err(|_| "The source archive binding is unreadable")?;
    source
        .busy_timeout(std::time::Duration::from_millis(100))
        .map_err(|_| "The source archive binding is unavailable")?;
    let tx = source
        .transaction()
        .map_err(|_| "Cannot read the source archive binding")?;
    let row:Option<(String,String,String,Option<String>,String)>=tx.query_row(
        "SELECT c.workspace_id,CASE WHEN length(CAST(c.existence_json AS BLOB))<=4096 THEN c.existence_json ELSE 'null' END,c.kind_tag,CASE WHEN length(CAST(c.content_json AS BLOB))<=4194304 THEN c.content_json ELSE NULL END,CASE WHEN length(CAST(w.existence_json AS BLOB))<=4096 THEN w.existence_json ELSE 'null' END FROM cards c JOIN workspaces w ON w.workspace_id=c.workspace_id WHERE c.card_id=?1",
        [card.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).optional().map_err(|_|"The source archive pane is unreadable")?;
    let Some((workspace, existence, kind, content, workspace_existence)) = row else {
        return Ok(None);
    };
    let bindings: Option<Option<String>> = tx
        .query_row(
            "SELECT CASE WHEN length(CAST(value AS BLOB))<=1048576 THEN value ELSE NULL END FROM metadata WHERE key=?1",
            [format!(
                "workspace:{workspace}:navigation:automation_archives"
            )],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| "The source archive binding is unreadable")?;
    let Some(bindings) = bindings else {
        return Ok(None);
    };
    let bindings=bindings.ok_or("The source archive binding exceeds its inspection limit")?;
    let bindings: std::collections::HashMap<crate::model::CardId, AgentId> =
        serde_json::from_str(&bindings).map_err(|_| "The source archive binding is invalid")?;
    let Some(agent) = bindings.get(&card).copied() else {
        return Ok(None);
    };
    let existence: Existence =
        serde_json::from_str(&existence).map_err(|_| "The source archive pane is invalid")?;
    let workspace_existence: Existence = serde_json::from_str(&workspace_existence)
        .map_err(|_| "The source archive workspace is invalid")?;
    if !existence.is_open()
        || !workspace_existence.is_open()
        || kind != "chat"
        || agent.as_uuid() != card.as_uuid()
    {
        return Err("The source archive binding changed".into());
    }
    let current: Session = serde_json::from_str(
        content
            .as_deref()
            .ok_or("The source archive descriptor is missing")?,
    )
    .map_err(|_| "The source archive descriptor is invalid")?;
    if CardKind::Chat(current) != CardKind::Chat(session.clone()) {
        return Err("The source archive descriptor changed".into());
    }
    let model = Connection::open_with_flags(registry, flags)
        .map_err(|_| "The saved automation archive is unavailable")?;
    model
        .busy_timeout(std::time::Duration::from_millis(100))
        .map_err(|_| "The saved automation archive is unavailable")?;
    let (record,snapshot):(String,String)=model.query_row("SELECT g.record,s.snapshot FROM agents g JOIN automation_snapshots s ON s.agent=g.id WHERE g.id=?1 AND g.closed=1 AND length(CAST(g.record AS BLOB))<=4194304 AND length(CAST(s.snapshot AS BLOB))<=4194304",[agent.to_string()],|row|Ok((row.get(0)?,row.get(1)?))).map_err(|_|"The claimed automation archive is missing or unreadable")?;
    let archive = ArchivedConversation {
        agent: serde_json::from_str(&record).map_err(|_| "The saved archive owner is invalid")?,
        snapshot: serde_json::from_str(&snapshot)
            .map_err(|_| "The saved archive payload is invalid")?,
    };
    ArchivedPaneReadiness::qualify(agent, session, &archive).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Agent, Origin, Placement, Spec, Surface, Target},
        chat::Snapshot,
        model::TerminalId,
    };
    fn fixture() -> (AgentId, Session, ArchivedConversation) {
        let agent = AgentId::new();
        let path = std::path::PathBuf::from("/fixture");
        let session = crate::chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            path.clone(),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
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
    #[test]
    fn archive_receipt_requires_exact_closed_read_only_history() {
        let (id, session, archive) = fixture();
        let receipt = ArchivedPaneReadiness::qualify(id, &session, &archive).unwrap();
        assert!(receipt.matches(id, &session));
        let mut changed = session.clone();
        changed.id = TerminalId::new();
        assert!(!receipt.matches(id, &changed));
        assert!(ArchivedPaneReadiness::qualify(id, &changed, &archive).is_err());
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
        #[cfg(unix)]
        {
            let alias = directory.path().join("alias");
            std::os::unix::fs::symlink(&registry, &alias).unwrap();
            assert!(inspect_archived_pane(&window, &alias, card, &session).is_err());
        }
    }
}
