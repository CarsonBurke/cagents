//! The conversations an Agent Client Protocol agent keeps, as it lists them
//! with `session/list`, and whether something holds one of them now.
//!
//! Listing starts the agent's ACP server for a moment, as the account probe
//! does, and asks for nothing but the list: no session is opened, so nothing
//! is added to the conversations it lists. Antigravity 1.5.13 answers
//! `{"sessions": [{"sessionId", "cwd", "title", "updatedAt"}]}` in one page,
//! unordered; the protocol allows more pages through `nextCursor`.
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};

use super::{Session, command_with_path, find_executable};
use crate::{account::Account, agent::Provider};

/// ACP's `auth_required` error code.
const AUTH_REQUIRED: i64 = -32000;
const METHOD_NOT_FOUND: i64 = -32601;

/// How long a listing may take, every page included.
const TIMEOUT: Duration = Duration::from_secs(20);
/// Pages asked for at most; an agent that keeps offering more is cut off.
const MAX_PAGES: usize = 64;
/// Conversations kept from one listing at most.
pub const MAX_CONVERSATIONS: usize = 5_000;
const MAX_ID: usize = 256;
/// Titles longer than this are cut before anything else reads them.
const MAX_TITLE_BYTES: usize = 4096;
/// Session directories looked at when finding a Muse conversation's lock.
const MUSE_ENTRY_LIMIT: usize = 100_000;

/// One conversation as the agent lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub id: String,
    /// The directory the conversation works in.
    pub directory: PathBuf,
    pub title: Option<String>,
    pub updated_at_unix_ms: Option<i64>,
}

/// Why an agent's conversations could not be listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unlisted {
    /// It needs its sign-in first.
    SignedOut,
    /// It failed, stopped or did not answer in time. What went wrong is
    /// logged, not shown.
    Failed,
}

/// Every conversation `account`'s agent lists, newest first. `Ok(None)` when
/// it is not installed or does not list its conversations.
pub async fn list(
    account: &Account,
    search_path: Option<&OsStr>,
) -> Result<Option<Vec<Listed>>, Unlisted> {
    let provider = account.provider;
    let label = super::cli_name(provider);
    let arguments: &[&str] = match provider {
        // Telemetry stays off, as in every Antigravity process cagents runs.
        Provider::Antigravity => &["acp", "--disable-telemetry"],
        // So does contributor mode, as in Muse's native chat.
        Provider::Muse => &["acp", "--contributor-mode", "off"],
        Provider::Claude | Provider::Codex | Provider::Omp => return Ok(None),
    };
    let Some(program) = find_executable(provider.executable(), search_path) else {
        return Ok(None);
    };
    // Muse's own executable is a wrapper; its upstream one serves ACP.
    let program = match program.to_str() {
        Some(found) => PathBuf::from(provider.subcommand_program(
            found,
            Path::new("/"),
            search_path.and_then(OsStr::to_str).unwrap_or_default(),
        )),
        None => program,
    };
    let mut command = command_with_path(account, &program, search_path);
    command.args(arguments);
    let listed = tokio::time::timeout(TIMEOUT, async {
        let mut session = Session::spawn(command).await.map_err(Asked::Failed)?;
        let listed = ask(&mut session).await;
        // The answer is in; the agent's exit is not waited on for it.
        tokio::spawn(session.close());
        listed
    })
    .await
    .unwrap_or_else(|_| Err(Asked::Failed("it did not answer in time".into())));
    listed.map_err(|asked| match asked {
        Asked::SignedOut => Unlisted::SignedOut,
        Asked::Failed(error) => {
            tracing::info!(%error, "{label} could not list its conversations");
            Unlisted::Failed
        }
    })
}

/// How asking went wrong.
enum Asked {
    SignedOut,
    Failed(String),
}

