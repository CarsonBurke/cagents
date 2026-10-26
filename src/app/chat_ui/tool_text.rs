//! Retained, selectable read-only tool content. Copy always uses the snapshot.
use super::*;
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Language {
    Plain,
    Shell,
    Json,
    Diff,
}

impl Language {
    pub(super) fn structured(text: &str) -> Self {
        if text.len() <= CachedItem::HIGHLIGHT_LIMIT
            && matches!(text.trim_start().as_bytes().first(), Some(b'{' | b'['))
            && serde_json::from_str::<serde_json::Value>(text).is_ok()
        {
            Self::Json
        } else {
            Self::Plain
        }
    }

    /// The highlighter's name for the language, where it is highlighted.
    fn token(self) -> Option<&'static str> {
        match self {
            Self::Plain => None,
            Self::Shell => Some("sh"),
            Self::Json => Some("json"),
            Self::Diff => Some("diff"),
        }
    }
}

/// Immutable reader allocations can be built by a worker and shared through
/// disclosure changes. Native interaction state remains on the event thread.
#[derive(Clone)]
pub(super) struct PreparedText {
    key: u64,
    language: Language,
    pub(super) text: Arc<str>,
    source: reader_source::Source,
}
impl PreparedText {
    fn new(text: &str, language: Language) -> Self {
        let language = if text.len() > CachedItem::HIGHLIGHT_LIMIT {
            Language::Plain
        } else {
            language
        };
        Self {
            key: tail_digest(language as u64, text),
            language,
            text: Arc::from(text),
            source: reader_source::Source::new(text),
        }
    }
}
#[derive(Clone, Default)]
pub(super) struct PreparedSources {
    pub(super) input: Option<PreparedText>,
    pub(super) output: Option<PreparedText>,
    pub(super) patches: Vec<PreparedText>,
}
impl PreparedSources {
    pub(super) fn new(content: &ItemContent) -> Self {
        match content {
            ItemContent::Command {
                command, output, ..
            } => Self {
                input: Some(PreparedText::new(command, Language::Shell)),
                output: Some(PreparedText::new(output, Language::structured(output))),
                ..Self::default()
            },
            ItemContent::Tool { input, output, .. } => Self {
                input: Some(PreparedText::new(input, Language::structured(input))),
                output: Some(PreparedText::new(output, Language::structured(output))),
                ..Self::default()
            },
            ItemContent::Changes { changes, .. } => Self {
                patches: changes
                    .iter()
                    .map(|change| PreparedText::new(&change.diff, Language::Diff))
                    .collect(),
                ..Self::default()
            },
            _ => Self::default(),
        }
    }
    pub(super) fn retained(cached: &CachedItem) -> Self {
        Self {
            input: cached.tool_input.as_ref().map(ToolText::prepared),
            output: cached.tool_output.as_ref().map(ToolText::prepared),
            patches: cached.patches.iter().map(ToolText::prepared).collect(),
        }
    }
}

pub(super) struct ToolText {
    key: u64,
    // Reader identity survives body appends; source provenance decides when
    // a rewrite must reset the native tree and its selection.
    reader_identity: u64,
    pub(super) reader_source: reader_source::Source,
    pub(super) flow_cache: flow_layout::Cache,
    language: Language,
    pub(super) text: Arc<str>,
    syntax: std::cell::RefCell<Option<(crate::settings::EditorSyntaxTheme, prose::CodeBlock)>>,
}

/// What a closed step's row says beyond its verb.
///
/// `text` is what the step is: the command that ran, the target it was applied
/// to, a tool's input. `tail` is the last line of what it has printed, which
/// is how a reader follows a build without opening it. The two are kept
/// apart because the output used to take the command's place on the row, and
/// a transcript of `Compiling…` lines no longer said what had been run.
pub(super) struct Preview {
    pub(super) text: String,
    pub(super) tail: String,
    /// Exact bounded skill target, prepared with the input off the view path.
    pub(super) skill: Option<String>,
    language: Language,
    /// The text highlighted in both palettes, drawn from on every frame.
    block: prose::CodeBlock,
    theme: crate::settings::EditorSyntaxTheme,
}

/// A tool's short target when the input supplies one; otherwise describe
/// the disclosure without promoting arbitrary payload text into the transcript.
fn tool_preview(input: &str, name: &str) -> (String, Option<String>) {
    // Bound all inspection, including trimming a whitespace-only JSON prefix
    // that Claude can publish before the rest of its streamed input arrives.
    if input.len() > CachedItem::HIGHLIGHT_LIMIT {
        return ("Input available".into(), None);
    }
    if input.trim().is_empty() {
        return (String::new(), None);
    }
    let object = serde_json::from_str::<serde_json::Value>(input).ok();
    let Some(object) = object.as_ref().and_then(serde_json::Value::as_object) else {
        return ("Input available".into(), None);
    };
    let skill = name
        .eq_ignore_ascii_case("skill")
        .then(|| {
            object
                .get("skill")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| {
                    !value.is_empty()
                        && value.chars().count() <= 120
                        && !value.contains(['\n', '\r'])
                })
                .map(str::to_owned)
        })
        .flatten();
    if let Some(skill) = skill {
        return (skill.clone(), Some(skill));
    }
    for key in [
        "file_path",
        "path",
        "url",
        "query",
        "pattern",
        "skill",
        "subject",
    ] {
        if let Some(value) = object.get(key).and_then(serde_json::Value::as_str) {
            let value = value.trim();
            // Long prose and multiline documents are detail, even when a
            // provider uses a familiar target field for them.
            if !value.is_empty() && value.chars().count() <= 120 && !value.contains(['\n', '\r']) {
                return (value.to_owned(), None);
            }
        }
    }
    (
        match object.len() {
            0 => String::new(),
            1 => "1 input field".into(),
            count => format!("{count} input fields"),
        },
        None,
    )
}

