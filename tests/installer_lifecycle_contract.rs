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
fn upgrade_fixture(
    compatible: bool,
    terminals: usize,
    publish_failure: Option<&str>,
    restart: Option<&str>,
) {
    release_fixture(compatible, terminals, publish_failure, restart, None);
}

fn release_fixture(
    compatible: bool,
    terminals: usize,
    publish_failure: Option<&str>,
    restart: Option<&str>,
    download: Option<&str>,
) {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("data with spaces");
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
CLI=old
case "$*" in
  'coordinate-install '*|'verify-install-guard')
    exec "$FIXTURE_REAL_CLI" "$@"
    ;;
  '--json daemon status')
    [ "$#" -eq 3 ] || exit 98
    if [ "$CLI" = new ] && { [ "$FIXTURE_RESTART" = start-unknown ] || [ "$FIXTURE_RESTART" = empty-start-unknown ]; }; then exit 9; fi
    if [ "$CLI" = new ] && [ "$FIXTURE_COMPATIBLE" = false ] &&
       [ "$(cat "$FIXTURE_RUNNING" 2>/dev/null)" = old ]; then exit 9; fi
    if [ -e "$FIXTURE_RUNNING" ]; then running=true; else running=false; fi
    if [ -e "$FIXTURE_PENDING" ]; then
      [ "$CLI" = old ] || exit 98
      printf '%s\n' old:draining >> "$FIXTURE_ACTIONS"
      rm "$FIXTURE_PENDING" "$FIXTURE_RUNNING"
    elif [ "$running" = false ] && [ -e "$FIXTURE_CHECKPOINT" ]; then
      printf '%s:stopped\n' "$CLI" >> "$FIXTURE_ACTIONS"
    fi
    printf '{"ok":true,"command":"daemon.status","data":{"running":%s}}\n' "$running"
    ;;
  'daemon --help')
    [ "$#" -eq 2 ] || exit 98
    printf '%s:help\n' "$CLI" >> "$FIXTURE_ACTIONS"
    if [ "$FIXTURE_RESTART" = legacy ]; then
      printf 'Commands:\n  prepare-upgrade-old  Not the checkpoint command\n'
      exit 0
    fi
    printf 'Commands:\n  prepare-upgrade  Checkpoint supported sessions\n'
    ;;
  'daemon prepare-upgrade --help')
    [ "$FIXTURE_RESTART" != relaunch-unsupported ] || exit 0
    printf 'Options:\n  --relaunch-agents  Accept fresh launches\n'
    [ "$FIXTURE_RESTART" != decline ] || printf '  --no-relaunch-agents  Refuse instead\n'
    ;;
  '--json daemon prepare-upgrade --yes'|'--json daemon prepare-upgrade --yes --relaunch-agents'|'--json daemon prepare-upgrade --yes --no-relaunch-agents')
    if [ "$FIXTURE_RESTART" = relaunch ]; then
      [ "$#" -eq 5 ] && [ "$5" = --relaunch-agents ] || exit 98
    elif [ "$FIXTURE_RESTART" = decline ]; then
      [ "$#" -eq 5 ] && [ "$5" = --no-relaunch-agents ] || exit 98
    else
      [ "$#" -eq 4 ] || exit 98
    fi
    printf '%s:prepare\n' "$CLI" >> "$FIXTURE_ACTIONS"
    [ "$CLI" = old ] || exit 98
    [ "$FIXTURE_RESTART" != prepare-failure ] || exit 3
    [ "$(readlink "$FIXTURE_PREFIX/current")" = versions/old ] || exit 98
    printf '%s\n' exact-conversation-and-shells > "$FIXTURE_CHECKPOINT"
    touch "$FIXTURE_PENDING"
    printf '{"ok":true,"command":"daemon.prepare_upgrade","data":{"terminals":%s,"chats":1}}\n' "$FIXTURE_TERMINALS"
    ;;
  'daemon stop --require-empty')
    [ "$#" -eq 3 ] || exit 98
    printf '%s\n' require-empty >> "$FIXTURE_ACTIONS"
    [ "$FIXTURE_TERMINALS" = 0 ] || exit 3
    rm "$FIXTURE_RUNNING"
    ;;
  'daemon start')
    [ "$#" -eq 2 ] || exit 98
    printf '%s:start\n' "$CLI" >> "$FIXTURE_ACTIONS"
    if [ "$CLI" = new ] && { [ "$FIXTURE_RESTART" = start-failure ] || [ "$FIXTURE_RESTART" = empty-start-failure ]; }; then exit 4; fi
    if [ -e "$FIXTURE_CHECKPOINT" ]; then
      [ ! -e "$FIXTURE_RUNNING" ] && [ ! -e "$FIXTURE_PENDING" ] || exit 98
      expected=versions/old
      [ "$CLI" = old ] || expected=versions/9.9.9
      [ "$(readlink "$FIXTURE_PREFIX/current")" = "$expected" ] || exit 98
      cp "$FIXTURE_CHECKPOINT" "$FIXTURE_RESTORED"
    fi
    if [ ! -e "$FIXTURE_RUNNING" ]; then printf '%s\n' "$CLI" > "$FIXTURE_RUNNING"; fi
    if [ "$CLI" = new ] && { [ "$FIXTURE_RESTART" = start-unknown ] || [ "$FIXTURE_RESTART" = empty-start-unknown ]; }; then exit 4; fi
    if [ "$CLI" = new ] && [ "$FIXTURE_RESTART" = start-signal ]; then
      kill -TERM "$PPID"
      exit 4
    fi
    ;;
  *) printf '%s\n' UNSAFE >> "$FIXTURE_ACTIONS"; exit 99 ;;
