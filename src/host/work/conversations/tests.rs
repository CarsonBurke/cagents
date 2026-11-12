use super::*;
use crate::{
    model::WorktreeId,
    pull_request_signals::Signal,
    work::{PullRequestState, Relation},
};
use std::io::Write as _;

fn repo(owner: &str) -> GitHubRepoKey {
    GitHubRepoKey::new("github.com", None, owner, "repo").unwrap()
}

fn key(owner: &str, number: u64) -> PullRequestKeyOrd {
    PullRequestKeyOrd {
        repository: (&repo(owner)).into(),
        number,
    }
}

fn listed(number: u64, branch: &str, state: &str) -> crate::github::PullRequest {
    serde_json::from_value(serde_json::json!({
        "number": number,
        "title": format!("PR {number}"),
        "state": state,
        "isDraft": false,
        "headRefName": branch,
        "headRefOid": "abc",
        "headRepository": {"name": "repo"},
        "headRepositoryOwner": {"login": "me"},
        "baseRefName": "main",
        "updatedAt": "2026-11-01T00:00:00Z",
        "url": format!("https://github.com/me/repo/pull/{number}"),
    }))
    .unwrap()
}

fn worktree(path: &str, branch: &str) -> super::super::scan::Worktree {
    super::super::scan::Worktree {
        id: WorktreeId::new(),
        path: PathBuf::from(path),
        primary: false,
        locked: false,
        created_at_unix_ms: None,
        branch: Some(branch.to_owned()),
        head: None,
        comparison: None,
        dirty: None,
        link: None,
        error: None,
    }
}

/// A conversation in `/w/app`, whose primary worktree has `main` checked out
/// and whose `.worktrees/feat` worktree has `feat`; the board lists #1 for
/// `feat` and #2, merged, for `old`.
fn facts(directory: &str) -> Facts {
    let mut local = super::super::scan::Local::default();
    local.worktrees = vec![
        worktree("/w/app", "main"),
        worktree("/w/app/.worktrees/feat", "feat"),
    ];
    Facts {
        directory: PathBuf::from(directory),
        project: Some(ProjectFacts {
            snapshot: Some(Arc::new(forge::Snapshot {
                repository: repo("me"),
                remotes: vec![repo("me"), repo("upstream")],
                pull_requests: vec![listed(1, "feat", "OPEN"), listed(2, "old", "MERGED")],
                others: BTreeMap::new(),
            })),
            local: Some(Arc::new(local)),
            forge_error: None,
            synced_at_unix_ms: Some(1_000),
        }),
    }
}

fn watched(signals: &[(Reference, Relation, bool)]) -> Watched {
    let mut watched = Watched::new();
    for (reference, relation, by_subagent) in signals {
        watched.record(transcript::Found {
            signal: Signal {
                reference: reference.clone(),
                relation: *relation,
            },
            by_subagent: *by_subagent,
        });
    }
    watched
}

fn branch(owner: Option<&str>, name: &str) -> Reference {
    Reference::Branch {
        repository: owner.map(|owner| (&repo(owner)).into()),
        name: name.into(),
    }
}

fn resolved(reference: &Reference, facts: &Facts) -> Option<PullRequestKeyOrd> {
    match resolve(reference, facts, &BTreeMap::new()) {
        Resolved::Key(key) => Some(key),
        _ => None,
    }
}

fn rows(list: &ConversationPullRequests) -> Vec<(u64, Relation, bool, bool)> {
    list.pull_requests
        .iter()
        .map(|row| {
            (
                row.number,
                row.relation,
                row.current_branch,
                row.by_subagent,
            )
        })
        .collect()
}

#[test]
fn references_resolve_through_the_project_s_board() {
    let facts = facts("/w/app");
    // A bare number belongs to the conversation's repository.
    assert_eq!(resolved(&Reference::Number(9), &facts), Some(key("me", 9)));
    // A branch the board lists resolves without asking GitHub.
    assert_eq!(
        resolved(&branch(Some("me"), "feat"), &facts),
        Some(key("me", 1))
    );
    assert_eq!(resolved(&branch(None, "old"), &facts), Some(key("me", 2)));
    // An edit in a worktree points at that worktree's branch, even from a
    // relative path through `..`.
    assert_eq!(
        resolved(
            &Reference::CurrentBranch {
                directory: Some("src/../.worktrees/feat/src".into())
            },
            &facts
        ),
        Some(key("me", 1))
    );
    // A branch the board does not list is asked about, in this repository.
    assert!(matches!(
        resolve(&branch(Some("upstream"), "new"), &facts, &BTreeMap::new()),
        Resolved::Branch { base, head, branch }
            if base == repo("me") && head == repo("upstream") && branch == "new"
    ));
}

