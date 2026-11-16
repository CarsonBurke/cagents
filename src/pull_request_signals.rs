//! The pull requests a conversation points at, read from what its agents and
//! its user actually did: the commands they ran and what those printed, the
//! tools they called, and the prose they wrote. Nothing here asks an agent to
//! declare anything, and nothing here contacts a forge; a reference names a
//! pull request only as precisely as the transcript does, and resolving a
//! number or a branch to a pull request is the forge poller's job.

use crate::github_identity::{GitHubRepoKey, PullRequestKey};
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

pub use crate::work::Relation;

/// A pull request as precisely as the transcript names it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Reference {
    /// A full URL, or `owner/repo#N`, or a number qualified by `--repo`.
    PullRequest(PullRequestKeyOrd),
    /// `#N` or a bare number: the conversation's own repository, and only if
    /// the forge says it is a pull request rather than an issue.
    Number(u64),
    /// The pull request whose head is this branch.
    Branch {
        /// The repository the branch was pushed to, when the output said.
        repository: Option<RepoKeyOrd>,
        name: String,
    },
    /// Whatever branch is checked out in the command's directory, as for
    /// `gh pr merge` or `git push` without a branch. `None` is the
    /// conversation's own working directory.
    CurrentBranch { directory: Option<String> },
}

/// One thing the conversation did that points at a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Signal {
    pub reference: Reference,
    pub relation: Relation,
}

/// [`GitHubRepoKey`] ordered by its fields, so references sort and dedupe.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RepoKeyOrd {
    pub host: String,
    pub port: Option<u16>,
    pub owner: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PullRequestKeyOrd {
    pub repository: RepoKeyOrd,
    pub number: u64,
}

impl From<&GitHubRepoKey> for RepoKeyOrd {
    /// Owner and name compare case-insensitively on GitHub, so they are kept
    /// in one spelling here; the forge answers with its canonical one.
    fn from(key: &GitHubRepoKey) -> Self {
        Self {
            host: key.host.to_ascii_lowercase(),
            port: key.port,
            owner: key.owner.to_ascii_lowercase(),
            name: key.name.to_ascii_lowercase(),
        }
    }
}

impl From<&RepoKeyOrd> for GitHubRepoKey {
    fn from(key: &RepoKeyOrd) -> Self {
        Self {
            host: key.host.clone(),
            port: key.port,
            owner: key.owner.clone(),
            name: key.name.clone(),
        }
    }
}

impl From<&PullRequestKey> for PullRequestKeyOrd {
    fn from(key: &PullRequestKey) -> Self {
        Self {
            repository: (&key.repository).into(),
            number: key.number,
        }
    }
}

impl From<&PullRequestKeyOrd> for PullRequestKey {
    fn from(key: &PullRequestKeyOrd) -> Self {
        Self {
            repository: (&key.repository).into(),
            number: key.number,
        }
    }
}

fn signal(reference: Reference, relation: Relation) -> Signal {
    Signal {
        reference,
        relation,
    }
}

fn key_reference(key: &PullRequestKey) -> Reference {
    Reference::PullRequest(key.into())
}

/// A shell command a provider ran, with what it printed. `succeeded` is
/// `None` while it runs or when the provider does not say.
pub fn command(command: &str, output: &str, succeeded: Option<bool>) -> Vec<Signal> {
    let mut signals = Vec::new();
    script(command, output, succeeded, 0, &mut signals);
    // Any pull request URL in the command itself, such as `curl` or
    // `gh api` against one, at least names it.
    for key in pull_request_urls(command) {
        signals.push(signal(key_reference(&key), Relation::Mentioned));
    }
    dedupe(signals)
}

fn script(
    command: &str,
    output: &str,
    succeeded: Option<bool>,
    depth: u8,
    signals: &mut Vec<Signal>,
) {
    let mut directory: Option<String> = None;
    for words in simple_commands(command) {
        let words = strip_assignments(&words);
        let Some((program, rest)) = words.split_first() else {
            continue;
        };
        match program_name(program) {
            "cd" => directory = rest.first().cloned(),
            "git" => git(rest, output, succeeded, &directory, signals),
            "gh" => gh(rest, output, succeeded, &directory, signals),
            // Codex runs every command as `bash -lc '…'`.
            "bash" | "sh" | "zsh" | "dash" if depth < 2 => {
                if let Some(at) = rest.iter().position(|arg| {
                    arg.starts_with('-') && !arg.starts_with("--") && arg.ends_with('c')
                }) && let Some(inner) = rest.get(at + 1)
                {
                    script(inner, output, succeeded, depth + 1, signals);
                }
            }
            _ => {}
        }
    }
}

