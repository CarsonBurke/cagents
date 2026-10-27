use super::*;
use crate::process::CancellationToken;
use std::path::Path;
use std::time::Duration;

fn command(directory: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        // Detached auto-maintenance would race the object-store assertions.
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
            "-C",
        ])
        .arg(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@localhost")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@localhost");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        command.env_remove(name);
    }
    command
}

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = command(directory).args(arguments).output().unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

/// A repository on `main` with `a.txt`, `d/c.txt` and `e/x.txt`, and a linked
/// worktree `wt` on `feature` at the same commit.
struct Fixture {
    _root: tempfile::TempDir,
    repository: PathBuf,
    worktree: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::build("repository", &[])
    }

    fn with_init(options: &[&str]) -> Self {
        Self::build("repository", options)
    }

    fn build(name: &str, options: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let repository = base.join(name);
        let mut init = vec!["init", "-q", "-b", "main"];
        init.extend_from_slice(options);
        init.push(repository.to_str().unwrap());
        git(&base, &init);
        write(&repository, "a.txt", "a\n");
        write(&repository, "d/c.txt", "c\n");
        write(&repository, "e/x.txt", "x\n");
        commit(&repository, "base");
        let worktree = base.join("wt");
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        Self {
            _root: root,
            repository,
            worktree,
        }
    }

    fn request(&self) -> Request {
        Request {
            path: self.worktree.clone(),
            targets: Vec::new(),
            merged_head: None,
        }
    }

    fn inspect(&self) -> Inspection {
        self.inspect_with(&self.request())
    }

    fn inspect_with(&self, request: &Request) -> Inspection {
        inspect(request, &CancellationToken::new()).unwrap()
    }

    fn admin(&self) -> PathBuf {
        PathBuf::from(git(&self.worktree, &["rev-parse", "--absolute-git-dir"]))
    }

    fn head(&self) -> String {
        git(&self.worktree, &["rev-parse", "HEAD"])
    }

    /// Lands everything `feature` adds onto `main` as one squash commit.
    fn squash(&self) {
        git(&self.repository, &["merge", "-q", "--squash", "feature"]);
        git(&self.repository, &["commit", "-q", "-m", "squash feature"]);
    }
}

fn write(directory: &Path, path: &str, content: &str) {
    let path = directory.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn commit(directory: &Path, message: &str) {
    git(directory, &["add", "-A"]);
    git(directory, &["commit", "-q", "-m", message]);
}

fn blocker(inspection: &Inspection) -> &Blocker {
    match &inspection.verdict {
        Verdict::Blocked(blocker) => blocker,
        verdict => panic!("expected a refusal, got {verdict:?}"),
    }
}

const UNSAVED: Verdict = Verdict::Unsaved {
    uncommitted: true,
    unreachable: false,
    beyond_snapshot: false,
};
const DISCARD_ONLY: Verdict = Verdict::Unsaved {
    uncommitted: true,
    unreachable: false,
    beyond_snapshot: true,
};

#[test]
fn a_squash_merged_branch_is_landed() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "feature.txt", "one\n");
    commit(&fixture.worktree, "one");
    write(&fixture.worktree, "a.txt", "a2\n");
    commit(&fixture.worktree, "two");
    fixture.squash();

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, Verdict::Landed);
    assert_eq!(inspection.landed_in.as_deref(), Some("refs/heads/main"));
    assert_eq!(inspection.branch.as_deref(), Some("feature"));
    assert_eq!(inspection.head, Some(fixture.head()));
    assert!(inspection.token.is_some());
}

#[test]
fn a_rebase_merged_branch_is_landed() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "feature.txt", "one\n");
    commit(&fixture.worktree, "one");
    write(&fixture.worktree, "a.txt", "a2\n");
    commit(&fixture.worktree, "two");
    write(&fixture.repository, "main.txt", "main\n");
    commit(&fixture.repository, "main moves on");
    git(&fixture.repository, &["cherry-pick", "main..feature"]);

    assert_eq!(fixture.inspect().verdict, Verdict::Landed);
}

#[test]
fn a_merged_then_reverted_branch_is_landed_by_ancestry() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "feature.txt", "one\n");
    commit(&fixture.worktree, "one");
    write(&fixture.repository, "main.txt", "main\n");
    commit(&fixture.repository, "main moves on");
    git(
        &fixture.repository,
        &["merge", "-q", "--no-ff", "-m", "merge", "feature"],
    );
    git(
        &fixture.repository,
        &["revert", "--no-edit", "-m", "1", "HEAD"],
    );

    // The commits stay in the target's history, where a revert can be undone.
    assert_eq!(fixture.inspect().verdict, Verdict::Landed);
}

