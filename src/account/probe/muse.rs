//! Asking Muse how its sign-in stands, and which models it offers, without
//! opening a session.
//!
//! Muse saves every session it opens, an empty one included, and lists it
//! among the reader's conversations, so the probe never starts `muse acp`.
//! `muse usage --json` says whether a login works and takes no model turn;
//! the models are the ones Muse last fetched for its own pickers, from its
//! model catalog cache.
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
};

use serde_json::Value;
use tokio::io::AsyncReadExt;

use super::{Catalog, Group, Probed, UsageStatus, after_close, now_ms};
use crate::{account::Account, agent::Provider, chat::Model};

/// The most of Muse's complaint the probe reads; it is one line.
const STDERR_LIMIT: u64 = 64 * 1024;
/// How Muse says a command needs a login: "muse usage: not logged in: run
/// `muse login` first".
const NOT_LOGGED_IN: &str = "not logged in";
/// The catalog cache layout this reads. Another is left alone.
const CATALOG_SCHEMA: u64 = 3;
/// A catalog cache larger than this is not one.
const CATALOG_LIMIT: u64 = 4 * 1024 * 1024;

/// Probes the system Muse sign-in.
pub(super) async fn probe_muse(
    account: &Account,
    program: &Path,
    search_path: Option<&OsStr>,
) -> Result<Probed, String> {
    let program = program.to_str().ok_or("Muse's path is not text")?;
    let path = search_path.and_then(OsStr::to_str).unwrap_or_default();
    let program = PathBuf::from(Provider::Muse.subcommand_program(program, Path::new("/"), path));
    let mut command = super::command_with_path(account, &program, search_path);
    command.args(["usage", "--json"]).stderr(Stdio::piped());
    let mut group = Group::spawn(command).await?;
    let stdout = group.stdout()?;
    let stderr = group.child.stderr.take();
    let read = async {
        let (mut output, mut complaint) = (Vec::new(), Vec::new());
        let stdout = async {
            let mut stdout = stdout;
            stdout.read_to_end(&mut output).await
        };
        let stderr = async {
            match stderr {
                Some(stderr) => stderr
                    .take(STDERR_LIMIT)
                    .read_to_end(&mut complaint)
                    .await
                    .map(|_| ()),
                None => Ok(()),
            }
        };
        let (read, _) = tokio::join!(stdout, stderr);
        read.map(|_| (output, complaint))
            .map_err(|_| "Muse's output could not be read".to_owned())
    };
    let result = after_close(read.await, group.finish()).await;
    let (output, complaint) = result?;
    let observed_at = now_ms();
    let catalog = catalog(&data_home(&|name| std::env::var(name).ok()), observed_at);
    usage(&output, &complaint, observed_at, catalog)
}

/// What `muse usage --json` printed, and complained, means.
fn usage(
    output: &[u8],
    complaint: &[u8],
    observed_at: u64,
    catalog: Option<Catalog>,
) -> Result<Probed, String> {
    if String::from_utf8_lossy(complaint).contains(NOT_LOGGED_IN) {
        return Ok(Probed::state(
            UsageStatus::NeedsLogin,
            "Signed out of Muse",
            observed_at,
            catalog,
        ));
    }
    let report: Value = serde_json::from_slice(output)
        .map_err(|_| "Muse could not report its sign-in just now".to_owned())?;
    if !report["schemaVersion"].is_u64() {
        return Err("Muse printed something unexpected".into());
    }
    // A subscription's allowance has no published shape; that there is none
    // is plain, and is why Muse refuses every turn.
    let (status, detail) = match (&report["allowance"], report["reason"].as_str()) {
        (Value::Null, Some("no_allowance")) => (
            UsageStatus::Limited,
            "This Muse login has no subscription allowance",
        ),
        // A login that works is shown, though its limits are not known: an
        // unavailable system account is taken for one not set up, and hidden.
        _ => (UsageStatus::Ok, "Muse does not report its limits"),
    };
    Ok(Probed::state(status, detail, observed_at, catalog))
}

