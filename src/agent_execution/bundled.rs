//! Install the instruction-only bundle through provider-native skill discovery.
//! Callers hold the account lifecycle lease across installation and child spawn.
use crate::agent::Provider;
use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::{Path, PathBuf},
};

const CONTENT: &str = include_str!("../../assets/skills/cagents-agents/SKILL.md");
const NAME: &str = "cagents-agents";

pub(crate) fn install(provider: Option<Provider>, environment: &BTreeMap<String, String>) {
    let Some(home) = environment
        .get("HOME")
        .map(Path::new)
        .filter(|home| home.is_dir())
    else {
        return;
    };
    let mut roots = Vec::new();
    if provider != Some(Provider::Claude) {
        // Both Codex and OMP document the user .agents skill provider. This
        // remains under the process HOME, including an explicitly overridden one.
        roots.push((home.join(".agents"), true));
    }
    if provider.is_none() || provider == Some(Provider::Claude) {
        let selected = environment
            .get("CLAUDE_CONFIG_DIR")
            .filter(|root| !root.is_empty());
        roots.push(match selected {
            Some(root) => (PathBuf::from(root), false),
            None => (home.join(".claude"), true),
        });
    }
    for (root, create_root) in roots {
        if let Err(error) = install_at(&root, create_root) {
            tracing::warn!(path = %root.join("skills").join(NAME).join("SKILL.md").display(), %error,
                "cannot install bundled agent skill; cagents --json agent remains available");
        }
    }
}

fn install_at(root: &Path, create_root: bool) -> io::Result<()> {
    if create_root {
        directory(root)?;
    }
    // Never create a selected account root. Account removal cannot be undone
    // by skill installation, including a missing linked configuration directory.
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "provider configuration root is absent",
        ));
    }
    let skills = root.join("skills");
    directory(&skills)?;
    let skill = skills.join(NAME);
    directory(&skill)?;
    let destination = skill.join("SKILL.md");
    match std::fs::read(&destination) {
        Ok(content) if content == CONTENT.as_bytes() => return Ok(()),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "existing skill differs from the bundled version; preserving user content",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut pending = tempfile::NamedTempFile::new_in(&skill)?;
    pending.write_all(CONTENT.as_bytes())?;
    pending.as_file().sync_all()?;
    match pending.persist_noclobber(&destination) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            // Two simultaneous launches may install the identical bundle.
            if std::fs::read(&destination)? == CONTENT.as_bytes() {
                Ok(())
            } else {
                Err(error.error)
            }
        }
        Err(error) => Err(error.error),
    }
}

fn directory(path: &Path) -> io::Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

/// Use the CLI paired with this daemon, without persisting the installation
/// path into user-authored recipes or replacing the rest of their PATH.
pub(crate) fn matched_path(current: Option<&str>) -> Option<String> {
    let executable = std::env::current_exe().ok()?;
    let directory = executable.parent()?;
    if !directory.join("cagents").is_file() {
        return None;
    }
    let mut paths = vec![directory.to_owned()];
    paths.extend(
        std::env::split_paths(current.unwrap_or_default()).filter(|path| path != directory),
    );
    std::env::join_paths(paths).ok()?.into_string().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_locations_follow_process_home_and_selected_claude_account() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("other-home");
        let selected = directory.path().join("selected-claude");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&selected).unwrap();
        let environment = BTreeMap::from([
            ("HOME".into(), home.to_str().unwrap().into()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                selected.to_str().unwrap().into(),
            ),
        ]);
        install(Some(Provider::Claude), &environment);
        assert_eq!(
            std::fs::read_to_string(selected.join("skills/cagents-agents/SKILL.md")).unwrap(),
            CONTENT
        );
        assert!(!home.join(".claude").exists());
        assert!(!home.join(".agents").exists());
        for provider in [Provider::Codex, Provider::Omp] {
            install(Some(provider), &environment);
        }
        assert_eq!(
            std::fs::read_to_string(home.join(".agents/skills/cagents-agents/SKILL.md")).unwrap(),
            CONTENT
        );
    }

    #[test]
    fn user_content_is_preserved_and_removed_account_is_never_recreated() {
        let directory = tempfile::tempdir().unwrap();
        install_at(directory.path(), false).unwrap();
        let path = directory.path().join("skills/cagents-agents/SKILL.md");
        std::fs::write(&path, "user-authored skill").unwrap();
        assert_eq!(
            install_at(directory.path(), false).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "user-authored skill"
        );
        let removed = directory.path().join("removed");
        assert!(install_at(&removed, false).is_err());
        assert!(!removed.exists());
    }
}
