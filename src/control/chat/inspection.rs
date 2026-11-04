//! One cache-only page through the authority-bearing Agent route. No owner
//! resolver, presentation attachment, terminal authentication or follow loop.
use super::super::*;
use crate::agent_execution::{
    Operation, Reply,
    inspection::{Coverage, Cursor, Kind, Page},
};

#[derive(Debug, clap::Args)]
pub struct ReadArgs {
    /// Full native session ID from cagents ls --json; short agent handles are not accepted.
    session: TerminalId,
    /// Opaque continuation returned by the previous page for this same session.
    #[arg(long, value_parser = Cursor::parse)]
    cursor: Option<Cursor>,
}

pub(super) fn execute(arguments: ReadArgs) -> Result<CommandOutput, AppError> {
    let authority = Authority::from_environment();
    let socket = control_daemon_socket()?;
    let mut client = ControlClient::connect(&socket)
        .map_err(|error| AppError::new("daemon_connection", error))?;
    execute_request(arguments, authority, |authority, operation| {
        client.agent(authority, operation)
    })
}

fn execute_request(
    arguments: ReadArgs,
    authority: Authority,
    mut request: impl FnMut(Authority, Operation) -> Result<Reply, ControlError>,
) -> Result<CommandOutput, AppError> {
    let ReadArgs { session, cursor } = arguments;
    let reply = request(
        authority,
        Operation::InspectChat {
            session,
            cursor: cursor.clone(),
        },
    )
    .map_err(protocol_error)?;
    let Reply::ChatPage(page) = reply else {
        return Err(AppError::new(
            "chat_read",
            "The daemon did not return a peer conversation page",
        ));
    };
    page.validate_for(session)
        .map_err(|error| AppError::new("chat_read", error))?;
    if !page.at_end
        && page
            .next_cursor
            .as_ref()
            .zip(cursor.as_ref())
            .is_some_and(|(next, previous)| next == previous)
    {
        return Err(AppError::new(
            "chat_read",
            "The conversation cursor did not advance; no page was published",
        ));
    }
    page_output(page)
}

fn page_output(page: Page) -> Result<CommandOutput, AppError> {
    let mut human = format!("Cached conversation for {}\n", page.session);
    human.push_str(match page.coverage {
        Coverage::Cached => {
            "Coverage: available cache only; earlier provider history may be absent.\n"
        }
        Coverage::Partial => "Coverage: partial cache; earlier conversation content is missing.\n",
    });
    if page.parts.is_empty() {
        human.push_str("\nNo visible cached content in this page.\n");
    }
    for part in &page.parts {
        let heading = match &part.kind {
            Kind::UserText => "User".to_owned(),
            Kind::AssistantText {
                phase: crate::chat::Phase::Final,
            } => "Assistant".to_owned(),
            Kind::AssistantText {
                phase: crate::chat::Phase::Commentary,
            } => "Assistant progress".to_owned(),
            Kind::ToolOutput { name, status } => {
                format!(
                    "Tool output: {} ({})",
                    format::plain(name),
                    status_label(*status)
                )
            }
            Kind::CommandOutput { status, exit_code } => match exit_code {
                Some(code) => format!("Command output ({}, exit {code})", status_label(*status)),
                None => format!("Command output ({})", status_label(*status)),
            },
        };
        human.push('\n');
        human.push_str(&heading);
        if part.offset > 0 {
            human.push_str(&format!(" (continued at byte {})", part.offset));
        }
        human.push('\n');
        // Preserve readable multiline output without letting a transcript
        // control sequence become a terminal command. JSON retains exact text.
        for character in part.text.chars() {
            if character.is_control() && !matches!(character, '\n' | '\t') {
                human.extend(character.escape_default());
            } else {
                human.push(character);
            }
        }
        if !part.text.ends_with('\n') {
            human.push('\n');
        }
        if !part.complete {
            human.push_str("[Cached text continues]\n");
        }
    }
    if let Some(cursor) = &page.next_cursor {
        human.push_str("\nMore cached content may follow. Next cursor (--cursor): ");
        human.push_str(cursor.as_str());
        human.push('\n');
    } else {
        human.push_str("\nEnd of currently available cached content.\n");
    }
    let data = serde_json::to_value(page)
        .map_err(|_| AppError::new("chat_read", "The conversation page could not be encoded"))?;
    Ok(CommandOutput {
        command: "chat.read",
        data,
        human,
    })
}

