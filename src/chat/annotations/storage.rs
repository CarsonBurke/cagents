//! Small durable command/decision authority, separate from ephemeral body caches.
use super::*;
use crate::account::scope::LaunchAccountScope;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

const CHUNK: usize = 128 * 1024;
const ID_BYTES: usize = 4096;
const META_BYTES: usize = 64 * 1024;

pub(crate) struct Store {
    db: Connection,
    scope: LaunchAccountScope,
    directory: PathBuf,
}
/// Opaque physical annotation namespace. This is invalidation metadata only;
/// possessing it never authorizes reading or recreating an annotation store.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct OriginalLineage(String);

impl OriginalLineage {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PurgeReceipt {
    /// Namespaces blocked by a visible marker or a fail-closed marker lookup.
    /// Includes already-absent namespaces; this does not promise durable sync.
    pub(crate) revoked: Vec<OriginalLineage>,
    pub(crate) removed_namespaces: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PurgeFailure {
    pub(crate) message: String,
    /// A later failure cannot discard a marker that already blocks reads.
    pub(crate) receipt: PurgeReceipt,
}

impl PurgeReceipt {
    fn record_revoked(&mut self, key: &str) {
        let lineage = OriginalLineage(key.to_owned());
        if !self.revoked.contains(&lineage) {
            self.revoked.push(lineage);
            self.revoked.sort();
        }
    }
}

impl From<String> for PurgeFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            receipt: PurgeReceipt::default(),
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= ID_BYTES
}
fn account_key(scope: &LaunchAccountScope) -> String {
    let mut hash = Sha256::new();
    hash.update(postcard::to_stdvec(&scope.provider).expect("provider serialization"));
    hash.update((scope.canonical_home.as_os_str().as_bytes().len() as u64).to_le_bytes());
    hash.update(scope.canonical_home.as_os_str().as_bytes());
    hash.update(scope.device.to_le_bytes());
    hash.update(scope.inode.to_le_bytes());
    format!("{:x}", hash.finalize())
}
fn reject_revoked(root: &Path, key: &str) -> Result<(), String> {
    let marker = root.join("revoked-accounts").join(key);
    match fs::symlink_metadata(marker) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
        Ok(_) => Err(
            "Original command authority was explicitly deleted for this physical account".into(),
        ),
    }
}
fn revoke_account(root: &Path, key: &str, receipt: &mut PurgeReceipt) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let directory = root.join("revoked-accounts");
        crate::secure_file::prepare_private_directory(&directory).map_err(|e| e.to_string())?;
        let path = directory.join(key);
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|e| e.to_string())?;
        // Store::reject_revoked refuses as soon as this pathname exists, before
        // validation or either sync. Keep that effect even if those steps fail.
        receipt.record_revoked(key);
        crate::secure_file::validate_and_privatize_open_file(&path, &file)
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() && reject_revoked(root, key).is_err() {
        // An existing unsafe marker may prevent opening it at all. Match the
        // reader's fail-closed decision without claiming that sync succeeded.
        // A pure pre-effect error with no marker does not add a lineage.
        receipt.record_revoked(key);
    }
    result
}
fn installation_root() -> Result<PathBuf, String> {
    crate::instance::project_dirs()
        .map(|dirs| dirs.data_dir().join("conversation-annotations"))
        .ok_or_else(|| "Cannot locate private conversation annotations".into())
}
/// Invoke while the lifecycle lock still protects the captured existing home.
/// Archive and cache eviction deliberately do not call this function.
pub(crate) fn purge_account(scope: &LaunchAccountScope) -> Result<(), String> {
    purge_account_with_receipt(scope)
        .map(|_| ())
        .map_err(|failure| failure.message)
}

/// Invoke under the same lifecycle custody as `purge_account`. Consumers must
/// retain confirmed revocations even when a later removal step reports a warning.
pub(crate) fn purge_account_with_receipt(
    scope: &LaunchAccountScope,
) -> Result<PurgeReceipt, PurgeFailure> {
    purge_account_receipt_at(&installation_root()?, scope)
}

fn purge_account_at(root: &Path, scope: &LaunchAccountScope) -> Result<(), String> {
    purge_account_receipt_at(root, scope)
        .map(|_| ())
        .map_err(|failure| failure.message)
}

fn remove_revoked_namespace(root: &Path, key: &str) -> Result<bool, String> {
    let path = root.join(key);
    crate::secure_file::reject_symlink_components(&path).map_err(|e| e.to_string())?;
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string()),
        Ok(_) => {
            crate::secure_file::validate_owned_directory(&path).map_err(|e| e.to_string())?;
            fs::remove_dir_all(path).map_err(|e| e.to_string())?;
            Ok(true)
        }
    }
}

