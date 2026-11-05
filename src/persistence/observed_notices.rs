//! App-observed diagnostic notices, separate from provider history and pane descriptors.
//!
//! Its identity ledger is append-only. Raw bodies retain the newest 32 notices;
//! older windows cannot resurrect rotated records. Anchor changes require the
//! current stored revision; stale or unversioned changes are refused.
//! Exceeding a byte bound refuses the entire write without changing old data.

use std::{
    io::{self, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::PathBuf,
    sync::Arc,
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Result, StateStore, StoreError, ensure_open, metadata, set_metadata};
use crate::{
    agent::Provider,
    chat::{
        Launch,
        failure::{Failure, FailureKind},
    },
};

pub(crate) const MAX_NOTICES: usize = 32;
pub(crate) const MAX_CONVERSATION_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_GLOBAL_BYTES: usize = 128 * 1024 * 1024;
const MAX_SCOPE_BYTES: usize = 16 * 1024;
const MAX_MARKER_BYTES: usize = 256;
const KEY_PREFIX: &str = "observed-notices-v1:";
const KEY_GLOB: &str = "observed-notices-v1:*";
const FIELD: &str = "observed diagnostic notices";

/// Physical account and exact conversation identity, captured away from the UI.
/// Local filesystem identity must never be inferred for a selected SSH device.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Scope {
    provider: Provider,
    owner_domain: String,
    local_uid: u32,
    #[serde(with = "crate::selected_file_path::required")]
    canonical_home: PathBuf,
    device: u64,
    inode: u64,
    session_id: String,
}

impl Scope {
    /// `launch` must contain the provider-confirmed exact conversation ID and
    /// the account selector captured for this launch. `owner_domain` identifies
    /// the stable local host/owner context, not a card or daemon incarnation.
    ///
    /// Do not fill missing selectors from this process's current environment:
    /// that can belong to a different account by the time an attach completes.
    pub(crate) fn resolve_local(launch: &Launch, owner_domain: String) -> Result<Self> {
        if crate::device::is_remote() {
            return Err(invalid(
                "Remote observed notices need an attested remote account home",
            ));
        }
        let variable = crate::account::home_variable(launch.provider)
            .ok_or_else(|| invalid("This provider has no attested account-home selector"))?;
        let selected = launch
            .recipe
            .environment
            .get(variable)
            .filter(|value| !value.is_empty())
            .filter(|_| !launch.recipe.unset_environment.contains(variable))
            .ok_or_else(|| invalid("The conversation has no captured account-home selector"))?;
        if !launch.recipe.working_directory.is_absolute() {
            return Err(invalid("The provider working directory must be absolute"));
        }
        let home = launch.recipe.working_directory.join(selected);
        let canonical_home = home.canonicalize()?;
        let physical = std::fs::metadata(&canonical_home)?;
        if !physical.is_dir() {
            return Err(invalid("The captured provider home is not a directory"));
        }
        let scope = Self {
            provider: launch.provider,
            owner_domain,
            // SAFETY: geteuid has no preconditions or ownership side effects.
            local_uid: unsafe { libc::geteuid() },
            canonical_home,
            device: physical.dev(),
            inode: physical.ino(),
            session_id: launch
                .session_id
                .as_deref()
                .filter(|id| id.len() == 36)
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                .ok_or_else(|| invalid("The provider has not confirmed an exact conversation ID"))?
                .to_string(),
        };
        scope.key()?;
        Ok(scope)
    }

