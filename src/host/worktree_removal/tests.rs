use super::*;
use crate::{
    agent::Provider,
    agent_execution::{Destination, Origin, Spec, Submit, Surface, Target},
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, ProjectKind, TerminalId},
    workspace_model::ModelEdit,
};

fn host() -> SharedHost {
    Arc::new(super::super::state::HostLock::new(
        super::super::state::HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("worktree-removal"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("fixture"),
        }),
    ))
}

/// A root holding `worktree/child` and a sibling `other`.
fn tree() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("worktree/child")).unwrap();
    std::fs::create_dir(root.path().join("other")).unwrap();
    let worktree = root.path().join("worktree").canonicalize().unwrap();
    (root, worktree)
}

#[test]
fn launches_inside_a_reserved_worktree_are_refused_until_it_is_released() {
    let (root, worktree) = tree();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&worktree, &alias).unwrap();
    let host = host();
    let reservation = Reservation::install(&host, &alias).unwrap();
    assert_eq!(reservation.path(), worktree);
    for inside in [
        worktree.clone(),
        worktree.join("child"),
        worktree.join("not/yet/created"),
        alias.join("child"),
    ] {
        let error = ensure_outside(&host, &inside).unwrap_err();
        assert_eq!(error.message, STARTING_INSIDE, "{}", inside.display());
    }
    ensure_outside(&host, &root.path().join("other")).unwrap();
    ensure_outside(&host, root.path()).unwrap();
    assert!(ensure_outside(&host, Path::new("relative")).is_err());
    drop(reservation);
    ensure_outside(&host, &worktree.join("child")).unwrap();
    ensure_outside(&host, Path::new("relative")).unwrap();
}

#[test]
fn a_moved_worktree_still_refuses_launches_by_inode() {
    let (root, worktree) = tree();
    let host = host();
    let _reservation = Reservation::install(&host, &worktree).unwrap();
    let pinned = open_directory(&worktree.join("child"), 0).unwrap();
    let moved = root.path().join("moved");
    std::fs::rename(&worktree, &moved).unwrap();
    assert!(ensure_outside_directory(&host, &pinned).is_err());
    assert!(ensure_outside(&host, &moved.join("child")).is_err());
    let unrelated = open_directory(&root.path().join("other"), 0).unwrap();
    ensure_outside_directory(&host, &unrelated).unwrap();
}

#[test]
fn removals_nested_in_or_around_a_reserved_worktree_are_refused() {
    let (root, worktree) = tree();
    let host = host();
    let _reservation = Reservation::install(&host, &worktree).unwrap();
    for overlapping in [worktree.clone(), worktree.join("child"), root.path().into()] {
        let error = Reservation::install(&host, &overlapping)
            .err()
            .expect("an overlapping removal is refused");
        assert!(error.message.contains("already being removed"));
    }
    Reservation::install(&host, &root.path().join("other")).unwrap();
}

#[test]
fn installation_waits_for_launches_already_admitted() {
    let (_root, worktree) = tree();
    let host = host();
    let gate = host.lock().unwrap().control_gate.clone();
    let admitted = gate.read().unwrap();
    assert!(Reservation::install_within(&host, &worktree, std::time::Duration::ZERO).is_err());
    // Installation retries while the request already admitted finishes.
    std::thread::scope(|scope| {
        let installing = scope.spawn(|| Reservation::install(&host, &worktree).map(drop));
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(admitted);
        installing.join().unwrap().unwrap();
    });
}

#[test]
fn the_process_scan_finds_working_directories_and_open_files() {
    let (root, worktree) = tree();
    let scan = || occupant(Path::new("/proc"), &worktree, std::process::id()).unwrap();
    assert_eq!(scan(), None);
    let mut resident = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(worktree.join("child"))
        .spawn()
        .unwrap();
    assert_eq!(scan(), Some((resident.id(), "sleep".into())));
    resident.kill().unwrap();
    resident.wait().unwrap();
    assert_eq!(scan(), None);
    std::fs::write(worktree.join("child/notes"), "draft").unwrap();
    let mut writer = std::process::Command::new("sh")
        .args(["-c", "exec 3<child/notes; cd ../other; exec sleep 30"])
        .current_dir(&worktree)
        .spawn()
        .unwrap();
    // The shell moves out of the worktree before exec, keeping the file open.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::fs::read_link(format!("/proc/{}/cwd", writer.id())).ok()
        != Some(root.path().join("other").canonicalize().unwrap())
    {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(scan().map(|(pid, _)| pid), Some(writer.id()));
    writer.kill().unwrap();
    writer.wait().unwrap();
    assert_eq!(scan(), None);
}

#[test]
fn hosted_and_durable_work_inside_the_worktree_refuses_its_removal() {
    let (root, worktree) = tree();
    let host = host();
    let mut registry =
        super::super::model::Registry::at(&root.path().join("registry.sqlite3")).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: root.path().into(),
            name: "Removal fixture".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let registry = Arc::new(Mutex::new(registry));
    host.lock().unwrap().execution_registry = Some(Arc::clone(&registry));
    let reservation = Reservation::install(&host, &worktree).unwrap();
    reservation.ensure_unoccupied(&host).unwrap();

    let id = TerminalId::new();
    let session = crate::chat::fresh_session(
        Provider::Claude,
        id,
        worktree.join("child"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    host.lock().unwrap().chat_spawning.insert(id, session);
    let error = reservation.ensure_unoccupied(&host).unwrap_err();
    assert!(error.starts_with("A starting chat"), "{error}");
    host.lock().unwrap().chat_spawning.remove(&id);

    let submit = |directory: PathBuf, key: &str| {
        registry
            .lock()
            .unwrap()
            .submit_agent(
                &Origin::User,
                &Submit {
                    name: None,
                    destination: Destination::New(Spec {
                        provider: Provider::Claude,
                        account: None,
                        target: Target {
                            repository: root.path().into(),
                            directory,
                            placement: None,
                        },
                        surface: Surface::Chat,
                        model: None,
                        permission_mode: None,
                        reasoning_effort: None,
                    }),
                    prompt: "Accepted work".into(),
                    idempotency_key: key.into(),
                },
                1,
            )
            .unwrap()
    };
    submit(root.path().join("other"), "elsewhere");
    reservation.ensure_unoccupied(&host).unwrap();
    submit(worktree.join("child"), "inside");
    let error = reservation.ensure_unoccupied(&host).unwrap_err();
    assert!(error.starts_with("Agent work"), "{error}");
}
