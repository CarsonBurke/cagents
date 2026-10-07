//! Claude's edit tools as the file changes they make.
//!
//! Claude Code states an edit as text to find and text to put in its place,
//! and a write as a file's whole content. Shown as the JSON it was called
//! with, an edit is a line of escaped newlines; shown as a patch, it is what
//! changed. The patch is made here from the call alone, so it exists while
//! the edit is still waiting to be allowed, and is replaced by Claude Code's
//! own once the edit has been applied - that one is made against the file,
//! so it has the file's line numbers and the lines around the change.

use crate::chat::{ChangeKind, FileChange};
use imara_diff::{Algorithm, Diff, InternedInput};
use serde_json::Value;

/// Past this much text an edit is listed as removed and added lines rather
/// than compared: a patch that size is not read line by line, and comparing
/// it would hold up every other frame the provider has sent.
const COMPARED: usize = 512 * 1024;

/// The unchanged lines shown either side of a change, as `diff -u` has it.
const CONTEXT: usize = 3;

/// The change an edit tool's call describes, or `None` for any other tool
/// and for a call that does not carry the whole of what it would do.
///
/// A notebook edit names a cell and its new source and nothing of what the
/// cell held, so there is no patch to make of it and it stays a tool call.
pub(super) fn changes(name: &str, input: &Value) -> Option<Vec<FileChange>> {
    let path = input["file_path"]
        .as_str()
        .filter(|path| !path.is_empty())?;
    let (kind, diff) = match name {
        "Edit" => {
            let old = input["old_string"].as_str()?;
            (
                // An edit that replaces nothing is how Claude creates a file
                // with this tool.
                if old.is_empty() {
                    ChangeKind::Added
                } else {
                    ChangeKind::Updated
                },
                replacement(input)?,
            )
        }
        "MultiEdit" => {
            let edits = input["edits"]
                .as_array()
                .filter(|edits| !edits.is_empty())?;
            (
                ChangeKind::Updated,
                edits
                    .iter()
                    .map(replacement)
                    .collect::<Option<Vec<_>>>()?
                    .concat(),
            )
        }
        // Whether the file existed is only known once it has been written.
        "Write" => (
            ChangeKind::Added,
            unified("", input["content"].as_str()?, ""),
        ),
        _ => return None,
    };
    Some(vec![FileChange {
        path: path.to_owned(),
        kind,
        diff,
    }])
}

/// Writes one hunk: `before` and `after` are its lines on either side,
/// starting at the given line of each text, and `changes` the ranges of
/// those texts that differ. With no ranges given, every line does.
fn hunk(
    diff: &mut String,
    before_start: usize,
    before: &[String],
    after_start: usize,
    after: &[String],
    changes: &[(std::ops::Range<usize>, std::ops::Range<usize>)],
) {
    // A range of no lines is numbered by the line before it, as `diff` has it.
    let range = |start: usize, lines: usize| format!("{},{lines}", start + usize::from(lines > 0));
    diff.push_str(&format!(
        "@@ -{} +{} @@\n",
        range(before_start, before.len()),
        range(after_start, after.len())
    ));
    let mut write = |sign: char, lines: &[String]| {
        for line in lines {
            diff.push(sign);
            diff.push_str(line);
        }
    };
    let whole = [(
        before_start..before_start + before.len(),
        after_start..after_start + after.len(),
    )];
    let mut shown = before_start;
    for (removed, added) in if changes.is_empty() {
        &whole[..]
    } else {
        changes
    } {
        write(
            ' ',
            &before[shown - before_start..removed.start - before_start],
        );
        write(
            '-',
            &before[removed.start - before_start..removed.end - before_start],
        );
        write(
            '+',
            &after[added.start - after_start..added.end - after_start],
        );
        shown = removed.end;
    }
    write(' ', &before[shown - before_start..]);
}

/// One `old_string` to `new_string` replacement as the hunks it makes.
fn replacement(edit: &Value) -> Option<String> {
    Some(unified(
        edit["old_string"].as_str()?,
        edit["new_string"].as_str()?,
        // Stated after the hunk's range, where a patch names the function
        // it is in: the hunk shows one occurrence and stands for them all.
        if edit["replace_all"].as_bool() == Some(true) {
            " every occurrence"
        } else {
            ""
        },
    ))
}

