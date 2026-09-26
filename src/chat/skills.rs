//! A filesystem-only draft catalogue. The running provider remains authoritative
//! for plugin, remote, and dynamically supplied skills.
use super::{Launch, Skill};
use crate::agent::Provider;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use yaml_rust::YamlLoader;
use yaml_rust::scanner::{Scanner, TokenType};

const MAX_ENTRIES: usize = 8192;
const MAX_DEPTH: usize = 12;
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024;

pub fn discover(launch: &Launch) -> Vec<Skill> {
    let Some(root) = launch.provider.authored_content_root(&launch.recipe) else {
        return Vec::new();
    };
    let Some(home) = launch.recipe.environment.get("HOME") else {
        return Vec::new();
    };
    discover_at(launch, Path::new(home), &root)
}

fn discover_at(launch: &Launch, home: &Path, root: &Path) -> Vec<Skill> {
    if launch.provider == Provider::Claude
        && launch
            .recipe
            .arguments
            .iter()
            .any(|arg| arg == "--disable-slash-commands" || arg == "--bare")
    {
        return Vec::new();
    }
    if launch.provider == Provider::Omp
        && launch
            .recipe
            .arguments
            .iter()
            .any(|arg| arg == "--no-skills")
    {
        return Vec::new();
    }
    let cwd = &launch.recipe.working_directory;
    let root = cwd.join(root);
    let mut roots = Vec::new();
    let mut commands = Vec::new();
    if launch.provider == Provider::Claude {
        // Claude gives personal skills precedence over project skills.
        roots.push((root.join("skills"), false));
        commands.push(root.join("commands"));
    }
    for directory in cwd.ancestors() {
        match launch.provider {
            Provider::Codex => roots.push((directory.join(".agents/skills"), true)),
            Provider::Claude => {
                // The selected account replaces the personal .claude directory.
                if directory != home {
                    roots.push((directory.join(".claude/skills"), false));
                    commands.push(directory.join(".claude/commands"));
                }
            }
            Provider::Omp => {
                roots.push((directory.join(".omp/skills"), true));
                roots.push((directory.join(".agents/skills"), false));
                roots.push((directory.join(".agent/skills"), false));
            }
        }
        // A worktree has a .git file; an ordinary repository has a directory.
        if directory.join(".git").exists() || directory == home {
            break;
        }
    }
    if launch.provider != Provider::Claude {
        roots.push((root.join("skills"), true));
        roots.push((
            home.join(".agents/skills"),
            launch.provider == Provider::Codex,
        ));
    }
    if launch.provider == Provider::Omp {
        roots.push((home.join(".agent/skills"), false));
    }
    let disabled = if launch.provider == Provider::Codex {
        disabled_codex_skills(&root)
    } else {
        HashSet::new()
    };
    let mut catalog = Catalog {
        provider: launch.provider,
        disabled,
        visited: HashSet::new(),
        remaining: MAX_ENTRIES,
        skills: BTreeMap::new(),
    };
    for (root, require_description) in roots {
        // A symlinked root may participate in more than one discovery source
        // with different metadata requirements. Only suppress cycles within
        // a source; the catalogue itself deduplicates discovered identities.
        catalog.visited.clear();
        catalog.scan(&root, 0, require_description);
    }
    for root in commands {
        catalog.visited.clear();
        catalog.scan_commands(&root, &root, 0);
    }
    catalog.skills.into_values().collect()
}

fn disabled_codex_skills(root: &Path) -> HashSet<PathBuf> {
    let Some(config) = read_document(&root.join("config.toml"))
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
    else {
        return HashSet::new();
    };
    config
        .get("skills")
        .and_then(|v| v.get("config"))
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| entry.get("enabled").and_then(toml::Value::as_bool) == Some(false))
        .filter_map(|entry| entry.get("path").and_then(toml::Value::as_str))
        .filter_map(|path| fs::canonicalize(root.join(path)).ok())
        .collect()
}

struct Catalog {
    provider: Provider,
    disabled: HashSet<PathBuf>,
    visited: HashSet<PathBuf>,
    remaining: usize,
    skills: BTreeMap<String, Skill>,
}