#[test]
fn a_reverted_squash_keeps_the_commits() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "feature.txt", "one\n");
    commit(&fixture.worktree, "one");
    fixture.squash();
    git(&fixture.repository, &["revert", "--no-edit", "HEAD"]);

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, Verdict::CommitsKept);
    assert_eq!(inspection.landed_in, None);
}

#[test]
fn a_squash_rewritten_since_on_the_target_is_not_proven() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "a.txt", "feature\n");
    commit(&fixture.worktree, "one");
    fixture.squash();
    write(&fixture.repository, "a.txt", "rewritten\n");
    commit(&fixture.repository, "rewrite");

    assert_eq!(fixture.inspect().verdict, Verdict::CommitsKept);
}

#[test]
fn a_partly_landed_branch_keeps_the_commits() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "one.txt", "one\n");
    commit(&fixture.worktree, "one");
    let first = fixture.head();
    write(&fixture.worktree, "two.txt", "two\n");
    commit(&fixture.worktree, "two");
    git(&fixture.repository, &["cherry-pick", &first]);

    assert_eq!(fixture.inspect().verdict, Verdict::CommitsKept);
}

#[test]
fn a_clean_unlanded_branch_keeps_the_commits() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "one.txt", "one\n");
    commit(&fixture.worktree, "one");

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, Verdict::CommitsKept);
    assert_eq!((inspection.staged, inspection.modified), (0, 0));
}

#[test]
fn uncommitted_changes_already_on_the_target_are_landed() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "one.txt", "one\n");
    commit(&fixture.worktree, "one");
    fixture.squash();
    write(&fixture.repository, "later.txt", "later\n");
    write(&fixture.repository, "a.txt", "a2\n");
    commit(&fixture.repository, "later");
    write(&fixture.worktree, "later.txt", "later\n");
    write(&fixture.worktree, "a.txt", "a2\n");

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, Verdict::Landed);
    assert_eq!((inspection.modified, inspection.untracked), (1, 1));
}

#[test]
fn uncommitted_changes_missing_from_the_target_are_unsaved() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "one.txt", "one\n");
    commit(&fixture.worktree, "one");
    fixture.squash();
    write(&fixture.worktree, "a.txt", "edited\n");
    git(&fixture.worktree, &["add", "a.txt"]);
    write(&fixture.worktree, "d/c.txt", "edited\n");

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, UNSAVED);
    assert_eq!((inspection.staged, inspection.modified), (1, 1));
}

#[test]
fn undoing_a_landed_commit_in_the_worktree_is_unsaved() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "a.txt", "a2\n");
    commit(&fixture.worktree, "change");
    fixture.squash();
    // The target has the change; the worktree's edit undoes it.
    write(&fixture.worktree, "a.txt", "a\n");

    assert_eq!(fixture.inspect().verdict, UNSAVED);
}

#[test]
fn untracked_files_are_unsaved() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "notes/new.txt", "twelve bytes");

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, UNSAVED);
    assert_eq!(inspection.untracked, 1);
    assert_eq!(inspection.untracked_bytes, 12);
}

#[test]
fn ignored_files_are_listed_and_bound_by_the_token() {
    let fixture = Fixture::new();
    write(&fixture.repository, ".gitignore", "*.log\nbuild/\n");
    commit(&fixture.repository, "ignore");
    git(&fixture.worktree, &["merge", "-q", "--ff-only", "main"]);
    write(&fixture.worktree, "build/out/x.o", "x");
    write(&fixture.worktree, "a.log", "log");

    let first = fixture.inspect();
    assert_eq!(first.verdict, Verdict::Landed);
    assert_eq!(first.ignored_total, 2);
    assert!(first.ignored.contains(&IgnoredEntry {
        path: "a.log".into(),
        directory: false
    }));
    assert!(first.ignored.contains(&IgnoredEntry {
        path: "build/".into(),
        directory: true
    }));

    write(&fixture.worktree, "b.log", "log");
    let second = fixture.inspect();
    assert_eq!(second.verdict, Verdict::Landed);
    assert_ne!(first.token, second.token);
}

#[test]
fn a_checked_out_submodule_blocks() {
    let fixture = Fixture::new();
    let head = fixture.head();
    git(
        &fixture.worktree,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},sub"),
        ],
    );
    git(&fixture.worktree, &["commit", "-q", "-m", "gitlink"]);
    std::fs::create_dir_all(fixture.worktree.join("sub")).unwrap();
    assert!(!matches!(fixture.inspect().verdict, Verdict::Blocked(_)));

    std::fs::create_dir_all(fixture.worktree.join("sub/.git")).unwrap();
    assert_eq!(
        blocker(&fixture.inspect()),
        &Blocker::Submodule("sub".into())
    );
}