/// `before` and `after` as a unified diff with three lines of context, each
/// hunk's range followed by `section`.
///
/// The ranges count from the start of the text given, which for an edit is
/// the text it looks for and not the file.
fn unified(before: &str, after: &str, section: &str) -> String {
    // Text that stops short of a newline is still a last line, and comparing
    // it as written would report that line changed whenever one side went on.
    let ended = |text: &str| {
        if text.is_empty() || text.ends_with('\n') {
            text.to_owned()
        } else {
            format!("{text}\n")
        }
    };
    let (before, after) = (ended(before), ended(after));
    let mut diff = String::new();
    if before.len().saturating_add(after.len()) > COMPARED {
        let lines =
            |text: &str| -> Vec<String> { text.split_inclusive('\n').map(str::to_owned).collect() };
        hunk(&mut diff, 0, &lines(&before), 0, &lines(&after), &[]);
    } else {
        let input = InternedInput::new(before.as_str(), after.as_str());
        let mut compared = Diff::compute(Algorithm::Histogram, &input);
        compared.postprocess_lines(&input);
        let lines = |tokens: &[imara_diff::Token]| -> Vec<String> {
            tokens
                .iter()
                .map(|token| input.interner[*token].to_owned())
                .collect()
        };
        let (before, after) = (lines(&input.before), lines(&input.after));
        // Changes whose context would touch or overlap share a hunk.
        let mut groups: Vec<Vec<(std::ops::Range<usize>, std::ops::Range<usize>)>> = Vec::new();
        for change in compared.hunks() {
            let change = (
                change.before.start as usize..change.before.end as usize,
                change.after.start as usize..change.after.end as usize,
            );
            match groups.last_mut() {
                Some(group)
                    if change.0.start - group.last().map_or(0, |last| last.0.end)
                        <= 2 * CONTEXT =>
                {
                    group.push(change)
                }
                _ => groups.push(vec![change]),
            }
        }
        for group in &groups {
            let (first, last) = (&group[0], &group[group.len() - 1]);
            let lead = first.0.start.min(CONTEXT);
            let trail = (before.len() - last.0.end).min(CONTEXT);
            hunk(
                &mut diff,
                first.0.start - lead,
                &before[first.0.start - lead..last.0.end + trail],
                first.1.start - lead,
                &after[first.1.start - lead..last.1.end + trail],
                group,
            );
        }
    }
    if !section.is_empty() {
        diff = diff
            .split_inclusive('\n')
            .map(|line| match line.strip_suffix('\n') {
                Some(range) if range.starts_with("@@ ") => format!("{range}{section}\n"),
                _ => line.to_owned(),
            })
            .collect();
    }
    diff
}

/// Brings a change up to date with the result of the edit that made it.
///
/// `structured` is what Claude Code reports beside the result's text: the
/// patch it applied, and for a write whether the file was there before.
/// Either may be missing - an older version, a transcript that was trimmed -
/// and then the change stays as the call described it.
pub(super) fn applied(changes: &mut [FileChange], structured: &Value) {
    let [change] = changes else {
        return;
    };
    if structured["type"] == "update" {
        change.kind = ChangeKind::Updated;
    }
    if let Some(patch) = structured_patch(&structured["structuredPatch"]) {
        change.diff = patch;
    }
}

/// Claude Code's own patch as unified diff text, or `None` where it reported
/// none or one that is not in the form this reads.
fn structured_patch(hunks: &Value) -> Option<String> {
    let hunks = hunks.as_array().filter(|hunks| !hunks.is_empty())?;
    let mut diff = String::new();
    for hunk in hunks {
        let number = |key: &str| hunk[key].as_u64();
        diff.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            number("oldStart")?,
            number("oldLines")?,
            number("newStart")?,
            number("newLines")?
        ));
        for line in hunk["lines"].as_array()? {
            diff.push_str(line.as_str()?);
            diff.push('\n');
        }
    }
    Some(diff)
}

