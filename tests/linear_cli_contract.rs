use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output, Stdio},
};

const ISSUE_ID: &str = "2b498c52-7dc7-4c76-ad59-dfc488c40111";
const TEAM_ID: &str = "2b498c52-7dc7-4c76-ad59-dfc488c40222";
const KEY: &str = "fixture-linear-secret";

struct Fixture {
    root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("bin")).unwrap();
        fs::write(
            root.path().join("bin/curl"),
            include_bytes!("fixtures/linear-curl.sh"),
        )
        .unwrap();
        fs::set_permissions(
            root.path().join("bin/curl"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        // Missing-key behavior must not depend on a CI host's Secret Service
        // installation or desktop environment. Specific keyring tests replace it.
        let keyring = root.path().join("bin/secret-tool");
        fs::write(&keyring, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&keyring, fs::Permissions::from_mode(0o700)).unwrap();
        let f = Self { root };
        let output = Command::new("git")
            .args(["init", "-q"])
            .current_dir(f.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        f.read(json!({"issue":{
            "id": ISSUE_ID,"identifier":"ENG-12","title":"Fixture issue","description":"Body",
            "team":{"id":TEAM_ID,"key":"ENG"}, "state":{"id":TEAM_ID,"name":"In Progress","type":"started"},
            "labels":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}},
            "comments":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}
        }}));
        f.mutation(
            json!({"issueUpdate":{"success":true,"issue":{"id":ISSUE_ID,"title":"Updated"}},
            "issueCreate":{"success":true,"issue":{"id":ISSUE_ID}},
            "commentCreate":{"success":true,"comment":{"id":TEAM_ID,"body":"Done"}}}),
        );
        f
    }
    fn path(&self) -> &Path {
        self.root.path()
    }
    fn read(&self, data: Value) {
        fs::write(self.path().join("read"), json!({"data":data}).to_string()).unwrap();
    }
    fn mutation(&self, data: Value) {
        fs::write(
            self.path().join("mutation"),
            json!({"data":data}).to_string(),
        )
        .unwrap();
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_cagents"));
        c.env_clear()
            .env("HOME", self.path())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.path().join("bin").display()),
            )
            .env("LINEAR_FIXTURE", self.path())
            .env("LINEAR_API_KEY", KEY)
            .env("CAGENTS_LINEAR_API_KEY_WORK", KEY)
            .current_dir(self.path())
            .arg("--json")
            .args(args);
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn success(&self, args: &[&str]) -> Value {
        let o = self.run(args);
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        assert_eq!(v["ok"], true);
        v["data"].clone()
    }
    fn failure(&self, args: &[&str]) -> Value {
        let o = self.run(args);
        assert!(!o.status.success(), "{args:?}");
        assert!(!String::from_utf8_lossy(&o.stderr).contains(KEY));
        serde_json::from_slice::<Value>(&o.stderr).unwrap()["error"].clone()
    }
    fn request(&self) -> String {
        fs::read_to_string(self.path().join("request")).unwrap()
    }
    fn calls(&self) -> usize {
        fs::read_to_string(self.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}

#[test]
fn issue_reads_link_current_and_unlink_need_no_daemon() {
    let f = Fixture::new();
    assert_eq!(
        f.success(&[
            "linear",
            "issue",
            "https://linear.app/team/issue/ENG-12/title",
            "--full"
        ])["id"],
        ISSUE_ID
    );
    assert!(
        !fs::read_to_string(f.path().join("argv"))
            .unwrap()
            .contains(KEY)
    );
    assert!(f.request().contains("public-file-urls-expire-in: 300"));
    f.success(&["linear", "--workspace", "work", "link", "ENG-12"]);
    assert_eq!(f.success(&["linear", "issue", "--current"])["id"], ISSUE_ID);
    let calls = f.calls();
    assert_eq!(
        f.failure(&["linear", "--workspace", "other", "issue", "--current"])["kind"],
        "linear_input"
    );
    assert_eq!(f.calls(), calls);
    f.success(&["linear", "unlink"]);
    assert_eq!(
        f.failure(&["linear", "issue", "--current"])["kind"],
        "linear_issue_required"
    );
}

#[test]
fn inherited_git_overrides_cannot_redirect_worktree_link() {
    let f = Fixture::new();
    let other = Fixture::new();
    let o = f
        .command(&["linear", "link", "ENG-12"])
        .env("GIT_DIR", other.path().join(".git"))
        .env("GIT_WORK_TREE", other.path())
        .env("GIT_COMMON_DIR", other.path().join(".git"))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(f.path().join(".git/cagents-linear.json").exists());
    assert!(!other.path().join(".git/cagents-linear.json").exists());
}

#[test]
fn paginated_search_preserves_cursor_and_truncation() {
    let f = Fixture::new();
    f.read(json!({"searchIssues":{"nodes":[{"identifier":"ENG-12"}],"pageInfo":{"hasNextPage":true,"endCursor":"next"}}}));
    let v = f.success(&[
        "linear", "search", "fixture", "--limit", "1", "--cursor", "previous",
    ]);
    assert_eq!(v["pageInfo"]["hasNextPage"], true);
    assert!(f.request().contains("previous"));
    assert!(f.request().contains("first\\\":1"));
}

#[test]
fn setters_use_resolved_ids_and_uncertain_writes_are_single_attempt() {
    let f = Fixture::new();
    f.success(&["linear", "link", "ENG-12"]);
    f.success(&["linear", "set", "--current", "title", "Updated"]);
    assert!(f.request().contains(ISSUE_ID));
    assert!(f.request().contains("Updated"));
    fs::write(
        f.path().join("mutation"),
        "{\"data\":null,\"errors\":[{\"message\":\"internal failure\"}]}",
    )
    .unwrap();
    let before = f.calls();
    let error = f.failure(&["linear", "set", "--current", "title", "Next"]);
    assert_eq!(error["kind"], "linear_write_unconfirmed");
    assert_eq!(
        f.calls() - before,
        2,
        "one read and one write; no mutation retry"
    );
}

#[test]
fn missing_credentials_and_invalid_inputs_fail_without_network() {
    let f = Fixture::new();
    let o = f
        .command(&["linear", "issue", "ENG-12"])
        .env_remove("LINEAR_API_KEY")
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&o.stderr).unwrap()["error"]["kind"],
        "linear_auth_required"
    );
    f.failure(&["linear", "issue", "https://evil.test/issue/ENG-12"]);
    f.failure(&["linear", "search", "x", "--limit", "101"]);
    assert_eq!(f.calls(), 0);
}

