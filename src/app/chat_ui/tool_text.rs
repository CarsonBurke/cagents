//! Retained, selectable read-only tool content. Copy always uses the snapshot.
use super::*;

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

pub(super) struct ToolText {
    key: u64,
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
    language: Language,
    /// The text highlighted in both palettes, drawn from on every frame.
    block: prose::CodeBlock,
    theme: crate::settings::EditorSyntaxTheme,
}

impl Preview {
    pub(super) fn retained_bytes(&self) -> usize {
        self.text.capacity() + self.tail.capacity() + self.block.retained_span_bytes()
    }


    pub(super) fn update(
        previous: Option<Self>,
        content: &ItemContent,
        theme: crate::settings::EditorSyntaxTheme,
    ) -> Self {
        let (source, language, output) = match content {
            // Only a shell command is shell: a read's or a search's command
            // is the path or the pattern it was given.
            ItemContent::Command {
                command,
                label,
                action: chat::CommandAction::Run,
                output,
                ..
            } if label.is_empty() || label == command => {
                (turn::first_line(command), Language::Shell, output.as_str())
            }
            ItemContent::Command {
                action: chat::CommandAction::Run,
                output,
                ..
            } => (turn::step_label(content), Language::Plain, output.as_str()),
            // The last line of a file that was read, or of a search's
            // matches, says nothing about how the step went.
            ItemContent::Command { .. } => (turn::step_label(content), Language::Plain, ""),
            // The row already names the tool, so its second line is what
            // the tool was called with. That stays once there is output: a
            // name alone does not tell one call of a tool from the next.
            ItemContent::Tool { input, output, .. } => {
                (input.as_str(), Language::structured(input), output.as_str())
            }
            _ => (turn::step_label(content), Language::Plain, ""),
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
        let text = bounded(source);
        let tail = bounded(
            output
                .rsplit('\n')
                .map(str::trim_end)
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default(),
        );
        if let Some(previous) = previous
            && previous.text == text
            && previous.language == language
            && previous.theme == theme
        {
            return Self { tail, ..previous };
        }
        let block = prose::CodeBlock::new(language.token(), &text, theme);
        Self {
            text,
            tail,
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
    pub(super) fn retained_bytes(&self) -> usize {
        self.text.len() + self.syntax.borrow().as_ref().map_or(0, |(_, block)| block.retained_span_bytes())
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
        if let Some(previous) = previous
            && previous.key == key
            && previous.language == language
            && previous.text.as_ref() == text
        {
            return previous;
        }
        Self {
            key,
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
        self.surface(theme, limit.map(|height| (height, true)), target, find, None, None)
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
        &self, theme: crate::settings::EditorSyntaxTheme, limit: Option<(f32, bool)>,
        target: Target, find: Option<&str>, logical: selectable_prose::Logical,
    ) -> Element<'static, Message> {
        self.surface(theme, limit, target, find, Some(logical), None)
    }

    /// `limit` is the height the text scrolls within and whether it follows
    /// its end.
    pub(super) fn view_logical_with_reader(
        &self, theme: crate::settings::EditorSyntaxTheme, limit: Option<(f32, bool)>,
        target: Target, find: Option<&str>, logical: selectable_prose::Logical,
        memory: scroll::Memory, context: Option<String>,
    ) -> Element<'static, Message> {
        let identity = logical.identity;
        self.surface(theme, limit, target, find, Some(logical), Some((memory, context, identity)))
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
        let mut syntax = self.syntax.borrow_mut();
        if self.text.len() > CachedItem::HIGHLIGHT_LIMIT { *syntax = None; }
        else if syntax
            .as_ref()
            .is_none_or(|(previous, _)| *previous != theme)
        {
            *syntax = Some((
                theme,
                prose::CodeBlock::new(self.language.token(), &self.text, theme),
            ));
        }
        let spans = syntax.as_ref().map(|(_, block)| block.spans(cosmic::theme::is_dark()).clone())
            .unwrap_or_else(|| Arc::from([]));
        let body: Element<'static, Message> = widget::container(prose::code_text(
            self.text.clone(), spans, design::TEXT_BODY.into(), iced::Font::MONOSPACE,
            target, find, logical,
        ))
        .padding(design::with_vertical_rail(iced::Padding::from([
            step::GAP_S, step::GAP_M,
        ])))
        .width(Length::Fill)
        .into();
        // Find reveals the matching source in the outer transcript. A nested
        // viewport would otherwise keep a deep match hidden at its old offset.
        let limit = limit.filter(|_| !find.is_some_and(|query| find::contains(&self.text, query)));
        let Some((height, follows)) = limit else {
            return body;
        };
        let scrolled = widget::scrollable(body)
            .direction(iced::widget::scrollable::Direction::Vertical(
                design::scrollbar(),
            ))
            .height(Length::Shrink);
        let scrolled: Element<'static, Message> = if follows {
            scroll::tool_output(scrolled.anchor_bottom())
        } else { scrolled.into() };
        let scrolled = match reader {
            Some((memory, context, identity)) => scroll::block(scrolled, memory, context, identity, follows),
            None => scrolled,
        };
        widget::container(scrolled).max_height(height).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                _: iced::Rectangle,
                _: iced::Rectangle,
                translation: iced::Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                self.0.push(translation.y);
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
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let source = format!("{}needle at the end", "ordinary line\n".repeat(200));
        let block = ToolText::update(None, &source, Language::Shell);
        let height = |query| {
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
            (
                tool("{\"query\":\"x\"}"),
                "{\"query\":\"x\"}",
                Language::Json,
                "",
            ),
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
        // A finished tool is still the call it was, with its output under it.
        let preview = Preview::update(None, &finished, theme);
        assert_eq!(
            (
                preview.text.as_str(),
                preview.language,
                preview.tail.as_str()
            ),
            ("{\"query\":\"x\"}", Language::Json, "last")
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
        assert_eq!(preview.text.chars().count(), 513);
        assert!(preview.text.ends_with('…'));
    }

    #[test]
    fn structured_preview_has_syntax_in_both_palettes_and_rethemes() {
        let mut preview = Preview::update(
            None,
            &tool("{\"message\":\"hello\",\"count\":42}"),
            crate::settings::EditorSyntaxTheme::InspiredGitHub,
        );
        assert_eq!(preview.language, Language::Json);
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
        for text in ["longword".repeat(100), "one\ntwo\nthree\nfour".into()] {
            let preview =
                Preview::update(None, &tool(&text), crate::settings::EditorSyntaxTheme::Auto);
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
            &tool("short"),
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