impl Preview {
    pub(super) fn empty(theme: crate::settings::EditorSyntaxTheme) -> Self {
        Self {
            text: String::new(),
            tail: String::new(),
            skill: None,
            language: Language::Plain,
            block: prose::CodeBlock::new(None, "", theme),
            theme,
        }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.text.capacity()
            + self.tail.capacity()
            + self.skill.as_ref().map_or(0, String::capacity)
            + self.block.retained_span_bytes()
    }

    pub(super) fn update(
        previous: Option<Self>,
        content: &ItemContent,
        theme: crate::settings::EditorSyntaxTheme,
    ) -> Self {
        let mut skill = None;
        let (source, language, output) = match content {
            // Only a shell command is shell: a read's or a search's command
            // is the path or the pattern it was given.
            ItemContent::Command {
                command,
                label,
                action: chat::CommandAction::Run,
                output,
                ..
            } if label.is_empty() || label == command => (
                Cow::Borrowed(turn::first_line(command)),
                Language::Shell,
                output.as_str(),
            ),
            ItemContent::Command {
                action: chat::CommandAction::Run,
                output,
                ..
            } => (
                Cow::Borrowed(turn::step_label(content)),
                Language::Plain,
                output.as_str(),
            ),
            // The last line of a file that was read, or of a search's
            // matches, says nothing about how the step went.
            ItemContent::Command { .. } => (
                Cow::Borrowed(turn::step_label(content)),
                Language::Plain,
                "",
            ),
            // Unknown tools can carry prompts, entire documents, and protocol
            // framing. Their raw arguments and successful output belong to
            // the explicit Input/Output disclosure, not the closed row.
            ItemContent::Tool {
                name,
                input,
                output,
                status,
                ..
            } => {
                let (preview, target) = tool_preview(input, name);
                skill = target;
                (
                    Cow::Owned(preview),
                    Language::Plain,
                    if matches!(
                        status,
                        chat::ToolStatus::Failed
                            | chat::ToolStatus::Denied
                            | chat::ToolStatus::Stopped
                    ) {
                        output.as_str()
                    } else {
                        ""
                    },
                )
            }
            _ => (
                Cow::Borrowed(turn::step_label(content)),
                Language::Plain,
                "",
            ),
        };
        // Bound preview work independently of the complete source kept for
        // expansion and copying. Layout also caps wrapped text at two rows.
        let bounded = |source: &str| {
            let mut chars = source.chars();
            let mut text: String = chars.by_ref().take(512).collect();
            if chars.next().is_some() {
                text.push('…');
            }
            text
        };
        let text = bounded(&source);
        let tail = bounded(
            output
                .rsplit('\n')
                .map(str::trim_end)
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default(),
        );
        if let Some(previous) = previous
            && previous.text == text
            && previous.skill == skill
            && previous.language == language
            && previous.theme == theme
        {
            return Self { tail, ..previous };
        }
        let block = prose::CodeBlock::new(language.token(), &text, theme);
        Self {
            text,
            tail,
            skill,
            language,
            block,
            theme,
        }
    }

    pub(super) fn rehighlight(&mut self, theme: crate::settings::EditorSyntaxTheme) {
        self.theme = theme;
        self.block = prose::CodeBlock::new(self.language.token(), &self.text, theme);
    }

    pub(super) fn view(&self) -> Element<'static, Message> {
        let text = iced::widget::rich_text(self.block.spans(cosmic::theme::is_dark()).clone())
            .font(iced::Font::MONOSPACE)
            .size(design::TEXT_BODY)
            .line_height(1.3)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
            .width(Length::Fill)
            .ellipsize(iced::advanced::text::Ellipsize::End(
                iced::advanced::text::EllipsizeHeightLimit::Lines(2),
            ));
        // Rich text keeps the full paragraph's intrinsic height even with a
        // line-count ellipsis. Supply the matching layout limit as well;
        // Shrink preserves a single row for short previews.
        widget::container(text)
            .width(Length::Fill)
            .max_height(design::TEXT_BODY * 1.3 * 2.0)
            .clip(true)
            .into()
    }

    /// The step's latest output, on one muted line under what it is.
    pub(super) fn tail_view(&self) -> Option<Element<'static, Message>> {
        (!self.tail.is_empty()).then(|| {
            design::caption(self.tail.clone())
                .font(iced::Font::MONOSPACE)
                .wrapping(iced::widget::text::Wrapping::None)
                .ellipsize(iced::advanced::text::Ellipsize::End(
                    iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill)
                .into()
        })
    }
}

impl ToolText {
    fn prepared(&self) -> PreparedText {
        PreparedText {
            key: self.key,
            language: self.language,
            text: self.text.clone(),
            source: self.reader_source.clone(),
        }
    }
    pub(super) fn from_prepared(previous: Option<Self>, prepared: &PreparedText) -> Self {
        if previous.as_ref().is_some_and(|previous| {
            previous.language == prepared.language && Arc::ptr_eq(&previous.text, &prepared.text)
        }) {
            return previous.unwrap();
        }
        Self {
            key: prepared.key,
            reader_identity: previous
                .as_ref()
                .map_or(prepared.key, |previous| previous.reader_identity),
            reader_source: prepared.source.clone(),
            flow_cache: previous
                .as_ref()
                .map(|previous| previous.flow_cache.clone())
                .unwrap_or_default(),
            language: prepared.language,
            text: prepared.text.clone(),
            syntax: std::cell::RefCell::new(None),
        }
    }
    pub(super) fn retained_bytes(&self) -> usize {
        self.text.len()
            + self
                .syntax
                .borrow()
                .as_ref()
                .map_or(0, |(_, block)| block.retained_span_bytes())
    }

