//! Transfer provider histories between CLI homes without login stores or configuration.
//! Exact switching copies one unambiguous conversation; explicit account moves retain all histories.
use crate::{agent::Provider, chat::Session};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const MAX_TRANSCRIPT: u64 = 256 * 1024 * 1024;
const ENTRY_BUDGET: usize = 262_144;

pub fn prepare(source: &Session, replacement: &Session) -> Result<(), String> {
    transfer(source, replacement, true)
}

pub fn preflight(source: &Session, replacement: &Session) -> Result<(), String> {
    transfer(source, replacement, false)
}

fn transfer(source: &Session, replacement: &Session, commit: bool) -> Result<(), String> {
    let provider = source.launch.provider;
    let id = source
        .launch
        .session_id
        .as_deref()
        .ok_or("The conversation has no confirmed identity")?;
    if uuid::Uuid::parse_str(id).is_err() || id.len() != 36 {
        return Err("Automatic account switching requires an exact provider UUID".into());
    }
    let original = provider
        .session_root(&source.launch.recipe)
        .ok_or("Cannot locate the source account history")?;
    let target = provider
        .session_root(&replacement.launch.recipe)
        .ok_or("Cannot locate the alternate account history")?;
    if !original.is_absolute() || !target.is_absolute() {
        return Err("Account switching needs absolute provider home paths".into());
    }
    let original = original.canonicalize().map_err(|error| error.to_string())?;
    let path = match provider {
        Provider::Claude => {
            // Claude can keep this session under a previous cwd or an explicit
            // project storage name. Its UUID resume searches across projects;
            // preserve exactly one history branch instead of guessing a slug.
            let mut found = Vec::new();
            let mut budget = ENTRY_BUDGET;
            find(
                &original.join("projects"),
                6,
                ".jsonl",
                &mut budget,
                &mut found,
            )?;
            let name = format!("{id}.jsonl");
            found.retain(|path| path.file_name().is_some_and(|file| file == name.as_str()));
            if found.len() != 1 {
                return Err("The exact Claude transcript is missing or ambiguous; continue in a new conversation to retain the original history".into());
            }
            found.pop().unwrap()
        }
        Provider::Codex => {
            let mut found = Vec::new();
            let mut budget = ENTRY_BUDGET;
            find(
                &original.join("sessions"),
                4,
                ".jsonl",
                &mut budget,
                &mut found,
            )?;
            let expected =
                uuid::Uuid::parse_str(id).map_err(|_| "The conversation identity is invalid")?;
            found.retain(|path| transcript_identity(provider, path) == Some(expected));
            // The provider prefers a plain sibling over a compressed copy.
            // Distinct rollout versions remain ambiguous and use the explicit
            // continuation fallback rather than guessing a history branch.
            found.retain(|path| {
                !path.to_string_lossy().ends_with(".zst") || !path.with_extension("").exists()
            });
            if found.len() != 1 {
                return Err("The exact Codex transcript is missing or ambiguous; the conversation stays on its original account".into());
            }
            found.pop().unwrap()
        }
        Provider::Omp => return Err("Omp manages its own account rotation".into()),
    };
    let relative = path
        .strip_prefix(&original)
        .map_err(|_| "The transcript escapes the original account home")?;
    let destination = target.join(relative);
    check_representation(&path, &destination).map_err(|error| error.to_string())?;
    check_target_histories(provider, &[(path.clone(), relative.to_owned())], &target)?;
    copy_transcript(&path, &destination, commit).map_err(|error| {
        format!("The conversation history could not be prepared on the alternate account: {error}")
    })
}

fn find(
    root: &Path,
    depth: usize,
    suffix: &str,
    budget: &mut usize,
    found: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if depth == 0 {
        return Err("The provider history exceeds the bounded account-switch depth".into());
    }
    crate::secure_file::reject_symlink_components(root).map_err(|error| error.to_string())?;
    for entry in fs::read_dir(root).map_err(|error| error.to_string())? {
        if *budget == 0 {
            return Err("The provider history exceeds the bounded account-switch scan".into());
        }
        *budget -= 1;
        let entry = entry.map_err(|error| error.to_string())?;
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_symlink() {
            return Err(
                "Provider history contains a symbolic link; resolve it before moving conversations"
                    .into(),
            );
        }
        if kind.is_dir() {
            find(&entry.path(), depth - 1, suffix, budget, found)?;
        } else if kind.is_file() && {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.ends_with(suffix) || (suffix == ".jsonl" && name.ends_with(".jsonl.zst"))
        } {
            found.push(entry.path());
        }
    }
    Ok(())
}

