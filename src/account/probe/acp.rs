//! Asking an Agent Client Protocol agent how its sign-in stands, and what
//! it offers, by opening a session and asking nothing of it.
//!
//! `initialize` alone succeeds signed out, so only `session/new` tells: it
//! fails with the protocol's `auth_required` error, or answers with the
//! session's models, reasoning levels and modes. Such a session is saved by
//! the agent like any other, so a provider's probe has to keep it out of the
//! reader's conversations; Antigravity's keeps it in a data directory of its
//! own (see [`probe_antigravity`]).
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};
use tokio::process::Command;

use super::{Catalog, Identity, Observed, Probed, Session, UsageSnapshot, UsageStatus};
use crate::{
    account::Account,
    chat::{Model, acp_levels},
};

/// ACP's `auth_required` error code.
const AUTH_REQUIRED: i64 = -32000;
const METHOD_NOT_FOUND: i64 = -32601;
/// Where Antigravity keeps the probe's sessions: apart from the conversations
/// `agy` lists, beside the sign-in they share. `agy acp --app-data-dir` takes
/// only a path relative to the account's home, `~/.gemini`.
const ANTIGRAVITY_PROBE_DATA: &str = "antigravity-cli/cagents-probe";

/// What a session opened for the probe said.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Opened {
    pub signed_in: bool,
    pub session: Option<String>,
    pub catalog: Option<Catalog>,
    /// Antigravity's name for the plan the sign-in is on.
    pub tier: Option<String>,
}

/// Probes the system Antigravity sign-in.
pub(super) async fn probe_antigravity(
    account: &Account,
    program: &Path,
    search_path: Option<&OsStr>,
) -> Result<Probed, String> {
    let data = account
        .resolved_home()
        .filter(|home| home.is_absolute())
        .ok_or("Antigravity's data directory could not be found")?
        .join(ANTIGRAVITY_PROBE_DATA);
    let mut command = super::command_with_path(account, program, search_path);
    command
        .args(["acp", "--disable-telemetry", "--app-data-dir"])
        .arg(ANTIGRAVITY_PROBE_DATA);
    probe_antigravity_in(data, command).await
}

/// Probes with `command`, an `agy acp` keeping its data in `data`.
///
/// One probe of `data` runs at a time, so none removes another's session
/// while it is open. Whatever the agent saved of a probe's session is
/// removed afterwards, whether the probe finished, failed or was abandoned by
/// its timeout, and again before the next probe, for anything a probe cut
/// short at a moment no cleanup could follow: nothing in `data` is anyone's
/// conversation.
async fn probe_antigravity_in(data: PathBuf, mut command: Command) -> Result<Probed, String> {
    let workspace = data.join("workspace");
    // Waited for without blocking a thread, so a probe abandoned by its
    // timeout stops waiting as well, whatever holds the lock.
    let lock = loop {
        let (data, workspace) = (data.clone(), workspace.clone());
        let attempt = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&data)?;
            let lock =
                crate::instance::owned_lock(&data.join(PROBE_LOCK), libc::LOCK_EX | libc::LOCK_NB)?;
            forget(&data);
            std::fs::create_dir_all(&workspace)?;
            Ok::<_, std::io::Error>(lock)
        })
        .await
        .map_err(|error| error.to_string())?;
        match attempt {
            Ok(lock) => break lock,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(_) => return Err("Antigravity's probe directory could not be prepared".to_owned()),
        }
    };
    command.current_dir(&workspace);
    // Declared before the session, so it is dropped after the session's
    // process group is killed.
    let mut claim = Claim(Some((data, lock)));
    let mut session = Session::spawn(command).await?;
    let result = open(&mut session, &workspace).await;
    let result = super::after_close(result, session.close()).await;
    if let Some(claimed) = claim.0.take() {
        let _ = tokio::task::spawn_blocking(move || release(claimed)).await;
    }
    let Observed {
        value: opened,
        observed_at,
    } = result?;
    Ok(probed("Antigravity", opened, observed_at))
}

/// The lock a probe of a data directory holds there from before it starts
/// the agent until its session is forgotten.
const PROBE_LOCK: &str = ".cagents-probe.lock";

/// The session stores of a probe's data directory, in which Antigravity
/// names every entry after its session: `conversations/<id>.db`,
/// `brain/<id>/`, `annotations/<id>.pbtxt` and `implicit/<id>.pb`.
const SESSION_STORES: [&str; 4] = ["conversations", "brain", "annotations", "implicit"];

