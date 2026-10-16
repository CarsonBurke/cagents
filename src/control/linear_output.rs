//! Human projections of the existing GraphQL connection responses.
use super::super::format::table;
use serde_json::Value;

#[derive(Clone, Copy, Debug)]
pub(super) enum List {
    Issues,
    Comments,
    Attachments,
    Relations,
    Teams,
    States,
    Labels,
    Members,
    Projects,
}

fn text(row: &Value, pointer: &str) -> String {
    row.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned()
}

fn priority(row: &Value) -> String {
    match row.get("priority").and_then(Value::as_u64) {
        Some(0) => "none",
        Some(1) => "urgent",
        Some(2) => "high",
        Some(3) => "normal",
        Some(4) => "low",
        _ => "unknown",
    }
    .into()
}

pub(super) fn render(list: List, data: &Value) -> String {
    let Some(rows) = data.get("nodes").and_then(Value::as_array) else {
        return "Linear returned an unrecognized list response; use --json to inspect it".into();
    };
    let mut human = match list {
        List::Issues => table(
            ["ISSUE", "TITLE", "STATE", "PRIORITY", "ASSIGNEE"],
            rows.iter()
                .map(|r| {
                    [
                        text(r, "/identifier"),
                        text(r, "/title"),
                        text(r, "/state/name"),
                        priority(r),
                        if r.get("assignee").is_some_and(Value::is_null) {
                            "unassigned".into()
                        } else {
                            text(r, "/assignee/name")
                        },
                    ]
                })
                .collect(),
            "No Linear issues",
        ),
        List::Comments => table(
            ["COMMENT", "AUTHOR", "BODY"],
            rows.iter()
                .map(|r| [text(r, "/id"), text(r, "/user/name"), text(r, "/body")])
                .collect(),
            "No Linear comments",
        ),
        List::Attachments => table(
            ["ATTACHMENT", "TITLE", "URL"],
            rows.iter()
                .map(|r| [text(r, "/id"), text(r, "/title"), text(r, "/url")])
                .collect(),
            "No Linear attachments",
        ),
        List::Relations => table(
            ["RELATION", "TYPE", "ISSUE", "RELATED ISSUE"],
            rows.iter()
                .map(|r| {
                    [
                        text(r, "/id"),
                        text(r, "/type"),
                        text(r, "/issue/identifier"),
                        text(r, "/relatedIssue/identifier"),
                    ]
                })
                .collect(),
            "No Linear relations",
        ),
        List::Teams => table(
            ["TEAM", "NAME", "ID"],
            rows.iter()
                .map(|r| [text(r, "/key"), text(r, "/name"), text(r, "/id")])
                .collect(),
            "No Linear teams",
        ),
        List::States => table(
            ["STATE", "TYPE", "ID"],
            rows.iter()
                .map(|r| [text(r, "/name"), text(r, "/type"), text(r, "/id")])
                .collect(),
            "No Linear states",
        ),
        List::Labels => table(
            ["LABEL", "ID"],
            rows.iter()
                .map(|r| [text(r, "/name"), text(r, "/id")])
                .collect(),
            "No Linear labels",
        ),
        List::Members => table(
            ["MEMBER", "EMAIL", "ID"],
            rows.iter()
                .map(|r| [text(r, "/name"), text(r, "/email"), text(r, "/id")])
                .collect(),
            "No Linear members",
        ),
        List::Projects => table(
            ["PROJECT", "URL", "ID"],
            rows.iter()
                .map(|r| [text(r, "/name"), text(r, "/url"), text(r, "/id")])
                .collect(),
            "No Linear projects",
        ),
    };
    if data
        .pointer("/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        == Some(true)
    {
        human.push_str("\nMore results available");
        if let Some(cursor) = data.pointer("/pageInfo/endCursor").and_then(Value::as_str) {
            if !cursor.is_empty()
                && cursor
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_=/+".contains(&c))
            {
                human.push_str("; continue with --after=");
                human.push_str(cursor);
            } else {
                human.push_str("; use the exact pageInfo.endCursor from --json with --after");
            }
        }
    }
    human
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn readable_linear_collections_have_headers_and_explicit_empty_states() {
        let cases = [
            (List::Issues, "ISSUE", "No Linear issues", "ENG-7"),
            (
                List::Comments,
                "COMMENT",
                "No Linear comments",
                "Comment text",
            ),
            (
                List::Attachments,
                "ATTACHMENT",
                "No Linear attachments",
                "https://example.invalid",
            ),
            (List::Relations, "RELATION", "No Linear relations", "ENG-8"),
            (List::Teams, "TEAM", "No Linear teams", "A name"),
            (List::States, "STATE", "No Linear states", "started"),
            (List::Labels, "LABEL", "No Linear labels", "A name"),
            (
                List::Members,
                "MEMBER",
                "No Linear members",
                "member@example.invalid",
            ),
            (
                List::Projects,
                "PROJECT",
                "No Linear projects",
                "https://example.invalid",
            ),
        ];
        let row = json!({"id":"id1","identifier":"ENG-7","title":"Fix it","name":"A name","key":"ENG","type":"started","url":"https://example.invalid","email":"member@example.invalid","priority":2,"state":{"name":"In progress"},"assignee":null,"user":{"name":"Writer"},"body":"Comment text","issue":{"identifier":"ENG-7"},"relatedIssue":{"identifier":"ENG-8"}});
        for (kind, header, empty, expected) in cases {
            assert_eq!(render(kind, &json!({"nodes":[]})), empty);
            let result = render(kind, &json!({"nodes":[row.clone()]}));
            assert!(result.starts_with(header), "{kind:?}: {result}");
            assert_eq!(result.lines().count(), 2);
            assert!(result.contains(expected), "{kind:?}: {result}");
            for debug in ["Some(", "None", "{ ", "_ms"] {
                assert!(!result.contains(debug));
            }
        }
        let issues = render(List::Issues, &json!({"nodes":[row]}));
        assert_eq!(
            issues,
            "ISSUE  TITLE   STATE        PRIORITY  ASSIGNEE\nENG-7  Fix it  In progress  high      unassigned"
        );
    }

    #[test]
    fn readable_linear_output_sanitizes_controls_and_keeps_pagination_visible() {
        let data = json!({"nodes":[{"name":"界\n\u{1b}label","id":"one"}],"pageInfo":{"hasNextPage":true,"endCursor":"cursor\nnext"}});
        let before = data.clone();
        let output = render(List::Labels, &data);
        assert!(output.starts_with("LABEL"));
        assert!(output.contains("界  label"));
        assert!(output.contains(
            "More results available; use the exact pageInfo.endCursor from --json with --after"
        ));
        assert!(!output.contains("cursor next"));
        let next = render(
            List::Labels,
            &json!({"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":"-cursor_2+/="}}),
        );
        assert!(next.contains("--after=-cursor_2+/="));
        assert!(!output.contains('\u{1b}'));
        assert_eq!(
            data, before,
            "the API JSON is never normalized by presentation"
        );
        assert!(render(List::Issues, &json!({"errors":[]})).contains("unrecognized list"));
    }
}
