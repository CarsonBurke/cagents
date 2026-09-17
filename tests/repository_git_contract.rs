#![cfg(feature = "ui")]

//! Observable Git behaviour of `RepositoryService` against the real `git`.
//!
//! Every assertion here is made against a temporary worktree and a local bare
//! remote created by this test, and is cross-checked with plain `git` where the
//! service claims a repository-visible effect. Nothing reads the developer's
//! configuration: raw setup commands and every command the service runs are
//! given `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` pointing at `/dev/null` plus
//! an explicit identity, so a local `diff.renames`, `commit.gpgsign`, or
//! `core.hooksPath` cannot change the outcome.

use std::{
    ffi::{OsStr, OsString},
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use cagents::repository::{
    CancellationToken, ChangeArea, ChangeKind, CommandSpec, CommitOutcome, CommitRequest,
    ConflictKind, DiffBasis, DiffContent, DiffRequest, DiffSide, DiscardRequest, HeadState,
    HistoryRequest, HistoryRevision, MutationFailure, MutationLocks, MutationOutcome, Oid,
    PathSelection, ProcessExecutor, ProcessFailure, ProcessFailureKind, ProcessOutput, PushOutcome,
    PushRequest, PushTarget, RemoteSideRef, RepoPath, RepositoryService, RepositorySnapshot,
    StageRequest, SystemProcessExecutor, TrackingState, UnstageRequest, ValidatedBranchName,
    ValidatedRefName,
};

/// Applies configuration isolation and a fixed identity to every command,
/// including the ones the service builds itself.
struct IsolatedExecutor(SystemProcessExecutor);

impl ProcessExecutor for IsolatedExecutor {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        self.0.execute(&isolate(command.clone()), cancellation)
    }
}

/// Runs every command for real, but reports the push itself as a launched
/// failure — exactly the state a cancelled or killed `git push` leaves behind,
/// where the remote may already have been updated.
struct AmbiguousPushExecutor(SystemProcessExecutor);

impl ProcessExecutor for AmbiguousPushExecutor {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        let output = self.0.execute(&isolate(command.clone()), cancellation)?;
        if command.arguments.iter().any(|argument| argument == "push") {
            return Err(ProcessFailure {
                kind: ProcessFailureKind::Cancelled,
                program: command.program.clone(),
                exit_code: None,
                signal: None,
                stdout: output.stdout,
                stderr: output.stderr,
                message: "cancelled after launch".into(),
                started: true,
            });
        }
        Ok(output)
    }
}

fn isolate(command: CommandSpec) -> CommandSpec {
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Cagents Contract")
        .env("GIT_AUTHOR_EMAIL", "contract@example.invalid")
        .env("GIT_COMMITTER_NAME", "Cagents Contract")
        .env("GIT_COMMITTER_EMAIL", "contract@example.invalid")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
}

struct Fixture {
    root: tempfile::TempDir,
    git: PathBuf,
}

impl Fixture {
    /// Returns `None` when `git` is not installed, so the suite skips instead
    /// of failing on a machine without it.
    fn new() -> Option<Self> {
        let git = PathBuf::from("git");
        let mut probe = Command::new(&git);
        probe.arg("--version");
        if probe.output().is_err() {
            eprintln!("skipping: `git` is not available on PATH");
            return None;
        }
        let root = tempfile::tempdir().expect("temporary directory");
        let fixture = Self { root, git };
        fixture.run(
            fixture.root.path(),
            &["init", "--bare", "-b", "main", "origin.git"],
        );
        fixture.run(fixture.root.path(), &["init", "-b", "main", "work"]);
        // Repository-local identity as well as the environment override, so a
        // hook or a subprocess that ignores the environment still commits.
        fixture.run(
            &fixture.work(),
            &["config", "user.name", "Cagents Contract"],
        );
        fixture.run(
            &fixture.work(),
            &["config", "user.email", "contract@example.invalid"],
        );
        fixture.run(&fixture.work(), &["config", "commit.gpgsign", "false"]);
        fixture.run(&fixture.work(), &["config", "diff.renames", "true"]);
        Some(fixture)
    }

    fn work(&self) -> PathBuf {
        self.root.path().join("work")
    }

    fn origin(&self) -> PathBuf {
        self.root.path().join("origin.git")
    }

    fn service(&self) -> RepositoryService<IsolatedExecutor> {
        RepositoryService::new(&self.git, Arc::new(IsolatedExecutor(SystemProcessExecutor)))
    }

    /// Runs a raw `git` command and requires success, so a broken fixture fails
    /// where it was set up rather than inside an assertion.
    ///
    /// Arguments are `OsStr` rather than `str` because a ref name is an
    /// arbitrary byte string and one of these tests has to hand `git` a branch
    /// name that no encoding decodes.
    fn run<S: AsRef<OsStr> + fmt::Debug>(&self, cwd: &Path, arguments: &[S]) -> Vec<u8> {
        let mut command = Command::new(&self.git);
        command.current_dir(cwd).args(arguments);
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Cagents Contract")
            .env("GIT_AUTHOR_EMAIL", "contract@example.invalid")
            .env("GIT_COMMITTER_NAME", "Cagents Contract")
            .env("GIT_COMMITTER_EMAIL", "contract@example.invalid")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        let output = command.output().expect("git runs");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn git_in_work<S: AsRef<OsStr> + fmt::Debug>(&self, arguments: &[S]) -> Vec<u8> {
        self.run(&self.work(), arguments)
    }

    /// Runs a raw `git` command that is expected to fail, and returns its
    /// stderr so a test can pin the reason Git gave.
    fn run_failing(&self, cwd: &Path, arguments: &[&str]) -> String {
        self.run_failing_status(cwd, arguments).1
    }

    /// The same, keeping the exit status: Git separates a refusal it made
    /// before writing anything, which is an `error()` and exit 1, from a
    /// failure raised inside a write loop, which reaches `exit` as 255. A test
    /// that pins only the wording cannot tell those apart.
    fn run_failing_status(&self, cwd: &Path, arguments: &[&str]) -> (Option<i32>, String) {
        let mut command = Command::new(&self.git);
        command.current_dir(cwd).args(arguments);
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        let output = command.output().expect("git runs");
        assert!(
            !output.status.success(),
            "git {arguments:?} was expected to fail"
        );
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn text(&self, arguments: &[&str]) -> String {
        String::from_utf8(self.git_in_work(arguments))
            .expect("UTF-8 git output")
            .trim_end()
            .to_owned()
    }

    fn write(&self, relative: &str, contents: &[u8]) {
        let path = self.work().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory");
        }
        fs::write(path, contents).expect("write fixture file");
    }

    fn commit(&self, message: &str) -> Oid {
        self.git_in_work(&["commit", "--quiet", "-m", message]);
        self.head()
    }

    fn head(&self) -> Oid {
        Oid::parse(self.text(&["rev-parse", "HEAD"])).expect("full object id")
    }

    fn porcelain(&self) -> String {
        self.text(&["status", "--porcelain"])
    }

    fn snapshot(&self) -> RepositorySnapshot {
        self.service()
            .snapshot(&self.work(), &CancellationToken::new())
            .expect("snapshot")
    }
}

fn path(value: &str) -> RepoPath {
    RepoPath::parse(value).expect("valid repository path")
}

fn entry<'snapshot>(
    snapshot: &'snapshot RepositorySnapshot,
    area: ChangeArea,
    value: &str,
) -> &'snapshot cagents::repository::ChangedPath {
    let wanted = path(value);
    snapshot
        .status
        .entries
        .iter()
        .find(|entry| entry.area == area && entry.path == wanted)
        .unwrap_or_else(|| {
            panic!(
                "no {area:?} entry for {value}; entries were {:?}",
                snapshot
                    .status
                    .entries
                    .iter()
                    .map(|entry| (entry.area, entry.path.display().into_owned(), entry.kind))
                    .collect::<Vec<_>>()
            )
        })
}

fn text_side(side: &DiffSide) -> &str {
    side.text()
        .unwrap_or_else(|| panic!("expected text content, got {side:?}"))
}

#[test]
fn simultaneous_staged_and_unstaged_state_is_reported_per_area() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("src/main.rs", b"one\ntwo\n");
    fixture.write("delete.txt", b"remove me\n");
    fixture.write("old-name.txt", b"rename me\n");
    // Filenames that would be read as pathspec magic without
    // `--literal-pathspecs`.
    fixture.write(":colon.txt", b"colon\n");
    fixture.write(":(top)magic.txt", b"magic\n");
    fixture.write("art/logo.bin", &[0, 1, 2, 3, 0, 4]);
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");

    fixture.git_in_work(&["mv", "old-name.txt", "new-name.txt"]);
    fixture.write("src/main.rs", b"one\ntwo\nthree\n");
    fixture.git_in_work(&["add", "src/main.rs"]);
    fixture.write("src/main.rs", b"one\ntwo\nthree\nfour\n");
    fixture.write("art/logo.bin", &[0, 9, 9, 9]);
    fixture.git_in_work(&["add", "art/logo.bin"]);
    fs::remove_file(fixture.work().join("delete.txt")).expect("remove tracked file");
    fixture.write("nested/untracked.txt", b"untracked\n");

    let snapshot = fixture.snapshot();

    // One path modified in the index and again in the worktree is two separate
    // pieces of work, so it is two entries.
    let staged = entry(&snapshot, ChangeArea::Staged, "src/main.rs");
    assert_eq!(staged.kind, ChangeKind::Modified);
    assert_eq!(staged.added_lines, Some(1));
    assert_eq!(staged.removed_lines, Some(0));
    let unstaged = entry(&snapshot, ChangeArea::Unstaged, "src/main.rs");
    assert_eq!(unstaged.kind, ChangeKind::Modified);
    assert_eq!(unstaged.added_lines, Some(1));

    let renamed = entry(&snapshot, ChangeArea::Staged, "new-name.txt");
    assert_eq!(renamed.kind, ChangeKind::Renamed);
    assert_eq!(renamed.old_path, Some(path("old-name.txt")));

    let deleted = entry(&snapshot, ChangeArea::Unstaged, "delete.txt");
    assert_eq!(deleted.kind, ChangeKind::Deleted);

    // Binary content has no line counts, and `-` must never become zero.
    let binary = entry(&snapshot, ChangeArea::Staged, "art/logo.bin");
    assert_eq!(binary.added_lines, None);
    assert_eq!(binary.removed_lines, None);

    // `--untracked-files=all` reports the file, not just its directory.
    let untracked = entry(&snapshot, ChangeArea::Untracked, "nested/untracked.txt");
    assert_eq!(untracked.kind, ChangeKind::Untracked);
    assert_eq!(untracked.added_lines, Some(1));
    assert_eq!(untracked.removed_lines, Some(0));

    assert_eq!(snapshot.status.dirty.staged, 3);
    assert!(snapshot.status.dirty.is_dirty());
    assert_eq!(snapshot.status.branch(), Some("main"));

    // Pathspec-looking names survive as ordinary tracked files.
    assert!(snapshot.files.paths.contains(&path(":colon.txt")));
    assert!(snapshot.files.paths.contains(&path(":(top)magic.txt")));
    assert!(snapshot.files.paths.contains(&path("nested/untracked.txt")));

    let tree = cagents::repository::build_file_tree(&snapshot.files.paths);
    assert!(
        tree.iter()
            .any(|row| row.directory && row.path == path("src")),
        "the file tree must synthesize directory rows"
    );
}

