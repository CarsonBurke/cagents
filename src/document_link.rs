//! Resolve document links without treating local paths as external commands.
use std::path::{Path, PathBuf};
use url::Url;

#[derive(Debug, PartialEq)]
pub enum DocumentLink {
    External(Url),
    Local {
        path: PathBuf,
        fragment: Option<String>,
    },
}

/// Parse coding-agent coordinates on bare paths, preserving URI query and fragment data.
pub fn file_position(target: &str) -> (&str, Option<String>) {
    if target.contains("://")
        || target.split_once(':').is_some_and(|(scheme, _)| {
            ["http", "https", "file", "mailto"]
                .iter()
                .any(|known| scheme.eq_ignore_ascii_case(known))
        })
        || target.contains('#')
        || target.contains('?')
    {
        return (target, None);
    }
    let Some((prefix, last)) = target.rsplit_once(':') else {
        return (target, None);
    };
    let Ok(last) = last.parse::<u32>() else {
        return (target, None);
    };
    if last == 0 {
        return (target, None);
    }
    let path = prefix
        .rsplit_once(':')
        .filter(|(_, line)| line.parse::<u32>().is_ok())
        .map_or(prefix, |(path, _)| path);
    if !path.contains('/') && !path.contains('.') {
        return (target, None);
    }
    match prefix.rsplit_once(':') {
        Some((path, line)) if line.parse::<u32>().is_ok_and(|line| line > 0) => {
            (path, Some(format!("L{line}:{last}")))
        }
        _ => (prefix, Some(format!("L{last}"))),
    }
}

