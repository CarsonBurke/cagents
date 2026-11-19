//! Asking an Agent Client Protocol agent how its sign-in stands, and what
//! it offers, by opening a session and asking nothing of it.
//!
//! `initialize` alone succeeds signed out, so only `session/new` tells: it
//! fails with the protocol's `auth_required` error, or answers with the
//! session's models, reasoning levels and modes. Such a session is saved by
//! the agent like any other, so a provider's probe has to keep it out of the
//! reader's conversations; Antigravity's keeps it in a data directory of its
//! own (see [`probe_antigravity`]).
use std::{ffi::OsStr, path::Path};

use serde_json::{Value, json};

use super::{Catalog, Identity, Observed, Probed, Session, UsageSnapshot, UsageStatus};
use crate::{account::Account, chat::Model};

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
    let workspace = data.join("workspace");
    std::fs::create_dir_all(&workspace)
        .map_err(|_| "Antigravity's probe directory could not be created".to_owned())?;
    let mut command = super::command_with_path(account, program, search_path);
    command
        .args(["acp", "--disable-telemetry", "--app-data-dir"])
        .arg(ANTIGRAVITY_PROBE_DATA)
        .current_dir(&workspace);
    let mut session = Session::spawn(command).await?;
    let result = open(&mut session, &workspace).await;
    let result = super::after_close(result, session.close()).await;
    if let Ok(Observed {
        value: Opened {
            session: Some(id), ..
        },
        ..
    }) = &result
    {
        forget(&data, id);
    }
    let Observed {
        value: opened,
        observed_at,
    } = result?;
    Ok(probed("Antigravity", opened, observed_at))
}

/// Removes what the agent saved of the probe's session from the probe's own
/// data directory: nothing anyone will open again.
fn forget(data: &Path, id: &str) {
    if id.len() != 36 || uuid::Uuid::parse_str(id).is_err() {
        return;
    }
    let conversations = data.join("conversations");
    for suffix in ["db", "db-wal", "db-shm"] {
        let _ = std::fs::remove_file(conversations.join(format!("{id}.{suffix}")));
    }
    let _ = std::fs::remove_dir_all(data.join("brain").join(id));
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
/// the reasoning levels, and the defaults it opened with.
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
    let catalog = select("model").and_then(|option| {
        let models = values(option)
            .into_iter()
            .map(|(id, label)| Model {
                id,
                label,
                reasoning: levels.clone(),
            })
            .collect::<Vec<_>>();
        let mut catalog = super::observed_catalog(models, super::now_ms())?;
        catalog.default_model = option["currentValue"].as_str().map(str::to_owned);
        if let (Some(model), Some(level)) = (
            catalog.default_model.clone(),
            reasoning.and_then(|option| option["currentValue"].as_str()),
        ) {
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
        assert_eq!(catalog.models[1].reasoning, ["low", "high"]);
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
    fn only_the_probe_s_own_session_files_are_forgotten() {
        let data = tempfile::tempdir().unwrap();
        let id = "5eb802bd-a4cd-45e4-95df-8a58b97d9c8c";
        std::fs::create_dir_all(data.path().join("conversations")).unwrap();
        std::fs::create_dir_all(data.path().join("brain").join(id)).unwrap();
        for name in [
            format!("{id}.db"),
            format!("{id}.db-wal"),
            "other.db".into(),
        ] {
            std::fs::write(data.path().join("conversations").join(name), b"").unwrap();
        }
        forget(data.path(), "../../escape");
        forget(data.path(), id);
        let left = std::fs::read_dir(data.path().join("conversations"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(left, ["other.db"]);
        assert!(!data.path().join("brain").join(id).exists());
    }
}
