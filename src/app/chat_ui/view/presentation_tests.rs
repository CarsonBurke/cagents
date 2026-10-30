//! Skill disclosure contracts using prepared content and the real widget tree.
//! These fixtures do not execute a provider, native accessibility bus or clipboard.
use super::*;
use iced::advanced::widget::Operation;
use iced::runtime::{UserInterface, user_interface::Cache};
use iced_accessibility::accesskit::Role;

fn tool(name: &str, input: &str, status: chat::ToolStatus) -> chat::Item {
    chat::Item {
        id: "skill-target".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: name.into(),
            input: input.into(),
            output: "Complete provider output λ\nPermission denied\n".into(),
            status,
        },
        started_at: None,
        at: None,
    }
}

#[derive(Default)]
struct Mounted(Vec<String>);
impl Operation for Mounted {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }

    fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
        self.0.push(text.into());
    }
}

fn inspect(
    element: Element<'static, Message>,
    width: f32,
    cache: Cache,
    renderer: &mut cosmic::Renderer,
) -> (Cache, Vec<(Role, String)>, Vec<String>) {
    let mut ui = UserInterface::build(element, iced::Size::new(width, 1000.0), cache, renderer);
    let accessible = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
    let labels = accessible
        .root()
        .iter()
        .chain(accessible.children())
        .filter_map(|node| {
            node.node()
                .label()
                .map(|label| (node.node().role(), label.to_owned()))
        })
        .collect();
    let mut mounted = Mounted::default();
    ui.operate(renderer, &mut mounted);
    (ui.into_cache(), labels, mounted.0)
}

