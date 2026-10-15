//! Shared headless widget contract checks; no provider or filesystem work.
use super::*;
use iced::advanced::{
    Layout, Shell, clipboard, layout,
    widget::{Operation, Tree},
};

#[derive(Default)]
pub(in crate::app) struct Rendered(pub Vec<(String, iced::Rectangle)>);
impl Operation for Rendered {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }
    fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
        if bounds.width > 0.0 && bounds.height > 0.0 {
            self.0.push((text.to_owned(), bounds));
        }
    }
}

pub(in crate::app) fn inspect(
    mut element: Element<'_, Message>,
    click: Option<&str>,
) -> (Rendered, Vec<Message>) {
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(element.as_widget());
    let viewport = iced::Rectangle::with_size(iced::Size::new(1100.0, 4000.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, viewport.size()),
    );
    let mut rendered = Rendered::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut rendered);
    let mut messages = Vec::new();
    if let Some(label) = click {
        let bounds = rendered
            .0
            .iter()
            .find(|(text, _)| text == label)
            .unwrap_or_else(|| panic!("Expected visible control {label:?}; text={:?}", rendered.0))
            .1;
        let pointer = bounds.center();
        for event in [
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                iced::mouse::Cursor::Available(pointer),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
    }
    (rendered, messages)
}

impl State {
    pub(in crate::app) fn set_fixture_login_for_test(&mut self, id: &AccountId, email: &str) {
        let account = self.account_mut(id).expect("fixture account exists");
        account.identity = Some(Identity {
            email: Some(email.into()),
            ..Default::default()
        });
        account.snapshot = Some(UsageSnapshot::state(UsageStatus::Ok, "", now_ms()));
    }
}
