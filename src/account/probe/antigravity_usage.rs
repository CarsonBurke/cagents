//! Antigravity's headless `/usage` invokes the same quota summary as its
//! interactive panel. The CLI owns token storage, refresh and account selection.

use std::time::Duration;

use serde_json::Value;
use tokio::{process::Command, time::Instant};

use super::Observed;
use crate::account::{UsageWindow, WindowKind, clean_label};

const QUOTA_TIMEOUT: Duration = Duration::from_secs(10);
const TIMED_OUT: &str = "Antigravity did not return usage limits in time";
const MAX_WINDOWS: usize = 64;
const NO_QUOTA: &str = "Antigravity returned no quota figures for this account";

pub(super) async fn read(
    mut command: Command,
    version: Option<&str>,
    deadline: Instant,
) -> Result<Observed<Vec<UsageWindow>>, &'static str> {
    // The 1.1.11 release added read-only slash commands to print mode.
    // Earlier versions might send /usage to a model as a prompt.
    let supports_quota = version
        .and_then(|version| semver::Version::parse(version.trim().trim_start_matches('v')).ok())
        .is_some_and(|version| version >= semver::Version::new(1, 1, 11));
    if !supports_quota {
        return Err("Update Antigravity to a version that supports reading usage limits");
    }
    let now = Instant::now();
    if deadline <= now {
        return Err(TIMED_OUT);
    }
    let deadline = deadline.min(now + QUOTA_TIMEOUT);
    // Ensure the CLI handles /usage locally even if the user's settings
    // disable slash commands. The inline settings apply only to this process.
    command
        .args([
            "--print",
            "/usage",
            "--output-format",
            "json",
            "--disable-slash-commands=false",
            "--settings",
            r#"{"disableSlashCommands":false}"#,
            "--log-file",
            "/dev/null",
        ])
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true");
    super::scrub(&mut command, super::SHELL_SCRUBBED_ENVIRONMENT);
    let report = tokio::time::timeout_at(deadline, super::run_json(command))
        .await
        .map_err(|_| TIMED_OUT)?
        .map_err(|_| "Antigravity did not return a readable usage report")?;
    Ok(Observed {
        value: windows(&report.value)?,
        observed_at: report.observed_at,
    })
}

fn windows(report: &Value) -> Result<Vec<UsageWindow>, &'static str> {
    if report["status"] != "SUCCESS" {
        return Err(match report["error"].as_str() {
            Some("/usage failed: no quota summary is available for this account") => NO_QUOTA,
            _ => "Antigravity could not retrieve usage limits",
        });
    }
    let command = &report["command"];
    let Some(groups) = command["data"]["groups"]
        .as_array()
        .filter(|_| command["name"] == "usage")
    else {
        return Err("Antigravity returned an unrecognized usage report");
    };
    let mut windows = Vec::new();
    for group in groups {
        let Some(pool) = group["name"].as_str().and_then(clean_label) else {
            continue;
        };
        let pool_id = group["id"].as_str().and_then(clean_label);
        let pool_key = pool_id.as_deref().unwrap_or(&pool);
        for bucket in group["buckets"].as_array().into_iter().flatten() {
            if let Some(window) = window(&pool, pool_key, bucket)
                && !windows
                    .iter()
                    .any(|existing: &UsageWindow| existing.key == window.key)
            {
                windows.push(window);
                if windows.len() == MAX_WINDOWS {
                    return Ok(windows);
                }
            }
        }
    }
    if windows.is_empty() {
        return Err(NO_QUOTA);
    }
    Ok(windows)
}

