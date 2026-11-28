use super::{Staged, Staging, stage_transcript};
use crate::agent::Provider;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const ENTRY_LIMIT: usize = 262_144;

pub(crate) struct Inventory {
    provider: Provider,
    home: PathBuf,
    transcripts: BTreeMap<(Option<PathBuf>, Uuid), PathBuf>,
}

impl Inventory {
    pub(crate) fn read(provider: Provider, home: &Path) -> Result<Self, String> {
        let home = home.canonicalize().map_err(|error| error.to_string())?;
        crate::secure_file::validate_owned_directory(&home).map_err(|error| error.to_string())?;
        let mut inventory = Self {
            provider,
            home,
            transcripts: BTreeMap::new(),
        };
        let mut budget = ENTRY_LIMIT;
        match provider {
            Provider::Claude => inventory.scan(Path::new("projects"), 1, false, &mut budget)?,
            Provider::Codex => {
                // Match the exact-thread reader, including flat legacy rollouts.
                inventory.scan(Path::new("sessions"), 3, true, &mut budget)?;
                inventory.scan(Path::new("archived_sessions"), 0, true, &mut budget)?;
            }
            _ => return Err("This provider does not support account history transfer".into()),
        }
        Ok(inventory)
    }

    fn scan(
        &mut self,
        directory: &Path,
        depth: usize,
        files_here: bool,
        budget: &mut usize,
    ) -> Result<(), String> {
        let path = self.home.join(directory);
        crate::secure_file::reject_symlink_components(&path).map_err(|error| error.to_string())?;
        let entries = match fs::read_dir(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            result => result.map_err(|error| error.to_string())?,
        };
        for entry in entries {
            *budget = budget
                .checked_sub(1)
                .ok_or("Account history is too large to inspect")?;
            let entry = entry.map_err(|error| error.to_string())?;
            let name = entry.file_name();
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            let relative = directory.join(&name);
            if depth > 0 && kind.is_symlink() {
                return Err("An account history entry is a symbolic link".into());
            }
            if depth > 0 && kind.is_dir() {
                self.scan(&relative, depth - 1, true, budget)?;
            } else if files_here && let Some(id) = self.provider.session_file_id(&relative) {
                if !kind.is_file() {
                    return Err("An account transcript is not a regular file".into());
                }
                // Claude looks up an ID inside its working directory's project.
                // Codex looks up a thread ID across all dated and archived rollouts.
                let project = (self.provider == Provider::Claude).then(|| directory.to_owned());
                let id = Uuid::parse_str(&id).map_err(|error| error.to_string())?;
                if self.transcripts.insert((project, id), relative).is_some() {
                    return Err(
                        "The account has more than one transcript for a conversation".into(),
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn count(&self) -> usize {
        self.transcripts.len()
    }

    pub(crate) fn stage_to(&self, target: &Self) -> Result<Vec<Staged>, String> {
        if self.provider != target.provider
            || self.home.starts_with(&target.home)
            || target.home.starts_with(&self.home)
        {
            return Err("Choose a distinct account home for the same provider".into());
        }
        let mut cleaned = BTreeSet::new();
        self.transcripts
            .iter()
            .map(|(identity, relative)| {
                if target.transcripts.get(identity).is_some_and(|existing| existing != relative) {
                    return Err("The target account has a transcript for this conversation at a different path".into());
                }
                let destination = target.home.join(relative);
                let clean = destination.parent().is_some_and(|parent| cleaned.insert(parent.to_owned()));
                stage_transcript(self.home.join(relative), destination, Staging::Idle { clean })
                    .map_err(|error| format!("Could not prepare a transcript on the target account: {error}"))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_counts_top_level_project_transcripts_and_preserves_project_paths() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        let project = Path::new("projects/-work-project");
        let id = Uuid::new_v4();
        let relative = project.join(format!("{id}.jsonl"));
        fs::create_dir_all(source.join(project).join("subagents")).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join(&relative), b"conversation").unwrap();
        fs::write(
            source
                .join(project)
                .join("subagents")
                .join(format!("{id}.jsonl")),
            b"nested",
        )
        .unwrap();
        fs::write(source.join("fixture-login"), b"private fixture login").unwrap();
        fs::write(source.join("projects/settings.json"), b"config").unwrap();

        let inventory = Inventory::read(Provider::Claude, &source).unwrap();
        assert_eq!(inventory.count(), 1);
        for copy in inventory
            .stage_to(&Inventory::read(Provider::Claude, &target).unwrap())
            .unwrap()
        {
            copy.commit().unwrap();
        }
        let identical = inventory
            .stage_to(&Inventory::read(Provider::Claude, &target).unwrap())
            .unwrap();
        assert_eq!(identical.len(), 1);
        assert!(identical[0].temporary.is_none());
        for copy in identical {
            copy.commit().unwrap();
        }
        assert_eq!(fs::read(target.join(&relative)).unwrap(), b"conversation");
        assert_eq!(fs::read(source.join(&relative)).unwrap(), b"conversation");
        assert!(!target.join("fixture-login").exists());
        assert!(!target.join(project).join("subagents").exists());
        std::os::unix::fs::symlink(source.join(project), source.join("projects/-alias")).unwrap();
        assert!(
            Inventory::read(Provider::Claude, &source)
                .err()
                .unwrap()
                .contains("symbolic link")
        );
    }

    #[test]
    fn codex_counts_created_and_archived_rollouts_and_rejects_other_target_paths() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        let day = Path::new("sessions/2026/11/27");
        let archive = Path::new("archived_sessions");
        for home in [&source, &target] {
            fs::create_dir_all(home.join(day)).unwrap();
            fs::create_dir(home.join(archive)).unwrap();
        }
        let active = format!("rollout-2026-11-27T10-00-00-{}.jsonl", Uuid::new_v4());
        let archived = format!("rollout-2026-11-27T11-00-00-{}.jsonl", Uuid::new_v4());
        let legacy = format!("rollout-2026-11-27T09-00-00-{}.jsonl", Uuid::new_v4());
        fs::write(source.join("sessions").join(&legacy), b"legacy").unwrap();
        fs::write(source.join(day).join(&active), b"active").unwrap();
        fs::write(source.join(archive).join(&archived), b"archived").unwrap();
        fs::write(target.join(archive).join(&active), b"existing").unwrap();
        let inventory = Inventory::read(Provider::Codex, &source).unwrap();
        assert_eq!(inventory.count(), 3);
        assert!(
            inventory
                .stage_to(&Inventory::read(Provider::Codex, &target).unwrap())
                .is_err()
        );
        assert!(!target.join(archive).join(&archived).exists());
        assert_eq!(
            fs::read(target.join(archive).join(&active)).unwrap(),
            b"existing"
        );

        fs::remove_file(target.join(archive).join(&active)).unwrap();
        for copy in inventory
            .stage_to(&Inventory::read(Provider::Codex, &target).unwrap())
            .unwrap()
        {
            copy.commit().unwrap();
        }
        assert_eq!(fs::read(target.join(day).join(&active)).unwrap(), b"active");
        assert_eq!(
            fs::read(target.join("sessions").join(&legacy)).unwrap(),
            b"legacy"
        );
        assert_eq!(
            fs::read(target.join(archive).join(&archived)).unwrap(),
            b"archived"
        );
    }
}
