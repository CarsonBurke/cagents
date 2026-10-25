//! The matched CLI's fixed helper must work without any daemon/device route.
#![cfg(unix)]

use cagents::{
    account::workspace_probe::Receipt,
    process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
};
use std::{ffi::OsString, os::unix::fs::MetadataExt, path::Path, time::Duration};

fn invoke(root: &Path, extra: &[&str]) -> cagents::process::ProcessOutput {
    let metadata = std::fs::metadata(root).unwrap();
    let nonce = uuid::Uuid::new_v4();
    let mut arguments: Vec<OsString> = extra.iter().map(OsString::from).collect();
    arguments.extend([
        "internal-workspace-probe".into(),
        root.as_os_str().into(),
        "--device-id".into(),
        metadata.dev().to_string().into(),
        "--inode".into(),
        metadata.ino().to_string().into(),
        "--nonce".into(),
        nonce.to_string().into(),
    ]);
    let spec = CommandSpec::new(env!("CARGO_BIN_EXE_cagents"))
        .args(arguments)
        // Preserve caller identity/authority. A regression must never recover
        // a real daemon or read the user's device configuration.
        .env("XDG_CONFIG_HOME", root.join("private-config"))
        .env("XDG_RUNTIME_DIR", root.join("private-runtime"))
        .env("CAGENTS_NO_START", "1")
        .timeout(Duration::from_secs(5))
        .output_limits(4096, 4096);
    SystemProcessExecutor
        .execute(&spec, &CancellationToken::new())
        .unwrap()
}

#[test]
fn fixed_workspace_helper_round_trips_and_removes_its_scratch_without_a_daemon() {
    let root = tempfile::tempdir().unwrap();
    let output = invoke(root.path(), &[]);
    assert_eq!(output.exit_code, Some(0));
    let receipt: Receipt = serde_json::from_slice(&output.stdout).unwrap();
    assert!(receipt.is_verified());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn fixed_helper_refuses_device_selection_before_device_lookup_or_service_recovery() {
    let root = tempfile::tempdir().unwrap();
    let output = invoke(
        root.path(),
        &["--json", "--device", "must-not-be-looked-up"],
    );
    assert_eq!(output.exit_code, Some(1));
    assert!(output.stdout.is_empty());
    let response: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["kind"], "workspace_probe");
    assert_eq!(
        response["error"]["message"],
        "The internal workspace probe is local only"
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
