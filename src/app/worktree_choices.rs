//! One local ref read when the creation dialog opens; typing only filters memory.
use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Local,
    Remote,
    Tag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Choice {
    pub name: String,
    pub reference: String,
    pub oid: String,
    pub kind: Kind,
    pub ahead: u64,
    pub behind: u64,
    pub remote_branch: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Choices {
    pub refs: Vec<Choice>,
    pub default_base: String,
    pub prefix: String,
    pub remotes: Vec<String>,
    pub primary: Option<std::path::PathBuf>,
    pub setup_plan: Option<crate::worktree_setup::SetupPlan>,
    pub setup_trusted: bool,
    pub setup_error: Option<String>,
}

impl Choices {
    pub fn load(repository: &Path, cancellation: &CancellationToken) -> Result<Self, String> {
        let command = git(repository)
            .args([
                "for-each-ref",
                "--format=%(refname)%00%(objectname)%00%(upstream:track)%00%(symref)%00%(HEAD)",
                "refs/heads",
                "refs/remotes",
                "refs/tags",
            ])
            .timeout(std::time::Duration::from_secs(10))
            .output_limits(2 * 1024 * 1024, 64 * 1024);
        let output = SystemProcessExecutor
            .execute(&command, cancellation)
            .and_then(|output| output.require_success(&command.program))
            .map_err(|error| error.to_string())?;
        let mut choices = Self::parse(&output.stdout)?;
        choices.remotes = crate::repository::RepositoryService::system("git")
            .remotes(repository, cancellation)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|remote| remote.name)
            .collect();
        for choice in &mut choices.refs {
            if choice.kind == Kind::Remote {
                choice.remote_branch = choices
                    .remotes
                    .iter()
                    .filter_map(|remote| choice.name.strip_prefix(&format!("{remote}/")))
                    .min_by_key(|branch| branch.len())
                    .map(str::to_owned);
            }
        }
        let command = git(repository)
            .args(["config", "--local", "--get", "cagents.worktreeBranchPrefix"])
            .timeout(std::time::Duration::from_secs(5))
            .output_limits(4096, 4096);
        if let Ok(output) = SystemProcessExecutor.execute(&command, cancellation)
            && output.success
        {
            choices.prefix = String::from_utf8(output.stdout)
                .map_err(|error| error.to_string())?
                .trim()
                .to_owned();
        }
        let service = crate::repository::RepositoryService::system("git");
        choices.primary = service
            .list_worktrees(repository, cancellation)
            .map_err(|error| error.to_string())?
            .first()
            .filter(|record| !record.bare)
            .map(|record| record.path.clone());
        if let Some(primary) = &choices.primary {
            match crate::worktree_setup::load(primary) {
                Ok(plan) => {
                    choices.setup_plan = plan;
                    if let Some(plan) = &choices.setup_plan {
                        match crate::worktree_setup::is_trusted(primary, plan, cancellation) {
                            Ok(trusted) => choices.setup_trusted = trusted,
                            Err(error) => choices.setup_error = Some(error.to_string()),
                        }
                    }
                }
                Err(error) => choices.setup_error = Some(error.to_string()),
            }
        }
        Ok(choices)
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
        let mut choices = Self::default();
        let mut local_head = None;
        for line in text.lines() {
            let fields = line.split('\0').collect::<Vec<_>>();
            let [reference, oid, tracking, symbolic, head] = fields.as_slice() else {
                return Err("Git returned an invalid branch list".into());
            };
            if !symbolic.is_empty() {
                if *reference == "refs/remotes/origin/HEAD" || choices.default_base.is_empty() {
                    choices.default_base = symbolic.to_string();
                }
                continue;
            }
            let (kind, name) = if let Some(name) = reference.strip_prefix("refs/heads/") {
                (Kind::Local, name)
            } else if let Some(name) = reference.strip_prefix("refs/remotes/") {
                (Kind::Remote, name)
            } else if let Some(name) = reference.strip_prefix("refs/tags/") {
                (Kind::Tag, name)
            } else {
                continue;
            };
            if *head == "*" {
                local_head = Some(reference.to_string());
            }
            let counts = tracking
                .trim_matches(['[', ']'])
                .split(", ")
                .collect::<Vec<_>>();
            let count = |prefix| {
                counts
                    .iter()
                    .find_map(|part| {
                        part.strip_prefix(prefix)
                            .and_then(|value| value.parse().ok())
                    })
                    .unwrap_or(0)
            };
            choices.refs.push(Choice {
                name: name.to_string(),
                reference: reference.to_string(),
                oid: oid.to_string(),
                kind,
                ahead: count("ahead "),
                behind: count("behind "),
                remote_branch: (kind == Kind::Remote)
                    .then(|| name.split_once('/').map(|(_, branch)| branch.to_string()))
                    .flatten(),
            });
        }
        if !choices.refs.iter().any(|choice| {
            choice.name == choices.default_base || choice.reference == choices.default_base
        }) {
            choices.default_base.clear();
        }
        if choices.default_base.is_empty() {
            choices.default_base = ["refs/remotes/origin/main", "refs/remotes/origin/master"]
                .into_iter()
                .find(|reference| {
                    choices
                        .refs
                        .iter()
                        .any(|choice| choice.kind == Kind::Remote && choice.reference == *reference)
                })
                .map(str::to_owned)
                .or(local_head)
                .unwrap_or_default();
        }
        Ok(choices)
    }

    pub fn existing_branch(&self, name: &str) -> Option<&Choice> {
        // Configured remote prefixes are explicit remote intent in the core
        // resolver. A dropdown's full local identity overrides that separately.
        if let Some(choice) = self.refs.iter().find(|choice| {
            choice.kind == Kind::Remote && choice.name == name && choice.remote_branch.is_some()
        }) {
            return Some(choice);
        }
        if let Some(choice) = self
            .refs
            .iter()
            .find(|choice| choice.kind == Kind::Local && choice.name == name)
        {
            return Some(choice);
        }
        let mut remote = self.refs.iter().filter(|choice| {
            choice.kind == Kind::Remote && choice.remote_branch.as_deref() == Some(name)
        });
        let choice = remote.next()?;
        remote.next().is_none().then_some(choice)
    }

    pub fn find(&self, name: &str) -> Option<&Choice> {
        self.refs
            .iter()
            .find(|choice| choice.reference == name)
            .or_else(|| self.refs.iter().find(|choice| choice.name == name))
    }

    pub fn suggestions(&self, query: &str, tags: bool) -> Vec<&Choice> {
        let query = query.to_lowercase();
        let mut choices = self
            .refs
            .iter()
            .filter(|choice| tags || choice.kind != Kind::Tag)
            .filter(|choice| {
                let mut characters = choice.name.chars().flat_map(char::to_lowercase);
                query
                    .chars()
                    .all(|wanted| characters.by_ref().any(|candidate| candidate == wanted))
            })
            .collect::<Vec<_>>();
        choices.sort_by(|left, right| {
            (
                !left.name.to_lowercase().contains(&query),
                left.kind != Kind::Local,
                &left.name,
            )
                .cmp(&(
                    !right.name.to_lowercase().contains(&query),
                    right.kind != Kind::Local,
                    &right.name,
                ))
        });
        choices.truncate(6);
        choices
    }

    pub fn effective_branch(&self, typed: &str) -> String {
        let typed = typed.trim();
        if self.prefix.is_empty()
            || typed.starts_with(&self.prefix)
            || self.existing_branch(typed).is_some()
            || self
                .remotes
                .iter()
                .any(|remote| typed.starts_with(&format!("{remote}/")))
            || typed.contains("://")
        {
            typed.to_owned()
        } else {
            format!("{}{typed}", self.prefix)
        }
    }

    /// Pin the exact cached ref advertised by the caption. Existing branches
    /// keep their own history, including explicit remote names not cached yet.
    pub fn creation_base(&self, branch: &str, typed: &str, selected: Option<&str>) -> String {
        if !typed.trim().is_empty() {
            return typed.to_owned();
        }
        let branch = self.effective_branch(branch);
        if selected.is_some()
            || self.existing_branch(&branch).is_some()
            || self
                .remotes
                .iter()
                .any(|remote| branch.starts_with(&format!("{remote}/")))
        {
            return String::new();
        }
        self.find(&self.default_base)
            .map(|choice| choice.reference.clone())
            .unwrap_or_default()
    }

    pub fn outcome(&self, branch: &str, base: &str) -> String {
        self.outcome_selected(branch, base, None)
    }

    pub fn outcome_selected(&self, branch: &str, base: &str, selected_ref: Option<&str>) -> String {
        let branch = self.effective_branch(branch);
        let selected = selected_ref.and_then(|reference| {
            self.refs
                .iter()
                .find(|choice| choice.reference == reference)
        });
        if let Some(existing) = selected.or_else(|| self.existing_branch(&branch)) {
            let progress = if existing.ahead > 0 {
                format!(" ({} ahead)", existing.ahead)
            } else {
                String::new()
            };
            return format!("Checks out existing {}{progress}", existing.name);
        }
        if self
            .remotes
            .iter()
            .any(|remote| branch.starts_with(&format!("{remote}/")))
        {
            return format!("Refreshes remote branch {branch} before checkout");
        }
        let base = if base.trim().is_empty() {
            self.default_base.as_str()
        } else {
            base.trim()
        };
        let base = if base.is_empty() {
            "repository default"
        } else {
            base
        };
        let stale = self
            .find(base)
            .filter(|choice| choice.kind == Kind::Local && choice.behind > 0)
            .map(|choice| {
                format!(
                    " · local {} is {} behind its upstream",
                    choice.name, choice.behind
                )
            })
            .unwrap_or_default();
        let display_base = self.find(base).map_or(base, |choice| choice.name.as_str());
        format!("New branch {branch} from {display_base}{stale}")
    }
}

