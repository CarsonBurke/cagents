//! The native-evidence fixture's offline forge, `tests/fixtures/fixture-gh.sh`,
//! must answer every read the sidebar, review-request poll and pull request
//! browser make in shapes these parsers accept. Its own contract suite compares
//! against copied strings; this runs the script through the real service.
//!
//! The calls mirror the app's GitHub entry points: `load_repository_github` and
//! `attached_worktree_pull_requests` in src/app.rs, `read` in
//! src/host/work/forge.rs, and the `GitHubReviewApi` pages. When one of them
//! starts a new gh read, add it here and teach the forge to answer it.
use super::*;
use std::process::Command;

struct Forge {
    _root: tempfile::TempDir,
    gh: PathBuf,
    switch: PathBuf,
    seed: PathBuf,
    project: PathBuf,
}

fn forge() -> Forge {
    let root = tempfile::tempdir().unwrap();
    let gh = root.path().join("bin/gh");
    std::fs::create_dir_all(gh.parent().unwrap()).unwrap();
    // Embedded, not copied: a test built on the build host runs elsewhere.
    std::fs::write(&gh, include_str!("../../tests/fixtures/fixture-gh.sh")).unwrap();
    std::fs::set_permissions(&gh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let switch = root.path().join("home/.config/fixture-forge");
    std::fs::create_dir_all(switch.parent().unwrap()).unwrap();
    std::fs::write(&switch, "").unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    for arguments in [
        &["init", "--quiet", "--initial-branch=main"][..],
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:Fixture-Owner/fixture-app.git",
        ],
    ] {
        let status = Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }
    Forge {
        seed: root.path().join("home/.config/fixture-forge-prs.json"),
        _root: root,
        gh,
        switch,
        project,
    }
}

#[test]
fn the_fixture_forge_reaches_ready_and_lists_nothing() {
    let forge = forge();
    let service = GitHubService::system(&forge.gh);
    let cancellation = CancellationToken::new();

    let key = service
        .resolve_repository(&forge.project, &[], &cancellation)
        .unwrap();
    assert_eq!(
        key,
        GitHubRepoKey::new("github.com", None, "Fixture-Owner", "fixture-app").unwrap()
    );
    let auth = service.auth_status(&key, &cancellation).unwrap();
    assert_eq!(auth.state, GhAuthState::Authenticated);
    assert_eq!(auth.login.as_deref(), Some("fixture"));

    for state in [PullRequestStateFilter::Open, PullRequestStateFilter::All] {
        let request = ListPullRequestsRequest {
            repository: key.clone(),
            state,
            limit: 30,
        };
        assert!(service.list(&request, &cancellation).unwrap().is_empty());
    }
    let authored = service
        .authored_pull_requests(&key, 50, 100, &cancellation)
        .unwrap();
    assert_eq!(authored.viewer, "fixture");
    assert!(authored.open.is_empty() && authored.all.is_empty());
    let head = "a".repeat(40);
    assert!(
        service
            .resolve_branch_head(&key, &key, "main", &head, &cancellation)
            .unwrap()
            .is_none()
    );

    let requests = service
        .review_request_page(&key, None, &cancellation)
        .unwrap();
    assert_eq!(requests.viewer.login, "fixture");
    assert!(requests.entries.is_empty() && requests.next_cursor.is_none());
    let api: &dyn GitHubReviewApi = &service;
    let page = api.pull_request_page(&key, None, &cancellation).unwrap();
    assert!(page.pull_requests.is_empty() && page.next_cursor.is_none());
    let found = api
        .search_pull_requests(&key, "fix", None, &cancellation)
        .unwrap();
    assert!(found.pull_requests.is_empty() && found.next_cursor.is_none());

    // A pull request the forge does not have is gh's not-found failure.
    let missing = PullRequestKey::new(key, 7).unwrap();
    assert!(service.view(&missing, &cancellation).is_err());
}

#[test]
fn without_its_switch_the_fixture_forge_refuses() {
    let forge = forge();
    std::fs::remove_file(&forge.switch).unwrap();
    let service = GitHubService::system(&forge.gh);
    let error = service
        .resolve_repository(&forge.project, &[], &CancellationToken::new())
        .unwrap_err();
    let GitHubError::Process(failure) = &error else {
        panic!("{error:?}");
    };
    assert_eq!(failure.stderr, b"gh: the fixture has no forge access\n");
}

