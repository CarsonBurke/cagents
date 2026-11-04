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

#[test]
fn installer_entrypoints_refuse_inherited_callers_before_tools_or_purge() {
    use cagents::process::{
        CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor,
    };
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    for role in [
        OsString::from("inert-fixture-role"),
        OsString::new(),
        OsString::from_vec(vec![0xff]),
    ] {
        for script in ["install.sh", "packaging/cagents-uninstall"] {
            let root = tempfile::tempdir().unwrap();
            for leaf in [
                "bin",
                "home",
                "data/cagents",
                "config/cagents",
                "cache/cagents",
            ] {
                fs::create_dir_all(root.path().join(leaf)).unwrap();
            }
            for leaf in ["data", "config", "cache"] {
                fs::write(
                    root.path().join(leaf).join("cagents/saved"),
                    b"private fixture",
                )
                .unwrap();
            }
            for name in ["id", "uname", "getconf", "mktemp", "curl", "wget", "rm"] {
                executable(
                    &root.path().join("bin").join(name),
                    "#!/bin/sh\nprintf invoked > \"$HOME/../tool-invoked\"\nexit 91\n",
                );
            }
            let command = || {
                CommandSpec::new("/bin/sh")
                    .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(script))
                    .env("CAGENTS_AGENT_AUTHORITY", &role)
                    .env("HOME", root.path().join("home"))
                    .env("XDG_DATA_HOME", root.path().join("data"))
                    .env("XDG_CONFIG_HOME", root.path().join("config"))
                    .env("XDG_CACHE_HOME", root.path().join("cache"))
                    .env("PATH", root.path().join("bin"))
                    .cwd(root.path())
                    .timeout(std::time::Duration::from_secs(10))
                    .output_limits(64 * 1024, 64 * 1024)
            };
            let options = if script == "install.sh" {
                vec![
                    vec![],
                    vec!["--stage-only"],
                    vec!["--uninstall"],
                    vec!["--purge"],
                ]
            } else {
                vec![vec![], vec!["--purge"]]
            };
            for args in options {
                let output = SystemProcessExecutor
                    .execute(&command().args(args), &CancellationToken::new())
                    .unwrap();
                assert_eq!(output.exit_code, Some(1), "{script}: {output:?}");
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("require your own terminal"),
                    "{script}: {output:?}"
                );
                assert!(output.stdout.is_empty());
                assert!(!root.path().join("tool-invoked").exists());
                for leaf in ["data", "config", "cache"] {
                    assert_eq!(
                        fs::read(root.path().join(leaf).join("cagents/saved")).unwrap(),
                        b"private fixture"
                    );
                }
            }
            let output = SystemProcessExecutor
                .execute(
                    &command().arg("--help").env("PATH", "/usr/bin:/bin"),
                    &CancellationToken::new(),
                )
                .unwrap();
            assert!(output.success, "{script}: {output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).starts_with("Usage:"));
            assert!(!root.path().join("tool-invoked").exists());
        }
    }
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

