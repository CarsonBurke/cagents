//! Immutable membership-removal receipts for new read-only report presentations.
//! These rows grant no provider authority and never change execution identities.
use super::*;
use crate::{
    automations::{ArchiveReport, archive::ArchivedPaneReadiness},
    chat::{ArchiveReportReference, Session},
    model::{AgentId, CardId},
    workspace_model::views::{Pane, Scope},
};

const MAX_DESCRIPTOR: usize = 3 * 1024 * 1024;

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let current: u32 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if current > SCHEMA_VERSION {
        return Err(format!(
            "workspace registry schema {current} is newer than supported schema {SCHEMA_VERSION}"
        ));
    }
    if current >= 15 {
        return tx.commit().map_err(|error| error.to_string());
    }
    if current != 14 {
        return Err(
            "Complete archive discovery migrations before report presentation receipts".into(),
        );
    }
    tx.execute_batch("CREATE TABLE automation_report_usage (
        key INTEGER PRIMARY KEY CHECK(key=0), descriptors INTEGER NOT NULL DEFAULT 0 CHECK(descriptors>=0),
        receipts INTEGER NOT NULL DEFAULT 0 CHECK(receipts>=0), receipt_bytes INTEGER NOT NULL DEFAULT 0 CHECK(receipt_bytes>=0)
    ) STRICT;
    INSERT INTO automation_report_usage(key) VALUES(0);
    CREATE TABLE automation_report_descriptors (
        agent TEXT NOT NULL CHECK(length(CAST(agent AS BLOB))=36) REFERENCES automation_snapshots(agent) ON DELETE CASCADE,
        digest TEXT NOT NULL CHECK(length(CAST(digest AS BLOB))=64), content TEXT NOT NULL, PRIMARY KEY(agent,digest)
    ) STRICT;
    CREATE TABLE automation_report_sources (
        operation TEXT PRIMARY KEY CHECK(length(CAST(operation AS BLOB))=36),
        agent TEXT NOT NULL CHECK(length(CAST(agent AS BLOB))=36) REFERENCES automation_snapshots(agent) ON DELETE CASCADE,
        card TEXT NOT NULL CHECK(length(CAST(card AS BLOB))=36), registry TEXT NOT NULL CHECK(length(CAST(registry AS BLOB))=36), scope TEXT NOT NULL, repository TEXT NOT NULL,
        revision INTEGER NOT NULL CHECK(revision>=0), descriptor TEXT NOT NULL CHECK(length(CAST(descriptor AS BLOB))=64), presentation TEXT NOT NULL,
        FOREIGN KEY(agent,descriptor) REFERENCES automation_report_descriptors(agent,digest) ON DELETE CASCADE
    ) STRICT;
    CREATE INDEX automation_report_sources_by_card ON automation_report_sources(card,operation);
    CREATE INDEX automation_report_sources_by_agent ON automation_report_sources(agent);
    CREATE TRIGGER automation_report_descriptor_budget BEFORE INSERT ON automation_report_descriptors
      WHEN (SELECT count(*) FROM automation_report_usage WHERE key=0)!=1 OR length(CAST(NEW.content AS BLOB))>3145728 OR
        (SELECT descriptors FROM automation_report_usage WHERE key=0)+length(CAST(NEW.content AS BLOB))>1073741824
      BEGIN SELECT RAISE(ABORT,'Saved report descriptor budget is full; its pane and run report were preserved'); END;
    CREATE TRIGGER automation_report_descriptor_added AFTER INSERT ON automation_report_descriptors BEGIN
      UPDATE automation_report_usage SET descriptors=descriptors+length(CAST(NEW.content AS BLOB)) WHERE key=0; END;
    CREATE TRIGGER automation_report_descriptor_removed AFTER DELETE ON automation_report_descriptors BEGIN
      UPDATE automation_report_usage SET descriptors=descriptors-length(CAST(OLD.content AS BLOB)) WHERE key=0; END;
    CREATE TRIGGER automation_report_source_budget BEFORE INSERT ON automation_report_sources
      WHEN (SELECT count(*) FROM automation_report_usage WHERE key=0)!=1 OR length(CAST(NEW.presentation AS BLOB))>16384 OR length(CAST(NEW.scope AS BLOB))>16384 OR length(CAST(NEW.repository AS BLOB))>16384 OR
        (SELECT receipts FROM automation_report_usage WHERE key=0)>=65536 OR
        (SELECT count(*) FROM automation_report_sources WHERE agent=NEW.agent)>=4096 OR
        (SELECT receipt_bytes FROM automation_report_usage WHERE key=0)+length(CAST(NEW.presentation AS BLOB))+length(CAST(NEW.scope AS BLOB))+length(CAST(NEW.repository AS BLOB))+256>67108864
      BEGIN SELECT RAISE(ABORT,'Saved report receipt budget is full; its pane and run report were preserved'); END;
    CREATE TRIGGER automation_report_source_added AFTER INSERT ON automation_report_sources BEGIN
      UPDATE automation_report_usage SET receipts=receipts+1,receipt_bytes=receipt_bytes+length(CAST(NEW.presentation AS BLOB))+length(CAST(NEW.scope AS BLOB))+length(CAST(NEW.repository AS BLOB))+256 WHERE key=0; END;
    CREATE TRIGGER automation_report_source_removed AFTER DELETE ON automation_report_sources BEGIN
      UPDATE automation_report_usage SET receipts=receipts-1,receipt_bytes=receipt_bytes-length(CAST(OLD.presentation AS BLOB))-length(CAST(OLD.scope AS BLOB))-length(CAST(OLD.repository AS BLOB))-256 WHERE key=0; END;
    CREATE TRIGGER automation_report_descriptor_immutable BEFORE UPDATE ON automation_report_descriptors
      BEGIN SELECT RAISE(ABORT,'Saved report descriptors are immutable'); END;
    CREATE TRIGGER automation_report_source_immutable BEFORE UPDATE ON automation_report_sources
      BEGIN SELECT RAISE(ABORT,'Saved report receipts are immutable'); END;
    PRAGMA user_version=15;").map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Presentation {
    title: String,
    user_title: Option<String>,
    width_fraction: f32,
    scope: Option<PathBuf>,
    source_reference: Option<ArchiveReportReference>,
}

