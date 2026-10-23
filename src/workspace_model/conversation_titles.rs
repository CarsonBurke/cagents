//! Cheap fallback naming, shared by live conversations and resume discovery.
//! Provider names remain preferable. This only examines a bounded prompt when
//! conversation content changes; it does not run inference or read history.

const INPUT_BYTES: usize = 32 * 1024;
const INPUT_LINES: usize = 128;

pub fn channel_title(text: &str) -> Option<String> {
    let title = crate::names::normalize_card_name(text)?;
    // Provider names can be numeric or emoji-only. Task meaningfulness belongs
    // to the prompt candidate policy, not this authoritative label sanitizer.
    if title.chars().count() <= 80 {
        return Some(title);
    }
    let shortened: String = title.chars().take(79).collect();
    let end = shortened
        .rfind(' ')
        .filter(|end| *end > shortened.len() / 2);
    Some(format!(
        "{}…",
        end.map_or(shortened.as_str(), |end| &shortened[..end])
            .trim_end()
    ))
}

/// Prefer a task-bearing instruction to greetings, quoted context, commands
/// and pasted scaffolding. Equal candidates retain their original order.
pub fn channel_title_from_prompt(prompt: &str) -> Option<String> {
    let limit = prompt.floor_char_boundary(prompt.len().min(INPUT_BYTES));
    let mut fence = None;
    let mut context_depth = 0usize;
    let mut best: Option<(usize, String)> = None;
    for line in prompt[..limit].lines().take(INPUT_LINES) {
        let mut line = line.trim();
        let quoted = line.starts_with('>');
        line = line.trim_start_matches(['>', ' ']);
        if line.starts_with("```") || line.starts_with("~~~") {
            let marker = line.as_bytes()[0];
            if fence == Some(marker) {
                fence = None;
            } else if fence.is_none() {
                fence = Some(marker);
            }
            continue;
        }
        if fence.is_some() {
            continue;
        }
        if matches!(
            line,
            "<INSTRUCTIONS>" | "<environment_context>" | "<system-reminder>"
        ) {
            context_depth += 1;
            continue;
        }
        if matches!(
            line,
            "</INSTRUCTIONS>" | "</environment_context>" | "</system-reminder>"
        ) {
            context_depth = context_depth.saturating_sub(1);
            continue;
        }
        if context_depth != 0 || line.starts_with("# AGENTS.md") || line.starts_with('<') {
            continue;
        }
        let Some((score, title)) = candidate(line, quoted) else {
            continue;
        };
        if best.as_ref().is_none_or(|(previous, _)| score > *previous) {
            best = Some((score, title));
        }
    }
    best.map(|(_, title)| title)
}

fn candidate(line: &str, quoted: bool) -> Option<(usize, String)> {
    let line = line.trim_start_matches(['#', '*', '-', ' ', '"', '“', '”']);
    let line = line
        .split_once(". ")
        .filter(|(prefix, _)| !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()))
        .map_or(line, |(_, rest)| rest);
    let line = strip_prefix(
        line,
        &[
            "task:",
            "request:",
            "please ",
            "can you ",
            "could you ",
            "i want you to ",
            "i'd like you to ",
            "help me ",
        ],
    );
    if line.is_empty() || line.starts_with(['$', '@']) || reference(line) {
        return None;
    }
    let lower = line.to_ascii_lowercase();
    if [
        "you are ",
        "act as ",
        "context:",
        "instructions:",
        "hello",
        "hi there",
        "user request",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
    {
        return None;
    }
    // A skill invocation may carry the actual task; the invocation alone does
    // not. Keep its arguments, which identify the work rather than the skill.
    let line = if line.starts_with('/') {
        let (_, arguments) = line.split_once(char::is_whitespace)?;
        arguments.trim()
    } else {
        line
    };
    if reference(line) {
        return None;
    }
    let line = line.trim_end_matches(['"', '“', '”', '.', '?', '!']);
    let words: Vec<_> = line.split_whitespace().take(48).collect();
    if words.len() < 2 {
        return None;
    }
    let task = words.iter().take(4).any(|word| {
        matches!(
            word.trim_matches(|c: char| !c.is_alphabetic())
                .to_ascii_lowercase()
                .as_str(),
            "review"
                | "investigate"
                | "fix"
                | "implement"
                | "build"
                | "add"
                | "remove"
                | "improve"
                | "analyze"
                | "analyse"
                | "compare"
                | "optimize"
                | "debug"
                | "design"
                | "test"
                | "audit"
                | "explain"
                | "find"
                | "update"
                | "refactor"
                | "check"
                | "evaluate"
                | "resolve"
        )
    });
    if words.first().is_some_and(|word| {
        matches!(
            *word,
            "cargo"
                | "git"
                | "npm"
                | "pnpm"
                | "yarn"
                | "python"
                | "python3"
                | "cd"
                | "curl"
                | "wget"
        )
    }) {
        return None;
    }
    let meaningful = words
        .iter()
        .filter(|word| word.chars().filter(|c| c.is_alphabetic()).count() >= 3)
        .count();
    if meaningful < 2 && !task {
        return None;
    }
    let text = line.replace('`', "");
    let title = channel_title(&text)?;
    let score = meaningful.min(12) + usize::from(task) * 64 + usize::from(!quoted) * 16;
    Some((score, title))
}