#[test]
fn a_branch_pushed_outside_the_project_s_remotes_is_not_its_pull_request() {
    let facts = facts("/w/app");
    assert!(matches!(
        resolve(&branch(Some("stranger"), "feat"), &facts, &BTreeMap::new()),
        Resolved::Nothing
    ));
    // Nor is anything without a project to resolve it in.
    let outside = Facts {
        directory: PathBuf::from("/elsewhere"),
        project: None,
    };
    assert!(matches!(
        resolve(&Reference::Number(1), &outside, &BTreeMap::new()),
        Resolved::Nothing
    ));
    // A fork's URL names its own pull request regardless.
    assert_eq!(
        resolved(&Reference::PullRequest(key("stranger", 4)), &outside),
        Some(key("stranger", 4))
    );
}

#[test]
fn a_branch_known_to_have_no_pull_request_is_not_asked_about_again_soon() {
    let facts = facts("/w/app");
    let mut branches = BTreeMap::new();
    branches.insert(
        ((&repo("me")).into(), "lonely".to_owned()),
        BranchLookup {
            number: None,
            checked: Instant::now(),
        },
    );
    assert!(matches!(
        resolve(&branch(None, "lonely"), &facts, &branches),
        Resolved::Nothing
    ));
    branches.insert(
        ((&repo("me")).into(), "found".to_owned()),
        BranchLookup {
            number: Some(30),
            checked: Instant::now(),
        },
    );
    assert!(matches!(
        resolve(&branch(None, "found"), &facts, &branches),
        Resolved::Key(found) if found == key("me", 30)
    ));
}

#[test]
fn the_list_ranks_worked_on_first_and_marks_the_current_branch() {
    let facts = facts("/w/app/.worktrees/feat");
    let watched = watched(&[
        (
            Reference::PullRequest(key("me", 2)),
            Relation::Mentioned,
            false,
        ),
        (
            Reference::PullRequest(key("other", 7)),
            Relation::Mentioned,
            false,
        ),
        (
            Reference::PullRequest(key("me", 2)),
            Relation::Reviewed,
            false,
        ),
        (branch(None, "feat"), Relation::Pushed, false),
    ]);
    let mut observed = BTreeMap::new();
    observed.insert(key("other", 7), observed_now(&listed(7, "x", "OPEN")));
    let list = assemble(&watched, Some(&facts), &observed, &BTreeMap::new(), None);
    assert_eq!(list.repository, Some(repo("me")));
    assert_eq!(
        rows(&list),
        vec![
            // The branch the conversation is on, which it also pushed.
            (1, Relation::Pushed, true, false),
            (2, Relation::Reviewed, false, false),
            (7, Relation::Mentioned, false, false),
        ]
    );
    let first = &list.pull_requests[0];
    assert_eq!(
        first.pull_request.as_ref().map(|found| found.state),
        Some(PullRequestState::Open)
    );
    assert_eq!(list.pull_requests[2].repository, repo("other"));
}

#[test]
fn rows_use_the_repository_s_own_spelling_and_the_board_s_sync_time() {
    let mut facts = facts("/w/app");
    let project = facts.project.as_mut().unwrap();
    let mut snapshot = (**project.snapshot.as_ref().unwrap()).clone();
    snapshot.repository = GitHubRepoKey::new("github.com", None, "Me", "Repo").unwrap();
    project.snapshot = Some(Arc::new(snapshot));
    let watched = watched(&[(
        Reference::PullRequest(key("me", 1)),
        Relation::Mentioned,
        false,
    )]);
    let first = assemble(
        &watched,
        Some(&facts),
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    );
    let row = &first.pull_requests[0];
    // The key is kept in one case, but the row names the repository as the
    // board does, so the panel knows it is the conversation's own.
    assert_eq!(Some(&row.repository), first.repository.as_ref());
    // A listed row is as old as the board's sync, so a later pass with
    // nothing new sends the same list.
    assert_eq!(row.observed_at_unix_ms, Some(1_000));
    let second = assemble(
        &watched,
        Some(&facts),
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    );
    assert_eq!(first.pull_requests, second.pull_requests);
}

