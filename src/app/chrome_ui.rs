use super::*;
use crate::interaction::tooltip;

#[derive(Debug, Default)]
pub(super) struct State {
    popup: Option<Popup>,
    focus_index: usize,
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
pub(super) enum NewTab {
    Terminal,
    NativeChat,
    Omp,
    Claude,
    Codex,
    Browser,
}

const NEW_TAB_CHOICES: [(&str, &str, NewTab, &str); 6] = [
    (
        "Terminal",
        "Shell in the current worktree",
        NewTab::Terminal,
        "chrome.new-tab.terminal",
    ),
    (
        "Native chat",
        "Conversation inside cagents",
        NewTab::NativeChat,
        "chrome.new-tab.native",
    ),
    (
        "OMP",
        "Oh My Pi terminal agent",
        NewTab::Omp,
        "chrome.new-tab.omp",
    ),
    (
        "Claude Code",
        "Anthropic terminal agent",
        NewTab::Claude,
        "chrome.new-tab.claude",
    ),
    (
        "Codex",
        "OpenAI terminal agent",
        NewTab::Codex,
        "chrome.new-tab.codex",
    ),
    (
        "Browser",
        "Open a browser tab",
        NewTab::Browser,
        "chrome.new-tab.browser",
    ),
];

#[derive(Debug, Clone)]
pub(super) enum Action {
    ToggleNewTab,
    ToggleNotices,
    Close,
    DismissNotice,
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
            let ids = match popup {
                Popup::NewTab => NEW_TAB_CHOICES.map(|(_, _, _, id)| Some(id)),
                Popup::Notices => {
                    let action =
                        self.save_health
                            .as_ref()
                            .and_then(|health| match &health.health {
                                SessionSaveHealth::RetryExhausted { .. }
                                    if !health.retry_requested =>
                                {
                                    Some("session-save.retry")
                                }
                                SessionSaveHealth::Recovered { .. } => {
                                    Some("session-save.acknowledge-recovery")
                                }
                                _ => None,
                            });
                    [
                        Some("chrome.notices.close"),
                        self.status.as_ref().map(|_| "chrome.notices.dismiss"),
                        action,
                        None,
                        None,
                        None,
                    ]
                }
            };
            let count = ids.iter().flatten().count();
            self.chrome_ui.focus_index = if backwards {
                (self.chrome_ui.focus_index + count - 1) % count
            } else {
                (self.chrome_ui.focus_index + 1) % count
            };
            return Some(
                iced::widget::operation::focus(widget::Id::new(
                    ids.into_iter()
                        .flatten()
                        .nth(self.chrome_ui.focus_index)
                        .unwrap(),
                ))
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
            Action::DismissNotice => {
                self.status = None;
                self.reconcile_notice_popup()
            }
            Action::Launch(kind) => {
                self.chrome_ui.close();
                match kind {
                    NewTab::Terminal => cosmic::task::message(Message::NewTerminal),
                    NewTab::NativeChat => self.new_native_chat(),
                    NewTab::Browser => cosmic::task::message(Message::NewBrowser),
                    NewTab::Omp | NewTab::Claude | NewTab::Codex => {
                        self.show_workspace_for_action();
                        let Some(working_directory) = self.terminal_directory() else {
                            return cosmic::task::message(Message::OpenRepository);
                        };
                        self.add_terminal_recipe(LaunchRecipe {
                            program: match kind {
                                NewTab::Omp => "omp",
                                NewTab::Claude => "claude",
                                NewTab::Codex => "codex",
                                _ => unreachable!(),
                            }
                            .to_owned(),
                            arguments: Vec::new(),
                            working_directory,
                            environment: Default::default(),
                        })
                    }
                }
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
                        NewTab::NativeChat => chat_icon(),
                        NewTab::Omp => {
                            crate::ui_controls::provider_icon(crate::agent::Provider::Omp)
                        }
                        NewTab::Claude => {
                            crate::ui_controls::provider_icon(crate::agent::Provider::Claude)
                        }
                        NewTab::Codex => {
                            crate::ui_controls::provider_icon(crate::agent::Provider::Codex)
                        }
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
                widget::divider::horizontal::default().into(),
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
        self.status.is_some()
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
        let error = self
            .status
            .as_ref()
            .is_some_and(|notice| notice.severity == NoticeSeverity::Error)
            || self.save_health.as_ref().is_some_and(|health| {
                matches!(
                    health.health,
                    SessionSaveHealth::RetryExhausted { .. } | SessionSaveHealth::WriterUnavailable
                )
            });
        let control = crate::ui_controls::icon_control(
            crate::ui_controls::symbolic_icon(if error {
                "dialog-error-symbolic"
            } else {
                "dialog-warning-symbolic"
            }),
            if error {
                "Application errors"
            } else {
                "Application notices"
            },
            self.chrome_ui.popup == Some(Popup::Notices),
            !self.modal_is_open(),
            (!self.modal_is_open()).then_some(Message::ChromeUi(Action::ToggleNotices)),
        )
        .id(widget::Id::new("chrome.notices"))
        .icon_color(if error {
            cosmic::theme::active().cosmic().destructive.base.into()
        } else {
            cosmic::theme::active().cosmic().warning.base.into()
        });
        let content =
            (self.chrome_ui.popup == Some(Popup::Notices) && !self.modal_is_open()).then(|| {
                let mut notices = Vec::with_capacity(2);
                if let Some(notice) = &self.status {
                    notices.push(
                        widget::column::with_children(vec![
                            widget::text::body(&notice.message)
                                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                                .width(Length::Fill)
                                .into(),
                            widget::button::text("Dismiss notice")
                                .id(widget::Id::new("chrome.notices.dismiss"))
                                .name("Dismiss application notice")
                                .on_press(Message::ChromeUi(Action::DismissNotice))
                                .into(),
                        ])
                        .spacing(8)
                        .into(),
                    );
                }
                if let Some(health) = self.save_health.as_ref().and_then(save_health_view) {
                    notices.push(health);
                }
                let heading = widget::row::with_children(vec![
                    design::heading("Application notices")
                        .width(Length::Fill)
                        .into(),
                    command(
                        widget::icon::from_name("window-close-symbolic"),
                        "Close application notices",
                        widget::Id::new("chrome.notices.close"),
                        Some(Message::ChromeUi(Action::Close)),
                    ),
                ])
                .align_y(Alignment::Center)
                .spacing(8);
                widget::container(
                    widget::column::with_children(vec![
                        heading.into(),
                        widget::scrollable(widget::column::with_children(notices).spacing(12))
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
            .center_y(design::HEADER_HEIGHT - design::BORDER_WIDTH)
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
        let update = if self.has_update_control() {
            design::PRIMARY_CONTROL_SIZE + 4.0
        } else {
            0.0
        };
        let notice = if !self.context_inspector_open && self.has_chrome_notices() {
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
        let available = (self.workspace.viewport_width() - self.workspace_header_width()).max(0.0);
        let width = ((available - design::PRIMARY_CONTROL_SIZE - 2.0).max(0.0)
            / self.workspace.cards().len().max(1) as f32)
            .clamp(96.0, 200.0);
        (available, width)
    }

    pub(super) fn reveal_active_tab(&self) -> Task<Message> {
        let Some(index) = self
            .workspace
            .focused()
            .and_then(|focused| self.workspace.order().position(|id| id == focused))
        else {
            return Task::none();
        };
        let (available, width) = self.tab_strip_geometry();
        workspace_widget::scroll_to(
            widget::Id::new("workspace.tabs"),
            ((index + 1) as f32 * (width + 2.0) - available).max(0.0),
        )
        .map(cosmic::Action::App)
    }

    /// One compact navigation strip, independent of pane widths and camera offset.
    pub(super) fn workspace_chrome<'a>(
        &'a self,
        content: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let (_, tab_width) = self.tab_strip_geometry();
        let tabs = widget::row::with_children(
            self.workspace
                .order()
                .map(|id| self.column_header(id, self.workspace.focused() == Some(id), tab_width)),
        )
        .push(self.new_tab_control())
        .spacing(2)
        .height(design::PRIMARY_CONTROL_SIZE);
        let tabs = crate::interaction::overflow_fade_x(crate::interaction::horizontal_wheel(
            widget::scrollable::horizontal(tabs)
                .id(widget::Id::new("workspace.tabs"))
                .direction(iced::widget::scrollable::Direction::Horizontal(
                    iced::widget::scrollable::Scrollbar::hidden(),
                ))
                .width(Length::Fill)
                .height(design::PRIMARY_CONTROL_SIZE),
        ));
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
            widget::divider::horizontal::default().into(),
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
        vec![
            widget::row::with_children(actions)
                .spacing(2)
                .align_y(Alignment::Center)
                .into(),
            widget::row::with_children(self.update_control()).into(),
            widget::row::with_children(
                (!self.context_inspector_open)
                    .then(|| self.notice_control())
                    .flatten(),
            )
            .into(),
            widget::row::with_children(
                (!self.context_inspector_open).then(|| self.sidebar_toggle()),
            )
            .into(),
        ]
    }

    fn column_header(&self, id: CardId, focused: bool, width: f32) -> Element<'_, Message> {
        let card = self.card_state(id);
        let title = card.map_or("Column", |card| card.title.as_str());
        let labels = card_kind_labels(card.map(|card| &card.kind));
        let chrome = self.card_chrome.get(&id);
        let focus_id = chrome.map_or_else(
            || workspace_widget::card_focus_id(id),
            |chrome| chrome.focus_id.clone(),
        );
        let caption = card.map(|card| card_kind_caption(&card.kind));
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
        let mut children = vec![tooltip(title_control, tip)];
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
                widget::progress_bar::circular::Circular::new()
                    .size(14.0)
                    .into()
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
            let indicator = match activity.state {
                crate::agent::ActivityState::Working => Some(("agent-working-symbolic", "Working")),
                crate::agent::ActivityState::Waiting => {
                    Some(("agent-waiting-symbolic", "Waiting for input"))
                }
                _ => None,
            };
            if let Some((icon, label)) = indicator {
                children.push(tooltip(
                    crate::ui_controls::symbolic_icon(icon).icon().size(14),
                    label,
                ));
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
                (!self.modal_is_open()).then_some(Message::ChatUi(chat_ui::Action::Switch(id))),
            ));
        }
        children.push(command(
            widget::icon::from_name("window-close-symbolic"),
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
        }))
        .into()
    }

    pub(super) fn context_panel(
        &self,
        model: Option<inspector::InspectorModel>,
        width: f32,
    ) -> Element<'_, Message> {
        let tabs = widget::row::with_children(ContextTab::ALL.into_iter().map(|tab| {
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
                .then(|| {
                    let key = self.sidebar_state.active_worktree()?;
                    let project = self
                        .sidebar_model
                        .projects
                        .iter()
                        .find(|project| project.id == key.project_id)?;
                    let worktree = project
                        .worktrees
                        .iter()
                        .find(|worktree| worktree.path == key.canonical_path)?;
                    let branch = worktree.branch.as_deref()?;
                    project
                        .pull_requests
                        .iter()
                        .find(|pr| pr.head_ref == branch)
                        .map(sidebar::pull_request_visual)
                })
                .flatten();
            let control = crate::ui_controls::icon_control(
                visual.as_ref().map_or_else(
                    || crate::ui_controls::symbolic_icon(icon),
                    |(icon, _)| icon.clone(),
                ),
                format!("{} panel tab", tab.label()),
                selected,
                enabled,
                enabled.then_some(Message::SelectContextTab(tab)),
            );
            let changes = tab == ContextTab::Changes
                && self
                    .sidebar_model
                    .source_control
                    .as_ref()
                    .is_some_and(|source| {
                        self.sidebar_state.active_worktree() == Some(&source.key)
                            && source.has_changes()
                    });
            match visual {
                Some((_, color)) => control.icon_color(color).into(),
                None if changes => control
                    .icon_color(cosmic::theme::active().cosmic().warning.base.into())
                    .into(),
                None => control.into(),
            }
        }))
        .spacing(2)
        .align_y(Alignment::Center);
        // Keep named controls in fixed slots as notices and updates come and go.
        let end = widget::row::with_children(vec![
            widget::row::with_children(
                self.context_inspector_is_overlay()
                    .then(|| self.update_control())
                    .flatten(),
            )
            .into(),
            widget::row::with_children(self.notice_control()).into(),
            self.sidebar_toggle(),
        ])
        .spacing(2)
        .align_y(Alignment::Center)
        .into();
        let heading = self.native_header(iced::widget::Space::new().into(), Some(end), true);
        let tabs = widget::container(tabs)
            .padding([0, 8])
            .center_y(design::HEADER_HEIGHT - design::BORDER_WIDTH)
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
                |model| inspector::view(model, width).map(Message::ContextInspector),
            ),
        };
        widget::container(
            widget::column::with_children(vec![
                heading.into(),
                widget::divider::horizontal::default().into(),
                tabs.into(),
                widget::divider::horizontal::default().into(),
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
