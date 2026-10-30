use super::*;
use crate::{
    model::ProjectKind,
    work::{Removal, Request, Response},
    workspace_model::ModelEdit,
    worktree_content::Verdict,
};
use std::{os::unix::fs::PermissionsExt, process::Command, sync::Arc};

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
        .args(arguments)
        .current_dir(directory)
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.com")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A Work board over one repository with linked worktrees `feature`, which
/// has a commit and an untracked file of its own, and `landed`, at main.
struct Fixture {
    host: SharedHost,
    service: super::super::Service,
    repository: PathBuf,
}

impl Fixture {
    fn new(root: &Path) -> Self {
        let repository = root.join("project/repo");
        std::fs::create_dir_all(&repository).unwrap();
        let repository = repository.canonicalize().unwrap();
        git(&repository, &["init", "-q", "-b", "main"]);
        std::fs::write(repository.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(repository.join("readme"), "one\n").unwrap();
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "-q", "-m", "initial"]);
        for (name, branch) in [("feature", "feature"), ("landed", "landed")] {
            git(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    branch,
                    &format!(".worktrees/{name}"),
                ],
            );
        }
        let feature = repository.join(".worktrees/feature");
        std::fs::write(feature.join("readme"), "two\n").unwrap();
        git(&feature, &["commit", "-q", "-am", "feature work"]);
        std::fs::write(feature.join("draft"), "unsaved\n").unwrap();
        std::fs::create_dir(feature.join("build")).unwrap();
        std::fs::write(feature.join("build/output"), "generated\n").unwrap();

        let host = super::super::super::service::test_host();
        let mut registry =
            super::super::super::model::Registry::at(&root.join("data/registry.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: repository.clone(),
                name: "repo".into(),
                kind: ProjectKind::Git,
            })
            .unwrap();
        host.lock().unwrap().execution_registry = Some(Arc::new(std::sync::Mutex::new(registry)));
        let service =
            super::super::Service::start_with(&host, Err("no store in this fixture".into()))
                .unwrap();
        Self {
            host,
            service,
            repository,
        }
    }

    fn process(&self, request: Request) -> Result<Response, String> {
        self.service.handle().process(&self.host, request)
    }

    fn plan(&self, item: &str) -> RemovalPlan {
        match self.process(Request::RemovalPlan {
            scope: Scope::All,
            item: item.into(),
            fetch: false,
        }) {
            Ok(Response::RemovalPlan(plan)) => *plan,
            other => panic!("unexpected {other:?}"),
        }
    }

    fn remove(
        &self,
        item: &str,
        token: &str,
        keep: Keep,
        delete_branch: bool,
    ) -> Result<Removed, String> {
        self.process(Request::Remove(Box::new(Removal {
            scope: Scope::All,
            item: item.into(),
            token: token.into(),
            keep,
            delete_branch,
        })))
        .map(|response| match response {
            Response::Removed(removed) => *removed,
            other => panic!("unexpected {other:?}"),
        })
    }

    fn worktrees(&self) -> String {
        git(&self.repository, &["worktree", "list", "--porcelain"])
    }
}

const CHILD: &str = "CAGENTS_WORK_REMOVAL_TEST";

/// Runs the test `name` again in a child process with private XDG
/// directories, and returns the fixture root to that child; the parent gets
/// `None` once the child passed. Removal censuses every process of the user,
/// so where unprivileged namespaces allow, the child also gets a PID
/// namespace of its own: an editor the developer has open is none of the
/// fixture's.
fn isolated(name: &str) -> Option<PathBuf> {
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return Some(PathBuf::from(
            std::env::var_os("CAGENTS_WORK_REMOVAL_ROOT").unwrap(),
        ));
    }
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    for path in ["home", "data", "config", "cache", "tmp", "run"] {
        std::fs::create_dir_all(root.join(path)).unwrap();
    }
    std::fs::set_permissions(root.join("run"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let namespaced = Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--pid",
            "--fork",
            "--mount-proc",
            "true",
        ])
        .output()
        .is_ok_and(|output| output.status.success());
    let mut command = if namespaced {
        let mut command = Command::new("unshare");
        command
            .args([
                "--user",
                "--map-root-user",
                "--pid",
                "--fork",
                "--mount-proc",
            ])
            .arg(std::env::current_exe().unwrap());
        command
    } else {
        Command::new(std::env::current_exe().unwrap())
    };
    let status = command
        .args([
            "--exact",
            &format!("host::work::removal::tests::{name}"),
            "--nocapture",
        ])
        .env_clear()
        .env(CHILD, name)
        .env("CAGENTS_WORK_REMOVAL_ROOT", root)
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root.join("tmp"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("PATH", "/usr/bin:/bin")
        .status()
        .unwrap();
    assert!(status.success(), "the isolated removal fixture failed");
    None
}