fn purge_namespace(root: &Path, key: String, receipt: &mut PurgeReceipt) -> Result<(), String> {
    revoke_account(root, &key, receipt)?;
    if remove_revoked_namespace(root, &key)? {
        receipt.removed_namespaces += 1;
    }
    Ok(())
}

pub(crate) fn purge_account_receipt_at(
    root: &Path,
    scope: &LaunchAccountScope,
) -> Result<PurgeReceipt, PurgeFailure> {
    scope.revalidate()?;
    let mut receipt = PurgeReceipt::default();
    if let Err(message) = purge_namespace(root, account_key(scope), &mut receipt) {
        return Err(PurgeFailure { message, receipt });
    }
    Ok(receipt)
}

/// Purge-only historical association. `None` means this installation has no
/// trustworthy prior association for the registered ID; it grants no read
/// access and must not be used to reconstruct an absent credential home.
pub(crate) fn purge_registered(id: &crate::account::AccountId) -> Result<Option<usize>, String> {
    purge_registered_with_receipt(id)
        .map(|receipt| receipt.map(|receipt| receipt.removed_namespaces))
        .map_err(|failure| failure.message)
}

pub(crate) fn purge_registered_with_receipt(
    id: &crate::account::AccountId,
) -> Result<Option<PurgeReceipt>, PurgeFailure> {
    purge_registered_receipt_at(&installation_root()?, id)
}

fn purge_registered_at(
    root: &Path,
    id: &crate::account::AccountId,
) -> Result<Option<usize>, String> {
    purge_registered_receipt_at(root, id)
        .map(|receipt| receipt.map(|receipt| receipt.removed_namespaces))
        .map_err(|failure| failure.message)
}

