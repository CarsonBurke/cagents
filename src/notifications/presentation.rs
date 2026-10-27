//! Plain, bounded notification text. Desktop servers own the toast's layout;
//! provider Markdown and terminal control sequences are never toast markup.

const INPUT_LIMIT: usize = 8192;
pub const PREVIEW_LIMIT: usize = 240;
pub const TITLE_LIMIT: usize = 80;

#[derive(Debug, PartialEq, Eq)]
pub struct Message {
    pub title: String,
    pub body: String,
    pub action: &'static str,
    pub needs_input: bool,
}

pub fn message(event: &super::Event, previews: bool) -> Message {
    use super::{Kind, RequestType, Target};
    let content = previews.then_some(event.content.as_ref()).flatten();
    let outcome = match event.evidence.kind {
        Kind::Succeeded => "Response ready",
        Kind::Failed => "Run failed",
        Kind::Cancelled => "Run cancelled",
        Kind::Interrupted => "Outcome unconfirmed",
        Kind::Waiting => match content.and_then(|content| content.request) {
            Some(RequestType::Approval) => "Approval needed",
            Some(RequestType::Question) => "Question for you",
            Some(RequestType::Consent) => "Consent requested",
            None => "Needs your input",
        },
        Kind::ReviewRequested => "Review requested",
        Kind::MessageReceived => "Messages waiting",
        Kind::AutomationPaused => "Automation pause recorded",
        Kind::AutomationBlocked => "Automation block recorded",
        Kind::ProposalCreated => "Automation request created",
    };
    let inbox = matches!(event.target, Target::SessionInbox(_));
    let context = if inbox {
        String::new()
    } else {
        title(&super::context(event))
    };
    let conversation = content
        .map(|content| title(&content.conversation))
        .filter(|value| !value.is_empty());
    let title = conversation.unwrap_or_else(|| {
        if context.is_empty() {
            outcome.into()
        } else {
            context.clone()
        }
    });
    let excerpt = content
        .map(|content| plain_preview(&content.preview))
        .filter(|value| !value.is_empty());
    // Some desktop servers show only the first body line. Put the useful
    // context beside its event distinction, ahead of secondary attribution.
    let mut body = match &excerpt {
        Some(excerpt) => format!("{outcome}: {excerpt}"),
        None => outcome.to_owned(),
    };
    if inbox {
        body.push('\n');
        body.push_str(&super::context(event));
    }
    if !context.is_empty() && context != title {
        body.push_str(if excerpt.is_some() { "\n" } else { " · " });
        body.push_str(&context);
    }
    Message {
        title,
        body,
        action: match event.target {
            Target::PullRequest(_) => "Open review",
            Target::SessionInbox(_) => "Open terminal",
            Target::Automation { .. } => "Open history",
            Target::AutomationProposal { .. } => "Review request",
            _ => "Open conversation",
        },
        needs_input: matches!(
            event.evidence.kind,
            Kind::Waiting
                | Kind::ReviewRequested
                | Kind::MessageReceived
                | Kind::AutomationPaused
                | Kind::AutomationBlocked
                | Kind::ProposalCreated
        ),
    }
}

/// A bounded excerpt for a system surface. Prefer prose over fenced code;
/// retain a short code excerpt when the response consists entirely of code.
/// Never inspect more than 8 KiB, even for an adversarial long provider item.
pub fn preview(text: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let clean = without_controls(text);
    let mut prose = String::new();
    let mut code = String::new();
    let mut code_block = false;
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(&clean, options) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => code_block = true,
            Event::End(TagEnd::CodeBlock) => {
                code_block = false;
                code.push(' ');
            }
            Event::Text(text) | Event::Code(text) => {
                if code_block {
                    code.push_str(&text);
                } else {
                    prose.push_str(&text);
                }
            }
            Event::SoftBreak
            | Event::HardBreak
            | Event::Rule
            | Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::BlockQuote(_)
                | TagEnd::Item
                | TagEnd::TableCell
                | TagEnd::TableRow
                | TagEnd::TableHead
                | TagEnd::Table,
            ) => {
                if code_block {
                    code.push(' ');
                } else {
                    prose.push(' ');
                }
            }
            // HTML tags, link destinations, language markers and formatting
            // delimiters are presentation, not the user's readable excerpt.
            _ => {}
        }
    }
    bounded_words(
        if prose.trim().is_empty() {
            &code
        } else {
            &prose
        },
        PREVIEW_LIMIT,
    )
}

