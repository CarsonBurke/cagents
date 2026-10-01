//! Bounded, read-only discovery. No provider process or model request is needed.
use super::*;
use serde_json::Value;
use std::collections::HashSet;

const FILE_LIMIT: usize = 8192;
const READ_BYTES: u64 = 256 * 1024;

pub(super) fn discover(seeds: &[chat::Session]) -> Vec<SavedConversation> {
    let mut roots = HashSet::new();
    let mut paths = HashSet::new();
    let mut entries = Vec::new();
    for seed in seeds {
        let provider = seed.launch.provider;
        let Some(root) = provider.session_root(&seed.launch.recipe) else {
            continue;
        };
        let (root, depth) = match provider {
            Provider::Claude => (root.join("projects"), 2),
            Provider::Codex => (root.join("sessions"), 4),
            Provider::Omp => (root.join("sessions"), 3),
        };
        let root = root.canonicalize().unwrap_or(root);
        if !roots.insert((provider, root.clone())) {
            continue;
        }
        let mut files = Vec::new();
        let mut budget = FILE_LIMIT;
        collect(&root, depth, &mut budget, &mut files);
        for file in files {
            if !paths.insert((provider, file.clone())) {
                continue;
            }
            if let Some(entry) = metadata(seed, &file) {
                entries.push(entry);
            }
        }
    }
    entries
}

fn collect(root: &Path, depth: usize, budget: &mut usize, files: &mut Vec<PathBuf>) {
    if depth == 0 || *budget == 0 {
        return;
    }
    let Ok(directory) = std::fs::read_dir(root) else {
        return;
    };
    // Bound directory entries too, including unrelated files and symlinks.
    for entry in directory.flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() && path.file_name().is_none_or(|name| name != "subagents") {
            collect(&path, depth - 1, budget, files);
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(path);
        }
    }
}