    pub(super) fn update(previous: Option<Self>, text: &str, language: Language) -> Self {
        let language = if text.len() > CachedItem::HIGHLIGHT_LIMIT {
            Language::Plain
        } else {
            language
        };
        // The key reads a bounded tail of the text, as an item's change token
        // does. It is only a cheap rejection filter: exact text equality is
        // still required before retaining the previous presentation.
        let key = tail_digest(language as u64, text);
        if previous.as_ref().is_some_and(|previous| {
            previous.key == key && previous.language == language && previous.text.as_ref() == text
        }) {
            return previous.unwrap();
        }
        let reader_source = previous
            .as_ref()
            .filter(|previous| previous.language == language)
            .map_or_else(
                || reader_source::Source::new(text),
                |previous| {
                    reader_source::Source::updated(&previous.reader_source, &previous.text, text)
                },
            );
        let flow_cache = previous
            .as_ref()
            .map(|previous| previous.flow_cache.clone())
            .unwrap_or_default();
        let reader_identity = previous
            .as_ref()
            .map_or(key, |previous| previous.reader_identity);
        Self {
            key,
            reader_identity,
            reader_source,
            flow_cache,
            language,
            text: Arc::from(text),
            syntax: std::cell::RefCell::new(None),
        }
    }

    /// Bounded output follows its last line; matching uses original text bytes.
    pub(super) fn view(
        &self,
        theme: crate::settings::EditorSyntaxTheme,
        limit: Option<f32>,
        target: Target,
        find: Option<&str>,
    ) -> Element<'static, Message> {
        self.surface(
            theme,
            limit.map(|height| (height, true)),
            target,
            find,
            None,
            None,
        )
    }

    /// Patches and read results open at their first line.
    pub(super) fn view_from_start(
        &self,
        theme: crate::settings::EditorSyntaxTheme,
        height: f32,
        target: Target,
        find: Option<&str>,
    ) -> Element<'static, Message> {
        self.surface(theme, Some((height, false)), target, find, None, None)
    }

    pub(super) fn view_logical(
        &self,
        theme: crate::settings::EditorSyntaxTheme,
        limit: Option<(f32, bool)>,
        target: Target,
        find: Option<&str>,
        logical: selectable_prose::Logical,
    ) -> Element<'static, Message> {
        self.surface(theme, limit, target, find, Some(logical), None)
    }

    /// `limit` is the height the text scrolls within and whether it follows
    /// its end.
    pub(super) fn view_logical_with_reader(
        &self,
        theme: crate::settings::EditorSyntaxTheme,
        limit: Option<(f32, bool)>,
        target: Target,
        find: Option<&str>,
        logical: selectable_prose::Logical,
        memory: scroll::Memory,
        context: Option<String>,
    ) -> Element<'static, Message> {
        let identity = logical.identity;
        self.surface(
            theme,
            limit,
            target,
            find,
            Some(logical),
            Some((memory, context, identity)),
        )
    }

    pub(super) fn find_identity(&self) -> u64 {
        self.reader_identity
    }

    fn surface(
        &self,
        theme: crate::settings::EditorSyntaxTheme,
        limit: Option<(f32, bool)>,
        target: Target,
        find: Option<&str>,
        logical: Option<selectable_prose::Logical>,
        reader: Option<(scroll::Memory, Option<String>, u64)>,
    ) -> Element<'static, Message> {
        let reader =
            Some(reader.unwrap_or_else(|| (scroll::Memory::default(), None, self.reader_identity)));
        let mut syntax = self.syntax.borrow_mut();
        if self.text.len() > CachedItem::HIGHLIGHT_LIMIT {
            *syntax = None;
        } else if syntax
            .as_ref()
            .is_none_or(|(previous, _)| *previous != theme)
        {
            *syntax = Some((
                theme,
                prose::CodeBlock::new(self.language.token(), &self.text, theme),
            ));
        }
        let spans = syntax
            .as_ref()
            .map(|(_, block)| block.spans(cosmic::theme::is_dark()).clone())
            .unwrap_or_else(|| Arc::from([]));
        let logical_id = logical.as_ref().map(|logical| logical.identity);
        let previous_width = reader.as_ref().map_or(0.0, |(memory, context, identity)| {
            memory.reader_width_for(
                context.as_deref(),
                scroll::reader_key(*identity, scroll::ReaderAxis::Horizontal),
                &self.reader_source,
            )
        });
        let body = widget::container(prose::code_text(
            self.text.clone(),
            spans,
            design::TEXT_BODY.into(),
            iced::Font::MONOSPACE,
            target,
            find,
            logical,
            Some(self.flow_cache.clone()),
            Some(self.reader_source.clone()),
        ))
        .padding(design::with_vertical_rail(iced::Padding::from([
            step::GAP_S,
            step::GAP_M,
        ])))
        .width(Length::Shrink);
        let body = scroll::minimum_width(body, previous_width);
        let mut horizontal = widget::scrollable(body)
            .direction(iced::widget::scrollable::Direction::Horizontal(
                design::scrollbar(),
            ))
            .width(Length::Fill);
        if let Some(identity) = logical_id {
            let context = reader
                .as_ref()
                .and_then(|(_, context, _)| context.as_deref());
            horizontal = horizontal.id(scroll::block_id(
                target,
                context,
                scroll::reader_key(identity, scroll::ReaderAxis::Horizontal),
            ));
        }
        let body: Element<'static, Message> = match &reader {
            Some((memory, context, identity)) => scroll::block_source(
                horizontal,
                memory.clone(),
                context.clone(),
                scroll::reader_key(*identity, scroll::ReaderAxis::Horizontal),
                false,
                Some(self.reader_source.clone()),
            ),
            None => horizontal.into(),
        };
        // Find reveals the matching source in the outer transcript. A nested
        // viewport would otherwise keep a deep match hidden at its old offset.
        let limit = limit.filter(|_| {
            !find.is_some()
                || !find_navigation::tool_uncapped(target, self.reader_identity, &self.text)
        });
        let Some((height, follows)) = limit else {
            return body;
        };
        let mut scrolled = widget::scrollable(body)
            .direction(iced::widget::scrollable::Direction::Vertical(
                design::scrollbar(),
            ))
            .height(Length::Shrink);
        if let Some(identity) = logical_id {
            let context = reader
                .as_ref()
                .and_then(|(_, context, _)| context.as_deref());
            scrolled = scrolled.id(scroll::block_id(
                target,
                context,
                scroll::reader_key(identity, scroll::ReaderAxis::Vertical),
            ));
        }
        let scrolled: Element<'static, Message> = if follows {
            scroll::tool_output(scrolled.anchor_bottom())
        } else {
            scroll::viewport(scrolled)
        };
        let scrolled = match reader {
            Some((memory, context, identity)) => scroll::block_source(
                scrolled,
                memory,
                context,
                scroll::reader_key(identity, scroll::ReaderAxis::Vertical),
                follows,
                Some(self.reader_source.clone()),
            ),
            None => scrolled,
        };
        widget::container(scrolled).max_height(height).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Geometry fixtures explicitly finish the same preparation stage before
    // inspecting glyphs. Timed frame/native workloads never use this helper.
    fn laid_out(
        block: &ToolText,
        make: &impl Fn() -> Element<'static, Message>,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
        limits: &iced::advanced::layout::Limits,
    ) -> (Element<'static, Message>, iced::advanced::layout::Node) {
        let mut element = make();
        element.as_widget_mut().diff(tree);
        let mut node = element.as_widget_mut().layout(tree, renderer, limits);
        for _ in 0..3 {
            if !block.flow_cache.complete_fixture() {
                break;
            }
            element = make();
            element.as_widget_mut().diff(tree);
            node = element.as_widget_mut().layout(tree, renderer, limits);
        }
        (element, node)
    }
    #[test]
    fn view_shares_cached_syntax_spans_and_keeps_complete_tool_text() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree},
        };
        struct ReadText(Vec<Arc<str>>);
        impl Operation for ReadText {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn custom(
                &mut self,
                _: Option<&widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(prose) = state.downcast_ref::<selectable_prose::State>() {
                    self.0.push(prose.plain.clone());
                }
            }
        }
        let renderer = crate::app::tests::headless_renderer();
        let theme = crate::settings::EditorSyntaxTheme::default();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let source = "printf 'λ'\nsecond line\n";
        let mut block = ToolText::update(None, source, Language::Shell);
        let read = |block: &ToolText| {
            let mut element = block.view(theme, None, target, None);
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(400.0, 400.0)),
            );
            let mut read = ReadText(Vec::new());
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
            assert_eq!(read.0.len(), 1);
            read.0.pop().unwrap()
        };
        assert_eq!(read(&block).as_ref(), source);
        let first = block
            .syntax
            .borrow()
            .as_ref()
            .unwrap()
            .1
            .spans(cosmic::theme::is_dark())
            .clone();
        read(&block);
        assert!(Arc::ptr_eq(
            &first,
            block
                .syntax
                .borrow()
                .as_ref()
                .unwrap()
                .1
                .spans(cosmic::theme::is_dark())
        ));
        let appended = format!("{source}next\n");
        block = ToolText::update(Some(block), &appended, Language::Shell);
        assert_eq!(read(&block).as_ref(), appended);
        assert!(!Arc::ptr_eq(
            &first,
            block
                .syntax
                .borrow()
                .as_ref()
                .unwrap()
                .1
                .spans(cosmic::theme::is_dark())
        ));
    }

    #[test]
    fn long_tool_lines_have_distinct_horizontal_viewports_and_accessible_tail_glyphs() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree, operation},
        };
        struct Move {
            horizontal: widget::Id,
            vertical: widget::Id,
            bottom: bool,
        }
        impl Operation for Move {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                _: iced::Rectangle,
                _: iced::Rectangle,
                _: iced::Vector,
                state: &mut dyn operation::Scrollable,
            ) {
                if id == Some(&self.horizontal) {
                    state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                        x: Some(f32::MAX),
                        y: None,
                    });
                } else if id == Some(&self.vertical) {
                    state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(if self.bottom { 0.0 } else { f32::MAX }),
                    });
                }
            }
        }
        #[derive(Default)]
        struct Tail {
            translation: iced::Vector,
            pending: Option<iced::Vector>,
            glyphs: Vec<iced::Rectangle>,
            ids: HashSet<widget::Id>,
            duplicate: bool,
            horizontal_travel: f32,
        }
        impl Operation for Tail {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                let before = self.translation;
                if let Some(translation) = self.pending.take() {
                    self.translation += translation;
                }
                visit(self);
                self.translation = before;
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                viewport: iced::Rectangle,
                content: iced::Rectangle,
                translation: iced::Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                if let Some(id) = id {
                    self.duplicate |= !self.ids.insert(id.clone());
                }
                self.horizontal_travel = self.horizontal_travel.max(content.width - viewport.width);
                self.pending = Some(translation);
            }
            fn custom(
                &mut self,
                _: Option<&widget::Id>,
                bounds: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                let Some(state) = state.downcast_ref::<selectable_prose::State>() else {
                    return;
                };
                let byte = 300 * "ordinary line\n".len() + 5999;
                if let Some(mut glyph) = state.logical_byte_bounds(42, byte) {
                    glyph.x += bounds.x - self.translation.x;
                    glyph.y += bounds.y - self.translation.y;
                    self.glyphs.push(glyph);
                }
            }
        }
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let source = format!("{}{}", "ordinary line\n".repeat(300), "x".repeat(6000));
        let block = ToolText::update(None, &source, Language::Plain);
        for limit in [None, Some((120.0, false)), Some((120.0, true))] {
            let memory = scroll::Memory::default();
            let make = || {
                scroll::viewport(block.view_logical_with_reader(
                    crate::settings::EditorSyntaxTheme::Auto,
                    limit,
                    target,
                    None,
                    selectable_prose::Logical {
                        identity: 42,
                        start: 0,
                    },
                    memory.clone(),
                    None,
                ))
            };
            let mut tree = Tree::new(&make());
            let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(400.0, 20000.0));
            let (mut element, mut node) = laid_out(&block, &make, &mut tree, &renderer, &limits);
            for _ in 0..3 {
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut Move {
                        horizontal: scroll::block_id(
                            target,
                            None,
                            scroll::reader_key(42, scroll::ReaderAxis::Horizontal),
                        ),
                        vertical: scroll::block_id(
                            target,
                            None,
                            scroll::reader_key(42, scroll::ReaderAxis::Vertical),
                        ),
                        bottom: limit.is_some_and(|(_, bottom)| bottom),
                    },
                );
                (element, node) = laid_out(&block, &make, &mut tree, &renderer, &limits);
            }
            let mut tail = Tail::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut tail);
            assert!(!tail.duplicate, "nested axes must have distinct IDs");
            assert!(
                tail.horizontal_travel > 10000.0,
                "long line needs real native horizontal travel"
            );
            assert!(
                tail.glyphs.iter().any(|glyph| glyph.x >= -1.0
                    && glyph.x + glyph.width <= 401.0
                    && glyph.y >= -1.0
                    && glyph.y + glyph.height <= node.size().height + 1.0),
                "tail glyph must be accessible on both axes inside {:?}: {:?}",
                node.size(),
                tail.glyphs
            );
        }
    }

    #[test]
    fn horizontal_bookmark_survives_wide_line_eviction_and_tree_remount() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree, operation},
        };
        struct Probe {
            horizontal: widget::Id,
            vertical: widget::Id,
            x: Option<f32>,
            y: Option<f32>,
            observed: Option<(f32, f32)>,
        }
        impl Operation for Probe {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                _: iced::Rectangle,
                content: iced::Rectangle,
                translation: iced::Vector,
                state: &mut dyn operation::Scrollable,
            ) {
                if id == Some(&self.horizontal) {
                    self.observed = Some((translation.x, content.width));
                    if let Some(x) = self.x {
                        state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                            x: Some(x),
                            y: None,
                        });
                    }
                } else if id == Some(&self.vertical)
                    && let Some(y) = self.y
                {
                    state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(y),
                    });
                }
            }
        }
        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let memory = scroll::Memory::default();
        let block = ToolText::update(
            None,
            &format!("{}\n{}", "x".repeat(6000), "short line\n".repeat(1000)),
            Language::Plain,
        );
        let make = || {
            block.view_logical_with_reader(
                crate::settings::EditorSyntaxTheme::Auto,
                Some((120.0, false)),
                target,
                None,
                selectable_prose::Logical {
                    identity: 42,
                    start: 0,
                },
                memory.clone(),
                None,
            )
        };
        let probe = |x, y| Probe {
            horizontal: scroll::block_id(
                target,
                None,
                scroll::reader_key(42, scroll::ReaderAxis::Horizontal),
            ),
            vertical: scroll::block_id(
                target,
                None,
                scroll::reader_key(42, scroll::ReaderAxis::Vertical),
            ),
            x,
            y,
            observed: None,
        };
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(400.0, 2000.0));
        let mut element = make();
        let mut tree = Tree::new(&element);
        let (mut element, mut node) = laid_out(&block, &make, &mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut probe(Some(10000.0), None),
        );
        (element, node) = laid_out(&block, &make, &mut tree, &renderer, &limits);
        let mut read = probe(None, None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        let (x, width) = read.observed.unwrap();
        assert!((x - 10000.0).abs() < 1.0 && width > 30000.0);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut probe(None, Some(f32::MAX)),
        );
        (element, node) = laid_out(&block, &make, &mut tree, &renderer, &limits);
        let mut read = probe(None, None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        assert_eq!(
            read.observed.unwrap(),
            (x, width),
            "short visible lines must not collapse measured horizontal travel"
        );
        drop(tree);
        drop(element);
        let mut element = make();
        let mut tree = Tree::new(&element);
        let (mut element, mut node) = laid_out(&block, &make, &mut tree, &renderer, &limits);
        let mut read = probe(None, None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        assert_eq!(
            read.observed.unwrap(),
            (x, width),
            "cheap geometry restores x while the wide line is still evicted"
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut probe(None, Some(0.0)),
        );
        (element, node) = laid_out(&block, &make, &mut tree, &renderer, &limits);
        let mut read = probe(None, None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        assert!(
            (read.observed.unwrap().0 - x).abs() < 1.0,
            "returning to the wide line keeps the horizontal bookmark"
        );
    }

    /// Output is opened at its last line and a patch at its first.
    #[test]
    fn output_opens_at_its_end_and_a_patch_at_its_start() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree, operation},
        };
        struct Scrolled(Vec<f32>);
        impl Operation for Scrolled {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                _: Option<&widget::Id>,
                viewport: iced::Rectangle,
                content: iced::Rectangle,
                translation: iced::Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                if content.height > viewport.height + 0.5 {
                    self.0.push(translation.y);
                }
            }
        }
        let renderer = crate::app::tests::headless_renderer();
        let theme = crate::settings::EditorSyntaxTheme::default();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let text = (0..200)
            .map(|line| format!("+line {line}\n"))
            .collect::<String>();
        let block = ToolText::update(None, &text, Language::Diff);
        let offset = |mut element: Element<'static, Message>| {
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(400.0, 2000.0)),
            );
            assert!(node.size().height <= 121.0, "{:?}", node.size());
            let mut scrolled = Scrolled(Vec::new());
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut scrolled,
            );
            assert_eq!(scrolled.0.len(), 1);
            scrolled.0[0]
        };
        assert_eq!(
            offset(block.view_from_start(theme, 120.0, target, None)),
            0.0
        );
        assert!(offset(block.view(theme, Some(120.0), target, None)) > 0.0);
    }

    #[test]
    fn finding_a_deep_source_match_reveals_it_without_an_inner_scroll_barrier() {
        use iced::advanced::{layout, widget::Tree};
        let renderer = crate::app::tests::headless_renderer();
        let source = format!("{}needle at the end", "ordinary line\n".repeat(200));
        let item = chat::Item {
            id: "deep-tool".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Tool {
                name: "FixtureInspect".into(),
                input: "{}".into(),
                output: source,
                status: chat::ToolStatus::Succeeded,
            },
            at: None,
            started_at: None,
        };
        let (runtime, _handle) = view::prepared_runtime_for_test(&[item]);
        let target = runtime.target;
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
        drop(app.handle_chat_ui(Action::Toggle(target, "deep-tool".into())));
        let mut height = |query: Option<&str>| {
            if let Some(query) = query {
                drop(find_navigation::find_for_test(
                    &mut app,
                    target,
                    query.into(),
                ));
            } else {
                drop(app.handle_chat_ui(Action::CloseFind(target)));
            }
            let block = app.chat_runtime_mut(target).unwrap().cache["deep-tool"]
                .tool_output
                .as_ref()
                .expect("actual expanded tool output");
            let mut element = block.view_from_start(
                crate::settings::EditorSyntaxTheme::Auto,
                320.0,
                target,
                query,
            );
            let mut tree = Tree::new(&element);
            element
                .as_widget_mut()
                .layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(iced::Size::ZERO, iced::Size::new(640.0, 10000.0)),
                )
                .size()
                .height
        };
        assert!(height(None) <= 320.0);
        assert!(height(Some("absent")) <= 320.0);
        assert!(height(Some("needle")) > 320.0);
        assert!(
            height(None) <= 320.0,
            "closing Find restores bounded details"
        );
    }

    #[test]
    fn expanded_patches_retain_lines_beyond_the_old_inline_limit() {
        let diff = (0..600)
            .map(|line| format!("+let value_{line} = {line};\n"))
            .collect::<String>();
        let item = ItemContent::Changes {
            changes: vec![chat::FileChange {
                path: "src/example.rs".into(),
                kind: chat::ChangeKind::Updated,
                diff: diff.clone(),
            }],
            status: chat::ToolStatus::Succeeded,
        };
        let theme = crate::settings::EditorSyntaxTheme::default();
        let collapsed = CachedItem::updated(&item, theme, None, false, false, None);
        assert!(collapsed.patches.is_empty());
        let expanded = CachedItem::updated(&item, theme, Some(collapsed), true, false, None);
        assert_eq!(expanded.patches[0].text.as_ref(), diff);
        assert_eq!(
            copy_text(CopyPart::Change(0), &item).as_deref(),
            Some(diff.as_str())
        );
        assert!(expanded.patches[0].text.as_ref().contains("value_599"));
    }

    fn tool(input: &str) -> ItemContent {
        ItemContent::Tool {
            name: "fixture".into(),
            input: input.into(),
            output: String::new(),
            status: chat::ToolStatus::Succeeded,
        }
    }

    fn command(
        label: &str,
        command: &str,
        action: chat::CommandAction,
        output: &str,
    ) -> ItemContent {
        ItemContent::Command {
            label: label.into(),
            command: command.into(),
            working_directory: String::new(),
            action,
            status: chat::ToolStatus::Running,
            exit_code: None,
            duration_ms: None,
            output: output.into(),
        }
    }

    /// A row goes on saying what ran while its output scrolls past under it.
    #[test]
    fn output_follows_a_step_without_replacing_what_it_is() {
        use chat::CommandAction::{Read, Run};
        let theme = crate::settings::EditorSyntaxTheme::Auto;
        for (content, text, language, tail) in [
            (
                command("", "cargo test", Run, ""),
                "cargo test",
                Language::Shell,
                "",
            ),
            (
                command(
                    "",
                    "cargo test",
                    Run,
                    "   Compiling a\n   Compiling b  \n\n",
                ),
                "cargo test",
                Language::Shell,
                "   Compiling b",
            ),
            (
                command("Run the tests", "cargo test", Run, "ok\n"),
                "Run the tests",
                Language::Plain,
                "ok",
            ),
            // A read's command is a path, its output is the file, and a
            // stored label loses its verb.
            (
                command("src/app.rs", "src/app.rs", Read, "fn main() {}\n"),
                "src/app.rs",
                Language::Plain,
                "",
            ),
            (
                command("Read src/app.rs", "sed -n 1,9p src/app.rs", Read, ""),
                "src/app.rs",
                Language::Plain,
                "",
            ),
            (tool("{\"query\":\"x\"}"), "x", Language::Plain, ""),
        ] {
            let preview = Preview::update(None, &content, theme);
            assert_eq!(
                (
                    preview.text.as_str(),
                    preview.language,
                    preview.tail.as_str()
                ),
                (text, language, tail),
                "{content:?}"
            );
            assert_eq!(preview.tail_view().is_some(), !tail.is_empty());
        }
        let mut finished = tool("{\"query\":\"x\"}");
        if let ItemContent::Tool { output, .. } = &mut finished {
            *output = "first\nlast\n".into();
        }
        // Successful generic output stays in the explicit disclosure.
        let preview = Preview::update(None, &finished, theme);
        assert_eq!(
            (
                preview.text.as_str(),
                preview.language,
                preview.tail.as_str()
            ),
            ("x", Language::Plain, "")
        );
        // New output moves the tail and leaves the highlighted command alone.
        let before = Preview::update(None, &command("", "cargo test", Run, "one\n"), theme);
        let spans = before.block.spans(true).clone();
        let after = Preview::update(
            Some(before),
            &command("", "cargo test", Run, "one\ntwo\n"),
            theme,
        );
        assert_eq!(after.tail, "two");
        assert!(Arc::ptr_eq(&spans, after.block.spans(true)));
    }

    #[test]
    fn generic_tool_payloads_stay_in_disclosure_across_streaming_and_completion() {
        let theme = crate::settings::EditorSyntaxTheme::Auto;
        let input = serde_json::json!({
            "path": "src/app.rs",
            "prompt": "Internal framing\n".repeat(600),
        })
        .to_string();
        let output = "Complete document\nInternal protocol framing";
        let mut content = tool(&input);
        for status in [chat::ToolStatus::Running, chat::ToolStatus::Succeeded] {
            if let ItemContent::Tool {
                status: current,
                output: body,
                ..
            } = &mut content
            {
                *current = status;
                *body = output.into();
            }
            let preview = Preview::update(None, &content, theme);
            assert_eq!(preview.text, "src/app.rs");
            assert!(preview.tail.is_empty());
            let closed = CachedItem::updated(&content, theme, None, false, false, None);
            assert!(closed.tool_input.is_none());
            let open = CachedItem::updated(&content, theme, Some(closed), true, false, None);
            assert_eq!(open.tool_input.as_ref().unwrap().text.as_ref(), input);
            assert_eq!(open.tool_output.as_ref().unwrap().text.as_ref(), output);
            assert_eq!(
                copy_text(CopyPart::Input, &content).as_deref(),
                Some(input.as_str())
            );
            assert_eq!(
                copy_text(CopyPart::Output, &content).as_deref(),
                Some(output)
            );
        }
        // An incomplete JSON prefix never flashes its prompt onto the row.
        let prefix = tool(r#"{"prompt":"Internal framing"#);
        assert_eq!(
            Preview::update(None, &prefix, theme).text,
            "Input available"
        );
        // Unknown fields and multiline values disclose their structure only.
        for (input, expected) in [
            (
                r#"{"prompt":"Internal framing","body":"document"}"#,
                "2 input fields",
            ),
            (r#"{"query":"first\nsecond"}"#, "1 input field"),
            (
                r#"{"skill":"writing-copy","args":"Internal framing"}"#,
                "writing-copy",
            ),
        ] {
            assert_eq!(Preview::update(None, &tool(input), theme).text, expected);
        }
    }

    #[test]
    fn generic_tool_failures_keep_actionable_output_visible() {
        for status in [
            chat::ToolStatus::Failed,
            chat::ToolStatus::Denied,
            chat::ToolStatus::Stopped,
        ] {
            let content = ItemContent::Tool {
                name: "mcp__fixture__inspect".into(),
                input: r#"{"path":"src/app.rs","prompt":"internal framing"}"#.into(),
                output: "Attempted read\nPermission denied\n".into(),
                status,
            };
            let preview = Preview::update(None, &content, crate::settings::EditorSyntaxTheme::Auto);
            assert_eq!(preview.text, "src/app.rs");
            assert_eq!(preview.tail, "Permission denied");
        }
    }

    #[test]
    fn appended_output_preserves_complete_text() {
        let initial = "first λ line\nselected text\nlast";
        let mut block = ToolText::update(None, initial, Language::Plain);
        for suffix in [" + continuation", "\nnew λ line\n"] {
            let complete = format!("{}{suffix}", block.text);
            block = ToolText::update(Some(block), &complete, Language::Plain);
            assert_eq!(block.text.as_ref(), complete);
        }
    }

    #[test]
    fn rewritten_output_replaces_text_instead_of_appending() {
        let block = ToolText::update(None, "original λ", Language::Plain);
        let block = ToolText::update(Some(block), "replacement λ", Language::Plain);
        assert_eq!(block.text.as_ref(), "replacement λ");
    }

    #[test]
    fn collapsed_cache_is_lazy_and_metadata_updates_reuse_expanded_source() {
        let source = format!("{{\"text\":\"{}\"}}", "λ".repeat(700));
        let mut item = tool(&source);
        let theme = crate::settings::EditorSyntaxTheme::Auto;
        let cached = CachedItem::updated(&item, theme, None, false, false, None);
        assert!(cached.tool_input.is_none());
        assert!(cached.tool_output.is_none());
        assert!(cached.preview.text.len() < source.len());
        assert_eq!(
            copy_text(CopyPart::Input, &item).as_deref(),
            Some(source.as_str())
        );
        let cached = CachedItem::updated(&item, theme, Some(cached), true, false, None);
        let source_cache = cached.tool_input.as_ref().unwrap().text.clone();
        if let ItemContent::Tool { status, .. } = &mut item {
            *status = chat::ToolStatus::Failed;
        }
        let cached = CachedItem::updated(&item, theme, Some(cached), true, false, None);
        assert!(Arc::ptr_eq(
            &source_cache,
            &cached.tool_input.as_ref().unwrap().text
        ));
        let cached = CachedItem::updated(&item, theme, Some(cached), false, false, None);
        assert!(cached.tool_input.is_none());
        assert!(cached.tool_output.is_none());
        assert_eq!(
            copy_text(CopyPart::Input, &item).as_deref(),
            Some(source.as_str())
        );
    }

    #[test]
    fn shell_preview_hides_payload_but_copy_preserves_complete_command() {
        let source = "cat > tests.rs <<'EOF'\nprivate payload\nEOF";
        let content = ItemContent::Command {
            label: source.into(),
            command: source.into(),
            working_directory: String::new(),
            action: chat::CommandAction::Run,
            status: chat::ToolStatus::Running,
            exit_code: None,
            duration_ms: None,
            output: String::new(),
        };
        let preview = Preview::update(None, &content, crate::settings::EditorSyntaxTheme::Auto);
        assert_eq!(preview.text.as_str(), "cat > tests.rs <<'EOF'");
        assert_eq!(
            copy_text(CopyPart::Input, &content).as_deref(),
            Some(source)
        );
        let cached = CachedItem::updated(
            &content,
            crate::settings::EditorSyntaxTheme::Auto,
            None,
            true,
            false,
            None,
        );
        assert_eq!(cached.tool_input.as_ref().unwrap().text.as_ref(), source);
    }

    #[test]
    fn structured_highlighting_is_bounded_without_truncating_full_content() {
        assert_eq!(
            Language::structured(" {\"value\": [1, true]}"),
            Language::Json
        );
        assert_eq!(Language::structured("{partial"), Language::Plain);
        assert_eq!(Language::structured("ordinary log output"), Language::Plain);
        let large = format!(
            "{{\"text\":\"{}\"}}",
            "λ".repeat(CachedItem::HIGHLIGHT_LIMIT)
        );
        assert_eq!(Language::structured(&large), Language::Plain);
        let block = ToolText::update(None, &large, Language::Json);
        assert_eq!(block.language, Language::Plain);
        assert_eq!(block.text.as_ref(), large);
        let preview = Preview::update(
            None,
            &tool(&large),
            crate::settings::EditorSyntaxTheme::Auto,
        );
        assert_eq!(preview.text, "Input available");
        // A partial JSON stream can consist entirely of whitespace; it is
        // still bounded before inspecting any of those bytes.
        let whitespace = " ".repeat(CachedItem::HIGHLIGHT_LIMIT + 1);
        assert_eq!(
            Preview::update(
                None,
                &tool(&whitespace),
                crate::settings::EditorSyntaxTheme::Auto
            )
            .text,
            "Input available"
        );
        assert!(
            Preview::update(
                None,
                &tool(" \n\t"),
                crate::settings::EditorSyntaxTheme::Auto
            )
            .text
            .is_empty()
        );
    }

    #[test]
    fn shell_preview_has_syntax_in_both_palettes_and_rethemes() {
        let mut preview = Preview::update(
            None,
            &command("", "cargo test --locked", chat::CommandAction::Run, ""),
            crate::settings::EditorSyntaxTheme::InspiredGitHub,
        );
        assert_eq!(preview.language, Language::Shell);
        for dark in [false, true] {
            assert!(preview.block.spans(dark).len() > 1);
        }
        let previous = preview.block.spans(true).clone();
        preview.rehighlight(crate::settings::EditorSyntaxTheme::SolarizedDark);
        assert_ne!(*preview.block.spans(true), previous);
    }

    #[test]
    fn narrow_preview_is_limited_to_two_visual_lines() {
        use iced::advanced::{layout, renderer::Headless, widget::Tree};
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("executor")
            .block_on(cosmic::Renderer::new(
                iced::Font::MONOSPACE,
                iced::Pixels(design::TEXT_BODY),
                Some("tiny-skia"),
            ))
            .expect("headless renderer");
        for text in [
            "longword".repeat(100),
            "one two three four five six seven eight nine ten".into(),
        ] {
            let preview = Preview::update(
                None,
                &command("", &text, chat::CommandAction::Run, ""),
                crate::settings::EditorSyntaxTheme::Auto,
            );
            let mut element = preview.view();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(110.0, 1000.0)),
            );
            assert!(node.size().height > design::TEXT_BODY * 1.3);
            assert!(
                node.size().height <= design::TEXT_BODY * 1.3 * 2.0 + 1.0,
                "{:?}",
                node.size()
            );
        }
        let preview = Preview::update(
            None,
            &command("", "short", chat::CommandAction::Run, ""),
            crate::settings::EditorSyntaxTheme::Auto,
        );
        let mut element = preview.view();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(110.0, 1000.0)),
        );
        assert!(
            node.size().height <= design::TEXT_BODY * 1.3 + 1.0,
            "{:?}",
            node.size()
        );
    }
}
