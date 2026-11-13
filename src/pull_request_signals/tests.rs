use super::*;

fn key(owner: &str, name: &str, number: u64) -> Reference {
    Reference::PullRequest(PullRequestKeyOrd {
        repository: RepoKeyOrd {
            host: "github.com".into(),
            port: None,
            owner: owner.into(),
            name: name.into(),
        },
        number,
    })
}

fn branch(repository: Option<(&str, &str)>, name: &str) -> Reference {
    Reference::Branch {
        repository: repository.map(|(owner, name)| RepoKeyOrd {
            host: "github.com".into(),
            port: None,
            owner: owner.into(),
            name: name.into(),
        }),
        name: name.into(),
    }
}

fn of(signals: &[Signal]) -> Vec<(Reference, Relation)> {
    signals
        .iter()
        .map(|signal| (signal.reference.clone(), signal.relation))
        .collect()
}

#[test]
fn gh_pr_create_output_names_the_created_pull_request() {
    let signals = command(
        "cd /work/app && gh pr create --title 'Fix #12 crash' --body \"See https://github.com/o/r/pull/3\"",
        "Creating pull request for feat into main in o/r\n\nhttps://github.com/O/R/pull/42\n",
        Some(true),
    );
    // The body's own link is a mention; the printed URL is the creation.
    // Repository spelling folds to one case.
    assert_eq!(
        of(&signals),
        vec![
            (key("o", "r", 3), Relation::Mentioned),
            (key("o", "r", 42), Relation::Created),
        ]
    );
}

#[test]
fn only_the_url_create_prints_last_is_created_and_a_failed_create_creates_nothing() {
    // The script's output is shared: #5 is what `gh pr view` printed.
    let signals = command(
        "gh pr view 5 --json url && gh pr create --fill",
        "{\"url\":\"https://github.com/o/r/pull/5\"}\nhttps://github.com/o/r/pull/6\n",
        Some(true),
    );
    assert_eq!(
        of(&signals),
        vec![
            (key("o", "r", 6), Relation::Created),
            (Reference::Number(5), Relation::Mentioned),
        ]
    );
    let signals = command(
        "gh pr create --fill",
        "could not create: https://github.com/o/r/pull/7 is not reachable",
        Some(false),
    );
    assert!(of(&signals).is_empty());
}

#[test]
fn gh_flags_with_and_without_values_keep_the_selector() {
    assert_eq!(
        of(&command("gh pr checks --fail-fast 12", "", Some(true))),
        vec![(Reference::Number(12), Relation::Mentioned)]
    );
    assert_eq!(
        of(&command("gh pr view --color never 13", "", Some(true))),
        vec![(Reference::Number(13), Relation::Mentioned)]
    );
}

#[test]
fn short_flags_that_are_switches_in_one_subcommand_do_not_swallow_the_selector() {
    assert_eq!(
        of(&command("gh pr merge -r -d 12", "", Some(true))),
        vec![(Reference::Number(12), Relation::Updated)]
    );
    assert_eq!(
        of(&command("gh pr merge -m 12", "", Some(true))),
        vec![(Reference::Number(12), Relation::Updated)]
    );
    assert_eq!(
        of(&command("gh pr review -a 12", "", Some(true))),
        vec![(Reference::Number(12), Relation::Reviewed)]
    );
    assert_eq!(
        of(&command(
            "gh pr review -r -b 'needs tests' 14",
            "",
            Some(true)
        )),
        vec![(Reference::Number(14), Relation::Reviewed)]
    );
    // In `edit` and `create` they take values.
    assert_eq!(
        of(&command("gh pr edit -m v2 15", "", Some(true))),
        vec![(Reference::Number(15), Relation::Updated)]
    );
}

#[test]
fn an_existing_pull_request_reported_by_create_is_the_branch_s_not_a_new_one() {
    let signals = command(
        "gh pr create --fill",
        "a pull request for branch \"feat\" into branch \"main\" already exists:\nhttps://github.com/o/r/pull/9",
        Some(false),
    );
    assert_eq!(of(&signals), vec![(key("o", "r", 9), Relation::Pushed)]);
}

#[test]
fn git_push_output_names_branches_and_the_repository_they_went_to() {
    let output = "Enumerating objects: 5, done.\n\
        remote: Create a pull request for 'feat/x' on GitHub by visiting:\n\
        remote:      https://github.com/me/r/pull/new/feat/x\n\
        To github.com:me/r.git\n \
        * [new branch]      feat/x -> feat/x\n\
        branch 'feat/x' set up to track 'origin/feat/x'.\n";
    let signals = command("git push -u origin feat/x", output, Some(true));
    assert_eq!(
        of(&signals),
        vec![(branch(Some(("me", "r")), "feat/x"), Relation::Pushed)]
    );
}

#[test]
fn a_fetch_beside_a_push_in_one_script_is_not_a_push() {
    let output = "From github.com:o/r\n \
        * [new branch]      other      -> origin/other\n \
        * branch            main       -> FETCH_HEAD\n\
        To github.com:o/r.git\n   \
        1a2b3c4..5d6e7f8  feat -> feat\n";
    let signals = command("git pull origin main && git push", output, Some(true));
    assert_eq!(
        of(&signals),
        vec![(branch(Some(("o", "r")), "feat"), Relation::Pushed)]
    );
}