fn status_label(status: crate::chat::ToolStatus) -> &'static str {
    match status {
        crate::chat::ToolStatus::Running => "running",
        crate::chat::ToolStatus::Succeeded => "succeeded",
        crate::chat::ToolStatus::Failed => "failed",
        crate::chat::ToolStatus::Denied => "denied",
        crate::chat::ToolStatus::Stopped => "stopped",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_execution::inspection::{MAX_CURSOR_BYTES, Part};

    fn arguments(session: TerminalId, cursor: Option<Cursor>) -> ReadArgs {
        ReadArgs { session, cursor }
    }

    fn ended(session: TerminalId, coverage: Coverage) -> Page {
        let mut page = Page::new(session, coverage);
        page.at_end = true;
        page
    }

    #[test]
    fn chat_read_parser_uses_full_session_and_rejects_scope_and_unbounded_options() {
        let session = TerminalId::new().to_string();
        let parsed = Cli::try_parse_from([
            "cagents",
            "chat",
            "read",
            &session,
            "--cursor",
            "page-2:opaque",
        ])
        .unwrap();
        let TopCommand::Chat {
            command: super::super::ChatCommand::Read(read),
        } = parsed.command
        else {
            panic!("chat read did not reach its command");
        };
        assert_eq!(read.session.to_string(), session);
        assert_eq!(read.cursor.unwrap().as_str(), "page-2:opaque");
        assert!(Cli::try_parse_from(["cagents", "chat", "read", &session[..8]]).is_err());
        for option in [
            "--channel",
            "--project",
            "--path",
            "--limit",
            "--follow",
            "--all",
        ] {
            assert!(
                Cli::try_parse_from(["cagents", "chat", "read", &session, option, "value"])
                    .is_err(),
                "unexpected caller-selected scope or budget: {option}"
            );
        }
        for cursor in [
            String::new(),
            "x".repeat(MAX_CURSOR_BYTES + 1),
            "../raw/path".into(),
        ] {
            assert!(
                Cli::try_parse_from(["cagents", "chat", "read", &session, "--cursor", &cursor])
                    .is_err()
            );
        }
        let help = Cli::try_parse_from(["cagents", "chat", "read", "--help"])
            .unwrap_err()
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for phrase in ["ls --json", "cached", "--cursor", "SESSION"] {
            assert!(
                help.contains(phrase),
                "missing discovery/coverage help: {phrase}"
            );
        }
    }

    #[test]
    fn peer_read_uses_one_exact_authority_request_without_owner_resolution_or_management() {
        let session = TerminalId::new();
        let authority = Authority::session("inert-cli-peer-read-capability".into());
        let cursor = Cursor::parse("page-2:opaque").unwrap();
        let mut calls = 0;
        let result = execute_request(arguments(session, Some(cursor.clone())), authority.clone(),
            |received, operation| {
                calls += 1;
                assert_eq!(received, authority);
                assert!(matches!(operation, Operation::InspectChat { session: target, cursor: Some(value) }
                    if target == session && value == cursor));
                Ok(Reply::ChatPage(ended(session, Coverage::Cached)))
            }).unwrap();
        assert_eq!(calls, 1);
        assert_eq!(result.command, "chat.read");
        assert_eq!(result.data["session"], json!(session));

        // An accidental management-route call cannot turn this into a raw
        // ChatRead/Authenticate request or resolve an owned agent handle.
        let error = super::super::execute_requests(
            super::super::ChatCommand::Read(arguments(session, None)),
            |_| panic!("peer read entered terminal management RPC"),
        )
        .err()
        .expect("wrong route must refuse");
        assert_eq!(error.kind, "chat_read_route");
    }

    #[test]
    fn peer_read_refuses_wrong_reply_target_schema_and_nonadvancing_page() {
        let session = TerminalId::new();
        let cursor = Cursor::parse("page-2:opaque").unwrap();
        let mut wrong_schema = ended(session, Coverage::Cached);
        wrong_schema.schema_version += 1;
        let mut repeated = Page::new(session, Coverage::Cached);
        repeated.next_cursor = Some(cursor.clone());
        for (reply, requested_cursor) in [
            (Reply::Listed(Vec::new()), None),
            (
                Reply::ChatPage(ended(TerminalId::new(), Coverage::Cached)),
                None,
            ),
            (Reply::ChatPage(wrong_schema), None),
            (Reply::ChatPage(repeated), Some(cursor)),
        ] {
            let mut reply = Some(reply);
            let result = execute_request(
                arguments(session, requested_cursor),
                Authority::default(),
                |_, _| Ok(reply.take().expect("one bounded call")),
            );
            assert!(result.is_err(), "invalid reply must not publish a page");
        }
    }

    #[test]
    fn peer_page_keeps_exact_json_and_readable_safe_text_with_truthful_cache_coverage() {
        let session = TerminalId::new();
        let mut page = Page::new(session, Coverage::Partial);
        page.next_cursor = Some(Cursor::parse("page-3:opaque").unwrap());
        page.parts = vec![
            Part {
                item_id: "assistant-1".into(),
                offset: 17,
                text: "Unicode 界\nnext\tcolumn\u{1b}]52;clipboard".into(),
                complete: false,
                kind: Kind::AssistantText {
                    phase: crate::chat::Phase::Commentary,
                },
                started_at: None,
                at: None,
            },
            Part {
                item_id: "tool-1".into(),
                offset: 0,
                text: "visible tool output".into(),
                complete: true,
                kind: Kind::ToolOutput {
                    name: "Build".into(),
                    status: crate::chat::ToolStatus::Failed,
                },
                started_at: None,
                at: None,
            },
        ];
        page.validate_for(session).unwrap();
        let exact = serde_json::to_value(&page).unwrap();
        let output = page_output(page).unwrap();
        assert_eq!(output.data, exact);
        assert!(
            output
                .human
                .contains("earlier conversation content is missing")
        );
        assert!(
            output
                .human
                .contains("Assistant progress (continued at byte 17)")
        );
        assert!(output.human.contains("Unicode 界\nnext\tcolumn"));
        assert!(!output.human.contains('\u{1b}'));
        assert!(output.human.contains("\\u{1b}]52;clipboard"));
        assert!(output.human.contains("Tool output: Build (failed)"));
        assert!(
            output
                .human
                .contains("Next cursor (--cursor): page-3:opaque")
        );
        let empty = page_output(ended(session, Coverage::Cached)).unwrap();
        assert!(
            empty
                .human
                .contains("earlier provider history may be absent")
        );
        assert!(
            empty
                .human
                .contains("No visible cached content in this page")
        );
        assert!(
            empty
                .human
                .contains("End of currently available cached content")
        );
    }
}