#[test]
fn merge_conflicts_expose_index_stages_and_conflict_kinds() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("content.txt", b"base\n");
    fixture.write("deleted.txt", b"base\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("base");

    fixture.git_in_work(&["checkout", "--quiet", "-b", "theirs"]);
    fixture.write("content.txt", b"theirs\n");
    fs::remove_file(fixture.work().join("deleted.txt")).expect("remove file");
    fixture.write("added.txt", b"theirs added\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("theirs");

    fixture.git_in_work(&["checkout", "--quiet", "main"]);
    fixture.write("content.txt", b"ours\n");
    fixture.write("deleted.txt", b"ours modified\n");
    fixture.write("added.txt", b"ours added\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("ours");

    let merge = Command::new(&fixture.git)
        .current_dir(fixture.work())
        .args(["merge", "--no-edit", "theirs"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git merge runs");
    assert!(!merge.status.success(), "the merge must conflict");

    let snapshot = fixture.snapshot();
    assert_eq!(
        entry(&snapshot, ChangeArea::Conflict, "content.txt").conflict,
        Some(ConflictKind::BothModified)
    );
    assert_eq!(
        entry(&snapshot, ChangeArea::Conflict, "added.txt").conflict,
        Some(ConflictKind::BothAdded)
    );
    assert_eq!(
        entry(&snapshot, ChangeArea::Conflict, "deleted.txt").conflict,
        Some(ConflictKind::DeletedByThem)
    );
    // A conflicted path has no measured line counts.
    assert_eq!(
        entry(&snapshot, ChangeArea::Conflict, "content.txt").added_lines,
        None
    );
    assert_eq!(snapshot.status.dirty.conflicted, 3);

    let service = fixture.service();
    let diff_of = |name: &str| {
        service
            .diff(
                &DiffRequest {
                    worktree: fixture.work(),
                    basis: DiffBasis::IndexToWorktree,
                    path: path(name),
                    old_path: None,
                },
                &CancellationToken::new(),
            )
            .expect("diff")
    };

    match diff_of("content.txt") {
        DiffContent::Unmerged {
            base,
            ours,
            theirs,
            worktree,
        } => {
            assert_eq!(text_side(&base), "base\n");
            assert_eq!(text_side(&ours), "ours\n");
            assert_eq!(text_side(&theirs), "theirs\n");
            // The worktree still holds Git's conflict markers.
            assert!(text_side(&worktree).contains("<<<<<<<"));
        }
        other => panic!("expected an unmerged comparison, got {other:?}"),
    }

    match diff_of("added.txt") {
        DiffContent::Unmerged {
            base, ours, theirs, ..
        } => {
            // Add/add has no common ancestor stage, but the two sides exist.
            assert_eq!(base, DiffSide::Absent);
            assert_eq!(text_side(&ours), "ours added\n");
            assert_eq!(text_side(&theirs), "theirs added\n");
        }
        other => panic!("expected an unmerged comparison, got {other:?}"),
    }

    match diff_of("deleted.txt") {
        DiffContent::Unmerged {
            base, ours, theirs, ..
        } => {
            // Modify/delete keeps stages 1 and 2; the missing stage 3 must not
            // turn the path into an added file.
            assert_eq!(text_side(&base), "base\n");
            assert_eq!(text_side(&ours), "ours modified\n");
            assert_eq!(theirs, DiffSide::Absent);
        }
        other => panic!("expected an unmerged comparison, got {other:?}"),
    }
}

#[test]
fn every_diff_basis_reads_the_matching_pair_of_sides() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    let root = fixture.commit("root");

    fixture.git_in_work(&["checkout", "--quiet", "-b", "feature"]);
    fixture.write("a.txt", b"two\n");
    fixture.git_in_work(&["add", "-A"]);
    let feature = fixture.commit("feature");

    fixture.write("a.txt", b"three\n");
    fixture.git_in_work(&["add", "a.txt"]);
    fixture.write("a.txt", b"four\n");

    let service = fixture.service();
    let sides = |basis: DiffBasis| {
        let content = service
            .diff(
                &DiffRequest {
                    worktree: fixture.work(),
                    basis,
                    path: path("a.txt"),
                    old_path: None,
                },
                &CancellationToken::new(),
            )
            .expect("diff");
        match content {
            DiffContent::Comparison {
                old,
                new,
                old_label,
                new_label,
            } => (old, new, old_label, new_label),
            other => panic!("expected a comparison, got {other:?}"),
        }
    };

    let (old, new, old_label, new_label) = sides(DiffBasis::IndexToWorktree);
    assert_eq!(text_side(&old), "three\n");
    assert_eq!(text_side(&new), "four\n");
    assert_eq!(
        (old_label.as_str(), new_label.as_str()),
        ("Index", "Working tree")
    );

    let (old, new, old_label, new_label) = sides(DiffBasis::HeadToIndex);
    assert_eq!(text_side(&old), "two\n");
    assert_eq!(text_side(&new), "three\n");
    assert_eq!((old_label.as_str(), new_label.as_str()), ("HEAD", "Index"));

    let (old, new, ..) = sides(DiffBasis::MergeBaseToHead {
        base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
        head: feature.clone(),
        merge_base: root.clone(),
    });
    // The merge base is the root commit, not the tip of `main`.
    assert_eq!(text_side(&old), "one\n");
    assert_eq!(text_side(&new), "two\n");

    let (old, new, ..) = sides(DiffBasis::CommitToParent {
        oid: feature.clone(),
        parent: Some(root.clone()),
    });
    assert_eq!(text_side(&old), "one\n");
    assert_eq!(text_side(&new), "two\n");

    let (old, new, old_label, _) = sides(DiffBasis::CommitToParent {
        oid: root.clone(),
        parent: None,
    });
    // A root commit adds every file, so the old side is absent rather than
    // empty text.
    assert_eq!(old, DiffSide::Absent);
    assert_eq!(text_side(&new), "one\n");
    assert_eq!(old_label, "root commit");

    let base = service
        .resolve_compare_base(
            &fixture.work(),
            &ValidatedRefName::parse("refs/heads/main").unwrap(),
            &CancellationToken::new(),
        )
        .expect("merge base");
    assert_eq!(base, root);

    let branch_entries = service
        .branch_diff_entries(
            &fixture.work(),
            &ValidatedRefName::parse("refs/heads/main").unwrap(),
            &CancellationToken::new(),
        )
        .expect("branch entries");
    assert_eq!(branch_entries.len(), 1);
    assert_eq!(branch_entries[0].path, path("a.txt"));
    assert_eq!(branch_entries[0].kind, ChangeKind::Modified);
    assert_eq!(branch_entries[0].added_lines, Some(1));
    assert_eq!(branch_entries[0].removed_lines, Some(1));

    let root_entries = service
        .commit_diff_entries(&fixture.work(), &root, None, &CancellationToken::new())
        .expect("root entries");
    assert_eq!(root_entries.len(), 1);
    assert_eq!(root_entries[0].kind, ChangeKind::Added);
}

#[test]
fn history_reports_root_and_multi_parent_commits() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    let root = fixture.commit("root subject");

    fixture.git_in_work(&["checkout", "--quiet", "-b", "side"]);
    fixture.write("side.txt", b"side\n");
    fixture.git_in_work(&["add", "-A"]);
    let side = fixture.commit("side subject");

    fixture.git_in_work(&["checkout", "--quiet", "main"]);
    fixture.write("main.txt", b"main\n");
    fixture.git_in_work(&["add", "-A"]);
    let mainline = fixture.commit("main subject\n\nmain body\n");
    fixture.git_in_work(&[
        "merge",
        "--no-ff",
        "--no-edit",
        "-m",
        "merge subject",
        "side",
    ]);
    let merge = fixture.head();

    let service = fixture.service();
    let commits = service
        .history(
            &HistoryRequest {
                worktree: fixture.work(),
                revision: HistoryRevision::Head,
                page: 0,
                path: None,
            },
            &CancellationToken::new(),
        )
        .expect("history");

    assert_eq!(commits.len(), 4);
    assert_eq!(commits[0].oid, merge);
    assert_eq!(commits[0].parents, vec![mainline.clone(), side.clone()]);
    assert_eq!(commits[0].subject, "merge subject");
    let root_commit = commits.last().expect("root commit");
    assert_eq!(root_commit.oid, root);
    assert!(
        root_commit.parents.is_empty(),
        "a root commit has no parents"
    );
    assert_eq!(root_commit.author.name, "Cagents Contract");
    assert_eq!(root_commit.author.email, "contract@example.invalid");
    assert_eq!(root_commit.author.timestamp, 1_700_000_000);
    assert_eq!(root_commit.author.offset_minutes, 0);

    let with_body = commits
        .iter()
        .find(|commit| commit.oid == mainline)
        .expect("mainline commit");
    assert_eq!(with_body.subject, "main subject");
    assert_eq!(with_body.body, "main body");

    // Paging past the end is empty rather than an error.
    let second_page = service
        .history(
            &HistoryRequest {
                worktree: fixture.work(),
                revision: HistoryRevision::Head,
                page: 1,
                path: None,
            },
            &CancellationToken::new(),
        )
        .expect("second page");
    assert!(second_page.is_empty());

    // A path-restricted walk only sees commits touching that path.
    let for_path = service
        .history(
            &HistoryRequest {
                worktree: fixture.work(),
                revision: HistoryRevision::Reference {
                    name: ValidatedRefName::parse("refs/heads/side").unwrap(),
                },
                page: 0,
                path: Some(path("side.txt")),
            },
            &CancellationToken::new(),
        )
        .expect("path history");
    assert_eq!(for_path.len(), 1);
    assert_eq!(for_path[0].oid, side);
}

#[test]
fn history_of_an_unborn_branch_is_empty() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let commits = fixture
        .service()
        .history(
            &HistoryRequest {
                worktree: fixture.work(),
                revision: HistoryRevision::Head,
                page: 0,
                path: None,
            },
            &CancellationToken::new(),
        )
        .expect("history of an unborn branch");
    assert!(commits.is_empty());

    // Unstaging on an unborn branch removes the index entry instead of trying
    // to restore from a HEAD that does not exist.
    fixture.write("first.txt", b"first\n");
    fixture.git_in_work(&["add", "-A"]);
    let outcome = fixture
        .service()
        .unstage(
            &UnstageRequest {
                worktree: fixture.work(),
                selection: vec![
                    PathSelection::new(path("first.txt")),
                    PathSelection::new(path("never-existed.txt")),
                ],
            },
            &CancellationToken::new(),
        )
        .expect("unstage");
    assert_eq!(
        outcome,
        MutationOutcome::Applied {
            paths: vec![path("first.txt")]
        }
    );
    assert_eq!(fixture.porcelain(), "?? first.txt");
}

/// A brand-new repository has no HEAD, and the first commit is the path every
/// new project takes: the snapshot, the staged-change check, and the commit
/// itself all have to work against the empty tree.
#[test]
fn the_first_commit_on_an_unborn_branch_is_created() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("first.txt", b"first\n");
    let service = fixture.service();
    let outcome = service
        .stage(
            &StageRequest {
                worktree: fixture.work(),
                selection: vec![PathSelection::new(path("first.txt"))],
            },
            &CancellationToken::new(),
        )
        .expect("stage");
    assert!(matches!(outcome, MutationOutcome::Applied { .. }));

    let snapshot = fixture.snapshot();
    assert_eq!(
        snapshot.status.head,
        HeadState::Unborn {
            branch: Some("main".into())
        }
    );
    let staged = entry(&snapshot, ChangeArea::Staged, "first.txt");
    assert_eq!(staged.kind, ChangeKind::Added);
    assert_eq!(staged.added_lines, Some(1));
    assert!(
        service
            .has_staged_changes(&fixture.work(), &CancellationToken::new())
            .expect("staged check")
    );

    let outcome = service
        .commit(
            &CommitRequest {
                worktree: fixture.work(),
                message: "first commit".into(),
            },
            &CancellationToken::new(),
        )
        .expect("commit");
    match outcome {
        CommitOutcome::Created { oid, .. } => assert_eq!(oid, fixture.head()),
        other => panic!("expected a created commit, got {other:?}"),
    }
    assert_eq!(fixture.text(&["log", "-1", "--format=%s"]), "first commit");
    assert!(fixture.porcelain().is_empty());
}

