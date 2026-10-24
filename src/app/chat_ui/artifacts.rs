//! Explicit review links are actions, not inferred filesystem attachments.
use super::*;

const MAX_TEXT: usize = 16 * 1024;
const MAX_LINKS: usize = 16;

/// Extract only links the producer actually supplied in Markdown. Bare paths,
/// shell shorthand, and code blocks remain text; this never probes a directory.
pub(super) fn links(request: &chat::Request) -> Vec<(String, String)> {
    if request.kind != chat::RequestKind::Question {
        return Vec::new();
    }
    let mut output = Vec::new();
    let mut remaining = MAX_TEXT;
    let texts = std::iter::once(request.description.as_str()).chain(
        request.fields.iter().flat_map(|field| {
            [field.label.as_str(), field.description.as_str()]
                .into_iter()
                .chain(
                    field
                        .choices
                        .iter()
                        .flat_map(|choice| [choice.label.as_str(), choice.description.as_str()]),
                )
        }),
    );
    for text in texts {
        if remaining == 0 || output.len() == MAX_LINKS {
            break;
        }
        let end = text.floor_char_boundary(text.len().min(remaining));
        remaining -= end;
        let text = &text[..end];
        if !text.contains(['[', '<']) {
            continue;
        }
        collect(text, &mut output);
    }
    output
}

fn add(label: &str, uri: &str, output: &mut Vec<(String, String)>) {
    if output.len() == MAX_LINKS
        || uri.is_empty()
        || uri.starts_with('#')
        || uri.len() > 4096
        || output.iter().any(|(_, old)| old == uri)
    {
        return;
    }
    let web = match Url::parse(uri) {
        Ok(url) if matches!(url.scheme(), "https" | "http") => url.host_str().is_some(),
        Ok(url) if url.scheme() == "file" && url.to_file_path().is_ok() => false,
        Ok(_) => return,
        Err(_) => false,
    };
    // A producer can encode a literal metacharacter in an explicit file URL.
    // Unescaped shell patterns do not become artifact buttons.
    if !web
        && (uri.contains(['*', '?', '[', ']'])
            || uri.contains('{') && uri.contains(',') && uri.contains('}'))
    {
        return;
    }
    let label = if label.trim().is_empty() {
        "artifact"
    } else {
        label.trim()
    };
    output.push((Runtime::short_description(label, 80), uri.to_owned()));
}

fn collect(text: &str, output: &mut Vec<(String, String)>) {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    // Read the explicit CommonMark link events, not the display parser's
    // paragraph/image projections. Mixed inline images must not consume an
    // earlier link's label or destination.
    let mut pending = Vec::<(usize, String, String)>::new();
    let mut depth = 0usize;
    for event in Parser::new(text).take(4096) {
        if output.len() == MAX_LINKS {
            break;
        }
        match event {
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                depth += 1;
                if depth <= 16 {
                    pending.push((depth, dest_url.into_string(), String::new()));
                }
            }
            Event::Text(text) | Event::Code(text) => {
                for (_, _, label) in &mut pending {
                    let end = text
                        .floor_char_boundary(text.len().min(320usize.saturating_sub(label.len())));
                    label.push_str(&text[..end]);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                for (_, _, label) in &mut pending {
                    if label.len() < 320 {
                        label.push(' ');
                    }
                }
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                if pending.last().is_some_and(|(level, _, _)| *level == depth) {
                    let (_, uri, label) = pending.pop().unwrap();
                    add(&label, &uri, output);
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn question(text: &str) -> chat::Request {
        chat::Request {
            kind: chat::RequestKind::Question,
            id: "review".into(),
            title: "Review".into(),
            description: text.into(),
            fields: vec![],
            actions: vec![],
        }
    }
    #[test]
    fn review_artifacts_require_explicit_concrete_links_and_keep_literal_names() {
        let text = "Screenshots: tree-real-*.png and tree-{collapsed,expanded}.png. [shorthand](tree-real-*.png) [brace](tree-{collapsed,expanded}.png) [collapsed](docs/tree-collapsed.png) ![expanded](docs/tree-expanded.png) [duplicate](docs/tree-collapsed.png) [literal](file:///tmp/literal%2A%20%23%E7%95%8C.png) [unsafe](javascript:alert) ` [code](hidden.png) `";
        assert_eq!(
            links(&question(text)),
            vec![
                ("collapsed".into(), "docs/tree-collapsed.png".into()),
                ("expanded".into(), "docs/tree-expanded.png".into()),
                (
                    "literal".into(),
                    "file:///tmp/literal%2A%20%23%E7%95%8C.png".into()
                ),
            ]
        );
        let mut approval = question("[file](docs/image.png)");
        approval.kind = chat::RequestKind::Approval;
        assert!(
            links(&approval).is_empty(),
            "approval payloads are not review artifacts"
        );
    }
    #[test]
    fn review_artifact_selection_is_bounded_without_scanning_the_filesystem() {
        let many = (0..100)
            .map(|i| format!("[image{i}](docs/{i}.png) "))
            .collect::<String>();
        assert_eq!(links(&question(&many)).len(), MAX_LINKS);
        let oversized = format!("{} [late](hidden.png)", "界".repeat(MAX_TEXT));
        assert!(links(&question(&oversized)).is_empty());
    }

    #[test]
    fn review_artifacts_require_a_destination_beyond_the_question_itself() {
        assert_eq!(
            links(&question(
                "[empty]() [section](#details) [real](docs/image.png#details)"
            )),
            vec![("real".into(), "docs/image.png#details".into())]
        );
    }
}