#[test]
fn the_current_branch_s_pull_request_is_listed_before_anything_touches_it() {
    let facts = facts("/w/app/.worktrees/feat/src");
    let list = assemble(
        &Watched::new(),
        Some(&facts),
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    );
    assert_eq!(rows(&list), vec![(1, Relation::CheckedOut, true, false)]);
    // On a branch without one, there is nothing.
    let list = assemble(
        &Watched::new(),
        Some(&self::facts("/w/app")),
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    );
    assert!(list.pull_requests.is_empty());
}

#[test]
fn an_old_merged_pull_request_of_the_branch_is_listed_only_once_touched() {
    let mut facts = facts("/w/app");
    let project = facts.project.as_mut().unwrap();
    let mut local = (**project.local.as_ref().unwrap()).clone();
    local.worktrees[0].branch = Some("old".into());
    project.local = Some(Arc::new(local));
    let list = assemble(
        &Watched::new(),
        Some(&facts),
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    );
    assert!(list.pull_requests.is_empty());
    let watched = watched(&[(
        Reference::CurrentBranch { directory: None },
        Relation::Pushed,
        false,
    )]);
    let list = assemble(
        &watched,
        Some(&facts),
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    );
    assert_eq!(rows(&list), vec![(2, Relation::Pushed, true, false)]);
}

#[test]
fn numbers_appear_only_once_github_says_they_are_pull_requests() {
    let facts = facts("/w/app");
    let watched = watched(&[
        (Reference::Number(40), Relation::Mentioned, false),
        (Reference::Number(41), Relation::Mentioned, false),
        (Reference::Number(42), Relation::Mentioned, false),
        (
            Reference::PullRequest(key("me", 43)),
            Relation::Mentioned,
            false,
        ),
    ]);
    let mut observed = BTreeMap::new();
    observed.insert(key("me", 40), observed_now(&listed(40, "x", "OPEN")));
    observed.insert(
        key("me", 41),
        Observed {
            missing: true,
            checked: Some(Instant::now()),
            ..Observed::default()
        },
    );
    let list = assemble(&watched, Some(&facts), &observed, &BTreeMap::new(), None);
    let mut numbers = rows(&list).into_iter().map(|row| row.0).collect::<Vec<_>>();
    numbers.sort();
    // #41 is an issue and #42 is not known yet; a URL names a pull request
    // even before its state is.
    assert_eq!(numbers, vec![40, 43]);
    let unknown = list
        .pull_requests
        .iter()
        .find(|row| row.number == 43)
        .unwrap();
    assert!(unknown.pull_request.is_none());
    assert!(unknown.observed_at_unix_ms.is_none());
}

#[test]
fn a_pull_request_is_by_a_subagent_only_when_every_signal_was() {
    let facts = facts("/w/app");
    let watched = watched(&[
        (
            Reference::PullRequest(key("me", 5)),
            Relation::Created,
            true,
        ),
        (
            Reference::PullRequest(key("me", 6)),
            Relation::Created,
            true,
        ),
        (Reference::Number(6), Relation::Mentioned, false),
        // The same pull request by URL and by its branch is one row.
        (branch(None, "old"), Relation::Pushed, true),
        (
            Reference::PullRequest(key("me", 2)),
            Relation::Mentioned,
            false,
        ),
    ]);
    let list = assemble(
        &watched,
        Some(&facts),
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    );
    let mut found = rows(&list);
    found.sort();
    assert_eq!(
        found,
        vec![
            (2, Relation::Pushed, false, false),
            (5, Relation::Created, false, true),
            (6, Relation::Created, false, false),
        ]
    );
}