fn metadata(seed: &chat::Session, path: &Path) -> Option<SavedConversation> {
    let mut file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let mut head = Vec::new();
    (&mut file).take(READ_BYTES).read_to_end(&mut head).ok()?;
    let mut values = head
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .collect::<Vec<_>>();
    if metadata.len() > READ_BYTES {
        file.seek(std::io::SeekFrom::End(-64 * 1024)).ok()?;
        let mut tail = Vec::new();
        (&mut file).take(64 * 1024).read_to_end(&mut tail).ok()?;
        values.extend(
            tail.split(|byte| *byte == b'\n')
                .filter_map(|line| serde_json::from_slice::<Value>(line).ok()),
        );
    }
    let provider = seed.launch.provider;
    let header = values
        .iter()
        .find_map(|value| match (provider, value["type"].as_str()) {
            (Provider::Codex, Some("session_meta")) if !value["payload"]["source"].is_object() => {
                Some(&value["payload"])
            }
            (Provider::Omp, Some("session")) => Some(value),
            (Provider::Claude, _)
                if value["cwd"].is_string()
                    && value["sessionId"].is_string()
                    && value["isSidechain"] != true =>
            {
                Some(value)
            }
            _ => None,
        })?;
    let id = header[if provider == Provider::Claude {
        "sessionId"
    } else {
        "id"
    }]
    .as_str()?;
    if id.len() != 36 || uuid::Uuid::parse_str(id).is_err() {
        return None;
    }
    let directory = PathBuf::from(header["cwd"].as_str()?);
    if !directory.is_absolute() {
        return None;
    }
    let title = values
        .iter()
        .rev()
        .find_map(|value| {
            match value["type"].as_str() {
                Some("custom-title") => value["customTitle"].as_str(),
                Some("title") => value["title"].as_str(),
                Some("session_info") => value["name"].as_str(),
                _ => None,
            }
            .and_then(crate::account::clean_label)
        })
        .or_else(|| {
            values.iter().find_map(|value| {
                let message = match provider {
                    Provider::Claude
                        if value["type"] == "user"
                            && value["isMeta"] != true
                            && value["isSidechain"] != true =>
                    {
                        &value["message"]
                    }
                    Provider::Omp
                        if value["type"] == "message" && value["message"]["role"] == "user" =>
                    {
                        &value["message"]
                    }
                    Provider::Codex
                        if value["type"] == "event_msg"
                            && value["payload"]["type"] == "user_message" =>
                    {
                        return value["payload"]["message"]
                            .as_str()
                            .and_then(crate::account::clean_label);
                    }
                    _ => return None,
                };
                let content = &message["content"];
                content
                    .as_str()
                    .or_else(|| {
                        content
                            .as_array()?
                            .iter()
                            .find_map(|part| part["text"].as_str())
                    })
                    .and_then(crate::account::clean_label)
            })
        })
        .unwrap_or_else(|| format!("{} conversation", provider.label()));
    let mut session = seed.clone();
    session.reset_owned_codex_listener();
    session.id = TerminalId::new();
    session.proof = None;
    session.terminal = None;
    session.launch.recipe.working_directory = directory;
    session.launch.session_id = Some(id.into());
    if provider == Provider::Omp {
        let args = &mut session.launch.recipe.arguments;
        let mut index = 0;
        while index < args.len() {
            if args[index] == "--session-dir" {
                args.drain(index..(index + 2).min(args.len()));
            } else if args[index].starts_with("--session-dir=") {
                args.remove(index);
            } else {
                index += 1;
            }
        }
        args.extend([
            "--session-dir".into(),
            path.parent()?.to_string_lossy().into_owned(),
        ]);
    }
    // Rebuild resume arguments from the exact discovered identity rather than
    // carry another saved conversation's resume selector into its launch.
    session.launch.recipe = session.launch.terminal_recipe()?;
    Some(SavedConversation {
        card: CardId::new(),
        title,
        session,
        open: false,
        updated_at_unix_ms: metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis()
            .try_into()
            .ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn discovers_all_provider_history_without_changing_account_or_identity() {
        let root = tempfile::tempdir().unwrap();
        let id = "f0e49307-af96-4edd-b23f-60b185fe4fea";
        for provider in Provider::ALL {
            let account_home = root.path().join(provider.executable());
            std::fs::create_dir_all(&account_home).unwrap();
            let account = crate::account::Account {
                provider,
                home: crate::account::AccountHome::Linked {
                    key: uuid::Uuid::new_v4(),
                    path: account_home.clone(),
                },
            };
            let mut seed =
                chat::fresh_session(provider, TerminalId::new(), root.path().into(), &account)
                    .unwrap();
            if provider == Provider::Omp {
                seed.launch.recipe.environment.insert(
                    "PI_CODING_AGENT_DIR".into(),
                    account_home.to_string_lossy().into_owned(),
                );
            }
            let base = provider.session_root(&seed.launch.recipe).unwrap();
            let directory = base.join(if provider == Provider::Claude {
                "projects/project"
            } else {
                "sessions/native"
            });
            std::fs::create_dir_all(&directory).unwrap();
            let record = match provider {
                Provider::Claude => {
                    json!({"type":"user","sessionId":id,"cwd":root.path(),"message":{"content":"Restore me"}})
                }
                Provider::Codex => {
                    json!({"type":"session_meta","payload":{"id":id,"cwd":root.path(),"source":"cli"}})
                }
                Provider::Omp => json!({"type":"session","id":id,"cwd":root.path()}),
            };
            std::fs::write(directory.join(format!("{id}.jsonl")), format!("{record}\n")).unwrap();
            let entries = discover(&[seed.clone()]);
            assert_eq!(entries.len(), 1, "{provider:?}");
            assert_eq!(entries[0].session.launch.session_id.as_deref(), Some(id));
            assert_eq!(
                entries[0].session.launch.recipe.working_directory,
                root.path()
            );
            assert_eq!(
                provider.session_root(&entries[0].session.launch.recipe),
                Some(base)
            );
            assert!(
                !entries[0]
                    .session
                    .launch
                    .recipe
                    .arguments
                    .contains(&"bypassPermissions".into())
            );
        }
    }

    #[test]
    fn directory_budget_bounds_empty_trees_and_excludes_child_transcripts() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("subagents")).unwrap();
        std::fs::write(root.path().join("subagents/child.jsonl"), "{}").unwrap();
        for index in 0..10 {
            std::fs::create_dir(root.path().join(format!("empty-{index}"))).unwrap();
        }
        let mut files = Vec::new();
        let mut budget = 3;
        collect(root.path(), 3, &mut budget, &mut files);
        assert_eq!(budget, 0);
        assert!(files.is_empty());
    }
}
