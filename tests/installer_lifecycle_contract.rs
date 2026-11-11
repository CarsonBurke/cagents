#![cfg(target_os = "linux")]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

// Opaque transport only: these descriptors are not a usable production
// recovery receipt, and validating them never marks a fixture daemon Ready.
struct RecoveryTransportFixture {
    descriptors: Vec<(i32, fs::File)>,
}

impl RecoveryTransportFixture {
    const PAYLOAD: &'static [u8] =
        b"opaque installer transport fixture; no recovery authorization\n";

    fn new(root: &Path, daemon: &fs::File, candidate: &Path) -> Self {
        use std::io::Write;
        use std::os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::{MetadataExt, OpenOptionsExt},
        };
        let raw = unsafe {
            libc::memfd_create(
                c"installer-transport-fixture".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        assert!(raw >= 0);
        let mut receipt = unsafe { fs::File::from_raw_fd(raw) };
        receipt.write_all(Self::PAYLOAD).unwrap();
        assert_eq!(unsafe { libc::fchmod(raw, 0o600) }, 0);
        assert_eq!(
            unsafe {
                libc::fcntl(
                    raw,
                    libc::F_ADD_SEALS,
                    libc::F_SEAL_SEAL
                        | libc::F_SEAL_SHRINK
                        | libc::F_SEAL_GROW
                        | libc::F_SEAL_WRITE,
                )
            },
            0
        );
        let mut roles = Vec::new();
        for (fd, name, shared) in [
            (189, "legacy", true),
            (194, "worker", false),
            (195, "ui", false),
        ] {
            let path = root.join(format!("transport-{name}.lock"));
            let file = fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            let mode = if shared { libc::LOCK_SH } else { libc::LOCK_EX };
            assert_eq!(
                unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) },
                0
            );
            roles.push((fd, file, Some(path), shared));
        }
        roles.push((193, receipt, None, false));
        roles.push((
            196,
            daemon.try_clone().unwrap(),
            Some(root.join("runtime/worker-held.lock")),
            false,
        ));
        let mut descriptors = Vec::new();
        let mut identities = serde_json::Map::new();
        for (fd, file, path, shared) in roles {
            // Keep source descriptors above all remap targets. Each duplicate
            // retains the same OFD; a post-start shared offset proves that too.
            let raw = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 220) };
            assert!(raw >= 220);
            let file = unsafe { fs::File::from_raw_fd(raw) };
            let metadata = file.metadata().unwrap();
            identities.insert(fd.to_string(), serde_json::json!({"dev":metadata.dev(),"ino":metadata.ino(),"offset":9000 + fd,"path":path,"shared":shared}));
            assert_eq!(
                unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_SET) },
                0
            );
            descriptors.push((fd, file));
        }
        fs::write(root.join("recovery-identities.json"), serde_json::to_vec(&serde_json::json!({"roles":identities,"payload":String::from_utf8_lossy(Self::PAYLOAD),"candidate":candidate})).unwrap()).unwrap();
        Self { descriptors }
    }

    fn inherit(&self, command: &mut Command) {
        use std::os::{fd::AsRawFd, unix::process::CommandExt};
        let mapping: Vec<_> = self
            .descriptors
            .iter()
            .map(|(target, file)| (file.as_raw_fd(), *target))
            .collect();
        command.env("CAGENTS_UPDATE_RECOVERY_FD", "193");
        // SAFETY: only descriptor syscalls run after fork; self owns every
        // source until the child has exited and the observations are checked.
        unsafe {
            command.pre_exec(move || {
                for (source, target) in &mapping {
                    if libc::dup2(*source, *target) == -1
                        || libc::fcntl(*target, libc::F_SETFD, 0) == -1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }

    fn assert_shared_offsets(&self, forwarded: bool, output: &std::process::Output) {
        use std::os::fd::AsRawFd;
        for (target, file) in &self.descriptors {
            let expected = if forwarded { 9000 + *target } else { 0 };
            assert_eq!(
                unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR) },
                i64::from(expected),
                "transport fd {target} lost its open-file description or reached an unintended process; installer output: {output:?}"
            );
        }
    }
}

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
                let message = String::from_utf8_lossy(&output.stderr);
                assert!(message.contains("Cagents terminals are hosted sessions."));
                assert!(message.contains(
                    "A person must run this command from a system terminal outside cagents."
                ));
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
print(json.dumps({'Ok':{'stopped':[], 'stopped_windows':[], 'labels':{}}}))
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
    let handoff = restart.is_some_and(|mode| mode.starts_with("handoff-"));
    let orphan_bundle = coordinator == "bundle-orphan" || restart == Some("handoff-orphan-ui");
    let recovery_bundle = handoff || orphan_bundle;
    let bundle_forwarded = handoff && publish_failure.is_none() && !orphan_bundle;
    let downgrade_refusal = matches!(
        restart,
        Some("empty-downgrade-refusal" | "empty-downgrade-refusal-fails")
    );
    let maintenance = restart == Some("handoff-maintenance-failure") || downgrade_refusal;
    let bootstrap = restart == Some("handoff-bootstrap");
    let direct_bash = restart == Some("handoff-bash");
    let pending_standalone = matches!(
        coordinator,
        "pending-standalone" | "pending-standalone-supplied"
    );
    let ordinary_start_failure = matches!(
        coordinator,
        "first-start-failure"
            | "absent-start-failure"
            | "serving-start-failure"
            | "first-start-checkpoint"
            | "absent-start-checkpoint"
    );
    let first_start_checkpoint = matches!(
        coordinator,
        "first-start-checkpoint" | "absent-start-checkpoint"
    );
    let absent_source = matches!(
        coordinator,
        "first-start-failure"
            | "absent-start-failure"
            | "first-start-checkpoint"
            | "absent-start-checkpoint"
    );
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
    if !matches!(
        coordinator,
        "initial-independent" | "first-start-failure" | "first-start-checkpoint"
    ) {
        std::os::unix::fs::symlink("versions/old", prefix.join("current")).unwrap();
    }
    let status = r#"#!/bin/sh
CLI=old
if [ "$CLI:$*:$FIXTURE_RECOVERY_START_EXPECTED" = 'new:daemon start:true' ]; then
  "$FIXTURE_RECOVERY_CHECK" "$0" || exit 97
  # Model the real CLI's early sealing/consumption: no ordinary helper below
  # this point may inherit bundle or daemon startup authority.
  eval 'exec 189<&- 193<&- 194<&- 195<&- 196<&-'
  unset CAGENTS_UPDATE_RECOVERY_FD
else
  [ -z "${CAGENTS_UPDATE_RECOVERY_FD:-}" ] || exit 97
  for transport_fd in 189 193 194 195 196; do
    [ ! -e /proc/self/fd/$transport_fd ] || exit 97
  done
fi
fixture_recovery_receipt() {
  printf '{"version":1,"transaction":"00000000-0000-4000-8000-000000000001","census_digest":"0000000000000000000000000000000000000000000000000000000000000000","outcome":{"state":"%s"' "$2" > "$1"
  # These are mocked helper observations; the native proof is tested through
  # the real helper and daemon lanes. Keep their envelope schema exact.
  case "$2" in
    usable)
      printf ',"serving":{"host_session_id":"fixture-service","boot_id":"fixture-boot","host_incarnation":"fixture-incarnation"},"terminals":%s,"chats":0' "$FIXTURE_TERMINALS" >> "$1" ;;
    unverified)
      printf ',"serving":null,"readiness":null,"surviving_terminals":0,"expected_terminals":%s,"missing":[],"unexpected":[],"changed":[],"detail":"%s"' "$FIXTURE_TERMINALS" "${3:-unverified fixture}" >> "$1" ;;
    needs_target_recovery)
      printf ',"checkpoint_digest":"0000000000000000000000000000000000000000000000000000000000000000","source":{"host_session_id":"fixture-source","boot_id":"fixture-boot","host_incarnation":"fixture-source-incarnation"}' >> "$1" ;;
  esac
  printf '},"checkpoint_before":null}\n' >> "$1"
  if [ "$1" != "$FIXTURE_OUTCOME_OBSERVED" ]; then cp "$1" "$FIXTURE_OUTCOME_OBSERVED"; fi
}
if [ "$FIXTURE_HANDOFF" = true ]; then
  case "$CLI:$*" in
    'new:daemon verify-update-lease'|'new:daemon start'|'new:daemon store-schema'|'new:daemon downgrade-store 22')
      "$FIXTURE_LEASE_CHECK" || exit 97
      # Match CLI startup: ordinary helpers cannot retain update authority.
      exec 3<&-
      unset CAGENTS_DAEMON_LEASE_FD
      ;;
    *)
      # Includes older installed coordinators and every liveness/help probe.
      [ -z "${CAGENTS_DAEMON_LEASE_FD:-}" ] && [ ! -e /proc/self/fd/3 ] || exit 97
      ;;
  esac
  # The worker already owns window coordination in this fixture. This CLI
  # stands in for its verified inherited fence; real guard/adoption validation
  # is covered by installer_guard_contract and the daemon process tests.
  case "$*" in
    'verify-install-guard'*) exit 0 ;;
    'coordinate-install --help'|'record-deferred-install --help')
      printf 'Options:\n  --stage-only  Preserve running windows\n'
      exit 0
      ;;
  esac