#[test]
fn forced_and_fast_forward_updates_count_but_rejections_and_deletions_do_not() {
    let output = "To https://github.com/o/r.git\n   \
        1a2b3c4..5d6e7f8  fast -> fast\n \
        + 1a2b3c4...5d6e7f8 forced -> forced (forced update)\n \
        ! [rejected]        stale -> stale (non-fast-forward)\n \
        - [deleted]         gone\n \
        * [new tag]         v1 -> v1\n";
    let signals = command(
        "git push origin fast forced stale :gone v1",
        output,
        Some(false),
    );
    assert_eq!(
        of(&signals),
        vec![
            (branch(Some(("o", "r")), "fast"), Relation::Pushed),
            (branch(Some(("o", "r")), "forced"), Relation::Pushed),
        ]
    );
}

#[test]
fn porcelain_push_output_is_read_too() {
    let output = "To github.com:o/r.git\n*\trefs/heads/feat:refs/heads/feat\t[new branch]\nDone\n";
    let signals = command("git push --porcelain origin feat", output, Some(true));
    assert_eq!(
        of(&signals),
        vec![(branch(Some(("o", "r")), "feat"), Relation::Pushed)]
    );
}

#[test]
fn a_quiet_push_falls_back_to_its_arguments_only_when_it_worked() {
    assert_eq!(
        of(&command("git push -q origin HEAD:feat", "", Some(true))),
        vec![(branch(None, "feat"), Relation::Pushed)]
    );
    assert_eq!(
        of(&command("git -C ../wt push -q", "", Some(true))),
        vec![(
            Reference::CurrentBranch {
                directory: Some("../wt".into())
            },
            Relation::Pushed
        )]
    );
    assert!(command("git push -q origin feat", "", Some(false)).is_empty());
    assert!(command("git push -q origin feat", "", None).is_empty());
    assert!(
        command(
            "git push origin feat",
            "Everything up-to-date\n",
            Some(true)
        )
        .is_empty()
    );
    assert!(command("git push --delete origin feat", "", Some(true)).is_empty());
}

#[test]
fn gh_pr_subcommands_rank_by_what_they_do() {
    let cases = [
        ("gh pr checkout 12", Relation::CheckedOut),
        ("gh pr merge 12 --squash", Relation::Updated),
        ("gh pr ready 12", Relation::Updated),
        ("gh pr review 12 --approve -b 'ok'", Relation::Reviewed),
        ("gh pr comment 12 --body 'thanks'", Relation::Reviewed),
        ("gh pr view 12 --json title", Relation::Mentioned),
        ("gh pr checks 12", Relation::Mentioned),
    ];
    for (text, relation) in cases {
        assert_eq!(
            of(&command(text, "", Some(true))),
            vec![(Reference::Number(12), relation)],
            "{text}"
        );
    }
    // A failed action named the pull request but did no work on it.
    assert_eq!(
        of(&command(
            "gh pr merge 12",
            "X Pull request is not mergeable",
            Some(false)
        )),
        vec![(Reference::Number(12), Relation::Mentioned)]
    );
    // Listing pull requests names none in particular.
    assert!(command("gh pr list --state open", "12\tFix\tfeat\tOPEN", Some(true)).is_empty());
}

#[test]
fn gh_selectors_resolve_repository_urls_and_branches() {
    assert_eq!(
        of(&command("gh pr view 7 -R up/r", "", Some(true))),
        vec![(key("up", "r", 7), Relation::Mentioned)]
    );
    assert_eq!(
        of(&command(
            "gh --repo=ghe.example.com/up/r pr merge 7",
            "",
            Some(true)
        )),
        vec![(
            Reference::PullRequest(PullRequestKeyOrd {
                repository: RepoKeyOrd {
                    host: "ghe.example.com".into(),
                    port: None,
                    owner: "up".into(),
                    name: "r".into(),
                },
                number: 7,
            }),
            Relation::Updated
        )]
    );
    assert_eq!(
        of(&command(
            "gh pr checkout https://github.com/up/r/pull/8/files",
            "",
            Some(true)
        )),
        vec![(key("up", "r", 8), Relation::CheckedOut)]
    );
    assert_eq!(
        of(&command("gh pr checkout fork-owner:fix-it", "", Some(true))),
        vec![(branch(None, "fix-it"), Relation::CheckedOut)]
    );
    assert_eq!(
        of(&command(
            "cd ../wt && gh pr merge --auto --squash",
            "",
            Some(true)
        )),
        vec![(
            Reference::CurrentBranch {
                directory: Some("../wt".into())
            },
            Relation::Updated
        )]
    );
}