/// Muse's data directory: `$XDG_DATA_HOME/muse`, by the base directory
/// spec, which ignores a relative value.
fn data_home(environment: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    environment("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|data| data.is_absolute())
        .or_else(|| {
            environment("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| Path::new(&home).join(".local/share"))
        })
        .map(|data| data.join("muse"))
}

/// The models Muse's own pickers offer, from the catalog it last fetched:
/// its newest cache, of the layout this reads. Hidden and contributor models
/// are left out, as a Muse chat leaves them out. Their reasoning levels are
/// left for the conversation, which offers fewer than the catalog lists.
fn catalog(data: &Option<PathBuf>, observed_at: u64) -> Option<Catalog> {
    let directory = data.as_ref()?.join("model-catalog");
    let newest = std::fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            (metadata.is_file() && metadata.len() <= CATALOG_LIMIT)
                .then(|| (metadata.modified().ok(), entry.path()))
        })
        .max()?
        .1;
    parse_catalog(&std::fs::read(newest).ok()?, observed_at)
}

fn parse_catalog(bytes: &[u8], observed_at: u64) -> Option<Catalog> {
    let cache: Value = serde_json::from_slice(bytes).ok()?;
    if cache["schema_version"].as_u64() != Some(CATALOG_SCHEMA) {
        return None;
    }
    let rows = cache["rows"].as_array()?;
    let offered = |row: &&Value| {
        row["visibility"] == "visible"
            && row["model_id"]
                .as_str()
                .is_some_and(crate::agent::muse_offers_model)
    };
    let models: Vec<Model> = rows
        .iter()
        .filter(offered)
        .filter_map(|row| {
            let id = row["model_id"].as_str()?;
            Some(Model {
                id: id.to_owned(),
                label: row["display_label"].as_str().unwrap_or(id).to_owned(),
                reasoning: Vec::new(),
            })
        })
        .collect();
    let default_model = rows
        .iter()
        .filter(offered)
        .find(|row| row["is_default"] == true)
        .and_then(|row| row["model_id"].as_str())
        .map(str::to_owned);
    (!models.is_empty()).then(|| Catalog {
        models,
        observed_at,
        default_model,
        reasoning_defaults: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_without_allowance_is_limited_and_a_missing_one_needs_signing_in() {
        let none = br#"{"allowance":null,"hint":null,"reason":"no_allowance","schemaVersion":1}"#;
        let probed = usage(none, b"", 7, None).unwrap();
        assert_eq!(probed.snapshot.status, UsageStatus::Limited);
        let signed_out = usage(
            b"",
            b"muse usage: not logged in: run `muse login` first\n",
            7,
            None,
        )
        .unwrap();
        assert_eq!(signed_out.snapshot.status, UsageStatus::NeedsLogin);
        // Anything else is not a sign-in state at all.
        assert!(usage(b"", b"network unreachable", 7, None).is_err());
        assert!(usage(b"{}", b"", 7, None).is_err());
        let other = br#"{"allowance":{"remaining":3},"reason":null,"schemaVersion":1}"#;
        // An allowance of a shape not yet seen is a working login of
        // unknown limits, which keeps the account shown.
        let working = usage(other, b"", 7, None).unwrap().snapshot;
        assert_eq!(working.status, UsageStatus::Ok);
        assert!(working.windows.is_empty());
    }

    #[test]
    fn the_catalog_offers_visible_models_without_contributor_ones() {
        let cache = br#"{"schema_version":3,"rows":[
            {"model_id":"muse-spark-1.3","display_label":"muse-spark-1.3","visibility":"visible","is_default":true},
            {"model_id":"muse-spark-1.3-contributor","visibility":"hidden","is_default":false},
            {"model_id":"muse-spark-1.2-contributor","visibility":"visible","is_default":false},
            {"model_id":"muse-image-1.0","display_label":"Muse Image","visibility":"hidden"},
            {"model_id":"muse-spark-1.2","visibility":"visible"}]}"#;
        let catalog = parse_catalog(cache, 9).unwrap();
        let ids: Vec<_> = catalog
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect();
        assert_eq!(ids, ["muse-spark-1.3", "muse-spark-1.2"]);
        assert_eq!(catalog.default_model.as_deref(), Some("muse-spark-1.3"));
        assert!(
            catalog
                .models
                .iter()
                .all(|model| model.reasoning.is_empty())
        );
        assert!(parse_catalog(br#"{"schema_version":4,"rows":[]}"#, 9).is_none());
    }

    #[test]
    fn the_data_home_follows_the_base_directory_spec() {
        let environment = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        assert_eq!(
            data_home(&environment(&[("HOME", "/h"), ("XDG_DATA_HOME", "/d")])),
            Some(PathBuf::from("/d/muse"))
        );
        assert_eq!(
            data_home(&environment(&[("HOME", "/h"), ("XDG_DATA_HOME", "rel")])),
            Some(PathBuf::from("/h/.local/share/muse"))
        );
    }
}
