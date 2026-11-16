//! Production probe/consume code against isolated provider peers only.
use super::super::resets::ConsumeStatus;
use super::*;
use std::{os::unix::fs::PermissionsExt, process::Command as StdCommand};

const CHILD: &str = "CAGENTS_RESET_CONTRACT_CHILD";

struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn reads_consumes_refreshes_and_reuses_keys_against_fake_providers() {
    if std::env::var_os(CHILD).is_none() {
        let fixture = tempfile::tempdir().unwrap();
        // The offload TMPDIR can traverse symlinked ancestors; the fake
        // service deliberately accepts only an owned physical private root.
        let physical = fixture.path().canonicalize().unwrap();
        std::fs::set_permissions(&physical, std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = physical.as_path();
        let helper =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/usage-reset-fixture.py");
        let seed = StdCommand::new("/usr/bin/python3")
            .arg(&helper)
            .arg("seed")
            .arg(root)
            .output()
            .unwrap();
        assert!(
            seed.status.success(),
            "{}",
            String::from_utf8_lossy(&seed.stderr)
        );
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let claude = bin.join("claude");
        std::fs::write(&claude, r#"#!/usr/bin/python3
import json,os,pathlib,sys
home=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])
if '--version' in sys.argv:
 print('2.1.296 (fixture)');sys.exit()
if sys.argv[1:3]==['auth','status']:
 alternate=home/'fixture-auth-email'
 print(json.dumps({'loggedIn':True,'email':alternate.read_text() if alternate.exists() else 'fixture@example.test','subscriptionType':'max'}));sys.exit()
for line in sys.stdin:
 frame=json.loads(line); request=frame['request']
 result={'models':[]} if request['subtype']=='initialize' else json.loads((home/'fixture-usage.json').read_text())
 print(json.dumps({'type':'control_response','response':{'request_id':frame['request_id'],'subtype':'success','response':result}}),flush=True)
"#).unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700)).unwrap();
        let codex = bin.join("codex");
        let peer =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-usage-resets.py");
        std::fs::write(
            &codex,
            format!(
                "#!/bin/sh\nexec /usr/bin/python3 '{}' \"$@\"\n",
                peer.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _server = Server(
            StdCommand::new("/usr/bin/python3")
                .arg(&helper)
                .arg("serve")
                .arg(root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let marker = root.join("home/.claude/fixture-reset-api.json");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !marker.is_file() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(marker.is_file(), "fake Claude HTTP server did not start");
        let output = StdCommand::new(std::env::current_exe().unwrap())
            .args(["--exact","account::probe::reset_contract_tests::reads_consumes_refreshes_and_reuses_keys_against_fake_providers","--nocapture"])
            .env(CHILD, root).env("HOME",root.join("home"))
            .env("XDG_DATA_HOME",root.join("data")).env("XDG_CONFIG_HOME",root.join("config"))
            .env("XDG_CACHE_HOME",root.join("cache")).env("XDG_STATE_HOME",root.join("state"))
            .env("SHELL","/bin/sh").env("PATH",format!("{}:/usr/bin:/bin",bin.display()))
            .env("CLAUDE_CONFIG_DIR",root.join("home/.claude")).env("CODEX_HOME",root.join("home/.codex"))
            .env("CAGENTS_USAGE_RESETS_FIXTURE_ROOT",root)
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
    let claude = Account::system(Provider::Claude);
    let codex = Account::system(Provider::Codex);
    let identity = |email: &str| {
        Some(Identity {
            email: Some(email.into()),
            ..Identity::default()
        })
    };
    let claude_identity = identity("fixture@example.test");
    let codex_identity = identity("codex@fixture.invalid");
    let cache_directory = root.join("production-cache");
    crate::secure_file::prepare_private_directory(&cache_directory).unwrap();
    let cache = Some(Cache::at(&cache_directory));
    // Keep cache publication blocked after the fake provider answers. A
    // returned Answered must mean both publication and probe-lock release
    // finished, so an immediate refresh cannot collide with its own write.
    let publication_lock =
        crate::instance::owned_lock(&cache_directory.join("account-usage.lock"), libc::LOCK_EX)
            .unwrap();
    let mut first = Box::pin(run(claude.clone(), cache.clone(), true));
    assert!(
        tokio::time::timeout(Duration::from_secs(2), &mut first)
            .await
            .is_err(),
        "Answered returned before cache publication finished"
    );
    assert!(cache.as_ref().unwrap().load(now_ms()).is_empty());
    drop(publication_lock);
    let first = first.await;

    let Outcome::Answered { snapshot, .. } = first else {
        panic!("fake Claude usage read failed: {first:?}")
    };
    let published = cache
        .as_ref()
        .unwrap()
        .load(now_ms())
        .remove(&claude.cache_key().unwrap())
        .unwrap();
    assert_eq!(published.snapshot, snapshot);
    assert!(
        cache
            .as_ref()
            .unwrap()
            .try_probe_lock(&claude.lock_name())
            .unwrap()
            .is_some(),
        "a completed probe still held admission"
    );
    assert_eq!(
        snapshot
            .resets
            .iter()
            .map(|reset| reset.available_count)
            .sum::<u64>(),
        4
    );
    change_state(&root, "claude", |state| {
        state["mode"] = "unconfirmed".into()
    });
    let other_reader = cache
        .as_ref()
        .unwrap()
        .try_probe_lock(&claude.lock_name())
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            run(claude.clone(), cache.clone(), false).await,
            Outcome::Busy
        ),
        "background probes should not wait"
    );
    let timed_out = run_forced(
        claude.clone(),
        cache.clone(),
        None,
        Duration::from_millis(75),
    )
    .await;
    assert!(
        matches!(timed_out, Outcome::Failed(_)),
        "a held read lock must respect the explicit-refresh deadline"
    );
    let mut pending_reset = Box::pin(consume(
        claude.clone(),
        "cedar_ember/fable-fixture".into(),
        "claude-success".into(),
        claude_identity.clone(),
        cache.clone(),
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(75), &mut pending_reset)
            .await
            .is_err(),
        "explicit reset preflight should await the other reader"
    );
    assert_eq!(
        state(&root, "claude")["consume_count"],
        0,
        "waiting for read admission must not send a consume"
    );
    drop(other_reader);
    let response = pending_reset.await;
    assert_eq!(
        response.status,
        ConsumeStatus::Unconfirmed,
        "HTTP 200 can still be ambiguous: {response:?}"
    );
    let snapshot = response.snapshot.unwrap();
    assert_eq!(
        snapshot
            .windows
            .iter()
            .find(|window| window.key == "weekly_scoped:Fable")
            .unwrap()
            .used,
        0.0
    );
    assert!(
        snapshot
            .windows
            .iter()
            .find(|window| window.key == "weekly_all")
            .unwrap()
            .used
            > 0.9,
        "a model reset must not clear the account"
    );
    let marker = root.join("home/.claude/fixture-reset-api.json");
    let saved_marker = std::fs::read(&marker).unwrap();
    std::fs::write(&marker, "service unavailable").unwrap();
    let pending = consume(
        claude.clone(),
        "cedar_ember/fable-fixture".into(),
        "claude-success".into(),
        claude_identity.clone(),
        cache.clone(),
    )
    .await;
    assert_eq!(
        pending.status,
        ConsumeStatus::Unconfirmed,
        "a failed retry preflight cannot settle the original claim"
    );
    let fresh = consume(
        claude.clone(),
        "cedar_ember/fable-fixture".into(),
        "claude-service-unavailable".into(),
        claude_identity.clone(),
        cache.clone(),
    )
    .await;
    assert_eq!(fresh.status, ConsumeStatus::Unavailable);
    assert_eq!(state(&root, "claude")["consume_count"], 1);
    std::fs::write(&marker, saved_marker).unwrap();
    let replay = consume(
        claude.clone(),
        "cedar_ember/fable-fixture".into(),
        "claude-success".into(),
        claude_identity.clone(),
        cache.clone(),
    )
    .await;
    assert_eq!(
        replay.status,
        ConsumeStatus::NoCredit,
        "the replay reports an already-used claim, not a new reset"
    );
    assert_eq!(
        state(&root, "claude")["consume_count"],
        1,
        "an ambiguous HTTP 200 must reuse the original claim"
    );
    change_state(&root, "claude", |state| state["mode"] = "disconnect".into());
    let response = consume(
        claude.clone(),
        "cedar_ember/fable-fixture".into(),
        "claude-lost-reply".into(),
        claude_identity.clone(),
        cache.clone(),
    )
    .await;
    assert_eq!(response.status, ConsumeStatus::Unconfirmed, "{response:?}");
    assert!(
        response.snapshot.is_some(),
        "an ambiguous consume still refreshes"
    );
    assert_eq!(
        state(&root, "claude")["consume_count"],
        2,
        "disconnect must not trigger an automatic retry"
    );
    let response = consume(
        claude.clone(),
        "cedar_ember/fable-fixture".into(),
        "claude-lost-reply".into(),
        claude_identity.clone(),
        cache.clone(),
    )
    .await;
    assert_eq!(
        response.status,
        ConsumeStatus::Reset,
        "same original key/reset is replayed"
    );
    assert_eq!(state(&root, "claude")["consume_count"], 2);
    change_state(&root, "claude", |state| {
        state["mode"] = "success".into();
        state["usage"]["rate_limits"]["model_scoped"][0]["utilization"] = 100.into();
    });
    let response = consume(
        claude.clone(),
        "cedar_ember/account-fixture".into(),
        "claude-account".into(),
        claude_identity.clone(),
        cache.clone(),
    )
    .await;
    assert_eq!(response.status, ConsumeStatus::Reset, "{response:?}");
    let snapshot = response.snapshot.unwrap();
    assert_eq!(
        snapshot
            .windows
            .iter()
            .find(|window| window.key == "weekly_all")
            .unwrap()
            .used,
        0.0
    );
    assert_eq!(
        snapshot
            .windows
            .iter()
            .find(|window| window.key == "weekly_scoped:Fable")
            .unwrap()
            .used,
        1.0,
        "an account reset must not clear Fable"
    );
    let response = consume(
        claude.clone(),
        "cedar_ember/account-fixture".into(),
        "claude-empty".into(),
        claude_identity.clone(),
        cache.clone(),
    )
    .await;
    assert_eq!(response.status, ConsumeStatus::NoCredit);
    assert_eq!(state(&root, "claude")["consume_count"], 3);
    // Also exercise the server's spent-grant reply directly, not just preflight.
    let program = root.join("bin/claude");
    let client = reset_transport::Client::load(&claude, &program, None)
        .await
        .unwrap();
    let spent = client
        .consume("cedar_ember/account-fixture", "claude-spent-grant")
        .await
        .unwrap();
    assert_eq!(
        reset_consume::claude_status(&spent),
        ConsumeStatus::NoCredit
    );
    std::fs::write(
        root.join("home/.claude/fixture-auth-email"),
        "changed@fixture.invalid",
    )
    .unwrap();
    let response = consume(
        claude.clone(),
        "cedar_ember/account-fixture".into(),
        "changed-login".into(),
        claude_identity.clone(),
        cache.clone(),
    )
    .await;
    assert_eq!(response.status, ConsumeStatus::Unavailable);
    std::fs::remove_file(root.join("home/.claude/fixture-auth-email")).unwrap();

    // A failed inventory lookup may retain disabled metadata only for the
    // same confirmed login; changing accounts cannot inherit another's grants.
    let marker = root.join("home/.claude/fixture-reset-api.json");
    let saved_marker = std::fs::read(&marker).unwrap();
    std::fs::write(&marker, "invalid fixture marker").unwrap();
    let mut previous = UsageSnapshot::state(UsageStatus::Warning, "", now_ms());
    previous.resets.push(super::super::UsageReset {
        id: "previous-login".into(),
        kind: super::super::ResetKind::Granted,
        scopes: vec![super::super::ResetScope::AccountWeekly],
        available_count: 9,
        expires_at: None,
        usable: true,
    });
    for (cache_name, switched) in [("same-login-cache", false), ("changed-login-cache", true)] {
        let cache = Cache::at(root.join(cache_name));
        cache
            .store(
                &claude.cache_key().unwrap(),
                Entry {
                    identity: claude_identity.clone(),
                    snapshot: previous.clone(),
                },
            )
            .unwrap();
        if switched {
            std::fs::write(
                root.join("home/.claude/fixture-auth-email"),
                "changed@fixture.invalid",
            )
            .unwrap();
        }
        let response = run(claude.clone(), Some(cache), true).await;
        let Outcome::Answered { snapshot, .. } = response else {
            panic!("failed cache contract: {response:?}")
        };
        let retained = snapshot
            .resets
            .iter()
            .find(|reset| reset.id == "previous-login");
        if switched {
            assert!(
                retained.is_none(),
                "another login's reset counts must not be attributed to this login"
            );
        } else {
            assert!(
                !retained.unwrap().usable,
                "failed inventory must disable the old action"
            );
        }
    }
    std::fs::remove_file(root.join("home/.claude/fixture-auth-email")).unwrap();
    std::fs::write(&marker, saved_marker).unwrap();

    change_state(&root, "claude", |state| {
        state["mode"] = "disconnect".into();
        state["usage"]["rate_limits"]["five_hour"]["utilization"] = 100.into();
        state["usage"]["rate_limits"]["seven_day"]["utilization"] = 96.into();
    });
    let response = consume(
        claude.clone(),
        "juniper_tide".into(),
        "claude-session-lost-reply".into(),
        claude_identity.clone(),
        cache.clone(),
    )
    .await;
    assert_eq!(response.status, ConsumeStatus::Unconfirmed, "{response:?}");
    let refreshed = response.snapshot.unwrap();
    assert_eq!(
        refreshed
            .windows
            .iter()
            .find(|window| window.key == "session")
            .unwrap()
            .used,
        0.0
    );
    assert!(
        refreshed
            .windows
            .iter()
            .find(|window| window.key == "weekly_all")
            .unwrap()
            .used
            > 0.9
    );
    assert_eq!(
        refreshed
            .windows
            .iter()
            .find(|window| window.key == "weekly_scoped:Fable")
            .unwrap()
            .used,
        1.0
    );
    assert_eq!(state(&root, "claude")["session_post_count"], 1);
    let response = consume(
        claude,
        "juniper_tide".into(),
        "claude-session-lost-reply".into(),
        claude_identity,
        cache.clone(),
    )
    .await;
    assert_eq!(response.status, ConsumeStatus::Unconfirmed);
    assert!(response.snapshot.is_some());
    assert_eq!(
        state(&root, "claude")["session_post_count"],
        1,
        "session resets without server idempotency can never be resubmitted"
    );

    let first = run(codex.clone(), cache.clone(), true).await;
    let Outcome::Answered { snapshot, .. } = first else {
        panic!("fake Codex usage read failed: {first:?}")
    };
    assert_eq!(
        snapshot
            .resets
            .iter()
            .map(|reset| reset.available_count)
            .sum::<u64>(),
        2
    );
    let one = "00000000-0000-4000-8000-000000000002";
    let two = "00000000-0000-4000-8000-000000000003";
    let response = consume(
        codex.clone(),
        one.into(),
        "codex-success".into(),
        codex_identity.clone(),
        cache.clone(),
    )
    .await;
    assert_eq!(response.status, ConsumeStatus::Reset, "{response:?}");
    assert!(
        response
            .snapshot
            .unwrap()
            .windows
            .iter()
            .all(|window| window.used == 0.0)
    );
    change_state(&root, "codex", |state| state["mode"] = "disconnect".into());
    let response = consume(
        codex.clone(),
        two.into(),
        "codex-lost-reply".into(),
        codex_identity.clone(),
        cache.clone(),
    )
    .await;
    assert_eq!(response.status, ConsumeStatus::Unconfirmed, "{response:?}");
    assert!(response.snapshot.is_some());
    assert_eq!(state(&root, "codex")["consume_count"], 2);
    let response = consume(
        codex.clone(),
        two.into(),
        "codex-lost-reply".into(),
        codex_identity.clone(),
        cache.clone(),
    )
    .await;
    assert_eq!(response.status, ConsumeStatus::Reset, "{response:?}");
    assert_eq!(state(&root, "codex")["consume_count"], 2);
    let response = consume(
        codex.clone(),
        two.into(),
        "codex-empty".into(),
        codex_identity.clone(),
        cache.clone(),
    )
    .await;
    assert_eq!(response.status, ConsumeStatus::NoCredit);
    let response = reset_consume::codex_consume(
        &codex,
        &root.join("bin/codex"),
        None,
        "missing-credit",
        "codex-no-credit",
        codex_identity.as_ref().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response, ConsumeStatus::NoCredit);
    // Reusing a key for another account must fail before another submit.
    let response = consume(
        codex,
        two.into(),
        "claude-lost-reply".into(),
        codex_identity,
        cache.clone(),
    )
    .await;
    assert_eq!(response.status, ConsumeStatus::Unavailable);
    assert_eq!(state(&root, "codex")["consume_count"], 2);
}

fn state(root: &Path, provider: &str) -> Value {
    serde_json::from_slice(
        &std::fs::read(root.join(format!("home/.{provider}/fixture-reset-state.json"))).unwrap(),
    )
    .unwrap()
}
fn change_state(root: &Path, provider: &str, change: impl FnOnce(&mut Value)) {
    let mut state = state(root, provider);
    change(&mut state);
    std::fs::write(
        root.join(format!("home/.{provider}/fixture-reset-state.json")),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    if provider == "claude" {
        std::fs::write(
            root.join("home/.claude/fixture-usage.json"),
            serde_json::to_vec(&state["usage"]).unwrap(),
        )
        .unwrap();
    }
}