#[test]
fn the_list_carries_why_github_could_not_be_asked() {
    let mut facts = facts("/w/app");
    let watched = watched(&[(
        Reference::PullRequest(key("me", 2)),
        Relation::Mentioned,
        false,
    )]);
    let list = assemble(
        &watched,
        Some(&facts),
        &BTreeMap::new(),
        &BTreeMap::new(),
        Some("gh: rate limited".into()),
    );
    assert_eq!(list.forge_error.as_deref(), Some("gh: rate limited"));
    // The board's own poll failing says more about the project than a lookup.
    facts.project.as_mut().unwrap().forge_error = Some("gh is not signed in".into());
    let list = assemble(
        &watched,
        Some(&facts),
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    );
    assert_eq!(list.forge_error.as_deref(), Some("gh is not signed in"));
    // What the board listed is still shown.
    assert!(list.pull_requests[0].pull_request.is_some());
}

#[test]
fn many_mentions_drop_the_oldest_mention_before_any_work() {
    let mut signals = vec![(
        Reference::PullRequest(key("me", 1)),
        Relation::Pushed,
        false,
    )];
    signals.extend((100..100 + REFERENCES as u64 + 5).map(|number| {
        (
            Reference::PullRequest(key("me", number)),
            Relation::Mentioned,
            false,
        )
    }));
    let watched = watched(&signals);
    assert_eq!(watched.references.len(), REFERENCES);
    assert!(
        watched
            .references
            .contains_key(&Reference::PullRequest(key("me", 1)))
    );
    assert!(
        !watched
            .references
            .contains_key(&Reference::PullRequest(key("me", 100)))
    );
}

#[test]
fn failed_lookups_back_off() {
    let mut state = State::default();
    assert!(state.lookups_due());
    let error = GitHubError::NoActiveAccount("github.com".into());
    state.failed(&error);
    assert!(!state.lookups_due());
    assert_eq!(backoff(1), Duration::from_secs(30));
    assert_eq!(backoff(2), Duration::from_secs(60));
    assert_eq!(backoff(40), FINISHED_FOR);
    state.succeeded();
    assert!(state.lookups_due());
    assert!(state.forge_error.is_none());
}

/// A transcript file under `/var/tmp`, removed when dropped.
struct Transcript(PathBuf);

impl Transcript {
    fn new(name: &str) -> Self {
        let directory = Path::new("/var/tmp").join(format!(
            "cagents-conversations-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        Self(directory.join("transcript.jsonl"))
    }

    fn append(&self, text: &str) {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.0)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }
}

impl Drop for Transcript {
    fn drop(&mut self) {
        if let Some(directory) = self.0.parent() {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
}

fn mention(number: u64) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type": "user", "message": {"content":
            format!("See https://github.com/me/repo/pull/{number}")}})
    )
}

fn numbers(watched: &Watched) -> Vec<u64> {
    watched
        .references
        .keys()
        .filter_map(|reference| match reference {
            Reference::PullRequest(key) => Some(key.number),
            _ => None,
        })
        .collect()
}

fn located(path: &Path) -> Source {
    Source::Located(Located {
        format: Format::Claude,
        directory: PathBuf::from("/w/app"),
        transcript: path.to_owned(),
        subagents: Some(path.with_extension("").join("subagents")),
    })
}

#[test]
fn a_transcript_is_read_as_it_grows_one_whole_line_at_a_time() {
    let file = Transcript::new("grows");
    let mut watched = Watched::new();
    watched.source = located(&file.0);
    read(&mut watched);
    assert!(matches!(
        watched.coverage,
        TranscriptCoverage::Unavailable { .. }
    ));

    file.append(&mention(1));
    let second = mention(2);
    let (head, tail) = second.split_at(20);
    file.append(head);
    read(&mut watched);
    // A line still being written is not read yet.
    assert_eq!(numbers(&watched), vec![1]);
    assert_eq!(watched.coverage, TranscriptCoverage::Complete);

    file.append(tail);
    read(&mut watched);
    assert_eq!(numbers(&watched), vec![1, 2]);

    // A subagent's transcript is found beside it.
    let subagents = file.0.with_extension("").join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    std::fs::write(subagents.join("agent-a.jsonl"), mention(3)).unwrap();
    std::fs::write(subagents.join("notes.txt"), mention(4)).unwrap();
    read(&mut watched);
    assert_eq!(numbers(&watched), vec![1, 2, 3]);
    assert!(watched.references[&Reference::PullRequest(key("me", 3))].by_subagent);
}

