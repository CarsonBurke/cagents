use super::Relocation;
use std::path::{Path,PathBuf};

// A Locate choice must not attach unrelated linked checkouts to this repo.
// Verify both the old marker and the moved admin directory's backlink before
// handing an explicit linked path to `git worktree repair`.
fn read_marker(path:&Path)->Option<String> {
    use std::{io::Read,os::unix::fs::OpenOptionsExt};
    let file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).ok()?;
    if !file.metadata().ok()?.is_file() { return None; }
    let mut text=String::new();
    file.take(4097).read_to_string(&mut text).ok()?;
    (text.len()<=4096).then_some(text)
}

pub(crate) fn belongs_to_relocated_worktree(path: &Path, relocation: &Relocation) -> bool {
    let Some(marker)=read_marker(&path.join(".git")) else {return false;};
    let Some(admin) = marker.trim().strip_prefix("gitdir: ") else {
        return false;
    };
    let admin = Path::new(admin);
    let admin = if admin.is_absolute() {
        admin.to_path_buf()
    } else {
        path.join(admin)
    };
    if !admin.starts_with(&relocation.repository) {
        return false;
    }
    let admin = relocation.mapped(&admin);
    let Some(backlink)=read_marker(&admin.join("gitdir")) else {return false;};
    relocation.mapped(Path::new(backlink.trim())) == path.join(".git")
}

// `git worktree repair` does not update an external Git directory's
// core.worktree. Change only the exact previous checkout chosen by Locate.
pub(crate) fn repair_checkout_config(relocation: &Relocation) -> Result<(), String> {
    use crate::repository::{CommandSpec, ProcessExecutor, SystemProcessExecutor};
    let (admin, common) = crate::project_location::checkout_identity(&relocation.new_root)
        .ok_or("The selected checkout no longer has readable Git metadata")?;
    if common != relocation.destination {
        return Err("The selected checkout changed repositories while locating it".into());
    }
    if admin != common {
        return Ok(());
    }
    let command = || {
        CommandSpec::new("git")
            .arg("--git-dir")
            .arg(&relocation.destination)
            .cwd(&relocation.new_root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .timeout(std::time::Duration::from_secs(15))
            .output_limits(64 * 1024, 64 * 1024)
    };
    let read = command().args(["config", "--local", "--null", "--get", "core.worktree"]);
    let output = SystemProcessExecutor
        .execute(&read, &crate::repository::CancellationToken::default())
        .map_err(|error| error.to_string())?;
    if output.exit_code == Some(1) {
        return Ok(());
    }
    let output = output
        .require_success(&read.program)
        .map_err(|error| error.to_string())?;
    let bytes = output
        .stdout
        .strip_suffix(&[0])
        .ok_or("Git returned an invalid checkout setting")?;
    let configured = PathBuf::from(
        String::from_utf8(bytes.to_vec()).map_err(|_| "Git checkout setting is not valid UTF-8")?,
    );
    let absolute = if configured.is_absolute() {
        configured
    } else {
        relocation.destination.join(configured)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    if normalized == relocation.new_root {
        return Ok(());
    }
    if normalized != relocation.old_root {
        return Err(format!(
            "This Git directory names another checkout: {}. Its setting was preserved.",
            normalized.display()
        ));
    }
    let write = command()
        .args(["config", "--local", "core.worktree"])
        .arg(&relocation.new_root);
    SystemProcessExecutor
        .execute(&write, &crate::repository::CancellationToken::default())
        .and_then(|output| output.require_success(&write.program))
        .map_err(|error| error.to_string())?;
    Ok(())
}


/// The caller holds the exact project's scoped location reservation. Perform
/// repairs only after registry instance validation, never in a stale chooser.
pub fn repair(relocation: &Relocation, worktrees: &[PathBuf]) -> Result<(), String> {
    repair_checkout_config(relocation)?;
    use crate::repository::{CommandSpec, ProcessExecutor, SystemProcessExecutor};
    let mut command = CommandSpec::new("git").cwd(&relocation.new_root)
        .args(["worktree", "repair"])
        .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR").env_remove("GIT_INDEX_FILE")
        .timeout(std::time::Duration::from_secs(15)).output_limits(64*1024,64*1024);
    let mut known=worktrees.to_vec();
    // The UI's loaded tree may omit closed linked checkouts. Git's own
    // bounded admin records supply candidates; the exact backlink check below
    // still gates each explicit repair argument.
    if let Ok(entries)=std::fs::read_dir(relocation.destination.join("worktrees")) {
        for entry in entries.take(4096).flatten() {
            if let Some(marker)=read_marker(&entry.path().join("gitdir")) {
                if let Some(path)=Path::new(marker.trim()).parent() { known.push(path.to_owned()); }
            }
        }
    }
    known.sort();known.dedup();
    for path in &known {
        let path = relocation.mapped(path);
        if path != relocation.new_root && belongs_to_relocated_worktree(&path,relocation) { command=command.arg(path); }
    }
    if let Err(error)=SystemProcessExecutor.execute(&command,&crate::repository::CancellationToken::default()).and_then(|output|output.require_success(&command.program)) {
        // Separately missing linked checkouts remain actionable after primary repair.
        tracing::warn!(%error,"some linked worktrees could not be repaired after locating project");
    }
    Ok(())
}