esac
"#;
    executable(&old_bin.join("cagents"), status);
    let archive_root = home.path().join("archive");
    let candidate = archive_root.join("cagents-9.9.9");
    fs::create_dir_all(candidate.join("bin")).unwrap();
    let candidate_cli = status.replace("CLI=old", "CLI=new");
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
    let downloads = home.path().join("downloads");
    executable(
        &tools.join("gh"),
        r#"#!/bin/sh
set -eu
case "$1 $2" in
  'auth status')
    [ "$#" -eq 4 ] && [ "$3" = --hostname ] && [ "$4" = github.com ] || exit 98
    printf '%s\n' gh:auth >> "$FIXTURE_DOWNLOADS"
    [ "$FIXTURE_DOWNLOAD" != public ] && [ "$FIXTURE_DOWNLOAD" != auth-failure ]
    ;;
  'release view')
    [ "$#" -eq 8 ] && [ "$3" = --repo ] && [ "$4" = github.com/CarsonBurke/cagents ] &&
      [ "$5" = --json ] && [ "$6" = tagName ] && [ "$7" = --jq ] && [ "$8" = .tagName ] || exit 98
    printf '%s\n' gh:latest >> "$FIXTURE_DOWNLOADS"
    printf '%s\n' v9.9.9
    ;;
  'release download')
    [ "$#" -eq 9 ] && [ "$3" = v9.9.9 ] && [ "$4" = --repo ] &&
      [ "$5" = github.com/CarsonBurke/cagents ] && [ "$6" = --pattern ] &&
      [ "$8" = --output ] || exit 98
    printf 'gh:%s\n' "$7" >> "$FIXTURE_DOWNLOADS"
    [ "$FIXTURE_DOWNLOAD" != gh-failure ] || exit 4
    case "$7" in
      cagents-9.9.9-x86_64-unknown-linux-gnu.tar.gz) cp "$FIXTURE_ARCHIVE" "$9" ;;
      SHA256SUMS) cp "$FIXTURE_CHECKSUMS" "$9" ;;
      *) exit 98 ;;
    esac
    ;;
  *) exit 98 ;;
esac
"#,
    );
    executable(
        &tools.join("curl"),
        r#"#!/bin/sh
set -eu
[ "$FIXTURE_DOWNLOAD" = public ] || exit 98
destination=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) destination=$2; shift 2 ;;
    --proto|-w) shift 2 ;;
    --tlsv1.2|--fail|--location|--silent|--show-error) shift ;;
    https://github.com/fixture/public-cagents/*) url=$1; shift ;;
    *) exit 98 ;;
  esac
