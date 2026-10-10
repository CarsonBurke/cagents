//! App-owned browser semantics. Native webpage pixels are not a DOM bridge.
use super::*;
use crate::capability::BrowserCapability;
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use cosmic::iced::{Event, Rectangle, Size, Vector, mouse};
use iced_accessibility::{
    A11yNode, A11yTree,
    accesskit::{Node, Rect, Role},
};

/// Page-controlled titles can contain a credential URL or arbitrary secret
/// text. Bounding or attempting to recognize URL-shaped substrings does not
/// establish a safe presentation boundary. Preserve them in navigation state,
/// but use a fixed host title and SanitizedLocation in application chrome.
pub(super) const TITLE: &str = "Browser";
pub(super) fn presentation_title<'a>(kind: &CardKind, title: &'a str) -> &'a str {
    if matches!(kind, CardKind::Browser(_)) {
        TITLE
    } else {
        title
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LoadState {
    Loading,
    Ready,
    Failed,
    Unavailable,
    Suspended,
}
impl LoadState {
    fn label(self) -> &'static str {
        match self {
            Self::Loading => "Loading",
            Self::Ready => "Ready",
            Self::Failed => "Failed",
            Self::Unavailable => "Unavailable",
            Self::Suspended => "Suspended",
        }
    }
}

struct HostIdentity(widget::Id);

struct Host<'a, Message> {
    id: widget::Id,
    description: String,
    content: Element<'a, Message>,
}

pub(super) fn surface<'a, Message: 'a>(
    card: CardId,
    location: &SanitizedLocation,
    capability: BrowserCapability,
    load: LoadState,
    focused: bool,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    Element::new(Host {
        id: card_control_id(card, "browser.host"),
        description: format!(
            "Browser card {card}. Location: {}. {}. {}. {}. Embedded webpage accessibility is unavailable. Use Open in system browser to access the webpage in your browser.",
            location.as_str(),
            match capability {
                BrowserCapability::Integrated => "Integrated browser",
                BrowserCapability::ExternalOnly => "System browser only",
            },
            load.label(),
            if focused { "Focused" } else { "Not focused" },
        ),
        content,
    })
}

// This is a transparent semantic wrapper, with the child's exact layout. It
// neither owns keyboard focus nor receives native browser input independently.
impl<Message> Widget<Message, cosmic::Theme, cosmic::Renderer> for Host<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<HostIdentity>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(HostIdentity(self.id.clone()))
    }
    fn id(&self) -> Option<widget::Id> {
        Some(self.id.clone())
    }
    fn set_id(&mut self, id: widget::Id) {
        self.id = id;
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        let identity = tree.state.downcast_mut::<HostIdentity>();
        if !iced_accessibility::IdEq::eq(&identity.0, &self.id) {
            identity.0 = self.id.clone();
        }
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
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
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, cosmic::Theme, cosmic::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            layout,
            renderer,
            destinations,
        );
    }
    fn a11y_nodes(&self, layout: Layout<'_>, tree: &Tree, cursor: mouse::Cursor) -> A11yTree {
        let bounds = layout.bounds();
        let mut node = Node::new(Role::Group);
        node.set_label("Browser host");
        node.set_description(self.description.clone());
        node.set_bounds(Rect::new(
            f64::from(bounds.x),
            f64::from(bounds.y),
            f64::from(bounds.x + bounds.width),
            f64::from(bounds.y + bounds.height),
        ));
        A11yTree::node_with_child_tree(
            A11yNode::new(node, tree.state.downcast_ref::<HostIdentity>().0.clone()),
            self.content
                .as_widget()
                .a11y_nodes(layout, &tree.children[0], cursor),
        )
    }
}

impl Cagents {
    pub(super) fn browser_host_load(&self, card: CardId) -> LoadState {
        if BrowserCapability::current() == BrowserCapability::ExternalOnly {
            return LoadState::Unavailable;
        }
        match self.cards.get(&card) {
            Some(CardRuntime::Failed(_)) => LoadState::Failed,
            Some(CardRuntime::Loading) | None => LoadState::Loading,
            #[cfg(feature = "integrated-browser")]
            Some(CardRuntime::Browser(browser)) => match browser.lifecycle {
                BrowserLifecycle::Restoring => LoadState::Loading,
                BrowserLifecycle::Hibernating | BrowserLifecycle::Hibernated => {
                    LoadState::Suspended
                }
                BrowserLifecycle::Closed => LoadState::Unavailable,
                _ if browser.load_state.is_some() => LoadState::Loading,
                _ => LoadState::Ready,
            },
            _ => LoadState::Unavailable,
        }
    }