fn git(repository: &Path) -> CommandSpec {
    crate::repository::RepositoryService::system("git")
        .git_base(repository, false)
        .env("GIT_NO_LAZY_FETCH", "1")
}

pub(super) fn save_prefix(
    repository: &Path,
    prefix: &str,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    if !prefix.is_empty() {
        crate::repository::ValidatedBranchName::parse(format!("{prefix}placeholder"))
            .map_err(|error| error.to_string())?;
    }
    let command = git(repository).args([
        "config",
        "--local",
        "--replace-all",
        "cagents.worktreeBranchPrefix",
        prefix,
    ]);
    SystemProcessExecutor
        .execute(&command, cancellation)
        .and_then(|output| output.require_success(&command.program))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Preserve literal branch case while converting pasted prose into branch syntax.
pub(super) fn sanitize_branch(value: &str) -> String {
    if value.contains("://") {
        return value.to_string();
    }
    let prose = value.chars().any(char::is_whitespace);
    let mut result = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_whitespace() {
            if !result.ends_with('-') {
                result.push('-');
            }
        } else if !character.is_control()
            && !matches!(character, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        {
            if prose {
                result.extend(character.to_lowercase());
            } else {
                result.push(character);
            }
        }
    }
    result.trim_start_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn choices() -> Choices {
        Choices::parse(b"refs/heads/main\0aaaa\0[behind 14]\0\0*\nrefs/heads/feature/x\0bbbb\0[ahead 2]\0\0 \nrefs/remotes/origin/main\0cccc\0\0\0 \nrefs/remotes/origin/feature/y\0dddd\0\0\0 \nrefs/remotes/origin/HEAD\0cccc\0\0refs/remotes/origin/main\0 \nrefs/tags/v1\0eeee\0\0\0 \n").unwrap()
    }
    #[test]
    fn outcomes_explain_new_existing_remote_and_stale_local() {
        let choices = choices();
        assert_eq!(
            choices.outcome("new", ""),
            "New branch new from origin/main"
        );
        assert_eq!(
            choices.outcome("feature/x", ""),
            "Checks out existing feature/x (2 ahead)"
        );
        assert_eq!(
            choices.outcome("origin/feature/y", ""),
            "Checks out existing origin/feature/y"
        );
        assert_eq!(
            choices.outcome("new", "main"),
            "New branch new from main · local main is 14 behind its upstream"
        );
    }
    #[test]
    fn remote_default_stays_distinct_from_a_local_branch_with_its_display_name() {
        let mut choices = choices();
        let mut collision = choices
            .refs
            .iter()
            .find(|choice| choice.name == "main")
            .unwrap()
            .clone();
        collision.name = "origin/main".into();
        collision.reference = "refs/heads/origin/main".into();
        collision.oid = "ffff".into();
        choices.refs.insert(0, collision);
        assert_eq!(choices.default_base, "refs/remotes/origin/main");
        assert_eq!(
            choices.creation_base("new", "", None),
            "refs/remotes/origin/main"
        );
    }
    #[test]
    fn selected_local_identity_wins_over_a_matching_remote_display_name() {
        let mut choices = choices();
        let mut local = choices
            .refs
            .iter()
            .find(|choice| choice.name == "origin/feature/y")
            .unwrap()
            .clone();
        local.kind = Kind::Local;
        local.reference = "refs/heads/origin/feature/y".into();
        local.ahead = 7;
        choices.refs.push(local);
        assert_eq!(
            choices.outcome("origin/feature/y", ""),
            "Checks out existing origin/feature/y"
        );
        assert_eq!(
            choices.outcome_selected("origin/feature/y", "", Some("refs/heads/origin/feature/y")),
            "Checks out existing origin/feature/y (7 ahead)"
        );
    }
    #[test]
    fn blank_base_pins_the_captioned_ref_across_remote_default_ambiguity() {
        let mut choices = Choices::parse(b"refs/heads/main\0aaaa\0\0\0*\nrefs/remotes/origin/main\0bbbb\0\0\0 \nrefs/remotes/upstream/release\0cccc\0\0\0 \nrefs/remotes/upstream/HEAD\0cccc\0\0refs/remotes/upstream/release\0 \n").unwrap();
        choices.remotes = vec!["origin".into(), "upstream".into()];
        assert!(
            choices
                .outcome("new", "")
                .ends_with("from upstream/release")
        );
        assert_eq!(
            choices.creation_base("new", "", None),
            "refs/remotes/upstream/release"
        );
        choices.remotes = vec!["upstream".into(), "another".into()];
        assert_eq!(
            choices.creation_base("new", "", None),
            "refs/remotes/upstream/release"
        );
        assert_eq!(choices.creation_base("main", "", None), "");
        assert_eq!(choices.creation_base("upstream/not-cached", "", None), "");
        assert_eq!(choices.creation_base("new", "main", None), "main");
    }
    #[test]
    fn autocomplete_includes_remote_branches_and_base_tags() {
        let choices = choices();
        assert_eq!(
            choices.suggestions("ofy", false)[0].name,
            "origin/feature/y"
        );
        assert!(choices.suggestions("v1", false).is_empty());
        assert_eq!(choices.suggestions("v1", true)[0].kind, Kind::Tag);
    }
    #[test]
    fn pasted_titles_are_sanitized_and_prefix_only_applies_to_new_branches() {
        assert_eq!(sanitize_branch("Fix Login Redirect"), "fix-login-redirect");
        assert_eq!(
            sanitize_branch("feature/CaseSensitive"),
            "feature/CaseSensitive"
        );
        let mut choices = choices();
        choices.prefix = "agent/".into();
        assert_eq!(choices.effective_branch("new"), "agent/new");
        assert_eq!(choices.effective_branch("agent/new"), "agent/new");
        assert_eq!(choices.effective_branch("feature/x"), "feature/x");
    }
}