fn descriptor_key(agent: AgentId, encoded: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(agent.to_string().as_bytes());
    digest.update([0]);
    digest.update(encoded.as_bytes());
    format!("{:x}", digest.finalize())
}

/// Capture one tiny receipt per End, sharing the exact native descriptor body
/// across repeated new-card presentations of the same retained closed owner.
/// Both bodies and receipts have transactional aggregate admission budgets.
pub(super) fn capture_removal(
    db: &Connection,
    operation: Uuid,
    scope: &Scope,
    revision: u64,
    pane: &Pane,
) -> Result<(), String> {
    if operation.is_nil() {
        return Err("The archive removal operation is missing".into());
    }
    pane.validate()?;
    let agent = super::automation_membership::archived_owner_in(db, pane)?
        .ok_or("The original archive source is no longer available")?;
    if pane.title.len() + pane.user_title.as_ref().map_or(0, String::len) > 16384 {
        return Err(
            "The saved report presentation metadata exceeds its limit; its pane was preserved"
                .into(),
        );
    }
    let mut kind = pane.kind.clone();
    let source_reference = match &mut kind {
        crate::model::CardKind::Chat(session) => session.archive_report.take(),
        _ => None,
    };
    let encoded = serde_json::to_string(&kind).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_DESCRIPTOR {
        return Err("The archive source exceeds its descriptor limit".into());
    }
    let descriptor = descriptor_key(agent.id, &encoded);
    let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM automation_report_descriptors WHERE agent=?1 AND digest=?2)",params![agent.id.to_string(),descriptor],|row|row.get(0)).map_err(|error|error.to_string())?;
    if !exists {
        db.execute(
            "INSERT INTO automation_report_descriptors(agent,digest,content) VALUES(?1,?2,?3)",
            params![agent.id.to_string(), descriptor, encoded],
        )
        .map_err(|error| error.to_string())?;
    }
    let repository: PathBuf = match scope {
        Scope::Worktree { repository, .. } => repository.clone(),
        Scope::Channel(channel) => {
            let repository:Option<String>=db.query_row("SELECT CASE WHEN length(CAST(repository AS BLOB))<=16384 THEN repository END FROM channels WHERE id=?1",[channel.to_string()],|row|row.get(0)).map_err(|_|"The original report project is unavailable")?;
            repository
                .ok_or("The original report project exceeds its metadata limit")?
                .into()
        }
    };
    let repository = serde_json::to_string(&repository).map_err(|error| error.to_string())?;
    let presentation = serde_json::to_string(&Presentation {
        title: pane.title.clone(),
        user_title: pane.user_title.clone(),
        width_fraction: pane.width_fraction,
        scope: pane.scope.clone(),
        source_reference,
    })
    .map_err(|error| error.to_string())?;
    let scope = serde_json::to_string(scope).map_err(|error| error.to_string())?;
    if presentation.len() > 16384 || scope.len() > 16384 || repository.len() > 16384 {
        return Err(
            "The saved report presentation metadata exceeds its limit; its pane was preserved"
                .into(),
        );
    }
    let identity: String = db.query_row("SELECT CASE WHEN length(CAST(identity AS BLOB))<=36 THEN identity ELSE '' END FROM metadata WHERE key=0", [], |row| row.get(0)).map_err(|error| error.to_string())?;
    db.execute("INSERT INTO automation_report_sources(operation,agent,card,registry,scope,revision,descriptor,presentation,repository) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![operation.to_string(),agent.id.to_string(),pane.id.to_string(),identity,scope,revision,descriptor,presentation,repository])
        .map_err(|error| error.to_string())?;
    Ok(())
}