#[test]
fn unsaved_work_is_kept_as_a_snapshot_before_its_worktree_goes() {
    let Some(root) = isolated("unsaved_work_is_kept_as_a_snapshot_before_its_worktree_goes") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let feature = fixture.repository.join(".worktrees/feature");

    let plan = fixture.plan("feature");
    assert_eq!(plan.item, "feature");
    assert_eq!(plan.inspection.path, feature);
    assert!(matches!(
        plan.inspection.verdict,
        Verdict::Unsaved {
            uncommitted: true,
            unreachable: false,
            beyond_snapshot: false
        }
    ));
    assert_eq!(plan.inspection.untracked, 1);
    assert_eq!(plan.inspection.ignored_total, 1);
    let token = plan.inspection.token.clone().unwrap();

    // Neither leaving the work behind nor deleting an unlanded branch is
    // allowed, and a stale token is refused.
    let error = fixture
        .remove("feature", &token, Keep::Nothing, false)
        .unwrap_err();
    assert!(error.contains("unsaved work"), "{error}");
    let error = fixture
        .remove("feature", &token, Keep::Snapshot, true)
        .unwrap_err();
    assert!(error.contains("Only a branch whose work landed"), "{error}");
    let error = fixture
        .remove("feature", "stale", Keep::Snapshot, false)
        .unwrap_err();
    assert_eq!(error, CHANGED);
    assert!(feature.exists());

    // A process inside the worktree refuses the removal.
    let mut resident = Command::new("sleep")
        .arg("30")
        .current_dir(&feature)
        .spawn()
        .unwrap();
    let error = fixture
        .remove("feature", &token, Keep::Snapshot, false)
        .unwrap_err();
    assert!(
        error.contains(&format!("process {}", resident.id())),
        "{error}"
    );
    resident.kill().unwrap();
    resident.wait().unwrap();
    assert!(feature.exists());
    assert!(
        git(&fixture.repository, &["for-each-ref", SNAPSHOT_REFS]).is_empty(),
        "a refused removal keeps no snapshot"
    );

    let removed = fixture
        .remove("feature", &token, Keep::Snapshot, false)
        .unwrap();
    assert_eq!(removed.path, feature);
    assert!(removed.deleted_branch.is_none());
    let snapshot = removed.snapshot.unwrap();
    assert!(!feature.exists());
    assert!(!fixture.worktrees().contains(".worktrees/feature"));
    // The branch stays, and the snapshot restores the unsaved file.
    git(
        &fixture.repository,
        &["rev-parse", "--verify", "refs/heads/feature"],
    );
    assert_eq!(
        git(
            &fixture.repository,
            &["show", &format!("{}^3:draft", snapshot.oid)]
        ),
        "unsaved"
    );

    let Ok(Response::Snapshots(snapshots)) =
        fixture.process(Request::Snapshots { scope: Scope::All })
    else {
        panic!("no snapshot listing");
    };
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].refname, snapshot.refname);
    assert_eq!(snapshots[0].oid, snapshot.oid);
    assert_eq!(snapshots[0].worktree.as_deref(), Some(feature.as_path()));
    assert!(snapshots[0].subject.starts_with("WIP on feature: "));
    assert!(snapshots[0].created_at_unix_ms.is_some());
}

#[test]
fn a_landed_worktree_goes_with_its_branch() {
    let Some(root) = isolated("a_landed_worktree_goes_with_its_branch") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let landed = fixture.repository.join(".worktrees/landed");
    // A packed branch below a ref directory has no loose directory to lock.
    git(&landed, &["branch", "-m", "team/landed"]);
    git(&fixture.repository, &["pack-refs", "--all"]);
    let loose = git(&fixture.repository, &["rev-parse", "--git-common-dir"]);
    let team = fixture.repository.join(loose).join("refs/heads/team");
    let _ = std::fs::remove_dir(&team);
    assert!(!team.exists());

    let plan = fixture.plan(&landed.display().to_string());
    assert_eq!(plan.inspection.verdict, Verdict::Landed);
    assert_eq!(plan.inspection.branch.as_deref(), Some("team/landed"));
    let token = plan.inspection.token.clone().unwrap();
    plan.permits(Keep::Nothing, true).unwrap();

    // Content changed after the plan: the removal is refused.
    std::fs::write(landed.join("late"), "arrived after the plan\n").unwrap();
    let error = fixture
        .remove("team/landed", &token, Keep::Nothing, true)
        .unwrap_err();
    assert_eq!(error, CHANGED);
    std::fs::remove_file(landed.join("late")).unwrap();

    let removed = fixture
        .remove("team/landed", &token, Keep::Nothing, true)
        .unwrap();
    assert_eq!(removed.deleted_branch.as_deref(), Some("team/landed"));
    assert_eq!(removed.branch_error, None);
    assert!(removed.snapshot.is_none());
    assert!(!landed.exists());
    assert!(
        git(&fixture.repository, &["for-each-ref", "refs/heads/team/"]).is_empty(),
        "the branch is deleted"
    );
    // The primary worktree is never offered for removal.
    let error = fixture
        .process(Request::RemovalPlan {
            scope: Scope::All,
            item: "main".into(),
            fetch: false,
        })
        .unwrap_err();
    assert_eq!(error, "The primary worktree is never removed");
}