// The CLI-only lane has no built host. Keep this process-boundary fixture
// beside a private copy of the actual coordinator, as in installer_guard_contract.
// It checks the real inherited fence and request, not host SQLite/provider panes.
fn install_source_inspector_fixture(path: &Path) {
    executable(
        path,
        r#"#!/usr/bin/python3
import json, os, pathlib, subprocess, sys
assert len(sys.argv) == 3 and sys.argv[1] == '--inspect-upgrade-source'
request=json.loads(sys.argv[2])
assert request == {'protocol':int(os.environ['FIXTURE_SOURCE_PROTOCOL']),
    'proof':None, 'require_resumable':os.environ['FIXTURE_REQUIRE_RESUMABLE']=='true',
    'reconcile_retained':False}
assert not any(key in os.environ for key in ['CAGENTS_INSTALL_RESULT_FD',
    'CAGENTS_INSTALL_OWNER_CLI', 'CAGENTS_ARCHIVE', 'CAGENTS_CHECKSUMS', 'CAGENTS_REPOSITORY'])
fd=int(os.environ['CAGENTS_UI_UPDATE_GUARD_FD'])
verified=subprocess.run([os.environ['FIXTURE_REAL_CLI'], 'verify-install-guard', '--stage-only'],
    pass_fds=(fd,), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
if verified.returncode:
    sys.stderr.buffer.write(verified.stderr)
    sys.exit(91)
with (pathlib.Path(os.environ['HOME'])/'source-inspected').open('a') as inspected:
    inspected.write(json.dumps(request)+'\n')
print(json.dumps({'Ok':None}))
"#,
    );
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
    coordinated_release_fixture(
        compatible,
        terminals,
        publish_failure,
        restart,
        download,
        "installed",
    );
}

fn coordinated_release_fixture(
    compatible: bool,
    terminals: usize,
    publish_failure: Option<&str>,
    restart: Option<&str>,
    download: Option<&str>,
    coordinator: &str,
) {
    coordinated_release_mode(
        compatible,
        terminals,
        publish_failure,
        restart,
        download,
        coordinator,
        false,
    );
}

fn coordinated_release_mode(
    compatible: bool,
    terminals: usize,
    publish_failure: Option<&str>,
    restart: Option<&str>,
    download: Option<&str>,
    coordinator: &str,
    stage_only: bool,
) {
    let home = tempfile::tempdir().unwrap();
    let coordinator_bin = home.path().join("coordinator-bin");
    fs::create_dir(&coordinator_bin).unwrap();
    let real_cli = coordinator_bin.join("cagents");
    fs::copy(env!("CARGO_BIN_EXE_cagents"), &real_cli).unwrap();
    install_source_inspector_fixture(&coordinator_bin.join("cagentsd"));
    let data = home.path().join("data with spaces");
    let prefix = data.join("cagents-install");
    let old_bin = prefix.join("versions/old/bin");
    fs::create_dir_all(&old_bin).unwrap();
    fs::write(
        prefix.join(".cagents-install-root"),
        "cagents install root\n",
    )
    .unwrap();
    if coordinator != "initial-independent" {
        std::os::unix::fs::symlink("versions/old", prefix.join("current")).unwrap();
    }
    let status = r#"#!/bin/sh
CLI=old
case "$*" in
  'coordinate-install '*|'verify-install-guard'*|'record-deferred-install'*)
    case "$FIXTURE_COORDINATOR:$CLI" in
      installed:new) [ "$FIXTURE_STAGE_ONLY" = true ] || exit 64 ;;
      candidate:old|none:*|stage-unsupported:new) exit 64 ;;
    esac
    if [ "$FIXTURE_COORDINATOR:$CLI" = legacy:old ] && [ "$2" = --help ]; then
      "$FIXTURE_REAL_CLI" "$@" | sed '/--stage-only/d'
      exit 0
    fi
    if [ "$1" = coordinate-install ] && [ "$2" != --help ]; then
      printf '%s\n' "$CLI" >> "$FIXTURE_COORDINATOR_USED"
    fi
    if [ "$1" = record-deferred-install ] && [ "$2" != --help ] && [ "$FIXTURE_PRE_CAS_CHANGE" = true ]; then
      "$FIXTURE_REAL_CLI" "$@" || exit "$?"
      /usr/bin/ln -snf versions/9.9.9 "$FIXTURE_PREFIX/current"
      exit 0
    fi
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
    if [ "$FIXTURE_COORDINATOR:$CLI" = starting:new ]; then
      printf '{"ok":true,"command":"daemon.status","data":{"running":%s,"daemon_version":null}}\n' "$running"
    else
      printf '{"ok":true,"command":"daemon.status","data":{"running":%s,"daemon_version":"fixture"}}\n' "$running"
    fi
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
    if let Some(failure) = publish_failure.filter(|failure| *failure != "pre-cas-candidate") {
        let fail = if failure == "current-changed" {
            "/usr/bin/ln -snf versions/independent \"$FIXTURE_PREFIX/current\"; exit 8"
        } else if matches!(failure, "outer-signal" | "outer-signal-invalid-target") {
            r#"target=$(cat "$FIXTURE_OUTER_PID_FILE") || exit 97
if [ "$FIXTURE_REJECT_SIGNAL" = true ]; then target=not-a-pid; fi
# Successful delivery makes the outer installer cancel this tool's group.
# Keep that TERM from interrupting the builtin send-and-record section.
trap '' TERM
if ! kill -TERM "$target"; then
  printf '%s\n' "$target" > "$FIXTURE_SIGNAL_REJECTED"
  exit 97
fi
printf '%s\n' "$target" > "$FIXTURE_SIGNAL_DELIVERED" || exit 97
trap - TERM
# A lost outer cancellation must allow publication to succeed. An unrelated
# exit failure here would also roll back and hide broken signal forwarding.
sleep 1"#
        } else if failure == "signal" {
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
    let outer_signal = matches!(
        publish_failure,
        Some("outer-signal" | "outer-signal-invalid-target")
    );
    let mut command = Command::new("sh");
    if outer_signal {
        // Publish from the child before the installer can reach the signal
        // fixture. exec preserves this PID as the actual outer installer.
        command.args([
            "-c",
            r#"printf '%s\n' "$$" > "$FIXTURE_OUTER_PID_FILE" || exit 97
exec sh "$@""#,
            "installer-signal-fixture",
        ]);
    }
    command.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"));
    if stage_only {
        command.arg("--stage-only");
    }
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
        .env("FIXTURE_REAL_CLI", &real_cli)
        .env(
            "FIXTURE_SOURCE_PROTOCOL",
            cagents::terminal_ipc::PROTOCOL_VERSION.to_string(),
        )
        .env("FIXTURE_REQUIRE_RESUMABLE", restart.is_some().to_string())
        .env("FIXTURE_COORDINATOR", coordinator)
        .env("FIXTURE_STAGE_ONLY", stage_only.to_string())
        .env(
            "FIXTURE_PRE_CAS_CHANGE",
            (publish_failure == Some("pre-cas-candidate")).to_string(),
        )
        .env(
            "FIXTURE_COORDINATOR_USED",
            home.path().join("coordinator-used"),
        )
        .env("FIXTURE_OUTER_PID_FILE", home.path().join("outer-pid"))
        .env(
            "FIXTURE_SIGNAL_DELIVERED",
            home.path().join("signal-delivered"),
        )
        .env(
            "FIXTURE_SIGNAL_REJECTED",
            home.path().join("signal-rejected"),
        )
        .env(
            "FIXTURE_REJECT_SIGNAL",
            (publish_failure == Some("outer-signal-invalid-target")).to_string(),
        )
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
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let outer_pid = output.id();
    let output = output.wait_with_output().unwrap();
    let source_inspected = home.path().join("source-inspected");
    if stage_only || !home.path().join("coordinator-used").exists() {
        assert!(
            !source_inspected.exists(),
            "staging and pre-coordinator refusals must not inspect source panes"
        );
    } else {
        let inspections = fs::read_to_string(source_inspected).unwrap_or_else(|error| {
            panic!("source inspector was not reached: {error}; coordinator output: {output:?}")
        });
        assert_eq!(inspections.lines().count(), 1, "{inspections}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(inspections.trim()).unwrap(),
            serde_json::json!({
                "protocol": cagents::terminal_ipc::PROTOCOL_VERSION,
                "proof": null,
                "require_resumable": restart.is_some(),
                "reconcile_retained": false,
            })
        );
    }
    if outer_signal {
        assert_eq!(
            fs::read_to_string(home.path().join("outer-pid")).unwrap(),
            format!("{outer_pid}\n")
        );
        if publish_failure == Some("outer-signal-invalid-target") {
            assert_eq!(
                fs::read_to_string(home.path().join("signal-delivered"))
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::NotFound,
                "a rejected injection must never count as delivered"
            );
            assert_eq!(
                fs::read_to_string(home.path().join("signal-rejected")).unwrap(),
                "not-a-pid\n"
            );
        } else {
            assert_eq!(
                fs::read_to_string(home.path().join("signal-delivered"))
                    .expect("outer interruption requires successful signal delivery"),
                format!("{outer_pid}\n")
            );
            assert!(!home.path().join("signal-rejected").exists());
        }
    }
    if coordinator == "none" || coordinator == "stage-unsupported" {
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(if coordinator == "none" {
                "install a current release first"
            } else {
                "this release does not support safe staging"
            })
        );
        assert_eq!(
            fs::read_link(prefix.join("current")).unwrap(),
            Path::new("versions/old")
        );
        assert!(!prefix.join("versions/9.9.9").exists());
        assert!(fs::read_to_string(actions).unwrap_or_default().is_empty());
        return;
    }
    let success = !matches!(download, Some("gh-failure" | "auth-failure"))
        && match restart {
            Some("success" | "relaunch" | "decline" | "decline-legacy") => {
                publish_failure.is_none()
            }
            Some("empty") => terminals == 0 && publish_failure.is_none(),
            Some(_) => false,
            None => publish_failure.is_none(),
        };
    assert_eq!(
        output.status.success(),
        success,
        "coordinator={coordinator} stage={stage_only} failure={publish_failure:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let retained_candidate = success
        || matches!(
            publish_failure,
            Some("current-changed" | "pre-cas-candidate")
        )
        || matches!(
            restart,
            Some("start-unknown" | "start-signal" | "empty-start-unknown")
        );
    assert_eq!(
        fs::read_link(prefix.join("current")).unwrap(),
        Path::new(if publish_failure == Some("current-changed") {
            "versions/independent"
        } else if retained_candidate {
            "versions/9.9.9"
        } else {
            "versions/old"
        })
    );
    let actions = fs::read_to_string(actions).unwrap_or_default();
    if coordinator == "starting" {
        assert!(
            actions.is_empty(),
            "an unconfirmed startup lock must not authorize daemon replacement"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("running sessions were preserved")
        );
    }
    assert!(!actions.contains("UNSAFE"), "{actions}");
    if stage_only {
        assert_eq!(
            fs::read_to_string(home.path().join("coordinator-used")).unwrap(),
            "new\n"
        );
        assert!(
            actions.is_empty(),
            "staging must never call daemon control: {actions}"
        );
        assert!(!checkpoint.exists());
        assert!(!restored.exists());
        if matches!(
            publish_failure,
            Some("current-changed" | "pre-cas-candidate")
        ) {
            assert!(prefix.join("versions/old").exists());
            if publish_failure == Some("current-changed") {
                assert!(
                    home.path()
                        .join(".local/bin/cagents")
                        .symlink_metadata()
                        .is_ok(),
                    "uncertain rollback must retain created shared launchers"
                );
            }
            assert!(prefix.join("versions/9.9.9/.deferred-owner.json").exists());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("retained both version trees")
            );
        }
        if success {
            let marker: serde_json::Value = serde_json::from_slice(
                &fs::read(prefix.join("versions/9.9.9/.deferred-owner.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(marker["version"], 1);
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .contains("running windows and daemon were preserved")
            );
        }
    } else if restart == Some("empty-start-unknown") {
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
        assert!(
            actions.is_empty(),
            "deferred updates must not stop/start/checkpoint: {actions}"
        );
        assert!(!checkpoint.exists());
        if success {
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("running sessions were preserved")
            );
        }
    }
    assert_eq!(
        fs::read_to_string(&running).unwrap(),
        if retained_candidate && restart.is_some() {
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
fn candidate_coordinator_supports_upgrading_an_older_installed_cli() {
    coordinated_release_fixture(true, 2, None, None, None, "candidate");
}

#[test]
fn candidate_startup_lock_without_handshake_defers_to_the_verified_owner() {
    coordinated_release_fixture(true, 2, None, None, None, "starting");
}

#[test]
fn unsupported_coordinators_refuse_before_mutating_the_installation() {
    coordinated_release_fixture(true, 2, None, None, None, "none");
}

#[test]
fn interrupting_the_outer_installer_waits_for_inner_rollback() {
    coordinated_release_fixture(true, 2, Some("outer-signal"), None, None, "installed");
}

#[test]
fn rejected_outer_signal_injection_cannot_count_as_delivered() {
    coordinated_release_fixture(
        true,
        2,
        Some("outer-signal-invalid-target"),
        None,
        None,
        "installed",
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
fn incompatible_install_defers_daemon_replacement_without_stopping_sessions() {
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

#[test]
fn staging_preserves_daemons_and_rolls_back_publication_without_control_calls() {
    for compatible in [true, false] {
        for failure in [None, Some("publish"), Some("signal"), Some("outer-signal")] {
            for coordinator in ["installed", "candidate", "legacy"] {
                coordinated_release_mode(compatible, 38, failure, None, None, coordinator, true);
            }
        }
    }
}

#[test]
fn staging_rejects_destructive_options_before_touching_installation() {
    for option in [
        "--restart-daemon",
        "--require-empty-daemon",
        "--relaunch-agents",
        "--no-relaunch-agents",
        "--uninstall",
        "--purge",
    ] {
        let root = tempfile::tempdir().unwrap();
        let output = Command::new("sh")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
            .args(["--stage-only", option])
            .env("HOME", root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--stage-only cannot be combined")
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

#[test]
fn staging_keeps_both_trees_when_current_cannot_be_safely_rolled_back() {
    coordinated_release_mode(
        false,
        38,
        Some("current-changed"),
        None,
        None,
        "initial-independent",
        true,
    );
    coordinated_release_mode(
        false,
        38,
        Some("pre-cas-candidate"),
        None,
        None,
        "legacy",
        true,
    );
    coordinated_release_mode(
        false,
        38,
        Some("current-changed"),
        None,
        None,
        "legacy",
        true,
    );
}

#[test]
fn staging_cannot_borrow_candidate_support_from_an_installed_coordinator() {
    coordinated_release_mode(false, 38, None, None, None, "stage-unsupported", true);
}
