//! Regression for #2118: a lost account registry leaves its managed homes,
//! with their logins and histories, on disk. The daemon's startup sweep
//! restores each home its provider confirms a login for, under its original
//! key, and leaves every other one exactly as it was.

use super::account_lifecycle;
use crate::account::{
    Account,
    home::Homes,
    registry::{Registered, Store},
};
use crate::agent::Provider;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const CHILD: &str = "CAGENTS_ACCOUNT_RESTORE_TEST";

/// A stand-in for Claude Code. A home is signed in when it holds a `login`
/// file naming its email; the stand-in answers `auth status` and the usage
/// session from it, as the real CLI would from its own login.
fn install_claude(program: &Path) {
    std::fs::write(
        program,
        r#"#!/bin/sh
home="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
if [ ! -f "$home/login" ]; then
  if [ "$1" = auth ]; then echo '{"loggedIn": false}'; fi
  exit 1
fi
if [ "$1" = auth ]; then
  printf '{"loggedIn": true, "email": "%s", "subscriptionType": "max"}\n' "$(cat "$home/login")"
  exit 0
fi
read init
echo '{"type":"control_response","response":{"subtype":"success","request_id":"cagents-usage-initialize","response":{"models":[]}}}'
read usage
echo '{"type":"control_response","response":{"subtype":"success","request_id":"cagents-usage","response":{"subscription_type":"max","rate_limits":{"limits":[{"kind":"session","group":"session","percent":25,"resets_at":null,"is_active":true}]}}}}'
cat >/dev/null
"#,
    )
    .unwrap();
    std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn sign_in(home: &Path, email: &str) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join("login"), email).unwrap();
}

/// Directory creation times order the sweep; keep them distinct.
fn create_home(homes: &Homes) -> (uuid::Uuid, std::path::PathBuf) {
    std::thread::sleep(std::time::Duration::from_millis(20));
    homes.create(Provider::Claude, None).unwrap()
}