/// Removes what the agent saved of any probe's session from the probe's own
/// data directory: nothing anyone will open again. Only entries named after
/// a session are touched, and a link is removed rather than followed.
fn forget(data: &Path) {
    for store in SESSION_STORES {
        let Ok(entries) = std::fs::read_dir(data.join(store)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(stem) = name
                .to_str()
                .map(|name| name.split('.').next().unwrap_or(name))
            else {
                continue;
            };
            if stem.len() != 36 || uuid::Uuid::parse_str(stem).is_err() {
                continue;
            }
            let path = entry.path();
            let _ = match entry.file_type() {
                Ok(kind) if kind.is_dir() => std::fs::remove_dir_all(&path),
                Ok(_) => std::fs::remove_file(&path),
                Err(_) => continue,
            };
        }
    }
}

/// A probe's hold on its data directory: its sessions are forgotten, then
/// the directory is left to the next probe.
type Claimed = (PathBuf, crate::instance::OwnedLock);

fn release((data, lock): Claimed) {
    forget(&data);
    drop(lock);
}

/// Releases a probe's data directory when the probe is dropped before it
/// could: abandoned by its timeout. The removal runs on a blocking thread,
/// since a drop cannot wait for it; without a runtime, the next probe's
/// own sweep removes it.
struct Claim(Option<Claimed>);

impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(claimed) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn_blocking(move || release(claimed));
        }
    }
}

/// The account as a probe's opened session describes it.
fn probed(label: &str, opened: Opened, observed_at: u64) -> Probed {
    if !opened.signed_in {
        return Probed::state(
            UsageStatus::NeedsLogin,
            &format!("Signed out of {label}"),
            observed_at,
            None,
        );
    }
    // A working sign-in with no limits to report: the account is there to
    // use, so it is listed, and its chip says it reports no limits. An
    // `Unavailable` system account is one that isn't set up, and isn't shown.
    let mut snapshot = UsageSnapshot::state(
        UsageStatus::Ok,
        format!("{label} does not report usage limits"),
        observed_at,
    );
    snapshot.plan_display_name = opened.tier.clone();
    Probed {
        // A session opened, so the sign-in works, whoever it belongs to.
        identity: Some(Identity {
            email: None,
            organization: None,
            plan: opened.tier,
            disputed_by: None,
        }),
        snapshot,
        plan_metadata: Default::default(),
        catalog: opened.catalog,
    }
}

/// `initialize`, then `session/new` in `cwd`, then `session/close` where the
/// agent offers it.
async fn open(session: &mut Session, cwd: &Path) -> Result<Observed<Opened>, String> {
    let cwd = cwd
        .to_str()
        .ok_or("The probe's directory is not text")?
        .to_owned();
    session
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
            "clientInfo": {"name": "cagents", "title": "cagents", "version": env!("CARGO_PKG_VERSION")},
        }}))
        .await?;
    let initialized = response(session, 1).await?;
    let capabilities = match initialized.value {
        Ok(result) => result["agentCapabilities"].clone(),
        Err(error) if error["code"].as_i64() == Some(AUTH_REQUIRED) => {
            return Ok(Observed {
                value: Opened::default(),
                observed_at: initialized.observed_at,
            });
        }
        Err(_) => return Err("The agent refused to start".into()),
    };
    session
        .send(
            &json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {
                "cwd": cwd, "mcpServers": [],
            }}),
        )
        .await?;
    let created = response(session, 2).await?;
    let opened = match created.value {
        Ok(result) => opened(&result),
        Err(error) if error["code"].as_i64() == Some(AUTH_REQUIRED) => Opened::default(),
        Err(_) => return Err("The agent could not open a session".into()),
    };
    if let Some(id) = &opened.session
        && capabilities["sessionCapabilities"]["close"].is_object()
    {
        session
            .send(
                &json!({"jsonrpc": "2.0", "id": 3, "method": "session/close", "params": {
                    "sessionId": id,
                }}),
            )
            .await?;
        let _ = response(session, 3).await;
    }
    Ok(Observed {
        value: opened,
        observed_at: created.observed_at,
    })
}

