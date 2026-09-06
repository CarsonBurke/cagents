#![cfg(target_os = "linux")]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

// Exercise both public uninstall entrypoints. They share the upgrade gate that
// must recognize the CLI's JSON before removing an existing installation.
fn uninstall_fixture(script: &str, status: &str, expect_removal: bool) {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join(".local/share");
    let prefix = data.join("cagents-install");
    let bin = prefix.join("versions/test/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        prefix.join(".cagents-install-root"),
        "cagents install root\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("versions/test", prefix.join("current")).unwrap();
    let cli = bin.join("cagents");
    fs::write(
        &cli,
        "#!/bin/sh\ncase \"$*\" in\n  '--json daemon status') printf '%s\\n' \"$TEST_DAEMON_STATUS\" ;;\n  *) exit 99 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).unwrap();
    let saved = data.join("cagents/project-data");
    fs::create_dir_all(saved.parent().unwrap()).unwrap();
    fs::write(&saved, "preserve me").unwrap();

    let mut command = Command::new("sh");
    command.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(script));
    if script == "install.sh" {
        command.arg("--uninstall");
    }
    let output = command
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("TEST_DAEMON_STATUS", status)
        .output()
        .unwrap();
    assert_eq!(
        output.status.success(),
        expect_removal,
        "{script}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(cli.exists(), !expect_removal);
    assert_eq!(fs::read_to_string(saved).unwrap(), "preserve me");
}

#[test]
fn uninstall_accepts_current_and_legacy_cli_status_envelopes() {
    let current = serde_json::json!({
        "ok": true,
        "command": "daemon.status",
        "data": {"instance": "production", "running": false, "socket": "/run/cagentsd.sock"}
    })
    .to_string();
    let legacy = r#"{"ok":true,"command":"daemon.status","data":{"running":false}}"#;
    for script in ["install.sh", "packaging/cagents-uninstall"] {
        for status in [current.as_str(), legacy] {
            uninstall_fixture(script, status, true);
        }
    }
}

#[test]
fn uninstall_preserves_installation_when_daemon_status_is_untrusted() {
    for script in ["install.sh", "packaging/cagents-uninstall"] {
        for status in [
            r#"{"command":"daemon.status","data":{"running":false,"socket":"/run/cagentsd.sock"},"ok":false}"#,
            r#"{"command":"terminal.list","data":{"running":false,"socket":"/run/cagentsd.sock"},"ok":true}"#,
            "not JSON",
        ] {
            uninstall_fixture(script, status, false);
        }
    }
}
