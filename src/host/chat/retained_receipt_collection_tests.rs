//! The runtime collector releases the final transcripts of exactly the actors
//! whose retirement the daemon committed and whose receipts prove it, across
//! every page of the actor census. No helper or provider runs: each actor's
//! receipts are published by the writers a helper uses, over processes that
//! have ended or, where a stage needs one, the test process itself.
use super::*;
use crate::chat::runtime::retained_receipt_fixture::{self as fixture, Stage};
use std::{fs, path::PathBuf};

/// Where an actor stands with the daemon and its receipts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    /// Committed retirement and verified receipts: the only case compacted.
    Retired,
    /// Verified receipts whose retirement the daemon has not committed.
    Uncommitted,
    /// Committed, while the helper is still running.
    Running,
    /// Committed; the helper ended but its provider may still run.
    ExitedUnproved,
    /// Committed; both ended, but the helper's discovery is still published.
    DiscoveryPending,
    /// Committed and compacted by an earlier pass.
    Compacted,
    /// Verified receipts whose committed retirement row cannot be read.
    Unreadable,
}

const INELIGIBLE: [Case; 6] = [
    Case::Uncommitted,
    Case::Running,
    Case::ExitedUnproved,
    Case::DiscoveryPending,
    Case::Compacted,
    Case::Unreadable,
];

/// The census in actor order, which is the order it is paged in 128s.
///
/// Pages one and two interleave ineligible actors with retired ones, and a
/// retired actor sits on each side of their boundary, so a page that drops its
/// first or last row leaves a retired actor uncompacted. Page three holds no
/// retired actor at all, and page four is a partial page that starts with one,
/// so a pass that stops at an empty page, or skips the row after one, falls
/// short of all 300.
fn census() -> Vec<Case> {
    let mut ineligible = INELIGIBLE.iter().copied().cycle();
    let mut cases = (0..256)
        .map(|row| match row % 4 {
            1 => ineligible.next().unwrap(),
            _ => Case::Retired,
        })
        .collect::<Vec<_>>();
    cases.extend((0..128).map(|_| ineligible.next().unwrap()));
    let retired = cases.iter().filter(|case| **case == Case::Retired).count();
    cases.extend(std::iter::repeat_n(Case::Retired, 300 - retired));
    cases
}

fn reaped_process() -> crate::daemon::holder_registry::Process {
    let mut child = std::process::Command::new("/bin/cat")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let identity = crate::daemon::holder_registry::Process::read(child.id() as i32).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    identity
}

