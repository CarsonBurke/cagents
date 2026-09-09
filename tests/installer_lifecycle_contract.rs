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

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

// The fixture CLI records every destructive request, not merely its final
// status: a stop/start pair would otherwise look like preserved sessions.
fn upgrade_fixture(compatible: bool, terminals: usize, publish_failure: Option<&str>) {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("data");
    let prefix = data.join("cagents-install");
    let old_bin = prefix.join("versions/old/bin");
    fs::create_dir_all(&old_bin).unwrap();
    fs::write(
        prefix.join(".cagents-install-root"),
        "cagents install root\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("versions/old", prefix.join("current")).unwrap();
    let status = r#"#!/bin/sh
case "$*" in
  '--json daemon status')
    if [ -e "$FIXTURE_STOPPED" ]; then running=false; else running=true; fi
    printf '{"ok":true,"command":"daemon.status","data":{"running":%s}}\n' "$running"
    ;;
  'daemon stop --require-empty')
    printf '%s\n' require-empty >> "$FIXTURE_ACTIONS"
    [ "$FIXTURE_TERMINALS" = 0 ] || exit 3
    touch "$FIXTURE_STOPPED"
    ;;
  'daemon start') printf '%s\n' start >> "$FIXTURE_ACTIONS" ;;
  *) printf '%s\n' UNSAFE >> "$FIXTURE_ACTIONS"; exit 99 ;;
esac
"#;
    executable(&old_bin.join("cagents"), status);
    let archive_root = home.path().join("archive");
    let candidate = archive_root.join("cagents-9.9.9");
    fs::create_dir_all(candidate.join("bin")).unwrap();
    let candidate_cli = if compatible {
        status.to_string()
    } else {
        status.replace(
            "if [ -e \"$FIXTURE_STOPPED\" ]; then running=false; else running=true; fi",
            "[ -e \"$FIXTURE_STOPPED\" ] || exit 9\n    running=false",
        )
    };
    executable(&candidate.join("bin/cagents"), &candidate_cli);
    for binary in ["cagents-ui", "cagentsd", "cagents-uninstall"] {
        executable(&candidate.join("bin").join(binary), "#!/bin/sh\nexit 0\n");
    }
    fs::write(candidate.join("bin/.cagents-ui-lease-v1"), "").unwrap();
    for relative in [
        "share/applications/dev.cagents.Cagents.desktop",
        "share/autostart/dev.cagents.Cagents.Daemon.desktop",
        "share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg",
        "share/icons/hicolor/128x128/apps/dev.cagents.Cagents.png",
        "share/metainfo/dev.cagents.Cagents.metainfo.xml",
        "share/doc/cagents/LICENSE",
    ] {
        let file = candidate.join(relative);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "[Desktop Entry]\nExec=cagents ui\nIcon=cagents\n").unwrap();
    }
    let archive = home
        .path()
        .join("cagents-9.9.9-x86_64-unknown-linux-gnu.tar.gz");
    assert!(
        Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(&archive_root)
            .arg("cagents-9.9.9")
            .status()
            .unwrap()
            .success()
    );
    let hash = Command::new("sha256sum").arg(&archive).output().unwrap();
    assert!(hash.status.success());
    let hash = String::from_utf8(hash.stdout).unwrap();
    let checksums = home.path().join("SHA256SUMS");
    fs::write(
        &checksums,
        format!(
            "{}  {}\n",
            hash.split_whitespace().next().unwrap(),
            archive.file_name().unwrap().to_str().unwrap()
        ),
    )
    .unwrap();
    let tools = home.path().join("tools");
    fs::create_dir(&tools).unwrap();
    // Fail after current switches, exercising the real rollback trap.
    if let Some(failure) = publish_failure {
        let fail = if failure == "signal" {
            "kill -TERM \"$PPID\"; exit 8"
        } else {
            "exit 8"
        };
        executable(
            &tools.join("ln"),
            &format!(
                "#!/bin/sh\ncase \"$*\" in *'/applications/'*) {fail} ;; esac\nexec /usr/bin/ln \"$@\"\n"
            ),
        );
    }
    for tool in ["gtk-update-icon-cache", "update-desktop-database"] {
        executable(&tools.join(tool), "#!/bin/sh\nexit 0\n");
    }
    let actions = home.path().join("actions");
    let output = Command::new("sh")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("CAGENTS_ARCHIVE", &archive)
        .env("CAGENTS_CHECKSUMS", &checksums)
        .env("FIXTURE_ACTIONS", &actions)
        .env("FIXTURE_STOPPED", home.path().join("stopped"))
        .env("FIXTURE_TERMINALS", terminals.to_string())
        .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
        .output()
        .unwrap();
    let success = (compatible || terminals == 0) && publish_failure.is_none();
    assert_eq!(
        output.status.success(),
        success,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_link(prefix.join("current")).unwrap(),
        Path::new(if success {
            "versions/9.9.9"
        } else {
            "versions/old"
        })
    );
    let actions = fs::read_to_string(actions).unwrap_or_default();
    assert!(!actions.contains("UNSAFE"), "{actions}");
    if compatible {
        assert!(!actions.contains("require-empty"), "{actions}");
        assert!(!home.path().join("stopped").exists());
    } else {
        assert_eq!(
            actions
                .lines()
                .filter(|line| *line == "require-empty")
                .count(),
            1
        );
        assert_eq!(home.path().join("stopped").exists(), terminals == 0);
        assert_eq!(
            actions.lines().filter(|line| *line == "start").count(),
            usize::from(terminals == 0),
            "An empty daemon may be restarted once, never replayed: {actions}"
        );
    }
    if !success {
        assert!(!prefix.join("versions/9.9.9").exists());
    }
}

#[test]
fn compatible_switch_preserves_terminals_without_any_shutdown_request() {
    for count in [0, 1, 7] {
        upgrade_fixture(true, count, None);
    }
}

#[test]
fn incompatible_switch_only_stops_an_atomically_empty_daemon() {
    for count in [0, 1, 7] {
        upgrade_fixture(false, count, None);
    }
}

#[test]
fn failed_compatible_switch_rolls_back_without_stopping_live_sessions() {
    upgrade_fixture(true, 2, Some("error"));
}

#[test]
fn interrupted_switch_preserves_sessions_and_rolls_back_version() {
    upgrade_fixture(true, 2, Some("signal"));
}

#[test]
fn incompatible_empty_switch_failure_restarts_previous_version_once() {
    upgrade_fixture(false, 0, Some("error"));
}