fn read_transcript(path: &Path) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    crate::secure_file::reject_symlink_components(path)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_TRANSCRIPT {
        return Err(io::Error::other("Transcript is not a bounded regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_TRANSCRIPT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TRANSCRIPT {
        return Err(io::Error::other(
            "Transcript grew beyond the account-switch limit",
        ));
    }
    Ok(bytes)
}

/// Never let a different target representation silently take precedence. When
/// both physical source representations exist, the regular bounded divergence
/// check proves that copying both preserves the same source choice.
fn check_representation(source: &Path, destination: &Path) -> io::Result<()> {
    let counterpart = |path: &Path| {
        if path.to_string_lossy().ends_with(".jsonl.zst") {
            path.with_extension("")
        } else {
            path.with_extension("jsonl.zst")
        }
    };
    let other = counterpart(destination);
    crate::secure_file::reject_symlink_components(&other)?;
    if other.exists() {
        let source_other = counterpart(source);
        if !source_other.exists() {
            return Err(io::Error::other(
                "The destination has a different transcript representation; choose a clean account or resolve its history first",
            ));
        }
        copy_transcript(&source_other, &other, false)?;
    }
    Ok(())
}

fn copy_transcript(source: &Path, destination: &Path, commit: bool) -> io::Result<()> {
    let bytes = read_transcript(source)?;
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("Missing transcript directory"))?;
    crate::secure_file::reject_symlink_components(destination)?;
    if destination.exists() {
        let previous = read_transcript(destination)?;
        if previous == bytes {
            return Ok(());
        }
        if !bytes.starts_with(&previous) {
            return Err(io::Error::other(
                "Alternate account has divergent conversation history; refusing to overwrite it",
            ));
        }
    }
    if !commit {
        return Ok(());
    }
    crate::secure_file::prepare_private_directory(parent)?;
    use std::os::unix::fs::OpenOptionsExt;
    let temporary = parent.join(format!(".cagents-handoff-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, destination)?;
        fs::File::open(parent)?.sync_all()
    })();
    let _ = fs::remove_file(&temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copies_only_history_and_refuses_divergence_and_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.jsonl");
        let target = directory.path().join("account/sessions/target.jsonl");
        fs::write(&source, b"one\ntwo\n").unwrap();
        copy_transcript(&source, &target, true).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"one\ntwo\n");
        fs::write(&source, b"one\ntwo\nthree\n").unwrap();
        copy_transcript(&source, &target, true).unwrap();
        fs::write(&source, b"other\n").unwrap();
        assert!(copy_transcript(&source, &target, true).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"one\ntwo\nthree\n");
        fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(&source, &target).unwrap();
        assert!(copy_transcript(&source, &target, true).is_err());
    }
}

/// Enumerate provider transcripts without touching login or configuration files.
/// Symlinks and overlarge trees fail closed before an archive/move can proceed.
fn history(account: &super::Account) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let home = account
        .resolved_home()
        .ok_or("This account has no history directory")?;
    let roots = match account.provider {
        Provider::Claude => vec![home.join("projects")],
        Provider::Codex => vec![home.join("sessions"), home.join("archived_sessions")],
        Provider::Omp => return Err("Omp manages its own history".into()),
    };
    let mut files = Vec::new();
    let mut budget = ENTRY_BUDGET;
    for root in roots {
        match fs::symlink_metadata(&root) {
            Ok(_) => find(&root, 6, ".jsonl", &mut budget, &mut files)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    files
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(&home)
                .map_err(|_| "History escapes its account home")?
                .to_owned();
            crate::secure_file::reject_symlink_components(&path)
                .map_err(|error| error.to_string())?;
            let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if !metadata.is_file() || metadata.len() > MAX_TRANSCRIPT {
                return Err("History contains an unsupported or oversized transcript".into());
            }
            Ok((path, relative))
        })
        .collect()
}

fn transcript_identity(provider: Provider, path: &Path) -> Option<uuid::Uuid> {
    let name = path.file_name()?.to_str()?;
    let name = name
        .strip_suffix(".zst")
        .unwrap_or(name)
        .strip_suffix(".jsonl")?;
    let id = match provider {
        Provider::Claude => name,
        Provider::Codex => {
            // Current Codex keeps a stable thread UUID before an optional
            // `_rollout-UUID` suffix after revert; old one-ID files still work.
            let (thread, rollout) = name.split_once('_').unwrap_or((name, ""));
            if !rollout.is_empty() && uuid::Uuid::parse_str(rollout).is_err() {
                return None;
            }
            thread.get(thread.len().checked_sub(36)?..)?
        }
        Provider::Omp => return None,
    };
    (id.len() == 36)
        .then(|| uuid::Uuid::parse_str(id).ok())
        .flatten()
}