    fn revalidate(&self) -> Result<()> {
        let physical = std::fs::metadata(&self.canonical_home)?;
        if !physical.is_dir() || physical.dev() != self.device || physical.ino() != self.inode {
            return Err(invalid("The captured provider account home was replaced"));
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.owner_domain.is_empty()
            || self.owner_domain.len() > 4096
            || self.owner_domain.contains('\0')
            || !self.canonical_home.is_absolute()
            || self.canonical_home.as_os_str().as_bytes().contains(&0)
            || self.canonical_home.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
            || self.session_id.len() != 36
            || !uuid::Uuid::parse_str(&self.session_id)
                .is_ok_and(|id| id.to_string() == self.session_id)
        {
            return Err(invalid("The observed-notice conversation scope is invalid"));
        }
        Ok(())
    }

    fn key(&self) -> Result<String> {
        self.validate()?;
        let encoded = encode_bounded(self, MAX_SCOPE_BYTES)?;
        Ok(format!("{KEY_PREFIX}{:x}", Sha256::digest(encoded)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Notice {
    pub message: Option<String>,
    pub after: Option<String>,
    pub text: Arc<str>,
    /// Zero means a newly observed notice without durable anchor authority.
    #[serde(default)]
    pub revision: u64,
}

#[derive(Clone, Copy)]
struct Limits {
    conversation: usize,
    global: usize,
}

const LIMITS: Limits = Limits {
    conversation: MAX_CONVERSATION_BYTES,
    global: MAX_GLOBAL_BYTES,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    scope: Scope,
    next_ordinal: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seen {
    ordinal: u64,
    endpoint_digest: String,
    revision: u64,
}

impl StateStore {
    pub(crate) fn load_observed_notices(&self, scope: &Scope) -> Result<Vec<Notice>> {
        let prefix = scope.key()?;
        scope.revalidate()?;
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        // Size inspection and body reads share one SQLite snapshot.
        let transaction = inner.connection.transaction()?;
        let notices = load(&transaction, scope, &prefix, LIMITS.conversation)?;
        transaction.commit()?;
        Ok(notices)
    }

    pub(crate) fn preserve_observed_notices(
        &self,
        scope: &Scope,
        notices: &[Notice],
    ) -> Result<()> {
        self.preserve_observed_notices_with_limits(scope, notices, LIMITS)
    }

    fn preserve_observed_notices_with_limits(
        &self,
        scope: &Scope,
        notices: &[Notice],
        limits: Limits,
    ) -> Result<()> {
        let prefix = scope.key()?;
        scope.revalidate()?;
        validate_offered(scope.provider, notices, limits.conversation)?;
        let mut inner = self.lock_inner()?;
        ensure_open(&inner)?;
        if notices.is_empty() {
            // Empty snapshots cannot remove observations they never contained.
            return Ok(());
        }
        let transaction = inner
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Refuse malformed saved rows rather than repairing them by rotation.
        // Decoding stays off the UI thread and is bounded by the journal cap.
        let mut retained = load(&transaction, scope, &prefix, limits.conversation)?;
        let mut header = read_header(&transaction, scope, &prefix, limits.conversation)?
            .unwrap_or_else(|| Header {
                scope: scope.clone(),
                next_ordinal: 1,
            });
        let mut changed = false;
        for notice in notices {
            let seen_key = format!("{prefix}:seen:{}", notice_digest(notice));
            if let Some(seen) = seen_marker(&transaction, &seen_key)? {
                let ordinal = seen.ordinal;
                if ordinal == 0 || ordinal >= header.next_ordinal {
                    return Err(StoreError::corrupt_row(
                        FIELD,
                        "invalid seen-notice ordinal",
                    ));
                }
                let Some(stored) = retained
                    .iter_mut()
                    .find(|stored| stored.message == notice.message && stored.text == notice.text)
                else {
                    // The retired identity can acknowledge an exact replay,
                    // but its unavailable body cannot prove an anchor update.
                    if endpoint_digest(notice.after.as_deref()) != seen.endpoint_digest {
                        return Err(invalid(
                            "A rotated observed notice cannot prove a different anchor",
                        ));
                    }
                    continue;
                };
                if notice.after == stored.after {
                    // Exact endpoint retries are idempotent regardless of the
                    // caller's older/newer token. Do not mint a new revision.
                    continue;
                }
                if notice.revision == 0 || notice.revision != stored.revision {
                    return Err(invalid(
                        "The observed notice anchor changed without its current revision",
                    ));
                }
                let mut updated = notice.clone();
                updated.revision = stored
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| invalid("The observed-notice anchor revision is exhausted"))?;
                write_notice(
                    &transaction,
                    &prefix,
                    ordinal,
                    &updated,
                    limits.conversation,
                )?;
                *stored = updated;
                changed = true;
                continue;
            }
            if notice.revision != 0 {
                return Err(invalid(
                    "A new observed notice cannot claim an existing anchor revision",
                ));
            }
            let ordinal = header.next_ordinal;
            header.next_ordinal = ordinal
                .checked_add(1)
                .filter(|next| *next <= i64::MAX as u64)
                .ok_or_else(|| invalid("The observed-notice journal ordinal is exhausted"))?;
            let mut stored = notice.clone();
            stored.revision = 1;
            write_notice(&transaction, &prefix, ordinal, &stored, limits.conversation)?;
            retained.push(stored);
            changed = true;
        }
        if !changed {
            transaction.commit()?;
            return Ok(());
        }
        // Only raw bodies rotate. The bounded digest ledger remembers every
        // committed identity, so a late window cannot make old bodies new.
        transaction.execute(
            "DELETE FROM metadata WHERE key IN
                (SELECT key FROM metadata WHERE key GLOB ?1
                 ORDER BY key DESC LIMIT -1 OFFSET ?2)",
            rusqlite::params![format!("{prefix}:notice:*"), MAX_NOTICES],
        )?;
        let encoded = encode_bounded(&header, MAX_SCOPE_BYTES)?;
        let encoded =
            std::str::from_utf8(&encoded).expect("serde_json always writes a UTF-8 JSON document");
        set_metadata(&transaction, &format!("{prefix}:header"), encoded)?;
        // These are live metadata bytes, including all keys and tombstones,
        // not an asserted bound on SQLite page slack or historical WAL bytes.
        if namespace_bytes(&transaction, &format!("{prefix}:*"))? > limits.conversation as u64 {
            return Err(invalid(
                "This conversation's observed-notice journal budget is full",
            ));
        }
        if namespace_bytes(&transaction, KEY_GLOB)? > limits.global as u64 {
            return Err(invalid(
                "The observed-notice journal storage budget is full",
            ));
        }
        transaction.commit()?;
        Ok(())
    }
}

fn load(connection: &Connection, scope: &Scope, prefix: &str, limit: usize) -> Result<Vec<Notice>> {
    let Some(header) = read_header(connection, scope, prefix, limit)? else {
        return Ok(Vec::new());
    };
    let mut statement = connection.prepare(
        "SELECT length(CAST(key AS BLOB)), length(CAST(value AS BLOB))
         FROM metadata WHERE key GLOB ?1 ORDER BY key LIMIT ?2",
    )?;
    let glob = format!("{prefix}:notice:*");
    let lengths = statement
        .query_map(rusqlite::params![&glob, MAX_NOTICES + 1], |row| {
            Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if lengths.len() > MAX_NOTICES
        || lengths
            .iter()
            .any(|(key, body)| *key > 256 || *body > limit as u64)
    {
        return Err(StoreError::corrupt_row(
            FIELD,
            "invalid raw-notice row bounds",
        ));
    }
    // All body allocations happen only after the SQL lengths above. The read
    // transaction prevents a competing connection changing those rows now.
    let mut statement = connection
        .prepare("SELECT key, value FROM metadata WHERE key GLOB ?1 ORDER BY key LIMIT ?2")?;
    let mut rows = statement.query(rusqlite::params![&glob, MAX_NOTICES + 1])?;
    let mut notices = Vec::with_capacity(lengths.len());
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let body: String = row.get(1)?;
        let ordinal = key
            .strip_prefix(&format!("{prefix}:notice:"))
            .and_then(|part| part.parse::<u64>().ok())
            .filter(|ordinal| *ordinal > 0 && *ordinal < header.next_ordinal)
            .filter(|ordinal| raw_key(prefix, *ordinal) == key)
            .ok_or_else(|| StoreError::corrupt_row(FIELD, "invalid raw-notice ordinal"))?;
        let notice: Notice =
            serde_json::from_str(&body).map_err(|error| StoreError::corrupt_row(FIELD, error))?;
        if !is_capacity(scope.provider, &notice) {
            return Err(StoreError::corrupt_row(
                FIELD,
                "saved notice is not a capacity diagnostic",
            ));
        }
        let seen = seen_marker(
            connection,
            &format!("{prefix}:seen:{}", notice_digest(&notice)),
        )?;
        if !seen.is_some_and(|seen| {
            seen.ordinal == ordinal
                && seen.revision == notice.revision
                && seen.endpoint_digest == endpoint_digest(notice.after.as_deref())
        }) {
            return Err(StoreError::corrupt_row(
                FIELD,
                "raw notice has no matching observation marker",
            ));
        }
        notices.push(notice);
    }
    Ok(notices)
}

fn read_header(
    connection: &Connection,
    scope: &Scope,
    prefix: &str,
    limit: usize,
) -> Result<Option<Header>> {
    let total = namespace_bytes(connection, &format!("{prefix}:*"))?;
    if total > limit as u64 {
        return Err(StoreError::corrupt_row(
            FIELD,
            "saved journal exceeds its byte limit",
        ));
    }
    let key = format!("{prefix}:header");
    let Some(bytes) = stored_bytes(connection, &key)? else {
        return if total == 0 {
            Ok(None)
        } else {
            Err(StoreError::corrupt_row(
                FIELD,
                "journal has no scope header",
            ))
        };
    };
    if bytes > MAX_SCOPE_BYTES as u64 {
        return Err(StoreError::corrupt_row(
            FIELD,
            "saved scope header exceeds its byte limit",
        ));
    }
    let body = metadata(connection, &key)?.ok_or_else(|| {
        StoreError::corrupt_row(FIELD, "scope header disappeared during its read")
    })?;
    let header: Header =
        serde_json::from_str(&body).map_err(|error| StoreError::corrupt_row(FIELD, error))?;
    if header.scope != *scope || header.next_ordinal == 0 || header.next_ordinal > i64::MAX as u64 {
        return Err(StoreError::corrupt_row(
            FIELD,
            "invalid conversation scope or ordinal",
        ));
    }
    Ok(Some(header))
}

fn namespace_bytes(connection: &Connection, glob: &str) -> Result<u64> {
    Ok(connection.query_row(
        "SELECT coalesce(sum(length(CAST(key AS BLOB)) + length(CAST(value AS BLOB))), 0)
         FROM metadata WHERE key GLOB ?1",
        [glob],
        |row| row.get(0),
    )?)
}

fn stored_bytes(connection: &Connection, key: &str) -> Result<Option<u64>> {
    Ok(connection
        .query_row(
            "SELECT length(CAST(value AS BLOB)) FROM metadata WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()?)
}

fn seen_marker(connection: &Connection, key: &str) -> Result<Option<Seen>> {
    let Some(bytes) = stored_bytes(connection, key)? else {
        return Ok(None);
    };
    if bytes == 0 || bytes > MAX_MARKER_BYTES as u64 {
        return Err(StoreError::corrupt_row(
            FIELD,
            "invalid observation marker size",
        ));
    }
    let body = metadata(connection, key)?
        .ok_or_else(|| StoreError::corrupt_row(FIELD, "observation marker disappeared"))?;
    let seen: Seen =
        serde_json::from_str(&body).map_err(|error| StoreError::corrupt_row(FIELD, error))?;
    if seen.ordinal == 0
        || seen.ordinal >= i64::MAX as u64
        || seen.revision == 0
        || seen.endpoint_digest.len() != 64
        || !seen
            .endpoint_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StoreError::corrupt_row(FIELD, "invalid observation marker"));
    }
    Ok(Some(seen))
}

fn raw_key(prefix: &str, ordinal: u64) -> String {
    format!("{prefix}:notice:{ordinal:020}")
}

fn write_notice(
    transaction: &rusqlite::Transaction<'_>,
    prefix: &str,
    ordinal: u64,
    notice: &Notice,
    limit: usize,
) -> Result<()> {
    let encoded = encode_bounded(notice, limit)?;
    let encoded =
        std::str::from_utf8(&encoded).expect("serde_json always writes a UTF-8 JSON document");
    set_metadata(transaction, &raw_key(prefix, ordinal), encoded)?;
    let seen = Seen {
        ordinal,
        endpoint_digest: endpoint_digest(notice.after.as_deref()),
        revision: notice.revision,
    };
    let marker = encode_bounded(&seen, MAX_MARKER_BYTES)?;
    let marker =
        std::str::from_utf8(&marker).expect("serde_json always writes a UTF-8 JSON document");
    set_metadata(
        transaction,
        &format!("{prefix}:seen:{}", notice_digest(notice)),
        marker,
    )
}

fn is_capacity(provider: Provider, notice: &Notice) -> bool {
    Failure::parse(provider, &notice.text)
        .is_some_and(|failure| failure.kind == FailureKind::Capacity)
}

fn endpoint_digest(after: Option<&str>) -> String {
    let mut digest = Sha256::new();
    digest.update([u8::from(after.is_some())]);
    if let Some(after) = after {
        digest.update((after.len() as u64).to_be_bytes());
        digest.update(after.as_bytes());
    }
    format!("{digest:x}", digest = digest.finalize())
}

fn notice_digest(notice: &Notice) -> String {
    let mut digest = Sha256::new();
    digest.update([u8::from(notice.message.is_some())]);
    if let Some(message) = &notice.message {
        digest.update((message.len() as u64).to_be_bytes());
        digest.update(message.as_bytes());
    }
    digest.update((notice.text.len() as u64).to_be_bytes());
    digest.update(notice.text.as_bytes());
    format!("{digest:x}", digest = digest.finalize())
}

fn validate_offered(provider: Provider, notices: &[Notice], limit: usize) -> Result<()> {
    if notices.len() > MAX_NOTICES {
        return Err(invalid("Too many observed diagnostic notices in one write"));
    }
    let mut bytes = 0usize;
    for notice in notices {
        if !is_capacity(provider, notice) {
            return Err(invalid(
                "Only observed capacity diagnostics can be preserved",
            ));
        }
        for part in [
            notice.message.as_deref(),
            notice.after.as_deref(),
            Some(notice.text.as_ref()),
        ] {
            bytes = bytes
                .checked_add(part.map_or(0, str::len))
                .ok_or_else(|| invalid("The observed diagnostic notices are too large"))?;
        }
        if bytes > limit {
            return Err(invalid(
                "The observed diagnostic notices exceed their byte limit",
            ));
        }
    }
    Ok(())
}

fn encode_bounded(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
        refused: bool,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                self.refused = true;
                return Err(io::Error::other(
                    "observed-notice journal exceeds its byte limit",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = Bounded {
        bytes: Vec::with_capacity(limit.min(4096)),
        limit,
        refused: false,
    };
    if let Err(error) = serde_json::to_writer(&mut output, value) {
        return if output.refused {
            Err(invalid(
                "The observed-notice journal exceeds its byte limit",
            ))
        } else {
            Err(StoreError::Serialization(error))
        };
    }
    Ok(output.bytes)
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidState(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, LaunchRecipe};
    use std::{collections::BTreeMap, os::unix::fs::symlink};

    struct Fixture {
        root: tempfile::TempDir,
        store: StateStore,
        launch: Launch,
        scope: Scope,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("account");
        std::fs::create_dir(&home).unwrap();
        let launch = Launch {
            provider: Provider::Claude,
            recipe: LaunchRecipe {
                program: "claude".into(),
                arguments: Vec::new(),
                working_directory: root.path().to_owned(),
                environment: BTreeMap::from([(
                    "CLAUDE_CONFIG_DIR".into(),
                    home.to_str().unwrap().to_owned(),
                )]),
                unset_environment: Default::default(),
            },
            session_id: Some("abc12345-1234-4234-8234-123456789abc".into()),
        };
        let scope = Scope::resolve_local(&launch, "fixture-machine".into()).unwrap();
        let store =
            StateStore::open(root.path().join("state.sqlite3"), BootId::new("test")).unwrap();
        Fixture {
            root,
            store,
            launch,
            scope,
        }
    }

    fn note(index: usize) -> Notice {
        Notice {
            message: Some(format!("prompt-{index}")),
            after: Some(format!("item-{index}")),
            text: format!("Provider is at capacity. Exact diagnostic {index}: λ\nraw details\0")
                .into(),
            revision: 0,
        }
    }

    fn durable(mut notice: Notice) -> Notice {
        notice.revision = 1;
        notice
    }

    fn rows(store: &StateStore) -> Vec<(String, String)> {
        let inner = store.lock_inner().unwrap();
        let mut statement = inner
            .connection
            .prepare("SELECT key,value FROM metadata WHERE key GLOB ?1 ORDER BY key")
            .unwrap();
        statement
            .query_map([KEY_GLOB], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }

    fn bytes(store: &StateStore, scope: &Scope) -> usize {
        let inner = store.lock_inner().unwrap();
        namespace_bytes(&inner.connection, &format!("{}:*", scope.key().unwrap())).unwrap() as usize
    }

    #[test]
    fn retains_exact_large_body_outside_workspace_and_survives_reopen() {
        let fixture = fixture();
        let original = Notice {
            text: format!(
                "Provider is at capacity. {}\nλ\0raw ending",
                "x".repeat(4 * 1024 * 1024 + 1)
            )
            .into(),
            ..note(0)
        };
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[original.clone()])
            .unwrap();
        let reopened = StateStore::open(
            fixture.root.path().join("state.sqlite3"),
            BootId::new("test"),
        )
        .unwrap();
        assert_eq!(
            reopened.load_observed_notices(&fixture.scope).unwrap(),
            [durable(original)]
        );
        let inner = fixture.store.lock_inner().unwrap();
        let descriptors: u64 = inner
            .connection
            .query_row("SELECT count(*) FROM cards", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            descriptors, 0,
            "journal writes cannot create conversation descriptors"
        );
    }

    #[test]
    fn newest_32_rotate_without_stale_resurrection_or_anchor_changes() {
        let fixture = fixture();
        let other = StateStore::open(
            fixture.root.path().join("state.sqlite3"),
            BootId::new("test"),
        )
        .unwrap();
        let first: Vec<_> = (0..32).map(note).collect();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &first)
            .unwrap();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(32)])
            .unwrap();
        let expected: Vec<_> = (1..33).map(|index| durable(note(index))).collect();
        assert_eq!(
            fixture.store.load_observed_notices(&fixture.scope).unwrap(),
            expected
        );
        let before = rows(&fixture.store);
        other
            .preserve_observed_notices(&fixture.scope, &[note(0)])
            .unwrap();
        other
            .preserve_observed_notices(&fixture.scope, &[durable(note(0))])
            .unwrap();
        let mut retired_update = durable(note(0));
        retired_update.after = Some("new-endpoint-for-retired-body".into());
        assert!(
            other
                .preserve_observed_notices(&fixture.scope, &[retired_update])
                .unwrap_err()
                .refuses_content()
        );
        other
            .preserve_observed_notices(&fixture.scope, &[])
            .unwrap();
        let mut moved = note(31);
        moved.after = Some("another-late-item".into());
        assert!(
            other
                .preserve_observed_notices(&fixture.scope, &[moved])
                .unwrap_err()
                .refuses_content()
        );
        assert_eq!(rows(&fixture.store), before);
        assert_eq!(
            fixture.store.load_observed_notices(&fixture.scope).unwrap(),
            expected
        );
    }

    #[test]
    fn identity_distinguishes_later_attempt_and_same_attempt_different_details() {
        let fixture = fixture();
        let first = note(0);
        let next_attempt = Notice {
            message: Some("next-prompt".into()),
            ..first.clone()
        };
        let next_details = Notice {
            text: "Provider is at capacity. Different exact body".into(),
            ..first.clone()
        };
        let offered = vec![first, next_attempt, next_details];
        let expected: Vec<_> = offered.iter().cloned().map(durable).collect();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &offered)
            .unwrap();
        assert_eq!(
            fixture.store.load_observed_notices(&fixture.scope).unwrap(),
            expected
        );
    }

    #[test]
    fn account_provider_thread_and_owner_scopes_do_not_mix() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(0)])
            .unwrap();
        let mut variants = Vec::new();
        let mut other = fixture.scope.clone();
        other.owner_domain = "other-host".into();
        variants.push(other);
        let mut other = fixture.scope.clone();
        other.local_uid = other.local_uid.wrapping_add(1);
        variants.push(other);
        let mut other = fixture.scope.clone();
        other.provider = Provider::Codex;
        variants.push(other);
        let mut other = fixture.scope.clone();
        other.session_id = uuid::Uuid::new_v4().to_string();
        variants.push(other);
        let home = fixture.root.path().join("other-account");
        std::fs::create_dir(&home).unwrap();
        let mut launch = fixture.launch.clone();
        launch
            .recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), home.to_str().unwrap().into());
        variants.push(Scope::resolve_local(&launch, "fixture-machine".into()).unwrap());
        for scope in variants {
            assert_ne!(scope.key().unwrap(), fixture.scope.key().unwrap());
            assert!(
                fixture
                    .store
                    .load_observed_notices(&scope)
                    .unwrap()
                    .is_empty()
            );
            fixture
                .store
                .preserve_observed_notices(&scope, &[note(1)])
                .unwrap();
        }
        assert_eq!(
            fixture.store.load_observed_notices(&fixture.scope).unwrap(),
            [durable(note(0))]
        );
    }

    #[test]
    fn scope_alias_and_uuid_case_resolve_to_the_same_physical_conversation() {
        let fixture = fixture();
        let alias = fixture.root.path().join("alias");
        symlink(&fixture.scope.canonical_home, &alias).unwrap();
        let mut launch = fixture.launch.clone();
        launch
            .recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), alias.to_str().unwrap().into());
        launch.session_id = launch.session_id.map(|id| id.to_uppercase());
        assert_eq!(
            Scope::resolve_local(&launch, "fixture-machine".into()).unwrap(),
            fixture.scope
        );
    }

    #[test]
    fn missing_unset_or_unconfirmed_scope_refuses_inherited_fallback() {
        let fixture = fixture();
        for kind in 0..4 {
            let mut launch = fixture.launch.clone();
            match kind {
                0 => {
                    launch.recipe.environment.clear();
                }
                1 => {
                    launch
                        .recipe
                        .unset_environment
                        .insert("CLAUDE_CONFIG_DIR".into());
                }
                2 => {
                    launch.session_id = None;
                }
                _ => {
                    launch.session_id = Some("latest".into());
                }
            }
            assert!(Scope::resolve_local(&launch, "fixture-machine".into()).is_err());
        }
    }

    #[test]
    fn replacing_the_captured_home_refuses_old_scope_reads_and_writes() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(0)])
            .unwrap();
        std::fs::rename(
            &fixture.scope.canonical_home,
            fixture.root.path().join("retired-account"),
        )
        .unwrap();
        std::fs::create_dir(&fixture.scope.canonical_home).unwrap();
        let before = rows(&fixture.store);
        assert!(fixture.store.load_observed_notices(&fixture.scope).is_err());
        assert!(
            fixture
                .store
                .preserve_observed_notices(&fixture.scope, &[note(1)])
                .is_err()
        );
        assert_eq!(rows(&fixture.store), before);
        let replacement = Scope::resolve_local(&fixture.launch, "fixture-machine".into()).unwrap();
        assert_ne!(replacement, fixture.scope);
        assert!(
            fixture
                .store
                .load_observed_notices(&replacement)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn conversation_and_global_refusals_roll_back_header_body_and_ledger() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(0)])
            .unwrap();
        let baseline = bytes(&fixture.store, &fixture.scope);
        for limits in [
            Limits {
                conversation: baseline + 1,
                global: MAX_GLOBAL_BYTES,
            },
            Limits {
                conversation: MAX_CONVERSATION_BYTES,
                global: baseline + 1,
            },
        ] {
            let before = rows(&fixture.store);
            let refusal = fixture
                .store
                .preserve_observed_notices_with_limits(&fixture.scope, &[note(1)], limits)
                .unwrap_err();
            assert!(matches!(refusal, StoreError::InvalidState(_)));
            assert!(refusal.refuses_content());
            assert_eq!(rows(&fixture.store), before);
            fixture
                .store
                .save_window_view("unrelated workspace edit")
                .unwrap();
            assert_eq!(
                fixture.store.load_window_view().unwrap().as_deref(),
                Some("unrelated workspace edit")
            );
        }
    }

    #[test]
    fn global_budget_counts_other_journals_and_preserves_every_old_byte() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(0)])
            .unwrap();
        let mut other = fixture.scope.clone();
        other.session_id = uuid::Uuid::new_v4().to_string();
        let before = rows(&fixture.store);
        let refusal = fixture
            .store
            .preserve_observed_notices_with_limits(
                &other,
                &[note(1)],
                Limits {
                    conversation: MAX_CONVERSATION_BYTES,
                    global: bytes(&fixture.store, &fixture.scope),
                },
            )
            .unwrap_err();
        assert!(refusal.refuses_content());
        assert_eq!(rows(&fixture.store), before);
        assert!(
            fixture
                .store
                .load_observed_notices(&other)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn ledger_keys_and_values_are_charged_after_raw_rotation() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &(0..32).map(note).collect::<Vec<_>>())
            .unwrap();
        let before = rows(&fixture.store);
        let baseline = bytes(&fixture.store, &fixture.scope);
        let refusal = fixture
            .store
            .preserve_observed_notices_with_limits(
                &fixture.scope,
                &[note(32)],
                Limits {
                    conversation: baseline,
                    global: MAX_GLOBAL_BYTES,
                },
            )
            .unwrap_err();
        assert!(refusal.refuses_content());
        assert_eq!(
            rows(&fixture.store),
            before,
            "rotation must roll back with its new marker"
        );
    }

    #[test]
    fn serialized_escape_growth_refuses_without_truncation() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(0)])
            .unwrap();
        let before = rows(&fixture.store);
        let escaped = Notice {
            text: format!("Provider is at capacity. {}", "\0".repeat(1024)).into(),
            ..note(1)
        };
        let refusal = fixture
            .store
            .preserve_observed_notices_with_limits(
                &fixture.scope,
                &[escaped],
                Limits {
                    conversation: 4096,
                    global: MAX_GLOBAL_BYTES,
                },
            )
            .unwrap_err();
        assert!(refusal.refuses_content());
        assert_eq!(rows(&fixture.store), before);
    }

    #[test]
    fn oversized_header_is_rejected_by_sql_length_before_json_decode() {
        let fixture = fixture();
        let key = format!("{}:header", fixture.scope.key().unwrap());
        fixture
            .store
            .lock_inner()
            .unwrap()
            .connection
            .execute(
                "INSERT INTO metadata(key,value) VALUES(?1,replace(hex(zeroblob(?2)),'00','λ'))",
                rusqlite::params![key, MAX_SCOPE_BYTES / 2 + 1],
            )
            .unwrap();
        let error = fixture
            .store
            .load_observed_notices(&fixture.scope)
            .unwrap_err();
        assert!(
            matches!(error, StoreError::CorruptRow { reason, .. } if reason == "saved scope header exceeds its byte limit")
        );
    }

    #[test]
    fn malformed_saved_notice_refuses_write_without_overwriting() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(0)])
            .unwrap();
        fixture
            .store
            .lock_inner()
            .unwrap()
            .connection
            .execute(
                "UPDATE metadata SET value='not json' WHERE key=?1",
                [raw_key(&fixture.scope.key().unwrap(), 1)],
            )
            .unwrap();
        let before = rows(&fixture.store);
        assert!(matches!(
            fixture
                .store
                .preserve_observed_notices(&fixture.scope, &[note(1)]),
            Err(StoreError::CorruptRow { .. })
        ));
        assert_eq!(rows(&fixture.store), before);
    }

    #[test]
    fn ordinal_exhaustion_refuses_atomically() {
        let fixture = fixture();
        let header = Header {
            scope: fixture.scope.clone(),
            next_ordinal: i64::MAX as u64,
        };
        fixture
            .store
            .lock_inner()
            .unwrap()
            .connection
            .execute(
                "INSERT INTO metadata(key,value) VALUES(?1,?2)",
                rusqlite::params![
                    format!("{}:header", fixture.scope.key().unwrap()),
                    serde_json::to_string(&header).unwrap()
                ],
            )
            .unwrap();
        let before = rows(&fixture.store);
        assert!(
            fixture
                .store
                .preserve_observed_notices(&fixture.scope, &[note(0)])
                .unwrap_err()
                .refuses_content()
        );
        assert_eq!(rows(&fixture.store), before);
    }

    #[test]
    fn new_notice_starts_at_revision_one_and_same_endpoint_retries_are_idempotent() {
        let fixture = fixture();
        let mut json = serde_json::to_value(note(0)).unwrap();
        json.as_object_mut().unwrap().remove("revision");
        let unversioned: Notice = serde_json::from_value(json).unwrap();
        assert_eq!(
            unversioned.revision, 0,
            "legacy input has no anchor authority"
        );
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[unversioned])
            .unwrap();
        assert_eq!(
            fixture.store.load_observed_notices(&fixture.scope).unwrap(),
            [durable(note(0))]
        );
        let before = rows(&fixture.store);
        for revision in [0, 1, 42, u64::MAX] {
            let mut retry = note(0);
            retry.revision = revision;
            fixture
                .store
                .preserve_observed_notices(&fixture.scope, &[retry])
                .unwrap();
        }
        assert_eq!(
            rows(&fixture.store),
            before,
            "same endpoint cannot mint a new revision"
        );
    }

    #[test]
    fn new_identity_cannot_claim_a_durable_revision() {
        let fixture = fixture();
        let before = rows(&fixture.store);
        assert!(
            fixture
                .store
                .preserve_observed_notices(&fixture.scope, &[durable(note(0))])
                .unwrap_err()
                .refuses_content()
        );
        assert_eq!(rows(&fixture.store), before);
    }

    #[test]
    fn current_revision_moves_anchor_once_without_changing_notice_order_or_identity() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(0), note(1)])
            .unwrap();
        let loaded = fixture.store.load_observed_notices(&fixture.scope).unwrap();
        let mut moved = loaded[0].clone();
        moved.after = Some("confirmed-new-endpoint".into());
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[moved.clone()])
            .unwrap();
        let after = fixture.store.load_observed_notices(&fixture.scope).unwrap();
        moved.revision = 2;
        assert_eq!(after, [moved, loaded[1].clone()]);
        let prefix = fixture.scope.key().unwrap();
        let inner = fixture.store.lock_inner().unwrap();
        let marker = seen_marker(
            &inner.connection,
            &format!("{prefix}:seen:{}", notice_digest(&after[0])),
        )
        .unwrap();
        assert_eq!(
            marker.as_ref().map(|seen| seen.ordinal),
            Some(1),
            "anchor updates do not become fresh observations"
        );
        let marker = marker.unwrap();
        assert_eq!(marker.revision, 2);
        assert_eq!(
            marker.endpoint_digest,
            endpoint_digest(after[0].after.as_deref())
        );
    }

    #[test]
    fn stale_unversioned_and_future_anchor_changes_roll_back_the_entire_offer() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(0)])
            .unwrap();
        let mut moved = fixture
            .store
            .load_observed_notices(&fixture.scope)
            .unwrap()
            .remove(0);
        moved.after = Some("first-authorized-move".into());
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[moved])
            .unwrap();
        let before = rows(&fixture.store);
        for revision in [0, 1, 3, u64::MAX] {
            let changed = Notice {
                after: Some("unproved-later-move".into()),
                revision,
                ..note(0)
            };
            let refusal = fixture
                .store
                .preserve_observed_notices(&fixture.scope, &[note(1), changed])
                .unwrap_err();
            assert!(matches!(refusal, StoreError::InvalidState(_)));
            assert_eq!(
                rows(&fixture.store),
                before,
                "no partial new note, marker or revision may commit"
            );
        }
    }

    #[test]
    fn anchor_revision_overflow_refuses_without_rewriting_the_notice() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(0)])
            .unwrap();
        let mut exhausted = durable(note(0));
        exhausted.revision = u64::MAX;
        {
            let mut inner = fixture.store.lock_inner().unwrap();
            let transaction = inner.connection.transaction().unwrap();
            write_notice(
                &transaction,
                &fixture.scope.key().unwrap(),
                1,
                &exhausted,
                MAX_CONVERSATION_BYTES,
            )
            .unwrap();
            transaction.commit().unwrap();
        }
        let mut changed = fixture
            .store
            .load_observed_notices(&fixture.scope)
            .unwrap()
            .remove(0);
        changed.after = Some("new-endpoint".into());
        let before = rows(&fixture.store);
        assert!(
            fixture
                .store
                .preserve_observed_notices(&fixture.scope, &[changed])
                .unwrap_err()
                .refuses_content()
        );
        assert_eq!(rows(&fixture.store), before);
    }

    #[test]
    fn noncapacity_offers_cannot_write_auth_or_other_diagnostics() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(0)])
            .unwrap();
        let before = rows(&fixture.store);
        for provider in [Provider::Claude, Provider::Codex] {
            let mut scope = fixture.scope.clone();
            scope.provider = provider;
            for text in [
                "Authentication failed. Please sign in again.",
                "You've hit your limit",
                "Connection refused",
                "An unknown internal error",
            ] {
                let noncapacity = Notice {
                    text: text.into(),
                    ..note(1)
                };
                let refusal = fixture
                    .store
                    .preserve_observed_notices(&scope, &[noncapacity])
                    .unwrap_err();
                assert!(matches!(refusal, StoreError::InvalidState(_)));
                assert_eq!(rows(&fixture.store), before);
            }
        }
    }

    #[test]
    fn saved_auth_diagnostic_is_refused_before_it_can_be_loaded() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(0)])
            .unwrap();
        let auth = Notice {
            text: "Authentication failed. Please sign in again.".into(),
            ..durable(note(0))
        };
        fixture
            .store
            .lock_inner()
            .unwrap()
            .connection
            .execute(
                "UPDATE metadata SET value=?2 WHERE key=?1",
                rusqlite::params![
                    raw_key(&fixture.scope.key().unwrap(), 1),
                    serde_json::to_string(&auth).unwrap()
                ],
            )
            .unwrap();
        let before = rows(&fixture.store);
        let error = fixture
            .store
            .load_observed_notices(&fixture.scope)
            .unwrap_err();
        assert!(
            matches!(error, StoreError::CorruptRow { reason, .. } if reason == "saved notice is not a capacity diagnostic")
        );
        assert!(matches!(
            fixture
                .store
                .preserve_observed_notices(&fixture.scope, &[note(1)]),
            Err(StoreError::CorruptRow { .. })
        ));
        assert_eq!(rows(&fixture.store), before);
    }

    #[test]
    fn rotated_notice_keeps_its_last_accepted_anchor_without_resurrecting_the_body() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(0)])
            .unwrap();
        let mut moved = fixture
            .store
            .load_observed_notices(&fixture.scope)
            .unwrap()
            .remove(0);
        moved.after = Some("last-proven-endpoint".into());
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[moved])
            .unwrap();
        let accepted = fixture
            .store
            .load_observed_notices(&fixture.scope)
            .unwrap()
            .remove(0);
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &(1..33).map(note).collect::<Vec<_>>())
            .unwrap();
        let before = rows(&fixture.store);
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[accepted.clone()])
            .unwrap();
        for refused in [
            note(0),
            Notice {
                after: Some("unproved-new-endpoint".into()),
                ..accepted
            },
        ] {
            assert!(
                fixture
                    .store
                    .preserve_observed_notices(&fixture.scope, &[refused])
                    .unwrap_err()
                    .refuses_content()
            );
            assert_eq!(rows(&fixture.store), before);
        }
        assert_eq!(
            fixture.store.load_observed_notices(&fixture.scope).unwrap(),
            (1..33)
                .map(|index| durable(note(index)))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn oversized_marker_is_rejected_by_sql_length_before_json_decode() {
        let fixture = fixture();
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[note(0)])
            .unwrap();
        let key = format!(
            "{}:seen:{}",
            fixture.scope.key().unwrap(),
            notice_digest(&note(0))
        );
        fixture
            .store
            .lock_inner()
            .unwrap()
            .connection
            .execute(
                "UPDATE metadata SET value=replace(hex(zeroblob(?2)),'00','λ') WHERE key=?1",
                rusqlite::params![key, MAX_MARKER_BYTES / 2 + 1],
            )
            .unwrap();
        let before = rows(&fixture.store);
        let error = fixture
            .store
            .load_observed_notices(&fixture.scope)
            .unwrap_err();
        assert!(
            matches!(error, StoreError::CorruptRow { reason, .. } if reason == "invalid observation marker size")
        );
        assert!(matches!(
            fixture
                .store
                .preserve_observed_notices(&fixture.scope, &[note(1)]),
            Err(StoreError::CorruptRow { .. })
        ));
        assert_eq!(rows(&fixture.store), before);
    }

    #[test]
    fn exact_selected_model_capacity_message_round_trips_as_a_capacity_notice() {
        let fixture = fixture();
        let notice = Notice {
            text: "Selected model is at capacity. Please try a different model.".into(),
            ..note(0)
        };
        fixture
            .store
            .preserve_observed_notices(&fixture.scope, &[notice.clone()])
            .unwrap();
        assert_eq!(
            fixture.store.load_observed_notices(&fixture.scope).unwrap(),
            [durable(notice)]
        );
    }
}