fi
case "$*" in
  'daemon inspect-pending-update-recovery --candidate-cli '*)
    [ "$CLI" = new ] && { [ "$#" -eq 4 ] || { [ "$#" -eq 6 ] && [ "$5" = --census ]; }; } || exit 98
    [ "$4" = "$FIXTURE_PREFIX/versions/9.9.9/bin/cagents" ] || exit 98
    printf 'pending-inspect\n' >> "$FIXTURE_CENSUS_LOG"
    if [ -e "$FIXTURE_CHECKPOINT" ]; then
      printf "Run '%s' --no-start daemon recover-update --checkpoint fixture-digest --yes" "$4" >&2
      if [ "$#" -eq 6 ]; then printf " --census '%s'" "$6" >&2; fi
      printf '\n' >&2
      exit 1
    fi
    ;;
  'daemon capture-update-census --output '*)
    [ "$CLI" = new ] && [ "$#" -eq 6 ] && [ "$5" = --candidate-cli ] || exit 98
    [ "$6" = "$FIXTURE_PREFIX/versions/9.9.9/bin/cagents" ] || exit 98
    if [ "$FIXTURE_RESTART" = census-capture-failure ]; then
      printf 'The source did not provide an authenticated terminal census.\n' >&2
      exit 1
    fi
    cp "$FIXTURE_TERMINAL_INVENTORY" "$4"
    printf 'capture\n' >> "$FIXTURE_CENSUS_LOG"
    ;;
  'daemon verify-update-census --census '*' --outcome '*)
    [ "$CLI" = new ] || exit 98
    case "$#:$7:$8" in
      6::|7:--allow-absent:|7:--require-original:|7:--require-absent:|7:--validate-only:|8:--require-original:--allow-absent) ;;
      *) exit 98 ;;
    esac
    printf 'verify\n' >> "$FIXTURE_CENSUS_LOG"
    if [ ! -f "$4" ] || [ "$FIXTURE_RESTART" = provided-census-malformed ]; then
      fixture_recovery_receipt "$6" "unverified" "invalid_census"
      printf 'The supplied private census is invalid.\n' >&2
      exit 1
    fi
    if [ "$#" -eq 7 ] && [ "$7" = --require-absent ]; then
      if [ ! -e "$FIXTURE_RUNNING" ] && [ ! -s "$4" ] && [ ! -e "$FIXTURE_CHECKPOINT" ]; then
        fixture_recovery_receipt "$6" "no_source" ""
        exit 0
      fi
      fixture_recovery_receipt "$6" "unverified" "source_or_pending_recovery_exists"
      exit 1
    fi
    if [ "$#" -eq 7 ] && [ "$7" = --validate-only ]; then
      fixture_recovery_receipt "$6" "unverified" "census_validated"
      exit 0
    fi
    if [ "$#" -ge 7 ] && [ "$7" = --require-original ] && [ "$(cat "$FIXTURE_RUNNING" 2>/dev/null)" != old ]; then
      if [ "$#" -ne 8 ] || [ -e "$FIXTURE_RUNNING" ] || [ -s "$4" ]; then
        fixture_recovery_receipt "$6" "unverified" "not_original_source"
        exit 1
      fi
    fi
    if [ ! -e "$FIXTURE_RUNNING" ] && { [ "$7" = --allow-absent ] || [ "$8" = --allow-absent ]; } && [ ! -s "$4" ]; then
      fixture_recovery_receipt "$6" "no_source" ""
      exit 0
    fi
    if [ ! -e "$FIXTURE_RUNNING" ] || { [ "$FIXTURE_RESTART" = handoff-starting ] && [ "$(cat "$FIXTURE_RUNNING" 2>/dev/null)" = new ]; }; then
      fixture_recovery_receipt "$6" "unverified" "socket_not_ready"
      printf 'The terminal service is not ready. Inspect with --no-start daemon status and --no-start terminal list.\n' >&2
      exit 1
    fi
    if ! cmp -s "$4" "$FIXTURE_TERMINAL_INVENTORY"; then
      fixture_recovery_receipt "$6" "unverified" "terminal_inventory_mismatch"
      printf 'The terminal inventory does not match the pre-update census. Inspect with --no-start daemon status and --no-start terminal list.\n' >&2
      exit 1
    fi
    fixture_recovery_receipt "$6" "usable" ""
    ;;
  'daemon inspect-update-recovery --census '*' --outcome '*)
    [ "$CLI" = new ] && [ "$#" -eq 8 ] && [ "$7" = --candidate-cli ] || exit 98
    [ "$8" = "$FIXTURE_PREFIX/versions/9.9.9/bin/cagents" ] || exit 98
    printf 'inspect\n' >> "$FIXTURE_CENSUS_LOG"
    if [ -e "$FIXTURE_CHECKPOINT" ]; then
      fixture_recovery_receipt "$6" "needs_target_recovery" "Prepared"
      printf "Run '%s' --no-start daemon recover-update --checkpoint fixture-digest --yes --census '%s'\n" "$8" "$4" >&2
      exit 1
    fi
    fixture_recovery_receipt "$6" "restart_permitted" ""
    ;;
  'verify-install-guard')
    case "$FIXTURE_RESTART" in provided-census-*) exit 0 ;; esac
    exec "$FIXTURE_REAL_CLI" "$@"
    ;;
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
  'daemon verify-update-lease')
    [ "$CLI" = new ] && [ "$FIXTURE_HANDOFF" = true ] || exit 98
    ;;
  '--json daemon status')
    [ "$#" -eq 3 ] || exit 98
    if [ "$CLI" = new ] && { [ "$FIXTURE_RESTART" = start-unknown ] || [ "$FIXTURE_RESTART" = empty-start-unknown ] || [ "$FIXTURE_RESTART" = prepared-status-unknown ]; }; then exit 9; fi
    if [ "$CLI" = new ] && [ "$FIXTURE_COMPATIBLE" = false ] &&
       [ "$(cat "$FIXTURE_RUNNING" 2>/dev/null)" = old ]; then exit 9; fi
    if [ -e "$FIXTURE_RUNNING" ]; then running=true; else running=false; fi
    # Only the fixture's actual daemon marker reports a serving service.
    # A worker retaining its reservation after a dead candidate is not one.
    if [ -e "$FIXTURE_PENDING" ]; then
      [ "$CLI" = old ] || exit 98
      printf '%s\n' old:draining >> "$FIXTURE_ACTIONS"
      rm "$FIXTURE_PENDING" "$FIXTURE_RUNNING"
    elif [ "$running" = false ] && [ -e "$FIXTURE_CHECKPOINT" ]; then
      printf '%s:stopped\n' "$CLI" >> "$FIXTURE_ACTIONS"
    fi
    if [ "$FIXTURE_COORDINATOR:$CLI" = starting:new ] || [ "$FIXTURE_RESTART:$CLI" = handoff-starting:new ]; then
      printf '{"ok":true,"command":"daemon.status","data":{"running":false,"daemon_present":%s,"daemon_version":null,"inherited_lease":false}}\n' "$running"
    else
      printf '{"ok":true,"command":"daemon.status","data":{"running":%s,"daemon_version":"fixture","inherited_lease":false}}\n' "$running"
    fi
    ;;
  'daemon store-schema --help'|'daemon downgrade-store --help')
    [ "$#" -eq 3 ] || exit 98
    # Maintenance uses only the daemon lease, never the recovery bundle.
    if [ "$CLI" = new ] && [ "$FIXTURE_MAINTENANCE" = true ]; then exit 0; fi
    exit 64
    ;;
  'daemon store-schema')
    [ "$CLI" = new ] && [ "$FIXTURE_MAINTENANCE" = true ] || exit 98
    printf 'new:schema\n' >> "$FIXTURE_ACTIONS"
    printf '22\n'
    ;;
  'daemon downgrade-store 22')
    [ "$CLI" = new ] && [ "$FIXTURE_MAINTENANCE" = true ] || exit 98
    printf 'new:downgrade:22\n' >> "$FIXTURE_ACTIONS"
    case "$FIXTURE_RESTART" in empty-downgrade-refusal*) exit 7 ;; esac
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
    [ -s "$FIXTURE_CENSUS_LOG" ] || exit 98
    printf '%s:prepare\n' "$CLI" >> "$FIXTURE_ACTIONS"
    [ "$CLI" = old ] || exit 98
    [ "$FIXTURE_RESTART" != prepare-failure ] || exit 3
    [ "$(readlink "$FIXTURE_PREFIX/current")" = versions/old ] || exit 98
    printf '%s\n' exact-conversation-and-shells > "$FIXTURE_CHECKPOINT"
    if [ "$FIXTURE_RESTART" = pre-switch-recovery ]; then
      printf 'new\n' > "$FIXTURE_RUNNING"
      cp "$FIXTURE_CHECKPOINT" "$FIXTURE_RESTORED"
      printf 'external:ready\n' >> "$FIXTURE_ACTIONS"
      exit 3
    fi
    touch "$FIXTURE_PENDING"
    printf '{"ok":true,"command":"daemon.prepare_upgrade","data":{"terminals":%s,"chats":1}}\n' "$FIXTURE_TERMINALS"
    ;;
  'daemon stop --require-empty')
    [ "$#" -eq 3 ] || exit 98
    [ -s "$FIXTURE_CENSUS_LOG" ] || exit 98
    printf '%s\n' require-empty >> "$FIXTURE_ACTIONS"
    [ "$FIXTURE_TERMINALS" = 0 ] || exit 3
    rm "$FIXTURE_RUNNING"
    ;;
  'daemon start')
    [ "$#" -eq 2 ] || exit 98
    printf '%s:start\n' "$CLI" >> "$FIXTURE_ACTIONS"
    case "$CLI:$FIXTURE_RESTART" in
      new:empty-downgrade-refusal-fails) exit 4 ;;
      new:empty-downgrade-refusal)
        if [ ! -e "$FIXTURE_START_FAILED_ONCE" ]; then touch "$FIXTURE_START_FAILED_ONCE"; exit 4; fi
        ;;
      new:prepared-status-unknown) exit 4 ;;
    esac
    if [ "$CLI" = new ] && [ "$FIXTURE_ORDINARY_START_FAILURE" = true ]; then
      if [ "$FIXTURE_FIRST_START_CHECKPOINT" = true ]; then
        printf 'startup-created-checkpoint\n' > "$FIXTURE_CHECKPOINT"
      fi
      exit 4
    fi
    if [ "$CLI" = new ] && { [ "$FIXTURE_RESTART" = start-failure ] || [ "$FIXTURE_RESTART" = empty-start-failure ] || [ "$FIXTURE_RESTART" = handoff-start-failure ] || [ "$FIXTURE_RESTART" = handoff-maintenance-failure ] || [ "$FIXTURE_RESTART" = empty-recovery-unready ]; }; then exit 4; fi
    if [ -e "$FIXTURE_CHECKPOINT" ]; then
      [ ! -e "$FIXTURE_RUNNING" ] && [ ! -e "$FIXTURE_PENDING" ] || exit 98
      expected=versions/old
      [ "$CLI" = old ] || expected=versions/9.9.9
      [ "$(readlink "$FIXTURE_PREFIX/current")" = "$expected" ] || exit 98
      cp "$FIXTURE_CHECKPOINT" "$FIXTURE_RESTORED"
    fi
    if [ "$CLI:$FIXTURE_RESTART" = new:census-unready ] || [ "$CLI:$FIXTURE_RESTART" = old:empty-recovery-unready ]; then exit 0; fi
    if [ ! -e "$FIXTURE_RUNNING" ]; then printf '%s\n' "$CLI" > "$FIXTURE_RUNNING"; fi
    if [ "$CLI:$FIXTURE_RESTART" = new:census-missing ]; then
      printf 'terminal-0\n' > "$FIXTURE_TERMINAL_INVENTORY"
    fi
    if [ "$CLI" = new ] && { [ "$FIXTURE_RESTART" = start-unknown ] || [ "$FIXTURE_RESTART" = empty-start-unknown ]; }; then exit 4; fi
    if [ "$CLI" = new ] && [ "$FIXTURE_RESTART" = handoff-starting ]; then exit 4; fi
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
    let candidate_cli =
        status
            .replace("CLI=old", "CLI=new")
            .replacen("#!/bin/sh", "#!/bin/bash", 1);
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
# Above any pid_max, so delivery fails with ESRCH. A non-numeric target is not
# portable: bash 5.3 in POSIX mode reports it but exits 0.
if [ "$FIXTURE_REJECT_SIGNAL" = true ]; then target=2147483647; fi
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
        } else if failure == "external-recovery" {
            r#"if [ -e "$FIXTURE_CHECKPOINT" ]; then cp "$FIXTURE_CHECKPOINT" "$FIXTURE_RESTORED"; fi
printf 'new\n' > "$FIXTURE_RUNNING"; printf 'external:ready\n' >> "$FIXTURE_ACTIONS"; exit 8"#
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
    let utility_log = home.path().join("utility-descriptors");
    if recovery_bundle {
        // Every external installer utility is checked at exec, including
        // utilities reached through pipelines, substitutions and find -exec.
        for tool in [
            "awk",
            "cat",
            "chmod",
            "cp",
            "curl",
            "find",
            "getconf",
            "gh",
            "gtk-update-icon-cache",
            "id",
            "ln",
            "mkdir",
            "mktemp",
            "mv",
            "readlink",
            "rm",
            "rmdir",
            "sed",
            "sha256sum",
            "shasum",
            "sleep",
            "tar",
            "uname",
            "update-desktop-database",
        ] {
            let wrapper = tools.join(tool);
            let target = if wrapper.exists() {
                let saved = tools.join(format!("original-{tool}"));
                fs::rename(&wrapper, &saved).unwrap();
                saved
            } else {
                let system = Path::new("/usr/bin").join(tool);
                if !system.is_file() {
                    continue;
                }
                system
            };
            executable(
                &wrapper,
                &format!(
                    r#"#!/usr/bin/python3
import errno, os, sys
assert 'CAGENTS_DAEMON_LEASE_FD' not in os.environ
assert 'CAGENTS_UPDATE_RECOVERY_FD' not in os.environ
if os.environ['FIXTURE_BOOTSTRAP'] == 'true' and {tool:?} == 'id':
    parent = os.stat('/proc/' + str(os.getppid()) + '/exe')
    expected = os.stat('/bin/bash')
    assert (parent.st_dev, parent.st_ino) == (expected.st_dev, expected.st_ino), 'installer did not bootstrap system Bash before utilities'
for fd in (3, 189, 193, 194, 195, 196, 199):
    try:
        os.fstat(fd)
    except OSError as error:
        assert error.errno == errno.EBADF
    else:
        raise AssertionError(f'installer utility {{sys.argv[0]}} inherited fd {{fd}}')
with open(os.environ['FIXTURE_UTILITY_LOG'], 'a') as log:
    log.write({tool:?} + '\n')
target = {target:?}
os.execv(target, [target, *sys.argv[1:]])
"#,
                    target = target.to_str().unwrap()
                ),
            );
        }
    }
    let census_log = home.path().join("census-actions");
    let terminal_inventory = home.path().join("terminal-inventory");
    let census = home.path().join("update-census.json");
    let outcome = home.path().join("update-outcome.json");
    let inventory = (0..terminals)
        .map(|id| format!("terminal-{id}\n"))
        .collect::<String>();
    fs::write(&terminal_inventory, &inventory).unwrap();
    fs::write(&census, &inventory).unwrap();
    if restart == Some("provided-census-missing") {
        fs::remove_file(&census).unwrap();
    } else if restart == Some("provided-census-malformed") {
        fs::write(&census, "{invalid json").unwrap();
    } else if restart == Some("provided-census-mismatch") {
        fs::write(&census, "terminal-unrelated\n").unwrap();
    }
    let actions = home.path().join("actions");
    let running = home.path().join("running");
    let checkpoint = home.path().join("checkpoint");
    let restored = home.path().join("restored");
    if handoff && !maintenance {
        // Prepare/checkpoint already completed in the worker; the installer
        // receives a held lock and no running service, just as after shutdown.
        fs::write(&checkpoint, "exact-conversation-and-shells\n").unwrap();
    } else if pending_standalone {
        fs::write(&checkpoint, "exact-conversation-and-shells\n").unwrap();
    } else if !handoff && !absent_source && coordinator != "stage-no-source" {
        fs::write(&running, "old\n").unwrap();
    }
    let runtime = home.path().join("runtime");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let outer_signal = matches!(
        publish_failure,
        Some("outer-signal" | "outer-signal-invalid-target")
    );
    let bootstrap_poison = home.path().join("bootstrap-poisoned");
    let bootstrap_hook = home.path().join("bootstrap-hook");
    let shell = if bootstrap {
        let directory = home.path().join("bootstrap");
        fs::create_dir(&directory).unwrap();
        let shell = directory.join("sh");
        fs::copy("/bin/bash", &shell).unwrap();
        executable(
            &bootstrap_hook,
            r#"printf 'poisoned\n' > "$FIXTURE_BOOTSTRAP_POISON"
"#,
        );
        shell
    } else if direct_bash {
        std::path::PathBuf::from("/bin/bash")
    } else {
        std::path::PathBuf::from("sh")
    };
    let mut command = Command::new(shell);
    if direct_bash {
        // Enter ordinary noninteractive Bash with alias expansion initially
        // off; this path does not take the interpreter-inode reexec branch.
        command.args(["--noprofile", "--norc"]);
        for key in [
            "ENV",
            "BASH_ENV",
            "POSIXLY_CORRECT",
            "SHELLOPTS",
            "BASHOPTS",
        ] {
            command.env_remove(key);
        }
    }
    if bootstrap {
        command
            .env("BASH_ENV", &bootstrap_hook)
            .env("ENV", &bootstrap_hook)
            .env("BASH_VERSION", "spoofed-by-fixture");
    }
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
        Some(
            "empty"
                | "empty-start-failure"
                | "empty-start-unknown"
                | "empty-recovery-unready"
                | "empty-downgrade-refusal"
                | "empty-downgrade-refusal-fails"
        )
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
    let lease_path = runtime.join("worker-held.lock");
    let handoff_lease = recovery_bundle.then(|| {
        use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
        let lease = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lease_path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        lease
    });
    let mut transport = recovery_bundle.then(|| {
        RecoveryTransportFixture::new(
            home.path(),
            handoff_lease.as_ref().unwrap(),
            &prefix.join("versions/9.9.9/bin/cagents"),
        )
    });
    let recovery_check = home.path().join("check-recovery-transport");
    executable(
        &recovery_check,
        r#"#!/usr/bin/python3
import fcntl, json, os, pathlib, sys
assert os.environ['CAGENTS_UPDATE_RECOVERY_FD'] == '193'
assert os.environ['CAGENTS_DAEMON_LEASE_FD'] == '3'
manifest = json.loads(pathlib.Path(os.environ['FIXTURE_RECOVERY_IDENTITIES']).read_text())
assert pathlib.Path(sys.argv[1]) == pathlib.Path(manifest['candidate']), 'bundle reached current routing instead of exact candidate'
seals = fcntl.F_SEAL_SEAL | fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_GROW | fcntl.F_SEAL_WRITE
assert fcntl.fcntl(193, fcntl.F_GET_SEALS) & seals == seals
payload = manifest['payload'].encode()
assert os.pread(193, len(payload) + 1, 0) == payload
for number, expected in manifest['roles'].items():
    fd = int(number)
    actual = os.fstat(fd)
    assert (actual.st_dev, actual.st_ino) == (expected['dev'], expected['ino']), f'wrong role fd {fd}'
    assert not fcntl.fcntl(fd, fcntl.F_GETFD) & fcntl.FD_CLOEXEC
    if expected['path']:
        with open(expected['path'], 'r+') as contender:
            try: fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError: pass
            else: raise AssertionError(f'role fd {fd} lost its real flock')
            if expected['shared']:
                fcntl.flock(contender, fcntl.LOCK_SH | fcntl.LOCK_NB)
    os.lseek(fd, expected['offset'], os.SEEK_SET)
assert os.lseek(3, 0, os.SEEK_CUR) == manifest['roles']['196']['offset']
with open(os.environ['FIXTURE_RECOVERY_CALLS'], 'a') as log: log.write('candidate-start-transport\n')
"#,
    );
    if let Some(transport) = &transport {
        transport.inherit(&mut command);
    } else {
        command.env_remove("CAGENTS_UPDATE_RECOVERY_FD");
    }
    let lease_check = home.path().join("check-lease");
    if handoff {
        use std::os::{fd::AsRawFd, unix::process::CommandExt};
        executable(
            &lease_check,
            r#"#!/usr/bin/python3
import fcntl, os
assert os.environ['CAGENTS_DAEMON_LEASE_FD'] == '3'
held = os.fstat(3)
expected = os.stat(os.environ['FIXTURE_LEASE_PATH'])
assert (held.st_dev, held.st_ino) == (expected.st_dev, expected.st_ino)
fcntl.flock(3, fcntl.LOCK_EX | fcntl.LOCK_NB)
with open(os.environ['FIXTURE_LEASE_PATH'], 'r+') as contender:
    try:
        fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        pass
    else:
        raise AssertionError('worker lease was released during installation')
"#,
        );
        command
            .env("CAGENTS_DAEMON_LEASE_FD", "3")
            .env("CAGENTS_UI_UPDATE_GUARD_FD", "198");
        let descriptor = transport
            .as_ref()
            .unwrap()
            .descriptors
            .iter()
            .find(|(target, _)| *target == 196)
            .unwrap()
            .1
            .as_raw_fd();
        // SAFETY: the parent guard outlives installation, and only descriptor
        // syscalls run after fork. The fixed descriptor matches the real path.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(descriptor, 3) == -1 || libc::fcntl(3, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    } else if restart.is_some_and(|mode| mode.starts_with("provided-census-")) {
        command.env("CAGENTS_UI_UPDATE_GUARD_FD", "198");
    } else {
        command.env_remove("CAGENTS_UI_UPDATE_GUARD_FD");
    }
    if restart == Some("handoff-orphan-ui") {
        command.env_remove("CAGENTS_UI_UPDATE_GUARD_FD");
    }
    if handoff
        || restart.is_some_and(|mode| mode.starts_with("provided-census-"))
        || coordinator == "pending-standalone-supplied"
    {
        command.env("CAGENTS_INSTALL_CENSUS_PATH", &census);
    } else {
        command.env_remove("CAGENTS_INSTALL_CENSUS_PATH");
    }
    let output = command
        .env("CAGENTS_INSTALL_OUTCOME_PATH", &outcome)
        .env("FIXTURE_TERMINAL_INVENTORY", &terminal_inventory)
        .env("FIXTURE_CENSUS_LOG", &census_log)
        .env("FIXTURE_OUTCOME_OBSERVED", &outcome)
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("FIXTURE_HANDOFF", handoff.to_string())
        .env(
            "FIXTURE_ORDINARY_START_FAILURE",
            ordinary_start_failure.to_string(),
        )
        .env(
            "FIXTURE_FIRST_START_CHECKPOINT",
            first_start_checkpoint.to_string(),
        )
        .env("FIXTURE_MAINTENANCE", maintenance.to_string())
        .env("FIXTURE_BOOTSTRAP", bootstrap.to_string())
        .env("FIXTURE_BOOTSTRAP_POISON", &bootstrap_poison)
        .env("FIXTURE_RECOVERY_START_EXPECTED", handoff.to_string())
        .env("FIXTURE_RECOVERY_CHECK", &recovery_check)
        .env(
            "FIXTURE_RECOVERY_IDENTITIES",
            home.path().join("recovery-identities.json"),
        )
        .env(
            "FIXTURE_RECOVERY_CALLS",
            home.path().join("recovery-transport-calls"),
        )
        .env("FIXTURE_LEASE_CHECK", &lease_check)
        .env("FIXTURE_LEASE_PATH", &lease_path)
        .env("FIXTURE_UTILITY_LOG", &utility_log)
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
        .env(
            "FIXTURE_START_FAILED_ONCE",
            home.path().join("start-failed-once"),
        )
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
    if let Some(transport) = &transport {
        transport.assert_shared_offsets(bundle_forwarded, &output);
    }
    assert!(
        !bootstrap_poison.exists(),
        "Bash bootstrap must clear ENV/BASH_ENV before reexec"
    );
    if orphan_bundle {
        assert!(
            !output.status.success(),
            "orphan recovery authority was accepted: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("requires the worker daemon lease and launch fence")
        );
        assert!(
            !utility_log.exists(),
            "orphan bundle reached an external utility"
        );
        assert!(!actions.exists(), "orphan bundle changed source lifecycle");
        assert!(
            !census_log.exists(),
            "orphan bundle reached census/controller probes"
        );
        assert!(!home.path().join("coordinator-used").exists());
        assert_eq!(
            fs::read_link(prefix.join("current")).unwrap(),
            Path::new("versions/old")
        );
        assert!(!prefix.join("versions/9.9.9").exists());
        if handoff {
            assert_eq!(
                fs::read(&checkpoint).unwrap(),
                b"exact-conversation-and-shells\n"
            );
        } else {
            assert_eq!(fs::read_to_string(&running).unwrap(), "old\n");
        }
        return;
    }
    let source_inspected = home.path().join("source-inspected");
    if stage_only || handoff || !home.path().join("coordinator-used").exists() {
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
    if handoff && publish_failure != Some("external-recovery") {
        use std::os::fd::AsRawFd;
        let success = matches!(
            restart,
            Some("handoff-success" | "handoff-bootstrap" | "handoff-bash")
        );
        let starting = restart == Some("handoff-starting");
        assert_eq!(output.status.success(), success, "{output:?}");
        assert_eq!(
            fs::read_link(prefix.join("current")).unwrap(),
            Path::new(if maintenance {
                "versions/old"
            } else {
                "versions/9.9.9"
            })
        );
        let actions = fs::read_to_string(&actions).unwrap();
        assert_eq!(
            actions,
            if maintenance {
                "new:schema\nnew:start\nnew:downgrade:22\n"
            } else if success || starting {
                "new:start\n"
            } else {
                "new:start\nnew:stopped\n"
            }
        );
        let utility_calls = fs::read_to_string(&utility_log).unwrap();
        for tool in [
            "id", "uname", "getconf", "awk", "mktemp", "cp", "tar", "find", "chmod", "mkdir",
            "readlink", "sed", "mv", "rmdir", "ln", "rm",
        ] {
            assert!(
                utility_calls.lines().any(|called| called == tool),
                "{tool} was not checked: {utility_calls}"
            );
        }
        assert!(
            !actions.contains("old:start") && !actions.contains("old:prepare"),
            "the stopped source must not be prepared or restarted: {actions}"
        );
        if maintenance {
            assert!(!checkpoint.exists());
        } else {
            assert_eq!(
                fs::read(&checkpoint).unwrap(),
                b"exact-conversation-and-shells\n"
            );
        }
        assert_eq!(
            fs::read_to_string(home.path().join("recovery-transport-calls")).unwrap(),
            "candidate-start-transport\n"
        );
        if maintenance {
            assert!(!running.exists());
            assert!(!restored.exists());
            assert!(prefix.join("versions/9.9.9").exists());
        } else if success {
            assert_eq!(fs::read(&restored).unwrap(), fs::read(&checkpoint).unwrap());
            assert_eq!(fs::read_to_string(&running).unwrap(), "new\n");
        } else if starting {
            assert_eq!(fs::read_to_string(&running).unwrap(), "new\n");
            assert!(prefix.join("versions/9.9.9/bin/cagents").exists());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("original terminal service could not be verified"),
                "{output:?}"
            );
        } else {
            assert!(!running.exists());
            assert!(prefix.join("versions/9.9.9").exists());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("replacement daemon startup failed")
            );
            assert!(
                !String::from_utf8_lossy(&output.stderr)
                    .contains("original terminal service could not be verified"),
                "{output:?}"
            );
        }
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&outcome).unwrap()).unwrap();
        assert_eq!(
            receipt["outcome"]["state"],
            if maintenance {
                "restart_permitted"
            } else if success {
                "usable"
            } else if starting {
                "unverified"
            } else {
                "needs_target_recovery"
            }
        );
        let contender = fs::File::open(&lease_path).unwrap();
        assert_ne!(
            unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(transport.take());
        drop(handoff_lease);
        let released = unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if released != 0 {
            let error = std::io::Error::last_os_error();
            use std::os::unix::fs::MetadataExt;
            let expected = contender.metadata().unwrap();
            let mut owners = Vec::new();
            for process in fs::read_dir("/proc").unwrap().filter_map(Result::ok) {
                let Ok(pid) = process.file_name().to_string_lossy().parse::<u32>() else {
                    continue;
                };
                let Ok(descriptors) = fs::read_dir(process.path().join("fd")) else {
                    continue;
                };
                for fd in descriptors.filter_map(Result::ok) {
                    if fs::metadata(fd.path()).is_ok_and(|actual| {
                        actual.dev() == expected.dev() && actual.ino() == expected.ino()
                    }) {
                        owners.push(format!(
                            "pid={pid} fd={} inode={}",
                            fd.file_name().to_string_lossy(),
                            expected.ino()
                        ));
                    }
                }
            }
            panic!("installer reservation still held after worker exit: {error}; {owners:?}");
        }
        return;
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
                "2147483647\n"
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
    if publish_failure == Some("external-recovery") {
        assert!(!output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_link(prefix.join("current")).unwrap(),
            Path::new("versions/9.9.9")
        );
        assert_eq!(fs::read_to_string(&running).unwrap(), "new\n");
        assert!(prefix.join("versions/9.9.9/bin/cagents").exists());
        let calls = fs::read_to_string(&actions).unwrap();
        assert_eq!(
            calls, "external:ready\n",
            "no installer start, old restart or stop is permitted"
        );
        if handoff {
            assert_eq!(
                fs::read(&checkpoint).unwrap(),
                fs::read(&restored).unwrap(),
                "the serving recovery fixture retained the original checkpoint contents"
            );
            assert!(
                !home.path().join("recovery-transport-calls").exists(),
                "preserving a serving recovery must not invoke candidate start"
            );
        } else {
            assert!(
                !checkpoint.exists(),
                "the converter and checkpoint paths were not involved"
            );
        }
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&outcome).unwrap()).unwrap();
        assert_eq!(
            receipt["outcome"]["state"], "usable",
            "the actual recovery writer must be verified"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("instead of rewinding its binaries")
        );
        assert!(
            !fs::read_to_string(&census_log)
                .unwrap()
                .lines()
                .any(|probe| probe == "inspect"),
            "a verified live recovery writer must not enter stopped-source inspection"
        );
        return;
    }
    if pending_standalone {
        assert!(!output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_link(prefix.join("current")).unwrap(),
            Path::new("versions/old")
        );
        assert!(prefix.join("versions/9.9.9/bin/cagents").exists());
        assert_eq!(
            fs::read_to_string(&checkpoint).unwrap(),
            "exact-conversation-and-shells\n"
        );
        assert_eq!(fs::read_to_string(&census).unwrap(), inventory);
        assert_eq!(
            fs::read_to_string(&census_log).unwrap(),
            "pending-inspect\n"
        );
        assert!(fs::read_to_string(&actions).unwrap_or_default().is_empty());
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        let command = format!(
            "'{}' --no-start daemon recover-update --checkpoint fixture-digest --yes",
            prefix.join("versions/9.9.9/bin/cagents").display()
        );
        assert!(diagnostic.contains(&command), "{diagnostic}");
        if coordinator == "pending-standalone-supplied" {
            assert!(diagnostic.contains(&format!("--census '{}'", census.display())));
        }
        assert!(!String::from_utf8_lossy(&output.stdout).contains("your sessions are ready"));
        return;
    }
    if downgrade_refusal {
        assert!(!output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_link(prefix.join("current")).unwrap(),
            Path::new("versions/9.9.9")
        );
        assert_eq!(
            fs::read_to_string(&actions).unwrap(),
            "require-empty\nnew:schema\nnew:start\nnew:downgrade:22\nnew:start\n"
        );
        assert!(!checkpoint.exists());
        assert!(prefix.join("versions/9.9.9/bin/cagents").exists());
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&outcome).unwrap()).unwrap();
        if restart == Some("empty-downgrade-refusal") {
            assert_eq!(fs::read_to_string(&running).unwrap(), "new\n");
            assert_eq!(receipt["outcome"]["state"], "usable");
        } else {
            assert!(!running.exists());
            assert_ne!(receipt["outcome"]["state"], "usable");
            assert!(String::from_utf8_lossy(&output.stderr).contains(&format!(
                "'{}' daemon start",
                prefix.join("versions/9.9.9/bin/cagents").display()
            )));
        }
        assert!(
            prefix
                .join("versions/9.9.9/.cagents-retained-candidate-v1.json")
                .exists()
        );
        return;
    }
    if restart == Some("prepared-status-unknown") {
        assert!(!output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_link(prefix.join("current")).unwrap(),
            Path::new("versions/9.9.9")
        );
        assert!(!running.exists());
        assert_eq!(
            fs::read_to_string(&checkpoint).unwrap(),
            "exact-conversation-and-shells\n"
        );
        let actions = fs::read_to_string(&actions).unwrap();
        assert_eq!(
            actions,
            "old:help\nold:prepare\nold:draining\nold:stopped\nnew:start\n"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains(&format!(
            "'{}' --no-start daemon recover-update --checkpoint fixture-digest --yes --census",
            prefix.join("versions/9.9.9/bin/cagents").display()
        )));
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&outcome).unwrap()).unwrap();
        assert_eq!(receipt["outcome"]["state"], "needs_target_recovery");
        return;
    }
    if restart == Some("pre-switch-recovery") {
        assert!(!output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_link(prefix.join("current")).unwrap(),
            Path::new("versions/old")
        );
        assert!(prefix.join("versions/9.9.9/bin/cagents").exists());
        assert_eq!(fs::read_to_string(&running).unwrap(), "new\n");
        assert_eq!(fs::read(&checkpoint).unwrap(), fs::read(&restored).unwrap());
        assert_eq!(
            fs::read_to_string(&actions).unwrap(),
            "old:help\nold:prepare\nexternal:ready\n"
        );
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&outcome).unwrap()).unwrap();
        assert_eq!(receipt["outcome"]["state"], "usable");
        assert!(
            !fs::read_to_string(&census_log)
                .unwrap()
                .lines()
                .any(|probe| probe == "inspect")
        );
        return;
    }
    if ordinary_start_failure {
        assert_eq!(
            output.status.success(),
            absent_source && !first_start_checkpoint,
            "{output:?}"
        );
        assert_eq!(
            fs::read_link(prefix.join("current")).unwrap(),
            Path::new(if absent_source {
                "versions/9.9.9"
            } else {
                "versions/old"
            })
        );
        assert_eq!(fs::read_to_string(&actions).unwrap(), "new:start\n");
        assert_eq!(checkpoint.exists(), first_start_checkpoint);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("your sessions are ready"));
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&outcome).unwrap()).unwrap();
        if absent_source {
            assert!(prefix.join("versions/9.9.9/bin/cagents").exists());
            assert!(
                home.path()
                    .join("config/autostart/dev.cagents.Cagents.Daemon.desktop")
                    .is_symlink()
            );
            assert!(!running.exists());
            if first_start_checkpoint {
                assert!(
                    !String::from_utf8_lossy(&output.stdout)
                        .contains("will start at graphical login")
                );
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("absence is no longer proven")
                );
                assert_eq!(
                    fs::read_to_string(&checkpoint).unwrap(),
                    "startup-created-checkpoint\n"
                );
                assert_eq!(receipt["outcome"]["state"], "unverified");
            } else {
                assert!(
                    String::from_utf8_lossy(&output.stdout)
                        .contains("will start at graphical login")
                );
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("daemon start command failed")
                );
                assert_eq!(
                    receipt["outcome"]["state"], "no_source",
                    "file publication cannot mint readiness"
                );
            }
        } else {
            assert_eq!(fs::read_to_string(&running).unwrap(), "old\n");
            assert!(!prefix.join("versions/9.9.9").exists());
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains("will start at graphical login")
            );
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("replacement daemon startup failed")
            );
            assert_eq!(
                receipt["outcome"]["state"], "usable",
                "the original service remains verified after rollback"
            );
        }
        return;
    }
    if restart == Some("census-capture-failure")
        || restart.is_some_and(|mode| mode.starts_with("provided-census-"))
    {
        assert!(!output.status.success());
        assert_eq!(
            fs::read_link(prefix.join("current")).unwrap(),
            Path::new("versions/old")
        );
        assert_eq!(fs::read_to_string(&running).unwrap(), "old\n");
        assert!(fs::read_to_string(&actions).unwrap_or_default().is_empty());
        assert!(!checkpoint.exists());
        assert!(String::from_utf8_lossy(&output.stderr).contains(
            if restart == Some("census-capture-failure") {
                "cannot capture the terminal inventory"
            } else {
                "supplied terminal census is invalid or no longer matches"
            }
        ));
        return;
    }
    let recovery_required = restart.is_some_and(|mode| {
        matches!(
            mode,
            "census-missing" | "census-unready" | "empty-recovery-unready"
        )
    }) || (restart.is_some_and(|mode| matches!(mode, "start-failure"))
        || (restart == Some("success") && publish_failure.is_some()));
    if recovery_required {
        assert!(!output.status.success(), "{output:?}");
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&outcome).unwrap()).unwrap();
        assert!(
            receipt["outcome"]["state"] == "unverified"
                || receipt["outcome"]["state"] == "needs_target_recovery",
            "{receipt}"
        );
        assert!(
            prefix.join("versions/9.9.9/bin/cagents").exists(),
            "unverified recovery must retain its target"
        );
        if restart != Some("empty-recovery-unready") {
            assert_eq!(
                fs::read_link(prefix.join("current")).unwrap(),
                Path::new("versions/9.9.9"),
                "Prepared recovery keeps its capable target selected"
            );
        }
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains("terminal service readiness is unverified"),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains("--no-start daemon status"),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains("--no-start terminal list"),
            "{diagnostic}"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("your sessions are ready"));
        assert_eq!(
            fs::read(&checkpoint).ok(),
            if restart == Some("empty-recovery-unready") {
                None
            } else {
                Some(b"exact-conversation-and-shells\n".to_vec())
            }
        );
        if restart != Some("empty-recovery-unready") {
            assert!(
                !fs::read_to_string(&actions)
                    .unwrap_or_default()
                    .contains("old:start"),
                "a Prepared checkpoint must not be offered to the old daemon"
            );
        }
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
                    .contains("existing windows and the previous service state were preserved")
            );
            if coordinator == "stage-no-source" {
                assert!(!running.exists());
                let receipt: serde_json::Value =
                    serde_json::from_slice(&fs::read(&outcome).unwrap()).unwrap();
                assert_eq!(receipt["outcome"]["state"], "no_source");
                assert!(!String::from_utf8_lossy(&output.stdout).contains("daemon is running"));
                return;
            }
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
fn checkpointed_publication_failure_requires_target_recovery() {
    upgrade_fixture(false, 7, Some("error"), Some("success"));
}