/// A tool call other than a shell command, such as a forge MCP server's.
pub fn tool(name: &str, input: &str, output: &str) -> Vec<Signal> {
    let lower = name.to_ascii_lowercase();
    let mut signals = Vec::new();
    let forge = lower.contains("pull_request") || lower.contains("pullrequest");
    if forge {
        let relation = if lower.contains("create") {
            Relation::Created
        } else if lower.contains("merge") || lower.contains("update") {
            Relation::Updated
        } else if lower.contains("review") || lower.contains("comment") {
            Relation::Reviewed
        } else {
            Relation::Mentioned
        };
        if relation == Relation::Created {
            // The new pull request is the one the output links to.
            for key in pull_request_urls(output) {
                signals.push(signal(key_reference(&key), Relation::Created));
            }
        } else if let Some(key) = json_pull_request(input) {
            signals.push(signal(key_reference(&key), relation));
        }
    }
    for key in pull_request_urls(input) {
        signals.push(signal(key_reference(&key), Relation::Mentioned));
    }
    dedupe(signals)
}

/// Prose the user or an agent wrote, or a report delivered to the
/// conversation. Fenced and inline code are skipped for `#N`, where a number
/// is as likely a colour or a line as a pull request; URLs count anywhere.
pub fn prose(text: &str) -> Vec<Signal> {
    let mut signals = pull_request_urls(text)
        .into_iter()
        .map(|key| signal(key_reference(&key), Relation::Mentioned))
        .collect::<Vec<_>>();
    for line in outside_code(text) {
        hash_references(&line, &mut signals);
    }
    dedupe(signals)
}

fn dedupe(mut signals: Vec<Signal>) -> Vec<Signal> {
    signals.sort_by(|a, b| {
        a.reference
            .cmp(&b.reference)
            .then(a.relation.cmp(&b.relation))
    });
    signals.dedup_by(|later, earlier| later.reference == earlier.reference);
    signals
}

fn program_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn strip_assignments(words: &[String]) -> Vec<String> {
    let mut words = words
        .iter()
        .skip_while(|word| {
            word.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    // `env A=B git push`, `command git`, `exec gh`.
    while words
        .first()
        .is_some_and(|word| matches!(word.as_str(), "env" | "command" | "exec" | "time"))
    {
        words.remove(0);
        words = strip_assignments(&words);
    }
    words
}

/// Split shell text into simple commands of words. Quotes and backslashes
/// are honoured; separators, pipes and redirections end a word. This reads
/// what a command plainly says, not everything a shell could make of it.
fn simple_commands(text: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    let finish_word = |word: &mut String, in_word: &mut bool, words: &mut Vec<String>| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(next) = chars.next() {
                                word.push(next);
                            }
                        }
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    if next != '\n' {
                        in_word = true;
                        word.push(next);
                    }
                }
            }
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                finish_word(&mut word, &mut in_word, &mut words);
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            '>' | '<' => {
                // A redirection's target is not an argument, and neither is
                // the descriptor number in front of it, as in `2>&1`.
                if in_word && !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                }
                finish_word(&mut word, &mut in_word, &mut words);
                while chars.peek().is_some_and(|c| matches!(c, '>' | '&' | '<')) {
                    chars.next();
                }
                while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
                    chars.next();
                }
                while chars
                    .peek()
                    .is_some_and(|c| !c.is_whitespace() && !matches!(c, ';' | '&' | '|'))
                {
                    chars.next();
                }
            }
            '#' if !in_word => {
                // A comment runs to the end of the line.
                while chars.peek().is_some_and(|c| *c != '\n') {
                    chars.next();
                }
            }
            c if c.is_whitespace() => finish_word(&mut word, &mut in_word, &mut words),
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    finish_word(&mut word, &mut in_word, &mut words);
    if !words.is_empty() {
        commands.push(words);
    }
    commands
}