#[test]
fn a_complete_rename_can_be_staged_unstaged_and_discarded() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("old-name.txt", b"rename me\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");
    fixture.git_in_work(&["mv", "old-name.txt", "new-name.txt"]);

    let service = fixture.service();
    let snapshot = fixture.snapshot();
    let rename = entry(&snapshot, ChangeArea::Staged, "new-name.txt");
    let selection = vec![PathSelection::from_changed_path(rename)];

    // Unstaging must undo both halves, not just the destination.
    let outcome = service
        .unstage(
            &UnstageRequest {
                worktree: fixture.work(),
                selection: selection.clone(),
            },
            &CancellationToken::new(),
        )
        .expect("unstage");
    assert!(matches!(outcome, MutationOutcome::Applied { .. }));
    assert_eq!(fixture.porcelain(), " D old-name.txt\n?? new-name.txt");

    // Staging it again restores the recognised rename.
    let outcome = service
        .stage(
            &StageRequest {
                worktree: fixture.work(),
                selection: selection.clone(),
            },
            &CancellationToken::new(),
        )
        .expect("stage");
    assert!(matches!(outcome, MutationOutcome::Applied { .. }));
    assert_eq!(fixture.porcelain(), "R  old-name.txt -> new-name.txt");

    // Discarding the rename: the tracked source comes back from the index and
    // the untracked destination is deleted.
    let outcome = service
        .unstage(
            &UnstageRequest {
                worktree: fixture.work(),
                selection,
            },
            &CancellationToken::new(),
        )
        .expect("unstage again");
    assert!(matches!(outcome, MutationOutcome::Applied { .. }));
    let outcome = service
        .discard(
            &DiscardRequest {
                worktree: fixture.work(),
                tracked: vec![PathSelection::new(path("old-name.txt"))],
                untracked: vec![path("new-name.txt")],
                confirmed: true,
            },
            &CancellationToken::new(),
        )
        .expect("discard");
    assert_eq!(
        outcome,
        MutationOutcome::Applied {
            paths: vec![path("old-name.txt"), path("new-name.txt")]
        }
    );
    assert_eq!(fixture.porcelain(), "");
    assert!(fixture.work().join("old-name.txt").is_file());
    assert!(!fixture.work().join("new-name.txt").exists());
}

/// A `pre-commit` hook can leave a process outside Git's process group holding
/// the stdout and stderr the executor reads. The process group signalled at the
/// deadline cannot reach it, so nothing will ever close those pipes — and the
/// caller is holding this repository's mutation lock while it waits.
///
/// The failure mode being ruled out is a permanent hang, so this test's own
/// patience is what fails: it never asserts that the command was fast, only
/// that it produced a result at all and gave the lock back.
#[cfg(unix)]
#[test]
fn a_hook_that_backgrounds_a_pipe_holder_still_releases_the_mutation_lock() {
    use std::os::unix::fs::PermissionsExt;

    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");

    let holder = fixture.root.path().join("holder.pid");
    let hooks = fixture.work().join(".git/hooks");
    fs::create_dir_all(&hooks).expect("hooks directory");
    let hook = hooks.join("pre-commit");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\n\
             setsid sh -c 'printf %s \"$$\" > \"{}\"; exec sleep 300' &\n\
             exit 0\n",
            holder.display()
        ),
    )
    .expect("write hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("hook is executable");

    fixture.write("a.txt", b"two\n");
    fixture.git_in_work(&["add", "-A"]);

    let command = fixture
        .service()
        .commit_command(&CommitRequest {
            worktree: fixture.work(),
            message: "hooked".into(),
        })
        .expect("commit command")
        // Long enough that the hook certainly ran and detached, short enough
        // that the test does not sit through the production timeout.
        .timeout(Duration::from_secs(2));

    let locks = Arc::new(MutationLocks::new());
    let common = fixture.work().join(".git");
    let (report, results) = mpsc::channel();
    {
        let locks = Arc::clone(&locks);
        let common = common.clone();
        thread::spawn(move || {
            let guard = locks.lock(&common);
            let outcome = IsolatedExecutor(SystemProcessExecutor)
                .execute(&command, &CancellationToken::new());
            drop(guard);
            let _ = report.send(outcome.err().map(|failure| failure.kind));
        });
    }
    let kind = results
        .recv_timeout(Duration::from_secs(60))
        .expect("the commit never returned while a hook's child held its pipes");
    assert_eq!(kind, Some(ProcessFailureKind::TimedOut));
    assert!(
        holder.is_file(),
        "the hook never launched its detached child, so the hazard was not exercised"
    );

    let (acquired, acquisitions) = mpsc::channel();
    thread::spawn(move || {
        let _guard = locks.lock(&common);
        let _ = acquired.send(());
    });
    acquisitions
        .recv_timeout(Duration::from_secs(10))
        .expect("the mutation lock was still held after the command returned");

    // The sleeper is in its own session precisely so that nothing this service
    // signals can reach it, which leaves the cleanup to the test.
    if let Ok(text) = fs::read_to_string(&holder)
        && let Ok(group) = text.trim().parse::<u32>()
    {
        let _ = Command::new("kill")
            .arg("-9")
            .arg("--")
            .arg(format!("-{group}"))
            .output();
    }
}

/// A discard request is built from a snapshot and then sits in front of the
/// user in a confirmation dialog. Another Git client may stage or commit one of
/// those untracked paths in the meantime, and unlinking a file that is now
/// tracked is not what was confirmed.
#[test]
fn discard_refuses_a_path_that_became_tracked_after_it_was_selected() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");

    // Selected while it really was untracked.
    fixture.write("late.txt", b"work that exists nowhere else\n");
    let snapshot = fixture.snapshot();
    assert_eq!(
        entry(&snapshot, ChangeArea::Untracked, "late.txt").kind,
        ChangeKind::Untracked
    );
    let request = DiscardRequest {
        worktree: fixture.work(),
        tracked: Vec::new(),
        untracked: vec![path("late.txt")],
        confirmed: true,
    };

    // Another client stages it while the dialog is open.
    fixture.git_in_work(&["add", "late.txt"]);

    let outcome = fixture
        .service()
        .discard(&request, &CancellationToken::new())
        .expect("discard");
    match &outcome {
        MutationOutcome::NotApplied {
            failure: MutationFailure::Path { path: reported, .. },
        } => assert_eq!(*reported, path("late.txt")),
        other => panic!("expected a per-path refusal, got {other:?}"),
    }
    assert!(fixture.work().join("late.txt").is_file());
    assert_eq!(fixture.porcelain(), "A  late.txt");

    // Unstaging it restores the classification the request was built against,
    // and the same request then applies.
    fixture.git_in_work(&["restore", "--staged", "late.txt"]);
    let outcome = fixture
        .service()
        .discard(&request, &CancellationToken::new())
        .expect("discard");
    assert_eq!(
        outcome,
        MutationOutcome::Applied {
            paths: vec![path("late.txt")]
        }
    );
    assert!(!fixture.work().join("late.txt").exists());
}

/// Git will not look inside another repository, so even
/// `--untracked-files=all` reports a nested clone as a directory. A snapshot
/// of a worktree that contains one must still succeed.
#[test]
fn an_untracked_nested_repository_is_listed_as_its_directory() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");
    fs::create_dir(fixture.work().join("subclone")).expect("nested directory");
    fixture.run(
        &fixture.work().join("subclone"),
        &["init", "-b", "main", "."],
    );
    fixture.write("subclone/inner.txt", b"inner\n");
    fixture.write("plain/deep/file.txt", b"deep\n");

    let snapshot = fixture.snapshot();
    assert_eq!(
        entry(&snapshot, ChangeArea::Untracked, "subclone").kind,
        ChangeKind::Untracked
    );
    // The nested repository's own files are not this worktree's business.
    assert!(
        !snapshot
            .status
            .entries
            .iter()
            .any(|entry| entry.path.display().starts_with("subclone/"))
    );
    entry(&snapshot, ChangeArea::Untracked, "plain/deep/file.txt");
    assert!(snapshot.files.paths.contains(&path("subclone")));
}

#[test]
fn literal_pathspecs_stage_only_the_named_file() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("keep.txt", b"keep\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");

    fixture.write(":(top)magic.txt", b"magic\n");
    fixture.write(":colon.txt", b"colon\n");
    fixture.write("other.txt", b"other\n");

    let outcome = fixture
        .service()
        .stage(
            &StageRequest {
                worktree: fixture.work(),
                selection: vec![
                    PathSelection::new(path(":(top)magic.txt")),
                    PathSelection::new(path(":colon.txt")),
                ],
            },
            &CancellationToken::new(),
        )
        .expect("stage");

    assert!(matches!(outcome, MutationOutcome::Applied { .. }));
    // Without `--literal-pathspecs`, `:(top)` and `:` would be pathspec magic
    // and this would either fail or stage a different set of files.
    let staged = fixture.text(&["diff", "--cached", "--name-only"]);
    let mut staged: Vec<_> = staged.lines().collect();
    staged.sort_unstable();
    assert_eq!(staged, vec![":(top)magic.txt", ":colon.txt"]);
    assert_eq!(
        fixture.text(&["status", "--porcelain", "--", "other.txt"]),
        "?? other.txt"
    );
}

#[test]
fn a_commit_reports_the_object_id_and_message_it_created() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");

    let service = fixture.service();
    let empty = service
        .commit(
            &CommitRequest {
                worktree: fixture.work(),
                message: "nothing staged".into(),
            },
            &CancellationToken::new(),
        )
        .expect("commit attempt");
    assert!(
        matches!(empty, CommitOutcome::NotCreated { .. }),
        "an empty index must not produce a commit, got {empty:?}"
    );

    fixture.write("a.txt", b"one\ntwo\n");
    fixture.git_in_work(&["add", "a.txt"]);
    let before = fixture.head();
    let outcome = service
        .commit(
            &CommitRequest {
                worktree: fixture.work(),
                message: "subject line\n\nbody line\n".into(),
            },
            &CancellationToken::new(),
        )
        .expect("commit");

    let created = match outcome {
        CommitOutcome::Created { oid, warnings } => {
            assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
            oid
        }
        other => panic!("expected a created commit, got {other:?}"),
    };
    assert_ne!(created, before);
    assert_eq!(created, fixture.head());
    assert_eq!(
        fixture.text(&["log", "-1", "--format=%B"]),
        "subject line\n\nbody line"
    );
    assert_eq!(fixture.porcelain(), "");
}

/// A message typed into this application's own commit box was never shown in
/// an editor, so it contains no commentary for Git to strip — which is exactly
/// why Git's own default for `-F`/`--file` is `whitespace`. Under
/// `--cleanup=strip` every line beginning with `core.commentChar` is deleted
/// from the object, silently and permanently, and the outcome still reports a
/// created commit.
#[test]
fn a_commit_message_keeps_the_lines_the_user_typed() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");

    let service = fixture.service();
    fixture.write("a.txt", b"two\n");
    fixture.git_in_work(&["add", "-A"]);
    let outcome = service
        .commit(
            &CommitRequest {
                worktree: fixture.work(),
                message: "#42 fix the thing\n\nbody line\n".into(),
            },
            &CancellationToken::new(),
        )
        .expect("commit");
    assert!(
        matches!(outcome, CommitOutcome::Created { .. }),
        "got {outcome:?}"
    );
    assert_eq!(
        fixture.text(&["log", "-1", "--format=%s"]),
        "#42 fix the thing"
    );
    assert_eq!(fixture.text(&["log", "-1", "--format=%b"]), "body line");

    // A message that is nothing but comment lines used to leave Git with an
    // empty message and abort the commit.
    fixture.write("a.txt", b"three\n");
    fixture.git_in_work(&["add", "-A"]);
    let outcome = service
        .commit(
            &CommitRequest {
                worktree: fixture.work(),
                message: "#only a reference\n".into(),
            },
            &CancellationToken::new(),
        )
        .expect("commit");
    assert!(
        matches!(outcome, CommitOutcome::Created { .. }),
        "got {outcome:?}"
    );
    assert_eq!(
        fixture.text(&["log", "-1", "--format=%s"]),
        "#only a reference"
    );
    // Trailing whitespace is still cleaned, which is what `whitespace` means.
    assert_eq!(
        fixture.text(&["log", "-1", "--format=%B"]),
        "#only a reference"
    );
}