#[test]
fn skill_headers_name_prepared_exact_target_and_not_internal_arguments() {
    let input = r#"{"skill":" repo/review λ ","args":"Internal instructions stay in Input"}"#;
    let theme = crate::settings::EditorSyntaxTheme::Auto;
    let mut renderer = crate::app::tests::headless_renderer();
    for (status, name, failure) in [
        (
            chat::ToolStatus::Running,
            "Using skill repo/review λ",
            false,
        ),
        (
            chat::ToolStatus::Succeeded,
            "Used skill repo/review λ",
            false,
        ),
        (chat::ToolStatus::Failed, "Failed skill repo/review λ", true),
        (chat::ToolStatus::Denied, "Denied skill repo/review λ", true),
        (
            chat::ToolStatus::Stopped,
            "Stopped skill repo/review λ",
            true,
        ),
    ] {
        let item = tool("Skill", input, status);
        let (runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
        let mut cached = CachedItem::updated(&item.content, theme, None, false, false, None);
        assert_eq!(cached.preview.text, "repo/review λ");
        assert!(cached.tool_input.is_none());
        assert!(cached.tool_output.is_none());
        for open in [false, true] {
            cached = CachedItem::updated(&item.content, theme, Some(cached), open, false, None);
            for width in [260.0, 900.0] {
                let (_, labels, text) = inspect(
                    runtime.step_header(&item, &cached, open),
                    width,
                    Cache::new(),
                    &mut renderer,
                );
                assert_eq!(
                    labels
                        .iter()
                        .filter(|(role, label)| *role == Role::Button && label == name)
                        .count(),
                    1,
                    "the disclosure names its target and outcome at {width}px"
                );
                assert!(
                    labels
                        .iter()
                        .all(|(_, label)| !label.contains("Internal instructions"))
                );
                assert!(text.iter().any(|text| text == "Called"));
                assert!(text.iter().any(|text| text == "Skill"));
                assert!(
                    !text
                        .iter()
                        .any(|text| text.contains("Internal instructions"))
                );
                assert!(
                    !text
                        .iter()
                        .any(|text| text.contains("Complete provider output"))
                );
                assert_eq!(text.iter().any(|text| text == "Permission denied"), failure);
            }
        }
        let ItemContent::Tool { output, .. } = &item.content else {
            unreachable!()
        };
        assert_eq!(cached.tool_input.as_ref().unwrap().text.as_ref(), input);
        assert_eq!(
            cached.tool_output.as_ref().unwrap().text.as_ref(),
            output.as_str()
        );
        assert_eq!(
            copy_text(CopyPart::Input, &item.content).as_deref(),
            Some(input)
        );
        assert_eq!(
            copy_text(CopyPart::Output, &item.content).as_deref(),
            Some(output.as_str())
        );
        assert_eq!(
            copy_text(CopyPart::Text, &item.content),
            Some(format!("{input}\n\n{output}"))
        );
        let (_, _, text) = inspect(
            runtime.step_detail(
                &item,
                &cached,
                design::markdown_settings(&cosmic::theme::active(), design::TEXT_PROSE),
            ),
            260.0,
            Cache::new(),
            &mut renderer,
        );
        assert!(text.iter().any(|text| text == "Input"));
        assert!(text.iter().any(|text| text == "Output"));
    }
}

#[test]
fn reused_skill_previews_refresh_accessible_target_status_and_tool_context() {
    let first = r#"{"skill":"review","args":"Retain complete instructions"}"#;
    let second = r#"{"skill":"audit","args":"Retain complete instructions"}"#;
    let source = tool("Skill", first, chat::ToolStatus::Running);
    let (runtime, _handle) = frame_cost::live_runtime(&[source]);
    let theme = crate::settings::EditorSyntaxTheme::Auto;
    let mut renderer = crate::app::tests::headless_renderer();
    let mut widgets = Cache::new();
    let mut previous: Option<CachedItem> = None;
    let mut previous_input = None;
    for (tool_name, input, status, name) in [
        (
            "Skill",
            first,
            chat::ToolStatus::Running,
            "Using skill review",
        ),
        (
            "Skill",
            first,
            chat::ToolStatus::Failed,
            "Failed skill review",
        ),
        (
            "Skill",
            second,
            chat::ToolStatus::Denied,
            "Denied skill audit",
        ),
        // The visual preview stays identical across these tool-kind changes.
        // Reusing it must neither retain nor lose skill-specific context.
        ("Other", second, chat::ToolStatus::Succeeded, "Called Other"),
        (
            "skill",
            second,
            chat::ToolStatus::Succeeded,
            "Used skill audit",
        ),
        (
            "skill",
            second,
            chat::ToolStatus::Stopped,
            "Stopped skill audit",
        ),
    ] {
        let item = tool(tool_name, input, status);
        let retained_input = previous
            .as_ref()
            .map(|cached| cached.tool_input.as_ref().unwrap().text.clone());
        let cached = CachedItem::updated(&item.content, theme, previous.take(), true, false, None);
        if previous_input == Some(input) {
            assert!(Arc::ptr_eq(
                &retained_input.unwrap(),
                &cached.tool_input.as_ref().unwrap().text
            ));
        }
        let (next, labels, _) = inspect(
            runtime.step_header(&item, &cached, false),
            260.0,
            widgets,
            &mut renderer,
        );
        assert_eq!(
            labels
                .iter()
                .filter(|(role, label)| *role == Role::Button && label == name)
                .count(),
            1
        );
        assert_eq!(
            copy_text(CopyPart::Input, &item.content).as_deref(),
            Some(input)
        );
        widgets = next;
        previous = Some(cached);
        previous_input = Some(input);
    }
}

#[test]
fn skill_targets_remain_bounded_without_changing_visual_preview_or_copy() {
    let theme = crate::settings::EditorSyntaxTheme::Auto;
    let limit = "λ".repeat(120);
    let oversized = serde_json::json!({
        "skill": "review",
        "args": "x".repeat(CachedItem::HIGHLIGHT_LIMIT),
    })
    .to_string();
    for (tool_name, input, target, visual) in [
        (
            "Skill",
            serde_json::json!({"skill": &limit}).to_string(),
            Some(limit.as_str()),
            limit.as_str(),
        ),
        (
            "Skill",
            serde_json::json!({"skill": "λ".repeat(121)}).to_string(),
            None,
            "1 input field",
        ),
        (
            "Skill",
            r#"{"skill":"first\nsecond"}"#.into(),
            None,
            "1 input field",
        ),
        ("Skill", r#"{"skill":7}"#.into(), None, "1 input field"),
        ("Skill", r#"{"skill":"   "}"#.into(), None, "1 input field"),
        (
            "Skill",
            r#"{"skill":"unfinished"#.into(),
            None,
            "Input available",
        ),
        ("Skill", oversized, None, "Input available"),
        ("Other", r#"{"skill":"review"}"#.into(), None, "review"),
        (
            "Skill",
            r#"{"path":"existing visual target","skill":"review"}"#.into(),
            Some("review"),
            "existing visual target",
        ),
    ] {
        let item = tool(tool_name, &input, chat::ToolStatus::Succeeded);
        let cached = CachedItem::updated(&item.content, theme, None, false, false, None);
        assert_eq!(cached.preview.skill.as_deref(), target);
        assert_eq!(cached.preview.text, visual);
        assert_eq!(
            copy_text(CopyPart::Input, &item.content).as_deref(),
            Some(input.as_str())
        );
        assert!(cached.tool_input.is_none());
        assert!(cached.tool_output.is_none());
    }
}
