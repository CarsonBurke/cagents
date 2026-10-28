//! Shared real-filesystem operands for the owning HOST and UI inventory tests.
use super::*;
use std::time::{Duration, Instant};

pub(crate) fn covered_monitor(plan: Arc<Project>) -> Monitor {
    let monitor = Monitor::start(|| {}).unwrap();
    let id = plan.id;
    monitor.configure(vec![plan]);
    wait_for(&monitor, id, "initial coverage", |change| {
        change.coverage == Some(true) && change.error.is_none()
    });
    monitor
}

/// Replace a private fixture's linked administration with prepared copies.
/// The caller closes its Monitor first, so the following index edits have no
/// native coverage. The two renames are atomic individually, not an exchange.
pub(crate) fn replace_linked_metadata(plan: &Project) -> Change {
    fn copy_directory(source: &Path, target: &Path) {
        std::fs::create_dir(target).unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                copy_directory(&entry.path(), &target.join(entry.file_name()));
            } else {
                assert!(kind.is_file(), "fixture metadata must be regular files");
                std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
            }
        }
    }
    let parent = plan.common_dir.parent().unwrap();
    let current = plan.common_dir.join("worktrees");
    let prepared = parent.join("prepared-linked-metadata");
    let retired = parent.join("retired-linked-metadata");
    copy_directory(&current, &prepared);
    std::fs::rename(&current, &retired).unwrap();
    std::fs::rename(&prepared, &current).unwrap();
    // Use the real producer's interpretation of the first rename's paths,
    // rather than inventing the primary-only topology flags in each consumer.
    // This is a constructed event operand, not an observed native callback.
    plan.change(
        &Event::new(EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both,
        )))
        .add_path(current)
        .add_path(retired),
    )
    .expect("shared linked metadata replacement changes topology")
}

/// All fixture Git/filesystem mutations precede this fresh watcher. Register
/// the old inventory first, then require the new inventory's linked ACK before
/// an edit in a newly tracked directory. Old callbacks retain separate state
/// and cannot satisfy this fresh Monitor's observations.
pub(crate) fn reconfigure_then_observe(
    old: Arc<Project>,
    new: Arc<Project>,
    linked: WorktreeId,
    file: &Path,
) {
    let id = new.id;
    assert_eq!(old.id, id);
    let old_linked = old.worktrees.iter().find(|row| row.id == linked).unwrap();
    let new_linked = new.worktrees.iter().find(|row| row.id == linked).unwrap();
    assert_eq!(old_linked.root, new_linked.root);
    assert_eq!(old_linked.git_dir, new_linked.git_dir);
    assert_eq!(old_linked.tracks(file), None);
    assert_eq!(new_linked.tracks(file), Some(Tracked::File));
    for unchanged in old.worktrees.iter().filter(|row| row.id != linked) {
        let current = new
            .worktrees
            .iter()
            .find(|row| row.id == unchanged.id)
            .unwrap();
        assert_eq!(current, unchanged);
        assert!(
            Arc::ptr_eq(&current.census, &unchanged.census),
            "equal censuses remain shared after full revalidation"
        );
    }
    let monitor = covered_monitor(old);
    monitor.configure(vec![new]);
    wait_for(
        &monitor,
        id,
        "changed linked inventory acknowledged",
        |change| {
            change.coverage == Some(true)
                && change.error.is_none()
                && change.worktrees.contains(&linked)
        },
    );
    std::fs::write(file, "native edit after topology inventory recovery\n").unwrap();
    wait_for(
        &monitor,
        id,
        "edit in recovered linked inventory",
        |change| {
            change.worktrees.contains(&linked)
                && change.error.is_none()
                && change.coverage != Some(false)
                && !change.topology
        },
    );
}

fn wait_for(monitor: &Monitor, id: ProjectId, phase: &str, predicate: impl Fn(&Change) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = None;
    loop {
        if let Some(change) = monitor.drain().remove(&id) {
            let ready = predicate(&change);
            last = Some(change);
            if ready {
                return;
            }
        }
        assert!(Instant::now() < deadline, "{phase}: last change {last:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