impl From<String> for Asked {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

/// `initialize`, then `session/list` page by page.
async fn ask(session: &mut Session) -> Result<Option<Vec<Listed>>, Asked> {
    session
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
            "clientInfo": {"name": "cagents", "title": "cagents", "version": env!("CARGO_PKG_VERSION")},
        }}))
        .await?;
    let initialized = reply(session, 1).await?.map_err(|error| refusal(&error))?;
    if !initialized["agentCapabilities"]["sessionCapabilities"]["list"].is_object() {
        return Ok(None);
    }
    let mut listed = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor: Option<String> = None;
    for page in 0..MAX_PAGES {
        let id = 2 + page as u64;
        let params = match &cursor {
            Some(cursor) => json!({"cursor": cursor}),
            None => json!({}),
        };
        session
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": "session/list", "params": params}))
            .await?;
        let result = match reply(session, id).await? {
            Ok(result) => result,
            Err(error) if error["code"].as_i64() == Some(METHOD_NOT_FOUND) => {
                return Ok(None);
            }
            Err(error) => return Err(refusal(&error)),
        };
        for entry in result["sessions"].as_array().into_iter().flatten() {
            if listed.len() >= MAX_CONVERSATIONS {
                break;
            }
            if let Some(conversation) = conversation(entry)
                && seen.insert(conversation.id.clone())
            {
                listed.push(conversation);
            }
        }
        match result["nextCursor"].as_str() {
            Some(next)
                if !next.is_empty()
                    && cursor.as_deref() != Some(next)
                    && listed.len() < MAX_CONVERSATIONS =>
            {
                cursor = Some(next.to_owned());
            }
            _ => break,
        }
    }
    listed.sort_by_key(|conversation| std::cmp::Reverse(conversation.updated_at_unix_ms));
    Ok(Some(listed))
}

/// One listed conversation, or `None` for an entry cagents could not
/// resume: one without an identifier it can name or an absolute directory.
fn conversation(entry: &Value) -> Option<Listed> {
    let id = entry["sessionId"].as_str()?;
    if id.is_empty() || id.len() > MAX_ID || id.contains(char::is_control) {
        return None;
    }
    let directory = PathBuf::from(entry["cwd"].as_str()?);
    if !directory.is_absolute() {
        return None;
    }
    let title = entry["title"]
        .as_str()
        .map(|title| title[..title.floor_char_boundary(title.len().min(MAX_TITLE_BYTES))].trim())
        .filter(|title| !title.is_empty())
        .map(str::to_owned);
    Some(Listed {
        id: id.to_owned(),
        directory,
        title,
        updated_at_unix_ms: entry["updatedAt"]
            .as_str()
            .and_then(crate::account::parse_rfc3339)
            .and_then(|seconds| seconds.checked_mul(1000)),
    })
}

fn refusal(error: &Value) -> Asked {
    if error["code"].as_i64() == Some(AUTH_REQUIRED) {
        return Asked::SignedOut;
    }
    Asked::Failed(
        error["message"]
            .as_str()
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .map_or_else(
                || "it refused".into(),
                |message| message[..message.floor_char_boundary(message.len().min(240))].to_owned(),
            ),
    )
}

/// The reply to request `id`, refusing any request of the agent's own and
/// passing over its notifications.
async fn reply(session: &mut Session, id: u64) -> Result<Result<Value, Value>, String> {
    loop {
        let frame = session.next().await?;
        if frame.get("method").is_some() {
            if let Some(request) = frame.get("id") {
                session
                    .send(&json!({"jsonrpc": "2.0", "id": request, "error": {
                        "code": METHOD_NOT_FOUND, "message": "cagents is only listing conversations",
                    }}))
                    .await?;
            }
            continue;
        }
        if frame["id"].as_u64() != Some(id) {
            continue;
        }
        return Ok(match frame.get("error") {
            Some(error) => Err(error.clone()),
            None => Ok(frame["result"].clone()),
        });
    }
}