#[test]
fn a_nested_repository_is_unsaved() {
    let fixture = Fixture::new();
    let nested = fixture.worktree.join("nested");
    git(&fixture.worktree, &["init", "-q", nested.to_str().unwrap()]);
    write(&nested, "f.txt", "f");

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, DISCARD_ONLY);
    assert_eq!(inspection.nested, vec!["nested/".to_owned()]);
}

#[test]
fn an_ignored_nested_repository_is_unsaved() {
    let fixture = Fixture::new();
    write(&fixture.worktree, ".gitignore", "vendor/\n");
    commit(&fixture.worktree, "ignore vendor");
    let nested = fixture.worktree.join("vendor");
    git(&fixture.worktree, &["init", "-q", nested.to_str().unwrap()]);

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, DISCARD_ONLY);
    assert_eq!(inspection.nested, vec!["vendor/".to_owned()]);
}

#[test]
fn files_outside_a_sparse_checkout_are_not_deletions() {
    let fixture = Fixture::new();
    git(&fixture.worktree, &["sparse-checkout", "set", "d"]);
    assert!(!fixture.worktree.join("e/x.txt").exists());

    let clean = fixture.inspect();
    assert_eq!(clean.verdict, Verdict::Landed);
    assert_eq!((clean.staged, clean.modified), (0, 0));

    write(&fixture.worktree, "e/new.txt", "new\n");
    let dirty = fixture.inspect();
    assert_eq!(dirty.verdict, UNSAVED);
    assert_eq!(dirty.untracked, 1);
}

#[test]
fn assume_unchanged_edits_are_seen() {
    let fixture = Fixture::new();
    git(
        &fixture.worktree,
        &["update-index", "--assume-unchanged", "a.txt"],
    );
    write(&fixture.worktree, "a.txt", "hidden edit\n");
    assert_eq!(git(&fixture.worktree, &["status", "--porcelain"]), "");

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, UNSAVED);
    assert_eq!(inspection.modified, 1);
    // The real index keeps its flag.
    assert!(git(&fixture.worktree, &["ls-files", "-v", "a.txt"]).starts_with("h "));
}

#[test]
fn a_racily_clean_edit_is_seen() {
    let fixture = Fixture::new();
    git(&fixture.repository, &["config", "core.trustctime", "false"]);
    git(
        &fixture.repository,
        &["config", "core.checkStat", "minimal"],
    );
    let file = fixture.worktree.join("a.txt");
    let past = std::time::SystemTime::UNIX_EPOCH
        + Duration::from_secs(
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - 100,
        );
    let set_mtime = |path: &Path| {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(past)
            .unwrap();
    };
    set_mtime(&file);
    git(&fixture.worktree, &["update-index", "--refresh"]);
    // Same size and mtime: only the index being no newer than the entry
    // makes Git look at the content.
    std::fs::write(&file, "b\n").unwrap();
    set_mtime(&file);
    set_mtime(&fixture.admin().join("index"));

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, UNSAVED);
    assert_eq!(inspection.modified, 1);
}

#[test]
fn a_detached_head_on_the_target_is_landed() {
    let fixture = Fixture::new();
    git(&fixture.worktree, &["checkout", "-q", "--detach", "main"]);

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, Verdict::Landed);
    assert_eq!(inspection.branch, None);
}

#[test]
fn detached_commits_another_ref_reaches_are_kept() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "one.txt", "one\n");
    commit(&fixture.worktree, "one");
    git(&fixture.worktree, &["checkout", "-q", "--detach"]);

    assert_eq!(fixture.inspect().verdict, Verdict::CommitsKept);
}

#[test]
fn detached_commits_no_ref_reaches_are_unsaved() {
    let fixture = Fixture::new();
    git(&fixture.worktree, &["checkout", "-q", "--detach"]);
    write(&fixture.worktree, "one.txt", "one\n");
    commit(&fixture.worktree, "one");

    assert_eq!(
        fixture.inspect().verdict,
        Verdict::Unsaved {
            uncommitted: false,
            unreachable: true,
            beyond_snapshot: false,
        }
    );
}

#[test]
fn in_progress_operations_block() {
    let fixture = Fixture::new();
    let admin = fixture.admin();
    for (name, operation) in [
        ("MERGE_HEAD", Operation::Merge),
        ("rebase-merge", Operation::Rebase),
        ("rebase-apply", Operation::Rebase),
        ("CHERRY_PICK_HEAD", Operation::CherryPick),
        ("REVERT_HEAD", Operation::Revert),
        ("BISECT_LOG", Operation::Bisect),
        ("sequencer", Operation::Sequencer),
    ] {
        let marker = admin.join(name);
        std::fs::write(&marker, "").unwrap();
        assert_eq!(
            blocker(&fixture.inspect()),
            &Blocker::Operation(operation),
            "{name}"
        );
        std::fs::remove_file(marker).unwrap();
    }
}