/// The name of the tool an edit's input was written for, where only the
/// input was kept: each of the three takes an argument the others do not.
pub(super) fn tool_name(input: &Value) -> &'static str {
    if input["edits"].is_array() {
        "MultiEdit"
    } else if input["content"].is_string() {
        "Write"
    } else {
        "Edit"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn only(name: &str, input: Value) -> FileChange {
        let mut changes = changes(name, &input).expect("an edit is a change");
        assert_eq!(changes.len(), 1);
        changes.remove(0)
    }

    #[test]
    fn an_edit_is_the_patch_between_what_it_finds_and_what_it_writes() {
        let change = only(
            "Edit",
            json!({
                "file_path": "/repo/src/app.rs",
                "old_string": "fn main() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    run(a);\n}",
                "new_string": "fn main() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    run(a, b);\n    done();\n}\n",
                "replace_all": false
            }),
        );
        assert_eq!(change.path, "/repo/src/app.rs");
        assert_eq!(change.kind, ChangeKind::Updated);
        // Three lines of context, and a last line without a newline is the
        // same line as one with it.
        assert_eq!(
            change.diff,
            "@@ -3,5 +3,6 @@\n     let b = 2;\n     let c = 3;\n     let d = 4;\n-    run(a);\n+    run(a, b);\n+    done();\n }\n"
        );
    }

    /// Changes far enough apart are hunks of their own, each numbered from
    /// where it is in the text; changes whose context would meet share one.
    #[test]
    fn distant_changes_are_separate_hunks_and_near_ones_share_one() {
        let lines = |changed: &[usize]| {
            (1..=20)
                .map(|line| {
                    if changed.contains(&line) {
                        format!("changed {line}\n")
                    } else {
                        format!("line {line}\n")
                    }
                })
                .collect::<String>()
        };
        assert_eq!(
            unified(&lines(&[]), &lines(&[2, 16]), ""),
            "@@ -1,5 +1,5 @@\n line 1\n-line 2\n+changed 2\n line 3\n line 4\n line 5\n\
             @@ -13,7 +13,7 @@\n line 13\n line 14\n line 15\n-line 16\n+changed 16\n line 17\n line 18\n line 19\n"
        );
        assert_eq!(
            unified(&lines(&[10, 14]), &lines(&[]), ""),
            "@@ -7,11 +7,11 @@\n line 7\n line 8\n line 9\n-changed 10\n+line 10\n line 11\n line 12\n line 13\n-changed 14\n+line 14\n line 15\n line 16\n line 17\n"
        );
        assert_eq!(unified("same\n", "same\n", ""), "");
        // A deletion leaves a range of no lines on the side it emptied.
        assert_eq!(unified("gone\n", "", ""), "@@ -1,1 +0,0 @@\n-gone\n");
    }

    #[test]
    fn replacing_every_occurrence_is_stated_on_the_hunk() {
        let change = only(
            "Edit",
            json!({"file_path": "a.rs", "old_string": "foo", "new_string": "bar", "replace_all": true}),
        );
        assert_eq!(
            change.diff,
            "@@ -1,1 +1,1 @@ every occurrence\n-foo\n+bar\n"
        );
    }

    #[test]
    fn a_multi_edit_is_one_change_with_a_hunk_for_each_edit() {
        let change = only(
            "MultiEdit",
            json!({"file_path": "a.rs", "edits": [
                {"old_string": "one\ntwo", "new_string": "one\n2"},
                {"old_string": "x", "new_string": "y", "replace_all": true}
            ]}),
        );
        assert_eq!(change.kind, ChangeKind::Updated);
        assert_eq!(
            change.diff,
            "@@ -1,2 +1,2 @@\n one\n-two\n+2\n@@ -1,1 +1,1 @@ every occurrence\n-x\n+y\n"
        );
    }

    #[test]
    fn a_write_adds_every_line_and_an_empty_search_creates_the_file() {
        let written = only(
            "Write",
            json!({"file_path": "new.txt", "content": "first\nsecond\n"}),
        );
        assert_eq!(written.kind, ChangeKind::Added);
        assert_eq!(written.diff, "@@ -0,0 +1,2 @@\n+first\n+second\n");
        let created = only(
            "Edit",
            json!({"file_path": "new.txt", "old_string": "", "new_string": "only"}),
        );
        assert_eq!(created.kind, ChangeKind::Added);
        assert_eq!(created.diff, "@@ -0,0 +1,1 @@\n+only\n");
    }

    /// A call that has not said everything it would do is not yet a change,
    /// and neither is a tool that is not an edit.
    #[test]
    fn an_incomplete_or_unrelated_call_is_not_a_change() {
        for (name, input) in [
            ("Edit", json!({"file_path": "a.rs", "old_string": "a"})),
            ("Edit", json!({"old_string": "a", "new_string": "b"})),
            (
                "Edit",
                json!({"file_path": "", "old_string": "a", "new_string": "b"}),
            ),
            ("MultiEdit", json!({"file_path": "a.rs", "edits": []})),
            (
                "MultiEdit",
                json!({"file_path": "a.rs", "edits": [{"old_string": "a"}]}),
            ),
            ("Write", json!({"file_path": "a.rs"})),
            (
                "NotebookEdit",
                json!({"notebook_path": "a.ipynb", "new_source": "x"}),
            ),
            ("Read", json!({"file_path": "a.rs"})),
            ("Edit", Value::Null),
        ] {
            assert!(changes(name, &input).is_none(), "{name} {input}");
        }
    }

    #[test]
    fn text_too_long_to_compare_is_listed_as_removed_and_added() {
        let old = "old line\n".repeat(COMPARED / 9 + 1);
        let change = only(
            "Edit",
            json!({"file_path": "a.rs", "old_string": old, "new_string": "new\n"}),
        );
        assert!(
            change
                .diff
                .starts_with(&format!("@@ -1,{} +1,1 @@\n-old line\n", COMPARED / 9 + 1))
        );
        assert!(change.diff.ends_with("-old line\n+new\n"));
    }

    /// Claude Code's own patch replaces the one made from the call, since it
    /// was made against the file.
    #[test]
    fn the_applied_patch_replaces_the_one_made_from_the_call() {
        let mut edit = changes(
            "Edit",
            &json!({"file_path": "a.rs", "old_string": "foo", "new_string": "bar"}),
        )
        .unwrap();
        applied(
            &mut edit,
            &json!({
                "filePath": "a.rs", "oldString": "foo", "newString": "bar", "originalFile": null,
                "replaceAll": false, "userModified": false,
                "structuredPatch": [
                    {"oldStart": 114, "oldLines": 3, "newStart": 114, "newLines": 3,
                     "lines": [" before", "-foo", "+bar", " after"]},
                    {"oldStart": 200, "oldLines": 1, "newStart": 200, "newLines": 2,
                     "lines": [" x", "+y"]}
                ]
            }),
        );
        assert_eq!(edit[0].kind, ChangeKind::Updated);
        assert_eq!(
            edit[0].diff,
            "@@ -114,3 +114,3 @@\n before\n-foo\n+bar\n after\n@@ -200,1 +200,2 @@\n x\n+y\n"
        );
        // A result without a patch, or with one in another form, changes
        // nothing.
        let made = edit[0].diff.clone();
        for structured in [
            Value::Null,
            json!({"structuredPatch": []}),
            json!({"structuredPatch": [{"oldStart": 1, "lines": ["+a"]}]}),
            json!({"structuredPatch": [{"oldStart": 1, "oldLines": 0, "newStart": 1, "newLines": 1, "lines": [1]}]}),
        ] {
            applied(&mut edit, &structured);
            assert_eq!(edit[0].diff, made, "{structured}");
        }
    }

    /// A write is an addition until its result says the file was there.
    #[test]
    fn a_write_over_an_existing_file_is_an_update() {
        let write =
            || changes("Write", &json!({"file_path": "a.txt", "content": "new\n"})).unwrap();
        let mut created = write();
        applied(
            &mut created,
            &json!({"type": "create", "filePath": "a.txt", "content": "new\n", "structuredPatch": [], "originalFile": null}),
        );
        assert_eq!(created[0].kind, ChangeKind::Added);
        assert_eq!(created[0].diff, "@@ -0,0 +1,1 @@\n+new\n");
        let mut updated = write();
        applied(
            &mut updated,
            &json!({"type": "update", "filePath": "a.txt", "content": "new\n", "originalFile": "old\n",
                "structuredPatch": [{"oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": ["-old", "+new"]}]}),
        );
        assert_eq!(updated[0].kind, ChangeKind::Updated);
        assert_eq!(updated[0].diff, "@@ -1,1 +1,1 @@\n-old\n+new\n");
    }

    #[test]
    fn the_tool_an_input_was_written_for_is_read_off_its_arguments() {
        for (input, name) in [
            (
                json!({"file_path": "a", "old_string": "a", "new_string": "b"}),
                "Edit",
            ),
            (json!({"file_path": "a", "edits": []}), "MultiEdit"),
            (json!({"file_path": "a", "content": ""}), "Write"),
        ] {
            assert_eq!(tool_name(&input), name);
        }
    }
}