pub fn title(text: &str) -> String {
    bounded_words(&without_controls(text), TITLE_LIMIT)
}

/// Content is already plain text from the producer. Bound/control-filter it
/// without interpreting literal code punctuation as a second Markdown layer.
pub fn plain_preview(text: &str) -> String {
    bounded_words(&without_controls(text), PREVIEW_LIMIT)
}

fn without_controls(text: &str) -> String {
    // Work on a UTF-8 boundary and bound scanning before sanitization.
    let mut end = text.len().min(INPUT_LIMIT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = String::new();
    let mut chars = text[..end].chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for control in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&control) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(control) = chars.next() {
                        if control == '\u{7}' {
                            break;
                        }
                        if control == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if matches!(character, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
        {
            continue;
        }
        if matches!(character, '\n' | '\r' | '\t') {
            result.push(if character == '\n' { '\n' } else { ' ' });
        } else if !character.is_control() {
            // The portal body is plain text. Preserve literal operator and
            // identifier punctuation; Markdown parsing happens only at source.
            result.push(character);
        }
    }
    result
}

fn bounded_words(text: &str, limit: usize) -> String {
    let mut result = String::new();
    let mut count = 0;
    let mut pending_space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            pending_space = !result.is_empty();
            continue;
        }
        let needed = 1 + usize::from(pending_space);
        if count + needed > limit {
            while result.ends_with(' ') {
                result.pop();
            }
            if count == limit {
                result.pop();
            }
            result.push('…');
            return result;
        }
        if pending_space {
            result.push(' ');
            count += 1;
        }
        result.push(character);
        count += 1;
        pending_space = false;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_content_is_opt_in_and_request_actions_never_approve() {
        let mut event = crate::notifications::Event::session(
            crate::model::TerminalId::new(),
            crate::agent::Provider::Codex,
            "/project/worktree".into(),
            crate::notifications::Evidence {
                key: "request-one".into(),
                kind: crate::notifications::Kind::Waiting,
            },
        );
        event.content = Some(crate::notifications::Content {
            conversation: "Refactor authentication".into(),
            preview: "May I edit the login handler?".into(),
            request: Some(crate::notifications::RequestType::Approval),
        });
        let private = message(&event, false);
        assert!(!private.title.contains("authentication"));
        assert!(!private.body.contains("login"));
        let rich = message(&event, true);
        assert_eq!(rich.title, "Refactor authentication");
        assert_eq!(
            rich.body,
            "Approval needed: May I edit the login handler?\nCodex · worktree"
        );
        assert_eq!(rich.action, "Open conversation");
        assert!(rich.needs_input);
    }

    #[test]
    fn collapsed_desktop_captions_keep_the_outcome_and_useful_excerpt_together() {
        use crate::notifications::{Content, Event, Evidence, Kind, RequestType};
        for (kind, request, excerpt, expected) in [
            (
                Kind::Succeeded,
                None,
                "Migration ready. Preserved config_file.rs and verified the rollback plan.",
                "Response ready: Migration ready. Preserved config_file.rs and verified the rollback plan.",
            ),
            (
                Kind::Waiting,
                Some(RequestType::Question),
                "Updated B: preserve config_file.rs?",
                "Question for you: Updated B: preserve config_file.rs?",
            ),
            (
                Kind::Failed,
                None,
                "Migration validation failed: config_file.rs contains an incompatible schema.",
                "Run failed: Migration validation failed: config_file.rs contains an incompatible schema.",
            ),
        ] {
            let mut event = Event::session(
                crate::model::TerminalId::new(),
                crate::agent::Provider::Codex,
                "/project/worktree".into(),
                Evidence {
                    key: "caption".into(),
                    kind,
                },
            );
            event.content = Some(Content {
                conversation: "Migration review A".into(),
                preview: excerpt.into(),
                request,
            });
            let rich = message(&event, true);
            assert_eq!(rich.title, "Migration review A");
            assert_eq!(rich.body.lines().next(), Some(expected));
            assert_eq!(rich.body.lines().nth(1), Some("Codex · worktree"));
            // Privacy-off output remains the original generic presentation.
            let private = message(&event, false);
            assert!(!private.body.contains(excerpt));
            assert!(!private.title.contains("Migration"));
        }
    }

    #[test]
    fn empty_or_disabled_previews_keep_the_original_body_layout() {
        use crate::notifications::{Content, Event, Evidence, Kind};
        let mut event = Event::session(
            crate::model::TerminalId::new(),
            crate::agent::Provider::Codex,
            "/project/worktree".into(),
            Evidence {
                key: "empty-caption".into(),
                kind: Kind::Succeeded,
            },
        );
        let original = message(&event, false);
        assert_eq!(original.title, "Codex · worktree");
        assert_eq!(original.body, "Response ready");
        event.content = Some(Content {
            conversation: "Migration review A".into(),
            preview: String::new(),
            request: None,
        });
        assert_eq!(message(&event, false), original);
        assert_eq!(
            message(&event, true).body,
            "Response ready · Codex · worktree"
        );
    }

    #[test]
    fn prose_wins_over_code_and_control_garbage() {
        let input = "\u{1b}[32m## Finished\u{1b}[0m\n\nUpdated **authentication**.\n```rust\nfn secret() {}\n```\n[Tests passed](https://private.example/token).\u{202e}";
        assert_eq!(
            preview(input),
            "Finished Updated authentication. Tests passed."
        );
    }

    #[test]
    fn code_only_is_readable_without_fence_or_language_marker() {
        assert_eq!(
            preview("```rust\nlet answer = 42;\n```"),
            "let answer = 42;"
        );
        assert_eq!(
            preview("Fixed #1261 in `notification_preview`."),
            "Fixed #1261 in notification_preview."
        );
    }

    #[test]
    fn hyperlinks_ansi_osc_and_html_do_not_reach_desktop() {
        assert_eq!(
            preview("\u{1b}]8;;https://private\u{1b}\\Read this\u{1b}]8;;\u{1b}\\ <b>now</b>\u{0}"),
            "Read this now"
        );
    }

    #[test]
    fn long_unicode_results_have_bounded_single_line_previews() {
        let result = preview(&format!("Result:\n{}", "🦀 ".repeat(100_000)));
        assert!(result.chars().count() <= PREVIEW_LIMIT);
        assert!(result.ends_with('…'));
        assert!(!result.contains('\n'));
        assert!(title(&"🦀".repeat(1000)).chars().count() <= TITLE_LIMIT);
    }

    #[test]
    fn automation_captions_describe_recorded_history_and_never_offer_execution() {
        use crate::notifications::{Content, Event, Evidence, Kind, Target};
        let id = uuid::Uuid::new_v4();
        for (kind, caption) in [
            (Kind::AutomationPaused, "Automation pause recorded"),
            (Kind::AutomationBlocked, "Automation block recorded"),
        ] {
            let event = Event {
                target: Target::Automation { id, sequence: 17 },
                source_session: None,
                provider: None,
                directory: None,
                destination: None,
                evidence: Evidence {
                    key: format!("automation:{id}:lifecycle:17"),
                    kind,
                },
                content: Some(Content {
                    conversation: "PRIVATE_AUTOMATION".into(),
                    preview: "Project closed".into(),
                    request: None,
                }),
            };
            event.validate().unwrap();
            let private = message(&event, false);
            assert_eq!(private.title, caption);
            assert_eq!(private.body, caption);
            assert_eq!(private.action, "Open history");
            assert!(private.needs_input);
            assert!(!private.title.contains("PRIVATE") && !private.body.contains("Project closed"));
            let rich = message(&event, true);
            assert_eq!(rich.title, "PRIVATE_AUTOMATION");
            assert_eq!(rich.body, format!("{caption}: Project closed"));
            assert_eq!(rich.action, "Open history");
            assert_eq!(event.evidence.kind.label(), caption);
        }
    }
}