pub(crate) fn purge_registered_receipt_at(
    root: &Path,
    id: &crate::account::AccountId,
) -> Result<Option<PurgeReceipt>, PurgeFailure> {
    let directory = root
        .join("purge-index")
        .join(format!("{:x}", Sha256::digest(id.as_str().as_bytes())));
    crate::secure_file::reject_symlink_components(&directory).map_err(|e| e.to_string())?;
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string().into()),
    };
    crate::secure_file::validate_owned_directory(&directory).map_err(|e| e.to_string())?;
    let mut receipt = PurgeReceipt::default();
    let result = (|| -> Result<(), String> {
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or("Invalid annotation purge association")?;
            if name.len() != 64 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("Invalid annotation purge association".into());
            }
            crate::secure_file::open_existing_private_file(&entry.path())
                .map_err(|e| e.to_string())?;
            purge_namespace(root, name.to_owned(), &mut receipt)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(Some(receipt)),
        Err(message) => Err(PurgeFailure { message, receipt }),
    }
}
fn associate_for_purge(root: &Path, scope: &LaunchAccountScope) -> Result<(), String> {
    let mut ids = scope
        .purge_ids
        .iter()
        .chain(scope.registered_id.iter())
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    for id in ids {
        // These associations grant deletion only. Neither ID nor alias may
        // reconstruct a missing physical home or authorize metadata reads.
        let directory = root
            .join("purge-index")
            .join(format!("{:x}", Sha256::digest(id.as_str().as_bytes())));
        crate::secure_file::prepare_private_directory(&directory).map_err(|e| e.to_string())?;
        let path = directory.join(account_key(scope));
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|e| e.to_string())?;
        crate::secure_file::validate_and_privatize_open_file(&path, &file)
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

impl Store {
    pub(crate) fn original_lineage(&self) -> OriginalLineage {
        OriginalLineage(account_key(&self.scope))
    }
    fn revalidate(&self) -> Result<(), String> {
        self.scope.revalidate()?;
        let root = self
            .directory
            .parent()
            .and_then(Path::parent)
            .ok_or("Invalid annotation namespace")?;
        reject_revoked(root, &account_key(&self.scope))
    }

    /// `session` must come from adapter confirmation, never a requested resume
    /// recipe or a later UI selection. Callers retain that provenance.
    pub(crate) fn installation(scope: &LaunchAccountScope, session: &str) -> Result<Self, String> {
        Self::at(&installation_root()?, scope, session)
    }
    pub(crate) fn at(
        root: &Path,
        scope: &LaunchAccountScope,
        session: &str,
    ) -> Result<Self, String> {
        scope.revalidate()?;
        reject_revoked(root, &account_key(scope))?;
        if !valid_id(session) {
            return Err("Invalid confirmed conversation identity".into());
        }
        associate_for_purge(root, scope)?;
        let account = root.join(account_key(scope));
        crate::secure_file::prepare_private_directory(&account).map_err(|e| e.to_string())?;
        let directory = account.join(format!("{:x}", Sha256::digest(session.as_bytes())));
        crate::secure_file::prepare_private_directory(&directory).map_err(|e| e.to_string())?;
        let path = directory.join("annotations.sqlite");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|e| e.to_string())?;
        crate::secure_file::validate_and_privatize_open_file(&path, &file)
            .map_err(|e| e.to_string())?;
        let db = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|e| e.to_string())?;
        db.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        let version: u32 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|e| e.to_string())?;
        if version > 1 {
            return Err(format!(
                "Conversation annotation schema {version} is newer than supported schema 1"
            ));
        }
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-512; PRAGMA foreign_keys=ON;
          CREATE TABLE IF NOT EXISTS prompts(user_id TEXT PRIMARY KEY, metadata BLOB NOT NULL, digest BLOB NOT NULL);
          CREATE TABLE IF NOT EXISTS prompt_text(user_id TEXT NOT NULL REFERENCES prompts(user_id) ON DELETE CASCADE, position INTEGER NOT NULL, body BLOB NOT NULL, PRIMARY KEY(user_id,position));
          CREATE TABLE IF NOT EXISTS aliases(alias TEXT PRIMARY KEY,user_id TEXT NOT NULL REFERENCES prompts(user_id));
          CREATE TABLE IF NOT EXISTS approvals(agent TEXT NOT NULL,item TEXT NOT NULL,request TEXT NOT NULL,decision INTEGER NOT NULL,PRIMARY KEY(agent,item,request));
          CREATE TABLE IF NOT EXISTS current_approval(agent TEXT NOT NULL,item TEXT NOT NULL,request TEXT NOT NULL,PRIMARY KEY(agent,item));
          CREATE TABLE IF NOT EXISTS latest(singleton INTEGER PRIMARY KEY CHECK(singleton=1),user_id TEXT NOT NULL); PRAGMA user_version=1;")
            .map_err(|e|e.to_string())?;
        Ok(Self {
            db,
            scope: scope.clone(),
            directory,
        })
    }
    /// Snapshot an attachment BEFORE dispatch; send the returned private path
    /// to the provider as well, so later changes to the picker path cannot make
    /// the recorded original differ from the input that was actually accepted.
    pub(crate) fn retain_attachment(
        &self,
        path: &Path,
    ) -> Result<OriginalAttachment, OriginalError> {
        use std::io::{Read, Write};
        self.revalidate().map_err(OriginalError::Unavailable)?;
        let mut input = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| OriginalError::Unavailable(e.to_string()))?;
        if !input
            .metadata()
            .map_err(|e| OriginalError::Unavailable(e.to_string()))?
            .is_file()
        {
            return Err(OriginalError::Unavailable(
                "The original attachment is not a regular file".into(),
            ));
        }
        let staging = self
            .directory
            .join(format!("attachment-{}.pending", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&staging)
                .map_err(|e| OriginalError::Unavailable(e.to_string()))?;
            let mut hash = Sha256::new();
            let mut bytes = 0u64;
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let count = input
                    .read(&mut chunk)
                    .map_err(|e| OriginalError::Unavailable(e.to_string()))?;
                if count == 0 {
                    break;
                }
                bytes = bytes.saturating_add(count as u64);
                if bytes > 32 * 1024 * 1024 {
                    return Err(OriginalError::TooLarge);
                }
                hash.update(&chunk[..count]);
                output
                    .write_all(&chunk[..count])
                    .map_err(|e| OriginalError::Unavailable(e.to_string()))?;
            }
            output
                .sync_all()
                .map_err(|e| OriginalError::Unavailable(e.to_string()))?;
            let sha256: [u8; 32] = hash.finalize().into();
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .filter(|value| {
                    !value.is_empty()
                        && value.len() <= 16
                        && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
                });
            let retained = self.directory.join(format!(
                "attachment-{:x}{}",
                Sha256::digest(sha256),
                extension
                    .map(|value| format!(".{value}"))
                    .unwrap_or_default()
            ));
            match fs::hard_link(&staging, &retained) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    crate::secure_file::open_existing_private_file(&retained)
                        .map_err(|e| OriginalError::Unavailable(e.to_string()))?;
                }
                Err(error) => return Err(OriginalError::Unavailable(error.to_string())),
            }
            fs::File::open(&self.directory)
                .and_then(|file| file.sync_all())
                .map_err(|e| OriginalError::Unavailable(e.to_string()))?;
            Ok(OriginalAttachment {
                original_path: path.to_owned(),
                path: retained,
                bytes,
                sha256,
            })
        })();
        let _ = fs::remove_file(staging);
        result
    }
    /// Persist only after a provider acknowledgment binds this exact original
    /// command to its real user item. Conflicting rewrites are refused.
    pub(crate) fn accept(
        &mut self,
        original: &OriginalPrompt,
        text: &str,
        aliases: &[String],
    ) -> Result<(), OriginalError> {
        self.revalidate().map_err(OriginalError::Unavailable)?;
        if !valid_id(&original.user_id)
            || !valid_id(&original.turn_id)
            || aliases.len() > 128
            || aliases.iter().any(|id| !valid_id(id))
        {
            return Err(OriginalError::TooLarge);
        }
        if !original.settings.is_exact() {
            return Err(OriginalError::Unavailable(
                "The accepted provider settings are unknown".into(),
            ));
        }
        if text.len() != original.text_bytes
            || text.len() > ORIGINAL_TEXT_BYTES
            || original.attachments.len() > 32
            || original
                .attachments
                .iter()
                .try_fold(0u64, |sum, file| sum.checked_add(file.bytes))
                .is_none_or(|bytes| bytes > 64 * 1024 * 1024)
        {
            return Err(OriginalError::TooLarge);
        }
        let metadata =
            postcard::to_stdvec(original).map_err(|e| OriginalError::Unavailable(e.to_string()))?;
        if metadata.len() > META_BYTES {
            return Err(OriginalError::TooLarge);
        }
        let mut hash = Sha256::new();
        hash.update(&metadata);
        hash.update(text.as_bytes());
        let digest = hash.finalize();
        let transaction = self.db.transaction().map_err(storage_error)?;
        let old: Option<Vec<u8>> = transaction
            .query_row(
                "SELECT digest FROM prompts WHERE user_id=?1",
                [&original.user_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        if old.as_deref().is_some_and(|old| old != digest.as_slice()) {
            return Err(OriginalError::Changed);
        }
        if old.is_none() {
            transaction
                .execute(
                    "INSERT INTO prompts VALUES(?1,?2,?3)",
                    params![original.user_id, metadata, digest.as_slice()],
                )
                .map_err(storage_error)?;
            for (index, chunk) in text.as_bytes().chunks(CHUNK).enumerate() {
                transaction
                    .execute(
                        "INSERT INTO prompt_text VALUES(?1,?2,?3)",
                        params![original.user_id, index * CHUNK, chunk],
                    )
                    .map_err(storage_error)?;
            }
        }
        for alias in std::iter::once(&original.user_id).chain(aliases) {
            let existing: Option<String> = transaction
                .query_row("SELECT user_id FROM aliases WHERE alias=?1", [alias], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(storage_error)?;
            if existing.as_deref().is_some_and(|id| id != original.user_id) {
                return Err(OriginalError::Changed);
            }
            transaction
                .execute(
                    "INSERT OR IGNORE INTO aliases VALUES(?1,?2)",
                    params![alias, original.user_id],
                )
                .map_err(storage_error)?;
        }
        transaction.commit().map_err(storage_error)
    }
    /// Called only for the newest live confirmed user, in conversation order.
    /// An unannotated newest user intentionally makes latest lookup unavailable.
    pub(crate) fn latest_user(&mut self, user_id: &str) -> Result<(), String> {
        self.revalidate()?;
        if !valid_id(user_id) {
            return Err("Invalid user identity".into());
        }
        self.db.execute("INSERT INTO latest VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET user_id=excluded.user_id",[user_id]).map(|_|()).map_err(|e|e.to_string())
    }
    pub(crate) fn original(
        &self,
        user_id: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<OriginalPage, OriginalError> {
        self.revalidate().map_err(OriginalError::Unavailable)?;
        if !(4..=ORIGINAL_PAGE_BYTES).contains(&limit) {
            return Err(OriginalError::InvalidRange);
        }
        let latest: Option<String> = if user_id.is_none() {
            self.db
                .query_row("SELECT user_id FROM latest WHERE singleton=1", [], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(storage_error)?
        } else {
            None
        };
        let id = user_id
            .or(latest.as_deref())
            .ok_or_else(|| OriginalError::Unavailable("No confirmed latest user message".into()))?;
        let found:Option<(String,Vec<u8>)>=self.db.query_row("SELECT prompts.user_id,metadata FROM prompts JOIN aliases ON aliases.user_id=prompts.user_id WHERE aliases.alias=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(storage_error)?;
        let (id, metadata) = found.ok_or_else(|| {
            OriginalError::Unavailable("The exact original accepted command is unavailable".into())
        })?;
        let original: OriginalPrompt = postcard::from_bytes(&metadata)
            .map_err(|e| OriginalError::Unavailable(e.to_string()))?;
        if offset > original.text_bytes {
            return Err(OriginalError::InvalidRange);
        }
        let end = offset.saturating_add(limit).min(original.text_bytes);
        let mut statement=self.db.prepare("SELECT position,body FROM prompt_text WHERE user_id=?1 AND position+length(body)>?2 AND position<?3 ORDER BY position").map_err(storage_error)?;
        let rows = statement
            .query_map(params![id, offset, end], |r| {
                Ok((r.get::<_, usize>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(storage_error)?;
        let mut bytes = Vec::with_capacity(limit);
        for row in rows {
            let (position, body) = row.map_err(storage_error)?;
            let from = offset.saturating_sub(position);
            let to = (end - position).min(body.len());
            bytes.extend_from_slice(&body[from..to]);
        }
        if bytes.first().is_some_and(|byte| byte & 0xc0 == 0x80) {
            return Err(OriginalError::InvalidRange);
        }
        while std::str::from_utf8(&bytes).is_err() {
            if bytes.pop().is_none() {
                return Err(OriginalError::InvalidRange);
            }
        }
        let text = String::from_utf8(bytes).map_err(|_| OriginalError::InvalidRange)?;
        let next_offset = offset + text.len();
        let complete = next_offset == original.text_bytes;
        self.revalidate().map_err(OriginalError::Unavailable)?;
        Ok(OriginalPage {
            original,
            offset,
            next_offset,
            text,
            complete,
        })
    }
    /// Repeated delivery of an already seen Awaiting request is replay and
    /// cannot reopen a denied action. Only a distinct request starts a decision.
    pub(crate) fn approval(
        &mut self,
        key: &ApprovalKey,
        request: &str,
        decision: ApprovalDecision,
    ) -> Result<(), String> {
        self.revalidate()?;
        let agent = key.agent_id.as_deref().unwrap_or("");
        if !valid_id(&key.item_id) || !valid_id(request) || agent.len() > ID_BYTES {
            return Err("Approval identity exceeds its limit".into());
        }
        let transaction = self.db.transaction().map_err(|e| e.to_string())?;
        let known: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM approvals WHERE agent=?1 AND item=?2 AND request=?3)",
                params![agent, key.item_id, request],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if decision == ApprovalDecision::Awaiting {
            if !known {
                transaction
                    .execute(
                        "INSERT INTO approvals VALUES(?1,?2,?3,0)",
                        params![agent, key.item_id, request],
                    )
                    .map_err(|e| e.to_string())?;
                transaction.execute("INSERT INTO current_approval VALUES(?1,?2,?3) ON CONFLICT(agent,item) DO UPDATE SET request=excluded.request",params![agent,key.item_id,request]).map_err(|e|e.to_string())?;
            }
        } else {
            transaction.execute("UPDATE approvals SET decision=?4 WHERE agent=?1 AND item=?2 AND request=?3 AND decision=0 AND EXISTS(SELECT 1 FROM current_approval WHERE agent=?1 AND item=?2 AND request=?3)",params![agent,key.item_id,request,if decision==ApprovalDecision::Denied{2}else{1}]).map_err(|e|e.to_string())?;
        }
        transaction.commit().map_err(|e| e.to_string())
    }
    pub(crate) fn denied(&self, key: &ApprovalKey) -> Result<bool, String> {
        self.revalidate()?;
        self.db.query_row("SELECT EXISTS(SELECT 1 FROM current_approval c JOIN approvals a USING(agent,item,request) WHERE c.agent=?1 AND c.item=?2 AND a.decision=2)",params![key.agent_id.as_deref().unwrap_or(""),key.item_id],|r|r.get(0)).map_err(|e|e.to_string())
    }
}
fn storage_error(error: rusqlite::Error) -> OriginalError {
    OriginalError::Unavailable(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, os::unix::fs::PermissionsExt};
    fn scope(root: &Path, provider: crate::agent::Provider) -> LaunchAccountScope {
        fs::create_dir_all(root.join("home/.claude")).unwrap();
        fs::create_dir_all(root.join("home/.codex")).unwrap();
        LaunchAccountScope::resolve(provider, root, |name| {
            (name == "HOME").then(|| OsString::from(root.join("home")))
        })
        .unwrap()
    }
    fn prompt(id: &str, text: &str) -> OriginalPrompt {
        OriginalPrompt {
            user_id: id.into(),
            turn_id: format!("turn-{id}"),
            text_bytes: text.len(),
            attachments: vec![],
            settings: OriginalSettings {
                model: Setting::Value("confirmed-model".into()),
                reasoning: Setting::Value("low".into()),
                fast: Setting::Unsupported,
                permission: Setting::Value("ask".into()),
            },
        }
    }
    #[test]
    fn originals_are_immutable_paged_exact_and_isolated_across_cold_open() {
        let root = tempfile::tempdir().unwrap();
        let account = scope(root.path(), crate::agent::Provider::Claude);
        let text = "original /operator-command 🐙".repeat(20000);
        let original = prompt("provider-user-a", &text);
        {
            let mut store = Store::at(
                &root.path().join("annotations"),
                &account,
                "confirmed-session",
            )
            .unwrap();
            store
                .accept(&original, &text, &["provider-user-a:input/0".into()])
                .unwrap();
            store.latest_user(&original.user_id).unwrap();
            let mut modified = original.clone();
            modified.settings.model = Setting::Value("later-model".into());
            assert_eq!(
                store.accept(&modified, &text, &[]),
                Err(OriginalError::Changed)
            );
        }
        let mut store = Store::at(
            &root.path().join("annotations"),
            &account,
            "confirmed-session",
        )
        .unwrap();
        let mut collected = String::new();
        let mut offset = 0;
        loop {
            let page = store.original(None, offset, 4097).unwrap();
            assert_eq!(page.original, original);
            assert_eq!(page.offset, offset);
            offset = page.next_offset;
            collected.push_str(&page.text);
            if page.complete {
                break;
            }
        }
        assert_eq!(collected, text);
        assert_eq!(
            store
                .original(Some("provider-user-a:input/0"), 0, 4)
                .unwrap()
                .original,
            original
        );
        let other = Store::at(
            &root.path().join("annotations"),
            &account,
            "different-session",
        )
        .unwrap();
        assert!(matches!(
            other.original(Some(&original.user_id), 0, 4096),
            Err(OriginalError::Unavailable(_))
        ));
        let other_account = scope(root.path(), crate::agent::Provider::Codex);
        let other = Store::at(
            &root.path().join("annotations"),
            &other_account,
            "confirmed-session",
        )
        .unwrap();
        assert!(matches!(
            other.original(Some(&original.user_id), 0, 4096),
            Err(OriginalError::Unavailable(_))
        ));
        store
            .latest_user("newest-unannotated-provider-user")
            .unwrap();
        assert!(
            matches!(
                store.original(None, 0, 4096),
                Err(OriginalError::Unavailable(_))
            ),
            "must not fall back to an older recorded prompt"
        );
        let path = root.path().join("annotations").join(account_key(&account));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    #[test]
    fn denied_survives_more_than_256_actions_cold_reopen_and_stale_replay() {
        let root = tempfile::tempdir().unwrap();
        let account = scope(root.path(), crate::agent::Provider::Codex);
        let key = ApprovalKey {
            agent_id: None,
            item_id: "turn:tool".into(),
        };
        {
            let mut store = Store::at(
                &root.path().join("annotations"),
                &account,
                "confirmed-session",
            )
            .unwrap();
            for n in 0..400 {
                let key = ApprovalKey {
                    agent_id: None,
                    item_id: format!("turn:tool-{n}"),
                };
                let request = format!("approval-{n}");
                store
                    .approval(&key, &request, ApprovalDecision::Awaiting)
                    .unwrap();
                store
                    .approval(&key, &request, ApprovalDecision::Denied)
                    .unwrap();
            }
            store
                .approval(&key, "original-request", ApprovalDecision::Awaiting)
                .unwrap();
            store
                .approval(&key, "original-request", ApprovalDecision::Denied)
                .unwrap();
        }
        let mut store = Store::at(
            &root.path().join("annotations"),
            &account,
            "confirmed-session",
        )
        .unwrap();
        assert!(
            store
                .denied(&ApprovalKey {
                    agent_id: None,
                    item_id: "turn:tool-0".into()
                })
                .unwrap()
        );
        assert!(store.denied(&key).unwrap());
        store
            .approval(&key, "original-request", ApprovalDecision::Awaiting)
            .unwrap();
        store
            .approval(&key, "original-request", ApprovalDecision::Allowed)
            .unwrap();
        assert!(
            store.denied(&key).unwrap(),
            "replayed resolved request cannot reset a denial"
        );
        store
            .approval(&key, "genuinely-new-request", ApprovalDecision::Awaiting)
            .unwrap();
        assert!(!store.denied(&key).unwrap());
        store
            .approval(&key, "genuinely-new-request", ApprovalDecision::Allowed)
            .unwrap();
        assert!(
            !store.denied(&key).unwrap(),
            "later generic failure is free to remain Failed"
        );
        store
            .approval(&key, "original-request", ApprovalDecision::Awaiting)
            .unwrap();
        assert!(
            !store.denied(&key).unwrap(),
            "old request replay cannot become current"
        );
        assert!(
            !store
                .denied(&ApprovalKey {
                    agent_id: Some("child".into()),
                    item_id: key.item_id.clone()
                })
                .unwrap()
        );
    }
    #[test]
    fn unknown_settings_replacement_and_explicit_account_purge_are_checked() {
        let root = tempfile::tempdir().unwrap();
        let account = scope(root.path(), crate::agent::Provider::Claude);
        let annotations = root.path().join("annotations");
        let mut store = Store::at(&annotations, &account, "session").unwrap();
        let mut original = prompt("user", "text");
        original.settings.fast = Setting::Unknown;
        assert!(matches!(
            store.accept(&original, "text", &[]),
            Err(OriginalError::Unavailable(_))
        ));
        original.settings.fast = Setting::Unsupported;
        store.accept(&original, "text", &[]).unwrap();
        fs::rename(&account.canonical_home, root.path().join("old-home")).unwrap();
        fs::create_dir(&account.canonical_home).unwrap();
        assert!(matches!(
            store.original(Some("user"), 0, 4),
            Err(OriginalError::Unavailable(_))
        ));
        assert!(
            purge_account_at(&annotations, &account).is_err(),
            "replacement cannot authorize deleting another physical home's annotations"
        );
        fs::remove_dir(&account.canonical_home).unwrap();
        fs::rename(root.path().join("old-home"), &account.canonical_home).unwrap();
        drop(store);
        purge_account_at(&annotations, &account).unwrap();
        assert!(
            Store::at(&annotations, &account, "session").is_err(),
            "a purged physical namespace must not be recreated"
        );
    }
    #[test]
    fn attachment_snapshot_keeps_original_bytes_and_registered_purge_works_after_home_loss() {
        let root = tempfile::tempdir().unwrap();
        let mut account = scope(root.path(), crate::agent::Provider::Claude);
        account.registered_id = Some(format!("claude/{}", uuid::Uuid::new_v4()).parse().unwrap());
        let annotations = root.path().join("annotations");
        let store = Store::at(&annotations, &account, "session").unwrap();
        let path = root.path().join("picked.png");
        fs::write(&path, b"original image bytes").unwrap();
        let attachment = store.retain_attachment(&path).unwrap();
        fs::write(&path, b"changed later").unwrap();
        assert_eq!(attachment.original_path, path);
        assert_eq!(fs::read(&attachment.path).unwrap(), b"original image bytes");
        assert_eq!(
            attachment.sha256,
            <[u8; 32]>::from(Sha256::digest(b"original image bytes"))
        );
        assert_eq!(
            fs::metadata(attachment.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(store);
        fs::remove_dir(&account.canonical_home).unwrap();
        assert_eq!(
            purge_registered_at(&annotations, account.registered_id.as_ref().unwrap()).unwrap(),
            Some(1)
        );
        assert_eq!(
            purge_registered_at(&annotations, account.registered_id.as_ref().unwrap()).unwrap(),
            Some(0)
        );
        let unknown = format!("claude/{}", uuid::Uuid::new_v4()).parse().unwrap();
        assert_eq!(purge_registered_at(&annotations, &unknown).unwrap(), None);
    }

    #[test]
    fn purge_receipt_matches_physical_aliases_and_repeated_absent_namespaces() {
        let root = tempfile::tempdir().unwrap();
        let mut account = scope(root.path(), crate::agent::Provider::Claude);
        let first: crate::account::AccountId =
            format!("claude/{}", uuid::Uuid::new_v4()).parse().unwrap();
        let second: crate::account::AccountId =
            format!("claude/{}", uuid::Uuid::new_v4()).parse().unwrap();
        account.registered_id = None;
        account.purge_ids = vec![first.clone(), second.clone()];
        let annotations = root.path().join("annotations");
        let mut store = Store::at(&annotations, &account, "session").unwrap();
        let lineage = store.original_lineage();
        assert_eq!(lineage.as_str(), account_key(&account));
        store.accept(&prompt("user", "text"), "text", &[]).unwrap();
        fs::remove_dir_all(&account.canonical_home).unwrap();

        let receipt = purge_registered_receipt_at(&annotations, &second)
            .unwrap()
            .unwrap();
        assert_eq!(receipt.revoked, vec![lineage.clone()]);
        assert_eq!(receipt.removed_namespaces, 1);
        let repeated = purge_registered_receipt_at(&annotations, &first)
            .unwrap()
            .unwrap();
        assert_eq!(
            repeated.revoked, receipt.revoked,
            "every alias invalidates the same UI lineage even after files disappeared"
        );
        assert_eq!(repeated.removed_namespaces, 0);
        assert!(matches!(
            store.original(Some("user"), 0, 4),
            Err(OriginalError::Unavailable(_))
        ));

        let unknown = format!("claude/{}", uuid::Uuid::new_v4()).parse().unwrap();
        assert_eq!(
            purge_registered_receipt_at(&annotations, &unknown).unwrap(),
            None
        );
    }

    #[test]
    fn purge_receipt_survives_cleanup_failure_and_preserves_unrelated_originals() {
        let root = tempfile::tempdir().unwrap();
        let account = scope(root.path(), crate::agent::Provider::Claude);
        let other = scope(root.path(), crate::agent::Provider::Codex);
        let annotations = root.path().join("annotations");
        let mut store = Store::at(&annotations, &account, "session").unwrap();
        let mut other_store = Store::at(&annotations, &other, "session").unwrap();
        let original = prompt("user", "text");
        store.accept(&original, "text", &[]).unwrap();
        other_store.accept(&original, "text", &[]).unwrap();
        let lineage = store.original_lineage();
        let path = annotations.join(account_key(&account));
        let retained = annotations.join("retained-open-namespace");
        fs::rename(&path, &retained).unwrap();
        fs::write(&path, b"not a directory").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let failure = purge_account_receipt_at(&annotations, &account).unwrap_err();
        assert!(!failure.message.is_empty());
        assert_eq!(failure.receipt.revoked, vec![lineage.clone()]);
        assert_eq!(failure.receipt.removed_namespaces, 0);
        // Restoring the open store's directory does not restore its authority.
        fs::remove_file(&path).unwrap();
        fs::rename(&retained, &path).unwrap();
        assert!(matches!(
            store.original(Some("user"), 0, 4),
            Err(OriginalError::Unavailable(_))
        ));
        assert!(other_store.original(Some("user"), 0, 4).is_ok());
        assert_ne!(other_store.original_lineage(), lineage);
        assert_eq!(
            purge_account_receipt_at(&annotations, &account)
                .unwrap()
                .revoked,
            failure.receipt.revoked
        );
    }

    #[test]
    fn refused_replaced_scope_returns_no_revocation_and_preserves_originals() {
        let root = tempfile::tempdir().unwrap();
        let account = scope(root.path(), crate::agent::Provider::Claude);
        let annotations = root.path().join("annotations");
        let mut store = Store::at(&annotations, &account, "session").unwrap();
        store.accept(&prompt("user", "text"), "text", &[]).unwrap();
        let old_home = root.path().join("captured-home");
        fs::rename(&account.canonical_home, &old_home).unwrap();
        fs::create_dir(&account.canonical_home).unwrap();
        let failure = purge_account_receipt_at(&annotations, &account).unwrap_err();
        assert_eq!(failure.receipt, PurgeReceipt::default());
        fs::remove_dir(&account.canonical_home).unwrap();
        fs::rename(&old_home, &account.canonical_home).unwrap();
        assert!(store.original(Some("user"), 0, 4).is_ok());
        assert!(
            !annotations
                .join("revoked-accounts")
                .join(account_key(&account))
                .exists()
        );
    }

    #[test]
    fn unsafe_existing_marker_is_receipted_when_revocation_open_fails() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let account = scope(root.path(), crate::agent::Provider::Claude);
        let annotations = root.path().join("annotations");
        let mut store = Store::at(&annotations, &account, "session").unwrap();
        store.accept(&prompt("user", "text"), "text", &[]).unwrap();
        let markers = annotations.join("revoked-accounts");
        crate::secure_file::prepare_private_directory(&markers).unwrap();
        let target = root.path().join("untouched-target");
        fs::write(&target, b"unchanged").unwrap();
        symlink(&target, markers.join(account_key(&account))).unwrap();
        let failure = purge_account_receipt_at(&annotations, &account).unwrap_err();
        assert_eq!(failure.receipt.revoked, vec![store.original_lineage()]);
        assert_eq!(failure.receipt.removed_namespaces, 0);
        assert!(!failure.message.is_empty());
        assert!(annotations.join(account_key(&account)).exists());
        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
        assert!(matches!(
            store.original(Some("user"), 0, 4),
            Err(OriginalError::Unavailable(_))
        ));
    }

    #[test]
    fn pre_marker_preparation_failure_does_not_claim_revocation() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let account = scope(root.path(), crate::agent::Provider::Claude);
        let annotations = root.path().join("annotations");
        let mut store = Store::at(&annotations, &account, "session").unwrap();
        store.accept(&prompt("user", "text"), "text", &[]).unwrap();
        // Directory preparation refuses this dangling symlink, but the exact
        // marker lookup is NotFound, so existing originals remain readable.
        let absent = root.path().join("absent-marker-directory");
        symlink(&absent, annotations.join("revoked-accounts")).unwrap();
        let failure = purge_account_receipt_at(&annotations, &account).unwrap_err();
        assert_eq!(failure.receipt, PurgeReceipt::default());
        assert!(!absent.exists());
        assert!(store.original(Some("user"), 0, 4).is_ok());
    }
}