/// During a merge the index can legitimately equal HEAD's tree — every
/// conflict resolved in favour of ours — while `MERGE_HEAD` still exists and
/// the merge commit is still required. `git commit` creates it; refusing on
/// "nothing staged" refused an operation Git accepts, and left the user in a
/// merge this application offers no other way out of.
#[test]
fn a_merge_resolved_in_favour_of_ours_is_still_committed() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("f.txt", b"base\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("base");

    fixture.git_in_work(&["checkout", "--quiet", "-b", "side"]);
    fixture.write("f.txt", b"theirs\n");
    fixture.git_in_work(&["add", "-A"]);
    let theirs = fixture.commit("theirs");

    fixture.git_in_work(&["checkout", "--quiet", "main"]);
    fixture.write("f.txt", b"ours\n");
    fixture.git_in_work(&["add", "-A"]);
    let ours = fixture.commit("ours");

    // A conflict, resolved by keeping our side exactly as it was.
    fixture.run_failing(&fixture.work(), &["merge", "--no-edit", "side"]);
    assert!(fixture.work().join(".git/MERGE_HEAD").is_file());
    fixture.write("f.txt", b"ours\n");
    fixture.git_in_work(&["add", "f.txt"]);

    // Git's own two answers to "is there anything to commit": both say no.
    let snapshot = fixture.snapshot();
    assert!(
        snapshot.status.entries.is_empty(),
        "the resolution left the index equal to HEAD, so status has nothing to report: {:?}",
        snapshot.status.entries
    );
    assert_eq!(fixture.porcelain(), "");

    let outcome = fixture
        .service()
        .commit(
            &CommitRequest {
                worktree: fixture.work(),
                message: "merge side into main\n".into(),
            },
            &CancellationToken::new(),
        )
        .expect("commit");
    let created = match outcome {
        CommitOutcome::Created { oid, warnings } => {
            assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
            oid
        }
        other => panic!("expected the merge commit, got {other:?}"),
    };

    assert_eq!(created, fixture.head());
    assert_eq!(
        fixture.text(&["log", "-1", "--format=%P"]),
        format!("{} {}", ours.as_str(), theirs.as_str()),
        "the commit must have both parents, or it is not a merge"
    );
    assert!(
        !fixture.work().join(".git/MERGE_HEAD").exists(),
        "the merge is concluded"
    );
    // And with the merge concluded, the same refusal applies again.
    let outcome = fixture
        .service()
        .commit(
            &CommitRequest {
                worktree: fixture.work(),
                message: "nothing left".into(),
            },
            &CancellationToken::new(),
        )
        .expect("commit attempt");
    assert!(
        matches!(outcome, CommitOutcome::NotCreated { .. }),
        "got {outcome:?}"
    );
}

/// A ref name is an arbitrary byte string with no NUL and no LF in it, and
/// `git checkout -b` accepts one that no encoding decodes. Forcing the branch
/// label through a UTF-8 conversion that can fail discarded the entire
/// snapshot — every changed path, every count, every ref — because of a label.
#[cfg(unix)]
#[test]
fn a_branch_name_that_is_not_utf8_costs_nothing_but_its_own_label() {
    use std::os::unix::ffi::OsStrExt;

    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");

    let branch = OsStr::from_bytes(b"b\xffad");
    fixture.git_in_work(&[
        OsStr::new("checkout"),
        OsStr::new("--quiet"),
        OsStr::new("-b"),
        branch,
    ]);
    // Confirm the premise: this is really what Git stored, byte for byte.
    assert_eq!(
        fixture.git_in_work(&["symbolic-ref", "HEAD"]),
        b"refs/heads/b\xffad\n".to_vec()
    );

    fixture.write("a.txt", b"two\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.write("untracked.txt", b"work\n");

    let snapshot = fixture.snapshot();
    let expected = String::from_utf8_lossy(b"b\xffad").into_owned();
    assert_eq!(snapshot.status.branch(), Some(expected.as_str()));
    // Everything the snapshot exists to report survived the label.
    assert_eq!(snapshot.status.dirty.staged, 1);
    assert_eq!(snapshot.status.dirty.untracked, 1);
    entry(&snapshot, ChangeArea::Staged, "a.txt");
    entry(&snapshot, ChangeArea::Untracked, "untracked.txt");
    assert!(snapshot.files.paths.contains(&path("a.txt")));
    // `main` is still a ref this repository has, and `for-each-ref` emitted the
    // undecodable one in the same output.
    assert!(
        snapshot
            .local_refs
            .iter()
            .any(|record| record.name.as_str() == "refs/heads/main"),
        "refs were {:?}",
        snapshot
            .local_refs
            .iter()
            .map(|record| record.name.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        !snapshot.local_refs.iter().any(|record| record.head),
        "a ref name this cannot represent is left out rather than guessed at"
    );
}

/// Git splits a commit's recorded offset as `N / 100` hours plus `N % 100`
/// minutes with no range check, so these three shapes — all of which occur in
/// CVS- and Subversion-imported histories — are what it really reports. A
/// parser that required `<sign><4 digits>` with minutes below 60 discarded the
/// whole 200-commit page over one of them.
#[test]
fn a_commit_date_git_cannot_normalise_costs_only_its_own_row() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    let root = fixture.commit("initial");
    let tree = fixture.text(&["rev-parse", "HEAD^{tree}"]);

    // Written with `--literally` because these are exactly the objects Git
    // will not produce itself but will happily read back.
    let object = fixture.root.path().join("commit-object");
    let mut parent = root.as_str().to_owned();
    let mut written = Vec::new();
    for (offset, subject) in [
        ("+051800", "five digit offset"),
        ("+0060", "sixty minutes"),
        ("+garbage", "not a number"),
    ] {
        fs::write(
            &object,
            format!(
                "tree {tree}\n\
                 parent {parent}\n\
                 author Cagents Contract <contract@example.invalid> 1700000000 {offset}\n\
                 committer Cagents Contract <contract@example.invalid> 1700000000 +0000\n\
                 \n\
                 {subject}\n"
            ),
        )
        .expect("write commit object");
        let oid = String::from_utf8(fixture.git_in_work(&[
            OsStr::new("hash-object"),
            OsStr::new("-t"),
            OsStr::new("commit"),
            OsStr::new("-w"),
            OsStr::new("--literally"),
            object.as_os_str(),
        ]))
        .expect("UTF-8 object id");
        parent = oid.trim().to_owned();
        written.push(parent.clone());
    }
    fixture.git_in_work(&["update-ref", "refs/heads/imported", &parent]);

    let commits = fixture
        .service()
        .history(
            &HistoryRequest {
                worktree: fixture.work(),
                revision: HistoryRevision::Reference {
                    name: ValidatedRefName::parse("refs/heads/imported").unwrap(),
                },
                page: 0,
                path: None,
            },
            &CancellationToken::new(),
        )
        .expect("history");

    // Newest first: garbage, +0060, +051800, then the ordinary root commit.
    assert_eq!(commits.len(), 4);
    assert_eq!(commits[3].oid, root);
    assert_eq!(commits[3].author.offset_minutes, 0);

    assert_eq!(commits[0].subject, "not a number");
    assert_eq!(commits[0].oid.as_str(), written[2]);
    // Git prints no date at all for this one, so there is nothing to render.
    assert_eq!(
        fixture.text(&["log", "-1", "--format=%ad", "--date=raw", &written[2]]),
        ""
    );
    assert_eq!(commits[0].author.timestamp, 0);
    assert_eq!(commits[0].author.offset_minutes, 0);

    assert_eq!(commits[1].subject, "sixty minutes");
    assert_eq!(
        fixture.text(&["log", "-1", "--format=%ad", "--date=raw", &written[1]]),
        "1700000000 +0060"
    );
    assert_eq!(commits[1].author.timestamp, 1_700_000_000);
    assert_eq!(commits[1].author.offset_minutes, 60);

    assert_eq!(commits[2].subject, "five digit offset");
    // Git drops the leading zero, which is how a five-digit offset reaches the
    // parser in the first place.
    assert_eq!(
        fixture.text(&["log", "-1", "--format=%ad", "--date=raw", &written[0]]),
        "1700000000 +51800"
    );
    assert_eq!(commits[2].author.offset_minutes, 518 * 60);

    // Every committer half was ordinary, and none of them was disturbed.
    assert!(
        commits
            .iter()
            .all(|commit| commit.committer.timestamp == 1_700_000_000
                && commit.committer.offset_minutes == 0)
    );
}

/// The two ways `git restore --worktree` *refuses* a request — a pathspec that
/// matches nothing, a path that is unmerged — are both settled before it
/// writes anything, so both really have changed nothing at all and both exit
/// 1. Reporting them as "may have applied part of a multi-path change" asked
/// the user to reconcile a destructive operation that never happened.
///
/// This is a claim about a refusal and not about `restore` as a whole:
/// `a_restore_that_failed_inside_its_write_loop_never_says_nothing_happened`
/// covers the other side, where Git exits 255 having destroyed work. `git add`
/// is different again and genuinely applies what it could.
#[test]
fn a_refused_restore_changed_nothing_and_a_refused_add_may_have() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.write("c.txt", b"one\n");
    fixture.write(".gitignore", b"ignored.txt\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");
    fixture.write("a.txt", b"edited\n");

    // Git's own behaviour, pinned: the restore fails and a.txt stays dirty.
    // The exit code is what separates this from a failure inside the write
    // loop, so it is part of the claim.
    let (code, stderr) = fixture.run_failing_status(
        &fixture.work(),
        &["restore", "--worktree", "--", "a.txt", "no-such-file.txt"],
    );
    assert_eq!(code, Some(1), "git's exit code changed: {stderr}");
    assert!(
        stderr.contains("did not match"),
        "git's reason changed: {stderr}"
    );
    assert_eq!(fixture.porcelain(), " M a.txt");

    let service = fixture.service();
    let outcome = service
        .discard(
            &DiscardRequest {
                worktree: fixture.work(),
                tracked: vec![
                    PathSelection::new(path("a.txt")),
                    PathSelection::new(path("no-such-file.txt")),
                ],
                untracked: Vec::new(),
                confirmed: true,
            },
            &CancellationToken::new(),
        )
        .expect("discard");
    assert!(
        matches!(outcome, MutationOutcome::NotApplied { .. }),
        "a restore that changed nothing is not an unknown outcome: {outcome:?}"
    );
    assert_eq!(
        fixture.porcelain(),
        " M a.txt",
        "the file the user asked to keep an eye on is untouched"
    );

    // An unmerged path is refused for a different reason, with the same
    // all-or-nothing result.
    fixture.git_in_work(&["checkout", "--quiet", "-b", "side"]);
    fixture.git_in_work(&["checkout", "--quiet", "--", "a.txt"]);
    fixture.write("c.txt", b"theirs\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("theirs");
    fixture.git_in_work(&["checkout", "--quiet", "main"]);
    fixture.write("c.txt", b"ours\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("ours");
    fixture.run_failing(&fixture.work(), &["merge", "--no-edit", "side"]);
    let (code, stderr) =
        fixture.run_failing_status(&fixture.work(), &["restore", "--worktree", "--", "c.txt"]);
    assert_eq!(code, Some(1), "git's exit code changed: {stderr}");
    assert!(
        stderr.contains("unmerged"),
        "git's reason changed: {stderr}"
    );
    let outcome = service
        .discard(
            &DiscardRequest {
                worktree: fixture.work(),
                tracked: vec![PathSelection::new(path("c.txt"))],
                untracked: Vec::new(),
                confirmed: true,
            },
            &CancellationToken::new(),
        )
        .expect("discard");
    assert!(
        matches!(outcome, MutationOutcome::NotApplied { .. }),
        "got {outcome:?}"
    );
    fixture.git_in_work(&["merge", "--abort"]);

    // `git add` really is partial: it stages what it could before failing.
    fixture.write("ignored.txt", b"ignored\n");
    fixture.write("good.txt", b"good\n");
    let stderr = fixture.run_failing(
        &fixture.work(),
        &["add", "-A", "--", "good.txt", "ignored.txt"],
    );
    assert!(
        stderr.contains("ignored by one of your .gitignore files"),
        "git's reason changed: {stderr}"
    );
    assert_eq!(
        fixture.text(&["diff", "--cached", "--name-only"]),
        "good.txt"
    );
    fixture.git_in_work(&["restore", "--staged", "good.txt"]);

    let outcome = service
        .stage(
            &StageRequest {
                worktree: fixture.work(),
                selection: vec![
                    PathSelection::new(path("good.txt")),
                    PathSelection::new(path("ignored.txt")),
                ],
            },
            &CancellationToken::new(),
        )
        .expect("stage");
    assert!(
        matches!(outcome, MutationOutcome::OutcomeUnknown { .. }),
        "an add that staged one of two paths is exactly an unknown outcome: {outcome:?}"
    );
    assert_eq!(
        fixture.text(&["diff", "--cached", "--name-only"]),
        "good.txt",
        "and the outcome is unknown because this really did happen"
    );
}

/// Git reports an untracked nested repository as its directory even under
/// `--untracked-files=all`, and a directory is the one thing a discard will
/// never unlink. Abandoning the loop at that refusal made the set of files
/// that survived depend on nothing but where the directory's name sorted, and
/// reported the result as an unknown outcome naming none of them.
#[test]
fn a_discard_attempts_every_path_and_names_what_it_refused() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");

    // Sorted either side of the directory, so the old loop deleted the first
    // and kept the last.
    fixture.write("000-before.txt", b"work that exists nowhere else\n");
    fixture.write("zzz-after.txt", b"more of the same\n");
    fs::create_dir(fixture.work().join("nested")).expect("nested directory");
    fixture.run(&fixture.work().join("nested"), &["init", "-b", "main", "."]);
    fixture.write("nested/inner.txt", b"inner\n");

    // The premise, in git's own words and git's own order.
    let listed = fixture.text(&["status", "--porcelain", "--untracked-files=all"]);
    assert_eq!(
        listed.lines().collect::<Vec<_>>(),
        vec!["?? 000-before.txt", "?? nested/", "?? zzz-after.txt"]
    );

    let outcome = fixture
        .service()
        .discard(
            &DiscardRequest {
                worktree: fixture.work(),
                tracked: Vec::new(),
                untracked: vec![
                    path("000-before.txt"),
                    path("nested"),
                    path("zzz-after.txt"),
                ],
                confirmed: true,
            },
            &CancellationToken::new(),
        )
        .expect("discard");

    match outcome {
        MutationOutcome::PartiallyApplied { paths, failures } => {
            assert_eq!(paths, vec![path("000-before.txt"), path("zzz-after.txt")]);
            match failures.as_slice() {
                [MutationFailure::Path { path: refused, .. }] => {
                    assert_eq!(*refused, path("nested"));
                }
                other => panic!("expected one per-path refusal, got {other:?}"),
            }
        }
        other => panic!("expected a partial application, got {other:?}"),
    }
    // The path that sorts after the refusal was attempted, which is the whole
    // point, and the nested repository was not touched.
    assert!(!fixture.work().join("000-before.txt").exists());
    assert!(!fixture.work().join("zzz-after.txt").exists());
    assert!(fixture.work().join("nested/inner.txt").is_file());
    assert!(fixture.work().join("nested/.git").exists());
}

/// `refs/remotes/<remote>/HEAD` is a symbolic ref: it is the remote's
/// default-branch pointer, not a branch. Stripping `refs/remotes/` and
/// splitting on the first `/` turned it into a remote branch literally named
/// `HEAD`, which every clone has and which silently duplicated the default
/// branch wherever remote branches are listed. A ref that classifies as
/// neither is skipped, because one unrecognised ref is not evidence about the
/// rest of the output.
#[test]
fn a_remote_head_pointer_is_not_a_branch_and_an_odd_ref_is_not_fatal() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");
    let origin = fixture.origin();
    let origin_url = origin.to_str().expect("UTF-8 path");
    fixture.git_in_work(&["remote", "add", "origin", origin_url]);
    fixture.git_in_work(&["push", "--quiet", "origin", "main"]);
    fixture.git_in_work(&["fetch", "--quiet", "origin"]);
    // Every clone has this; a repository with a manually added remote gets it
    // the moment anything asks the remote what its default branch is.
    fixture.git_in_work(&["remote", "set-head", "origin", "main"]);
    let head_target = fixture.text(&["symbolic-ref", "refs/remotes/origin/HEAD"]);
    assert_eq!(head_target, "refs/remotes/origin/main");
    // Legal, and some tools create it: a ref directly under refs/remotes/.
    let oid = fixture.text(&["rev-parse", "HEAD"]);
    fixture.git_in_work(&["update-ref", "refs/remotes/weird", &oid]);

    let snapshot = fixture.snapshot();

    assert_eq!(
        snapshot
            .remote_refs
            .iter()
            .map(|record| (record.remote.as_str(), record.branch.as_str()))
            .collect::<Vec<_>>(),
        vec![("origin", "main")],
        "the pointer and the unclassifiable ref are both absent from the branch list"
    );
    assert_eq!(
        snapshot.remote_head("origin").map(ValidatedRefName::as_str),
        Some("refs/remotes/origin/main"),
        "and the pointer is reported as what it is"
    );
    // The rest of the snapshot is intact, which the hard error destroyed.
    assert!(
        snapshot
            .local_refs
            .iter()
            .any(|record| record.name.as_str() == "refs/heads/main" && record.head)
    );
    assert_eq!(snapshot.remotes.len(), 1);
}

