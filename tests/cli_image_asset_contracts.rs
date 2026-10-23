#![cfg(target_os = "linux")]
//! Public CLI refusals must precede backend preparation and private store access.
use cagents::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
use serde_json::Value;
use std::{path::Path, time::Duration};

#[test]
fn physical_image_assets_refuse_hosted_and_remote_routes_without_creating_state() {
    let root = tempfile::tempdir().unwrap();
    let authority = std::env::var_os(cagents::agent_execution::AUTHORITY_ENV);
    for hosted in [false, true] {
        let mut command = CommandSpec::new(Path::new(env!("CARGO_BIN_EXE_cagents")))
            .args([
                "--json",
                "--device",
                if hosted { "local" } else { "does-not-exist" },
                "chat",
                "image-assets",
                "list",
            ])
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
            .unwrap();
        assert!(!output.success);
        let envelope: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(envelope["ok"], false);
        let kind = envelope["error"]["kind"].as_str().unwrap();
        assert!(
            matches!(kind, "image_assets_user_only" | "image_assets_local_only"),
            "refusal must precede device lookup, daemon recovery and storage: {envelope}"
        );
        if hosted {
            assert_eq!(kind, "image_assets_user_only");
        }
        for directory in ["config", "data", "cache", "runtime"] {
            assert!(
                !root.path().join(directory).exists(),
                "refused route created {directory}"
            );
        }
    }
}
