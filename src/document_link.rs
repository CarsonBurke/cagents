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

pub fn resolve(source: &Path, target: &str) -> Result<DocumentLink, String> {
    let base = Url::from_file_path(source).map_err(|()| "Document path must be absolute")?;
    let url = base.join(target).map_err(|error| error.to_string())?;
    match url.scheme() {
        "http" | "https" | "mailto" => Ok(DocumentLink::External(url)),
        "file" => {
            let path = url
                .to_file_path()
                .map_err(|()| "The link is not a local file path")?;
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
    let style = markdown::Settings::from(iced::Theme::Dark).style;
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
            let mut suffix = 0;
            while !used.insert(unique.clone()) {
                suffix += 1;
                unique = format!("{slug}-{suffix}");
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
    struct Find {
        scroll: Id,
        target: Id,
        top: Option<f32>,
        offset: Option<f32>,
    }
    impl<T: 'static> Operation<T> for Find {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<T>)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            id: Option<&Id>,
            _: Rectangle,
            content: Rectangle,
            _: Vector,
            _: &mut dyn operation::Scrollable,
        ) {
            if id == Some(&self.scroll) {
                self.top = Some(content.y);
            }
        }
        fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
            if id == Some(&self.target) {
                self.offset = self.top.map(|top| (bounds.y - top).max(0.0));
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
    })
}

pub(crate) struct PreviewViewer;
impl<'a> markdown::Viewer<'a, markdown::Uri, cosmic::Theme, cosmic::Renderer> for PreviewViewer {
    fn on_link_click(uri: markdown::Uri) -> markdown::Uri {
        uri
    }
}