/// A blobless or partial clone whose promisor remote is unreachable answers
/// `<oid> missing` on the `--batch` stream. That is Git telling the truth
/// about the object, not Git malfunctioning, and it used to surface as a parse
/// error over the whole read.
///
/// This pins the bytes Git really emits; the unit test
/// `a_missing_object_is_reported_as_unavailable_not_as_malformed_output`
/// consumes exactly this string and asserts the side becomes `Unreadable`.
#[test]
fn git_answers_missing_for_an_object_this_repository_does_not_have() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");

    // A well-formed object id this repository has never seen, and a real blob
    // beside it so the stream's framing is exercised rather than just its
    // error path.
    let absent = Oid::parse("f".repeat(40)).expect("object id");
    let present = fixture.text(&["rev-parse", "HEAD:a.txt"]);

    let mut child = Command::new("git")
        .current_dir(fixture.work())
        .args(["cat-file", "--batch"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("git cat-file --batch");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{}\n{present}\n", absent.as_str()).as_bytes())
        .expect("write object ids");
    let output = child.wait_with_output().expect("cat-file output");
    assert!(output.status.success());

    assert_eq!(
        output.stdout,
        format!("{} missing\n{present} blob 4\none\n\n", absent.as_str()).into_bytes(),
        "the `missing` header, and the normal object after it, are what the \
         parser has to accept"
    );
}

/// `git add` and `git restore --worktree` match a pathspec against the index
/// and the working tree. The source half of a *staged* rename is in neither —
/// only HEAD still has that name — so sending both halves made `add` exit 128
/// having staged nothing and `restore` exit 1 having restored nothing.
/// `restore --staged` is the exception: it does want both halves, and unstage
/// keeps sending them.
#[test]
fn a_staged_rename_sends_each_operation_only_the_halves_it_can_match() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("sp ace.txt", b"rename me\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");
    fixture.git_in_work(&["mv", "sp ace.txt", "moved.txt"]);

    // Git's own behaviour with both halves, pinned in both orders.
    for arguments in [
        &["add", "-A", "--", "sp ace.txt", "moved.txt"],
        &["add", "-A", "--", "moved.txt", "sp ace.txt"],
    ] {
        let stderr = fixture.run_failing(&fixture.work(), arguments);
        assert!(
            stderr.contains("did not match"),
            "git's reason changed: {stderr}"
        );
    }
    let stderr = fixture.run_failing(
        &fixture.work(),
        &["restore", "--worktree", "--", "sp ace.txt", "moved.txt"],
    );
    assert!(
        stderr.contains("did not match"),
        "git's reason changed: {stderr}"
    );
    // Porcelain v1 quotes a path with a space in it; the service's own
    // pathspecs are byte-exact and never quoted.
    assert_eq!(fixture.porcelain(), "R  \"sp ace.txt\" -> moved.txt");

    let service = fixture.service();
    let snapshot = fixture.snapshot();
    let rename = entry(&snapshot, ChangeArea::Staged, "moved.txt");
    assert_eq!(rename.old_path.as_ref(), Some(&path("sp ace.txt")));
    let selection = vec![PathSelection::from_changed_path(rename)];

    // Staging an already-staged rename is a no-op that must still succeed.
    let outcome = service
        .stage(
            &StageRequest {
                worktree: fixture.work(),
                selection: selection.clone(),
            },
            &CancellationToken::new(),
        )
        .expect("stage");
    assert_eq!(
        outcome,
        MutationOutcome::Applied {
            paths: vec![path("moved.txt")]
        },
        "only the half `add` can match is sent"
    );
    assert_eq!(fixture.porcelain(), "R  \"sp ace.txt\" -> moved.txt");

    // Discarding the worktree half likewise restores what it can.
    fixture.write("moved.txt", b"edited after the rename\n");
    let outcome = service
        .discard(
            &DiscardRequest {
                worktree: fixture.work(),
                tracked: selection.clone(),
                untracked: Vec::new(),
                confirmed: true,
            },
            &CancellationToken::new(),
        )
        .expect("discard");
    assert_eq!(
        outcome,
        MutationOutcome::Applied {
            paths: vec![path("moved.txt")]
        }
    );
    assert_eq!(fixture.porcelain(), "R  \"sp ace.txt\" -> moved.txt");
    assert_eq!(
        fs::read(fixture.work().join("moved.txt")).expect("read"),
        b"rename me\n".to_vec()
    );

    // `restore --staged` wants both halves and correctly reverts both.
    let outcome = service
        .unstage(
            &UnstageRequest {
                worktree: fixture.work(),
                selection,
            },
            &CancellationToken::new(),
        )
        .expect("unstage");
    assert_eq!(
        outcome,
        MutationOutcome::Applied {
            paths: vec![path("sp ace.txt"), path("moved.txt")]
        }
    );
    assert_eq!(fixture.porcelain(), " D \"sp ace.txt\"\n?? moved.txt");
}

#[test]
fn publishing_then_pushing_a_branch_updates_the_remote() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    let first = fixture.commit("initial");
    let origin = fixture.origin();
    fixture.git_in_work(&["remote", "add", "origin", origin.to_str().unwrap()]);

    let service = fixture.service();
    let branch = ValidatedBranchName::parse("main").unwrap();
    assert_eq!(
        service
            .upstream_push_target(&fixture.work(), &branch, &CancellationToken::new())
            .expect("upstream lookup"),
        None,
        "a branch without an upstream must not be given one by guesswork"
    );

    let publish = service
        .push(
            &PushRequest {
                worktree: fixture.work(),
                branch: branch.clone(),
                target: PushTarget::SetUpstream {
                    remote: "origin".into(),
                    remote_branch: ValidatedBranchName::parse("main").unwrap(),
                },
            },
            &CancellationToken::new(),
        )
        .expect("publish");
    assert_eq!(
        publish,
        PushOutcome::Pushed {
            remote: "origin".into(),
            remote_ref: RemoteSideRef::parse("refs/heads/main").unwrap(),
            oid: first.clone(),
        }
    );
    assert_eq!(remote_oid(&fixture, "refs/heads/main"), first);

    // The upstream is now recorded, so a later push targets it explicitly.
    let target = service
        .upstream_push_target(&fixture.work(), &branch, &CancellationToken::new())
        .expect("upstream lookup")
        .expect("upstream exists");
    assert_eq!(
        target,
        PushTarget::Upstream {
            remote: "origin".into(),
            remote_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
        }
    );

    fixture.write("a.txt", b"one\ntwo\n");
    fixture.git_in_work(&["add", "-A"]);
    let second = fixture.commit("second");
    let outcome = service
        .push(
            &PushRequest {
                worktree: fixture.work(),
                branch,
                target,
            },
            &CancellationToken::new(),
        )
        .expect("second push");
    assert!(
        matches!(outcome, PushOutcome::Pushed { ref oid, .. } if *oid == second),
        "expected the second commit to be pushed, got {outcome:?}"
    );
    assert_eq!(remote_oid(&fixture, "refs/heads/main"), second);
}

