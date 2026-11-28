//! One-shot connection loss after a native-evidence operation succeeds.

use std::{
    fs,
    path::{Path, PathBuf},
};

const DROP_RUN_NOW: &str = "drop-automation-run-now-reply";

fn regular_file(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.is_file())
}

fn armed_marker(fixture: &Path, home: &Path, runtime: &Path) -> Option<PathBuf> {
    let task = fixture.parent()?;
    let marker = fixture.join(DROP_RUN_NOW);
    (regular_file(&marker)
        && fixture.file_name().is_some_and(|name| name == "fixture")
        && home == fixture.join("home")
        && runtime == task.join("rt")
        && fixture
            .canonicalize()
            .is_ok_and(|resolved| resolved == fixture)
        && home.canonicalize().is_ok_and(|resolved| resolved == home)
        && runtime
            .canonicalize()
            .is_ok_and(|resolved| resolved == runtime)
        && regular_file(&task.join(".native-evidence"))
        && regular_file(&task.join(".cagents-fixture-root")))
    .then_some(marker)
}

fn claim(marker: &Path) -> bool {
    // Unlink is the claim: concurrent requests can consume the marker once.
    fs::remove_file(marker).is_ok()
}

#[cfg(feature = "development")]
pub(super) fn drop_run_now_reply(peer_pid: u32) -> bool {
    let (Some(fixture), Some(home), Some(runtime)) = (
        std::env::var_os("CAGENTS_EVIDENCE_FIXTURE"),
        std::env::var_os("HOME"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    ) else {
        return false;
    };
    let Some(marker) = armed_marker(Path::new(&fixture), Path::new(&home), Path::new(&runtime))
    else {
        return false;
    };
    let ui_name = crate::companion::Product::Ui.file_name();
    if !crate::process::program_of(peer_pid)
        .is_ok_and(|(exe, _)| exe.file_name() == Some(ui_name.as_os_str()))
        || !claim(&marker)
    {
        return false;
    }
    tracing::info!("native-evidence dropped a RunNow reply");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_marked_native_task_can_arm_one_drop() {
        let directory = tempfile::tempdir().unwrap();
        let task = directory.path().canonicalize().unwrap();
        let fixture = task.join("fixture");
        let home = fixture.join("home");
        let runtime = task.join("rt");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir(&runtime).unwrap();
        let marker = fixture.join(DROP_RUN_NOW);
        fs::write(&marker, []).unwrap();

        assert!(armed_marker(&fixture, &home, &runtime).is_none());
        fs::write(task.join(".native-evidence"), []).unwrap();
        fs::write(task.join(".cagents-fixture-root"), []).unwrap();
        assert!(armed_marker(&fixture, &task, &runtime).is_none());
        assert!(armed_marker(&fixture, &home, &task).is_none());
        assert_eq!(
            armed_marker(&fixture, &home, &runtime),
            Some(marker.clone())
        );
        assert!(claim(&marker));
        assert!(!claim(&marker));

        std::os::unix::fs::symlink(task.join(".native-evidence"), &marker).unwrap();
        assert!(armed_marker(&fixture, &home, &runtime).is_none());
    }
}