done
case "$url" in
  */releases/latest)
    printf '%s\n' curl:latest >> "$FIXTURE_DOWNLOADS"
    printf '%s' https://github.com/fixture/public-cagents/releases/tag/v9.9.9
    ;;
  */releases/download/v9.9.9/cagents-9.9.9-x86_64-unknown-linux-gnu.tar.gz)
    printf '%s\n' curl:archive >> "$FIXTURE_DOWNLOADS"
    cp "$FIXTURE_ARCHIVE" "$destination"
    ;;
  */releases/download/v9.9.9/SHA256SUMS)
    printf '%s\n' curl:checksums >> "$FIXTURE_DOWNLOADS"
    cp "$FIXTURE_CHECKSUMS" "$destination"
    ;;
  *) exit 98 ;;
esac
"#,
    );
    let actions = home.path().join("actions");
    let running = home.path().join("running");
    let checkpoint = home.path().join("checkpoint");
    let restored = home.path().join("restored");
    fs::write(&running, "old\n").unwrap();
    let runtime = home.path().join("runtime");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = Command::new("sh");
    command.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"));
    if restart.is_some() {
        command.arg("--restart-daemon");
    }
    if matches!(
        restart,
        Some("empty" | "empty-start-failure" | "empty-start-unknown")
    ) {
        command.arg("--require-empty-daemon");
    }
    if matches!(restart, Some("relaunch" | "relaunch-unsupported")) {
        command.arg("--relaunch-agents");
    }
    if matches!(restart, Some("decline" | "decline-legacy")) {
        command.arg("--no-relaunch-agents");
    }
    if download.is_some() {
        command
            .env_remove("CAGENTS_ARCHIVE")
            .env_remove("CAGENTS_CHECKSUMS");
        if download == Some("gh-pinned") {
            command.args(["--version", "9.9.9"]);
        }
    } else {
        command
            .env("CAGENTS_ARCHIVE", &archive)
            .env("CAGENTS_CHECKSUMS", &checksums);
    }
    let output = command
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_RUNTIME_DIR", &runtime)
        .env_remove("CAGENTS_UI_UPDATE_GUARD_FD")
        .env("FIXTURE_REAL_CLI", env!("CARGO_BIN_EXE_cagents"))
        .env(
            "CAGENTS_REPOSITORY",
            if download == Some("public") {
                "fixture/public-cagents"
            } else {
                "CarsonBurke/cagents"
            },
        )
        .env("FIXTURE_DOWNLOADS", &downloads)
        .env("FIXTURE_DOWNLOAD", download.unwrap_or_default())
        .env("FIXTURE_ARCHIVE", &archive)
        .env("FIXTURE_CHECKSUMS", &checksums)
        .env("FIXTURE_ACTIONS", &actions)
        .env("FIXTURE_RUNNING", &running)
        .env("FIXTURE_PENDING", home.path().join("pending"))
        .env("FIXTURE_CHECKPOINT", &checkpoint)
        .env("FIXTURE_RESTORED", &restored)
        .env("FIXTURE_PREFIX", &prefix)
        .env("FIXTURE_COMPATIBLE", compatible.to_string())
        .env("FIXTURE_RESTART", restart.unwrap_or_default())
        .env("FIXTURE_TERMINALS", terminals.to_string())
        .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
        .output()
        .unwrap();
    let success = !matches!(download, Some("gh-failure" | "auth-failure"))
        && match restart {
            Some("success" | "relaunch" | "decline" | "decline-legacy") => {
                publish_failure.is_none()
            }
            Some("empty") => terminals == 0 && publish_failure.is_none(),
            Some(_) => false,
            None => (compatible || terminals == 0) && publish_failure.is_none(),
        };
    assert_eq!(
        output.status.success(),
        success,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let retained_candidate = success
        || matches!(
            restart,
            Some("start-unknown" | "start-signal" | "empty-start-unknown")
        );
    assert_eq!(
        fs::read_link(prefix.join("current")).unwrap(),
        Path::new(if retained_candidate {
            "versions/9.9.9"
        } else {
            "versions/old"
        })
    );
    let actions = fs::read_to_string(actions).unwrap_or_default();
    assert!(!actions.contains("UNSAFE"), "{actions}");
    if restart == Some("empty-start-unknown") {
        assert_eq!(actions, "require-empty\nnew:start\n");
        assert!(!checkpoint.exists());
        assert!(!restored.exists());
    } else if restart == Some("empty-start-failure") {
        assert_eq!(actions, "require-empty\nnew:start\nold:start\n");
        assert!(!checkpoint.exists());
        assert!(!restored.exists());
    } else if restart == Some("empty") {
        assert_eq!(
            actions,
            if terminals == 0 {
                "require-empty\nnew:start\n"
            } else {
                "require-empty\n"
            }
        );
        assert!(!checkpoint.exists());
        assert!(!restored.exists());
    } else if let Some(restart) = restart {
        assert!(!actions.contains("require-empty"), "{actions}");
        let expected = match (restart, publish_failure) {
            ("legacy" | "relaunch-unsupported", _) => "old:help\n",
            ("prepare-failure", _) => "old:help\nold:prepare\n",
            ("start-failure", _) => {
                "old:help\nold:prepare\nold:draining\nold:stopped\nnew:start\nnew:stopped\nold:start\n"
            }
            ("success" | "relaunch" | "decline" | "decline-legacy", None) => {
                "old:help\nold:prepare\nold:draining\nold:stopped\nnew:start\n"
            }
            ("success", Some(_)) => "old:help\nold:prepare\nold:draining\nold:stopped\nold:start\n",
            ("start-unknown" | "start-signal", _) => {
                "old:help\nold:prepare\nold:draining\nold:stopped\nnew:start\n"
            }
            _ => panic!("unknown fixture restart mode"),
        };
        assert_eq!(actions, expected);
        let prepared = !matches!(
            restart,
            "legacy" | "prepare-failure" | "relaunch-unsupported"
        );
        assert_eq!(checkpoint.exists(), prepared);
        assert_eq!(restored.exists(), prepared);
        if prepared {
            assert_eq!(
                fs::read(&restored).unwrap(),
                b"exact-conversation-and-shells\n"
            );
            assert_eq!(fs::read(&checkpoint).unwrap(), fs::read(&restored).unwrap());
        }
    } else if compatible {
        assert!(!actions.contains("require-empty"), "{actions}");
        assert!(!actions.contains("prepare"), "{actions}");
        assert!(!checkpoint.exists());
    } else {
        assert_eq!(
            actions
                .lines()
                .filter(|line| *line == "require-empty")
                .count(),
            1
        );
        assert!(!checkpoint.exists());
        assert_eq!(
            actions
                .lines()
                .filter(|line| line.ends_with(":start"))
                .count(),
            usize::from(terminals == 0),
            "An empty daemon may be restarted once, never replayed: {actions}"
        );
    }
    assert_eq!(
        fs::read_to_string(&running).unwrap(),
        if retained_candidate && (restart.is_some() || !compatible) {
            "new\n"
        } else {
            "old\n"
        }
    );
    if !retained_candidate {
        assert!(!prefix.join("versions/9.9.9").exists());
    }
    assert_eq!(
        fs::read_to_string(downloads).unwrap_or_default(),
        match download {
            None => "",
            Some("gh-latest") =>
                "gh:auth\ngh:latest\ngh:cagents-9.9.9-x86_64-unknown-linux-gnu.tar.gz\ngh:SHA256SUMS\n",
            Some("gh-pinned") =>
                "gh:auth\ngh:cagents-9.9.9-x86_64-unknown-linux-gnu.tar.gz\ngh:SHA256SUMS\n",
            Some("gh-failure") =>
                "gh:auth\ngh:latest\ngh:cagents-9.9.9-x86_64-unknown-linux-gnu.tar.gz\n",
            Some("auth-failure") => "gh:auth\n",
            Some("public") => "gh:auth\ncurl:latest\ncurl:archive\ncurl:checksums\n",
            Some(_) => panic!("unknown fixture download mode"),
        }
    );
}