#[test]
fn unmerged_paths_block() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "a.txt", "feature\n");
    commit(&fixture.worktree, "feature");
    write(&fixture.repository, "a.txt", "main\n");
    commit(&fixture.repository, "main");
    let merge = command(&fixture.worktree)
        .args(["merge", "-q", "main"])
        .output()
        .unwrap();
    assert!(!merge.status.success());
    assert_eq!(
        blocker(&fixture.inspect()),
        &Blocker::Operation(Operation::Merge)
    );

    std::fs::remove_file(fixture.admin().join("MERGE_HEAD")).unwrap();
    assert_eq!(blocker(&fixture.inspect()), &Blocker::Unmerged);
}

#[test]
fn a_custom_merge_driver_voids_the_proof() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "a.txt", "feature\n");
    commit(&fixture.worktree, "feature");
    write(&fixture.repository, "a.txt", "main\n");
    commit(&fixture.repository, "main");
    git(
        &fixture.repository,
        &["config", "merge.ours.driver", "true"],
    );
    write(
        &fixture.repository.join(".git/info"),
        "attributes",
        "* merge=ours\n",
    );
    // The trap is armed: merging the branch into main claims to change nothing.
    let main_tree = git(&fixture.repository, &["rev-parse", "main^{tree}"]);
    let merged = git(
        &fixture.repository,
        &["merge-tree", "--write-tree", "main", "feature"],
    );
    assert_eq!(merged, main_tree);

    assert_eq!(fixture.inspect().verdict, Verdict::CommitsKept);
}

#[test]
fn primary_and_locked_worktrees_block() {
    let fixture = Fixture::new();
    let primary = Request {
        path: fixture.repository.clone(),
        ..fixture.request()
    };
    assert_eq!(blocker(&fixture.inspect_with(&primary)), &Blocker::Primary);

    git(
        &fixture.repository,
        &["worktree", "lock", fixture.worktree.to_str().unwrap()],
    );
    assert_eq!(blocker(&fixture.inspect()), &Blocker::Locked);
}

#[test]
fn per_worktree_refs_block() {
    let fixture = Fixture::new();
    git(
        &fixture.worktree,
        &["update-ref", "refs/worktree/keep", "HEAD"],
    );

    assert_eq!(
        blocker(&fixture.inspect()),
        &Blocker::WorktreeRefs("refs/worktree/keep".into())
    );
}

#[test]
fn a_present_skip_worktree_file_blocks() {
    let fixture = Fixture::new();
    git(
        &fixture.worktree,
        &["update-index", "--skip-worktree", "a.txt"],
    );

    assert_eq!(
        blocker(&fixture.inspect()),
        &Blocker::SkipWorktree("a.txt".into())
    );
}

#[test]
fn reftable_repositories_block() {
    let fixture = Fixture::with_init(&["--ref-format=reftable"]);

    assert_eq!(blocker(&fixture.inspect()), &Blocker::RefStorage);
}

/// Every file in the repository's object store.
fn stored_objects(repository: &Path) -> std::collections::BTreeSet<PathBuf> {
    fn collect(directory: &Path, files: &mut std::collections::BTreeSet<PathBuf>) {
        for entry in std::fs::read_dir(directory).unwrap().flatten() {
            if entry.file_type().unwrap().is_dir() {
                collect(&entry.path(), files);
            } else {
                files.insert(entry.path());
            }
        }
    }
    let mut files = std::collections::BTreeSet::new();
    collect(&repository.join(".git/objects"), &mut files);
    files
}

#[test]
fn inspection_is_deterministic_and_writes_nothing() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "one.txt", "one\n");
    commit(&fixture.worktree, "one");
    fixture.squash();
    write(&fixture.worktree, "a.txt", "edited\n");
    write(&fixture.worktree, "new.txt", "new\n");
    let objects = stored_objects(&fixture.repository);
    let index = std::fs::read(fixture.admin().join("index")).unwrap();

    let first = fixture.inspect();
    let second = fixture.inspect();
    assert_eq!(first, second);
    let added = stored_objects(&fixture.repository)
        .difference(&objects)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(added, Vec::<PathBuf>::new());
    assert_eq!(std::fs::read(fixture.admin().join("index")).unwrap(), index);
    let scratch = std::fs::read_dir(fixture.repository.join(".git"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("cagents-"))
        .count();
    assert_eq!(scratch, 0);
}