fn window(pool: &str, pool_key: &str, bucket: &Value) -> Option<UsageWindow> {
    if bucket["disabled"] == true {
        return None;
    }
    let remaining = bucket["remaining_fraction"]
        .as_f64()
        .filter(|fraction| (0.0..=1.0).contains(fraction))?;
    let period = bucket["window"].as_str().unwrap_or_default();
    let (span_secs, period_label) = match period {
        "5h" => (Some(5 * 3600), "5-hour"),
        "weekly" => (Some(7 * 86400), "Weekly"),
        _ => (None, period),
    };
    let id = bucket["id"].as_str().and_then(clean_label);
    let name = bucket["name"]
        .as_str()
        .and_then(clean_label)
        .or_else(|| clean_label(period_label))
        .or_else(|| id.clone())?;
    let bucket_key = id.as_deref().unwrap_or(&name);
    Some(UsageWindow {
        key: format!("agy:{pool_key}:{bucket_key}"),
        kind: WindowKind::Scoped,
        label: format!("{pool} · {name}"),
        used: (1.0 - remaining) as f32,
        resets_at: bucket["reset_time"]
            .as_str()
            .and_then(|time| time.parse::<jiff::Timestamp>().ok())
            .map(|time| time.as_second()),
        span_secs,
        binding: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_model_pools_without_treating_disabled_or_missing_fractions_as_zero() {
        let report = json!({
            "status": "SUCCESS",
            "command": {"name": "usage", "data": {"groups": [
                {"name": "Gemini Models", "buckets": [
                    {"id": "gemini-5h", "name": "5-Hour Quota", "window": "5h",
                     "remaining_fraction": 0.75, "reset_time": "2026-11-24T05:20:00Z"},
                    {"id": "gemini-weekly", "window": "weekly",
                     "remaining_fraction": 0.0, "reset_time": "2026-11-28T06:15:00+01:00"},
                ]},
                {"name": "Claude and GPT models", "buckets": [
                    {"id": "3p-5h", "window": "5h", "remaining_fraction": 1.0},
                    {"id": "3p-weekly", "window": "weekly", "disabled": true,
                     "remaining_fraction": 0.0},
                    {"id": "spillover", "name": "Spillover", "remaining_amount": 5},
                    {"id": "unknown", "name": "Unknown", "remaining_fraction": -1},
                ]},
            ]}}
        });
        let windows = windows(&report).unwrap();
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].key, "agy:Gemini Models:gemini-5h");
        assert_eq!(windows[0].label, "Gemini Models · 5-Hour Quota");
        assert_eq!(windows[0].span_secs, Some(18_000));
        assert_eq!(windows[0].used, 0.25);
        assert_eq!(windows[0].resets_at, Some(1_795_497_600));
        assert_eq!(windows[1].used, 1.0);
        assert_eq!(windows[1].span_secs, Some(604_800));
        assert_eq!(windows[1].resets_at, Some(1_795_842_900));
        assert_eq!(windows[2].used, 0.0);
        assert_eq!(windows[2].resets_at, None);
        assert!(
            windows
                .iter()
                .all(|window| window.kind == WindowKind::Scoped && !window.binding)
        );
    }

    #[test]
    fn distinguishes_a_failed_empty_and_unrecognized_report() {
        assert_eq!(
            windows(&json!({"status":"ERROR"})),
            Err("Antigravity could not retrieve usage limits")
        );
        assert_eq!(
            windows(&json!({
                "status": "ERROR",
                "error": "/usage failed: no quota summary is available for this account"
            })),
            Err(NO_QUOTA)
        );
        assert_eq!(
            windows(&json!({"status":"SUCCESS", "response": "ordinary prompt output"})),
            Err("Antigravity returned an unrecognized usage report")
        );
        assert_eq!(
            windows(&json!({
                "status":"SUCCESS", "command":{"name":"usage", "data":{"groups":[]}}
            })),
            Err(NO_QUOTA)
        );
    }

    #[test]
    fn deduplicates_buckets_within_each_pool() {
        let bucket = json!({
            "id": "new-model-quota",
            "remaining_fraction": 0.4,
            "reset_time": "invalid"
        });
        let report = json!({
            "status": "SUCCESS",
            "command": {"name": "usage", "data": {"groups": [
                {"name": "New model", "id": "new", "buckets": [bucket.clone(), bucket.clone()]},
                {"name": "Another pool", "buckets": [bucket]},
            ]}}
        });
        let windows = windows(&report).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].key, "agy:new:new-model-quota");
        assert_eq!(windows[0].label, "New model · new-model-quota");
        assert_eq!(windows[0].span_secs, None);
        assert_eq!(windows[0].resets_at, None);
        assert_eq!(windows[1].key, "agy:Another pool:new-model-quota");
    }

    #[tokio::test]
    async fn refuses_unsupported_versions_before_starting_the_cli() {
        for version in [None, Some("1.1.10"), Some("1.1.11-rc.1"), Some("unknown")] {
            let result = read(
                Command::new("/does-not-exist"),
                version,
                Instant::now() + QUOTA_TIMEOUT,
            )
            .await;
            assert_eq!(
                result.err(),
                Some("Update Antigravity to a version that supports reading usage limits")
            );
        }
    }

    #[tokio::test]
    async fn skips_quota_when_the_probe_has_no_time_left() {
        let result = read(
            Command::new("/does-not-exist"),
            Some("1.1.11"),
            Instant::now(),
        )
        .await;
        assert_eq!(result.err(), Some(TIMED_OUT));
    }

    #[tokio::test]
    async fn bounds_a_quota_read_by_the_remaining_probe_deadline() {
        let account = crate::account::Account::system(crate::agent::Provider::Antigravity);
        let mut command =
            super::super::command_with_path(&account, std::path::Path::new("/bin/sh"), None);
        command.args(["-c", "sleep 30"]);
        let result = read(
            command,
            Some("1.5.18"),
            Instant::now() + Duration::from_millis(100),
        )
        .await;
        assert_eq!(
            result.err(),
            Some("Antigravity did not return usage limits in time")
        );
    }
}