fn count_conversations(provider: Provider, files: &[(PathBuf, PathBuf)]) -> usize {
    files
        .iter()
        .filter_map(|(path, _)| transcript_identity(provider, path))
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

/// Provider resume selects a logical conversation, not our copied filename.
/// In particular, Codex may have another rollout version for the same thread
/// elsewhere in its home. Never confirm an exact switch onto that other branch.
fn check_target_histories(
    provider: Provider,
    sources: &[(PathBuf, PathBuf)],
    target: &Path,
) -> Result<(), String> {
    let identities = sources
        .iter()
        .filter_map(|(path, _)| transcript_identity(provider, path))
        .collect::<std::collections::BTreeSet<_>>();
    let mut allowed = std::collections::BTreeSet::new();
    for (source, relative) in sources {
        allowed.insert(relative.clone());
        let (other_source, other_relative) = if source.to_string_lossy().ends_with(".jsonl.zst") {
            (source.with_extension(""), relative.with_extension(""))
        } else {
            (
                source.with_extension("jsonl.zst"),
                relative.with_extension("jsonl.zst"),
            )
        };
        if other_source.exists() {
            allowed.insert(other_relative);
        }
    }
    let account = super::Account::linked(provider, uuid::Uuid::nil(), target.to_owned());
    for (path, relative) in history(&account)? {
        if transcript_identity(provider, &path).is_some_and(|id| identities.contains(&id))
            && !allowed.contains(&relative)
        {
            return Err("The destination has a different saved version of this conversation; choose a clean account or resolve its history first".into());
        }
    }
    Ok(())
}

pub fn conversation_count(account: &super::Account) -> Result<usize, String> {
    history(account).map(|files| count_conversations(account.provider, &files))
}

/// Prepare every transcript before signing out or hiding the source account.
/// The source is retained, so a partial copy or failed logout loses no history.
pub fn move_history(source: &super::Account, target: &super::Account) -> Result<usize, String> {
    if source.provider != target.provider || source.id() == target.id() {
        return Err("Choose another account of the same provider".into());
    }
    let target = target
        .resolved_home()
        .ok_or("The destination has no provider home")?;
    let source_home = source
        .resolved_home()
        .ok_or("The source has no provider home")?;
    if source_home
        .canonicalize()
        .ok()
        .zip(target.canonicalize().ok())
        .is_some_and(|(source, target)| source == target)
    {
        return Err("The destination must use a different provider home".into());
    }
    if !target.is_absolute() {
        return Err("The destination provider home must be absolute".into());
    }
    let files = history(source)?;
    let conversations = count_conversations(source.provider, &files);
    // Include subordinate provider JSONL too; a main conversation can refer
    // to these saved subagent transcripts when it is resumed.
    // Check all destinations before the first write. A divergent existing
    // transcript never gets overwritten, even if another one already copied.
    for (source, relative) in &files {
        check_representation(source, &target.join(relative)).map_err(|error| error.to_string())?;
        copy_transcript(source, &target.join(relative), false)
            .map_err(|error| error.to_string())?;
    }
    check_target_histories(source.provider, &files, &target)?;
    for (source, relative) in &files {
        copy_transcript(source, &target.join(relative), true).map_err(|error| error.to_string())?;
    }
    Ok(conversations)
}

#[cfg(test)]
mod removal_tests {
    use super::*;
    #[test]
    fn claude_handoff_finds_named_or_moved_project_storage_and_refuses_duplicate_branches() {
        let root = tempfile::tempdir().unwrap();
        let source_home = root.path().join("source");
        let target_home = root.path().join("target");
        let id = uuid::Uuid::new_v4().to_string();
        let relative = PathBuf::from(format!("projects/named-work/{id}.jsonl"));
        let history = source_home.join(&relative);
        fs::create_dir_all(history.parent().unwrap()).unwrap();
        fs::write(&history, b"saved user conversation\n").unwrap();
        let source = Session {
            id: crate::model::TerminalId::new(),
            proof: None,
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: Provider::Claude,
                session_id: Some(id.clone()),
                recipe: crate::model::LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: root.path().join("original-project-spelling"),
                    environment: std::collections::BTreeMap::from([
                        ("HOME".into(), root.path().to_string_lossy().into_owned()),
                        (
                            "CLAUDE_CONFIG_DIR".into(),
                            source_home.to_string_lossy().into_owned(),
                        ),
                    ]),
                    unset_environment: Default::default(),
                },
            },
        };
        let mut target = source.clone();
        target.launch.recipe.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            target_home.to_string_lossy().into_owned(),
        );
        prepare(&source, &target).unwrap();
        assert_eq!(
            fs::read(target_home.join(&relative)).unwrap(),
            b"saved user conversation\n"
        );
        let other = source_home.join(format!("projects/another-branch/{id}.jsonl"));
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::write(&other, b"another history branch\n").unwrap();
        fs::remove_file(target_home.join(&relative)).unwrap();
        assert!(prepare(&source, &target).unwrap_err().contains("ambiguous"));
        assert!(!target_home.join(&relative).exists());
        assert_eq!(fs::read(history).unwrap(), b"saved user conversation\n");
        assert_eq!(fs::read(other).unwrap(), b"another history branch\n");
    }

    #[test]
    fn a_different_target_rollout_for_the_same_thread_refuses_before_any_copy() {
        let root = tempfile::tempdir().unwrap();
        let source_home = root.path().join("source");
        let target_home = root.path().join("target");
        let source = super::super::Account::managed(
            Provider::Codex,
            uuid::Uuid::new_v4(),
            source_home.clone(),
        );
        let target = super::super::Account::managed(
            Provider::Codex,
            uuid::Uuid::new_v4(),
            target_home.clone(),
        );
        let thread = uuid::Uuid::new_v4();
        let source_relative = PathBuf::from(format!(
            "sessions/2026/10/14/rollout-2026-10-14T20-00-00-{thread}.jsonl"
        ));
        let other_relative = PathBuf::from(format!(
            "archived_sessions/rollout-2026-10-14T21-00-00-{thread}_{}.jsonl.zst",
            uuid::Uuid::new_v4()
        ));
        let source_file = source_home.join(&source_relative);
        let other_file = target_home.join(&other_relative);
        fs::create_dir_all(source_file.parent().unwrap()).unwrap();
        fs::create_dir_all(other_file.parent().unwrap()).unwrap();
        fs::write(&source_file, b"source branch").unwrap();
        fs::write(&other_file, b"other target branch").unwrap();
        let files = history(&source).unwrap();
        assert!(
            check_target_histories(Provider::Codex, &files, &target_home)
                .unwrap_err()
                .contains("different saved version")
        );
        assert!(
            move_history(&source, &target)
                .unwrap_err()
                .contains("different saved version")
        );
        assert!(!target_home.join(&source_relative).exists());
        assert_eq!(fs::read(&source_file).unwrap(), b"source branch");
        assert_eq!(fs::read(&other_file).unwrap(), b"other target branch");
    }

    #[test]
    fn codex_moves_compressed_rollouts_and_counts_stable_threads_once() {
        let root = tempfile::tempdir().unwrap();
        let provider = Provider::Codex;
        let source_home = root.path().join("source");
        let target_home = root.path().join("target");
        let source =
            super::super::Account::managed(provider, uuid::Uuid::new_v4(), source_home.clone());
        let target =
            super::super::Account::managed(provider, uuid::Uuid::new_v4(), target_home.clone());
        let thread = uuid::Uuid::new_v4();
        let other_thread = uuid::Uuid::new_v4();
        let files = [
            format!("sessions/2026/10/14/rollout-2026-10-14T20-00-00-{thread}.jsonl"),
            format!(
                "sessions/2026/10/14/rollout-2026-10-14T20-01-00-{thread}_{}.jsonl.zst",
                uuid::Uuid::new_v4()
            ),
            format!("archived_sessions/rollout-2026-10-14T20-02-00-{other_thread}.jsonl.zst"),
        ];
        for file in &files {
            let path = source_home.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"opaque provider history; copied without decoding").unwrap();
        }
        assert_eq!(conversation_count(&source).unwrap(), 2);
        assert_eq!(move_history(&source, &target).unwrap(), 2);
        for file in &files {
            assert_eq!(
                fs::read(source_home.join(file)).unwrap(),
                fs::read(target_home.join(file)).unwrap()
            );
        }
        assert_eq!(conversation_count(&target).unwrap(), 2);
        assert_eq!(move_history(&source, &target).unwrap(), 2);
    }

    #[test]
    fn a_different_target_representation_is_refused_before_copying_any_history() {
        let root = tempfile::tempdir().unwrap();
        let provider = Provider::Codex;
        let source_home = root.path().join("source");
        let target_home = root.path().join("target");
        let source =
            super::super::Account::managed(provider, uuid::Uuid::new_v4(), source_home.clone());
        let target =
            super::super::Account::managed(provider, uuid::Uuid::new_v4(), target_home.clone());
        let relative = format!(
            "sessions/2026/10/14/rollout-2026-10-14T20-00-00-{}.jsonl.zst",
            uuid::Uuid::new_v4()
        );
        let source_file = source_home.join(&relative);
        let conflicting = target_home.join(&relative).with_extension("");
        fs::create_dir_all(source_file.parent().unwrap()).unwrap();
        fs::create_dir_all(conflicting.parent().unwrap()).unwrap();
        fs::write(&source_file, b"compressed source history").unwrap();
        fs::write(&conflicting, b"different plain target history").unwrap();
        let failure = move_history(&source, &target).unwrap_err();
        assert!(failure.contains("different transcript representation"));
        assert_eq!(
            fs::read(&source_file).unwrap(),
            b"compressed source history"
        );
        assert_eq!(
            fs::read(&conflicting).unwrap(),
            b"different plain target history"
        );
        assert!(!target_home.join(relative).exists());
    }

    #[test]
    fn exact_codex_handoff_accepts_one_compressed_rollout_without_decoding_it() {
        let root = tempfile::tempdir().unwrap();
        let source_home = root.path().join("source");
        let target_home = root.path().join("target");
        let id = uuid::Uuid::new_v4();
        let relative = format!("sessions/2026/10/14/rollout-2026-10-14T20-00-00-{id}.jsonl.zst");
        let transcript = source_home.join(&relative);
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, b"opaque compressed provider history").unwrap();
        let source = Session {
            id: crate::model::TerminalId::new(),
            proof: None,
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: Provider::Codex,
                session_id: Some(id.to_string()),
                recipe: crate::model::LaunchRecipe {
                    program: "codex".into(),
                    arguments: Vec::new(),
                    working_directory: root.path().into(),
                    environment: std::collections::BTreeMap::from([
                        ("HOME".into(), root.path().to_string_lossy().into_owned()),
                        (
                            "CODEX_HOME".into(),
                            source_home.to_string_lossy().into_owned(),
                        ),
                    ]),
                    unset_environment: Default::default(),
                },
            },
        };
        let mut target = source.clone();
        target.launch.recipe.environment.insert(
            "CODEX_HOME".into(),
            target_home.to_string_lossy().into_owned(),
        );
        prepare(&source, &target).unwrap();
        assert_eq!(
            fs::read(target_home.join(relative)).unwrap(),
            fs::read(transcript).unwrap()
        );
    }

    #[test]
    fn move_copies_every_provider_transcript_and_preserves_credentials_and_source() {
        for provider in [Provider::Claude, Provider::Codex] {
            let root = tempfile::tempdir().unwrap();
            let source_home = root.path().join("source");
            let target_home = root.path().join("target");
            let source = crate::account::Account::managed(
                provider,
                uuid::Uuid::new_v4(),
                source_home.clone(),
            );
            let target = crate::account::Account::managed(
                provider,
                uuid::Uuid::new_v4(),
                target_home.clone(),
            );
            for index in 0..3 {
                let id = uuid::Uuid::new_v4();
                let relative = if provider == Provider::Claude {
                    format!("projects/fixture/{id}.jsonl")
                } else {
                    format!("sessions/2026/10/14/rollout-{id}.jsonl")
                };
                let transcript = source_home.join(&relative);
                fs::create_dir_all(transcript.parent().unwrap()).unwrap();
                fs::write(&transcript, format!("fixture-{index}\n")).unwrap();
            }
            let auxiliary = if provider == Provider::Claude {
                "projects/fixture/session/subagents/agent-fixture.jsonl"
            } else {
                "sessions/2026/10/14/subagents/agent-fixture.jsonl"
            };
            let auxiliary_path = source_home.join(auxiliary);
            fs::create_dir_all(auxiliary_path.parent().unwrap()).unwrap();
            fs::write(&auxiliary_path, "subagent conversation").unwrap();
            fs::write(
                source_home.join("private-login-fixture"),
                "not a transcript",
            )
            .unwrap();
            assert_eq!(conversation_count(&source).unwrap(), 3);
            assert_eq!(move_history(&source, &target).unwrap(), 3);
            assert_eq!(conversation_count(&target).unwrap(), 3);
            assert_eq!(
                fs::read_to_string(target_home.join(auxiliary)).unwrap(),
                "subagent conversation"
            );
            assert_eq!(conversation_count(&source).unwrap(), 3);
            assert!(!target_home.join("private-login-fixture").exists());
            assert_eq!(
                move_history(&source, &target).unwrap(),
                3,
                "repeat moves remain safe"
            );
        }
    }
}