fn git(
    args: &[String],
    output: &str,
    succeeded: Option<bool>,
    directory: &Option<String>,
    signals: &mut Vec<Signal>,
) {
    let mut directory = directory.clone();
    let mut index = 0;
    // Global options before the subcommand.
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-C" => {
                directory = args.get(index + 1).cloned();
                index += 2;
            }
            "-c" | "--git-dir" | "--work-tree" | "--namespace" => index += 2,
            arg if arg.starts_with('-') => index += 1,
            _ => break,
        }
    }
    let Some(subcommand) = args.get(index) else {
        return;
    };
    let rest = &args[index + 1..];
    match subcommand.as_str() {
        "push" => push(rest, output, succeeded, directory, signals),
        "checkout" | "switch" => {
            if let Some(branch) = checkout_branch(rest) {
                signals.push(signal(
                    Reference::Branch {
                        repository: None,
                        name: branch,
                    },
                    Relation::CheckedOut,
                ));
            }
        }
        "worktree" if rest.first().map(String::as_str) == Some("add") => {
            if let Some(branch) = worktree_branch(&rest[1..]) {
                signals.push(signal(
                    Reference::Branch {
                        repository: None,
                        name: branch,
                    },
                    Relation::CheckedOut,
                ));
            }
        }
        _ => {}
    }
}

/// The branch `git checkout`/`git switch` lands on, if it plainly names one.
/// A path after `--`, a detached commit or `-` (the previous branch) does not.
fn checkout_branch(args: &[String]) -> Option<String> {
    let mut created = None;
    let mut positional = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => return created,
            "-b" | "-B" | "-c" | "-C" | "--orphan" => created = iter.next().cloned(),
            "--track" | "-t" | "--detach" | "-d" => {}
            arg if arg.starts_with('-') => {}
            arg => {
                if positional.is_none() {
                    positional = Some(arg.to_owned());
                }
            }
        }
    }
    created
        .or(positional)
        .filter(|branch| plausible_branch(branch))
        .map(|branch| strip_remote_prefix(&branch))
}

fn worktree_branch(args: &[String]) -> Option<String> {
    let mut created = None;
    let mut positionals = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-b" | "-B" => created = iter.next().cloned(),
            "--reason" => {
                iter.next();
            }
            arg if arg.starts_with('-') => {}
            arg => positionals.push(arg.to_owned()),
        }
    }
    created
        .or_else(|| positionals.get(1).cloned())
        .filter(|branch| plausible_branch(branch))
        .map(|branch| strip_remote_prefix(&branch))
}

fn plausible_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch != "-"
        && branch != "."
        && branch != "HEAD"
        && !branch.starts_with('@')
        && !branch.contains("..")
        && !(branch.len() >= 7 && branch.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn strip_remote_prefix(branch: &str) -> String {
    for remote in ["origin/", "upstream/"] {
        if let Some(name) = branch.strip_prefix(remote) {
            return name.to_owned();
        }
    }
    branch.to_owned()
}

fn push(
    args: &[String],
    output: &str,
    succeeded: Option<bool>,
    directory: Option<String>,
    signals: &mut Vec<Signal>,
) {
    let reported = pushed_branches(output);
    if !reported.is_empty() {
        for (repository, name) in reported {
            signals.push(signal(
                Reference::Branch {
                    repository: repository.as_ref().map(Into::into),
                    name,
                },
                Relation::Pushed,
            ));
        }
        return;
    }
    // Quiet or filtered output: only a push that is known to have worked
    // says what it pushed through its arguments.
    if succeeded != Some(true) || output_says_nothing_pushed(output) {
        return;
    }
    let mut positionals = Vec::new();
    let mut deleting = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--delete" | "-d" | "--tags" | "--mirror" | "--all" | "--dry-run" | "-n" => {
                deleting = true
            }
            "--repo" | "--push-option" | "-o" | "--receive-pack" | "--exec" => {
                iter.next();
            }
            arg if arg.starts_with('-') => {}
            arg => positionals.push(arg.to_owned()),
        }
    }
    if deleting {
        return;
    }
    let refspecs = positionals.iter().skip(1).collect::<Vec<_>>();
    if refspecs.is_empty() {
        signals.push(signal(
            Reference::CurrentBranch { directory },
            Relation::Pushed,
        ));
        return;
    }
    for refspec in refspecs {
        let refspec = refspec.trim_start_matches('+');
        let destination = refspec.rsplit_once(':').map_or(refspec, |(_, to)| to);
        let destination = destination
            .strip_prefix("refs/heads/")
            .unwrap_or(destination);
        if destination == "HEAD" || destination.is_empty() {
            signals.push(signal(
                Reference::CurrentBranch {
                    directory: directory.clone(),
                },
                Relation::Pushed,
            ));
        } else if plausible_branch(destination) && !destination.starts_with("refs/") {
            signals.push(signal(
                Reference::Branch {
                    repository: None,
                    name: destination.to_owned(),
                },
                Relation::Pushed,
            ));
        }
    }
}