/// Which of `ids`, conversations `provider` keeps under `root`, a running
/// process holds now, by the lock the agent takes on a conversation while
/// it runs it:
///
/// - Antigravity: `presence/ID.lock`, which stays behind after it exits.
/// - Muse: `sessions/YEAR/MONTH/DAY/ID/.session.lock` (not yet checked
///   against a running Muse).
///
/// A conversation whose lock is missing or not held is not held.
pub fn held<'a>(
    provider: Provider,
    root: &Path,
    ids: impl IntoIterator<Item = &'a str>,
) -> HashSet<String> {
    let locked = locked_inodes(&std::fs::read_to_string("/proc/locks").unwrap_or_default());
    if locked.is_empty() {
        return HashSet::new();
    }
    let ids = ids
        .into_iter()
        .filter(|id| {
            !id.is_empty()
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .collect::<HashSet<_>>();
    let locks: Vec<(String, PathBuf)> = match provider {
        Provider::Antigravity => ids
            .iter()
            .map(|id| {
                (
                    (*id).to_owned(),
                    root.join("presence").join(format!("{id}.lock")),
                )
            })
            .collect(),
        Provider::Muse => muse_session_locks(&root.join("sessions"), &ids),
        Provider::Claude | Provider::Codex | Provider::Omp => Vec::new(),
    };
    use std::os::unix::fs::MetadataExt;
    locks
        .into_iter()
        .filter(|(_, lock)| {
            let Ok(metadata) = std::fs::symlink_metadata(lock) else {
                return false;
            };
            // The lock table names a file by its inode alone, which another
            // file system can share, so a holder must have this file open.
            metadata.is_file()
                && locked.get(&metadata.ino()).is_some_and(|holders| {
                    holders
                        .iter()
                        .any(|&pid| holds_open(pid, (metadata.dev(), metadata.ino())))
                })
        })
        .map(|(id, _)| id)
        .collect()
}

/// Whether process `pid` has the file `(device, inode)` open.
fn holds_open(pid: u32, file: (u64, u64)) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(descriptors) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    descriptors.flatten().any(|descriptor| {
        std::fs::metadata(descriptor.path())
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == file)
    })
}

/// The `.session.lock` of each of `ids` Muse keeps under `sessions`, by its
/// date directories.
fn muse_session_locks(sessions: &Path, ids: &HashSet<&str>) -> Vec<(String, PathBuf)> {
    let mut budget = MUSE_ENTRY_LIMIT;
    let mut found = Vec::new();
    let mut directories = vec![(sessions.to_path_buf(), 0)];
    while let Some((directory, depth)) = directories.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if budget == 0 {
                return found;
            }
            budget -= 1;
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let path = entry.path();
            if depth < 3 {
                directories.push((path, depth + 1));
            } else if let Some(id) = entry.file_name().to_str()
                && ids.contains(id)
            {
                found.push((id.to_owned(), path.join(".session.lock")));
            }
        }
    }
    found
}

