//! Literal non-Markdown readers use the existing admitted preparation boundary.
use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, renderer,
    widget::{Operation, Tree, tree},
};

pub(super) struct NoticeData {
    pub(super) summary: markdown::Text,
    pub(super) body: Box<prompt::Prepared>,
}
pub(super) enum Prepared {
    Search(prose_document::Data),
    Notice(Box<NoticeData>),
    Image {
        document: prose_document::Data,
        url: Option<markdown::Uri>,
    },
}
pub(super) struct Notice(Option<Box<NoticeData>>);
impl Notice {
    pub(super) fn data(&self) -> &NoticeData {
        self.0.as_deref().expect("live literal notice")
    }
}
impl Drop for Notice {
    fn drop(&mut self) {
        if let Some(data) = self.0.take() {
            preparation::retire_literal(Prepared::Notice(data));
        }
    }
}
pub(super) enum Content {
    Search(prose_document::Document),
    Notice(Notice),
    Image {
        document: prose_document::Document,
        url: Option<markdown::Uri>,
    },
}
impl Prepared {
    pub(super) fn install(self) -> Content {
        match self {
            Self::Search(data) => Content::Search(prose_document::Document::new(data)),
            Self::Notice(data) => Content::Notice(Notice(Some(data))),
            Self::Image { document, url } => Content::Image {
                document: prose_document::Document::new(document),
                url,
            },
        }
    }
    pub(super) fn required(content: &ItemContent) -> bool {
        matches!(
            content,
            ItemContent::WebSearch { .. }
                | ItemContent::Image { .. }
                | ItemContent::Delivery {
                    kind: chat::DeliveryKind::Notice,
                    ..
                }
        )
    }
    pub(super) fn new(
        content: &ItemContent,
        cancelled: &AtomicBool,
    ) -> Result<Option<Self>, String> {
        let check = || {
            if cancelled.load(Ordering::Acquire) {
                Err("Message loading was cancelled.".to_owned())
            } else {
                Ok(())
            }
        };
        let text = |body: &str, link: Option<markdown::Uri>| -> Result<markdown::Text, String> {
            check()?;
            let text = markdown::Text::literal_linked(body.to_owned(), link);
            text.plain();
            check()?;
            text.plain_prefixes();
            check()?;
            Ok(text)
        };
        let absent = || markdown::Item::Quote(Vec::new());
        Ok(Some(match content {
            ItemContent::WebSearch { query, results } => {
                let mut items = Vec::with_capacity(results.len() + 1);
                items.push(markdown::Item::Paragraph(text(query, None)?));
                for result in results {
                    check()?;
                    let url: Option<markdown::Uri> =
                        (!result.url.is_empty()).then(|| Arc::from(result.url.as_str()));
                    let title = if result.title.is_empty() {
                        result.url.as_str()
                    } else {
                        result.title.as_str()
                    };
                    items.push(markdown::Item::Quote(vec![
                        markdown::Item::Paragraph(text(title, url)?),
                        if result.url.is_empty() {
                            absent()
                        } else {
                            markdown::Item::Paragraph(text(&result.url, None)?)
                        },
                        if result.snippet.is_empty() {
                            absent()
                        } else {
                            markdown::Item::Paragraph(text(&result.snippet, None)?)
                        },
                    ]));
                }
                Self::Search(prose_document::Data::items(items))
            }
            ItemContent::Delivery {
                kind: chat::DeliveryKind::Notice,
                summary,
                body,
                ..
            } => Self::Notice(Box::new(NoticeData {
                summary: text(summary, None)?,
                body: prompt::Prepared::with_policy(
                    body,
                    prompt::PreviewPolicy::Notice,
                    cancelled,
                )?,
            })),
            ItemContent::Image {
                path,
                description,
                reference,
            } => {
                let remote = path.starts_with("https://") || path.starts_with("http://");
                let url: Option<markdown::Uri> = remote.then(|| Arc::from(path.as_str()));
                // A managed path is a storage hint, not the reader's attachment
                // name. Keep the same stable label used by the image receipt.
                let managed_label = reference.as_ref().map(chat::ImageReference::label);
                let label = if let Some(label) = managed_label.as_deref() {
                    label
                } else if !path.starts_with("data:") {
                    path.rsplit('/')
                        .next()
                        .filter(|name| !name.is_empty())
                        .unwrap_or(if description.is_empty() {
                            "Attachment"
                        } else {
                            description
                        })
                } else if description.is_empty() {
                    "Attachment"
                } else {
                    description
                };
                let items = vec![
                    if description.is_empty() {
                        absent()
                    } else {
                        markdown::Item::Paragraph(text(description, None)?)
                    },
                    if path.is_empty() || path.starts_with("data:") || remote {
                        absent()
                    } else {
                        markdown::Item::Paragraph(text(path, None)?)
                    },
                    markdown::Item::Paragraph(text(label, None)?),
                    // Retain the URI in the same immutable document backing,
                    // even when only the shared Open image action displays it.
                    markdown::Item::Paragraph(text("Open image", url.clone())?),
                ];
                Self::Image {
                    document: prose_document::Data::items(items),
                    url,
                }
            }
            _ => return Ok(None),
        }))
    }
}
fn paragraph_at<'a>(
    document: &'a prose_document::Document,
    path: &[u32],
) -> Option<&'a markdown::Text> {
    match document.node(path)? {
        prose_document::Node::Item(markdown::Item::Paragraph(text)) => Some(text),
        _ => None,
    }
}
impl Content {
    pub(super) fn image_prompt(&self) -> Option<&markdown::Text> {
        match self {
            Self::Image { document, .. } => paragraph_at(document, &[2]),
            _ => None,
        }
    }
    pub(super) fn notice(&self) -> Option<&NoticeData> {
        match self {
            Self::Notice(notice) => Some(notice.data()),
            _ => None,
        }
    }
    pub(super) fn retained_bytes(&self) -> usize {
        match self {
            Self::Search(document) | Self::Image { document, .. } => {
                document.stats(&[]).bytes.saturating_mul(2)
            }
            Self::Notice(notice) => {
                notice.data().body.retained_bytes() + notice.data().summary.len().saturating_mul(2)
            }
        }
    }
    pub(super) fn catalog(
        &self,
        builder: &mut selection::DocumentBuilder,
        item: &chat::Item,
        expanded: bool,
    ) {
        match self {
            Self::Search(document) if expanded => {
                builder.markdown(&item.id, &[7], document.items())
            }
            Self::Notice(notice) => {
                let text = notice.data().body.shown(expanded);
                if text.len() != 0 {
                    builder.indexed(&item.id, &[2], text.plain(), text.plain_prefixes());
                }
            }
            Self::Image { document, .. } if expanded => {
                for index in [0, 1] {
                    if let Some(text) = paragraph_at(document, &[index]) {
                        builder.indexed(&item.id, &[8, index], text.plain(), text.plain_prefixes());
                    }
                }
            }
            _ => {}
        }
    }
}
impl Runtime {
    pub(super) fn prepared_literal(&self, item: &chat::Item) -> Option<&Content> {
        let cached = self.presentation_cache().get(&item.id)?;
        (!cached.placeholder
            && cached
                .item
                .as_deref()
                .is_some_and(|shown| std::ptr::eq(shown, item)))
        .then_some(cached.literal.as_ref())
        .flatten()
    }
}
fn field(
    cached: &CachedItem,
    item: &chat::Item,
    target: Target,
    text: &markdown::Text,
    path: &[u32],
    mut settings: markdown::Settings,
    find: Option<&str>,
) -> Element<'static, Message> {
    settings.style.font = iced::Font::default();
    prose::paragraph(
        settings,
        text,
        target,
        find,
        Some(selectable_prose::Logical {
            identity: selection::logical_identity(&item.id, path),
            start: 0,
        }),
        Some(&cached.flow_cache),
    )
}
pub(super) fn search_view(
    runtime: &Runtime,
    item: &chat::Item,
    cached: &CachedItem,
    mut settings: markdown::Settings,
) -> Element<'static, Message> {
    settings.text_size = design::TEXT_BODY.into();
    let Some(Content::Search(document)) = runtime.prepared_literal(item) else {
        return design::caption("Loading search results…").into();
    };
    let target = runtime.target;
    let find = runtime.find.as_deref();
    let query = field(
        cached,
        item,
        target,
        paragraph_at(document, &[0]).unwrap(),
        &[7, 0],
        settings,
        find,
    );
    let mut rows = Vec::with_capacity(document.items().len().saturating_sub(1));
    let item_id: Arc<str> = item.id.as_str().into();
    let revision = {
        use std::hash::{Hash, Hasher};
        let mut hash = std::hash::DefaultHasher::new();
        (
            cached.revision,
            find.map(|_| find_navigation::render_revision(target)),
            settings.text_size.0.to_bits(),
            settings.style.font,
        )
            .hash(&mut hash);
        hash.finish()
    };
    for index in 1..document.items().len() {
        let document = document.clone();
        let cache = cached.flow_cache.clone();
        let id = item_id.clone();
        let descendants = document.logical_ranges(&id, &[7], &[index as u32], false);
        let estimate = document.stats(&[index as u32]).estimate(settings) + step::GAP_S;
        let key = selection::logical_identity(&id, &[7, index as u32]);
        let enabled = find.is_some();
        let mut row = virtualized::Row::new(key, revision, estimate, None, move || {
            let mut fields = Vec::new();
            for part in 0..3 {
                let Some(text) = paragraph_at(&document, &[index as u32, part]) else {
                    continue;
                };
                let mut settings = settings;
                settings.style.font = iced::Font::default();
                if part == 1 {
                    settings.text_size = design::TEXT_CAPTION.into();
                }
                settings.style.link_color = design::accent_color(&cosmic::theme::active());
                fields.push(prose::paragraph(
                    settings,
                    text,
                    target,
                    enabled.then_some(""),
                    Some(selectable_prose::Logical {
                        identity: selection::logical_identity(&id, &[7, index as u32, part]),
                        start: 0,
                    }),
                    Some(&cache),
                ));
            }
            widget::column::with_children(fields)
                .spacing(step::GAP_XS)
                .into()
        });
        row.descendants = descendants;
        row.anchor = true;
        rows.push(row);
    }
    let results = if rows.is_empty() {
        design::caption("No results were reported.").into()
    } else {
        virtualized::column(rows, Default::default())
    };
    widget::column::with_children(vec![query, results])
        .spacing(step::GAP_S)
        .into()
}
pub(super) fn image_field(
    runtime: &Runtime,
    item: &chat::Item,
    cached: &CachedItem,
    index: u32,
    mut settings: markdown::Settings,
) -> Option<Element<'static, Message>> {
    if index == 0 {
        settings.text_size = design::TEXT_BODY.into();
    }
    let Content::Image { document, .. } = runtime.prepared_literal(item)? else {
        return None;
    };
    Some(field(
        cached,
        item,
        runtime.target,
        paragraph_at(document, &[index])?,
        &[8, index],
        settings,
        runtime.find.as_deref(),
    ))
}
pub(super) fn notice_body(
    runtime: &Runtime,
    item: &chat::Item,
    cached: &CachedItem,
    mut settings: markdown::Settings,
    limit: Option<f32>,
) -> Element<'static, Message> {
    let Some(notice) = runtime.prepared_literal(item).and_then(Content::notice) else {
        return design::caption("Loading notice…").into();
    };
    let text = notice.body.shown(runtime.item_expanded(&item.id));
    settings.text_size = design::TEXT_CAPTION.into();
    settings.style.font = iced::Font::MONOSPACE;
    let identity = selection::logical_identity(&item.id, &[2]);
    let body = prose::paragraph(
        settings,
        text,
        runtime.target,
        runtime.find.as_deref(),
        Some(selectable_prose::Logical { identity, start: 0 }),
        Some(&cached.flow_cache),
    );
    let uncapped = runtime.find.is_some()
        && find_navigation::tool_uncapped(runtime.target, identity, &notice.body.full.plain());
    if let Some(height) = limit.filter(|_| !uncapped) {
        widget::container(scroll::viewport(
            widget::scrollable(body).height(Length::Shrink),
        ))
        .max_height(height)
        .into()
    } else {
        body
    }
}
pub(super) fn notice_summary(
    runtime: &Runtime,
    item: &chat::Item,
    cached: &CachedItem,
    mut settings: markdown::Settings,
) -> Element<'static, Message> {
    let Some(notice) = runtime.prepared_literal(item).and_then(Content::notice) else {
        return design::caption("Loading notice…").into();
    };
    settings.text_size = design::TEXT_CAPTION.into();
    settings.style.font = iced::Font::default();
    let body = prose::paragraph(
        settings,
        &notice.summary,
        runtime.target,
        None,
        Some(selectable_prose::Logical {
            identity: selection::logical_identity(&item.id, &[9]),
            start: 0,
        }),
        Some(&cached.flow_cache),
    );
    Element::new(DisplayOnly(body))
}
/// Notice headings retain their previous nonselectable caption semantics.
/// Redraw still reaches Flow admission; no anonymous selection endpoint leaks
/// into the body-only transcript Catalog.
struct DisplayOnly(Element<'static, Message>);
impl Widget<Message, cosmic::Theme, cosmic::Renderer> for DisplayOnly {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Self>()
    }
    fn state(&self) -> tree::State {
        tree::State::None
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.0)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.children[0].diff(&mut self.0);
    }
    fn size(&self) -> iced::Size<Length> {
        self.0.as_widget().size()
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.0
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        defaults: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &iced::Rectangle,
    ) {
        self.0.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            defaults,
            layout,
            cursor,
            viewport,
        );
    }
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.0
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }
    fn operate(
        &mut self,
        _: &mut Tree,
        _: Layout<'_>,
        _: &cosmic::Renderer,
        _: &mut dyn Operation,
    ) {
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &iced::Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &iced::Rectangle,
    ) {
        if matches!(
            event,
            iced::Event::Window(iced::window::Event::RedrawRequested(_))
        ) {
            self.0.as_widget_mut().update(
                &mut tree.children[0],
                event,
                layout,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
    }
}