fn strip_prefix<'a>(mut text: &'a str, prefixes: &[&str]) -> &'a str {
    for _ in 0..3 {
        let Some(prefix) = prefixes.iter().find(|prefix| {
            text.get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        }) else {
            break;
        };
        text = text[prefix.len()..].trim_start();
    }
    text
}

fn reference(text: &str) -> bool {
    let text = text.trim();
    text.starts_with("https://")
        || text.starts_with("http://")
        || (!text.contains(char::is_whitespace)
            && (text.starts_with('/') || text.starts_with("./") || text.starts_with("../")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_beats_quotation_scaffolding_commands_and_links() {
        let prompt = "\"\n> You are an expert reviewer of this repository\n> Lots of supplied background about unrelated implementation details\n\"\nhttps://example.com/bundle\ncargo bench --release\n```sh\nopenssl enc -aes-256-cbc\n```\nPlease review bundle encryption and performance";
        assert_eq!(
            channel_title_from_prompt(prompt).as_deref(),
            Some("review bundle encryption and performance")
        );
    }

    #[test]
    fn empty_attachments_punctuation_and_bare_invocations_are_not_titles() {
        for prompt in [
            "\"",
            "---",
            "<image source='attachment' />",
            "/review",
            "https://example.com/a",
            "/home/user/work",
            "cargo test --locked",
            "cargo build --release",
            "git diff --check",
            "npm test",
        ] {
            assert_eq!(channel_title_from_prompt(prompt), None, "{prompt}");
        }
    }

    #[test]
    fn skill_arguments_and_quoted_task_have_meaningful_fallbacks() {
        assert_eq!(
            channel_title_from_prompt("/review Fix bundle encryption regression").as_deref(),
            Some("Fix bundle encryption regression")
        );
        assert_eq!(
            channel_title_from_prompt("> Review bundle encryption performance").as_deref(),
            Some("Review bundle encryption performance")
        );
        assert_eq!(
            channel_title_from_prompt("```rust\nlet ignored = 1;\n```\n- Fix history paging")
                .as_deref(),
            Some("Fix history paging")
        );
    }

    #[test]
    fn a_short_live_task_outranks_a_long_quoted_task() {
        assert_eq!(
            channel_title_from_prompt(
                "> Review the old implementation across all of the existing packages\nFix UI"
            )
            .as_deref(),
            Some("Fix UI")
        );
    }

    #[test]
    fn authoritative_names_preserve_numeric_and_emoji_labels() {
        for name in ["1341", "🔐", "🧑‍💻", "\"\""] {
            assert_eq!(channel_title(name).as_deref(), Some(name));
            assert!(channel_title_from_prompt(name).is_none());
        }
        assert_eq!(channel_title(" \u{202e}1341\n ").as_deref(), Some("1341"));
        assert!(channel_title(" \u{202e}\n ").is_none());
    }

    #[test]
    fn unicode_titles_are_bounded_and_complete() {
        let title = channel_title(&"修复会话标题与恢复列表 ".repeat(30)).unwrap();
        assert!(title.chars().count() <= 80);
        assert!(title.ends_with('…'));
        assert!(channel_title_from_prompt("\"\"").is_none());
    }
}