#[test]
fn a_replaced_or_truncated_transcript_is_read_again_from_its_start() {
    let file = Transcript::new("replaced");
    file.append(&mention(1));
    file.append(&mention(2));
    let mut watched = Watched::new();
    watched.source = located(&file.0);
    read(&mut watched);
    assert_eq!(numbers(&watched), vec![1, 2]);

    // Rewritten shorter in place.
    std::fs::write(&file.0, mention(5)).unwrap();
    read(&mut watched);
    assert_eq!(numbers(&watched), vec![1, 2, 5]);

    // Replaced by a new file of the same length.
    let replacement = file.0.with_extension("new");
    std::fs::write(&replacement, mention(6)).unwrap();
    std::fs::rename(&replacement, &file.0).unwrap();
    read(&mut watched);
    assert_eq!(numbers(&watched), vec![1, 2, 5, 6]);
}

#[test]
fn a_long_transcript_is_read_from_its_newest_part_and_says_so() {
    let file = Transcript::new("long");
    // One line of padding longer than the first read, then a mention.
    let padding = format!(
        "{}\n",
        serde_json::json!({"type": "user", "message": {"content":
            format!("https://github.com/me/repo/pull/9 {}", "x".repeat(FIRST_READ as usize))}})
    );
    file.append(&padding);
    file.append(&mention(7));
    let mut watched = Watched::new();
    watched.source = located(&file.0);
    // The backlog is read a pass at a time.
    for _ in 0..(FIRST_READ / READ_PER_PASS + 2) {
        read(&mut watched);
    }
    // The cut-off line is skipped rather than read from its middle.
    assert_eq!(numbers(&watched), vec![7]);
    assert_eq!(watched.coverage, TranscriptCoverage::Partial);
}

#[test]
fn an_over_long_line_is_skipped_without_making_the_list_partial() {
    let file = Transcript::new("image");
    // Longer than a line may be, such as an attached image; it spans reads
    // and also fits in one, and either way is skipped whole.
    for length in [MAX_LINE + 1, READ_PER_PASS as usize + 1] {
        file.append(&format!(
            "{}\n",
            serde_json::json!({"type": "user", "message": {"content":
                format!("https://github.com/me/repo/pull/9 {}", "x".repeat(length))}})
        ));
    }
    file.append(&mention(8));
    let mut watched = Watched::new();
    watched.source = located(&file.0);
    for _ in 0..4 {
        read(&mut watched);
    }
    assert_eq!(numbers(&watched), vec![8]);
    assert_eq!(watched.coverage, TranscriptCoverage::Complete);
}

#[test]
fn a_codex_search_that_finds_nothing_waits_longer_each_time() {
    assert_eq!(search_backoff(1), Duration::from_secs(5));
    assert_eq!(search_backoff(2), Duration::from_secs(10));
    assert_eq!(search_backoff(5), Duration::from_secs(80));
    assert_eq!(search_backoff(6), Duration::from_secs(120));
    assert_eq!(search_backoff(u32::MAX), Duration::from_secs(120));
}

#[test]
fn work_on_the_current_branch_stays_with_that_branch_after_a_switch() {
    let mut watched = Watched::new();
    let push = |directory: Option<&str>| transcript::Found {
        signal: Signal {
            reference: Reference::CurrentBranch {
                directory: directory.map(str::to_owned),
            },
            relation: Relation::Pushed,
        },
        by_subagent: false,
    };
    watched.apply(
        Scanned {
            found: vec![push(None), push(Some(".worktrees/feat"))],
            coverage: TranscriptCoverage::Complete,
        },
        Some(&facts("/w/app")),
    );
    assert_eq!(
        watched.references.keys().cloned().collect::<Vec<_>>(),
        vec![branch(None, "feat"), branch(None, "main")]
    );
}

