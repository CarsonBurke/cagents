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

pub(super) struct Preview {
    pub(super) text: String,
    language: Language,
    /// The text highlighted in both palettes, drawn from on every frame.
    block: prose::CodeBlock,
    theme: crate::settings::EditorSyntaxTheme,
}

impl Preview {
    pub(super) fn update(
        previous: Option<Self>,
        content: &ItemContent,
        theme: crate::settings::EditorSyntaxTheme,
    ) -> Self {
        let (source, language) = match content {
            ItemContent::Command { output, .. } | ItemContent::Tool { output, .. }
                if !output.trim().is_empty() =>
            {
                (
                    output
                        .rsplit('\n')
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or(output),
                    Language::Plain,
                )
            }
            ItemContent::Command { command, label, .. } if label.is_empty() || label == command => {
                (command.as_str(), Language::Shell)
            }
            ItemContent::Tool { input, output, .. } => {
                let source = if input.is_empty() { output } else { input };
                (source.as_str(), Language::structured(source))
            }
            _ => (turn::step_label(content), Language::Plain),
        };
        // Bound preview work independently of the complete source kept for
        // expansion and copying. Layout also caps wrapped text at two rows.
        let mut chars = source.chars();
        let mut text: String = chars.by_ref().take(512).collect();
        if chars.next().is_some() {
            text.push('…');
        }
        if let Some(previous) = previous
            && previous.text == text
            && previous.language == language
            && previous.theme == theme
        {
            return previous;
        }
        let block = prose::CodeBlock::new(language.token(), &text, theme);
        Self {
            text,
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
}

impl ToolText {
    pub(super) fn update(previous: Option<Self>, text: &str, language: Language) -> Self {
        let language = if text.len() > CachedItem::HIGHLIGHT_LIMIT {
            Language::Plain
        } else {
            language
        };
        // The key reads a bounded tail of the text, as an item's change token
        // does, rather than hashing all of it: a running command's output is
        // updated on every refresh, and grows for as long as it runs.
        let key = tail_digest(language as u64, text);
        if let Some(previous) = previous
            && previous.key == key
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

    pub(super) fn view(
        &self,
        theme: crate::settings::EditorSyntaxTheme,
        limit: Option<f32>,
        target: Target,
    ) -> Element<'static, Message> {
        let mut syntax = self.syntax.borrow_mut();
        if syntax
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
            .unwrap()
            .1
            .spans(cosmic::theme::is_dark())
            .clone();
        let body: Element<'static, Message> = widget::container(selectable_prose::text(
            spans,
            design::TEXT_BODY.into(),
            iced::Font::MONOSPACE,
            target,
            None,
        ))
        .padding(design::with_vertical_rail(iced::Padding::from([
            step::GAP_S,
            step::GAP_M,
        ])))
        .width(Length::Fill)
        .into();
        match limit {
            Some(height) => widget::container(scroll::tool_output(
                widget::scrollable(body)
                    .direction(iced::widget::scrollable::Direction::Vertical(
                        design::scrollbar(),
                    ))
                    .anchor_bottom()
                    .height(Length::Shrink),
            ))
            .max_height(height)
            .into(),
            None => body,
        }
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
            let mut element = block.view(theme, None, target);
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
        let collapsed = CachedItem::updated(&item, theme, None, false);
        assert!(collapsed.patches.is_empty());
        let expanded = CachedItem::updated(&item, theme, Some(collapsed), true);
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
        let cached = CachedItem::updated(&item, theme, None, false);
        assert!(cached.tool_input.is_none());
        assert!(cached.tool_output.is_none());
        assert!(cached.preview.text.len() < source.len());
        assert_eq!(
            copy_text(CopyPart::Input, &item).as_deref(),
            Some(source.as_str())
        );
        let cached = CachedItem::updated(&item, theme, Some(cached), true);
        let source_cache = cached.tool_input.as_ref().unwrap().text.clone();
        if let ItemContent::Tool { status, .. } = &mut item {
            *status = chat::ToolStatus::Failed;
        }
        let cached = CachedItem::updated(&item, theme, Some(cached), true);
        assert!(Arc::ptr_eq(
            &source_cache,
            &cached.tool_input.as_ref().unwrap().text
        ));
        let cached = CachedItem::updated(&item, theme, Some(cached), false);
        assert!(cached.tool_input.is_none());
        assert!(cached.tool_output.is_none());
        assert_eq!(
            copy_text(CopyPart::Input, &item).as_deref(),
            Some(source.as_str())
        );
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
