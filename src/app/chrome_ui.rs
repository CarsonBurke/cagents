use super::*;
use crate::interaction::tooltip;

#[derive(Debug, Default)]
pub(super) struct State {
    popup: Option<Popup>,
    focus_index: usize,
    /// Last reported horizontal offset of the tab strip.
    ///
    /// The native scrollable only accepts absolute offsets, so revealing a tab
    /// without knowing where the strip already sits means every reveal snaps
    /// the tab to the viewport edge - including a reveal triggered by merely
    /// hovering an already-visible tab under focus-follows-mouse.
    tabs_offset: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_activation_leaves_enter_for_the_input_surface() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Rectangle, Size};

        let app = super::super::tests::context_inspector_test_app(true);
        let renderer = super::super::tests::headless_renderer();
        let card = app.workspace.focused().unwrap();
        assert!(matches!(
            app.card_state(card).unwrap().kind,
            CardKind::Terminal(_)
        ));
        for (target, title_activated) in [
            (workspace_widget::card_focus_id(card), false),
            (card_control_id(card, "title"), true),
        ] {
            let mut header = app.column_header(card, true, 320.0);
            let mut tree = Tree::new(header.as_widget());
            let viewport = Rectangle::with_size(Size::new(320.0, 40.0));
            let node = header.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            header.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut iced::advanced::widget::operation::focusable::focus::<()>(target),
            );
            let event = Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            });
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            header.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut shell,
                &viewport,
            );
            assert_eq!(shell.is_event_captured(), title_activated);
            drop(shell);
            assert_eq!(
                messages
                    .iter()
                    .any(|message| matches!(message, Message::FocusCard(id) if *id == card)),
                title_activated,
            );
        }
    }

    #[test]
    fn chat_column_title_does_not_adopt_the_terminal_focus_id() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Rectangle, Size};

        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let state = Arc::make_mut(&mut app.durable)
            .cards
            .iter_mut()
            .find(|state| state.id == card)
            .unwrap();
        let CardKind::Terminal(terminal) = &mut state.kind else {
            unreachable!();
        };
        terminal.launch.program = "codex".into();
        let session = chat_ui::conversion(terminal).expect("codex terminal converts");
        state.kind = CardKind::Chat(session);

        let renderer = super::super::tests::headless_renderer();
        for (target, title_activated) in [
            (workspace_widget::card_focus_id(card), false),
            (card_control_id(card, "title"), true),
        ] {
            let mut header = app.column_header(card, true, 320.0);
            let mut tree = Tree::new(header.as_widget());
            let viewport = Rectangle::with_size(Size::new(320.0, 40.0));
            let node = header.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            header.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut iced::advanced::widget::operation::focusable::focus::<()>(target.clone()),
            );
            let event = Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            });
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            header.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut shell,
                &viewport,
            );
            assert_eq!(
                shell.is_event_captured(),
                title_activated,
                "focus target {target:?} should activate the title: {title_activated}"
            );
        }
    }
}

impl State {
    pub(super) fn is_open(&self) -> bool {
        self.popup.is_some()
    }

    pub(super) fn close(&mut self) {
        self.popup = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Popup {
    NewTab,
    Notices,
}

#[derive(Debug, Clone, Copy)]
pub enum NewTab {
    Terminal,
    Thread,
    Browser,
}

// Threads choose their model in the composer; backend details are automatic.
const NEW_TAB_CHOICES: [(&str, &str, NewTab, &str); 3] = [
    (
        "New thread",
        "Choose a model and start a conversation",
        NewTab::Thread,
        "chrome.new-tab.thread",
    ),
    (
        "Terminal",
        "Shell in the current worktree",
        NewTab::Terminal,
        "chrome.new-tab.terminal",
    ),
    (
        "Browser",
        "Open a browser tab",
        NewTab::Browser,
        "chrome.new-tab.browser",
    ),
];

#[derive(Debug, Clone)]
pub enum Action {
    ToggleNewTab,
    ToggleNotices,
    Close,
    TabsScrolled(f32),
    Launch(NewTab),
}

fn browser_icon() -> widget::icon::Handle {
    static ICON: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(&br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><g fill="none" stroke="black" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><ellipse cx="12" cy="12" rx="4" ry="9"/><path d="M3 12h18M5 6.5h14M5 17.5h14"/></g></svg>"#[..]).symbolic(true)
    });
    ICON.clone()
}

fn chat_icon() -> widget::icon::Handle {
    static ICON: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(&br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><path fill="none" stroke="black" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" d="M5 4h14a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2h-9l-5 3v-3a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2Zm2 5h10M7 13h7"/></svg>"#[..]).symbolic(true)
    });
    ICON.clone()
}