#[test]
fn collection_compacts_every_committed_verified_retirement_and_nothing_else() {
    // A supervisor refuses an --exact that selects nothing, bounds a hang and
    // reaps whatever the body leaves behind.
    if crate::retained_fixture::run_isolated() {
        return;
    }
    const ROOT: &str = "CAGENTS_RECEIPT_COLLECTION_FIXTURE";
    let Some(root) = std::env::var_os(ROOT).map(PathBuf::from) else {
        // The receipts live in the runtime directory and the reader must hold
        // the daemon lease, so the body runs in a process of its own.
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        for path in ["home", "run", "data", "config", "cache"] {
            fs::create_dir_all(directory.path().join(path)).unwrap();
        }
        let exact = thread::current().name().unwrap().to_owned();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &exact, "--nocapture"])
            .env_clear()
            .envs(crate::retained_fixture::owner_env())
            .env(ROOT, directory.path())
            .env("HOME", directory.path().join("home"))
            .env("XDG_RUNTIME_DIR", directory.path().join("run"))
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .env("XDG_CACHE_HOME", directory.path().join("cache"))
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated receipt collection failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed"),
            "the isolated body ran: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    };
    let proof = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("receipt-collection"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: crate::model::HostIncarnation::new("collector"),
    };
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let mut state = HostState::new(proof.clone());
    let store = root.join("registry.sqlite3");
    state.install_model(super::super::model::Registry::at(&store).unwrap());
    let host: SharedHost = Arc::new(HostLock::new(state));
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let ended = reaped_process();
    let scope = "fixture has no account scope";
    let census = census();
    let mut actors = Vec::new();
    for (row, case) in census.iter().copied().enumerate() {
        // Actor ids in row order, so the census pages in exactly this layout.
        let instance = uuid::Uuid::from_u128(row as u128 + 1);
        let stage = match case {
            Case::Retired | Case::Uncommitted | Case::Unreadable => Stage::Verified,
            Case::Running => Stage::Running,
            Case::ExitedUnproved => Stage::ExitedUnproved,
            Case::DiscoveryPending => Stage::DiscoveryPending,
            Case::Compacted => Stage::Compacted,
        };
        let actor = fixture::publish(instance, &proof, &ended, stage);
        let mut registry = registry.lock().unwrap();
        registry
            .register_native_actor_ownership(instance, &actor.record, &actor.source, Err(scope))
            .unwrap();
        if case != Case::Uncommitted {
            registry
                .commit_native_actor_retirement(
                    instance,
                    &actor.record,
                    &actor.source,
                    Err(scope),
                    actor.source.launch.session_id.as_deref(),
                )
                .unwrap();
        }
        actors.push((instance, case, actor));
    }
    let raw = rusqlite::Connection::open(&store).unwrap();
    for (instance, _, _) in actors
        .iter()
        .filter(|(_, case, _)| *case == Case::Unreadable)
    {
        raw.execute(
            "UPDATE native_actor_retirements SET receipt = '{' WHERE actor = ?1",
            [instance.to_string()],
        )
        .unwrap();
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_retirement(*instance)
                .is_err()
        );
    }
    drop(raw);
    let read = |actor: &fixture::Actor| {
        (
            fs::read(&actor.final_receipt).unwrap(),
            fs::read(&actor.retirement_receipt).unwrap(),
        )
    };
    let before = actors
        .iter()
        .map(|(_, _, actor)| read(actor))
        .collect::<Vec<_>>();

    let released = retained_actor::compact_retired_receipts(&host).unwrap();

    let mut freed = 0u64;
    let mut compacted = 0;
    for ((instance, case, actor), (final_before, retired_before)) in actors.iter().zip(&before) {
        let (final_after, retired_after) = read(actor);
        assert_eq!(
            &retired_after, retired_before,
            "{case:?} actor {instance}: the retirement proof is never rewritten"
        );
        if *case == Case::Retired {
            assert!(
                fixture::compacted(&actor.final_receipt),
                "actor {instance}: a committed, verified retirement is compacted"
            );
            assert!(final_after.len() < final_before.len());
            freed += (final_before.len() - final_after.len()) as u64;
            compacted += 1;
        } else {
            assert_eq!(
                &final_after, final_before,
                "{case:?} actor {instance}: the receipt stays byte for byte"
            );
        }
    }
    assert_eq!(compacted, 300, "every page of the census was visited");
    assert_eq!(released, freed, "the pass reports the bytes it released");
    assert!(
        Handle::inspect_retained_retirement(&actors[0].2.record, &proof).is_ok_and(
            |inspection| matches!(inspection, chat::runtime::RetirementInspection::Verified(_))
        ),
        "a compacted receipt still proves its retirement"
    );

    // A second pass finds nothing left to release and changes nothing.
    let after = actors
        .iter()
        .map(|(_, _, actor)| read(actor))
        .collect::<Vec<_>>();
    assert_eq!(retained_actor::compact_retired_receipts(&host).unwrap(), 0);
    for ((_, _, actor), previous) in actors.iter().zip(&after) {
        assert_eq!(&read(actor), previous);
    }
}

#[test]
fn census_layout_straddles_every_page_boundary() {
    let census = census();
    let page = |index: usize| &census[index * 128..((index + 1) * 128).min(census.len())];
    assert_eq!(
        census.iter().filter(|case| **case == Case::Retired).count(),
        300
    );
    assert_eq!(census.len().div_ceil(128), 4);
    for (row, retired) in [
        (127, true),
        (128, true),
        (255, true),
        (256, false),
        (383, false),
        (384, true),
    ] {
        assert_eq!(census[row] == Case::Retired, retired, "row {row}");
    }
    assert!(page(2).iter().all(|case| *case != Case::Retired));
    assert!(page(3).iter().all(|case| *case == Case::Retired));
    for case in INELIGIBLE {
        assert!(
            page(0).contains(&case) && page(1).contains(&case) && page(2).contains(&case),
            "{case:?} appears on every page before the last"
        );
    }
}