    /// Resolve at activation, never from a URL captured by a retained widget.
    pub(super) fn browser_system_request(
        &self,
        card: CardId,
    ) -> Option<(crate::model::BrowserId, Url)> {
        let state = self
            .card_state(card)
            .filter(|state| state.existence.is_open())?;
        let CardKind::Browser(session) = &state.kind else {
            return None;
        };
        Some((session.id, session.current_url.clone()))
    }
    pub(super) fn open_browser_in_system(&self, card: CardId) -> Task<Message> {
        let Some((browser, url)) = self.browser_system_request(card) else {
            return Task::none();
        };
        cosmic::task::future(async move {
            // The explicit action needs the complete navigation URL. Neither
            // the URL nor an opener's backend error enters a public property.
            Message::BrowserSystemOpened {
                card,
                browser,
                opened: open_system_url(url).await.is_ok(),
            }
        })
    }
    pub(super) fn browser_system_opened(
        &mut self,
        card: CardId,
        browser: crate::model::BrowserId,
        opened: bool,
    ) {
        if !self
            .browser_system_request(card)
            .is_some_and(|(current, _)| current == browser)
        {
            return;
        }
        self.status = Some(if opened {
            TransientNotice::success(format!("Browser card {card}: opened in the system browser"))
        } else {
            TransientNotice::error(format!(
                "Browser card {card}: could not open in the system browser"
            ))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_controls::accessibility_probe::{Probe, headless_renderer};
    use cosmic::iced::advanced::clipboard;
    use iced_accessibility::{
        IdEq,
        accesskit::{Action, ActionRequest},
    };

    fn app(raw: &str) -> (Cagents, CardId, crate::model::BrowserId) {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let browser = crate::model::BrowserId::new();
        let url = Url::parse(raw).unwrap();
        app.card_state_mut(card).unwrap().kind = CardKind::Browser(crate::model::BrowserSession {
            id: browser,
            current_url: url.clone(),
            history: vec![url],
            history_index: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            fidelity: crate::model::BrowserRestoreFidelity::HistoryAndScroll,
        });
        app.card_state_mut(card).unwrap().title = raw.into();
        (app, card, browser)
    }

    #[test]
    fn stable_host_forwards_retained_children_and_safe_states_without_owning_focus() {
        let card = CardId::new();
        let location = SanitizedLocation::from(
            &Url::parse("https://alice:password@example.test/path?oauth=secret#token").unwrap(),
        );
        let renderer = headless_renderer();
        let mut retained = None;
        let mut child_id = None;
        let mut host_id: Option<iced_accessibility::A11yId> = None;
        for (capability, state, focused) in [
            (BrowserCapability::Integrated, LoadState::Loading, true),
            (BrowserCapability::Integrated, LoadState::Ready, true),
            (BrowserCapability::Integrated, LoadState::Failed, true),
            (BrowserCapability::Integrated, LoadState::Suspended, false),
            (
                BrowserCapability::ExternalOnly,
                LoadState::Unavailable,
                false,
            ),
        ] {
            let mut element: Element<'_, ()> = surface(
                card,
                &location,
                capability,
                state,
                focused,
                Element::new(Probe),
            );
            let tree = retained.get_or_insert_with(|| Tree::new(&element));
            tree.diff(element.as_widget_mut());
            let node = element.as_widget_mut().layout(
                tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(400.0, 200.0)),
            );
            let layout = Layout::new(&node).with_virtual_offset(Vector::new(0.0, 37.0));
            let accessible =
                element
                    .as_widget()
                    .a11y_nodes(layout, tree, mouse::Cursor::Unavailable);
            assert_eq!(accessible.root().len(), 1);
            let host = &accessible.root()[0];
            assert_eq!(host.node().role(), Role::Group);
            assert_eq!(host.node().label(), Some("Browser host"));
            let description = host.node().description().unwrap();
            assert!(description.contains(state.label()));
            assert!(description.contains(&card.to_string()));
            assert!(description.contains("https://example.test/path"));
            assert!(description.contains("Embedded webpage accessibility is unavailable"));
            assert!(!host.node().supports_action(Action::Focus));
            assert!(!host.node().supports_action(Action::Click));
            for forbidden in ["alice", "password", "oauth", "secret", "token", "?", "#"] {
                assert!(!format!("{accessible:?}").contains(forbidden));
            }
            let child = accessible
                .children()
                .iter()
                .find(|n| n.node().label() == Some("Retained child"))
                .unwrap();
            if let Some(id) = &child_id {
                assert!(IdEq::eq(child.id(), id));
            } else {
                child_id = Some(child.id().clone());
            }
            if let Some(id) = &host_id {
                assert_eq!(
                    iced_accessibility::accesskit::NodeId::from(host.id().clone()),
                    iced_accessibility::accesskit::NodeId::from(id.clone())
                );
            } else {
                host_id = Some(host.id().clone());
            }
            assert!(host.node().children().contains(&child.id().clone().into()));
        }
        // No draw required: this exercises the actual retained semantic tree.
    }

    #[test]
    fn replacing_a_browser_card_in_a_retained_slot_changes_host_identity() {
        let first = CardId::new();
        let second = CardId::new();
        let location = SanitizedLocation::from(&Url::parse("https://example.test/").unwrap());
        let make = |card| {
            surface::<()>(
                card,
                &location,
                BrowserCapability::Integrated,
                LoadState::Loading,
                false,
                iced::widget::Space::new().into(),
            )
        };
        let mut element = make(first);
        let mut tree = Tree::new(&element);
        let node = layout::Node::new(Size::new(400.0, 200.0));
        let accessible =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let old_id: iced_accessibility::accesskit::NodeId =
            accessible.root()[0].id().clone().into();
        element = make(second);
        tree.diff(element.as_widget_mut());
        let accessible =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let new_id: iced_accessibility::accesskit::NodeId =
            accessible.root()[0].id().clone().into();
        assert_ne!(old_id, new_id);
        let description = accessible.root()[0].node().description().unwrap();
        assert!(description.contains(&second.to_string()));
        assert!(!description.contains(&first.to_string()));
    }

    #[derive(Default)]
    struct FocusCount {
        total: usize,
        focused: usize,
    }
    impl Operation for FocusCount {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn focusable(
            &mut self,
            _: Option<&widget::Id>,
            _: Rectangle,
            state: &mut dyn cosmic::iced::advanced::widget::operation::Focusable,
        ) {
            self.total += 1;
            self.focused += usize::from(state.is_focused());
        }
    }

    #[test]
    fn load_and_capability_updates_preserve_child_keyboard_focus() {
        let card = CardId::new();
        let location = SanitizedLocation::from(&Url::parse("https://example.test/").unwrap());
        let id = card_control_id(card, "browser.system");
        let renderer = headless_renderer();
        let make = |capability, load| {
            surface(
                card,
                &location,
                capability,
                load,
                true,
                crate::ui_controls::button::secondary("Open in system browser")
                    .id(id.clone())
                    .on_press(())
                    .into(),
            )
        };
        let mut element = make(BrowserCapability::Integrated, LoadState::Loading);
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 200.0));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut cosmic::iced::advanced::widget::operation::focusable::focus::<()>(id.clone()),
        );
        for (capability, load) in [
            (BrowserCapability::Integrated, LoadState::Ready),
            (BrowserCapability::Integrated, LoadState::Failed),
            (BrowserCapability::ExternalOnly, LoadState::Unavailable),
        ] {
            element = make(capability, load);
            tree.diff(element.as_widget_mut());
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut count = FocusCount::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut count);
            assert_eq!((count.total, count.focused), (1, 1));
        }
    }

    #[test]
    fn actual_card_trees_contain_one_host_and_never_echo_page_titles_or_failure_text() {
        let raw = "https://alice:password@example.test/page?oauth=secret#token";
        let (mut app, card, _) = app(raw);
        app.refresh_ui.focused = true;
        drop(app.sync_chrome());
        let renderer = headless_renderer();
        let mut retained = None;
        let mut id = None;
        for runtime in [
            CardRuntime::Loading,
            CardRuntime::Failed(format!("Backend failed at {raw}").into()),
        ] {
            app.cards.insert(card, runtime);
            let mut element = app.card_view(card, true, true);
            let tree = retained.get_or_insert_with(|| Tree::new(&element));
            tree.diff(element.as_widget_mut());
            let node = element.as_widget_mut().layout(
                tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(900.0, 600.0)),
            );
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&node),
                tree,
                mouse::Cursor::Unavailable,
            );
            let nodes: Vec<_> = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .collect();
            let hosts: Vec<_> = nodes
                .iter()
                .filter(|n| n.node().label() == Some("Browser host"))
                .collect();
            assert_eq!(hosts.len(), 1);
            if let Some(previous) = &id {
                assert!(IdEq::eq(hosts[0].id(), previous));
            } else {
                id = Some(hosts[0].id().clone());
            }
            assert!(nodes.iter().any(|n| n.node().role() == Role::Button
                && n.node().label() == Some("Open in system browser")));
            for forbidden in [
                "alice",
                "password",
                "oauth",
                "secret",
                "token",
                "Backend failed",
            ] {
                assert!(!format!("{accessible:?}").contains(forbidden));
            }
        }
    }

    #[test]
    fn opaque_payloads_never_enter_host_properties() {
        let card = CardId::new();
        for raw in [
            "data:text/html,secret-token",
            "javascript:secret-token",
            "file:///home/secret-token/private",
            "about:secret-token",
        ] {
            let location = SanitizedLocation::from(&Url::parse(raw).unwrap());
            let element: Element<'_, ()> = surface(
                card,
                &location,
                BrowserCapability::Integrated,
                LoadState::Ready,
                false,
                iced::widget::Space::new().into(),
            );
            let tree = Tree::new(&element);
            let node = layout::Node::new(Size::new(400.0, 200.0));
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                mouse::Cursor::Unavailable,
            );
            assert!(!format!("{accessible:?}").contains("secret-token"));
        }
    }

    #[test]
    fn a_real_fallback_button_action_crosses_the_host_wrapper() {
        let card = CardId::new();
        let location = SanitizedLocation::from(&Url::parse("https://example.test/").unwrap());
        let id = card_control_id(card, "browser.system");
        let mut element: Element<'_, CardId> = surface(
            card,
            &location,
            BrowserCapability::Integrated,
            LoadState::Ready,
            true,
            crate::ui_controls::button::secondary("Open in system browser")
                .id(id.clone())
                .on_press(card)
                .into(),
        );
        let mut tree = Tree::new(&element);
        let renderer = headless_renderer();
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(400.0, 200.0)),
        );
        let layout = Layout::new(&node);
        let accessible = element
            .as_widget()
            .a11y_nodes(layout, &tree, mouse::Cursor::Unavailable);
        let button = accessible
            .children()
            .iter()
            .find(|n| n.node().role() == Role::Button)
            .unwrap();
        assert_eq!(button.node().label(), Some("Open in system browser"));
        assert!(button.node().supports_action(Action::Click));
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &Event::A11y(
                id.into(),
                ActionRequest {
                    action: Action::Click,
                    target: button.id().clone().into(),
                    data: None,
                },
            ),
            layout,
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(Size::new(400.0, 200.0)),
        );
        assert_eq!(messages, vec![card]);
    }

    #[test]
    fn activation_reads_the_latest_descriptor_and_completion_preserves_focus() {
        let (mut app, card, browser) =
            app("https://alice:password@example.test/old?secret=one#token");
        let newer = Url::parse("https://bob:new-password@example.test/new?secret=two#second-token")
            .unwrap();
        if let CardKind::Browser(session) = &mut app.card_state_mut(card).unwrap().kind {
            session.current_url = newer.clone();
        }
        assert_eq!(app.browser_system_request(card), Some((browser, newer)));
        let focus = app.workspace.focused();
        let saved = app.durable.focused;
        for opened in [true, false] {
            app.browser_system_opened(card, browser, opened);
            assert_eq!(app.workspace.focused(), focus);
            assert_eq!(app.durable.focused, saved);
            let message = &app.status.as_ref().unwrap().message;
            assert!(message.contains(&card.to_string()));
            for forbidden in ["password", "secret", "token", "https", "bob"] {
                assert!(!message.contains(forbidden));
            }
        }
        let previous = app.status.clone();
        app.browser_system_opened(card, crate::model::BrowserId::new(), false);
        assert_eq!(app.status, previous);
        app.card_state_mut(card).unwrap().existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: None,
            },
        };
        assert!(app.browser_system_request(card).is_none());
        app.browser_system_opened(card, browser, false);
        assert_eq!(app.status, previous);
    }

    #[test]
    fn browser_page_titles_are_private_to_state_in_every_chrome_presenter() {
        let raw = "https://alice:password@example.test/page?secret=oauth#token";
        let (mut app, card, _) = app(raw);
        assert_eq!(app.card_display_title(app.card_state(card).unwrap()), TITLE);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        assert_eq!(app.workspace.card(card).unwrap().accessible_name, TITLE);
        drop(app.sync_chrome());
        let chrome = &app.card_chrome[&card];
        for presented in [
            &chrome.title,
            &chrome.tooltip,
            &chrome.focused_name,
            &chrome.unfocused_name,
            &chrome.close_name,
            &chrome.resize_name,
        ] {
            for forbidden in ["alice", "password", "secret", "oauth", "token"] {
                assert!(!presented.contains(forbidden));
            }
        }
        assert_eq!(app.card_state(card).unwrap().title, raw);
        assert_eq!(
            presentation_title(&app.card_state(card).unwrap().kind, raw),
            TITLE
        );
        app.chrome_ui
            .user_titles
            .insert(card, "Documentation".into());
        assert_eq!(
            app.card_display_title(app.card_state(card).unwrap()),
            "Documentation"
        );
        drop(app.sync_chrome());
        assert_eq!(app.card_chrome[&card].title, "Documentation");
        assert_eq!(
            app.workspace.card(card).unwrap().accessible_name,
            "Documentation"
        );
        assert_eq!(app.card_state(card).unwrap().title, raw);
        assert!(user_workspace_message_blocked_while_modal(
            &Message::OpenBrowserInSystem(card)
        ));
    }
}
