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
    let style = design::markdown_settings(&cosmic::theme::active(), design::TEXT_BODY).style;
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
        let items: Vec<_> = markdown::parse(text).take(128).collect();
        collect(&items, style, 0, &mut output);
    }
    output
}

fn add(label: &str, uri: &str, output: &mut Vec<(String, String)>) {
    if output.len() == MAX_LINKS || uri.len() > 4096 || output.iter().any(|(_, old)| old == uri) {
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

fn collect(
    items: &[markdown::Item],
    style: markdown::Style,
    depth: usize,
    output: &mut Vec<(String, String)>,
) {
    if depth == 16 {
        return;
    }
    for item in items {
        if output.len() == MAX_LINKS {
            return;
        }
        match item {
            markdown::Item::Paragraph(text) | markdown::Item::Heading(_, text) => {
                for span in text.spans(style).iter() {
                    if let Some(uri) = &span.link {
                        add(&span.text, &uri.to_string(), output);
                    }
                }
            }
            markdown::Item::Image { url, alt, .. } => {
                let label: String = alt
                    .spans(style)
                    .iter()
                    .map(|span| span.text.as_ref())
                    .collect();
                add(&label, &url.to_string(), output);
            }
            markdown::Item::Quote(items) => collect(items, style, depth + 1, output),
            markdown::Item::List { bullets, .. } => {
                for bullet in bullets {
                    let (markdown::Bullet::Point { items } | markdown::Bullet::Task { items, .. }) =
                        bullet;
                    collect(items, style, depth + 1, output);
                }
            }
            markdown::Item::Table { columns, rows } => {
                for column in columns {
                    collect(&column.header, style, depth + 1, output);
                }
                for row in rows {
                    for cell in &row.cells {
                        collect(cell, style, depth + 1, output);
                    }
                }
            }
            markdown::Item::CodeBlock { .. } | markdown::Item::Rule => {}
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
}