#[test]
fn the_token_follows_uncommitted_content() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "a.txt", "one\n");
    let first = fixture.inspect();
    write(&fixture.worktree, "a.txt", "two\n");
    let second = fixture.inspect();

    assert_eq!(first.verdict, second.verdict);
    assert_ne!(first.token, second.token);
}

#[test]
fn a_merged_pull_request_head_lands_the_commits() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "a.txt", "feature\n");
    commit(&fixture.worktree, "feature");
    // Merged with an edit on top, so no merge proof holds.
    write(&fixture.repository, "a.txt", "feature, amended\n");
    commit(&fixture.repository, "merge #1");
    assert_eq!(fixture.inspect().verdict, Verdict::CommitsKept);

    let request = Request {
        merged_head: Some(fixture.head()),
        ..fixture.request()
    };
    assert_eq!(fixture.inspect_with(&request).verdict, Verdict::Landed);
}

#[test]
fn a_further_target_lands_the_branch() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "one.txt", "one\n");
    commit(&fixture.worktree, "one");
    git(&fixture.repository, &["checkout", "-q", "-b", "release"]);
    fixture.squash();
    git(&fixture.repository, &["checkout", "-q", "main"]);
    assert_eq!(fixture.inspect().verdict, Verdict::CommitsKept);

    let request = Request {
        targets: vec!["refs/heads/release".into()],
        ..fixture.request()
    };
    let inspection = fixture.inspect_with(&request);
    assert_eq!(inspection.verdict, Verdict::Landed);
    assert_eq!(inspection.landed_in.as_deref(), Some("refs/heads/release"));
}

#[test]
fn a_snapshot_restores_index_worktree_and_untracked_files() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "a.txt", "staged\n");
    git(&fixture.worktree, &["add", "a.txt"]);
    write(&fixture.worktree, "a.txt", "unstaged\n");
    write(&fixture.worktree, "notes/new.txt", "new\n");
    let head = fixture.head();
    let request = fixture.request();
    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, UNSAVED);
    let cancellation = CancellationToken::new();
    assert!(matches!(
        snapshot(&request, "stale", &cancellation),
        Err(ContentError::Changed)
    ));

    let saved = snapshot(
        &request,
        inspection.token.as_deref().unwrap(),
        &cancellation,
    )
    .unwrap();
    assert!(saved.refname.starts_with(SNAPSHOT_REFS));
    assert_eq!(
        git(&fixture.repository, &["rev-parse", &saved.refname]),
        saved.oid
    );
    // The snapshot's objects went to the repository, so inspecting again is
    // still the same plan.
    assert_eq!(fixture.inspect(), inspection);

    git(
        &fixture.repository,
        &[
            "worktree",
            "remove",
            "--force",
            fixture.worktree.to_str().unwrap(),
        ],
    );
    let restored = fixture.repository.with_file_name("restored");
    git(
        &fixture.repository,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            restored.to_str().unwrap(),
            &head,
        ],
    );
    git(&restored, &["stash", "apply", "--index", &saved.refname]);
    assert_eq!(
        std::fs::read_to_string(restored.join("a.txt")).unwrap(),
        "unstaged\n"
    );
    assert_eq!(git(&restored, &["show", ":a.txt"]), "staged");
    assert_eq!(
        std::fs::read_to_string(restored.join("notes/new.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        git(
            &restored,
            &["status", "--porcelain", "--untracked-files=all"]
        ),
        "MM a.txt\n?? notes/new.txt"
    );
}

#[test]
fn a_snapshot_of_a_blocked_worktree_is_refused() {
    let fixture = Fixture::new();
    git(
        &fixture.worktree,
        &["update-ref", "refs/worktree/keep", "HEAD"],
    );

    assert!(matches!(
        snapshot(&fixture.request(), "any", &CancellationToken::new()),
        Err(ContentError::Blocked(Blocker::WorktreeRefs(_)))
    ));
}

#[test]
fn only_a_canonical_top_level_is_inspected() {
    let fixture = Fixture::new();
    let cancellation = CancellationToken::new();
    let link = fixture.worktree.with_file_name("link");
    std::os::unix::fs::symlink(&fixture.worktree, &link).unwrap();
    let linked = Request {
        path: link,
        ..fixture.request()
    };
    assert!(matches!(
        inspect(&linked, &cancellation),
        Err(ContentError::Path(_))
    ));
    let nested = Request {
        path: fixture.worktree.join("d"),
        ..fixture.request()
    };
    assert!(matches!(
        inspect(&nested, &cancellation),
        Err(ContentError::NotTopLevel(_))
    ));
}