fn output_says_nothing_pushed(output: &str) -> bool {
    output.contains("Everything up-to-date") || output.contains("[rejected]")
}

/// Branches `git push` reports updating, with the repository each `To` line
/// names. Rejected, deleted and tag updates are not pushes of a branch.
fn pushed_branches(output: &str) -> Vec<(Option<GitHubRepoKey>, String)> {
    let mut branches = Vec::new();
    let mut repository = None;
    // A script's `git fetch` or `git pull` reports its updates the same way
    // under a `From` line; those are not pushes.
    let mut fetched = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(remote) = trimmed.strip_prefix("To ") {
            repository = crate::github::remote_repository_key(remote.trim());
            fetched = false;
            continue;
        }
        if trimmed.starts_with("From ") {
            fetched = true;
            continue;
        }
        if fetched {
            continue;
        }
        // Human: ` * [new branch]      feat -> feat`, `   1a..2b  feat -> feat`,
        // ` + 1a...2b feat -> feat (forced update)`.
        // Porcelain: `*\trefs/heads/feat:refs/heads/feat\t[new branch]`.
        let destination = if let Some((_, right)) = trimmed.split_once(" -> ") {
            if trimmed.contains("[rejected]")
                || trimmed.contains("[remote rejected]")
                || trimmed.contains("[deleted]")
                || trimmed.contains("[new tag]")
                || trimmed.starts_with('!')
                || trimmed.starts_with('-')
            {
                continue;
            }
            let looks_like_update = trimmed.starts_with('*')
                || trimmed.starts_with('+')
                || trimmed.starts_with("[new")
                || trimmed
                    .split_whitespace()
                    .next()
                    .is_some_and(|range| range.contains(".."));
            if !looks_like_update {
                continue;
            }
            right.split_whitespace().next().unwrap_or_default()
        } else if let Some(rest) = line.strip_prefix(['*', '+', ' '])
            && let Some(rest) = rest.strip_prefix('\t')
        {
            let refs = rest.split('\t').next().unwrap_or_default();
            refs.rsplit_once(':').map_or("", |(_, to)| to)
        } else {
            continue;
        };
        let Some(name) = destination
            .strip_prefix("refs/heads/")
            .or((!destination.starts_with("refs/")).then_some(destination))
        else {
            continue;
        };
        if plausible_branch(name) {
            branches.push((repository.clone(), name.to_owned()));
        }
    }
    branches
}

/// Whether a `gh pr` flag takes a value. Some short flags take one in one
/// subcommand and are switches in another: `-r` is `--reviewer` to
/// `create` but `--rebase` to `merge` and `--request-changes` to `review`.
fn takes_value(subcommand: Option<&str>, flag: &str) -> bool {
    match flag {
        "-a" | "-r" => matches!(subcommand, Some("create" | "new")),
        "-m" => matches!(subcommand, Some("create" | "new" | "edit")),
        "-A" => subcommand == Some("merge"),
        "-i" => subcommand == Some("checks"),
        _ => matches!(
            flag,
            "-t" | "--title"
                | "-b"
                | "--body"
                | "-F"
                | "--body-file"
                | "-B"
                | "--base"
                | "-H"
                | "--head"
                | "-l"
                | "--label"
                | "--assignee"
                | "--reviewer"
                | "--milestone"
                | "-p"
                | "--project"
                | "--json"
                | "-q"
                | "--jq"
                | "--template"
                | "-T"
                | "--add-label"
                | "--remove-label"
                | "--add-reviewer"
                | "--remove-reviewer"
                | "--add-assignee"
                | "--remove-assignee"
                | "--add-project"
                | "--remove-project"
                | "--subject"
                | "--match-head-commit"
                | "--author-email"
                | "--branch"
                | "--remote-name"
                | "--interval"
                | "--color"
        ),
    }
}