#[test]
fn an_unconfirmed_update_cannot_interrupt_sessions_created_after_preflight() {
    for count in [0, 1, 7] {
        upgrade_fixture(true, count, None, Some("empty"));
    }
    upgrade_fixture(true, 0, None, Some("empty-start-failure"));
    upgrade_fixture(true, 0, None, Some("empty-start-unknown"));
}

#[test]
fn compatible_switch_preserves_terminals_without_any_shutdown_request() {
    for count in [0, 1, 7] {
        upgrade_fixture(true, count, None, None);
    }
}

#[test]
fn incompatible_switch_only_stops_an_atomically_empty_daemon() {
    for count in [0, 1, 7] {
        upgrade_fixture(false, count, None, None);
    }
}

#[test]
fn failed_compatible_switch_rolls_back_without_stopping_live_sessions() {
    upgrade_fixture(true, 2, Some("error"), None);
}

#[test]
fn interrupted_switch_preserves_sessions_and_rolls_back_version() {
    upgrade_fixture(true, 2, Some("signal"), None);
}

#[test]
fn incompatible_empty_switch_failure_restarts_previous_version_once() {
    upgrade_fixture(false, 0, Some("error"), None);
}

#[test]
fn explicit_restart_checkpoints_before_replacing_even_a_compatible_daemon() {
    upgrade_fixture(true, 7, None, Some("success"));
    upgrade_fixture(false, 7, None, Some("success"));
}

