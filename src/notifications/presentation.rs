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
    let mut body = outcome.to_owned();
    if inbox {
        body.push('\n');
        body.push_str(&super::context(event));
    }
    if !context.is_empty() && context != title {
        body.push_str(" · ");
        body.push_str(&context);
    }
    if let Some(excerpt) = content
        .map(|content| plain_preview(&content.preview))
        .filter(|value| !value.is_empty())
    {
        body.push('\n');
        body.push_str(&excerpt);
    }
    Message {
        title,
        body,
        action: match event.target {
            Target::PullRequest(_) => "Open review",
            Target::SessionInbox(_) => "Open terminal",
            _ => "Open conversation",
        },
        needs_input: matches!(
            event.evidence.kind,
            Kind::Waiting | Kind::ReviewRequested | Kind::MessageReceived
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
        assert!(
            rich.body
                .starts_with("Approval needed · Codex · worktree\n")
        );
        assert!(rich.body.ends_with("May I edit the login handler?"));
        assert_eq!(rich.action, "Open conversation");
        assert!(rich.needs_input);
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
}