fn gh(
    args: &[String],
    output: &str,
    succeeded: Option<bool>,
    directory: &Option<String>,
    signals: &mut Vec<Signal>,
) {
    let mut repository = None;
    let mut positionals = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg
            .strip_prefix("--repo=")
            .or_else(|| arg.strip_prefix("-R").filter(|value| !value.is_empty()))
        {
            repository = repository_selector(value);
        } else if arg == "-R" || arg == "--repo" {
            repository = iter.next().and_then(|value| repository_selector(value));
        } else if arg.starts_with('-') {
            // Flags whose value would otherwise read as the selector. The
            // subcommand precedes its flags.
            if !arg.contains('=') && takes_value(positionals.get(1).copied(), arg) {
                iter.next();
            }
        } else {
            positionals.push(arg.as_str());
        }
    }
    if positionals.first() != Some(&"pr") {
        return;
    }
    let Some(subcommand) = positionals.get(1).copied() else {
        return;
    };
    let selector = positionals.get(2).copied();
    let relation = match subcommand {
        "create" | "new" => {
            if succeeded == Some(false) && !output.contains("already exists") {
                return;
            }
            let existing = output.contains("already exists");
            // gh prints the pull request last; the rest of the output may be
            // another command's, sharing this one's in a script.
            if let Some(key) = pull_request_urls(output).pop() {
                let relation = if existing {
                    Relation::Pushed
                } else {
                    Relation::Created
                };
                signals.push(signal(key_reference(&key), relation));
            }
            return;
        }
        "checkout" | "co" => Relation::CheckedOut,
        "merge" | "edit" | "ready" | "close" | "reopen" | "update-branch" | "lock" | "unlock" => {
            Relation::Updated
        }
        "review" | "comment" => Relation::Reviewed,
        "view" | "diff" | "checks" => Relation::Mentioned,
        _ => return,
    };
    // A failed command still named the pull request; it only did no work.
    let relation = if succeeded == Some(false) {
        Relation::Mentioned
    } else {
        relation
    };
    let reference = match selector {
        Some(selector) => match selector_reference(selector, repository.as_ref()) {
            Some(reference) => reference,
            None => return,
        },
        None => Reference::CurrentBranch {
            directory: directory.clone(),
        },
    };
    signals.push(signal(reference, relation));
}

/// `gh -R` accepts `OWNER/REPO` or `HOST/OWNER/REPO`.
fn repository_selector(value: &str) -> Option<GitHubRepoKey> {
    let parts = value.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        [owner, name] => GitHubRepoKey::new("github.com", None, *owner, *name).ok(),
        [host, owner, name] => {
            let (host, port) = match host.rsplit_once(':') {
                Some((host, port)) => (host, port.parse().ok()),
                None => (*host, None),
            };
            GitHubRepoKey::new(host, port, *owner, *name).ok()
        }
        _ => None,
    }
}

/// A `gh pr` selector: a number, a URL or a branch.
fn selector_reference(selector: &str, repository: Option<&GitHubRepoKey>) -> Option<Reference> {
    if let Some(key) = pull_request_urls(selector).into_iter().next() {
        return Some(key_reference(&key));
    }
    let digits = selector.strip_prefix('#').unwrap_or(selector);
    if let Ok(number) = digits.parse::<u64>() {
        if number == 0 {
            return None;
        }
        return Some(match repository {
            Some(repository) => Reference::PullRequest(PullRequestKeyOrd {
                repository: repository.into(),
                number,
            }),
            None => Reference::Number(number),
        });
    }
    // `OWNER:branch` names a fork's branch.
    let branch = selector.rsplit_once(':').map_or(selector, |(_, b)| b);
    plausible_branch(branch).then(|| Reference::Branch {
        repository: None,
        name: branch.to_owned(),
    })
}