impl Catalog {
    fn scan_commands(&mut self, root: &Path, directory: &Path, depth: usize) {
        if depth > MAX_DEPTH || self.remaining == 0 {
            return;
        }
        self.remaining -= 1;
        let Ok(canonical) = fs::canonicalize(directory) else {
            return;
        };
        if !self.visited.insert(canonical) {
            return;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        let mut entries: Vec<_> = entries
            .take(self.remaining)
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        for path in entries {
            if self.remaining == 0 {
                break;
            }
            if path.is_dir() {
                self.scan_commands(root, &path, depth + 1);
            } else {
                self.remaining -= 1;
                if path.extension().is_some_and(|extension| extension == "md")
                    && let Some(skill) = parse_command(root, &path)
                {
                    self.skills.entry(skill.name.clone()).or_insert(skill);
                }
            }
        }
    }

    fn scan(&mut self, directory: &Path, depth: usize, require_description: bool) {
        if depth > MAX_DEPTH || self.remaining == 0 || (self.provider == Provider::Omp && depth > 1)
        {
            return;
        }
        self.remaining -= 1;
        let Ok(canonical) = fs::canonicalize(directory) else {
            return;
        };
        if !self.visited.insert(canonical.clone()) || self.disabled.contains(&canonical) {
            return;
        }
        let skill_file = directory.join("SKILL.md");
        if skill_file.is_file() && (self.provider != Provider::Omp || depth == 1) {
            if fs::canonicalize(&skill_file).is_ok_and(|path| !self.disabled.contains(&path))
                && let Some(skill) = parse_skill(&skill_file, self.provider, require_description)
            {
                let key = if self.provider == Provider::Codex {
                    format!("{}:{}", skill.name, skill.invocation)
                } else {
                    skill.name.clone()
                };
                self.skills.entry(key).or_insert(skill);
            }
            // Resources inside a skill are not separate skills.
            return;
        }
        if self.provider == Provider::Omp && depth == 1 {
            return;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        let mut children: Vec<_> = entries
            .take(self.remaining)
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        children.sort();
        for path in children {
            if self.remaining == 0 {
                break;
            }
            if path.file_name().is_some_and(|name| {
                name != ".git"
                    && (self.provider != Provider::Omp || !name.to_string_lossy().starts_with('.'))
            }) && path.is_dir()
            {
                self.scan(&path, depth + 1, require_description);
            } else {
                self.remaining = self.remaining.saturating_sub(1);
            }
        }
    }
}

fn read_document(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_DOCUMENT_BYTES {
        return None;
    }
    let mut contents = String::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_string(&mut contents)
        .ok()?;
    (contents.len() as u64 <= MAX_DOCUMENT_BYTES).then_some(contents)
}

fn parse_command(root: &Path, path: &Path) -> Option<Skill> {
    let text = read_document(path)?;
    let name = path
        .strip_prefix(root)
        .ok()?
        .with_extension("")
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?
        .join(":");
    let (document, body) = frontmatter(text.trim_start_matches('\u{feff}'))?;
    if document["user-invocable"].as_bool() == Some(false) {
        return None;
    }
    let description = document["description"].as_str().unwrap_or_else(|| {
        body.lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
    });
    let description = description.trim().chars().take(160).collect();
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        return None;
    }
    Some(Skill {
        invocation: format!("/{name}"),
        name,
        description,
    })
}

/// Missing metadata is valid for Claude and shared OMP skills. An opened but
/// malformed or unterminated header is not silently treated as instructions.
fn frontmatter(text: &str) -> Option<(yaml_rust::Yaml, &str)> {
    let Some(first) = text.lines().next() else {
        return Some((yaml_rust::Yaml::Null, text));
    };
    if first.trim() != "---" {
        return Some((yaml_rust::Yaml::Null, text));
    }
    let header_start = text.find('\n')? + 1;
    let mut offset = header_start;
    for line in text[header_start..].split_inclusive('\n') {
        if line.trim() == "---" {
            let header = &text[header_start..offset];
            // The file-size bound alone does not bound YAML alias expansion.
            // Skill metadata needs neither aliases nor deeply nested trees.
            let mut depth = 0usize;
            for token in Scanner::new(header.chars()) {
                match token.1 {
                    TokenType::Alias(_) => return None,
                    TokenType::BlockSequenceStart
                    | TokenType::BlockMappingStart
                    | TokenType::FlowSequenceStart
                    | TokenType::FlowMappingStart => {
                        depth += 1;
                        if depth > 32 {
                            return None;
                        }
                    }
                    TokenType::BlockEnd
                    | TokenType::FlowSequenceEnd
                    | TokenType::FlowMappingEnd => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
            let documents = YamlLoader::load_from_str(header).ok()?;
            if documents.len() > 1 {
                return None;
            }
            return Some((
                documents
                    .into_iter()
                    .next()
                    .unwrap_or(yaml_rust::Yaml::Null),
                &text[offset + line.len()..],
            ));
        }
        offset += line.len();
    }
    None
}

fn parse_skill(path: &Path, provider: Provider, require_description: bool) -> Option<Skill> {
    let text = read_document(path)?;
    let text = text.trim_start_matches('\u{feff}');
    let (document, body) = frontmatter(text)?;
    if provider == Provider::Codex && document.is_null() {
        return None;
    }
    if (provider == Provider::Claude && document["user-invocable"].as_bool() == Some(false))
        || (provider == Provider::Omp && document["enabled"].as_bool() == Some(false))
    {
        return None;
    }
    let fallback = path.parent()?.file_name()?.to_str()?;
    let name = document["name"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .or_else(|| (provider != Provider::Codex).then_some(fallback))?
        .trim();
    if name.is_empty()
        || name.len() > 128
        || name
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '/' | '\\' | '$'))
    {
        return None;
    }
    let description = document["description"].as_str().unwrap_or_default().trim();
    if require_description && description.is_empty() {
        return None;
    }
    let description = if description.is_empty() && provider == Provider::Claude {
        body.lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .trim()
    } else {
        description
    };
    let description: String = description.chars().take(160).collect();
    let invocation = match provider {
        Provider::Codex => {
            let path = fs::canonicalize(path).ok()?;
            let path = path.to_str()?;
            // A name unique on disk can still collide with a plugin skill
            // discovered at startup. Carry the selected path all the way to
            // the provider rather than resolving the name a second time.
            if path.contains(')') {
                return None;
            }
            format!("[${name}]({path})")
        }
        Provider::Claude => format!("/{name}"),
        Provider::Omp => format!("/skill:{name}"),
    };
    Some(Skill {
        name: name.into(),
        description,
        invocation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LaunchRecipe;

    #[test]
    fn metadata_cannot_expand_aliases_or_unbounded_nesting() {
        assert!(frontmatter("---\nname: &name review\ndescription: *name\n---\n").is_none());
        let nested = format!("---\nextra: {}0{}\n---\n", "[".repeat(33), "]".repeat(33));
        assert!(frontmatter(&nested).is_none());
        assert!(frontmatter("---\nname: review\ndescription: 'literal *name'\n---\n").is_some());
    }

    fn write_skill(path: &Path, name: &str, description: &str) {
        fs::create_dir_all(path).unwrap();
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nInstructions"),
        )
        .unwrap();
    }

    fn launch(provider: Provider, cwd: &Path, home: &Path, root: &Path) -> Launch {
        let key = match provider {
            Provider::Codex => "CODEX_HOME",
            Provider::Claude => "CLAUDE_CONFIG_DIR",
            Provider::Omp => "PI_CODING_AGENT_DIR",
        };
        Launch {
            provider,
            recipe: LaunchRecipe {
                program: provider.executable().into(),
                arguments: vec![],
                working_directory: cwd.into(),
                environment: [
                    ("HOME".into(), home.display().to_string()),
                    (key.into(), root.display().to_string()),
                ]
                .into(),
            },
            session_id: None,
        }
    }

    #[test]
    fn account_and_project_scopes_preserve_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let cwd = home.join("project/subdirectory");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(home.join("project/.git")).unwrap();
        let account = home.join("account");
        write_skill(
            &account.join("skills/account"),
            "account",
            "Selected account",
        );
        write_skill(&home.join(".codex/skills/other"), "other", "Wrong account");
        write_skill(&home.join(".agents/skills/shared"), "shared", "Shared");
        write_skill(
            &home.join("project/.agents/skills/shared"),
            "shared",
            "Project",
        );
        let skills = discover(&launch(Provider::Codex, &cwd, home, &account));
        assert_eq!(
            skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["account", "shared", "shared"]
        );
        assert!(skills.iter().any(|skill| skill.description == "Project"));
        assert!(skills[0].invocation.starts_with("[$account]("));
        assert!(skills[1].invocation.starts_with("[$shared]("));
    }

    #[test]
    fn symlinked_catalogues_and_cycles_are_safe() {
        let temp = tempfile::tempdir().unwrap();
        let account = temp.path().join("account");
        let source = temp.path().join("source");
        write_skill(
            &source.join("review"),
            "review",
            ">-\n  Review changes\n  carefully",
        );
        fs::create_dir_all(&account).unwrap();
        std::os::unix::fs::symlink(&source, account.join("skills")).unwrap();
        std::os::unix::fs::symlink(&source, source.join("loop")).unwrap();
        let skills = discover(&launch(
            Provider::Claude,
            temp.path(),
            temp.path(),
            &account,
        ));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "Review changes carefully");
        assert_eq!(skills[0].invocation, "/review");
    }

    #[test]
    fn disabled_and_malformed_skills_are_excluded() {
        let temp = tempfile::tempdir().unwrap();
        let account = temp.path().join("account");
        write_skill(&account.join("skills/disabled"), "disabled", "Disabled");
        write_skill(&account.join("skills/malformed"), "malformed", "[invalid");
        write_skill(&account.join("skills/valid"), "valid", "Valid");
        fs::write(
            account.join("config.toml"),
            format!(
                "[[skills.config]]\npath = {:?}\nenabled = false\n",
                account.join("skills/disabled/SKILL.md")
            ),
        )
        .unwrap();
        let skills = discover(&launch(Provider::Codex, temp.path(), temp.path(), &account));
        assert_eq!(
            skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["valid"]
        );
    }

    #[test]
    fn claude_commands_use_selected_account_and_project_scope() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("project");
        let account = temp.path().join("account");
        fs::create_dir_all(cwd.join(".claude/commands/team")).unwrap();
        fs::create_dir_all(account.join("commands")).unwrap();
        fs::create_dir_all(temp.path().join(".claude/commands")).unwrap();
        fs::write(
            cwd.join(".claude/commands/team/review.md"),
            "---\ndescription: Project review\n---\nReview",
        )
        .unwrap();
        fs::write(account.join("commands/personal.md"), "Review my changes").unwrap();
        fs::write(
            temp.path().join(".claude/commands/wrong.md"),
            "Wrong account",
        )
        .unwrap();
        fs::write(
            account.join("commands/broken.md"),
            "---\ndescription: Missing end",
        )
        .unwrap();
        let mut launch = launch(Provider::Claude, &cwd, temp.path(), &account);
        let skills = discover(&launch);
        assert_eq!(
            skills
                .iter()
                .map(|s| s.invocation.as_str())
                .collect::<Vec<_>>(),
            ["/personal", "/team:review"]
        );
        launch
            .recipe
            .arguments
            .push("--disable-slash-commands".into());
        assert!(discover(&launch).is_empty());
    }