#[test]
fn git_before_2_40_is_unsupported() {
    assert!(analysis::supported("git version 2.40.0"));
    assert!(analysis::supported("git version 3.0.1.windows.1"));
    assert!(!analysis::supported("git version 2.39.5 (Apple Git-154)"));
    assert!(!analysis::supported("not git"));
}

#[test]
fn a_worktree_nested_in_an_ignored_directory_blocks() {
    let fixture = Fixture::new();
    write(&fixture.worktree, ".gitignore", ".worktrees/\n");
    commit(&fixture.worktree, "ignore worktrees");
    let inner = fixture.worktree.join(".worktrees/inner");
    git(
        &fixture.worktree,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "inner",
            inner.to_str().unwrap(),
        ],
    );

    assert_eq!(
        blocker(&fixture.inspect()),
        &Blocker::NestedWorktree(".worktrees/inner/".into())
    );
}

#[test]
fn a_repository_deep_in_an_ignored_directory_is_unsaved() {
    let fixture = Fixture::new();
    write(&fixture.worktree, ".gitignore", "build/\n");
    commit(&fixture.worktree, "ignore build");
    let nested = fixture.worktree.join("build/vendor/dependency");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-q"]);

    let first = fixture.inspect();
    assert_eq!(first.verdict, DISCARD_ONLY);
    assert_eq!(first.nested, vec!["build/vendor/dependency/".to_owned()]);
    // Its content is not bound, but its presence is.
    std::fs::remove_dir_all(nested.join(".git")).unwrap();
    assert_ne!(fixture.inspect().token, first.token);
}

#[test]
fn files_in_an_uninitialized_submodule_directory_block() {
    let fixture = Fixture::new();
    let head = fixture.head();
    git(
        &fixture.worktree,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},sub"),
        ],
    );
    git(&fixture.worktree, &["commit", "-q", "-m", "gitlink"]);
    write(&fixture.worktree, "sub/notes.txt", "invisible to Git\n");

    assert_eq!(
        blocker(&fixture.inspect()),
        &Blocker::Submodule("sub".into())
    );
}

#[test]
fn content_a_clean_filter_drops_is_unsaved() {
    let fixture = Fixture::new();
    git(
        &fixture.repository,
        &["config", "filter.strip.clean", "grep -v '^OUTPUT'"],
    );
    git(
        &fixture.repository,
        &["config", "filter.strip.smudge", "cat"],
    );
    write(&fixture.worktree, ".gitattributes", "*.nb filter=strip\n");
    write(&fixture.worktree, "n.nb", "cell\n");
    commit(&fixture.worktree, "notebook");
    git(
        &fixture.repository,
        &["merge", "-q", "--ff-only", "feature"],
    );
    assert_eq!(fixture.inspect().verdict, Verdict::Landed);

    write(
        &fixture.worktree,
        "n.nb",
        "cell\nOUTPUT computed for hours\n",
    );
    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, DISCARD_ONLY);
    assert_eq!(inspection.filtered, vec!["n.nb".to_owned()]);
}

#[test]
fn a_lossless_filter_does_not_count_as_changes() {
    let fixture = Fixture::new();
    git(&fixture.repository, &["config", "filter.same.clean", "cat"]);
    git(
        &fixture.repository,
        &["config", "filter.same.smudge", "cat"],
    );
    write(&fixture.worktree, ".gitattributes", "*.bin filter=same\n");
    write(&fixture.worktree, "data.bin", "data\n");
    commit(&fixture.worktree, "data");
    fixture.squash();

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, Verdict::Landed);
    assert!(inspection.filtered.is_empty());
}

#[test]
fn staged_content_the_worktree_reverted_is_unsaved() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "a.txt", "staged\n");
    git(&fixture.worktree, &["add", "a.txt"]);
    write(&fixture.worktree, "a.txt", "a\n");

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, UNSAVED);
    assert_eq!((inspection.staged, inspection.modified), (1, 1));
}

#[test]
fn a_custom_driver_behind_a_builtin_name_voids_the_proof() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "a.txt", "feature\n");
    commit(&fixture.worktree, "feature");
    write(&fixture.repository, "a.txt", "main\n");
    commit(&fixture.repository, "main");
    git(
        &fixture.repository,
        &["config", "merge.text.driver", "true"],
    );
    write(
        &fixture.repository.join(".git/info"),
        "attributes",
        "* merge=text\n",
    );

    assert_eq!(fixture.inspect().verdict, Verdict::CommitsKept);
}

#[test]
fn an_undefined_merge_driver_falls_back_to_text() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "one.txt", "one\n");
    commit(&fixture.worktree, "one");
    fixture.squash();
    // Git LFS's attribute without its driver configured merges as text.
    write(
        &fixture.repository.join(".git/info"),
        "attributes",
        "* merge=lfs\n",
    );

    assert_eq!(fixture.inspect().verdict, Verdict::Landed);
}