struct Source {
    reference: ArchiveReportReference,
    scope: Scope,
    repository: PathBuf,
    revision: u64,
    pane: Pane,
}
fn source(db: &Connection, reference: &ArchiveReportReference) -> Result<Source, String> {
    if !reference.is_supported() {
        return Err("The saved report reference is unsupported".into());
    }
    let identity: String = db.query_row("SELECT CASE WHEN length(CAST(identity AS BLOB))<=36 THEN identity ELSE '' END FROM metadata WHERE key=0", [], |row| row.get(0)).map_err(|error| error.to_string())?;
    if identity != reference.registry.to_string() {
        return Err("The saved report belongs to a different registry".into());
    }
    type SourceRow = (
        String,
        String,
        String,
        Option<String>,
        u64,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    );
    let row: Option<SourceRow> = db.query_row(
        "SELECT CASE WHEN length(CAST(s.agent AS BLOB))<=36 THEN s.agent ELSE '' END,CASE WHEN length(CAST(s.registry AS BLOB))<=36 THEN s.registry ELSE '' END,CASE WHEN length(CAST(s.card AS BLOB))<=36 THEN s.card ELSE '' END,CASE WHEN length(CAST(s.scope AS BLOB))<=16384 THEN s.scope END,s.revision,CASE WHEN length(CAST(s.presentation AS BLOB))<=16384 THEN s.presentation END,CASE WHEN length(CAST(s.descriptor AS BLOB))<=64 THEN s.descriptor ELSE '' END,CASE WHEN length(CAST(d.content AS BLOB))<=3145728 THEN d.content END,CASE WHEN length(CAST(s.repository AS BLOB))<=16384 THEN s.repository END FROM automation_report_sources s JOIN automation_report_descriptors d ON d.agent=s.agent AND d.digest=s.descriptor WHERE s.operation=?1",
        [reference.source_operation.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?,row.get(8)?)))
        .optional().map_err(|error| error.to_string())?;
    let Some((agent, registry, card, scope, revision, presentation, key, content, repository)) =
        row
    else {
        return Err("The saved report source expired or was never committed".into());
    };
    if agent != reference.agent.to_string() || registry != identity {
        return Err("The saved report source owner changed".into());
    }
    let scope: Scope =
        serde_json::from_str(&scope.ok_or("The saved report scope exceeds its limit")?)
            .map_err(|_| "The saved report scope is unreadable")?;
    let repository: PathBuf =
        serde_json::from_str(&repository.ok_or("The saved report project exceeds its limit")?)
            .map_err(|_| "The saved report project is unreadable")?;
    let presentation: Presentation = serde_json::from_str(
        &presentation.ok_or("The saved report presentation exceeds its limit")?,
    )
    .map_err(|_| "The saved report presentation is unreadable")?;
    let content = content.ok_or("The saved report descriptor exceeds its limit")?;
    if descriptor_key(reference.agent, &content) != key {
        return Err("The saved report descriptor changed".into());
    }
    let mut kind: crate::model::CardKind =
        serde_json::from_str(&content).map_err(|_| "The saved report descriptor is unreadable")?;
    match &mut kind {
        crate::model::CardKind::Chat(session) if session.archive_report.is_none() => {
            session.archive_report = presentation.source_reference
        }
        crate::model::CardKind::Terminal(_) if presentation.source_reference.is_none() => {}
        _ => return Err("The saved report source kind changed".into()),
    }
    let pane = Pane {
        id: card
            .parse()
            .map_err(|_| "The saved report source card is invalid")?,
        title: presentation.title,
        user_title: presentation.user_title,
        width_fraction: presentation.width_fraction,
        scope: presentation.scope,
        pending_terminal_start: false,
        kind,
    };
    pane.validate()?;
    Ok(Source {
        reference: reference.clone(),
        scope,
        repository,
        revision,
        pane,
    })
}