#[test]
fn a_non_fast_forward_push_is_reported_as_not_pushed() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let published = publish_main(&fixture);

    // Advance the remote behind this worktree's back, exactly as another clone
    // would.
    let origin = fixture.origin();
    let tree = fixture.text(&["rev-parse", "HEAD^{tree}"]);
    let remote_commit = String::from_utf8(fixture.run(
        &origin,
        &[
            "commit-tree",
            &tree,
            "-p",
            published.as_str(),
            "-m",
            "remote side",
        ],
    ))
    .expect("UTF-8 commit id");
    let remote_commit = remote_commit.trim();
    fixture.run(&origin, &["update-ref", "refs/heads/main", remote_commit]);

    fixture.write("a.txt", b"local divergence\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("local divergence");

    let outcome = fixture
        .service()
        .push(
            &PushRequest {
                worktree: fixture.work(),
                branch: ValidatedBranchName::parse("main").unwrap(),
                target: PushTarget::Upstream {
                    remote: "origin".into(),
                    remote_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
                },
            },
            &CancellationToken::new(),
        )
        .expect("rejected push");

    match outcome {
        PushOutcome::NotPushed { failure } => {
            let evidence = failure.to_string();
            assert!(
                !evidence.is_empty(),
                "a rejection must carry the process evidence"
            );
        }
        other => panic!("expected a rejected push to be NotPushed, got {other:?}"),
    }
    // The remote is untouched, which is what `NotPushed` promises.
    assert_eq!(
        remote_oid(&fixture, "refs/heads/main").as_str(),
        remote_commit
    );
}

#[test]
fn an_ambiguous_push_is_reconciled_against_the_remote() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    publish_main(&fixture);
    fixture.write("a.txt", b"one\ntwo\n");
    fixture.git_in_work(&["add", "-A"]);
    let second = fixture.commit("second");

    // This service really runs the push, then reports it as a launched
    // failure, so only reconciliation can establish what happened.
    let service = RepositoryService::new(
        &fixture.git,
        Arc::new(AmbiguousPushExecutor(SystemProcessExecutor)),
    );
    let outcome = service
        .push(
            &PushRequest {
                worktree: fixture.work(),
                branch: ValidatedBranchName::parse("main").unwrap(),
                target: PushTarget::Upstream {
                    remote: "origin".into(),
                    remote_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
                },
            },
            &CancellationToken::new(),
        )
        .expect("ambiguous push");

    assert_eq!(
        outcome,
        PushOutcome::Pushed {
            remote: "origin".into(),
            remote_ref: RemoteSideRef::parse("refs/heads/main").unwrap(),
            oid: second.clone(),
        },
        "a push whose ref reached the remote must be reconciled as pushed"
    );
    assert_eq!(remote_oid(&fixture, "refs/heads/main"), second);

    // The same ambiguity with a remote that never received the commit stays
    // unknown rather than claiming success.
    fixture.write("a.txt", b"one\ntwo\nthree\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("third");
    let blocked = fixture.root.path().join("missing.git");
    fixture.git_in_work(&["remote", "add", "blocked", blocked.to_str().unwrap()]);
    let outcome = service
        .push(
            &PushRequest {
                worktree: fixture.work(),
                branch: ValidatedBranchName::parse("main").unwrap(),
                target: PushTarget::Upstream {
                    remote: "blocked".into(),
                    remote_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
                },
            },
            &CancellationToken::new(),
        )
        .expect("ambiguous push to a missing remote");
    assert!(
        matches!(outcome, PushOutcome::OutcomeUnknown { .. }),
        "expected an unknown outcome, got {outcome:?}"
    );
}

/// The remote-side half of a push refspec must name the branch the *remote*
/// keeps, never the remote-tracking ref this clone keeps for it.
///
/// `refs/heads/main:refs/remotes/origin/main` is a refspec Git accepts and
/// carries out: it creates a literal `refs/remotes/origin/main` inside the
/// remote repository, reports success in `--porcelain`, exits 0, and never
/// touches the branch. No outcome check can catch that, because the push
/// really did do what it was told — so the assertion has to be made in the
/// remote repository's own ref store, and again through the card's own
/// ahead/behind, which is the only place the user would ever have seen it.
#[test]
fn a_push_moves_the_remote_branch_and_invents_no_ref_in_the_remote() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let first = publish_main(&fixture);
    assert_eq!(remote_oid(&fixture, "refs/heads/main"), first);
    assert_eq!(bare_refs(&fixture), ["refs/heads/main"]);
    // `--set-upstream` has to have named the ref Git records as the upstream,
    // or the tracking configuration this whole path depends on never lands.
    assert_eq!(
        fixture.text(&["config", "--get", "branch.main.merge"]),
        "refs/heads/main"
    );

    // Exactly what the sidebar builds from a snapshot: the upstream as *this
    // clone* spells it, which is a `refs/remotes/...` ref and not a name the
    // remote has ever heard of.
    fixture.git_in_work(&["fetch", "--quiet", "origin"]);
    fixture.write("a.txt", b"one\ntwo\n");
    fixture.git_in_work(&["add", "-A"]);
    let second = fixture.commit("second");
    let outcome = fixture
        .service()
        .push(
            &PushRequest {
                worktree: fixture.work(),
                branch: ValidatedBranchName::parse("main").unwrap(),
                target: PushTarget::Upstream {
                    remote: "origin".into(),
                    remote_ref: ValidatedRefName::parse("refs/remotes/origin/main").unwrap(),
                },
            },
            &CancellationToken::new(),
        )
        .expect("push to the recorded upstream");

    assert_eq!(
        bare_refs(&fixture),
        ["refs/heads/main"],
        "a push must not create a remote-tracking ref inside the remote repository"
    );
    assert_eq!(remote_oid(&fixture, "refs/heads/main"), second);
    assert_eq!(
        outcome,
        PushOutcome::Pushed {
            remote: "origin".into(),
            remote_ref: RemoteSideRef::parse("refs/heads/main").unwrap(),
            oid: second.clone(),
        },
        "the reported ref is the one the remote moved, not this clone's tracking copy"
    );

    // The card's own view of it: fetched, this branch is no longer ahead.
    fixture.git_in_work(&["fetch", "--quiet", "origin"]);
    assert_eq!(
        fixture.snapshot().status.tracking,
        TrackingState::Compared {
            upstream: "origin/main".into(),
            ahead: 0,
            behind: 0,
        }
    );
}

/// A branch published under a different name follows the *upstream's* branch
/// name on every later push, not the local one.
///
/// An upstream of `refs/remotes/origin/published` for a local `work` is what a
/// rename on either side leaves behind, and it is the case that makes deriving
/// the destination from the local branch name — the other tempting shortcut —
/// silently push to the wrong branch instead of to no branch at all.
#[test]
fn a_push_follows_the_upstream_branch_name_rather_than_the_local_one() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    publish_main(&fixture);
    fixture.git_in_work(&["checkout", "--quiet", "-b", "work"]);
    fixture.write("b.txt", b"feature\n");
    fixture.git_in_work(&["add", "-A"]);
    let first = fixture.commit("feature");

    let service = fixture.service();
    let branch = ValidatedBranchName::parse("work").unwrap();
    let publish = service
        .push(
            &PushRequest {
                worktree: fixture.work(),
                branch: branch.clone(),
                target: PushTarget::SetUpstream {
                    remote: "origin".into(),
                    remote_branch: ValidatedBranchName::parse("published").unwrap(),
                },
            },
            &CancellationToken::new(),
        )
        .expect("publish under another name");
    assert_eq!(
        publish,
        PushOutcome::Pushed {
            remote: "origin".into(),
            remote_ref: RemoteSideRef::parse("refs/heads/published").unwrap(),
            oid: first.clone(),
        }
    );
    assert_eq!(remote_oid(&fixture, "refs/heads/published"), first);
    assert_eq!(
        fixture.text(&["config", "--get", "branch.work.merge"]),
        "refs/heads/published",
        "the tracking configuration must name the remote's branch"
    );

    // The service's own reading of that configuration, and the sidebar's
    // spelling of the same upstream, must reach the same branch on the wire.
    assert_eq!(
        service
            .upstream_push_target(&fixture.work(), &branch, &CancellationToken::new())
            .expect("upstream lookup")
            .expect("upstream exists"),
        PushTarget::Upstream {
            remote: "origin".into(),
            remote_ref: ValidatedRefName::parse("refs/heads/published").unwrap(),
        }
    );
    fixture.git_in_work(&["fetch", "--quiet", "origin"]);
    fixture.write("b.txt", b"feature\nmore\n");
    fixture.git_in_work(&["add", "-A"]);
    let second = fixture.commit("more");
    let outcome = service
        .push(
            &PushRequest {
                worktree: fixture.work(),
                branch,
                target: PushTarget::Upstream {
                    remote: "origin".into(),
                    remote_ref: ValidatedRefName::parse("refs/remotes/origin/published").unwrap(),
                },
            },
            &CancellationToken::new(),
        )
        .expect("push to the differently named upstream");
    assert_eq!(
        bare_refs(&fixture),
        ["refs/heads/main", "refs/heads/published"],
        "neither the local branch name nor a tracking ref may appear in the remote"
    );
    assert_eq!(remote_oid(&fixture, "refs/heads/published"), second);
    assert_eq!(
        outcome,
        PushOutcome::Pushed {
            remote: "origin".into(),
            remote_ref: RemoteSideRef::parse("refs/heads/published").unwrap(),
            oid: second.clone(),
        }
    );

    fixture.git_in_work(&["fetch", "--quiet", "origin"]);
    assert_eq!(
        fixture.snapshot().status.tracking,
        TrackingState::Compared {
            upstream: "origin/published".into(),
            ahead: 0,
            behind: 0,
        }
    );
}

#[test]
fn snapshot_reports_remotes_and_reference_names_in_full() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    publish_main(&fixture);
    fixture.git_in_work(&["fetch", "--quiet", "origin"]);

    let snapshot = fixture.snapshot();
    assert_eq!(snapshot.remotes.len(), 1);
    assert_eq!(snapshot.remotes[0].name, "origin");
    assert_eq!(
        snapshot.remotes[0].fetch_url.as_deref(),
        fixture.origin().to_str()
    );
    let main = snapshot
        .local_refs
        .iter()
        .find(|reference| reference.name.as_str() == "refs/heads/main")
        .expect("local main ref");
    assert!(main.head);
    assert_eq!(
        main.upstream.as_ref().map(ValidatedRefName::as_str),
        Some("refs/remotes/origin/main")
    );
    assert!(
        snapshot
            .remote_refs
            .iter()
            .any(|reference| reference.remote == "origin" && reference.branch == "main")
    );
}

/// Commits one file, adds the local bare remote, and publishes `main`.
fn publish_main(fixture: &Fixture) -> Oid {
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    let first = fixture.commit("initial");
    let origin = fixture.origin();
    fixture.git_in_work(&["remote", "add", "origin", origin.to_str().unwrap()]);
    let outcome = fixture
        .service()
        .push(
            &PushRequest {
                worktree: fixture.work(),
                branch: ValidatedBranchName::parse("main").unwrap(),
                target: PushTarget::SetUpstream {
                    remote: "origin".into(),
                    remote_branch: ValidatedBranchName::parse("main").unwrap(),
                },
            },
            &CancellationToken::new(),
        )
        .expect("publish");
    assert!(
        matches!(outcome, PushOutcome::Pushed { .. }),
        "publishing must succeed, got {outcome:?}"
    );
    first
}