#[test]
fn checkpointed_publication_interruption_requires_target_recovery() {
    upgrade_fixture(true, 7, Some("signal"), Some("success"));
}

#[test]
fn checkpointed_start_failure_requires_target_recovery() {
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
fn staging_preserves_daemons_and_rolls_back_without_mutating_sessions() {
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

/// Other tests fork concurrently, briefly inheriting even CLOEXEC descriptors
/// before exec. Give lease-release assertions a sole fixture process, so they
/// measure installer descendants rather than unrelated tests' fork windows.
fn isolated_handoff_fixture(test: &str, modes: &[&str]) {
    const CHILD: &str = "CAGENTS_TEST_INSTALLER_LEASE_TRANSACTION";
    if std::env::var(CHILD).ok().as_deref() != Some(test) {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env(CHILD, test)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    for mode in modes {
        upgrade_fixture(
            false,
            if *mode == "handoff-maintenance-failure" {
                0
            } else {
                7
            },
            if *mode == "handoff-external" {
                Some("external-recovery")
            } else {
                None
            },
            Some(mode),
        );
    }
}

/// Shell transaction coverage with a real inherited flock and mocked companion
/// replies; native daemon adoption is exercised by the daemon process tests.
#[test]
fn lease_path_installer_transaction() {
    isolated_handoff_fixture(
        "lease_path_installer_transaction",
        &["handoff-success", "handoff-start-failure"],
    );
}

/// A real starting daemon is distinct from the worker's held reservation.
/// Cleanup must retain the live process's executable tree.
#[test]
fn inherited_starting_daemon_keeps_its_new_version_on_installer_failure() {
    isolated_handoff_fixture(
        "inherited_starting_daemon_keeps_its_new_version_on_installer_failure",
        &["handoff-starting"],
    );
}

#[test]
fn successful_start_requires_ready_socket_and_complete_terminal_census() {
    upgrade_fixture(false, 7, None, Some("census-missing"));
    upgrade_fixture(false, 7, None, Some("census-unready"));
}

#[test]
fn zero_exit_old_restart_without_readiness_keeps_failure_and_recovery_target() {
    upgrade_fixture(false, 0, None, Some("empty-recovery-unready"));
}

#[test]
fn failed_census_capture_refuses_before_preparing_or_stopping_source() {
    upgrade_fixture(false, 7, None, Some("census-capture-failure"));
}

#[test]
fn staging_proves_absent_source_without_claiming_a_usable_daemon() {
    coordinated_release_mode(true, 0, None, None, None, "stage-no-source", true);
}

#[test]
fn externally_started_recovery_writer_before_installer_start_prevents_binary_rollback() {
    upgrade_fixture(true, 7, Some("external-recovery"), None);
}

#[test]
fn supplied_census_must_validate_and_match_before_source_shutdown() {
    for mode in [
        "provided-census-missing",
        "provided-census-malformed",
        "provided-census-mismatch",
    ] {
        upgrade_fixture(false, 7, None, Some(mode));
    }
}

#[test]
fn recovery_bundle_is_preserved_only_for_candidate_start_and_excluded_from_maintenance() {
    isolated_handoff_fixture(
        "recovery_bundle_is_preserved_only_for_candidate_start_and_excluded_from_maintenance",
        &["handoff-maintenance-failure"],
    );
}

#[test]
fn recovery_bundle_bootstraps_system_bash_before_any_utility() {
    isolated_handoff_fixture(
        "recovery_bundle_bootstraps_system_bash_before_any_utility",
        &["handoff-bootstrap"],
    );
}

#[test]
fn orphan_recovery_bundle_without_daemon_lease_is_rejected_before_tools() {
    coordinated_release_mode(true, 7, None, None, None, "bundle-orphan", false);
}

#[test]
fn orphan_recovery_bundle_without_application_guard_is_rejected_before_tools() {
    isolated_handoff_fixture(
        "orphan_recovery_bundle_without_application_guard_is_rejected_before_tools",
        &["handoff-orphan-ui"],
    );
}

#[test]
fn recovery_bundle_does_not_restart_or_rewind_an_already_serving_recovery() {
    isolated_handoff_fixture(
        "recovery_bundle_does_not_restart_or_rewind_an_already_serving_recovery",
        &["handoff-external"],
    );
}

#[test]
fn recovery_bundle_direct_bash_enables_alias_isolation_before_any_utility() {
    isolated_handoff_fixture(
        "recovery_bundle_direct_bash_enables_alias_isolation_before_any_utility",
        &["handoff-bash"],
    );
}

#[test]
fn ordinary_no_current_or_absent_source_installs_keep_files_when_first_start_fails() {
    for coordinator in ["first-start-failure", "absent-start-failure"] {
        coordinated_release_mode(true, 0, None, None, None, coordinator, false);
    }
}

#[test]
fn explicit_restart_without_a_source_keeps_files_when_first_start_fails() {
    for coordinator in ["first-start-failure", "absent-start-failure"] {
        coordinated_release_mode(
            true,
            0,
            None,
            Some("no-source-explicit"),
            None,
            coordinator,
            false,
        );
    }
}

#[test]
fn standalone_downgrade_refusal_restarts_the_selected_candidate_or_prints_exact_start_command() {
    for mode in ["empty-downgrade-refusal", "empty-downgrade-refusal-fails"] {
        upgrade_fixture(true, 0, None, Some(mode));
    }
}

#[test]
fn unknown_replacement_status_still_exposes_the_prepared_recovery_command() {
    upgrade_fixture(true, 7, None, Some("prepared-status-unknown"));
}

#[test]
fn ordinary_serving_source_start_failure_still_fails_and_rolls_back() {
    for terminals in [0, 7] {
        coordinated_release_mode(
            true,
            terminals,
            None,
            None,
            None,
            "serving-start-failure",
            false,
        );
    }
}

#[test]
fn failed_first_start_cannot_hide_a_new_checkpoint_behind_earlier_absence() {
    for coordinator in ["first-start-checkpoint", "absent-start-checkpoint"] {
        coordinated_release_mode(true, 0, None, None, None, coordinator, false);
    }
}

#[test]
fn standalone_pending_retry_keeps_original_scope_and_prints_explicit_recovery_command() {
    for coordinator in ["pending-standalone", "pending-standalone-supplied"] {
        coordinated_release_mode(true, 7, None, None, None, coordinator, false);
    }
}

#[test]
fn serving_target_recovery_before_current_switch_is_retained_without_old_restart() {
    upgrade_fixture(true, 7, None, Some("pre-switch-recovery"));
}