/// The inode of every file `/proc/locks` says is locked, whatever kind of
/// lock, by its `MAJOR:MINOR:INODE` field, with the processes holding it,
/// by the field before. The device is not compared: a Btrfs subvolume's
/// files report another device than the lock table does.
fn locked_inodes(table: &str) -> HashMap<u64, Vec<u32>> {
    let mut locked = HashMap::<u64, Vec<u32>>::new();
    for line in table.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let Some((at, inode)) = fields.iter().enumerate().find_map(|(at, field)| {
            let mut parts = field.split(':');
            let (major, minor, inode) = (parts.next()?, parts.next()?, parts.next()?);
            (parts.next().is_none()
                && [major, minor]
                    .iter()
                    .all(|part| !part.is_empty() && u32::from_str_radix(part, 16).is_ok()))
            .then(|| inode.parse::<u64>().ok().map(|inode| (at, inode)))
            .flatten()
        }) else {
            continue;
        };
        let holders = locked.entry(inode).or_default();
        // An open file description's lock names no process (-1).
        if let Some(pid) = at
            .checked_sub(1)
            .and_then(|before| fields[before].parse::<u32>().ok())
        {
            holders.push(pid);
        }
    }
    locked
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of executables: the offline ACP agent installed as
    /// `name`, keeping its conversations in `state` and answering as
    /// `environment` says.
    fn installed(name: &str, state: &Path, environment: &[(&str, &str)]) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let bin = tempfile::tempdir().unwrap();
        let agent = bin.path().join("fake-acp-agent.py");
        std::fs::write(
            &agent,
            include_str!("../../../tests/fixtures/fake-acp-agent.py"),
        )
        .unwrap();
        let mut script = format!("#!/bin/sh\nexport FAKE_ACP_STATE='{}'\n", state.display());
        for (name, value) in environment {
            script.push_str(&format!("export {name}='{value}'\n"));
        }
        // By the interpreter its own first line names: the agent runs with
        // only `bin` on its search path.
        script.push_str(&format!(
            "exec /usr/bin/python3 '{}' \"$@\"\n",
            agent.display()
        ));
        let program = bin.path().join(name);
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        bin
    }

    fn remember(state: &Path, conversations: serde_json::Value) {
        std::fs::create_dir_all(state).unwrap();
        std::fs::write(state.join("index.json"), conversations.to_string()).unwrap();
    }

    /// Every page the agent offers is read, newest first, and an entry
    /// cagents could not resume is left out.
    #[tokio::test]
    async fn an_agent_lists_every_page_of_its_conversations() {
        let state = tempfile::tempdir().unwrap();
        remember(
            state.path(),
            json!({
                "older": {"cwd": "/work/api", "title": "Older", "updatedAt": "2026-11-17T09:00:00Z"},
                "newest": {"cwd": "/work/web", "title": null, "updatedAt": "2026-11-19T09:00:00Z"},
                "middle": {"cwd": "/work/api", "title": "Middle", "updatedAt": "2026-11-18T09:00:00Z"},
                "nowhere": {"cwd": "relative", "title": "Nowhere", "updatedAt": "2026-11-16T09:00:00Z"},
            }),
        );
        let bin = installed(
            "agy",
            state.path(),
            &[("FAKE_ACP_SHAPE", "agy"), ("FAKE_ACP_LIST_PAGE", "2")],
        );
        let listed = list(
            &Account::system(Provider::Antigravity),
            Some(bin.path().as_os_str()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|conversation| (conversation.id.as_str(), conversation.title.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("newest", None),
                ("middle", Some("Middle")),
                ("older", Some("Older"))
            ]
        );
        assert_eq!(listed[0].directory, Path::new("/work/web"));
    }

    /// Muse is asked through its upstream executable, which serves ACP, and
    /// a refusal says what to do.
    #[tokio::test]
    async fn muse_is_asked_through_its_upstream_executable() {
        let state = tempfile::tempdir().unwrap();
        remember(
            state.path(),
            json!({"01a1f13f": {"cwd": "/work", "title": "cyan-wezen", "updatedAt": "2026-11-19T09:00:00Z"}}),
        );
        // The wrapper Muse installs as `muse` serves no ACP.
        use std::os::unix::fs::PermissionsExt;
        let muse = |environment: &[(&str, &str)]| {
            let bin = installed("muse-upstream", state.path(), environment);
            let wrapper = bin.path().join("muse");
            std::fs::write(&wrapper, "#!/bin/sh\nexit 3\n").unwrap();
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
            bin
        };
        let bin = muse(&[("FAKE_ACP_SHAPE", "muse")]);
        let account = Account::system(Provider::Muse);
        let listed = list(&account, Some(bin.path().as_os_str()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title.as_deref(), Some("cyan-wezen"));

        let refusing = muse(&[("FAKE_ACP_SHAPE", "muse"), ("FAKE_ACP_AUTH", "list")]);
        let error = list(&account, Some(refusing.path().as_os_str()))
            .await
            .unwrap_err();
        assert_eq!(error, Unlisted::SignedOut);

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            list(&account, Some(empty.path().as_os_str())).await,
            Ok(None)
        );
        assert_eq!(
            list(
                &Account::system(Provider::Claude),
                Some(bin.path().as_os_str())
            )
            .await,
            Ok(None)
        );
    }

    #[test]
    fn a_listed_conversation_needs_an_id_and_an_absolute_directory() {
        let listed = conversation(&json!({
            "sessionId": "61d4342c-6107-4eeb-9f69-bd7ce741ea0d",
            "cwd": "/work/api",
            "title": "  Filter Environment Variables  ",
            "updatedAt": "2026-11-19T01:11:46Z",
        }))
        .unwrap();
        assert_eq!(listed.directory, Path::new("/work/api"));
        assert_eq!(
            listed.title.as_deref(),
            Some("Filter Environment Variables")
        );
        assert_eq!(listed.updated_at_unix_ms, Some(1_795_050_706_000));
        let untitled = conversation(&json!({"sessionId": "a", "cwd": "/w", "title": " "})).unwrap();
        assert_eq!((untitled.title, untitled.updated_at_unix_ms), (None, None));
        for entry in [
            json!({"cwd": "/w"}),
            json!({"sessionId": "", "cwd": "/w"}),
            json!({"sessionId": "a\nb", "cwd": "/w"}),
            json!({"sessionId": "a", "cwd": "relative"}),
            json!({"sessionId": "a"}),
        ] {
            assert_eq!(conversation(&entry), None, "{entry}");
        }
    }

    #[test]
    fn the_lock_table_names_each_locked_inode() {
        let table = "\
266: FLOCK  ADVISORY  WRITE 2436558 00:1e:598782030 0 EOF
267: POSIX  ADVISORY  READ 12 fd:01:42 1073741826 1073742335
267: -> POSIX  ADVISORY  WRITE 13 fd:01:43 0 EOF
";
        assert_eq!(
            locked_inodes(table),
            HashMap::from([
                (598_782_030, vec![2_436_558]),
                (42, vec![12]),
                (43, vec![13])
            ])
        );
        assert!(locked_inodes("garbage 1:2 x:y:z\n").is_empty());
    }

    /// Only a conversation whose lock a process holds is held; a lock left
    /// behind, a missing one, and an identifier that would leave the
    /// directory are not.
    #[test]
    fn a_conversation_is_held_while_its_lock_is() {
        use std::os::fd::AsRawFd;
        let root = tempfile::tempdir().unwrap();
        let presence = root.path().join("presence");
        std::fs::create_dir_all(&presence).unwrap();
        let held_lock = std::fs::File::create(presence.join("running.lock")).unwrap();
        std::fs::File::create(presence.join("stopped.lock")).unwrap();
        // SAFETY: an advisory lock on a file this test owns.
        assert_eq!(
            unsafe { libc::flock(held_lock.as_raw_fd(), libc::LOCK_EX) },
            0
        );
        let ids = ["running", "stopped", "missing", "../presence/running"];
        assert_eq!(
            held(Provider::Antigravity, root.path(), ids),
            HashSet::from(["running".to_owned()])
        );
        drop(held_lock);
        assert!(held(Provider::Antigravity, root.path(), ids).is_empty());

        let session = root.path().join("sessions/2026/11/18/01a1f13f");
        std::fs::create_dir_all(&session).unwrap();
        let muse_lock = std::fs::File::create(session.join(".session.lock")).unwrap();
        // SAFETY: as above.
        assert_eq!(
            unsafe { libc::flock(muse_lock.as_raw_fd(), libc::LOCK_EX) },
            0
        );
        assert_eq!(
            held(Provider::Muse, root.path(), ["01a1f13f", "01a1f140"]),
            HashSet::from(["01a1f13f".to_owned()])
        );
        assert!(held(Provider::Claude, root.path(), ["01a1f13f"]).is_empty());
    }
}