#[test]
fn the_fixture_forge_answers_with_a_seeded_pull_request() {
    // The forge filters its seed with jq, as the fixture's documentation says.
    if Command::new("jq").arg("--version").output().is_err() {
        eprintln!("skipped: jq is not installed");
        return;
    }
    let forge = forge();
    let head = "b".repeat(40);
    let seed = serde_json::json!([{
        "id": "PR_kwDOFixture7",
        "number": 7,
        "title": "Teach the sidebar about avatars",
        "body": "",
        "state": "OPEN",
        "isDraft": false,
        "headRefName": "feature",
        "headRefOid": head,
        "headRepository": {"name": "fixture-app"},
        "headRepositoryOwner": {"login": "Fixture-Owner"},
        "baseRefName": "main",
        "author": {"login": "fixture"},
        "createdAt": "2026-11-01T00:00:00Z",
        "updatedAt": "2026-11-02T00:00:00Z",
        "url": "https://github.com/Fixture-Owner/fixture-app/pull/7",
        "labels": [],
    }]);
    std::fs::write(&forge.seed, seed.to_string()).unwrap();
    let service = GitHubService::system(&forge.gh);
    let cancellation = CancellationToken::new();
    let key = service
        .resolve_repository(&forge.project, &[], &cancellation)
        .unwrap();

    let request = ListPullRequestsRequest {
        repository: key.clone(),
        state: PullRequestStateFilter::Open,
        limit: 30,
    };
    let listed = service.list(&request, &cancellation).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "Teach the sidebar about avatars");
    let sidebar = service.sidebar_open(&key, 50, &cancellation).unwrap();
    assert_eq!(sidebar.len(), 1);
    assert_eq!(
        sidebar[0].head_repository_key(),
        listed[0].head_repository_key()
    );
    assert!(sidebar[0].check_summaries().is_empty());
    let mut seeded_checks = seed.clone();
    seeded_checks[0]["statusCheckRollup"] = serde_json::json!([
        {"__typename":"StatusContext","context":"ci","state":"FAILURE"}
    ]);
    std::fs::write(&forge.seed, seeded_checks.to_string()).unwrap();
    let failing = service.sidebar_open(&key, 50, &cancellation).unwrap();
    assert_eq!(failing[0].check_summaries()[0].state.0, "FAILURE");
    let history = service.recent_history(&key, 50, &cancellation).unwrap();
    assert!(history[0].check_summaries().is_empty());
    let authored = service
        .authored_pull_requests(&key, 50, 100, &cancellation)
        .unwrap();
    assert_eq!(authored.open.len(), 1);
    assert_eq!(authored.all.len(), 1);
    assert_eq!(authored.open[0].check_summaries()[0].state.0, "FAILURE");
    assert_eq!(authored.all[0].status_check_rollup, None);

    // The board's probe is unchanged until a seed it covers changes.
    let probe = "repos/Fixture-Owner/fixture-app/issues?creator=fixture&state=all&sort=updated&direction=desc&per_page=20";
    let Revalidation::Modified {
        etag: Some(etag),
        date: Some(_),
        ..
    } = service
        .revalidate(&key, probe, None, &cancellation)
        .unwrap()
    else {
        panic!("the probe returns a validator");
    };
    assert_eq!(
        service
            .revalidate(&key, probe, Some(&etag), &cancellation)
            .unwrap(),
        Revalidation::NotModified
    );
    seeded_checks[0]["updatedAt"] = "2026-11-03T00:00:00Z".into();
    std::fs::write(&forge.seed, seeded_checks.to_string()).unwrap();
    assert!(matches!(
        service
            .revalidate(&key, probe, Some(&etag), &cancellation)
            .unwrap(),
        Revalidation::Modified {
            etag: Some(next), ..
        } if next != etag
    ));

    let viewed = service
        .view(&PullRequestKey::new(key.clone(), 7).unwrap(), &cancellation)
        .unwrap();
    assert_eq!(
        (viewed.number, viewed.head_ref_oid.as_str()),
        (7, head.as_str())
    );
    let attached = service
        .resolve_branch_head(&key, &key, "feature", &head, &cancellation)
        .unwrap()
        .expect("the seeded pull request of the checkout's branch");
    assert_eq!(attached.number, 7);
    assert!(
        service
            .resolve_branch_head(&key, &key, "other", &head, &cancellation)
            .unwrap()
            .is_none()
    );
    // A checkout attaches it only when its remote is the seed's head repository.
    let fork = GitHubRepoKey::new("github.com", None, "Someone", "fixture-app").unwrap();
    assert!(
        service
            .resolve_branch_head(&key, &fork, "feature", &head, &cancellation)
            .unwrap()
            .is_none()
    );
}