#[test]
fn create_uncertainty_preserves_uuid_and_selected_workspace() {
    let f = Fixture::new();
    fs::write(f.path().join("mutation"), "{}").unwrap();
    let error = f.failure(&[
        "linear",
        "--workspace",
        "work",
        "create",
        "--team",
        TEAM_ID,
        "--title",
        "New ticket",
        "--request-id",
        ISSUE_ID,
    ]);
    assert_eq!(error["kind"], "linear_write_unconfirmed");
    assert_eq!(error["details"]["writeContext"]["requestId"], ISSUE_ID);
    assert_eq!(error["details"]["writeContext"]["issueId"], ISSUE_ID);
    assert_eq!(error["details"]["writeContext"]["workspace"], "WORK");
    assert_eq!(
        f.calls(),
        1,
        "a UUID team needs no metadata lookup or write retry"
    );
}

#[test]
fn comments_accept_multiline_stdin_and_resolved_issue_id() {
    let f = Fixture::new();
    let mut child = f
        .command(&[
            "linear",
            "comment",
            "add",
            "--id",
            "ENG-12",
            "--body-file",
            "-",
            "--request-id",
            TEAM_ID,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Done.\n\nPR: https://github.com/org/repo/pull/1\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = f.request();
    assert!(request.contains(ISSUE_ID));
    assert!(request.contains(TEAM_ID));
    assert!(request.contains("PR: https://github.com/org/repo/pull/1"));
    assert_eq!(f.calls(), 2);
}

#[test]
fn ambiguous_metadata_never_selects_first_match_for_a_write() {
    let f = Fixture::new();
    f.read(
        json!({"teams":{"nodes":[{"id":TEAM_ID,"key":"ENG"},{"id":ISSUE_ID,"key":"ENG"}],
        "pageInfo":{"hasNextPage":false,"endCursor":null}}}),
    );
    let error = f.failure(&["linear", "create", "--team", "ENG", "--title", "Ambiguous"]);
    assert!(error["message"].as_str().unwrap().contains("Ambiguous"));
    assert_eq!(f.calls(), 1);
    assert!(!f.request().contains("mutation("));
}

#[test]
fn team_only_updates_and_empty_label_sets_send_the_intended_fields() {
    let f = Fixture::new();
    f.success(&["linear", "save-issue", "--id", "ENG-12", "--team", ISSUE_ID]);
    assert!(f.request().contains("teamId"));
    f.success(&["linear", "label", "set", "--id", "ENG-12"]);
    assert!(f.request().contains("labelIds\\\":[]"));
}

#[test]
fn label_names_include_parent_teams_but_exclude_unrelated_teams() {
    let f = Fixture::new();
    let inherited = "2b498c52-7dc7-4c76-ad59-dfc488c40333";
    f.read(json!({
        "issue":{"id":ISSUE_ID,"team":{"id":TEAM_ID}},
        "team":{"id":TEAM_ID,"parent":{"id":ISSUE_ID}},
        "issueLabels":{"nodes":[
            {"id":inherited,"name":"Bug","team":{"id":ISSUE_ID}},
            {"id":TEAM_ID,"name":"Bug","team":{"id":"unrelated"}}
        ],"pageInfo":{"hasNextPage":false,"endCursor":null}}
    }));
    fs::write(
        f.path().join("parent-team"),
        json!({"data":{"team":{"id":ISSUE_ID,"parent":null}}}).to_string(),
    )
    .unwrap();
    f.success(&["linear", "label", "add", "--id", "ENG-12", "Bug"]);
    assert!(f.request().contains(inherited));
    assert!(f.request().contains("addedLabelIds"));
}

#[test]
fn issue_context_connections_are_pageable() {
    let f = Fixture::new();
    f.read(
        json!({"issue":{"comments":{"nodes":[{"id":ISSUE_ID,"body":"Markdown"}],
        "pageInfo":{"hasNextPage":true,"endCursor":"more-comments"}}}}),
    );
    let result = f.success(&[
        "linear", "comments", "--id", "ENG-12", "--limit", "1", "--cursor", "previous",
    ]);
    assert_eq!(result["nodes"][0]["body"], "Markdown");
    assert_eq!(result["pageInfo"]["endCursor"], "more-comments");
    assert!(f.request().contains("previous"));
}

#[test]
fn uncertain_relation_deletion_identifies_relation_without_mistaking_it_for_issue() {
    let f = Fixture::new();
    fs::write(f.path().join("mutation"), "{}").unwrap();
    let error = f.failure(&["linear", "relation", "remove", TEAM_ID]);
    assert_eq!(error["kind"], "linear_write_unconfirmed");
    assert_eq!(error["details"]["writeContext"]["objectId"], TEAM_ID);
    assert!(error["details"]["writeContext"]["issueId"].is_null());
    assert!(error["message"].as_str().unwrap().contains(TEAM_ID));
}

#[test]
fn stored_key_authenticates_without_shell_environment_and_environment_overrides_it() {
    let f = Fixture::new();
    let helper = f.path().join("bin/secret-tool");
    fs::write(&helper, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$LINEAR_FIXTURE/keyring-argv\"\nprintf '%s' 'fixture-keyring-secret'\n").unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let output = f
        .command(&["linear", "issue", "ENG-12"])
        .env_remove("LINEAR_API_KEY")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        f.request()
            .contains("Authorization: fixture-keyring-secret")
    );
    let keyring_args = fs::read_to_string(f.path().join("keyring-argv")).unwrap();
    assert!(keyring_args.contains("com.carson.cagents.linear"));
    assert!(keyring_args.contains("default"));
    assert!(!keyring_args.contains("fixture-keyring-secret"));
    fs::remove_file(f.path().join("keyring-argv")).unwrap();
    f.success(&["linear", "issue", "ENG-12"]);
    assert!(f.request().contains(&format!("Authorization: {KEY}")));
    assert!(
        !f.path().join("keyring-argv").exists(),
        "environment override avoids the keyring entirely"
    );
}

#[test]
fn team_and_state_names_use_filtered_connections_without_metadata_scans() {
    let f = Fixture::new();
    f.read(json!({"teams":{"nodes":[{"id":TEAM_ID,"name":"Engineering","key":"ENG"}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}));
    fs::write(f.path().join("mutation"), "{}").unwrap();
    f.failure(&[
        "linear",
        "create",
        "--team",
        "engineering",
        "--title",
        "New",
    ]);
    assert_eq!(
        f.calls(),
        2,
        "one filtered lookup and one attempted mutation"
    );
    // The fixture keeps every request, making the actual transport contract visible.
    let history = fs::read_to_string(f.path().join("requests")).unwrap();
    assert!(history.contains("$filter:TeamFilter!"));
    assert!(history.contains("eqIgnoreCase"));
    assert!(history.contains("engineering"));
    let f = Fixture::new();
    f.read(json!({
        "issue":{"id":ISSUE_ID,"team":{"id":TEAM_ID}},
        "workflowStates":{"nodes":[{"id":ISSUE_ID,"name":"In Progress","team":{"id":TEAM_ID}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}
    }));
    f.success(&["linear", "set", "--id", "ENG-12", "status", "In Progress"]);
    assert_eq!(
        f.calls(),
        3,
        "one issue read, one filtered state lookup, one mutation"
    );
    let history = fs::read_to_string(f.path().join("requests")).unwrap();
    assert!(history.contains("$filter:WorkflowStateFilter!"));
    assert!(history.contains("eqIgnoreCase"));
}

#[test]
fn unavailable_keyring_helper_reports_the_install_and_unlock_action() {
    let f = Fixture::new();
    let missing = f.path().join("missing-bin");
    fs::create_dir(&missing).unwrap();
    let output = f
        .command(&["linear", "issue", "ENG-12"])
        .env_remove("LINEAR_API_KEY")
        .env("PATH", &missing)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = serde_json::from_slice::<Value>(&output.stderr).unwrap()["error"].clone();
    assert_eq!(error["kind"], "linear_keyring_unavailable");
    assert!(error["message"].as_str().unwrap().contains("secret-tool"));
    assert!(error["message"].as_str().unwrap().contains("unlock"));
    assert_eq!(f.calls(), 0);
}
