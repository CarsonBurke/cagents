//! Census each workspace store and fence its writer while retiring notices.
use super::*;
use std::{fs, os::unix::fs::MetadataExt};

#[derive(serde::Deserialize)]
struct ChatIdentity {
    id: TerminalId,
    terminal: Option<TerminalIdentity>,
}

#[derive(serde::Deserialize)]
struct TerminalIdentity {
    id: TerminalId,
    recovery_origin: Option<TerminalId>,
    predecessor: Option<TerminalId>,
}

pub(crate) struct SavedSessions {
    connection: Connection,
    path: PathBuf,
    identity: (u64, u64),
    version: i64,
    pub(crate) sessions: BTreeSet<TerminalId>,
}

impl SavedSessions {
    /// Fence writes through this exact connection until retirement commits.
    /// A read transaction alone cannot exclude a WAL writer.
    pub(crate) fn lock_if_unchanged(&mut self) -> Result<Option<rusqlite::Transaction<'_>>> {
        self.connection.busy_timeout(Duration::ZERO)?;
        let transaction = match self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        {
            Ok(transaction) => transaction,
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy
                    || error.code == rusqlite::ErrorCode::DatabaseLocked =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let version: i64 =
            transaction.pragma_query_value(None, "data_version", |row| row.get(0))?;
        Ok(
            (self.identity == (metadata.dev(), metadata.ino()) && self.version == version)
                .then_some(transaction),
        )
    }
}

impl TerminalIdentity {
    fn remember(self, sessions: &mut BTreeSet<TerminalId>) {
        sessions.insert(self.id);
        sessions.extend(self.recovery_origin);
        sessions.extend(self.predecessor);
    }
}

/// Closed cards can still hold a resume recipe or conversation history. Read
/// them too, and refuse an incomplete store instead of skipping unknown rows.
pub(crate) fn saved_sessions(
    path: &Path,
    budget: &mut MigrationInspectionBudget,
) -> Result<SavedSessions> {
    budget.check_deadline()?;
    let metadata = fs::symlink_metadata(path)?;
    let identity = (metadata.dev(), metadata.ino());
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_millis(100))?;
    let version = connection.pragma_query_value(None, "data_version", |row| row.get(0))?;
    let transaction = connection.transaction()?;
    let schema: u32 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if schema > CURRENT_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found: schema,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }
    let (origin, _, predecessor) = terminal_provenance_columns(schema);
    let mut statement = transaction.prepare(&format!(
        "SELECT c.kind_tag,
                CASE WHEN length(CAST(c.content_json AS BLOB)) <= 4194304 THEN c.content_json END,
                r.terminal_id, {origin}, {predecessor}, c.content_json IS NOT NULL
         FROM cards c LEFT JOIN terminal_recipes r ON r.card_id = c.card_id
         WHERE c.kind_tag IN ('terminal', 'chat')"
    ))?;
    let mut rows = statement.query([])?;
    let mut sessions = BTreeSet::new();
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        if kind == "chat" {
            let content: String = row.get(1)?;
            budget.reserve(content.len() as u64)?;
            let chat: ChatIdentity = from_json("saved chat identity", &content)?;
            sessions.insert(chat.id);
            if let Some(terminal) = chat.terminal {
                terminal.remember(&mut sessions);
            }
        } else {
            let archived = row.get::<_, bool>(5)?;
            if archived {
                let content: String = row.get(1)?;
                budget.reserve(content.len() as u64)?;
                let terminal: TerminalIdentity = from_json("archived terminal", &content)?;
                terminal.remember(&mut sessions);
            } else {
                budget.reserve(0)?;
            }
            let read_id = |column| -> Result<Option<TerminalId>> {
                row.get::<_, Option<String>>(column)?
                    .map(|id| parse_id("saved terminal", &id))
                    .transpose()
            };
            if let Some(id) = read_id(2)? {
                TerminalIdentity {
                    id,
                    recovery_origin: read_id(3)?,
                    predecessor: read_id(4)?,
                }
                .remember(&mut sessions);
            } else if !archived {
                return Err(invalid_state!("Saved terminal has no retained identity"));
            }
        }
    }
    budget.check_deadline()?;
    drop(rows);
    drop(statement);
    transaction.commit()?;
    Ok(SavedSessions {
        connection,
        path: path.to_owned(),
        identity,
        version,
        sessions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn budget() -> MigrationInspectionBudget {
        MigrationInspectionBudget::new(Instant::now() + Duration::from_secs(2))
    }

    #[test]
    fn retained_descriptors_across_workspaces_and_later_changes_are_visible() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE cards(card_id TEXT PRIMARY KEY, workspace_id TEXT, kind_tag TEXT, content_json TEXT);
             CREATE TABLE terminal_recipes(card_id TEXT PRIMARY KEY, terminal_id TEXT);",
        ).unwrap();
        let archived = TerminalId::new();
        let paired = TerminalId::new();
        let chat = TerminalId::new();
        let terminal = TerminalId::new();
        db.execute(
            "INSERT INTO cards VALUES('archived','secondary','terminal',?1)",
            [serde_json::json!({"id": archived}).to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO cards VALUES('chat','primary','chat',?1)",
            [serde_json::json!({"id": chat,"terminal": {"id": paired}}).to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO cards VALUES('terminal','secondary','terminal',NULL)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO terminal_recipes VALUES('terminal',?1)",
            [terminal.to_string()],
        )
        .unwrap();
        let mut saved = saved_sessions(&path, &mut budget()).unwrap();
        assert_eq!(
            saved.sessions,
            BTreeSet::from([archived, chat, paired, terminal])
        );
        let lease = saved.lock_if_unchanged().unwrap().unwrap();
        db.busy_timeout(Duration::ZERO).unwrap();
        assert!(
            db.execute(
                "UPDATE cards SET content_json=NULL WHERE card_id='chat'",
                []
            )
            .is_err()
        );
        drop(lease);

        db.execute(
            "INSERT INTO cards VALUES('broken','tertiary','chat','{}')",
            [],
        )
        .unwrap();
        assert!(saved.lock_if_unchanged().unwrap().is_none());
        assert!(saved_sessions(&path, &mut budget()).is_err());
    }
}