#[test]
fn checkouts_name_branches_but_not_paths_commits_or_the_previous_branch() {
    assert_eq!(
        of(&command("git checkout feat", "", Some(true))),
        vec![(branch(None, "feat"), Relation::CheckedOut)]
    );
    assert_eq!(
        of(&command(
            "git switch -c new-thing origin/main",
            "",
            Some(true)
        )),
        vec![(branch(None, "new-thing"), Relation::CheckedOut)]
    );
    assert_eq!(
        of(&command("git switch --track origin/feat", "", Some(true))),
        vec![(branch(None, "feat"), Relation::CheckedOut)]
    );
    assert_eq!(
        of(&command(
            "git worktree add .worktrees/x feat/x",
            "",
            Some(true)
        )),
        vec![(branch(None, "feat/x"), Relation::CheckedOut)]
    );
    assert_eq!(
        of(&command(
            "git worktree add -b feat/y .worktrees/y origin/main",
            "",
            Some(true)
        )),
        vec![(branch(None, "feat/y"), Relation::CheckedOut)]
    );
    assert!(command("git checkout -- src/app.rs", "", Some(true)).is_empty());
    assert!(command("git checkout 1a2b3c4d", "", Some(true)).is_empty());
    assert!(command("git checkout -", "", Some(true)).is_empty());
}

#[test]
fn compound_commands_quotes_comments_and_redirections_are_split() {
    let signals = command(
        "FOO=1 git fetch origin && git checkout feat 2>/dev/null; echo '#3 && gh pr merge 4' # gh pr merge 5\n",
        "",
        Some(true),
    );
    assert_eq!(
        of(&signals),
        vec![(branch(None, "feat"), Relation::CheckedOut)]
    );
}

#[test]
fn prose_mentions_urls_and_hash_numbers_outside_code() {
    let text = "Opened [the fix](https://github.com/o/r/pull/5#issuecomment-1). \
        It supersedes #4, and up/r#6 too.\n\
        Colour `#333` and \n```\n#7 in code\n```\n\
        Anchor #8a, #0 and issue#9 are not references; (#10) is.";
    assert_eq!(
        of(&prose(text)),
        vec![
            (key("o", "r", 5), Relation::Mentioned),
            (key("up", "r", 6), Relation::Mentioned),
            (Reference::Number(4), Relation::Mentioned),
            (Reference::Number(10), Relation::Mentioned),
        ]
    );
}

#[test]
fn a_hash_number_after_a_multibyte_character_is_read_without_panicking() {
    // A dash, accent or ideograph right before `#` is outside any token.
    let text = "Four PRs are finished (#1084–#1090).\n\
        Café#12, 日本#13 and up/r#14 — #15.";
    assert_eq!(
        of(&prose(text)),
        vec![
            (key("up", "r", 14), Relation::Mentioned),
            (Reference::Number(15), Relation::Mentioned),
            (Reference::Number(1084), Relation::Mentioned),
        ]
    );
}

#[test]
fn non_pull_request_urls_are_not_mentions() {
    let text = "See https://github.com/o/r/issues/5, https://github.com/o/r/pull/new/feat \
        and https://github.com/o/r/pulls.";
    assert!(prose(text).is_empty());
}

#[test]
fn forge_tools_create_update_and_name_pull_requests() {
    assert_eq!(
        of(&tool(
            "mcp__github__create_pull_request",
            r#"{"owner":"o","repo":"r","title":"x","head":"feat","base":"main"}"#,
            r#"{"number":11,"html_url":"https://github.com/o/r/pull/11"}"#,
        )),
        vec![(key("o", "r", 11), Relation::Created)]
    );
    assert_eq!(
        of(&tool(
            "mcp__github__merge_pull_request",
            r#"{"owner":"o","repo":"r","pullNumber":12}"#,
            "merged",
        )),
        vec![(key("o", "r", 12), Relation::Updated)]
    );
    assert_eq!(
        of(&tool(
            "mcp__github__get_pull_request",
            r#"{"owner":"o","repo":"r","pull_number":"13"}"#,
            r#"{"body":"links https://github.com/o/r/pull/99"}"#,
        )),
        // Output of a read names the pull requests it found, not ones the
        // conversation pointed at.
        vec![(key("o", "r", 13), Relation::Mentioned)]
    );
    assert!(
        tool(
            "Read",
            r#"{"file_path":"/x"}"#,
            "#12 https://github.com/o/r/pull/1"
        )
        .is_empty()
    );
}

#[test]
fn a_reference_keeps_its_strongest_relation() {
    let signals = command(
        "gh pr view 5 && gh pr checkout 5 && gh pr merge 5",
        "",
        Some(true),
    );
    // Checking a pull request out ranks above merging it: the issue's order
    // is creation or pushes, then edits or checkouts, then everything else.
    assert_eq!(
        of(&signals),
        vec![(Reference::Number(5), Relation::CheckedOut)]
    );
}

#[test]
fn a_shell_wrapped_command_is_read_inside_its_wrapper() {
    assert_eq!(
        of(&command(
            "/bin/bash -lc 'cd /w && gh pr checkout 21'",
            "",
            Some(true)
        )),
        vec![(Reference::Number(21), Relation::CheckedOut)]
    );
}
