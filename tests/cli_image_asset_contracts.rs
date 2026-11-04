#![cfg(target_os = "linux")]
//! Public CLI refusals must precede backend preparation and private store access.
use cagents::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
use serde_json::Value;
use std::{path::Path, time::Duration};

#[test]
fn physical_image_assets_refuse_hosted_and_remote_routes_without_creating_state() {
    let root = tempfile::tempdir().unwrap();
    let authority = std::env::var_os(cagents::agent_execution::AUTHORITY_ENV);
    for arguments in [
        &["list"][..],
        &[
            "remove",
            "11111111-1111-4111-8111-111111111111",
            "--sha256",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--acknowledge-history-loss",
        ][..],
    ] {
        for hosted in [false, true] {
            let device = if hosted { "local" } else { "does-not-exist" };
            let route = if hosted {
                "hosted/local"
            } else {
                "remote device"
            };
            let context = format!("{} ({route}, device={device})", arguments[0]);
            let mut command = CommandSpec::new(Path::new(env!("CARGO_BIN_EXE_cagents")))
                .args(["--json", "--device", device, "chat", "image-assets"])
                .args(arguments.iter().copied())
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_CACHE_HOME", root.path().join("cache"))
                .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
                .cwd(root.path())
                .timeout(Duration::from_secs(10))
                .output_limits(64 * 1024, 64 * 1024);
            // Never strip an inherited hosted authority. The explicit hosted case
            // adds an invalid fixture capability only when no authority exists.
            if hosted && authority.is_none() {
                command = command.env(
                    cagents::agent_execution::AUTHORITY_ENV,
                    "image-assets-test-refusal",
                );
            }
            let output = SystemProcessExecutor
                .execute(&command, &CancellationToken::new())
                .unwrap_or_else(|error| panic!("{context}: bounded CLI failed: {error}"));
            assert!(!output.success, "{context}: refusal unexpectedly succeeded");
            assert!(
                output.stdout.is_empty(),
                "{context}: refusal emitted {} bytes on stdout",
                output.stdout.len()
            );
            let envelope: Value = serde_json::from_slice(&output.stderr)
                .unwrap_or_else(|error| panic!("{context}: invalid refusal JSON: {error}"));
            assert_eq!(envelope["ok"], false, "{context}: invalid refusal envelope");
            let kind = envelope["error"]["kind"]
                .as_str()
                .unwrap_or_else(|| panic!("{context}: missing refusal kind"));
            assert!(
                matches!(kind, "image_assets_user_only" | "image_assets_local_only"),
                "{context}: refusal must precede device lookup, daemon recovery and storage: {envelope}"
            );
            if hosted {
                assert_eq!(kind, "image_assets_user_only", "{context}");
            }
            for directory in ["config", "data", "cache", "runtime"] {
                assert!(
                    !root.path().join(directory).exists(),
                    "{context}: refused route created {directory}"
                );
            }
        }
    }
}