#[test]
fn edited_worktrees_are_listed_most_recent_first_and_outside_ones_dropped() {
    let mut watched = Watched::new();
    let edit = |directory: &str, by_subagent: bool| transcript::Found {
        signal: Signal {
            reference: Reference::CurrentBranch {
                directory: Some(directory.to_owned()),
            },
            relation: Relation::Edited,
        },
        by_subagent,
    };
    watched.apply(
        Scanned {
            found: vec![
                edit("/w/app/src", false),
                edit("/w/app/.worktrees/feat/src/deep", true),
                edit("/elsewhere/project", false),
                edit(".worktrees/feat", true),
            ],
            coverage: TranscriptCoverage::Complete,
        },
        Some(&facts("/w/app")),
    );
    let facts = facts("/w/app");
    let list = assemble(
        &watched,
        Some(&facts),
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    );
    assert_eq!(
        list.worktrees
            .iter()
            .map(|worktree| (
                worktree.path.to_str().unwrap(),
                worktree.branch.as_deref(),
                worktree.by_subagent
            ))
            .collect::<Vec<_>>(),
        vec![
            ("/w/app/.worktrees/feat", Some("feat"), true),
            ("/w/app", Some("main"), false),
        ]
    );
    // The feat branch heads #1, which editing it worked on, so the row shows
    // the worktree; main heads nothing.
    assert!(
        list.pull_requests
            .iter()
            .any(|row| row.number == 1 && row.relation == Relation::Edited)
    );
    assert_eq!(
        list.worktrees
            .iter()
            .map(|worktree| worktree.pull_request)
            .collect::<Vec<_>>(),
        vec![Some(1), None]
    );
    // Editing a directory again is the newest edit, by the conversation
    // itself this time.
    watched.apply(
        Scanned {
            found: vec![edit("/w/app", false), edit("/w/app/.worktrees/feat", false)],
            coverage: TranscriptCoverage::Complete,
        },
        Some(&facts),
    );
    let list = assemble(
        &watched,
        Some(&facts),
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    );
    assert_eq!(
        list.worktrees
            .iter()
            .map(|worktree| (worktree.path.to_str().unwrap(), worktree.by_subagent))
            .collect::<Vec<_>>(),
        vec![("/w/app/.worktrees/feat", false), ("/w/app", false)]
    );
}

#[test]
fn new_work_on_a_merged_pull_request_s_branch_name_is_not_an_edit_of_it() {
    // `old` headed #2, merged at commit `abc`; a worktree has `old` checked
    // out again.
    let facts_at = |oid: &str| {
        let mut facts = facts("/w/app");
        let project = facts.project.as_mut().unwrap();
        let mut local = (**project.local.as_ref().unwrap()).clone();
        let mut reused = worktree("/w/app/.worktrees/old", "old");
        reused.head = Some(crate::work::Head {
            oid: oid.to_owned(),
            committed_at_unix_ms: None,
        });
        local.worktrees.push(reused);
        project.local = Some(Arc::new(local));
        facts
    };
    let edited = |facts: &Facts| {
        let mut watched = Watched::new();
        watched.apply(
            Scanned {
                found: vec![transcript::Found {
                    signal: Signal {
                        reference: Reference::CurrentBranch {
                            directory: Some(".worktrees/old/src".to_owned()),
                        },
                        relation: Relation::Edited,
                    },
                    by_subagent: false,
                }],
                coverage: TranscriptCoverage::Complete,
            },
            Some(facts),
        );
        assemble(
            &watched,
            Some(facts),
            &BTreeMap::new(),
            &BTreeMap::new(),
            None,
        )
    };

    // Past the merge, the edit is new work: no #2 row, and the worktree is
    // listed on its own.
    let list = edited(&facts_at("def"));
    assert!(list.pull_requests.iter().all(|row| row.number != 2));
    assert_eq!(
        list.worktrees
            .iter()
            .map(|worktree| (worktree.path.to_str().unwrap(), worktree.pull_request))
            .collect::<Vec<_>>(),
        vec![("/w/app/.worktrees/old", None)]
    );

    // Still at the merged commit, it is #2's branch.
    let list = edited(&facts_at("abc"));
    assert!(
        list.pull_requests
            .iter()
            .any(|row| row.number == 2 && row.relation == Relation::Edited)
    );
    assert_eq!(list.worktrees[0].pull_request, Some(2));
}

#[test]
fn edited_directories_are_bounded_by_recency() {
    let mut watched = Watched::new();
    for index in 0..EDITED_DIRECTORIES + 8 {
        watched.edited_in(PathBuf::from(format!("/w/app/{index}")), false);
    }
    assert_eq!(watched.edited.len(), EDITED_DIRECTORIES);
    assert!(!watched.edited.contains_key(Path::new("/w/app/0")));
    assert!(
        watched
            .edited
            .contains_key(&PathBuf::from(format!("/w/app/{}", EDITED_DIRECTORIES + 7)))
    );
}