pub fn resolve(source: &Path, target: &str) -> Result<DocumentLink, String> {
    let base = Url::from_file_path(source).map_err(|()| "Document path must be absolute")?;
    let url = base.join(target).map_err(|error| error.to_string())?;
    match url.scheme() {
        "http" | "https" | "mailto" => Ok(DocumentLink::External(url)),
        "file" => {
            let path = url
                .to_file_path()
                .map_err(|()| "The link is not a local file path")?;
            if path.as_os_str().as_encoded_bytes().contains(&0) {
                return Err("The link contains an invalid file path".into());
            }
            // Decode fragments using the URL path decoder, without giving '#' or
            // '?' within the heading a second interpretation as URL syntax.
            let fragment = url.fragment().map(|fragment| {
                let mut decoder = Url::parse("file:///").unwrap();
                decoder.set_path(&format!("/{fragment}"));
                decoder.to_file_path().unwrap().to_string_lossy()[1..].to_owned()
            });
            Ok(DocumentLink::Local { path, fragment })
        }
        scheme => Err(format!(
            "Document links cannot open the '{scheme}' URL scheme"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coding_agent_coordinates_preserve_url_ports_and_unsupported_schemes() {
        assert_eq!(
            file_position("src/app.rs:42:7"),
            ("src/app.rs", Some("L42:7".into()))
        );
        assert_eq!(
            file_position("/repo/Cargo.toml:3"),
            ("/repo/Cargo.toml", Some("L3".into()))
        );
        for target in [
            "https://localhost:3000",
            "javascript:42",
            "src/app.rs:zero",
            "src/app.rs:0",
        ] {
            assert_eq!(file_position(target), (target, None));
        }
    }
    #[test]
    fn coding_agent_coordinates_preserve_explicit_schemes_without_authority() {
        // These are URI schemes even without `://`. Numeric tail bytes belong
        // to the URI, including a literal colon in an absolute file path.
        for target in [
            "http:example.test:8443",
            "HTTPS:example.test:8443",
            "file:/repo/report.txt:12:3",
            "FILE:/repo/report.txt:12:3",
            "mailto:person@example.test:42",
            "MAILTO:person@example.test:42",
        ] {
            assert_eq!(file_position(target), (target, None), "{target}");
        }
        for target in ["file:/repo/report.txt:12:3", "FILE:/repo/report.txt:12:3"] {
            assert_eq!(
                resolve(Path::new("/repo/source.md"), target).unwrap(),
                DocumentLink::Local {
                    path: "/repo/report.txt:12:3".into(),
                    fragment: None,
                },
                "{target}"
            );
        }
        // Scheme-looking basename coordinates remain the supported bare-path
        // grammar; recognizing every URL-parsable prefix would swallow these.
        assert_eq!(
            file_position("main.rs:12:3"),
            ("main.rs", Some("L12:3".into()))
        );
    }

    #[test]
    fn coding_agent_coordinates_preserve_uri_query_and_fragment_boundaries() {
        for target in [
            "src/main.rs#L12:3",
            "/repo/src/main.rs#L12:3",
            "#L12:3",
            "src/main.rs?revision=7:3",
            "src/main.rs?revision=7:3#L12:3",
            "file:///repo/src/main.rs#L12:3",
            "https://localhost:3000/src/main.rs?revision=7:3#L12:3",
        ] {
            assert_eq!(file_position(target), (target, None), "{target}");
        }
        // Encoded delimiter bytes belong to the filename, so they do not
        // prevent a bare-path suffix from supplying coordinates.
        for (target, path) in [
            ("src/a%23b.rs:12:3", "src/a%23b.rs"),
            ("src/a%3Fb.rs:12:3", "src/a%3Fb.rs"),
        ] {
            assert_eq!(file_position(target), (path, Some("L12:3".into())));
        }
    }

    #[test]
    fn heading_fragments_preserve_duplicate_and_explicit_suffix_collisions() {
        let items: Vec<_> =
            markdown::parse("# Repeated\n# Repeated-1\n# Repeated\n# Repeated-1\n# Repeated\n")
                .collect();
        for (fragment, index) in [
            ("repeated", 0),
            ("repeated-1", 1),
            ("repeated-2", 2),
            ("repeated-1-1", 3),
            ("repeated-3", 4),
        ] {
            assert_eq!(heading_index(&items, fragment), Some(index));
        }
        assert_eq!(heading_index(&items, "missing"), None);
    }

    #[test]
    fn heading_fragments_reach_late_duplicates_in_generated_documents() {
        let items: Vec<_> = markdown::parse(&"# Result\n".repeat(4096)).collect();
        assert_eq!(heading_index(&items, "result-4095"), Some(4095));
    }

    #[test]
    fn local_links_use_the_document_directory_and_decode_url_paths() {
        let source = Path::new("/repo/docs/top_runs.md");
        for (link, expected, fragment) in [
            (
                "../benchmark_results/run/",
                "/repo/benchmark_results/run/",
                None,
            ),
            (
                "temporal_sigreg.md#completed-causal-reader-comparison",
                "/repo/docs/temporal_sigreg.md",
                Some("completed-causal-reader-comparison"),
            ),
            ("../evidence%20one.json", "/repo/evidence one.json", None),
            ("#A%20heading", "/repo/docs/top_runs.md", Some("A heading")),
            ("file:///other/a%23b.md", "/other/a#b.md", None),
        ] {
            assert_eq!(
                resolve(source, link).unwrap(),
                DocumentLink::Local {
                    path: PathBuf::from(expected),
                    fragment: fragment.map(str::to_owned),
                }
            );
        }
        assert!(matches!(
            resolve(source, "https://example.com"),
            Ok(DocumentLink::External(_))
        ));
        assert!(resolve(source, "javascript:alert(1)").is_err());
        assert!(resolve(source, "file://remote/path").is_err());
    }
}

use cosmic::iced::{
    self, Rectangle, Vector,
    advanced::widget::{Id, Operation, operation},
};
use cosmic::widget::markdown;

pub(crate) fn heading_index(items: &[markdown::Item], fragment: &str) -> Option<usize> {
    let mut used = std::collections::HashSet::new();
    // Each base slug resumes after its last suffix, instead of retrying every
    // preceding duplicate when a generated document repeats a heading.
    let mut suffixes = std::collections::HashMap::<String, usize>::new();
    let style =
        crate::design::markdown_settings(&cosmic::theme::active(), crate::design::TEXT_BODY).style;
    for (index, item) in items.iter().enumerate() {
        if let markdown::Item::Heading(_, text) = item {
            let text: String = text
                .spans(style)
                .iter()
                .map(|span| span.text.as_ref())
                .collect();
            let slug: String = text
                .to_lowercase()
                .chars()
                .filter_map(|c| {
                    if c.is_whitespace() {
                        Some('-')
                    } else if c.is_alphanumeric() || c == '-' || c == '_' {
                        Some(c)
                    } else {
                        None
                    }
                })
                .collect();
            let mut unique = slug.clone();
            if !used.insert(unique.clone()) {
                let suffix = suffixes.entry(slug.clone()).or_default();
                loop {
                    *suffix += 1;
                    unique = format!("{slug}-{suffix}");
                    if used.insert(unique.clone()) {
                        break;
                    }
                }
            }
            if unique == fragment {
                return Some(index);
            }
        }
    }
    None
}

pub(crate) fn heading_id(card: crate::model::CardId, index: usize) -> Id {
    Id::new(format!("document-heading-{card}-{index}"))
}

pub(crate) fn scroll_to_heading<T: Send + 'static>(scroll: Id, target: Id) -> iced::Task<T> {
    scroll_to_heading_anchored(scroll, target, false)
}

pub(crate) fn scroll_to_heading_anchored<T: Send + 'static>(
    scroll: Id,
    target: Id,
    bottom: bool,
) -> iced::Task<T> {
    struct Find {
        scroll: Id,
        target: Id,
        top: Option<f32>,
        offset: Option<f32>,
        bottom: bool,
        travel: f32,
    }
    impl<T: 'static> Operation<T> for Find {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<T>)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            id: Option<&Id>,
            viewport: Rectangle,
            content: Rectangle,
            _: Vector,
            _: &mut dyn operation::Scrollable,
        ) {
            if id == Some(&self.scroll) {
                self.top = Some(content.y);
                self.travel = (content.height - viewport.height).max(0.0);
            }
        }
        fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
            if id == Some(&self.target) {
                self.offset = self.top.map(|top| {
                    let from_top = (bounds.y - top).clamp(0.0, self.travel);
                    if self.bottom {
                        self.travel - from_top
                    } else {
                        from_top
                    }
                });
            }
        }
        fn finish(&self) -> operation::Outcome<T> {
            self.offset.map_or(operation::Outcome::None, |y| {
                operation::Outcome::Chain(Box::new(operation::scrollable::scroll_to(
                    self.scroll.clone(),
                    operation::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(y),
                    },
                )))
            })
        }
    }
    iced::advanced::widget::operate(Find {
        scroll,
        target,
        top: None,
        offset: None,
        bottom,
        travel: 0.0,
    })
}

pub(crate) struct PreviewViewer;
impl<'a> markdown::Viewer<'a, markdown::Uri, cosmic::Theme, cosmic::Renderer> for PreviewViewer {
    fn on_link_click(uri: markdown::Uri) -> markdown::Uri {
        uri
    }
}