pub(super) fn report_in(
    db: &Connection,
    reference: &ArchiveReportReference,
    session: &Session,
) -> Result<ArchiveReport, String> {
    let source = source(db, reference)?;
    // Merely possessing a pointer is insufficient. Re-read and qualify the
    // exact original closed source plus bounded history before normalizing the
    // presentation-only reference out of its original native descriptor.
    let mut original_pane = source.pane.clone();
    original_pane.id = CardId::from_uuid(reference.agent.as_uuid());
    if let crate::model::CardKind::Chat(native) = &mut original_pane.kind {
        if native.archive_report.as_ref().is_some_and(|inner| {
            !inner.is_supported()
                || inner.registry != reference.registry
                || inner.agent != reference.agent
        }) {
            return Err("The committed report source has different ownership".into());
        }
        native.archive_report = None;
    }
    let owner = super::automation_membership::archived_owner_in(db, &original_pane)?
        .ok_or("The saved report is no longer retained")?;
    if owner.id != reference.agent {
        return Err("The saved report owner changed".into());
    }
    let encoded: String=db.query_row("SELECT snapshot FROM automation_snapshots WHERE agent=?1 AND length(CAST(snapshot AS BLOB))<=4194304",[owner.id.to_string()],|row|row.get(0)).map_err(|_|"The saved report is unavailable or exceeds its limit")?;
    let archive = crate::automations::ArchivedConversation {
        agent: owner,
        snapshot: serde_json::from_str(&encoded).map_err(|_| "The saved report is unreadable")?,
    };
    if session
        .archive_report
        .as_ref()
        .is_some_and(|value| value != reference)
    {
        return Err("The report presentation reference changed".into());
    }
    let mut original = session.clone();
    original.archive_report = None;
    ArchivedPaneReadiness::qualify(reference.agent, &original, &archive)?;
    let mut issued = original;
    issued.archive_report = Some(reference.clone());
    let presentation_scope = match &source.scope {
        Scope::Worktree { path, .. } => Some(path.clone()),
        Scope::Channel(_) => source.pane.scope.clone(),
    };
    Ok(ArchiveReport {
        reference: source.reference,
        scope: source.scope,
        repository: source.repository,
        source_revision: source.revision,
        presentation_scope,
        session_digest: ArchiveReport::session_digest(&issued)?,
        archive,
    })
}