#[test]
fn a_pull_request_row_carries_its_changes_and_every_check_as_it_ended() {
    let mut found = listed(9, "feat", "OPEN");
    found.additions = Some(12);
    found.deletions = Some(3);
    found.status_check_rollup = serde_json::from_value(serde_json::json!([
        {"__typename": "CheckRun", "name": "test", "workflowName": "CI",
         "status": "COMPLETED", "conclusion": "SUCCESS",
         "detailsUrl": "https://github.com/me/repo/runs/1"},
        {"__typename": "CheckRun", "name": "docs", "workflowName": "CI",
         "status": "COMPLETED", "conclusion": "SKIPPED"},
        {"__typename": "CheckRun", "name": "deploy", "workflowName": "Release",
         "status": "COMPLETED", "conclusion": "CANCELLED"},
        {"__typename": "CheckRun", "name": "lint", "workflowName": "CI",
         "status": "IN_PROGRESS"},
        {"__typename": "StatusContext", "context": "ci/legacy", "state": "FAILURE",
         "targetUrl": "https://ci.invalid/1"},
    ]))
    .unwrap();
    let detail = Detail::of(&found);
    assert_eq!(
        detail.diff,
        Some(DiffStat {
            additions: 12,
            deletions: 3
        })
    );
    assert_eq!(
        detail
            .checks
            .iter()
            .map(|check| (check.state, check.workflow.as_str(), check.name.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (CheckState::Failed, "", "ci/legacy"),
            (CheckState::Pending, "CI", "lint"),
            (CheckState::Cancelled, "Release", "deploy"),
            (CheckState::Passed, "CI", "test"),
            (CheckState::Skipped, "CI", "docs"),
        ]
    );
    // A check without its own page links to the pull request.
    assert_eq!(detail.checks[1].url, "https://github.com/me/repo/pull/9");
}

#[test]
fn an_edited_worktree_counts_its_changed_paths_and_takes_its_measured_lines() {
    let mut facts = facts("/w/app");
    let project = facts.project.as_mut().unwrap();
    let mut local = (**project.local.as_ref().unwrap()).clone();
    local.worktrees[0].dirty = Some(crate::work::Dirty {
        paths: 3,
        conflicted: 0,
        newest_modified_at_unix_ms: Some(5),
    });
    project.local = Some(Arc::new(local));
    let mut watched = Watched::new();
    watched.edited_in(PathBuf::from("/w/app/src"), false);
    watched.edited_in(PathBuf::from("/w/app/.worktrees/feat"), false);
    let mut list = assemble(
        &watched,
        Some(&facts),
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
    );
    let counted = |list: &ConversationPullRequests| {
        list.worktrees
            .iter()
            .map(|worktree| {
                (
                    worktree.path.to_str().unwrap().to_owned(),
                    (worktree.changed_paths, worktree.diff),
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    // The scan has not read the linked worktree's status yet.
    assert_eq!(
        counted(&list),
        BTreeMap::from([
            ("/w/app".to_owned(), (Some(3), None)),
            ("/w/app/.worktrees/feat".to_owned(), (None, None)),
        ])
    );
    let project = facts.project.as_ref().unwrap();
    let main = &project.local.as_ref().unwrap().worktrees[0];
    let measured = BTreeMap::from([(
        PathBuf::from("/w/app"),
        Measured {
            fingerprint: Fingerprint::of(main),
            diff: Some(DiffStat {
                additions: 7,
                deletions: 2,
            }),
        },
    )]);
    with_measured(&mut list, &measured);
    assert_eq!(
        counted(&list)["/w/app"],
        (
            Some(3),
            Some(DiffStat {
                additions: 7,
                deletions: 2
            })
        )
    );
    // Any change to its status, such as another edit's time, is a new
    // fingerprint and so is measured again.
    let mut edited = main.clone();
    edited.dirty.as_mut().unwrap().newest_modified_at_unix_ms = Some(6);
    assert_ne!(Fingerprint::of(&edited), Fingerprint::of(main));
}