#[test]
fn startup_restores_signed_in_homes_a_lost_registry_no_longer_names() {
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        sign_in(&root.join("home/.claude"), "system@example.com");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        install_claude(&root.join("bin/claude"));
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host::account_restore_tests::startup_restores_signed_in_homes_a_lost_registry_no_longer_names",
                "--nocapture",
            ])
            .env_clear()
            .env(CHILD, root)
            .env("HOME", root.join("home"))
            .env("SHELL", "/bin/sh")
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_RUNTIME_DIR", root)
            .env("TMPDIR", root)
            .env("PATH", format!("{}:/usr/bin:/bin", root.join("bin").display()))
            .status()
            .unwrap();
        assert!(status.success(), "account restore fixture failed");
        return;
    }
    let homes = Homes::installation().unwrap();
    let store = Store::installation().unwrap();
    let (plan, plan_home) = create_home(&homes);
    sign_in(&plan_home, "plan2@example.com");
    std::fs::write(plan_home.join("history.jsonl"), "saved conversation").unwrap();
    let (copy, copy_home) = create_home(&homes);
    sign_in(&copy_home, "plan2@example.com");
    let (system_copy, system_copy_home) = create_home(&homes);
    sign_in(&system_copy_home, "system@example.com");
    // A sign-in that never finished: its home holds no login.
    let (unfinished, unfinished_home) = create_home(&homes);
    std::fs::write(unfinished_home.join("partial"), "left alone").unwrap();
    // A sign-in given up as a duplicate of a listed account keeps its home
    // and its login, but was never an account the list lost.
    let (released, released_home) = create_home(&homes);
    sign_in(&released_home, "plan2@example.com");
    store
        .update(|registry| {
            registry.begin(Provider::Claude, released, 0);
            registry.release(released, &homes);
        })
        .unwrap();
    let parked = homes
        .path(Provider::Claude, uuid::Uuid::new_v4())
        .with_file_name(format!(".removing-{}", uuid::Uuid::new_v4()));
    sign_in(&parked, "parked@example.com");
    // The configuration directory was wiped, but an unrelated choice was
    // made since; restoring must keep it.
    let (before, ()) = store
        .update(|registry| {
            registry.set_label(&Account::system(Provider::Claude), Some("Work".into()))
        })
        .unwrap();
    let unfinished_before = std::fs::read_dir(&unfinished_home).unwrap().count();

    let host = account_lifecycle::tests::host();
    // A requested restore waits out another control operation, such as the
    // cleanup of the start its account refused, instead of reporting busy.
    {
        let gate = host.lock().unwrap().control_gate.clone();
        let writer = super::client::try_scoped_control_write(&gate).unwrap();
        let operation = crate::account::remote::Operation::Restore {
            provider: Provider::Claude,
            key: plan,
        };
        assert!(
            account_lifecycle::Reservation::for_operation(&host, &operation)
                .is_err_and(|error| error.retryable)
        );
        let waiting = {
            let host = host.clone();
            std::thread::spawn(move || {
                account_lifecycle::reserve_restore(&host, &operation).map(drop)
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(writer);
        waiting.join().unwrap().unwrap();
    }
    // A restore already asking the provider about a home - the sweep's, say
    // - neither holds up a shutdown or a chat's attachment, nor makes a
    // requested restore of the same home fail: that one waits for it.
    {
        let operation = crate::account::remote::Operation::Restore {
            provider: Provider::Claude,
            key: plan,
        };
        let sweeping = account_lifecycle::Reservation::for_operation(&host, &operation).unwrap();
        account_lifecycle::ensure_idle(&host).unwrap();
        assert!(account_lifecycle::Reservation::for_operation(&host, &operation).is_err());
        let waiting = {
            let host = host.clone();
            std::thread::spawn(move || {
                account_lifecycle::reserve_restore(&host, &operation).map(drop)
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(sweeping);
        waiting.join().unwrap().unwrap();
    }
    let mut sweep = account_lifecycle::RestoreSweep::default();
    assert!(account_lifecycle::restore_pass(
        &std::sync::Arc::downgrade(&host),
        &mut sweep
    ));
    assert!(sweep.deferred.is_empty(), "every home's check finished");
    assert_eq!(
        sweep.examined,
        [(Provider::Claude, unfinished)].into(),
        "only the home left alone is not asked about again"
    );

    let registry = store.load().unwrap();
    assert!(registry.revision() > before.revision());
    assert_eq!(
        registry.label(&Account::system(Provider::Claude)),
        Some("Work")
    );
    let row = |key| registry.find(key).cloned();
    assert!(matches!(
        row(plan),
        Some(Registered { email: Some(ref email), linked_home: None, .. }) if email == "plan2@example.com"
    ));
    assert!(
        !registry.is_archived(plan),
        "the first home of a login stays listed"
    );
    assert!(
        registry.is_archived(copy),
        "a second home of the same login is archived"
    );
    assert!(
        registry.is_archived(system_copy),
        "a home of the system login is archived"
    );
    assert!(
        row(unfinished).is_none(),
        "a home without a login is not adopted"
    );
    assert!(
        row(released).is_none() && registry.is_released(released),
        "a sign-in given up is not adopted"
    );
    assert!(released_home.join("login").exists());
    assert_eq!(
        std::fs::read_dir(&unfinished_home).unwrap().count(),
        unfinished_before
    );
    assert!(
        parked.join("login").exists(),
        "a parked removal is left alone"
    );
    assert_eq!(
        std::fs::read_to_string(plan_home.join("history.jsonl")).unwrap(),
        "saved conversation"
    );
    let listed: Vec<_> = registry
        .accounts(Some(&homes))
        .into_iter()
        .filter_map(|account| account.registry_key())
        .collect();
    assert_eq!(listed, [plan]);
    // The restored home launches again: the conversation pinned to it resumes
    // on the home that holds its history.
    // An archived copy is still registered, so its own conversations resume
    // on it too.
    for home in [&plan_home, &copy_home] {
        let mut recipe = crate::model::LaunchRecipe::shell("/".into()).unwrap();
        recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), home.to_str().unwrap().into());
        assert!(
            store
                .authorize_launch(&homes, &recipe, None)
                .unwrap()
                .is_some()
        );
    }
    assert!(
        host.lock()
            .unwrap()
            .account_lifecycle
            .lock()
            .unwrap()
            .is_empty(),
        "no reservation outlives the sweep"
    );
}