impl Registry {
    pub(crate) fn read_archive_report(
        &self,
        reference: &ArchiveReportReference,
        session: &Session,
    ) -> Result<ArchiveReport, String> {
        report_in(&self.db, reference, session)
    }
    pub(crate) fn archive_report_source(
        &self,
        identity: Uuid,
        card: CardId,
        session: &Session,
    ) -> Result<Option<ArchiveReport>, String> {
        let current:String=self.db.query_row("SELECT CASE WHEN length(CAST(identity AS BLOB))<=36 THEN identity ELSE '' END FROM metadata WHERE key=0",[],|row|row.get(0)).map_err(|error|error.to_string())?;
        if identity.is_nil() || current != identity.to_string() {
            return Err("The selected report registry changed; wait for its current model".into());
        }
        let operation:Option<(String,String)>=self.db.query_row("SELECT CASE WHEN length(CAST(operation AS BLOB))<=36 THEN operation ELSE '' END,CASE WHEN length(CAST(agent AS BLOB))<=36 THEN agent ELSE '' END FROM automation_report_sources WHERE card=?1 ORDER BY operation LIMIT 1",[card.to_string()],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(|error|error.to_string())?;
        let Some((operation, agent)) = operation else {
            if let Some(reference) = &session.archive_report {
                return self.read_archive_report(reference, session).map(Some);
            }
            // An ended original archive without a presentation receipt must
            // never fall through to provider resume, including after TTL.
            let owner:Option<Option<String>>=self.db.query_row(
                "SELECT CASE WHEN length(CAST(record AS BLOB))<=3145728 THEN record END FROM agents WHERE session_id=?1 AND json_type(record,'$.origin.Automation')='object' LIMIT 1",
                [session.id.to_string()],|row|row.get(0)).optional().map_err(|error|error.to_string())?;
            if let Some(owner) = owner {
                let owner: crate::agent_execution::Agent = serde_json::from_str(
                    &owner.ok_or("The source owner exceeds its descriptor limit")?,
                )
                .map_err(|_| "The source owner is unreadable")?;
                // This is only a refusal fence, never a receipt or a permission
                // to learn another owner/native descriptor.
                if matches!(
                    &owner.origin,
                    crate::agent_execution::Origin::Automation { .. }
                ) {
                    return Err("Open this automation result from its run history; no committed read-only presentation source is available".into());
                }
            }
            return Ok(None);
        };
        let identity:String=self.db.query_row("SELECT CASE WHEN length(CAST(identity AS BLOB))<=36 THEN identity ELSE '' END FROM metadata WHERE key=0",[],|row|row.get(0)).map_err(|error|error.to_string())?;
        let reference = ArchiveReportReference {
            format: 1,
            registry: identity
                .parse()
                .map_err(|_| "The report registry identity is invalid")?,
            agent: agent
                .parse()
                .map_err(|_| "The report owner identity is invalid")?,
            source_operation: operation
                .parse()
                .map_err(|_| "The report operation is invalid")?,
        };
        let source = source(&self.db, &reference)?;
        let mut requested = session.clone();
        if requested
            .archive_report
            .as_ref()
            .is_some_and(|old| old != &reference)
        {
            if !matches!(&source.pane.kind,crate::model::CardKind::Chat(captured) if captured==session)
            {
                return Err("The report's original closed presentation changed".into());
            }
            requested.archive_report = None;
        }
        self.read_archive_report(&reference, &requested).map(Some)
    }
}

pub(super) fn presentation_scope_in(
    db: &Connection,
    reference: &ArchiveReportReference,
) -> Result<Option<PathBuf>, String> {
    let source = source(db, reference)?;
    Ok(match source.scope {
        Scope::Worktree { path, .. } => Some(path),
        Scope::Channel(_) => source.pane.scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_receipt_migration_rechecks_the_reserved_version_and_preserves_newer_writers() {
        for newer in [15, SCHEMA_VERSION + 1] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("registry.sqlite3");
            let db = Connection::open(&path).unwrap();
            db.pragma_update(None, "user_version", 14).unwrap();
            let observed: u32 = db
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .unwrap();
            assert_eq!(observed, 14);
            // Advance after the opener's outer read, as a different writer
            // would do before this extension reserves its transaction.
            let competing = Connection::open(&path).unwrap();
            if newer == 15 {
                migrate(&competing).unwrap();
            } else {
                competing
                    .pragma_update(None, "user_version", newer)
                    .unwrap();
            }
            competing.execute_batch("CREATE TABLE second_writer_intent(value TEXT); INSERT INTO second_writer_intent VALUES('preserve');").unwrap();
            let result = migrate(&db);
            assert_eq!(result.is_ok(), newer == 15);
            let current: u32 = db
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .unwrap();
            assert_eq!(current, newer);
            let tables:u64=db.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='automation_report_sources'",[],|row|row.get(0)).unwrap();
            assert_eq!(
                tables,
                u64::from(newer == 15),
                "a compatible checkpoint is not replayed and a future one is never downgraded"
            );
            assert_eq!(
                db.query_row("SELECT value FROM second_writer_intent", [], |row| row
                    .get::<_, String>(
                    0
                ))
                .unwrap(),
                "preserve"
            );
        }
    }

    #[test]
    fn report_receipt_migration_rolls_back_ddl_and_retries_without_losing_prior_state() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA user_version=14; CREATE TABLE preserved(value TEXT); INSERT INTO preserved VALUES('original'); CREATE TABLE automation_report_sources_by_card(value TEXT);").unwrap();
        assert!(migrate(&db).is_err());
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            14
        );
        assert_eq!(db.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='automation_report_sources'",[],|row|row.get::<_,u64>(0)).unwrap(),0);
        assert_eq!(
            db.query_row("SELECT value FROM preserved", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "original"
        );
        db.execute("DROP TABLE automation_report_sources_by_card", [])
            .unwrap();
        migrate(&db).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            15
        );
        assert_eq!(
            db.query_row("SELECT value FROM preserved", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "original"
        );
    }
}

/// Compare one bounded candidate at a time. The immutable original native
/// recipe/proof/listener and exact paired console come from the registry, never
/// the digest request. Only aliases already admitted by the private archive
/// qualifier (unknown thread, default native marker, exact pair learning) are
/// tried; the matched descriptor is still privately requalified before reply.
fn descriptor_session(
    db: &Connection,
    reference: &ArchiveReportReference,
    descriptor: &crate::automations::ArchiveReportDescriptor,
    reading: bool,
) -> Result<Session, String> {
    let source = source(db, reference)?;
    let marker = if reading {
        Some(reference.clone())
    } else {
        match &source.pane.kind {
            crate::model::CardKind::Chat(session) => session.archive_report.clone(),
            _ => None,
        }
    };
    if descriptor.reference != marker {
        return Err("The saved report source reference changed".into());
    }
    let mut original = source.pane;
    original.id = CardId::from_uuid(reference.agent.as_uuid());
    if let crate::model::CardKind::Chat(session) = &mut original.kind {
        session.archive_report = None;
    }
    let owner = super::automation_membership::archived_owner_in(db, &original)?
        .ok_or("The original closed report owner is unavailable")?;
    let Some(crate::agent_execution::HostedSession::Chat(native)) = &owner.session else {
        return Err("The saved report has no native source".into());
    };
    if native.id != descriptor.id {
        return Err("The saved report native identity changed".into());
    }
    let reported:Option<String>=db.query_row("SELECT CASE WHEN json_type(snapshot,'$.native_continuity') IS NULL THEN '\"ReconnectOnly\"' WHEN length(CAST(json_quote(json_extract(snapshot,'$.native_continuity')) AS BLOB))<=4096 THEN json_quote(json_extract(snapshot,'$.native_continuity')) END FROM automation_snapshots WHERE agent=?1",[owner.id.to_string()],|row|row.get(0)).map_err(|_|"The saved report native marker is unreadable")?;
    let reported: crate::chat::NativeContinuity =
        serde_json::from_str(&reported.ok_or("The saved report native marker exceeds its limit")?)
            .map_err(|_| "The saved report native marker is invalid")?;
    for continuity in [
        native.native_continuity.clone(),
        crate::chat::NativeContinuity::ReconnectOnly,
        reported,
    ] {
        for known_thread in [true, false] {
            for paired in [true, false] {
                let mut candidate = native.clone();
                candidate.archive_report = marker.clone();
                candidate.native_continuity = continuity.clone();
                if !known_thread {
                    candidate.launch.session_id = None;
                }
                candidate.terminal = if paired {
                    owner.console.clone().or_else(|| native.terminal.clone())
                } else {
                    None
                };
                if ArchiveReport::session_digest(&candidate)? == descriptor.digest {
                    return Ok(candidate);
                }
            }
        }
    }
    Err("The saved report descriptor changed".into())
}

impl Registry {
    pub(crate) fn read_archive_report_descriptor(
        &self,
        reference: &ArchiveReportReference,
        descriptor: &crate::automations::ArchiveReportDescriptor,
    ) -> Result<ArchiveReport, String> {
        let session = descriptor_session(&self.db, reference, descriptor, true)?;
        self.read_archive_report(reference, &session)
    }

    pub(crate) fn archive_report_source_descriptor(
        &self,
        identity: Uuid,
        card: CardId,
        descriptor: &crate::automations::ArchiveReportDescriptor,
    ) -> Result<Option<ArchiveReport>, String> {
        let current:String=self.db.query_row("SELECT CASE WHEN length(CAST(identity AS BLOB))<=36 THEN identity ELSE '' END FROM metadata WHERE key=0",[],|row|row.get(0)).map_err(|error|error.to_string())?;
        if identity.is_nil() || current != identity.to_string() {
            return Err("The selected report registry changed".into());
        }
        let row:Option<(String,String)>=self.db.query_row("SELECT CASE WHEN length(CAST(operation AS BLOB))<=36 THEN operation ELSE '' END,CASE WHEN length(CAST(agent AS BLOB))<=36 THEN agent ELSE '' END FROM automation_report_sources WHERE card=?1 ORDER BY operation LIMIT 1",[card.to_string()],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(|error|error.to_string())?;
        let reference = match row {
            Some((operation, agent)) => ArchiveReportReference {
                format: 1,
                registry: identity,
                agent: agent
                    .parse()
                    .map_err(|_| "The source report owner is invalid")?,
                source_operation: operation
                    .parse()
                    .map_err(|_| "The source report operation is invalid")?,
            },
            None => {
                if let Some(reference) = &descriptor.reference {
                    return self
                        .read_archive_report_descriptor(reference, descriptor)
                        .map(Some);
                }
                let owned:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM agents WHERE session_id=?1 AND json_type(record,'$.origin.Automation')='object')",[descriptor.id.to_string()],|row|row.get(0)).map_err(|error|error.to_string())?;
                return if owned {
                    Err("Open this automation result from its run history; no committed read-only presentation source is available".into())
                } else {
                    Ok(None)
                };
            }
        };
        let session = descriptor_session(&self.db, &reference, descriptor, false)?;
        self.archive_report_source(identity, card, &session)
    }
}