fn remote_oid(fixture: &Fixture, reference: &str) -> Oid {
    let output = fixture.run(&fixture.origin(), &["rev-parse", reference]);
    Oid::parse(String::from_utf8(output).expect("UTF-8 object id").trim()).expect("object id")
}

/// Every ref the bare remote holds, in `for-each-ref`'s own sorted order.
///
/// A push is only verified by what the remote repository ended up with: a
/// refspec that names the wrong destination still exits 0, so the absence of
/// the ref it would have created is the assertion that discriminates.
fn bare_refs(fixture: &Fixture) -> Vec<String> {
    let output = fixture.run(&fixture.origin(), &["for-each-ref", "--format=%(refname)"]);
    String::from_utf8(output)
        .expect("UTF-8 ref names")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Keeps the unused-import warning honest: `OsString` is only needed when this
/// test inspects raw arguments, which the pathspec test does.
#[test]
fn command_arguments_keep_pathspecs_as_single_arguments() {
    let service: RepositoryService<IsolatedExecutor> =
        RepositoryService::new("git", Arc::new(IsolatedExecutor(SystemProcessExecutor)));
    let command = service
        .stage_command(Path::new("/repo"), &[path(":(top)magic.txt")])
        .expect("stage command");
    assert_eq!(command.arguments[0], OsString::from("--literal-pathspecs"));
    assert_eq!(
        command.arguments.last(),
        Some(&OsString::from(":(top)magic.txt"))
    );
}

/// A project remembers its Git common directory, so reopening one hands
/// `<root>/.git` straight back to discovery. Real `git` refuses
/// `rev-parse --show-toplevel` there — "this operation must be run in a work
/// tree", exit 128 — which failed one Git call on every single repository load
/// and left the reopened card rooted at `.git` and titled `.git`.
#[test]
fn a_git_directory_selected_as_the_repository_resolves_to_the_work_tree() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("src/main.rs", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");

    let git_directory = fixture.work().join(".git");
    let service = fixture.service();
    let cancellation = CancellationToken::new();

    // The premise, from real `git` rather than from memory: the work-tree
    // query is invalid inside a Git directory, so discovery must not issue it
    // there.
    let refused = IsolatedExecutor(SystemProcessExecutor)
        .execute(
            &service.worktree_root_command(&git_directory),
            &cancellation,
        )
        .expect("git runs");
    assert!(
        !refused.success,
        "git accepted --show-toplevel inside a Git directory"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("work tree"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    // Discovery from the same path nevertheless completes, and roots the
    // repository at the work tree rather than inside `.git`.
    let repository = service
        .discover(&git_directory, &cancellation)
        .expect("discovery from a Git directory");
    assert!(!repository.is_bare);
    let resolved = repository.worktree_root.expect("a resolved work tree");
    assert_ne!(resolved, git_directory);
    let named_from_inside =
        fixture.text(&["rev-parse", "--path-format=absolute", "--show-toplevel"]);
    assert_eq!(
        canonical(&resolved),
        canonical(Path::new(&named_from_inside)),
        "the root must be the one Git names from inside the work tree"
    );

    // A linked work tree's Git directory is `<common>/worktrees/<id>`, which
    // no path arithmetic can turn back into its work tree, so it is resolved
    // by asking each candidate for its own Git directory.
    let linked = fixture.root.path().join("linked");
    fixture.git_in_work(&[
        "worktree",
        "add",
        "-b",
        "linked",
        linked.to_str().expect("UTF-8 fixture path"),
    ]);
    let linked_git_directory = PathBuf::from(
        String::from_utf8(fixture.run(
            &linked,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        ))
        .expect("UTF-8 git output")
        .trim_end(),
    );
    let linked_repository = service
        .discover(&linked_git_directory, &cancellation)
        .expect("discovery from a linked work tree's Git directory");
    let linked_root = linked_repository
        .worktree_root
        .expect("a resolved work tree");
    assert_eq!(
        canonical(&linked_root),
        canonical(&linked),
        "a linked work tree's Git directory must resolve to that work tree"
    );
}

/// The shared workspace model keys a project by its repository, and the CLI and
/// a window derive that key independently: the CLI asks `git` directly, because
/// the shipped `cagents` binary has no Git library, while a window takes the
/// common directory discovery resolved. They must produce the same string.
///
/// They did not. The CLI asked `--show-toplevel` and got the work tree, while a
/// project stores its common directory, so a channel the CLI created was keyed
/// on `/repo` while every window's project was keyed on `/repo/.git`. No window
/// could find an owner for it, and the channel - the whole feature - never
/// appeared. This holds the two derivations to each other instead.
#[test]
fn the_cli_and_a_window_derive_one_repository_key_for_one_checkout() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("src/main.rs", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");
    fixture.write("nested/deeper/file.txt", b"two\n");
    let linked = fixture.root.path().join("linked");
    fixture.git_in_work(&[
        "worktree",
        "add",
        "-b",
        "linked",
        linked.to_str().expect("UTF-8 fixture path"),
    ]);

    let service = fixture.service();
    let cancellation = CancellationToken::new();
    // What a window stores for a project it opened, which is what it publishes
    // as `ModelEdit::OpenProject` and compares an adopted channel against.
    let published = canonical(
        &service
            .discover(&fixture.work(), &cancellation)
            .expect("discovery")
            .common_dir,
    );

    // Every place a user can be standing when they run the CLI, and every place
    // a window can be opened from. The key is the repository, not the
    // directory, so all of them have to agree - including a linked worktree,
    // whose own Git directory is `<common>/worktrees/<name>` and would have
    // keyed one project once per worktree.
    for from in [
        fixture.work(),
        fixture.work().join("nested/deeper"),
        fixture.work().join(".git"),
        linked.clone(),
    ] {
        let key = cagents::control::repository_key(&from)
            .unwrap_or_else(|error| panic!("the CLI keys {}: {error}", from.display()));
        assert_eq!(
            canonical(&key),
            published,
            "the CLI key from {} must be the key a window publishes",
            from.display()
        );
        // The window's half of the same claim. `cagents project open` in a
        // linked worktree and a window opened on the main checkout are the two
        // sides of the sidebar this shares, and a project each of them opened
        // has to be one row: discovery from anywhere in the checkout resolves
        // the same common directory the CLI derives.
        let discovered = canonical(
            &service
                .discover(&from, &cancellation)
                .unwrap_or_else(|error| panic!("a window opens {}: {error}", from.display()))
                .common_dir,
        );
        assert_eq!(
            discovered,
            published,
            "a window opened from {} must key the same project",
            from.display()
        );
    }

    // And the name the CLI supplies for that key has to be the checkout, not
    // the key's own last component, which is `.git` and names nothing.
    assert_eq!(
        cagents::workspace_model::repository_display_name(&published),
        fixture
            .work()
            .file_name()
            .expect("the fixture checkout is named")
            .to_string_lossy(),
    );
}

/// `git ls-files -o` lists *others*, i.e. untracked files, so counting the
/// combined listing claimed one extra tracked file for every untracked one:
/// the overview read "6 files tracked" where `git ls-files` returned five.
#[test]
fn the_tracked_file_count_agrees_with_git_ls_files() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("src/main.rs", b"one\n");
    fixture.write("Cargo.toml", b"[package]\n");
    fixture.write("keep.txt", b"keep\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");
    fixture.write("untracked-one.txt", b"one\n");
    fixture.write("nested/untracked-two.txt", b"two\n");

    let snapshot = fixture.snapshot();
    let tracked = fixture.text(&["ls-files"]).lines().count();
    let committed = fixture
        .text(&["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .count();

    assert_eq!(tracked, 3);
    assert_eq!(committed, 3, "the index and HEAD must agree here");
    assert_eq!(snapshot.files.tracked_count, tracked);
    // The combined listing is still what the file tree consumes, untracked
    // files included.
    assert_eq!(snapshot.files.paths.len(), tracked + 2);
    assert!(snapshot.files.paths.contains(&path("untracked-one.txt")));
    assert!(
        snapshot
            .files
            .paths
            .contains(&path("nested/untracked-two.txt"))
    );

    // One more untracked file moves the untracked total and leaves the tracked
    // count alone. Both moving together is exactly what was reported.
    fixture.write("untracked-three.txt", b"three\n");
    let after = fixture.snapshot();
    assert_eq!(after.files.tracked_count, tracked);
    assert_eq!(after.files.paths.len(), snapshot.files.paths.len() + 1);
}

/// `git status` rewrites `.git/index` when it finds the stat cache stale, and
/// `git diff` against the working tree rewrites it whatever the optional-lock
/// setting says. A filesystem watcher sees that write as a change to the
/// repository and asks for another state read, which reads again: the
/// application spawned Git processes by the hundred while sitting idle. A
/// read-only snapshot must therefore leave the index byte-for-byte alone.
#[test]
fn reading_a_snapshot_never_rewrites_the_index() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    const FILES: [&str; 3] = ["a.txt", "b.txt", "c.txt"];
    for name in FILES {
        fixture.write(name, b"content\n");
    }
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");

    // Settle the index first, then dirty the stat cache in a strictly later
    // second: Git treats an entry whose mtime falls inside the index's own
    // second as racily clean and declines to cache it, so the refresh this
    // test is about only happens once the timestamps are unambiguous. This is
    // the ordinary state of a worktree a build or a checkout has just touched.
    let _ = fixture.snapshot();
    thread::sleep(Duration::from_millis(1100));
    for name in FILES {
        let path = fixture.work().join(name);
        let contents = fs::read(&path).expect("read fixture file");
        fs::write(&path, contents).expect("rewrite fixture file with the same content");
    }

    let index = fixture.work().join(".git").join("index");
    let before = fs::read(&index).expect("read index");
    let snapshot = fixture.snapshot();

    // Nothing changed, so nothing may be reported changed either.
    assert!(
        snapshot.status.entries.is_empty(),
        "{:?}",
        snapshot.status.entries
    );
    assert_eq!(
        fs::read(&index).expect("read index"),
        before,
        "a read rewrote .git/index, which the filesystem watcher sees as a change"
    );
}

/// `git restore --worktree` resolves every pathspec before it writes anything,
/// but its checkout loop then walks the index ORing each entry's error into
/// one return value — so a failure raised *inside* the loop leaves the entries
/// it had already reached overwritten. An unwritable parent directory does
/// exactly that: exit 255, the other path's uncommitted work destroyed, in
/// either pathspec order. Classifying every non-zero `restore` as
/// "nothing was discarded" showed the user a red failure notice while the work
/// that notice promised was safe no longer existed.
#[cfg(unix)]
#[test]
fn a_restore_that_failed_inside_its_write_loop_never_says_nothing_happened() {
    use std::os::unix::fs::PermissionsExt;

    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("aaa/f", b"committed\n");
    fixture.write("zzz/f", b"committed\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");
    fixture.write("aaa/f", b"work that exists nowhere else\n");
    fixture.write("zzz/f", b"work that exists nowhere else\n");

    let blocked = fixture.work().join("aaa");
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o500)).expect("drop the write bit");
    // Root ignores a directory's write bit, so on that account the premise
    // this rests on does not hold and there is nothing here to observe.
    if fs::write(blocked.join("probe"), b"").is_ok() {
        fs::remove_file(blocked.join("probe")).expect("remove the probe");
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).expect("restore the mode");
        eprintln!("skipping: this user writes into a directory with no write bit");
        return;
    }

    // Git's own behaviour, pinned, in the order that puts the failing path
    // first: a mid-write failure is 255 and not the refusals' 1, and it takes
    // the other path's uncommitted work with it.
    let (code, stderr) = fixture.run_failing_status(
        &fixture.work(),
        &["restore", "--worktree", "--", "aaa/f", "zzz/f"],
    );
    assert_eq!(code, Some(255), "git's exit code changed: {stderr}");
    assert!(
        stderr.contains("unable to unlink old"),
        "git's reason changed: {stderr}"
    );
    assert_eq!(
        fs::read(fixture.work().join("zzz/f")).expect("read zzz/f"),
        b"committed\n",
        "the premise: git destroyed the path it did not fail on"
    );

    // And the same through the service, with the pathspecs the other way
    // round, because the loop follows index order and not the request's.
    fixture.write("zzz/f", b"work that exists nowhere else\n");
    let outcome = fixture
        .service()
        .discard(
            &DiscardRequest {
                worktree: fixture.work(),
                tracked: vec![
                    PathSelection::new(path("zzz/f")),
                    PathSelection::new(path("aaa/f")),
                ],
                untracked: Vec::new(),
                confirmed: true,
            },
            &CancellationToken::new(),
        )
        .expect("discard");
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).expect("restore the mode");

    assert_eq!(
        fs::read(fixture.work().join("zzz/f")).expect("read zzz/f"),
        b"committed\n",
        "the premise again: this discard really did destroy zzz/f"
    );
    assert!(
        matches!(outcome, MutationOutcome::OutcomeUnknown { .. }),
        "work was destroyed, so this may not be reported as a change that did not happen: {outcome:?}"
    );
}

