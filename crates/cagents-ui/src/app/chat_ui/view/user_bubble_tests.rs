//! Where a user's message bubble is drawn: on the right of the transcript,
//! whatever its length, whether it folds, and whether it is folded.

use super::*;
use iced::advanced::{
    Layout, layout,
    widget::{Operation, Tree, operation::Focusable},
};
use iced::{Rectangle, Size};

/// A widely and a narrowly drawn transcript: one with room beside the bubble,
/// and one narrower than the bubble's measure.
const WIDTHS: [f32; 2] = [1000.0, 360.0];

/// Long markdown, as people paste it: a quoted paragraph, a heading and a
/// code fence, over more lines than a message shows before it folds.
fn markdown(lines: usize) -> String {
    let mut text = String::from(
        "> The settings page re-renders three times on load, once for each provider.\n\n\
         ## What I want\n\n\
         ```ts\n",
    );
    for line in 0..lines {
        text.push_str(&format!(
            "const value{line} = useSetting(\"flag-{line}\");\n"
        ));
    }
    text.push_str("```\n");
    text
}

fn prompt(text: String) -> chat::Item {
    chat::Item {
        id: "prompt".into(),
        role: chat::Role::User,
        content: ItemContent::Text {
            text,
            thinking: false,
            phase: chat::Phase::Final,
        },
        started_at: None,
        at: None,
    }
}

/// Where the message's text and its "Show more" or "Show less" row are drawn.
#[derive(Debug)]
struct Drawn {
    text: Rectangle,
    disclosure: Option<Rectangle>,
}

fn draw(runtime: &Runtime, width: f32) -> Drawn {
    #[derive(Default)]
    struct Found {
        text: Vec<Rectangle>,
        controls: Vec<Rectangle>,
    }
    impl Operation for Found {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn custom(
            &mut self,
            _: Option<&widget::Id>,
            bounds: Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if state.is::<selectable_prose::State>() {
                self.text.push(bounds);
            }
        }
        fn focusable(&mut self, _: Option<&widget::Id>, bounds: Rectangle, _: &mut dyn Focusable) {
            self.controls.push(bounds);
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let item = runtime
        .active_item("prompt")
        .expect("the prompt is in the transcript");
    let mut element = runtime.prompt_view(item);
    let mut tree = Tree::new(element.as_widget());
    let limits = layout::Limits::new(Size::ZERO, Size::new(width, 8000.0));
    // Text over the flow threshold is shaped off the first layout; the second
    // draws what that shaped.
    let _ = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    for cached in runtime.cache.values() {
        cached.flow_cache.complete_fixture();
    }
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut found = Found::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut found);
    let [text] = found.text[..] else {
        panic!("one text block, found {:?}", found.text);
    };
    // The copy action is a compact icon; the disclosure row spans the bubble.
    let disclosure = found
        .controls
        .iter()
        .copied()
        .find(|bounds| bounds.width > 4.0 * f32::from(design::COMPACT_ICON_SIZE));
    Drawn { text, disclosure }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.5
}

#[test]
fn every_user_message_is_anchored_on_the_right_at_any_length_and_fold() {
    let cases = [
        (
            "short",
            "Please make the settings page stop flickering.".to_owned(),
            None,
        ),
        ("long, folded", markdown(24), Some(false)),
        ("long, unfolded", markdown(24), Some(true)),
        // Past 2048 characters the shown text is drawn from a shaped flow.
        ("very long, folded", markdown(160), Some(false)),
        ("very long, unfolded", markdown(160), Some(true)),
    ];
    for (name, text, unfolded) in cases {
        let (mut runtime, _handle) = frame_cost::live_runtime(&[prompt(text)]);
        if unfolded == Some(true) {
            runtime.expanded.insert("prompt".into());
        }
        for width in WIDTHS {
            let Drawn { text, disclosure } = draw(&runtime, width);
            let case = format!("{name} at {width}");
            let bubble = width.min(PROMPT_MEASURE);
            let inner_left = width - bubble + step::GAP_M;
            match unfolded {
                None => {
                    assert!(
                        disclosure.is_none(),
                        "{case}: a short message does not fold"
                    );
                    // The bubble's right edge is the transcript's; its text
                    // keeps the bubble's inset from it.
                    assert!(
                        close(text.x + text.width, width - step::GAP_M),
                        "{case}: text ends at {} in {text:?}",
                        text.x + text.width
                    );
                    // A short message sizes to its text, so it starts no
                    // further left than a full-measure bubble's text.
                    assert!(text.x >= inner_left - 0.5, "{case}: {text:?}");
                    if width > PROMPT_MEASURE {
                        assert!(text.width < bubble - 2.0 * step::GAP_M, "{case}: {text:?}");
                    }
                }
                Some(_) => {
                    // A message that folds takes the bubble's whole measure,
                    // folded or not, so its left edge stays put as it
                    // unfolds; its disclosure row spans the bubble, inset
                    // from the transcript's right edge.
                    let disclosure =
                        disclosure.unwrap_or_else(|| panic!("{case}: Show more or less is drawn"));
                    assert!(close(text.x, inner_left), "{case}: text at {text:?}");
                    assert!(
                        close(disclosure.x, inner_left),
                        "{case}: row at {disclosure:?}"
                    );
                    assert!(
                        close(disclosure.x + disclosure.width, width - step::GAP_M),
                        "{case}: row at {disclosure:?}"
                    );
                }
            }
        }
    }
}