// Tabs retain a neutral keyboard focus cue without an accent outline.
fn column_title_style() -> cosmic::theme::Button {
    fn appearance(focused: bool, theme: &cosmic::Theme) -> widget::button::Style {
        widget::button::Style {
            border_radius: theme.cosmic().corner_radii.radius_s.into(),
            background: focused
                .then(|| iced::Background::Color(theme.current_container().component.hover.into())),
            border_width: 0.0,
            icon_color: Some(design::muted_icon_color(theme)),
            ..widget::button::Style::new()
        }
    }
    cosmic::theme::Button::Custom {
        active: Box::new(appearance),
        disabled: Box::new(|_| widget::button::Style::new()),
        hovered: Box::new(|focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.hover.into(),
            )),
            ..appearance(focused, theme)
        }),
        pressed: Box::new(|focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.pressed.into(),
            )),
            ..appearance(focused, theme)
        }),
    }
}

fn command<'a>(
    icon: impl Into<widget::icon::Handle>,
    label: impl Into<Cow<'a, str>>,
    id: widget::Id,
    action: Option<Message>,
) -> Element<'a, Message> {
    crate::ui_controls::icon_control(icon.into(), label, false, action.is_some(), action)
        .id(id)
        .into()
}

impl Cagents {
    /// Keep Tab traversal and global shortcuts inside the open popup.
    pub(super) fn chrome_key_event(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        let popup = self.chrome_ui.popup?;
        if self.modal_is_open() {
            self.chrome_ui.close();
            return None;
        }
        if is_escape_press(event) {
            return Some(self.update_chrome(Action::Close));
        }
        let backwards = is_tab_press(event).or_else(|| match event {
            keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(key),
                modifiers,
                ..
            } if popup == Popup::NewTab && modifiers.is_empty() => match key {
                keyboard::key::Named::ArrowUp => Some(true),
                keyboard::key::Named::ArrowDown => Some(false),
                _ => None,
            },
            _ => None,
        });
        if let Some(backwards) = backwards {
            // The two popups have unrelated row counts, and padding the
            // shorter one to the longer one's length made the menu's length a
            // hidden constraint on unrelated code.
            let ids: Vec<&str> = match popup {
                Popup::NewTab => NEW_TAB_CHOICES.iter().map(|(_, _, _, id)| *id).collect(),
                Popup::Notices => {
                    let health = self.save_health.as_ref();
                    // In the order the notices draw them: the lane's own notice,
                    // then the refused writes. Only one of them offers a retry.
                    let acknowledge = health
                        .is_some_and(|health| {
                            matches!(health.health, SessionSaveHealth::Recovered { .. })
                        })
                        .then_some("session-save.acknowledge-recovery");
                    let retry = health
                        .is_some_and(SaveHealthState::offers_retry)
                        .then_some("session-save.retry");
                    [
                        Some("chrome.notices.close"),
                        (!self.notices.is_empty()).then_some("chrome.notices.dismiss"),
                        acknowledge,
                        retry,
                    ]
                    .into_iter()
                    .flatten()
                    .collect()
                }
            };
            let count = ids.len();
            self.chrome_ui.focus_index = if backwards {
                (self.chrome_ui.focus_index + count - 1) % count
            } else {
                (self.chrome_ui.focus_index + 1) % count
            };
            return Some(
                iced::widget::operation::focus(widget::Id::new(ids[self.chrome_ui.focus_index]))
                    .map(cosmic::Action::App),
            );
        }
        Some(Task::none())
    }

    pub(super) fn update_chrome(&mut self, action: Action) -> Task<Message> {
        if self.modal_is_open() && !matches!(action, Action::Close) {
            return Task::none();
        }
        match action {
            Action::ToggleNewTab | Action::ToggleNotices => {
                let popup = if matches!(action, Action::ToggleNewTab) {
                    Popup::NewTab
                } else {
                    Popup::Notices
                };
                if self.chrome_ui.popup == Some(popup) {
                    return self.update_chrome(Action::Close);
                }
                self.chrome_ui.popup = Some(popup);
                self.chrome_ui.focus_index = 0;
                iced::widget::operation::focus(widget::Id::new(match popup {
                    Popup::NewTab => NEW_TAB_CHOICES[0].3,
                    Popup::Notices => "chrome.notices.close",
                }))
                .map(cosmic::Action::App)
            }
            Action::Close => {
                let popup = self.chrome_ui.popup.take();
                popup.map_or_else(Task::none, |popup| {
                    iced::widget::operation::focus(widget::Id::new(match popup {
                        Popup::NewTab => TOOLBAR_FOCUS_ID,
                        Popup::Notices => "chrome.notices",
                    }))
                    .map(cosmic::Action::App)
                })
            }
            Action::Launch(kind) => {
                self.chrome_ui.close();
                match kind {
                    NewTab::Terminal => cosmic::task::message(Message::NewTerminal),
                    NewTab::Thread => self.new_native_thread(),
                    NewTab::Browser => cosmic::task::message(Message::NewBrowser),
                }
            }
            Action::TabsScrolled(offset) => {
                self.chrome_ui.tabs_offset = offset;
                Task::none()
            }
        }
    }

    fn new_tab_control(&self) -> Element<'_, Message> {
        let enabled = !self.modal_is_open() && !self.context_inspector_is_overlay();
        let control = command(
            crate::ui_controls::symbolic_icon("list-add-symbolic"),
            "New tab",
            widget::Id::new(TOOLBAR_FOCUS_ID),
            enabled.then_some(Message::ChromeUi(Action::ToggleNewTab)),
        );
        let content = (self.chrome_ui.popup == Some(Popup::NewTab) && enabled).then(|| {
            let items = NEW_TAB_CHOICES
                .into_iter()
                .map(|(label, description, kind, id)| {
                    let icon = match kind {
                        NewTab::Terminal => {
                            crate::ui_controls::symbolic_icon("utilities-terminal-symbolic")
                        }
                        NewTab::Thread => chat_icon(),
                        NewTab::Browser => browser_icon(),
                    };
                    crate::ui_controls::popover_item(
                        icon,
                        label,
                        description,
                        Message::ChromeUi(Action::Launch(kind)),
                    )
                    .id(widget::Id::new(id))
                    .name(format!("New {label} tab"))
                    .into()
                });
            widget::column::with_children(vec![
                widget::container(design::heading("New tab"))
                    .padding([6, 8])
                    .into(),
                crate::design::horizontal_rule().into(),
                widget::column::with_children(items).spacing(2).into(),
            ])
            .spacing(6)
            .width(292)
            .into()
        });
        crate::ui_controls::popover(
            control,
            content,
            widget::popover::Position::Bottom,
            Message::ChromeUi(Action::Close),
        )
    }

    fn has_chrome_notices(&self) -> bool {
        !self.notices.is_empty()
            || self.retained_update_notice()
            || self
                .save_health
                .as_ref()
                .is_some_and(SaveHealthState::is_visible)
    }

    pub(super) fn reconcile_notice_popup(&mut self) -> Task<Message> {
        if self.chrome_ui.popup == Some(Popup::Notices) && !self.has_chrome_notices() {
            self.chrome_ui.close();
            iced::widget::operation::focus(context_inspector_toggle_id()).map(cosmic::Action::App)
        } else {
            Task::none()
        }
    }

    fn notice_control(&self) -> Option<Element<'_, Message>> {
        if !self.has_chrome_notices() {
            return None;
        }
        // The anchor must carry the actual severity. Collapsing it to a binary
        // `error` flag rendered a completed pull request as an amber warning
        // triangle labelled "Application notices", and `NoticeSeverity::icon`
        // already owns the per-severity glyph.
        let health_severity = self.save_health.as_ref().and_then(|health| {
            (matches!(
                health.health,
                SessionSaveHealth::RetryExhausted { .. } | SessionSaveHealth::WriterUnavailable
            ) || !health.quarantined.is_empty())
            .then_some(NoticeSeverity::Error)
        });
        // From the log, not `self.status`: `update` drains that slot into the
        // log on every message, so reading it here would almost always see
        // `None` and fall back to a warning glyph for an error.
        let recorded_severity = self.notices.severity().max(health_severity);
        let update_only = recorded_severity.is_none() && self.retained_update_notice();
        let severity = recorded_severity.unwrap_or(NoticeSeverity::Warning);
        let mut control = crate::ui_controls::icon_control(
            crate::ui_controls::symbolic_icon(if update_only {
                "software-update-available-symbolic"
            } else {
                severity.icon()
            }),
            if update_only {
                "Application update available"
            } else {
                match severity {
                    NoticeSeverity::Success => "Application notices: success",
                    NoticeSeverity::Warning => "Application notices",
                    NoticeSeverity::Error => "Application errors",
                }
            },
            self.chrome_ui.popup == Some(Popup::Notices),
            !self.modal_is_open(),
            (!self.modal_is_open()).then_some(Message::ChromeUi(Action::ToggleNotices)),
        )
        .id(widget::Id::new("chrome.notices"));
        if !update_only {
            control = control.icon_color(match severity {
                NoticeSeverity::Success => cosmic::theme::active().cosmic().success.base.into(),
                NoticeSeverity::Warning => cosmic::theme::active().cosmic().warning.base.into(),
                NoticeSeverity::Error => cosmic::theme::active().cosmic().destructive.base.into(),
            });
        }
        let content =
            (self.chrome_ui.popup == Some(Popup::Notices) && !self.modal_is_open()).then(|| {
                let mut notices = Vec::with_capacity(self.notices.len() + 1);
                if let Some(update) = self.update_notice(true) {
                    notices.push(update);
                }
                for entry in self.notices.recent() {
                    let body: Element<'_, Message> = if entry.repeats > 1 {
                        widget::column::with_children(vec![
                            super::notice_body(&entry.notice.message),
                            super::notice_surface_detail(
                                entry.notice.severity,
                                format!("Repeated {} times", entry.repeats),
                            ),
                        ])
                        .spacing(design::GRID_UNIT)
                        .into()
                    } else {
                        super::notice_body(&entry.notice.message)
                    };
                    notices.push(super::notice_surface(
                        entry.notice.severity,
                        body,
                        Some(super::notice_surface_action(
                            entry.notice.severity,
                            "Dismiss",
                            "Dismiss application notice",
                            widget::Id::new(format!("chrome.notices.dismiss.{}", entry.id)),
                            Message::DismissNotice(entry.id),
                        )),
                        false,
                    ));
                }
                if let Some(health) = &self.save_health {
                    notices.extend(save_health_views(health, |card| {
                        self.card_state(card).map(|card| card.title.as_str())
                    }));
                }
                let mut heading_children = vec![
                    design::heading("Application notices")
                        .width(Length::Fill)
                        .into(),
                ];
                if !self.notices.is_empty() {
                    heading_children.push(
                        widget::button::text("Dismiss all")
                            .id(widget::Id::new("chrome.notices.dismiss"))
                            .name("Dismiss every application notice")
                            .on_press(Message::DismissAllNotices)
                            .into(),
                    );
                }
                heading_children.push(command(
                    crate::ui_controls::symbolic_icon("window-close-symbolic"),
                    "Close application notices",
                    widget::Id::new("chrome.notices.close"),
                    Some(Message::ChromeUi(Action::Close)),
                ));
                let heading = widget::row::with_children(heading_children)
                    .align_y(Alignment::Center)
                    .spacing(8);
                widget::container(
                    widget::column::with_children(vec![
                        heading.into(),
                        widget::scrollable(
                            widget::column::with_children(notices)
                                .spacing(12)
                                // Each notice ends in a Dismiss control on its
                                // right edge, which the floating thumb would
                                // otherwise cover.
                                .padding(design::with_vertical_rail(iced::Padding::ZERO)),
                        )
                        .direction(iced::widget::scrollable::Direction::Vertical(
                            design::scrollbar(),
                        ))
                        .height(Length::Shrink)
                        .into(),
                    ])
                    .spacing(8),
                )
                .padding(6)
                .width(408)
                .max_height(408)
                .into()
            });
        Some(crate::ui_controls::popover(
            control,
            content,
            widget::popover::Position::Bottom,
            Message::ChromeUi(Action::Close),
        ))
    }

    fn native_header<'a>(
        &'a self,
        start: Element<'a, Message>,
        end: Option<Element<'a, Message>>,
        window_controls: bool,
    ) -> Element<'a, Message> {
        let focused = self
            .core
            .focus_chain()
            .iter()
            .any(|id| Some(*id) == self.core.main_window_id());
        let mut header = widget::header_bar()
            .density(cosmic::cosmic_theme::Density::Compact)
            .focused(focused)
            .maximized(self.core.window.is_maximized)
            .sharp_corners(self.core.window.sharp_corners)
            .start(start)
            .on_drag(Message::NativeWindowAction(cosmic::app::Action::Drag))
            .on_double_click(Message::NativeWindowAction(cosmic::app::Action::Maximize))
            .on_right_click(Message::NativeWindowAction(
                cosmic::app::Action::ShowWindowMenu,
            ));
        if let Some(end) = end {
            header = header.end(end);
        }
        if window_controls {
            if self.core.window.show_close
                && let Some(window) = self.core.main_window_id()
            {
                header = header.on_close(Message::WindowCloseRequested(window));
            }
            if self.core.window.show_maximize && cosmic::config::show_maximize() {
                header =
                    header.on_maximize(Message::NativeWindowAction(cosmic::app::Action::Maximize));
            }
            if self.core.window.show_minimize && cosmic::config::show_minimize() {
                header =
                    header.on_minimize(Message::NativeWindowAction(cosmic::app::Action::Minimize));
            }
        }
        widget::container(header)
            .center_y(design::HEADER_CONTENT_HEIGHT)
            .width(Length::Fill)
            .class(cosmic::theme::Container::Background)
            .into()
    }

    fn sidebar_toggle(&self) -> Element<'_, Message> {
        crate::ui_controls::icon_control(
            crate::ui_controls::symbolic_icon("view-split-right-symbolic"),
            if self.context_inspector_open {
                "Hide sidebar"
            } else {
                "Show sidebar"
            },
            self.context_inspector_open,
            !self.modal_is_open(),
            (!self.modal_is_open()).then_some(Message::ToggleContextInspector),
        )
        .id(context_inspector_toggle_id())
        .into()
    }

    fn workspace_header_width(&self) -> f32 {
        let navigator = if should_show_card_navigator(self.workspace.cards().len()) {
            110.0
        } else {
            0.0
        };
        let controls = if self.context_inspector_open {
            0.0
        } else {
            let count = usize::from(self.core.window.show_close)
                + usize::from(self.core.window.show_maximize && cosmic::config::show_maximize())
                + usize::from(self.core.window.show_minimize && cosmic::config::show_minimize());
            count as f32 * 32.0
                + count.saturating_sub(1) as f32 * cosmic::theme::spacing().space_xxs as f32
                + design::PRIMARY_CONTROL_SIZE
                + 12.0
        };
        // The docked inspector owns both the alerts control and the update
        // control. Reserving the update here as well pushed it onto the
        // workspace header, on the far side of the bar from the alerts.
        let update = if !self.context_inspector_visible() && self.has_update_control() {
            design::PRIMARY_CONTROL_SIZE + 4.0
        } else {
            0.0
        };
        // Same predicate `workflow_header` mounts the notice control behind,
        // so the strip never reserves room for a control the panel is holding.
        let notice = if !self.context_inspector_visible() && self.has_chrome_notices() {
            design::PRIMARY_CONTROL_SIZE + 2.0
        } else {
            0.0
        };
        let width = navigator + controls + update + notice + 16.0;
        if self.workspace.viewport_width() > 0.0 {
            width.min(self.workspace.viewport_width())
        } else {
            width
        }
    }

    fn tab_strip_geometry(&self) -> (f32, f32) {
        // The strip's own wrapper insets both ends; keep that out of the
        // scroll viewport and reveal calculation.
        let insets = 2.0 * crate::tab_strip::EDGE_INSET;
        let available =
            (self.workspace.viewport_width() - self.workspace_header_width() - insets).max(0.0);
        // Match ceditor: tabs keep a readable fixed width and the strip
        // scrolls as soon as the tab set no longer fits.
        (available, 184.0)
    }

    /// Scrolls the tab strip by the least amount that brings the focused tab
    /// fully into view, and not at all when it is already visible.
    ///
    /// Reveal fires on every focus change, and under focus-follows-mouse that
    /// is every hover. An absolute right-aligning scroll therefore dragged the
    /// strip out from under the pointer.
    pub(super) fn reveal_active_tab(&self) -> Task<Message> {
        let Some(index) = self
            .workspace
            .focused()
            .and_then(|focused| self.workspace.order().position(|id| id == focused))
        else {
            return Task::none();
        };
        let (available, width) = self.tab_strip_geometry();
        if available <= 0.0 {
            return Task::none();
        }
        // The trailing "new tab" control shares the scrollable row, so a tab
        // revealed flush against the viewport edge would sit underneath it.
        let pitch = width + crate::tab_strip::SPACING;
        let content = self.workspace.order().count() as f32 * pitch + design::PRIMARY_CONTROL_SIZE;
        let max_offset = (content - available).max(0.0);
        if max_offset <= 0.0 {
            // Everything fits; any offset at all would be wrong.
            return if self.chrome_ui.tabs_offset > 0.0 {
                workspace_widget::scroll_to(widget::Id::new("workspace.tabs"), 0.0)
                    .map(cosmic::Action::App)
            } else {
                Task::none()
            };
        }
        let leading = index as f32 * pitch;
        let trailing = leading + width;
        let offset = self.chrome_ui.tabs_offset;
        let target = if leading < offset {
            leading
        } else if trailing > offset + available {
            trailing - available
        } else {
            return Task::none();
        };
        workspace_widget::scroll_to(
            widget::Id::new("workspace.tabs"),
            target.clamp(0.0, max_offset),
        )
        .map(cosmic::Action::App)
    }

    /// One compact navigation strip, independent of pane widths and camera offset.
    pub(super) fn workspace_chrome<'a>(
        &'a self,
        content: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let (_, tab_width) = self.tab_strip_geometry();
        let tabs = self
            .workspace
            .order()
            .map(|id| self.column_header(id, self.workspace.focused() == Some(id), tab_width))
            .chain(std::iter::once(self.new_tab_control()))
            .collect();
        let generation = self.workspace_view_generation;
        let tabs = crate::tab_strip::reorderable(
            widget::Id::new("workspace.tabs"),
            tabs,
            design::PRIMARY_CONTROL_SIZE,
            design::HEADER_HEIGHT,
            Some(Box::new(|viewport| {
                Message::ChromeUi(Action::TabsScrolled(viewport.absolute_offset().x))
            })),
            crate::tab_strip::Reordering {
                order: self.workspace.order().collect(),
                tab_width,
                enabled: !self.workspace_inputs_blocked(),
                select: Box::new(Message::FocusCard),
                dropped: Box::new(move |card, before| Message::Viewport {
                    generation,
                    event: WorkspaceViewportEvent::Reordered { card, before },
                }),
            },
        );
        let actions = widget::row::with_children(self.workflow_header())
            .spacing(2)
            .align_y(Alignment::Center);
        let header = self.native_header(
            tabs.into(),
            Some(actions.into()),
            !self.context_inspector_open,
        );
        widget::column::with_children(vec![
            header,
            crate::design::horizontal_rule().into(),
            content,
        ])
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    pub(super) fn workflow_header(&self) -> Vec<Element<'_, Message>> {
        if self.context_inspector_is_overlay() {
            return Vec::new();
        }
        let mut actions = Vec::with_capacity(8);
        let total = self.workspace.cards().len();
        if should_show_card_navigator(total) {
            let current = self
                .workspace
                .focused()
                .and_then(|id| self.workspace.order().position(|candidate| candidate == id))
                .map_or(0, |index| index + 1);
            actions.push(command(
                widget::icon::from_name("go-previous-symbolic"),
                "Previous column",
                widget::Id::new("command.previous-column"),
                (current > 1).then_some(Message::FocusAdjacent(
                    crate::workspace::FocusDirection::Left,
                )),
            ));
            actions.push(
                widget::container(crate::design::caption(format!("{current} / {total}")))
                    .center_y(ICON_CONTROL_SIZE)
                    .center_x(42)
                    .into(),
            );
            actions.push(command(
                widget::icon::from_name("go-next-symbolic"),
                "Next column",
                widget::Id::new("command.next-column"),
                (current > 0 && current < total).then_some(Message::FocusAdjacent(
                    crate::workspace::FocusDirection::Right,
                )),
            ));
        }
        // The inspector panel's own header mounts the notice control, the update
        // control and the inspector toggle, and the two headers must never
        // mount the same one at once: a custom `widget::Id` may name only one
        // node in a frame. The pinned runtime lifts every named node's state
        // into one thread-local map keyed by that name before diffing, hands
        // it to the first widget that asks for the name, and leaves any later
        // claimant holding the `State::None` the lift left behind - which the
        // next draw downcasts, aborting the process.
        //
        // Ownership therefore changes hands when the panel mounts and
        // unmounts, not when it is asked to open or close. Those differ for
        // the whole of the collapse animation: `context_inspector_open` goes
        // false at the click, while the panel stays mounted and keeps drawing
        // its header until the transition ends.
        //
        // The update control sits immediately to the right of the alerts.
        // Leaving it on the workspace header while the panel held the alerts
        // put the two icons on opposite ends of the window.
        let panel_owns_shared_controls = self.context_inspector_visible();
        vec![
            widget::row::with_children(actions)
                .spacing(2)
                .align_y(Alignment::Center)
                .into(),
            widget::row::with_children(
                (!panel_owns_shared_controls)
                    .then(|| self.notice_control())
                    .flatten(),
            )
            .into(),
            widget::row::with_children(
                (!panel_owns_shared_controls)
                    .then(|| self.update_control())
                    .flatten(),
            )
            .into(),
            widget::row::with_children(
                (!panel_owns_shared_controls).then(|| self.sidebar_toggle()),
            )
            .into(),
        ]
    }

    fn column_header(&self, id: CardId, focused: bool, width: f32) -> Element<'_, Message> {
        let card = self.card_state(id);
        let labels = card_kind_labels(card.map(|card| &card.kind));
        let chrome = self.card_chrome.get(&id);
        let title = chrome.map_or_else(
            || card.map_or("Column", |card| card.title.as_str()),
            |chrome| chrome.title.as_str(),
        );
        let focus_id = self.card_header_widget_id(id);
        let caption = chrome
            .map(|chrome| Cow::Borrowed(chrome.caption.as_str()))
            .or_else(|| card.map(|card| card_kind_caption(&card.kind)));
        let icon: Element<'_, Message> = match card.map(|card| &card.kind) {
            Some(CardKind::Browser(_)) => browser_icon().icon().size(16).into(),
            Some(CardKind::Repository(_)) => {
                crate::ui_controls::ceditor_icon().icon().size(16).into()
            }
            Some(CardKind::Chat(session)) => {
                crate::ui_controls::provider_icon(session.launch.provider)
                    .icon()
                    .size(16)
                    .into()
            }
            Some(CardKind::Terminal(session)) => self
                .sidebar_state
                .card_activity(id)
                .map(|activity| activity.provider)
                .or_else(|| crate::agent::Provider::recognize_launch(&session.launch))
                .map(crate::ui_controls::provider_icon)
                .unwrap_or_else(|| crate::ui_controls::symbolic_icon(labels.icon))
                .icon()
                .size(16)
                .into(),
            _ => crate::ui_controls::symbolic_icon(labels.icon)
                .icon()
                .size(16)
                .into(),
        };
        let title_control = widget::button::custom(
            widget::row::with_children(vec![
                icon,
                design::body(title)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(8)
            .align_y(Alignment::Center)
            .height(Length::Fill),
        )
        .id(focus_id)
        .name(chrome.map_or_else(
            || {
                Cow::Owned(card_accessible_name(
                    labels.type_label,
                    title,
                    caption.as_deref().unwrap_or(""),
                    focused,
                ))
            },
            |chrome| Cow::Borrowed(chrome.accessible_name(focused)),
        ))
        .class(column_title_style())
        .padding([0, 8])
        .width(Length::Fill)
        .height(Length::Fill)
        .on_press(Message::FocusCard(id));
        let tip = match caption {
            Some(caption) => format!("{title}\n{caption}"),
            None => title.to_owned(),
        };
        let mut children = vec![tooltip(crate::reorder::handle(title_control, id), tip)];
        let status = match self.cards.get(&id) {
            None | Some(CardRuntime::Loading) => Some((labels.starting, true)),
            Some(CardRuntime::Failed(_)) => Some(("Unavailable", false)),
            #[cfg(feature = "integrated-browser")]
            Some(CardRuntime::Browser(browser))
                if browser.lifecycle == BrowserLifecycle::Restoring =>
            {
                Some(("Restoring", true))
            }
            #[cfg(feature = "integrated-browser")]
            Some(CardRuntime::Browser(browser)) if browser.load_state.is_some() => {
                Some(("Loading", true))
            }
            _ => None,
        };
        if let Some((label, busy)) = status {
            let indicator: Element<'_, Message> = if busy {
                crate::ui_controls::activity_spinner_tone(14.0, design::accent_color)
            } else {
                widget::icon::from_name("dialog-warning-symbolic")
                    .size(14)
                    .into()
            };
            children.push(tooltip(indicator, label));
        }
        if self.sidebar_state.has_card_update(id) {
            children.push(tooltip(
                crate::ui_controls::symbolic_icon("notifications-symbolic")
                    .icon()
                    .size(14),
                "Needs attention",
            ));
        } else if let Some(activity) = self.sidebar_state.card_activity(id) {
            match activity.state {
                crate::agent::ActivityState::Working => children.push(tooltip(
                    crate::ui_controls::activity_spinner_tone(14.0, design::warning_color),
                    "Working",
                )),
                crate::agent::ActivityState::Waiting => children.push(tooltip(
                    crate::ui_controls::symbolic_icon("agent-waiting-symbolic")
                        .icon()
                        .size(14),
                    "Waiting for input",
                )),
                _ => {}
            }
        }
        if self.chat_switch_available(id) {
            children.push(command(
                if card.is_some_and(|card| matches!(card.kind, CardKind::Chat(_))) {
                    widget::icon::from_name("utilities-terminal-symbolic").into()
                } else {
                    chat_icon()
                },
                self.chat_switch_label(id),
                card_control_id(id, "chat"),
                (!self.modal_is_open() && self.chat_switch_ready(id))
                    .then_some(Message::ChatUi(chat_ui::Action::Switch(id))),
            ));
        }
        children.push(command(
            crate::ui_controls::symbolic_icon("window-close-symbolic"),
            chrome.map_or_else(
                || Cow::Owned(card_close_name(labels.type_label, title)),
                |chrome| Cow::Borrowed(chrome.close_name.as_str()),
            ),
            chrome.map_or_else(
                || card_control_id(id, "close"),
                |chrome| chrome.close_id.clone(),
            ),
            Some(Message::CloseCard(id)),
        ));
        crate::tab_strip::middle_click_close(
            widget::container(
                widget::row::with_children(children)
                    .align_y(Alignment::Center)
                    .height(Length::Fill)
                    .spacing(2)
                    .width(Length::Fill),
            )
            .height(design::PRIMARY_CONTROL_SIZE)
            .width(width)
            .class(cosmic::theme::Container::custom(move |theme| {
                iced::widget::container::Style {
                    background: focused.then(|| {
                        iced::Background::Color(
                            theme
                                .current_container()
                                .component
                                .selected_state_color()
                                .into(),
                        )
                    }),
                    border: iced::Border {
                        radius: theme.cosmic().corner_radii.radius_s.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            })),
            Message::CloseCard(id),
        )
    }

    /// The navigation row the context rail speaks for: the active worktree's
    /// own row, so the rail reads one worktree rather than re-deriving it.
    pub(super) fn context_rail_indicators(&self) -> sidebar::WorktreeIndicators<'_> {
        let Some(key) = self.sidebar_state.active_worktree() else {
            return sidebar::WorktreeIndicators::default();
        };
        self.sidebar_model
            .projects
            .iter()
            .find(|project| project.id == key.project_id)
            .and_then(|project| {
                project
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.id == key.worktree_id)
            })
            .map(sidebar::worktree_indicators)
            .unwrap_or_default()
    }

    pub(super) fn context_panel(
        &self,
        model: Option<inspector::InspectorModel>,
        width: f32,
    ) -> Element<'_, Message> {
        let indicators = self.context_rail_indicators();
        let tabs = widget::row::with_children(
            ContextTab::ALL
                .into_iter()
                .filter(|tab| {
                    !self.active_folder_scope()
                        || !matches!(tab, ContextTab::Changes | ContextTab::PullRequest)
                })
                .map(|tab| {
                    let selected = self.context_tab == tab;
                    let enabled = !self.modal_is_open()
                        && (tab != ContextTab::PullRequest || self.active_workspace.is_some());
                    let icon = match tab {
                        ContextTab::Files => "folder-symbolic",
                        ContextTab::Changes => "document-edit-symbolic",
                        ContextTab::PullRequest => "pull-request-symbolic",
                        ContextTab::Details => "dialog-information-symbolic",
                    };
                    let visual = (tab == ContextTab::PullRequest)
                        .then_some(indicators.pull_request)
                        .flatten()
                        .map(sidebar::pull_request_visual);
                    let dirty = (tab == ContextTab::Changes)
                        .then_some(indicators.dirty)
                        .flatten();
                    // The tab's one icon carries the worktree's state: the pull
                    // request's own glyph and tone, the changes tab tinted while the
                    // tree is dirty. What the tint means is said by the tab's name,
                    // which is the tooltip and the accessible name; a second icon
                    // beside the tab was the same fact drawn twice.
                    let mut name = format!("{} panel tab", tab.label());
                    match tab {
                        ContextTab::Changes => {
                            if let Some(label) = dirty {
                                name.push_str(": ");
                                name.push_str(label);
                            }
                        }
                        ContextTab::PullRequest if indicators.pending_checks => {
                            name.push_str(": checks pending");
                        }
                        _ => {}
                    }
                    let control = crate::ui_controls::icon_control(
                        visual.as_ref().map_or_else(
                            || crate::ui_controls::symbolic_icon(icon),
                            |(icon, _)| icon.clone(),
                        ),
                        name,
                        selected,
                        enabled,
                        enabled.then_some(Message::SelectContextTab(tab)),
                    );
                    match (&visual, dirty) {
                        (Some((_, color)), _) => control.icon_color(*color).into(),
                        (None, Some(_)) => control
                            .icon_tone(design::Tone::Themed(design::warning_color))
                            .into(),
                        (None, None) => control.into(),
                    }
                }),
        )
        .spacing(2)
        .align_y(Alignment::Center);
        // Keep named controls in fixed slots as notices and updates come and go.
        // Alerts, then the update icon on their right, then the sidebar toggle.
        // This header is the one mounted while the panel owns those controls,
        // including a docked inspector: the workspace header must not also
        // show the update or the icon stays on the far side of the window.
        let end = widget::row::with_children(vec![
            widget::row::with_children(self.notice_control()).into(),
            widget::row::with_children(self.update_control()).into(),
            self.sidebar_toggle(),
        ])
        .spacing(2)
        .align_y(Alignment::Center)
        .into();
        let heading = self.native_header(iced::widget::Space::new().into(), Some(end), true);
        let tabs = widget::container(tabs)
            .padding([0, 8])
            .center_y(design::HEADER_CONTENT_HEIGHT)
            .width(Length::Fill);
        let body = match self.context_tab {
            ContextTab::Files => sidebar::file_panel(
                &self.sidebar_model,
                &self.sidebar_state,
                width,
                self.modal_is_open(),
            )
            .map(Message::Sidebar),
            ContextTab::Changes => sidebar::git_panel(
                &self.sidebar_model,
                &self.sidebar_state,
                width,
                self.modal_is_open(),
            )
            .map(Message::Sidebar),
            ContextTab::PullRequest => self.pull_request_panel(width),
            ContextTab::Details => model.map_or_else(
                || {
                    widget::container(
                        widget::column::with_children(vec![
                            design::heading("No column selected").into(),
                            design::caption(
                                "Select a column for details, or browse files to open an editor.",
                            )
                            .wrapping(iced::widget::text::Wrapping::Word)
                            .width(Length::Fill)
                            .into(),
                            crate::interaction::tooltip(
                                crate::interaction::hover(
                                    widget::button::text("Browse files")
                                        .name("Browse files")
                                        .on_press(Message::SelectContextTab(ContextTab::Files)),
                                ),
                                "Browse files",
                            ),
                        ])
                        .spacing(8),
                    )
                    .padding(12)
                    .width(Length::Fill)
                    .into()
                },
                |model| {
                    let diagnostics_open = self.inspector_diagnostics_card == Some(model.card_id);
                    inspector::view(model, width, diagnostics_open).map(Message::ContextInspector)
                },
            ),
        };
        widget::container(
            widget::column::with_children(vec![
                heading.into(),
                crate::design::horizontal_rule().into(),
                tabs.into(),
                crate::design::horizontal_rule().into(),
                body,
            ])
            .spacing(0)
            .width(Length::Fill)
            .height(Length::Fill),
        )
        .class(cosmic::theme::Container::Background)
        .width(width)
        .height(Length::Fill)
        .into()
    }
}