#[test]
fn failed_checkpoint_preparation_preserves_old_installation_and_sessions() {
    upgrade_fixture(true, 7, None, Some("prepare-failure"));
}

#[test]
fn legacy_restart_refuses_without_checkpoint_or_shutdown() {
    upgrade_fixture(false, 7, None, Some("legacy"));
}

#[test]
fn installer_passes_relaunch_acceptance_only_to_a_supporting_installed_cli() {
    upgrade_fixture(false, 7, None, Some("relaunch"));
    upgrade_fixture(false, 7, None, Some("relaunch-unsupported"));
}

/// Declining is passed on where the installed CLI knows the choice, and is
/// already what an older CLI does when nothing is passed.
#[test]
fn installer_passes_a_declined_relaunch_only_to_a_cli_that_would_otherwise_accept() {
    upgrade_fixture(false, 7, None, Some("decline"));
    upgrade_fixture(false, 7, None, Some("decline-legacy"));
}

#[test]
fn checkpointed_publication_failure_restarts_previous_version_from_handoff() {
    upgrade_fixture(false, 7, Some("error"), Some("success"));
}

#[test]
fn checkpointed_publication_interruption_restarts_previous_version_from_handoff() {
    upgrade_fixture(true, 7, Some("signal"), Some("success"));
}

#[test]
fn checkpointed_start_failure_restarts_previous_version_from_handoff() {
    upgrade_fixture(false, 7, None, Some("start-failure"));
}

#[test]
fn ambiguous_candidate_start_preserves_replacement_and_never_restarts_old_writer() {
    upgrade_fixture(false, 7, None, Some("start-unknown"));
}

#[test]
fn interrupted_candidate_start_preserves_replacement_writer() {
    upgrade_fixture(false, 7, None, Some("start-signal"));
}

#[test]
fn restart_option_rejects_uninstall_and_extra_values_without_creating_installation() {
    let home = tempfile::tempdir().unwrap();
    for arguments in [
        vec!["--restart-daemon", "--uninstall"],
        vec!["--purge", "--restart-daemon"],
        vec!["--restart-daemon=yes"],
        vec!["--restart-daemon", "yes"],
        vec!["--relaunch-agents"],
        vec!["--no-relaunch-agents"],
    ] {
        let output = Command::new("sh")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
            .args(arguments)
            .env("HOME", home.path())
            .env("XDG_DATA_HOME", home.path().join("data"))
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!home.path().join("data").exists());
        assert!(!home.path().join("config").exists());
    }
}

#[test]
fn authenticated_private_release_resolves_and_downloads_with_existing_gh_account() {
    release_fixture(true, 7, None, None, Some("gh-latest"));
}

#[test]
fn authenticated_pinned_release_downloads_only_requested_tag() {
    release_fixture(true, 7, None, None, Some("gh-pinned"));
}

#[test]
fn authenticated_download_failure_preserves_installation_without_public_fallback() {
    release_fixture(true, 7, None, None, Some("gh-failure"));
}

#[test]
fn public_release_download_works_without_an_authenticated_gh_account() {
    release_fixture(true, 7, None, None, Some("public"));
}

#[test]
fn private_authentication_failure_preserves_installation_without_anonymous_downloads() {
    release_fixture(true, 7, None, None, Some("auth-failure"));
}