#[test]
fn files_differing_only_in_case_are_seen() {
    let fixture = Fixture::new();
    git(&fixture.repository, &["config", "core.ignoreCase", "true"]);
    write(&fixture.worktree, "A.txt", "another file\n");

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, UNSAVED);
    assert_eq!(inspection.untracked, 1);
}

#[test]
fn replace_refs_cannot_fake_ancestry() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "one.txt", "one\n");
    commit(&fixture.worktree, "one");
    let head = fixture.head();
    let main = git(&fixture.repository, &["rev-parse", "main"]);
    let fake = git(
        &fixture.repository,
        &[
            "commit-tree",
            "main^{tree}",
            "-p",
            &main,
            "-p",
            &head,
            "-m",
            "fake",
        ],
    );
    git(&fixture.repository, &["replace", &main, &fake]);
    assert!(
        command(&fixture.repository)
            .args(["merge-base", "--is-ancestor", &head, "main"])
            .status()
            .unwrap()
            .success()
    );

    assert_eq!(fixture.inspect().verdict, Verdict::CommitsKept);
}

#[test]
fn a_repository_path_with_a_colon_is_inspected() {
    let fixture = Fixture::build("co:lon", &[]);
    write(&fixture.worktree, "new.txt", "new\n");

    assert_eq!(fixture.inspect().verdict, UNSAVED);
}

#[test]
fn an_unremovable_directory_blocks() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let locked = fixture.worktree.join("locked");
    write(&locked, "secret.txt", "secret\n");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let inspection = fixture.inspect();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(
        blocker(&inspection),
        &Blocker::Unremovable("locked/".into())
    );
}

/// Mounts a file system inside the worktree, in a mount namespace of a child
/// test process where unprivileged namespaces allow.
#[test]
fn a_mount_inside_the_worktree_blocks() {
    const CHILD: &str = "CAGENTS_CONTENT_MOUNT_TEST";
    let unshare = || {
        let mut command = std::process::Command::new("unshare");
        command.args(["--user", "--map-root-user", "--mount"]);
        command
    };
    if std::env::var_os(CHILD).is_none() {
        if !unshare()
            .arg("true")
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("skipped: no unprivileged mount namespace");
            return;
        }
        let name = module_path!().split_once("::").unwrap().1;
        let output = unshare()
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("{name}::a_mount_inside_the_worktree_blocks"),
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "the namespaced fixture failed: {stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let fixture = Fixture::new();
    let mount = fixture.worktree.join("cache/mounted");
    std::fs::create_dir_all(&mount).unwrap();
    let status = std::process::Command::new("mount")
        .args(["-t", "tmpfs", "-o", "size=1m", "fixture"])
        .arg(&mount)
        .status()
        .unwrap();
    assert!(status.success());

    assert_eq!(
        blocker(&fixture.inspect()),
        &Blocker::MountPoint("cache/mounted/".into())
    );

    // A bind mount of the same file system has the same device; only its
    // mount, which kernels report since 5.8, tells it apart.
    if analysis::filesystem(&fixture.worktree)
        .unwrap()
        .is_none_or(|(_, _, mount)| mount.is_none())
    {
        eprintln!("bind mounts skipped: the kernel reports no mount IDs");
        return;
    }
    let mount_command = |arguments: &[&std::ffi::OsStr]| {
        let status = std::process::Command::new(arguments[0])
            .args(&arguments[1..])
            .status()
            .unwrap();
        assert!(status.success(), "{arguments:?}");
    };
    mount_command(&["umount".as_ref(), mount.as_os_str()]);
    let elsewhere = fixture.repository.parent().unwrap().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    mount_command(&[
        "mount".as_ref(),
        "--bind".as_ref(),
        elsewhere.as_os_str(),
        mount.as_os_str(),
    ]);
    assert_eq!(
        blocker(&fixture.inspect()),
        &Blocker::MountPoint("cache/mounted/".into())
    );
}

#[test]
fn a_remote_tracking_ref_does_not_keep_detached_commits() {
    let fixture = Fixture::new();
    git(&fixture.worktree, &["checkout", "-q", "--detach"]);
    write(&fixture.worktree, "one.txt", "one\n");
    commit(&fixture.worktree, "one");
    git(
        &fixture.repository,
        &["update-ref", "refs/remotes/origin/gone", &fixture.head()],
    );

    assert_eq!(
        fixture.inspect().verdict,
        Verdict::Unsaved {
            uncommitted: false,
            unreachable: true,
            beyond_snapshot: false,
        }
    );
}

