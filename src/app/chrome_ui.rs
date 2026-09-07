use super::*;
use crate::interaction::{hover, tooltip};

fn browser_icon() -> widget::icon::Handle {
    static ICON: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(&br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><g fill="none" stroke="black" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><ellipse cx="12" cy="12" rx="4" ry="9"/><path d="M3 12h18M5 6.5h14M5 17.5h14"/></g></svg>"#[..]).symbolic(true)
    });
    ICON.clone()
}

fn command<'a>(
    icon: impl Into<widget::icon::Handle>,
    label: impl Into<Cow<'a, str>>,
    id: widget::Id,
    action: Option<Message>,
) -> Element<'a, Message> {
    let label = label.into();
    tooltip(
        hover(
            widget::button::icon(icon)
                .id(id)
                .name(label.clone())
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .on_press_maybe(action),
        ),
        label,
    )
}

impl Cagents {
    pub(super) fn workflow_header(&self) -> Vec<Element<'_, Message>> {
        if self.context_inspector_is_overlay() {
            return Vec::new();
        }
        let mut actions = Vec::with_capacity(7);
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
                widget::container(widget::text::caption(format!("{current} / {total}")))
                    .center_y(ICON_CONTROL_SIZE)
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
        let new_terminal_label = workspace_widget::shortcut_tooltip(
            workspace_widget::GlobalShortcut::NewTerminal,
            &self.settings.keyboard_shortcuts,
        );
        actions.push(
            if self.window_width >= design::EXPANDED_NAVIGATION_BREAKPOINT {
                tooltip(
                    widget::button::standard("Terminal")
                        .leading_icon(
                            widget::icon::from_name("utilities-terminal-symbolic").size(16),
                        )
                        .id(widget::Id::new(TOOLBAR_FOCUS_ID))
                        .name(new_terminal_label.clone())
                        .height(ICON_CONTROL_SIZE)
                        .on_press(Message::NewTerminal),
                    new_terminal_label,
                )
            } else {
                command(
                    widget::icon::from_name("utilities-terminal-symbolic"),
                    new_terminal_label,
                    widget::Id::new(TOOLBAR_FOCUS_ID),
                    Some(Message::NewTerminal),
                )
            },
        );
        actions.push(command(
            browser_icon(),
            "New browser",
            widget::Id::new("command.new-browser"),
            Some(Message::NewBrowser),
        ));
        let panel_label = if self.context_inspector_open {
            "Hide context panel"
        } else {
            "Show context panel"
        };
        actions.push(
            if self.window_width >= design::EXPANDED_NAVIGATION_BREAKPOINT {
                tooltip(
                    widget::button::custom(
                        widget::row::with_children(vec![
                            widget::icon::from_name("view-list-symbolic")
                                .size(16)
                                .into(),
                            widget::text::body("Context").into(),
                        ])
                        .spacing(design::GRID_UNIT)
                        .align_y(Alignment::Center),
                    )
                    .class(cosmic::theme::Button::Text)
                    .id(context_inspector_toggle_id())
                    .name(panel_label)
                    .selected(self.context_inspector_open)
                    .height(ICON_CONTROL_SIZE)
                    .on_press(Message::ToggleContextInspector),
                    panel_label,
                )
            } else {
                command(
                    widget::icon::from_name("view-list-symbolic"),
                    panel_label,
                    context_inspector_toggle_id(),
                    Some(Message::ToggleContextInspector),
                )
            },
        );
        actions
    }

    pub(super) fn column_header(&self, id: CardId, focused: bool) -> Element<'_, Message> {
        let card = self.card_state(id);
        let title = card.map_or("Column", |card| card.title.as_str());
        let labels = card_kind_labels(card.map(|card| &card.kind));
        let chrome = self.card_chrome.get(&id);
        let caption = card.map(|card| card_kind_caption(&card.kind));
        let icon: Element<'_, Message> =
            if card.is_some_and(|card| matches!(card.kind, CardKind::Browser(_))) {
                browser_icon().icon().size(16).into()
            } else {
                widget::icon::from_name(labels.icon).size(16).into()
            };
        let title_control = widget::button::custom(
            widget::row::with_children(vec![
                icon,
                widget::text::body(title)
                    .size(13)
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
        .id(chrome.map_or_else(
            || workspace_widget::card_focus_id(id),
            |chrome| chrome.focus_id.clone(),
        ))
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
        .class(cosmic::theme::Button::Text)
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
        let row = widget::container(
            widget::row::with_children(children)
                .align_y(Alignment::Center)
                .height(Length::Fill)
                .spacing(4),
        )
        .height(CARD_HEADER_HEIGHT - 1.0)
        .padding([0, 4])
        .class(cosmic::theme::Container::Primary);
        let rail = widget::container(iced::widget::Space::new())
            .height(1)
            .width(Length::Fill)
            .class(cosmic::theme::Container::custom(move |theme| {
                iced::widget::container::Style {
                    background: Some(iced::Background::Color(if focused {
                        (*theme.cosmic().accent_color()).into()
                    } else {
                        (*theme.cosmic().bg_divider()).into()
                    })),
                    ..Default::default()
                }
            }));
        hover(
            widget::column::with_children(vec![row.into(), rail.into()])
                .spacing(0)
                .height(CARD_HEADER_HEIGHT),
        )
    }

    pub(super) fn context_panel(
        &self,
        model: Option<inspector::InspectorModel>,
        width: f32,
    ) -> Element<'_, Message> {
        let tabs = widget::row::with_children(ContextTab::ALL.into_iter().map(|tab| {
            widget::button::custom(widget::text::body(tab.label()))
                .class(if self.context_tab == tab {
                    cosmic::theme::Button::Suggested
                } else {
                    cosmic::theme::Button::Text
                })
                .name(format!("{} panel tab", tab.label()))
                .selected(self.context_tab == tab)
                .height(design::ROW_HEIGHT)
                .width(Length::Fill)
                .padding([4, 8])
                .on_press_maybe((!self.modal_is_open()).then_some(Message::SelectContextTab(tab)))
                .into()
        }))
        .spacing(design::GRID_UNIT)
        .width(Length::Fill);
        let heading = widget::row::with_children(vec![
            tabs.into(),
            command(
                widget::icon::from_name("window-close-symbolic"),
                "Close context panel",
                inspector::close_focus_id(),
                Some(Message::ContextInspector(InspectorAction::Close)),
            ),
        ])
        .spacing(design::GRID_UNIT)
        .align_y(Alignment::Center);
        let body = match self.context_tab {
            ContextTab::Changes => sidebar::git_panel(
                &self.sidebar_model, &self.sidebar_state, width, self.modal_is_open(),
            ).map(Message::Sidebar),
            ContextTab::Context | ContextTab::Code => model.map_or_else(
                || {
                    let (title, detail) = if self.context_tab == ContextTab::Code {
                        ("No code column selected", "Open a file from Changes → Files to see editor actions, diagnostics, and language-server results here.")
                    } else {
                        ("Workspace context", "Select a terminal, browser, or repository column to see its details and available actions.")
                    };
                    widget::container(widget::column::with_children(vec![
                        widget::icon::from_name(if self.context_tab == ContextTab::Code {
                            "text-x-generic-symbolic"
                        } else { "dialog-information-symbolic" }).size(28).into(),
                        widget::text::heading(title).into(),
                        widget::text::body(detail)
                            .wrapping(iced::widget::text::Wrapping::Word)
                            .width(Length::Fill).into(),
                        if self.context_tab == ContextTab::Code && self.sidebar_state.active_worktree().is_some() {
                            widget::button::standard("Browse files")
                                .on_press(Message::SelectContextTab(ContextTab::Changes)).into()
                        } else {
                            widget::button::standard("Open repository")
                                .on_press(Message::OpenRepository).into()
                        },
                    ]).spacing(12).width(Length::Fill))
                    .padding(20)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
                },
                |model| inspector::view(model, width).map(Message::ContextInspector),
            ),
        };
        widget::container(
            widget::column::with_children(vec![
                widget::container(heading)
                    .padding([8, 8])
                    .width(Length::Fill)
                    .into(),
                widget::divider::horizontal::light().into(),
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