/// Pull request URLs in free text: `https://HOST/OWNER/REPO/pull/N`, with or
/// without a trailing `/files`, query or fragment, and wrapped in Markdown,
/// quotes or punctuation.
pub fn pull_request_urls(text: &str) -> Vec<PullRequestKey> {
    let mut keys = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("https://") {
        let candidate = &rest[start..];
        let end = candidate
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | ')' | ']' | '|')
            })
            .unwrap_or(candidate.len());
        let url = &candidate[..end];
        rest = &candidate[end..];
        let url = url.split(['?', '#']).next().unwrap_or(url);
        let url = url.trim_end_matches(['.', ',', ';', ':', '!', '*']);
        let Some(index) = url.find("/pull/") else {
            continue;
        };
        let after = &url[index + "/pull/".len()..];
        let number_len = after.bytes().take_while(u8::is_ascii_digit).count();
        if number_len == 0 {
            continue;
        }
        let url = &url[..index + "/pull/".len() + number_len];
        if let Ok(key) = PullRequestKey::from_url(url) {
            keys.push(key);
        }
    }
    keys
}

/// `owner/repo#N` and `#N` in one line of prose.
fn hash_references(line: &str, signals: &mut Vec<Signal>) {
    let bytes = line.as_bytes();
    for (index, _) in line.match_indices('#') {
        let digits = bytes[index + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits == 0 || digits > 7 {
            continue;
        }
        let end = index + 1 + digits;
        if bytes
            .get(end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            continue; // `#123abc`: a colour or an anchor
        }
        let Ok(number) = line[index + 1..end].parse::<u64>() else {
            continue;
        };
        if number == 0 {
            continue;
        }
        let before = &line[..index];
        // The token starts after the last character outside it, which may
        // take more than one byte, as a dash in `#1084–#1090` does.
        let token_start = before
            .char_indices()
            .rfind(|&(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.')))
            .map_or(0, |(at, c)| at + c.len_utf8());
        let token = &before[token_start..];
        if token.is_empty() {
            let previous = before.chars().next_back();
            if previous.is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | ',' | ':')) {
                signals.push(signal(Reference::Number(number), Relation::Mentioned));
            }
        } else if let Some((owner, name)) = token.split_once('/')
            && !name.contains('/')
            && let Ok(repository) = GitHubRepoKey::new("github.com", None, owner, name)
        {
            signals.push(signal(
                Reference::PullRequest(PullRequestKeyOrd {
                    repository: (&repository).into(),
                    number,
                }),
                Relation::Mentioned,
            ));
        }
    }
}

/// The lines of Markdown outside fenced code, with inline code spans blanked.
fn outside_code(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut fence: Option<&str> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(open) = fence {
            if trimmed.starts_with(open) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some("~~~");
            continue;
        }
        let mut kept = String::with_capacity(line.len());
        let mut in_code = false;
        for c in line.chars() {
            if c == '`' {
                in_code = !in_code;
                kept.push(' ');
            } else {
                kept.push(if in_code { ' ' } else { c });
            }
        }
        lines.push(kept);
    }
    lines
}

/// `{"owner": .., "repo": .., "pull_number"|"pullNumber"|"number": ..}`, the
/// shape forge MCP tools take.
fn json_pull_request(input: &str) -> Option<PullRequestKey> {
    let value = serde_json::from_str::<serde_json::Value>(input).ok()?;
    let owner = value.get("owner")?.as_str()?;
    let name = value
        .get("repo")
        .or_else(|| value.get("repository"))?
        .as_str()?;
    let number = ["pull_number", "pullNumber", "number", "pr_number"]
        .iter()
        .find_map(|field| {
            let value = value.get(*field)?;
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })?;
    let repository = GitHubRepoKey::new("github.com", None, owner, name).ok()?;
    PullRequestKey::new(repository, number).ok()
}