/// The reply to request `id`, refusing any request of the agent's own,
/// which a probe offers nothing for, and passing over its notifications.
async fn response(
    session: &mut Session,
    id: u64,
) -> Result<Observed<Result<Value, Value>>, String> {
    loop {
        let frame = session.next().await?;
        if frame.get("method").is_some() {
            if let Some(request) = frame.get("id") {
                session
                    .send(&json!({"jsonrpc": "2.0", "id": request, "error": {
                        "code": METHOD_NOT_FOUND, "message": "cagents is only probing",
                    }}))
                    .await?;
            }
            continue;
        }
        if frame["id"].as_u64() != Some(id) {
            continue;
        }
        let observed_at = super::now_ms();
        let value = match frame.get("error") {
            Some(error) => Err(error.clone()),
            None => Ok(frame["result"].clone()),
        };
        return Ok(Observed { value, observed_at });
    }
}

/// What a `session/new` reply offers: its config options' models, each with
/// its own reasoning levels, and the defaults it opened with.
fn opened(result: &Value) -> Opened {
    let options = result["configOptions"].as_array();
    let select = |category: &str| {
        options
            .into_iter()
            .flatten()
            .find(|option| option["category"] == category && option["type"] == "select")
    };
    let values = |option: &Value| -> Vec<(String, String)> {
        option["options"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|entry| match entry["options"].as_array() {
                Some(grouped) => grouped.iter().collect::<Vec<_>>(),
                None => vec![entry],
            })
            .filter_map(|choice| {
                let value = choice["value"].as_str()?;
                Some((
                    value.to_owned(),
                    choice["name"].as_str().unwrap_or(value).to_owned(),
                ))
            })
            .take(256)
            .collect()
    };
    let reasoning = select("thought_level");
    let levels: Vec<String> = reasoning
        .map(|option| values(option).into_iter().map(|(value, _)| value).collect())
        .unwrap_or_default();
    // agy's combined identifiers, which name every model's own levels.
    let combined: Vec<String> = result["models"]["availableModels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| model["modelId"].as_str().map(str::to_owned))
        .take(256)
        .collect();
    let catalog = select("model").and_then(|option| {
        let current = option["currentValue"].as_str().unwrap_or_default();
        let offered = values(option);
        let named = acp_levels::names_levels(&combined, &levels, |model| {
            offered.iter().any(|(id, _)| id == model)
        });
        // As a conversation reads them: where the combined identifiers do
        // not name a model's levels, only the current model's are known.
        let models = offered
            .into_iter()
            .map(|(id, label)| Model {
                reasoning: if named {
                    acp_levels::joined(&id, &levels, &combined)
                } else if id == current {
                    levels.clone()
                } else {
                    Vec::new()
                },
                id,
                label,
            })
            .collect::<Vec<_>>();
        let mut catalog = super::observed_catalog(models, super::now_ms())?;
        catalog.default_model = option["currentValue"].as_str().map(str::to_owned);
        // A model runs at a level only if it offers one.
        if let (Some(model), Some(level)) = (
            catalog.default_model.clone(),
            reasoning.and_then(|option| option["currentValue"].as_str()),
        ) && catalog.models.iter().any(|offered| {
            offered.id == model && offered.reasoning.iter().any(|known| known == level)
        }) {
            catalog.reasoning_defaults.insert(model, level.to_owned());
        }
        Some(catalog)
    });
    Opened {
        signed_in: true,
        session: result["sessionId"].as_str().map(str::to_owned),
        catalog,
        tier: result["_meta"]["antigravity"]["tier"]["name"]
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_lists_its_models_levels_defaults_and_tier() {
        let opened = opened(&json!({
            "sessionId": "5eb802bd-a4cd-45e4-95df-8a58b97d9c8c",
            "configOptions": [
                {"id": "model", "category": "model", "type": "select", "currentValue": "gemini-4-neon",
                 "options": [{"value": "gemini-4-neon", "name": "Gemini 4 Neon"},
                             {"value": "gemini-3.8-flash", "name": "Gemini 3.8 Flash"}]},
                {"id": "thought_level", "category": "thought_level", "type": "select", "currentValue": "high",
                 "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
                {"id": "mode", "category": "mode", "type": "select", "currentValue": "default",
                 "options": [{"value": "default", "name": "Default"}]}],
            "models": {"currentModelId": "gemini-4-neon-high", "availableModels": [
                {"modelId": "gemini-4-neon-low"}, {"modelId": "gemini-4-neon-high"},
                {"modelId": "gemini-3.8-flash-low"}, {"modelId": "gpt-oss-120b-medium"}]},
            "_meta": {"antigravity": {"tier": {"name": "Google AI Pro", "id": "g1-pro-tier"}}},
        }));
        assert!(opened.signed_in);
        assert_eq!(
            opened.session.as_deref(),
            Some("5eb802bd-a4cd-45e4-95df-8a58b97d9c8c")
        );
        assert_eq!(opened.tier.as_deref(), Some("Google AI Pro"));
        let catalog = opened.catalog.unwrap();
        assert_eq!(
            catalog
                .models
                .iter()
                .map(|model| (model.id.as_str(), model.label.as_str()))
                .collect::<Vec<_>>(),
            [
                ("gemini-4-neon", "Gemini 4 Neon"),
                ("gemini-3.8-flash", "Gemini 3.8 Flash")
            ]
        );
        // Each model has its own levels, not the current model's.
        assert_eq!(catalog.models[0].reasoning, ["low", "high"]);
        assert_eq!(catalog.models[1].reasoning, ["low"]);
        assert_eq!(catalog.default_model.as_deref(), Some("gemini-4-neon"));
        assert_eq!(catalog.default_effort(None), Some("high"));
    }

    #[test]
    fn a_signed_out_agent_asks_for_sign_in_and_a_signed_in_one_reports_its_plan() {
        let signed_out = probed("Antigravity", Opened::default(), 7);
        assert_eq!(signed_out.snapshot.status, UsageStatus::NeedsLogin);
        assert!(signed_out.identity.is_none());
        let signed_in = probed(
            "Antigravity",
            Opened {
                signed_in: true,
                tier: Some("Google AI Pro".into()),
                ..Opened::default()
            },
            7,
        );
        assert_eq!(signed_in.snapshot.status, UsageStatus::Ok);
        assert!(signed_in.snapshot.windows.is_empty());
        assert_eq!(
            signed_in.snapshot.plan_display_name.as_deref(),
            Some("Google AI Pro")
        );
        assert!(super::super::confirms_login(
            signed_in.identity.as_ref(),
            signed_in.snapshot.status
        ));
    }

    #[test]
    fn only_the_probe_s_session_files_are_forgotten() {
        let data = tempfile::tempdir().unwrap();
        let id = "5eb802bd-a4cd-45e4-95df-8a58b97d9c8c";
        let at = |path: &str| data.path().join(path);
        for directory in ["conversations", "annotations", "implicit", "workspace"] {
            std::fs::create_dir_all(at(directory)).unwrap();
        }
        std::fs::create_dir_all(at(&format!("brain/{id}/nested"))).unwrap();
        for file in [
            format!("conversations/{id}.db"),
            format!("conversations/{id}.db-wal"),
            format!("annotations/{id}.pbtxt"),
            format!("implicit/{id}.pb"),
            "conversations/other.db".into(),
            "workspace/note".into(),
            "settings.json".into(),
        ] {
            std::fs::write(at(&file), b"").unwrap();
        }
        // A link named like a session is removed, not followed.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("kept"), b"").unwrap();
        std::os::unix::fs::symlink(
            outside.path(),
            at("brain/0ab4e7b5-91eb-4a02-981a-0190827ccaa1"),
        )
        .unwrap();
        forget(data.path());
        let left = |directory: &str| {
            let mut names = std::fs::read_dir(at(directory))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        assert_eq!(left("conversations"), ["other.db"]);
        assert!(left("brain").is_empty());
        assert!(left("annotations").is_empty());
        assert!(left("implicit").is_empty());
        assert_eq!(left("workspace"), ["note"]);
        assert!(at("settings.json").exists());
        assert!(outside.path().join("kept").exists());
    }

    /// The fake ACP agent, as `agy`, with its own home and state.
    struct Agent {
        directory: tempfile::TempDir,
        program: PathBuf,
    }

    impl Agent {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let directory = tempfile::tempdir().unwrap();
            let program = directory.path().join("agy");
            std::fs::write(
                &program,
                include_str!("../../../tests/fixtures/fake-acp-agent.py"),
            )
            .unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::create_dir_all(directory.path().join("state")).unwrap();
            Self { directory, program }
        }

        fn data(&self) -> PathBuf {
            self.directory
                .path()
                .join(".gemini")
                .join(ANTIGRAVITY_PROBE_DATA)
        }

        fn mark(&self, name: &str) {
            std::fs::write(self.directory.path().join("state").join(name), b"").unwrap();
        }

        fn command(&self) -> Command {
            let mut command = Command::new(&self.program);
            command
                .args(["acp", "--disable-telemetry", "--app-data-dir"])
                .arg(ANTIGRAVITY_PROBE_DATA)
                .env("HOME", self.directory.path())
                .env("FAKE_ACP_SHAPE", "agy")
                .env("FAKE_ACP_STATE", self.directory.path().join("state"))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .process_group(0);
            command
        }

        /// The session files left in the probe's data directory.
        fn sessions(&self) -> Vec<PathBuf> {
            SESSION_STORES
                .iter()
                .filter_map(|store| std::fs::read_dir(self.data().join(store)).ok())
                .flatten()
                .map(|entry| entry.unwrap().path())
                .collect()
        }
    }

    #[tokio::test]
    async fn a_probe_reads_each_model_s_levels_and_leaves_no_session_behind() {
        let agent = Agent::new();
        // What an earlier probe, cut short, left.
        let stale = agent
            .data()
            .join("conversations/16a9c3a8-8d41-4d2e-9a1c-8b8e2b0f7c11.db");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"").unwrap();
        let probed = probe_antigravity_in(agent.data(), agent.command())
            .await
            .unwrap();
        assert_eq!(probed.snapshot.status, UsageStatus::Ok);
        assert_eq!(
            probed.snapshot.plan_display_name.as_deref(),
            Some("Google AI Pro")
        );
        // Each model with its own levels, as the fixture's agy offers them.
        let catalog = probed.catalog.unwrap();
        let levels = catalog
            .models
            .iter()
            .map(|model| (model.id.as_str(), model.reasoning.join(" ")))
            .collect::<Vec<_>>();
        assert_eq!(
            levels,
            [
                ("gemini-3.8-flash", "low medium high".to_owned()),
                ("gemini-4-neon", "low medium high xhigh".to_owned()),
                ("gemini-pro-orchestrator-neon", String::new()),
                ("gpt-oss-120b-medium", String::new()),
            ]
        );
        assert_eq!(agent.sessions(), Vec::<PathBuf>::new());

        agent.mark("signed-out");
        let signed_out = probe_antigravity_in(agent.data(), agent.command())
            .await
            .unwrap();
        assert_eq!(signed_out.snapshot.status, UsageStatus::NeedsLogin);
    }

    #[tokio::test]
    async fn a_probe_abandoned_by_its_timeout_leaves_no_session_behind() {
        let agent = Agent::new();
        agent.mark("hang-probe");
        let mut probe = Box::pin(probe_antigravity_in(agent.data(), agent.command()));
        // Abandon it once the agent has saved all four entries of the
        // session it will not answer for.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while agent.sessions().len() < SESSION_STORES.len() {
            assert!(
                std::time::Instant::now() < deadline,
                "the agent saved no session"
            );
            tokio::select! {
                result = &mut probe => panic!("the hung probe answered: {:?}", result.err()),
                () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
        }
        drop(probe);
        // The removal runs on a blocking thread once the probe is dropped.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !agent.sessions().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(agent.sessions(), Vec::<PathBuf>::new());
    }

    /// A second probe, such as a sign-in's check beside the account's own,
    /// waits for the first rather than sweeping away its open session.
    #[tokio::test]
    async fn a_probe_waits_for_another_rather_than_removing_its_session() {
        let agent = Agent::new();
        agent.mark("hang-probe");
        let mut first = Box::pin(probe_antigravity_in(agent.data(), agent.command()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while agent.sessions().len() < SESSION_STORES.len() {
            assert!(
                std::time::Instant::now() < deadline,
                "the agent saved no session"
            );
            tokio::select! {
                result = &mut first => panic!("the hung probe answered: {:?}", result.err()),
                () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
        }
        let mut second = tokio::spawn(probe_antigravity_in(agent.data(), agent.command()));
        tokio::select! {
            _ = &mut second => panic!("the second probe did not wait"),
            () = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
        }
        assert_eq!(agent.sessions().len(), SESSION_STORES.len());
        // The first gives up; the second then runs, and leaves nothing.
        std::fs::remove_file(agent.directory.path().join("state/hang-probe")).unwrap();
        drop(first);
        let probed = tokio::time::timeout(std::time::Duration::from_secs(10), second)
            .await
            .expect("the second probe finished")
            .unwrap()
            .unwrap();
        assert_eq!(probed.snapshot.status, UsageStatus::Ok);
        assert_eq!(agent.sessions(), Vec::<PathBuf>::new());
    }
}