    #[test]
    fn claude_discovery_stops_at_repository_or_worktree_root() {
        for worktree in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path();
            let repository = home.join("parent/project");
            let cwd = repository.join("src");
            let account = home.join("account");
            fs::create_dir_all(&cwd).unwrap();
            if worktree {
                fs::write(
                    repository.join(".git"),
                    "gitdir: ../main/.git/worktrees/project",
                )
                .unwrap();
            } else {
                fs::create_dir(repository.join(".git")).unwrap();
            }
            write_skill(
                &repository.join(".claude/skills/inside"),
                "inside",
                "Project",
            );
            write_skill(
                &home.join("parent/.claude/skills/outside"),
                "outside",
                "Parent",
            );
            fs::create_dir_all(home.join("parent/.claude/commands")).unwrap();
            fs::write(
                home.join("parent/.claude/commands/outside-command.md"),
                "Outside command",
            )
            .unwrap();
            let skills = discover(&launch(Provider::Claude, &cwd, home, &account));
            assert_eq!(
                skills
                    .iter()
                    .map(|skill| skill.name.as_str())
                    .collect::<Vec<_>>(),
                ["inside"]
            );
        }
    }

    #[test]
    fn claude_personal_skills_win_and_skills_override_commands() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("project");
        let account = temp.path().join("account");
        write_skill(&account.join("skills/shared"), "shared", "Personal skill");
        write_skill(
            &cwd.join(".claude/skills/shared"),
            "shared",
            "Project skill",
        );
        write_skill(
            &cwd.join(".claude/skills/project"),
            "project",
            "Project skill beats command",
        );
        fs::create_dir_all(account.join("commands")).unwrap();
        fs::write(
            account.join("commands/shared.md"),
            "Personal shared command",
        )
        .unwrap();
        fs::write(
            account.join("commands/project.md"),
            "Personal project command",
        )
        .unwrap();
        let skills = discover(&launch(Provider::Claude, &cwd, temp.path(), &account));
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].description, "Project skill beats command");
        assert_eq!(skills[1].description, "Personal skill");
    }

    #[test]
    fn omp_xdg_configuration_is_separate_from_session_data() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let data = temp.path().join("data");
        write_skill(
            &config.join("omp/skills/configured"),
            "configured",
            "Configuration",
        );
        write_skill(&data.join("omp/skills/wrong"), "wrong", "Data");
        let mut launch = launch(
            Provider::Omp,
            temp.path(),
            temp.path(),
            &temp.path().join(".omp/agent"),
        );
        launch.recipe.environment.remove("PI_CODING_AGENT_DIR");
        launch
            .recipe
            .environment
            .insert("XDG_CONFIG_HOME".into(), config.display().to_string());
        launch
            .recipe
            .environment
            .insert("XDG_DATA_HOME".into(), data.display().to_string());
        assert_eq!(
            discover(&launch)
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["configured"]
        );
    }

    #[test]
    fn claude_skills_without_metadata_use_directory_and_body() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("skills/plain");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("SKILL.md"),
            "Review the project carefully.\nMore instructions.",
        )
        .unwrap();
        let skills = discover(&launch(
            Provider::Claude,
            temp.path(),
            temp.path(),
            temp.path(),
        ));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "plain");
        assert_eq!(skills[0].description, "Review the project carefully.");
        assert_eq!(skills[0].invocation, "/plain");
    }

    #[test]
    fn codex_skills_with_unrepresentable_paths_are_not_offered() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(&temp.path().join("skills/unsafe)"), "shared", "Unsafe link");
        write_skill(&temp.path().join("skills/safe"), "shared", "Safe link");
        write_skill(
            &temp.path().join("skills/unique)"),
            "unique",
            "Unrepresentable path",
        );
        let skills = discover(&launch(
            Provider::Codex,
            temp.path(),
            temp.path(),
            temp.path(),
        ));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "Safe link");
        assert!(skills[0].invocation.starts_with("[$shared]("));
    }

    #[test]
    fn omp_shared_skills_allow_missing_description_but_native_skills_require_it() {
        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join(".agents/skills/shared");
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join("SKILL.md"), "Instructions without metadata").unwrap();
        write_skill(&temp.path().join("skills/native"), "native", "''");
        write_skill(
            &temp.path().join("skills/disabled"),
            "disabled",
            "Disabled\nenabled: false",
        );
        let skills = discover(&launch(
            Provider::Omp,
            temp.path(),
            temp.path(),
            temp.path(),
        ));
        assert_eq!(
            skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["shared"]
        );
    }

    #[test]
    fn omp_can_disable_discovery_without_a_child_process() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(&temp.path().join("skills/test"), "test", "Test");
        let mut launch = launch(Provider::Omp, temp.path(), temp.path(), temp.path());
        assert_eq!(discover(&launch)[0].invocation, "/skill:test");
        launch.recipe.arguments.push("--no-skills".into());
        assert!(discover(&launch).is_empty());
    }
}
