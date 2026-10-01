//! Native document diagrams. Rendering never runs in the view or typing path.

use cosmic::{
    Element, iced,
    widget::{self, markdown},
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, LazyLock},
};

const MAX_DIAGRAMS: usize = 24;
const MAX_SOURCE_BYTES: usize = 32 * 1024;
const MAX_ELEMENTS: usize = 256;
const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;
static WORKERS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(2)));

pub struct CancellationGuard(pub tokio_util::sync::CancellationToken);
impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Debug, Clone)]
pub enum Diagram {
    Ready {
        handle: iced::widget::image::Handle,
        bytes: usize,
    },
    Error(String),
}

pub fn standalone(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("mmd") || s.eq_ignore_ascii_case("mermaid"))
}

pub fn sources(path: &Path, text: &str, items: &[markdown::Item]) -> Vec<String> {
    fn collect(items: &[markdown::Item], out: &mut Vec<String>, seen: &mut HashSet<blake3::Hash>) {
        for item in items {
            match item {
                markdown::Item::CodeBlock {
                    language: Some(language),
                    code,
                    ..
                } if language.eq_ignore_ascii_case("mermaid") => {
                    if out.len() < MAX_DIAGRAMS && seen.insert(blake3::hash(code.as_bytes())) {
                        out.push(code.clone());
                    }
                }
                markdown::Item::List { bullets, .. } => {
                    for bullet in bullets {
                        let (markdown::Bullet::Point { items }
                        | markdown::Bullet::Task { items, .. }) = bullet;
                        collect(items, out, seen);
                    }
                }
                markdown::Item::Quote(items) => collect(items, out, seen),
                markdown::Item::Table { columns, rows } => {
                    for column in columns {
                        collect(&column.header, out, seen);
                    }
                    for row in rows {
                        for cell in &row.cells {
                            collect(cell, out, seen);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if standalone(path) {
        return vec![text.to_owned()];
    }
    let mut result = Vec::new();
    collect(items, &mut result, &mut HashSet::new());
    result
}

pub fn available_budget(diagrams: &HashMap<blake3::Hash, Diagram>) -> usize {
    MAX_CACHE_BYTES.saturating_sub(
        diagrams
            .values()
            .map(|d| match d {
                Diagram::Ready { bytes, .. } => *bytes,
                _ => 0,
            })
            .sum::<usize>(),
    )
}

pub async fn render(
    sources: Vec<String>,
    dark: bool,
    available: usize,
    cancel: tokio_util::sync::CancellationToken,
) -> Vec<(blake3::Hash, Diagram)> {
    let permit = tokio::select! {
        _ = cancel.cancelled() => return Vec::new(),
        permit = WORKERS.clone().acquire_owned() => permit.expect("diagram semaphore remains open"),
    };
    let keys: Vec<_> = sources
        .iter()
        .map(|source| blake3::hash(source.as_bytes()))
        .collect();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut bytes = 0;
        sources
            .into_iter()
            .take_while(|_| !cancel.is_cancelled())
            .map(|source| {
                let key = blake3::hash(source.as_bytes());
                let result =
                    render_one_cancellable(&source, dark, available - bytes, Some(&cancel));
                if let Diagram::Ready { bytes: size, .. } = &result {
                    bytes += size;
                }
                (key, result)
            })
            .collect()
    })
    .await
    .unwrap_or_else(|error| {
        keys.into_iter()
            .map(|key| {
                (
                    key,
                    Diagram::Error(format!("Diagram worker failed: {error}")),
                )
            })
            .collect()
    })
}

#[cfg(test)]
fn render_one(source: &str, dark: bool, available: usize) -> Diagram {
    render_one_cancellable(source, dark, available, None)
}

fn render_one_cancellable(
    source: &str,
    dark: bool,
    available: usize,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Diagram {
    let render = || -> Result<Diagram, String> {
        if source.len() > MAX_SOURCE_BYTES || source.lines().count() > 512 {
            return Err("Diagram exceeds the native preview limit (32 KiB / 512 lines).".into());
        }
        let parsed =
            mermaid_rs_renderer::parse_mermaid_strict(source).map_err(|error| error.to_string())?;
        let g = &parsed.graph;
        let elements = g.nodes.len()
            + g.edges.len()
            + g.subgraphs.len()
            + g.sequence_frames.len()
            + g.sequence_notes.len()
            + g.sequence_activations.len()
            + g.pie_slices.len()
            + g.quadrant.points.len()
            + g.gantt_tasks.len()
            + g.gitgraph.commits.len()
            + g.c4.shapes.len()
            + g.c4.rels.len()
            + g.mindmap.nodes.len()
            + g.xychart
                .series
                .iter()
                .map(|s| s.values.len())
                .sum::<usize>()
            + g.timeline.events.len()
            + g.block.as_ref().map_or(0, |b| b.nodes.len());
        if elements > MAX_ELEMENTS {
            return Err("Diagram exceeds the native preview limit of 256 elements.".into());
        }
        let theme = if dark {
            mermaid_rs_renderer::Theme::dark()
        } else {
            mermaid_rs_renderer::Theme::modern()
        };
        let config = mermaid_rs_renderer::LayoutConfig::default();
        let layout = mermaid_rs_renderer::compute_layout(g, &theme, &config);
        if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
            return Err("Rendering cancelled.".into());
        }
        let svg = mermaid_rs_renderer::render_svg(&layout, &theme, &config);
        // Bound the surface before allocating a raster. The existing decoder
        // also refuses external resources, effects and recursive references.
        let dimensions = mermaid_rs_renderer::measure_svg_dimensions(&layout, &config, None);
        let required = (dimensions.width as f64 * dimensions.height as f64 * 4.0).ceil();
        if !required.is_finite() || required > available as f64 {
            return Err("Diagrams exceed this document's 32 MiB preview budget.".into());
        }
        let crate::editor::FileContent::Image {
            width,
            height,
            rgba,
        } = crate::editor::decode_svg(svg.as_bytes())?
        else {
            unreachable!()
        };
        let bytes = rgba.len();
        if bytes > available {
            return Err("Diagrams exceed this document's preview budget.".into());
        }
        Ok(Diagram::Ready {
            handle: iced::widget::image::Handle::from_rgba(
                width,
                height,
                iced::advanced::graphics::core::Bytes::from_owner(rgba),
            ),
            bytes,
        })
    };
    render().unwrap_or_else(Diagram::Error)
}

pub struct Viewer<'a> {
    pub diagrams: &'a HashMap<blake3::Hash, Diagram>,
    pub loading: bool,
}
impl<'a> markdown::Viewer<'a, markdown::Uri, cosmic::Theme, cosmic::Renderer> for Viewer<'a> {
    fn on_link_click(uri: markdown::Uri) -> markdown::Uri {
        uri
    }
    fn code_block(
        &self,
        settings: markdown::Settings,
        language: Option<&'a str>,
        code: &'a str,
        lines: &'a [markdown::Text],
    ) -> Element<'a, markdown::Uri> {
        if language.is_some_and(|s| s.eq_ignore_ascii_case("mermaid")) {
            return diagram_view(
                self.diagrams.get(&blake3::hash(code.as_bytes())),
                code,
                self.loading,
            );
        }
        markdown::code_block(settings, lines, Self::on_link_click)
    }
}

pub fn diagram_view<'a, Message: Clone + 'static>(
    diagram: Option<&'a Diagram>,
    source: &'a str,
    loading: bool,
) -> Element<'a, Message> {
    match diagram {
        Some(Diagram::Ready { handle, .. }) => iced::widget::image(handle.clone())
            .width(iced::Length::Fill)
            .content_fit(iced::ContentFit::Contain)
            .into(),
        _ => widget::column::with_children(vec![
            widget::text(match diagram {
                Some(Diagram::Error(error)) => error.as_str(),
                _ if loading => "Rendering diagram…",
                _ => "Diagram preview unavailable: at most 24 unique diagrams per document.",
            })
            .into(),
            widget::selectable_text(source)
                .font(iced::Font::MONOSPACE)
                .into(),
        ])
        .spacing(8)
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_diagram_job_never_parses_or_renders() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let guard = CancellationGuard(cancel.clone());
        drop(guard);
        assert!(
            render(vec!["not a diagram".into()], false, MAX_CACHE_BYTES, cancel)
                .await
                .is_empty()
        );
    }

    #[test]
    fn preview_sources_use_markdown_structure_and_deduplicate_nested_fences() {
        let text = "```mermaid\nflowchart LR; A-->B\n```\n\n> ```mermaid\n> flowchart LR; A-->B\n> ```\n\n```rust\nfn main() {}\n```\n\n- Nested:\n\n  ```mermaid\n  sequenceDiagram\n  A->>B: Hello\n  ```\n";
        let items: Vec<_> = markdown::parse(text).collect();
        let sources = sources(Path::new("README.md"), text, &items);
        assert_eq!(sources.len(), 2);
        assert!(sources[0].contains("flowchart"));
        assert!(sources[1].contains("sequenceDiagram"));
        assert_eq!(
            super::sources(Path::new("workflow.MMD"), "flowchart LR; A-->B", &[]),
            vec!["flowchart LR; A-->B"]
        );
    }

    #[test]
    fn native_diagrams_render_multiple_types_and_refuse_invalid_or_expensive_input() {
        for source in [
            "flowchart LR\nA[Start] --> B{Ready?}\nB -->|Yes| C[Done]",
            "sequenceDiagram\nparticipant A as Alice\nparticipant B as Bob\nA->>B: Hello",
            "classDiagram\nAnimal <|-- Duck\nAnimal : +String name",
            "stateDiagram-v2\n[*] --> Ready\nReady --> Done\nDone --> [*]",
            "pie title Usage\n\"Remaining\" : 64\n\"Used\" : 36",
        ] {
            let diagram = render_one(source, true, MAX_CACHE_BYTES);
            assert!(
                matches!(diagram, Diagram::Ready { bytes, .. } if bytes > 0),
                "{source}: {diagram:?}"
            );
        }
        for source in [
            "this is not a diagram".to_owned(),
            "x".repeat(MAX_SOURCE_BYTES + 1),
            format!(
                "flowchart LR\n{}",
                (0..300)
                    .map(|n| format!("N{n} --> N{}\n", n + 1))
                    .collect::<String>()
            ),
        ] {
            assert!(matches!(
                render_one(&source, false, MAX_CACHE_BYTES),
                Diagram::Error(_)
            ));
        }
        assert!(matches!(
            render_one("flowchart LR; A-->B", false, 0),
            Diagram::Error(_)
        ));
    }

    #[test]
    fn excessive_diagrams_are_not_submitted_and_cache_budget_includes_retained_images() {
        let text = (0..40)
            .map(|n| format!("```mermaid\nflowchart LR; A{n}-->B{n}\n```\n\n"))
            .collect::<String>();
        let items: Vec<_> = markdown::parse(&text).collect();
        assert_eq!(
            sources(Path::new("many.md"), &text, &items).len(),
            MAX_DIAGRAMS
        );
        let diagrams = HashMap::from([(
            blake3::hash(b"fixture"),
            Diagram::Ready {
                handle: iced::widget::image::Handle::from_rgba(1, 1, vec![0; 4]),
                bytes: MAX_CACHE_BYTES - 4,
            },
        )]);
        assert_eq!(available_budget(&diagrams), 4);
    }
}
