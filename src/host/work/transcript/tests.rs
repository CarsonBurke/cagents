use super::*;
use crate::pull_request_signals::{PullRequestKeyOrd, RepoKeyOrd};
use serde_json::json;

fn key(number: u64) -> Reference {
    Reference::PullRequest(PullRequestKeyOrd {
        repository: RepoKeyOrd {
            host: "github.com".into(),
            port: None,
            owner: "o".into(),
            name: "r".into(),
        },
        number,
    })
}

fn read(format: Format, subagent: bool, lines: &[Value]) -> Vec<(Reference, Relation, bool)> {
    let mut reader = Reader::new(format, subagent);
    let mut found = Vec::new();
    for line in lines {
        reader.line(line.to_string().as_bytes(), &mut found);
    }
    found
        .into_iter()
        .map(|found| {
            (
                found.signal.reference,
                found.signal.relation,
                found.by_subagent,
            )
        })
        .collect()
}

fn claude_call(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": id, "name": name, "input": input}
    ]}})
}

fn claude_result(id: &str, content: Value, is_error: bool) -> Value {
    json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error}
    ]}})
}

#[test]
fn a_claude_command_is_read_with_its_result() {
    let found = read(
        Format::Claude,
        false,
        &[
            claude_call("t1", "Bash", json!({"command": "gh pr create --fill"})),
            json!({"type": "user", "message": {"content": "Please open #7 too"}}),
            claude_result(
                "t1",
                json!([{"type": "text", "text": "https://github.com/o/r/pull/12"}]),
                false,
            ),
        ],
    );
    assert_eq!(
        found,
        vec![
            (Reference::Number(7), Relation::Mentioned, false),
            (key(12), Relation::Created, false),
        ]
    );
}

#[test]
fn a_failed_claude_push_with_no_report_names_nothing() {
    let found = read(
        Format::Claude,
        false,
        &[
            claude_call("t1", "Bash", json!({"command": "git push -q origin feat"})),
            claude_result("t1", json!("fatal: unable to access"), true),
        ],
    );
    assert!(found.is_empty());
}

#[test]
fn claude_edits_point_at_their_directory_s_branch() {
    let found = read(
        Format::Claude,
        false,
        &[claude_call(
            "t1",
            "Edit",
            json!({"file_path": "/w/app/src/main.rs", "old_string": "#1", "new_string": "#2"}),
        )],
    );
    assert_eq!(
        found,
        vec![(
            Reference::CurrentBranch {
                directory: Some("/w/app/src".into())
            },
            Relation::Edited,
            false
        )]
    );
}

#[test]
fn harness_text_and_reads_are_not_mentions() {
    let found = read(
        Format::Claude,
        false,
        &[
            json!({"type": "user", "isMeta": true, "message": {"content": "Skill text about #4"}}),
            json!({"type": "user", "message": {"content": "<command-name>/review</command-name> #5"}}),
            json!({"type": "user", "message": {"content": [{"type": "text",
                "text": "Look at #6<system-reminder>ignore #8</system-reminder>"}]}}),
            json!({"type": "assistant", "message": {"content": [{"type": "thinking",
                "thinking": "maybe #9"}]}}),
            claude_call("t2", "Read", json!({"file_path": "/x"})),
            claude_result("t2", json!("https://github.com/o/r/pull/10"), false),
        ],
    );
    assert_eq!(
        found,
        vec![(Reference::Number(6), Relation::Mentioned, false)]
    );
}

#[test]
fn subagent_lines_and_reports_are_marked_as_such() {
    let found = read(
        Format::Claude,
        true,
        &[
            claude_call("t1", "Bash", json!({"command": "gh pr checkout 3"})),
            claude_result("t1", json!("Switched to branch 'feat'"), false),
        ],
    );
    assert_eq!(
        found,
        vec![(Reference::Number(3), Relation::CheckedOut, true)]
    );
    let sidechain = read(
        Format::Claude,
        false,
        &[
            json!({"type": "assistant", "isSidechain": true, "message": {"content": [
            {"type": "text", "text": "Done: https://github.com/o/r/pull/4"}]}}),
        ],
    );
    assert_eq!(sidechain, vec![(key(4), Relation::Mentioned, true)]);
    // A delegated agent's report reaches the conversation as its result.
    let report = read(
        Format::Claude,
        false,
        &[
            claude_call("a1", "Agent", json!({"prompt": "Review #5"})),
            claude_result(
                "a1",
                json!([{"type": "text", "text": "Reviewed https://github.com/o/r/pull/5"}]),
                false,
            ),
        ],
    );
    assert_eq!(
        report,
        vec![
            (Reference::Number(5), Relation::Mentioned, false),
            (key(5), Relation::Mentioned, false),
        ]
    );
}

#[test]
fn codex_commands_outputs_and_patches_are_read() {
    let found = read(
        Format::Codex,
        false,
        &[
            json!({"type": "response_item", "payload": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "<environment_context>#1</environment_context>"}]}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "Land #2 please"}]}}),
            json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell",
                "call_id": "c1", "arguments": json!({"command": ["bash", "-lc", "git push origin feat"],
                "workdir": "/w/app"}).to_string()}}),
            json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "c1",
                "output": json!({"output": "To github.com:o/r.git\n * [new branch]      feat -> feat\n",
                "metadata": {"exit_code": 0}}).to_string()}}),
            json!({"type": "response_item", "payload": {"type": "custom_tool_call", "name": "apply_patch",
                "call_id": "c2", "input": "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-#3\n+#4\n*** End Patch"}}),
            json!({"type": "response_item", "payload": {"type": "function_call", "name": "exec_command",
                "call_id": "c3", "arguments": json!({"cmd": "gh pr merge 2 --squash"}).to_string()}}),
            json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "c3",
                "output": "Exit code: 1\nWall time: 1s\nOutput:\nnot mergeable"}}),
        ],
    );
    assert_eq!(
        found,
        vec![
            (Reference::Number(2), Relation::Mentioned, false),
            (
                Reference::Branch {
                    repository: Some(RepoKeyOrd {
                        host: "github.com".into(),
                        port: None,
                        owner: "o".into(),
                        name: "r".into(),
                    }),
                    name: "feat".into()
                },
                Relation::Pushed,
                false
            ),
            (
                Reference::CurrentBranch {
                    directory: Some("src".into())
                },
                Relation::Edited,
                false
            ),
            // The merge failed, so it only named the pull request.
            (Reference::Number(2), Relation::Mentioned, false),
        ]
    );
}

#[test]
fn unanswered_calls_are_bounded() {
    let mut reader = Reader::new(Format::Claude, false);
    let mut found = Vec::new();
    for index in 0..(PENDING * 3) {
        let line = claude_call(&format!("t{index}"), "Bash", json!({"command": "true"}));
        reader.line(line.to_string().as_bytes(), &mut found);
    }
    assert!(reader.pending.len() <= PENDING);
    reader.line(b"not json", &mut found);
    assert!(found.is_empty());
}