#[test]
fn a_driver_named_like_an_attribute_state_voids_the_proof() {
    let fixture = Fixture::new();
    write(&fixture.worktree, "a.txt", "feature\n");
    commit(&fixture.worktree, "feature");
    write(&fixture.repository, "a.txt", "main\n");
    commit(&fixture.repository, "main");
    git(
        &fixture.repository,
        &["config", "merge.unspecified.driver", "true"],
    );
    write(
        &fixture.repository.join(".git/info"),
        "attributes",
        "* merge=unspecified\n",
    );

    assert_eq!(fixture.inspect().verdict, Verdict::CommitsKept);
}

/// A stand-in for Git LFS's clean filter: the pointer of what it reads.
const LFS_CLEAN: &str = "sh -c 'f=$(mktemp); cat >\"$f\"; \
    printf \"version https://git-lfs.github.com/spec/v1\\noid sha256:%s\\nsize %s\\n\" \
    \"$(sha256sum \"$f\" | cut -d\" \" -f1)\" \"$(wc -c <\"$f\" | tr -d \" \")\"; rm \"$f\"'";

#[test]
fn lfs_files_are_judged_by_their_pointer_without_smudging() {
    let fixture = Fixture::new();
    git(
        &fixture.repository,
        &["config", "filter.lfs.clean", LFS_CLEAN],
    );
    git(&fixture.repository, &["config", "filter.lfs.smudge", "cat"]);
    write(&fixture.worktree, ".gitattributes", "*.bin filter=lfs\n");
    write(&fixture.worktree, "asset.bin", "large asset\n");
    commit(&fixture.worktree, "asset");
    let pointer = git(&fixture.worktree, &["cat-file", "blob", "HEAD:asset.bin"]);
    assert!(pointer.starts_with("version https://git-lfs"));
    // A smudge would fetch a missing object; inspection never runs one.
    git(
        &fixture.repository,
        &["config", "filter.lfs.smudge", "false"],
    );

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, Verdict::CommitsKept);
    assert!(inspection.filtered.is_empty());

    // A checkout that skipped smudging holds the pointer itself.
    write(&fixture.worktree, "asset.bin", &format!("{pointer}\n"));
    assert_eq!(fixture.inspect().verdict, Verdict::CommitsKept);
}

#[test]
fn a_filtered_file_outside_a_sparse_checkout_is_skipped() {
    let fixture = Fixture::new();
    git(
        &fixture.repository,
        &["config", "filter.strip.clean", "cat"],
    );
    git(
        &fixture.repository,
        &["config", "filter.strip.smudge", "cat"],
    );
    write(&fixture.worktree, ".gitattributes", "*.nb filter=strip\n");
    write(&fixture.worktree, "e/n.nb", "cell\n");
    commit(&fixture.worktree, "notebook");
    git(&fixture.worktree, &["sparse-checkout", "set", "d"]);
    assert!(!fixture.worktree.join("e/n.nb").exists());

    assert_eq!(fixture.inspect().verdict, Verdict::CommitsKept);
}

#[test]
fn a_stale_ident_expansion_is_not_a_change() {
    let fixture = Fixture::new();
    write(&fixture.worktree, ".gitattributes", "*.c ident\n");
    write(&fixture.worktree, "main.c", "/* $Id$ */\n");
    commit(&fixture.worktree, "ident");
    // Git stores the stale expansion as `$Id$`, so the tree is unchanged.
    write(&fixture.worktree, "main.c", "/* $Id: stale $ */\n");

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, Verdict::CommitsKept);
    assert!(inspection.filtered.is_empty());
}

#[test]
fn a_bare_repository_in_an_ignored_directory_is_unsaved() {
    let fixture = Fixture::new();
    write(&fixture.worktree, ".gitignore", "tmp/\n");
    commit(&fixture.worktree, "ignore tmp");
    let bare = fixture.worktree.join("tmp/remote.git");
    git(
        &fixture.worktree,
        &["init", "-q", "--bare", bare.to_str().unwrap()],
    );

    let inspection = fixture.inspect();
    assert_eq!(inspection.verdict, DISCARD_ONLY);
    assert_eq!(inspection.nested, vec!["tmp/remote.git/".to_owned()]);
}

#[test]
fn a_read_only_directory_blocks() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    write(&fixture.worktree, ".gitignore", "cache/\n");
    commit(&fixture.worktree, "ignore cache");
    let cache = fixture.worktree.join("cache/module");
    write(&cache, "module.go", "package module\n");
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o555)).unwrap();
    let inspection = fixture.inspect();
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(
        blocker(&inspection),
        &Blocker::Unremovable("cache/module/".into())
    );
}