/// `git restore --staged` and `git rm --cached` build the whole index in core
/// and write it once, after the last pathspec, so a refusal leaves every other
/// path exactly as staged as it was — the opposite of `git add`. Classifying
/// an unstage as incremental sent the user to reconcile an index Git never
/// wrote.
#[test]
fn a_refused_unstage_wrote_no_part_of_the_index() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.write("b.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");
    fixture.write("a.txt", b"two\n");
    fixture.write("b.txt", b"two\n");
    fixture.git_in_work(&["add", "-A"]);

    // Git's own behaviour, pinned: exit 1, and a.txt is still staged.
    let (code, stderr) = fixture.run_failing_status(
        &fixture.work(),
        &["restore", "--staged", "--", "a.txt", "no-such-file.txt"],
    );
    assert_eq!(code, Some(1), "git's exit code changed: {stderr}");
    assert!(
        stderr.contains("did not match"),
        "git's reason changed: {stderr}"
    );
    assert_eq!(
        fixture.text(&["diff", "--cached", "--name-only"]),
        "a.txt\nb.txt",
        "the premise: the index was not written at all"
    );

    let outcome = fixture
        .service()
        .unstage(
            &UnstageRequest {
                worktree: fixture.work(),
                selection: vec![
                    PathSelection::new(path("a.txt")),
                    PathSelection::new(path("no-such-file.txt")),
                ],
            },
            &CancellationToken::new(),
        )
        .expect("unstage");
    assert!(
        matches!(outcome, MutationOutcome::NotApplied { .. }),
        "an unstage that wrote no index is not an unknown outcome: {outcome:?}"
    );
    assert_eq!(
        fixture.text(&["diff", "--cached", "--name-only"]),
        "a.txt\nb.txt",
        "and nothing was unstaged, which is why there is nothing to reconcile"
    );
}

/// `prepare-commit-msg` runs for `git commit --file=-`, and prepending is what
/// it is for: a hook putting a ticket key in front of the subject is ordinary
/// furniture in a corporate repository. Requiring the stored message to start
/// with the one this call sent made every commit in such a repository fail —
/// git exited 0, HEAD moved, the commit was real, and the user was told "git
/// reported success but HEAD names a commit this did not write".
#[cfg(unix)]
#[test]
fn a_hook_that_prepends_to_the_message_does_not_fail_the_users_own_commit() {
    use std::os::unix::fs::PermissionsExt;

    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    let first = fixture.commit("initial");

    let hooks = fixture.work().join(".git").join("hooks");
    fs::create_dir_all(&hooks).expect("hooks directory");
    let hook = hooks.join("prepare-commit-msg");
    fs::write(
        &hook,
        b"#!/bin/sh\n{ printf 'PROJ-123: '; cat \"$1\"; } > \"$1.prefixed\"\nmv \"$1.prefixed\" \"$1\"\n",
    )
    .expect("write the hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("make the hook runnable");

    fixture.write("a.txt", b"two\n");
    fixture.git_in_work(&["add", "-A"]);
    let outcome = fixture
        .service()
        .commit(
            &CommitRequest {
                worktree: fixture.work(),
                message: "subject line\n\nbody\n".into(),
            },
            &CancellationToken::new(),
        )
        .expect("commit");

    // The premise: the hook really did rewrite the message Git stored, so the
    // sent message is not a prefix of it.
    assert_eq!(
        fixture.text(&["log", "-1", "--format=%B"]),
        "PROJ-123: subject line\n\nbody"
    );
    let head = fixture.head();
    assert_ne!(head, first, "the commit is real and HEAD moved to it");
    match outcome {
        CommitOutcome::Created { oid, warnings } => {
            assert_eq!(oid, head);
            assert!(warnings.is_empty(), "nothing failed: {warnings:?}");
        }
        other => panic!("the user's own commit was reported as {other:?}"),
    }
}

/// A ref name is bytes, and `git worktree list --porcelain -z` emits an
/// undecodable branch verbatim. Requiring that label to decode failed the
/// entire worktree listing over one worktree's label — and a project load
/// propagates that failure, so the project never opened at all.
#[cfg(unix)]
#[test]
fn a_worktree_on_a_branch_that_is_not_utf8_still_lists_every_worktree() {
    use std::os::unix::ffi::OsStrExt;

    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");

    let side = fixture.root.path().join("side");
    fixture.git_in_work(&[
        OsStr::new("worktree"),
        OsStr::new("add"),
        OsStr::new("--quiet"),
        OsStr::new("-b"),
        OsStr::from_bytes(b"b\xffad"),
        side.as_os_str(),
    ]);
    // The premise: this is what Git puts in the porcelain listing, byte for
    // byte, with no substitution of its own.
    let listing = fixture.git_in_work(&["worktree", "list", "--porcelain", "-z"]);
    assert!(
        listing
            .windows(15)
            .any(|window| window == b"refs/heads/b\xffad"),
        "git no longer emits the raw bytes: {}",
        String::from_utf8_lossy(&listing)
    );

    let worktrees = fixture
        .service()
        .list_worktrees(&fixture.work(), &CancellationToken::new())
        .expect("a branch label is not a broken worktree list");
    assert_eq!(worktrees.len(), 2);
    let lossy = String::from_utf8_lossy(b"refs/heads/b\xffad").into_owned();
    let degraded = worktrees
        .iter()
        .find(|record| record.branch.as_deref() == Some(lossy.as_str()))
        .unwrap_or_else(|| panic!("no worktree carried the lossy label: {worktrees:?}"));
    assert_eq!(degraded.path.file_name(), Some(OsStr::new("side")));
    // The worktree that merely shares the listing is untouched, which is the
    // whole point of degrading a label instead of failing a read.
    assert!(
        worktrees
            .iter()
            .any(|record| record.branch.as_deref() == Some("refs/heads/main")
                && record.head_oid.is_some()),
        "{worktrees:?}"
    );
}

/// The branch label a lossy conversion produces is display data, and what
/// stops it from *acting* is not the name validator: `ValidatedBranchName`
/// screens ASCII and U+FFFD carries no byte it forbids, so the lossy name
/// parses. What refuses it is the existence check `push` makes first, which is
/// therefore load-bearing rather than incidental, and is pinned here so it
/// cannot be dropped as redundant.
#[cfg(unix)]
#[test]
fn a_lossy_branch_label_cannot_act_on_the_branch_it_names() {
    use std::os::unix::ffi::OsStrExt;

    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");
    fixture.git_in_work(&[
        OsStr::new("checkout"),
        OsStr::new("--quiet"),
        OsStr::new("-b"),
        OsStr::from_bytes(b"b\xffad"),
    ]);
    let origin = fixture.origin();
    fixture.git_in_work(&[
        OsStr::new("remote"),
        OsStr::new("add"),
        OsStr::new("origin"),
        origin.as_os_str(),
    ]);

    let label = fixture
        .snapshot()
        .status
        .branch()
        .expect("a checked-out branch has a label")
        .to_owned();
    assert_eq!(label, String::from_utf8_lossy(b"b\xffad"));
    // The validator accepts it: nothing about U+FFFD is an ASCII byte it
    // screens for, so this is not what keeps the name out of a refspec.
    let branch = ValidatedBranchName::parse(label.clone()).expect("the ASCII screen accepts it");

    let outcome = fixture
        .service()
        .push(
            &PushRequest {
                worktree: fixture.work(),
                branch,
                target: PushTarget::Upstream {
                    remote: "origin".into(),
                    remote_ref: ValidatedRefName::parse("refs/heads/main").expect("a ref name"),
                },
            },
            &CancellationToken::new(),
        )
        .expect("push");
    match outcome {
        PushOutcome::NotPushed {
            failure: MutationFailure::Rejected { reason },
        } => {
            assert!(reason.contains("does not exist"), "{reason}");
            assert!(reason.starts_with("refs/heads/b"), "{reason}");
        }
        other => panic!("a name naming no ref reached Git as {other:?}"),
    }
    // Nothing left the machine, and the branch Git really has is still there
    // under the bytes Git really has for it.
    assert!(
        fixture.run(&origin, &["for-each-ref"]).is_empty(),
        "the remote was updated by a push that should never have been spawned"
    );
    assert_eq!(
        fixture.git_in_work(&["symbolic-ref", "HEAD"]),
        b"refs/heads/b\xffad\n".to_vec()
    );
}

/// `git remote add te/am` is legal, and it makes `refs/remotes/te/am/main`
/// ambiguous: `main` on `te/am`, or `am/main` on `te`. The remote's own
/// symbolic `HEAD` is where the boundary is not ambiguous, and both halves of
/// the ref listing are split there — because they exist to be joined on that
/// field. Splitting the branch records at the first slash instead made them
/// say `te` while the head record said `te/am`, and the consumer that asks a
/// branch's remote for its default branch fell through to origin's.
#[test]
fn a_remote_whose_name_contains_a_slash_is_named_the_same_way_by_both_halves() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture.write("a.txt", b"one\n");
    fixture.git_in_work(&["add", "-A"]);
    fixture.commit("initial");
    let origin = fixture.origin();
    fixture.git_in_work(&[
        OsStr::new("remote"),
        OsStr::new("add"),
        OsStr::new("te/am"),
        origin.as_os_str(),
    ]);
    fixture.git_in_work(&["push", "--quiet", "te/am", "main"]);
    fixture.git_in_work(&["fetch", "--quiet", "te/am"]);
    fixture.git_in_work(&["remote", "set-head", "te/am", "main"]);
    fixture.git_in_work(&["branch", "--set-upstream-to", "te/am/main", "main"]);

    let snapshot = fixture.snapshot();
    let head = snapshot
        .remote_heads
        .iter()
        .find(|head| head.remote == "te/am")
        .unwrap_or_else(|| panic!("{:?}", snapshot.remote_heads));
    assert_eq!(head.target.as_str(), "refs/remotes/te/am/main");
    let record = snapshot
        .remote_refs
        .iter()
        .find(|record| record.name.as_str() == "refs/remotes/te/am/main")
        .unwrap_or_else(|| panic!("{:?}", snapshot.remote_refs));
    assert_eq!(record.remote, "te/am");
    assert_eq!(record.branch, "main");

    // End to end through the consumer these two fields exist for: the branch's
    // upstream names its remote, and that remote names its default branch.
    assert_eq!(
        snapshot
            .default_branch_base()
            .map(|name| name.as_str().to_owned()),
        Some("refs/remotes/te/am/main".to_owned())
    );
}

/// Git resolves symbolic links when it reports an absolute path, so a
/// comparison against a temporary directory's path has to do the same.
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|error| panic!("canonicalize {path:?}: {error}"))
}
