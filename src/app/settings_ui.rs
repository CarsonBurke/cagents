use super::*;

fn settings_input_style(progress: f32) -> cosmic::theme::TextInput {
    use widget::text_input::StyleSheet;

    let progress = progress.clamp(0.0, 1.0);
    if progress == 1.0 {
        return cosmic::theme::TextInput::Default;
    }
    cosmic::theme::TextInput::Custom {
        active: Box::new(move |theme| {
            fade_input_appearance(theme.active(&cosmic::theme::TextInput::Default), progress)
        }),
        error: Box::new(move |theme| {
            fade_input_appearance(theme.error(&cosmic::theme::TextInput::Default), progress)
        }),
        hovered: Box::new(move |theme| {
            fade_input_appearance(theme.hovered(&cosmic::theme::TextInput::Default), progress)
        }),
        focused: Box::new(move |theme| {
            fade_input_appearance(theme.focused(&cosmic::theme::TextInput::Default), progress)
        }),
        disabled: Box::new(move |theme| {
            fade_input_appearance(theme.disabled(&cosmic::theme::TextInput::Default), progress)
        }),
    }
}

fn fade_input_appearance(
    mut appearance: widget::text_input::Appearance,
    progress: f32,
) -> widget::text_input::Appearance {
    // The outer fade already scales semantic text, selection, and border colors.
    // Pinned COSMIC replaces background alpha with 0.25 and blends placeholder
    // RGB into an opaque color, so only these final primitives need correction.
    appearance.background = appearance.background.scale_alpha(progress);
    appearance.placeholder_color.a *= progress;
    appearance
}

fn setting_input<'a>(
    label: &'static str,
    detail: impl Into<Cow<'a, str>> + 'a,
    value: &'a str,
    field: SettingsField,
    progress: f32,
) -> Element<'a, Message> {
    setting_field(
        label,
        detail,
        widget::text_input(label, value)
            .on_input(move |value| Message::SettingsChanged { field, value })
            .width(Length::Fill)
            .style(settings_input_style(progress))
            .into(),
    )
}

// Font names and shortcut bindings must not be applied while partly typed.
fn setting_input_on_submit<'a>(
    label: &'static str,
    detail: impl Into<Cow<'a, str>> + 'a,
    value: &'a str,
    field: SettingsField,
    progress: f32,
) -> Element<'a, Message> {
    setting_field(
        label,
        detail,
        widget::text_input(label, value)
            .on_input(move |value| Message::SettingsDraftChanged { field, value })
            .on_submit(move |value| Message::SettingsChanged { field, value })
            .width(Length::Fill)
            .style(settings_input_style(progress))
            .into(),
    )
}

fn setting_field<'a>(
    label: &'static str,
    detail: impl Into<Cow<'a, str>> + 'a,
    control: Element<'a, Message>,
) -> Element<'a, Message> {
    widget::column::with_children(vec![
        widget::text::caption_heading(label).into(),
        control,
        widget::text::caption(detail)
            .wrapping(iced::widget::text::Wrapping::Word)
            .width(Length::Fill)
            .into(),
    ])
    .spacing(design::GRID_UNIT)
    .width(Length::Fill)
    .into()
}

fn setting_stepper<'a>(
    label: &'static str,
    detail: impl Into<Cow<'a, str>> + 'a,
    value: &'a str,
    field: SettingsField,
    min: f32,
    max: f32,
    step: f32,
    default: f32,
    unit: &'static str,
) -> Element<'a, Message> {
    let current = value.parse::<f32>().unwrap_or(default).clamp(min, max);
    let change = |next: f32| Message::SettingsChanged {
        field,
        value: format!("{:.3}", next.clamp(min, max)),
    };
    let control = widget::row::with_children(vec![
        widget::button::icon(widget::icon::from_name("list-remove-symbolic"))
            .name(format!("Decrease {label}"))
            .width(design::ROW_HEIGHT)
            .height(design::ROW_HEIGHT)
            .on_press_maybe((current > min).then(|| change(current - step)))
            .into(),
        widget::container(widget::text::body(format!("{}{}", current, unit)))
            .center_x(Length::Fill)
            .center_y(design::ROW_HEIGHT)
            .width(Length::Fill)
            .into(),
        widget::button::icon(widget::icon::from_name("list-add-symbolic"))
            .name(format!("Increase {label}"))
            .width(design::ROW_HEIGHT)
            .height(design::ROW_HEIGHT)
            .on_press_maybe((current < max).then(|| change(current + step)))
            .into(),
        widget::button::text("Reset")
            .name(format!("Reset {label}"))
            .on_press_maybe((current != default).then(|| change(default)))
            .into(),
    ])
    .spacing(design::GRID_UNIT)
    .align_y(Alignment::Center)
    .width(Length::Fill)
    .into();
    setting_field(label, detail, control)
}

fn settings_choice<'a>(
    label: &'static str,
    detail: &'static str,
    field: SettingsField,
    choices: impl IntoIterator<Item = (&'static str, &'static str, bool)>,
    progress: f32,
) -> Element<'a, Message> {
    let buttons: Vec<Element<'a, Message>> = choices
        .into_iter()
        .map(|(choice, value, selected)| {
            let marker: Element<'a, Message> = if selected {
                settings_icon("object-select-symbolic", progress).into()
            } else {
                iced::widget::Space::new().width(16).height(16).into()
            };
            crate::interaction::hover(
                widget::button::custom(
                    widget::row::with_children(vec![marker, widget::text::body(choice).into()])
                        .spacing(2.0 * design::GRID_UNIT)
                        .align_y(Alignment::Center),
                )
                .name(choice)
                .class(cosmic::theme::Button::Text)
                .selected(selected)
                .padding([design::GRID_UNIT, 2.0 * design::GRID_UNIT])
                .height(design::ROW_HEIGHT)
                .width(Length::Fill)
                .on_press(Message::SettingsChanged {
                    field,
                    value: value.to_owned(),
                }),
            )
        })
        .collect();
    setting_field(
        label,
        detail,
        widget::column::with_children(buttons)
            .spacing(design::GRID_UNIT)
            .width(Length::Fill)
            .into(),
    )
}

fn settings_group<'a>(
    title: &'static str,
    description: &'static str,
    controls: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut children = Vec::with_capacity(controls.len() + 1);
    children.push(
        widget::column::with_children(vec![
            widget::text::title4(title).into(),
            widget::text::caption(description)
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into(),
        ])
        .spacing(design::GRID_UNIT)
        .into(),
    );
    children.extend(controls);
    widget::container(
        widget::column::with_children(children)
            .spacing(4.0 * design::GRID_UNIT)
            .width(Length::Fill),
    )
    .class(cosmic::theme::Container::Card)
    .padding(4.0 * design::GRID_UNIT)
    .width(Length::Fill)
    .into()
}

fn settings_category_controls<'a>(
    category: SettingsCategory,
    draft: &'a SettingsDraft,
    progress: f32,
) -> Vec<Element<'a, Message>> {
    match category {
        SettingsCategory::General => vec![settings_group(
            "Runtime",
            "Control how many live columns cagents keeps active and when background work is suspended.",
            vec![
                setting_input(
                    "Maximum hot columns",
                    "Applies immediately; extra background columns may hibernate.",
                    &draft.max_hot_cards,
                    SettingsField::MaxHotCards,
                    progress,
                ),
                setting_input(
                    "Hibernation delay (ms)",
                    "Applies immediately to background columns.",
                    &draft.hibernation_delay_ms,
                    SettingsField::HibernationDelayMs,
                    progress,
                ),
            ],
        )],
        SettingsCategory::Appearance => vec![
            settings_group(
                "Interface",
                "Make the workbench comfortable on your display. Changes apply immediately and are saved automatically.",
                vec![
                    settings_choice(
                        "Color scheme",
                        "System follows your desktop, including later theme changes.",
                        SettingsField::AppTheme,
                        [
                            ("System", "system", draft.app_theme == "system"),
                            ("Dark", "dark", draft.app_theme == "dark"),
                            ("Light", "light", draft.app_theme == "light"),
                        ],
                        progress,
                    ),
                    setting_stepper(
                        "Interface scale",
                        "Scales the entire interface independently of display scaling.",
                        &draft.ui_scale,
                        SettingsField::UiScale,
                        0.8,
                        1.5,
                        0.05,
                        1.0,
                        "×",
                    ),
                ],
            ),
            settings_group(
                "Workspace layout",
                "Keep navigation and context readable without giving up working space.",
                vec![
                    setting_stepper(
                        "Projects pane width",
                        "Also adjustable by dragging the pane edge.",
                        &draft.sidebar_width,
                        SettingsField::SidebarWidth,
                        design::NAVIGATION_MIN_WIDTH,
                        design::NAVIGATION_MAX_WIDTH,
                        20.0,
                        design::NAVIGATION_DEFAULT_WIDTH,
                        " px",
                    ),
                    setting_stepper(
                        "Context panel width",
                        "Used when docked; the panel fits the available width when overlaid.",
                        &draft.inspector_width,
                        SettingsField::InspectorWidth,
                        design::INSPECTOR_MIN_WIDTH,
                        design::INSPECTOR_MAX_WIDTH,
                        20.0,
                        design::INSPECTOR_DEFAULT_WIDTH,
                        " px",
                    ),
                    setting_stepper(
                        "New column width",
                        "Fraction of the workspace. Existing column widths are preserved.",
                        &draft.default_column_fraction,
                        SettingsField::DefaultColumnFraction,
                        0.25,
                        1.0,
                        0.05,
                        0.72,
                        "×",
                    ),
                ],
            ),
        ],
        SettingsCategory::Terminal => vec![
            settings_group(
                "Text & cursor",
                "Tune every open terminal live. Application colors are controlled by the shell or terminal program.",
                vec![
                    setting_stepper(
                        "Font size",
                        "Logical pixels before interface and display scaling.",
                        &draft.terminal_font_size,
                        SettingsField::TerminalFontSize,
                        8.0,
                        40.0,
                        1.0,
                        14.0,
                        " px",
                    ),
                    setting_input_on_submit(
                        "Font family",
                        "Leave empty for system monospace. Use an installed fixed-pitch family, including a Nerd Font for Powerline glyphs. Proportional families fall back to monospace. Press Enter to apply.",
                        &draft.terminal_font_family,
                        SettingsField::TerminalFontFamily,
                        progress,
                    ),
                    settings_choice(
                        "Font preset",
                        "Choose System to reset a custom family; enter any installed monospace family above.",
                        SettingsField::TerminalFontFamily,
                        [(
                            "System monospace",
                            "",
                            draft.terminal_font_family.is_empty(),
                        )],
                        progress,
                    ),
                    setting_stepper(
                        "Line height",
                        "Space between text baselines, relative to font size.",
                        &draft.terminal_line_height,
                        SettingsField::TerminalLineHeight,
                        1.0,
                        2.0,
                        0.05,
                        1.286,
                        "×",
                    ),
                    setting_stepper(
                        "Inner padding",
                        "Breathing room between terminal text and the column edge.",
                        &draft.terminal_padding,
                        SettingsField::TerminalPadding,
                        0.0,
                        24.0,
                        2.0,
                        8.0,
                        " px",
                    ),
                    settings_choice(
                        "Cursor shape",
                        "Application lets the running program choose its cursor; other choices override the shape.",
                        SettingsField::TerminalCursorStyle,
                        [
                            (
                                "Application",
                                "application",
                                draft.terminal_cursor_style == "application",
                            ),
                            ("Block", "block", draft.terminal_cursor_style == "block"),
                            ("Beam", "beam", draft.terminal_cursor_style == "beam"),
                            (
                                "Underline",
                                "underline",
                                draft.terminal_cursor_style == "underline",
                            ),
                        ],
                        progress,
                    ),
                ],
            ),
            settings_group(
                "History",
                "Keep enough output to investigate a task without retaining unlimited terminal history.",
                vec![setting_input_on_submit(
                    "Scrollback lines",
                    "0–1,000,000 lines per newly started terminal. Press Enter to apply; existing history is preserved.",
                    &draft.terminal_scrollback_lines,
                    SettingsField::TerminalScrollbackLines,
                    progress,
                )],
            ),
        ],
        SettingsCategory::Links => vec![settings_group(
            "Link handling",
            "Choose where modified clicks on known links open. Regular clicks stay inside the workspace.",
            vec![
                settings_choice(
                    "Open modified links in",
                    "Applies to pull request links and other links whose destination is known to cagents.",
                    SettingsField::LinkOpenDestination,
                    [
                        (
                            "Integrated browser",
                            LinkOpenDestination::IntegratedBrowser.as_str(),
                            draft.link_open_destination == LinkOpenDestination::IntegratedBrowser,
                        ),
                        (
                            "System default",
                            LinkOpenDestination::SystemDefault.as_str(),
                            draft.link_open_destination == LinkOpenDestination::SystemDefault,
                        ),
                    ],
                    progress,
                ),
                settings_choice(
                    "Modified-click gesture",
                    "This gesture opens the link in the destination above.",
                    SettingsField::LinkOpenGesture,
                    [
                        (
                            "Ctrl+Click",
                            LinkOpenGesture::ControlClick.as_str(),
                            draft.link_open_gesture == LinkOpenGesture::ControlClick,
                        ),
                        (
                            "Ctrl+Shift+Click",
                            LinkOpenGesture::ControlShiftClick.as_str(),
                            draft.link_open_gesture == LinkOpenGesture::ControlShiftClick,
                        ),
                    ],
                    progress,
                ),
            ],
        )],
        SettingsCategory::Keyboard => vec![settings_group(
            "Keyboard shortcuts",
            "Each row takes one or more chords separated by commas, and applies when you press Enter. Use Ctrl, Alt, or Super, optionally with Shift, plus A-Z, 0-9, Left, or Right. Every chord in the table must be unique.",
            vec![
                // Applied on submit, not per keystroke: a partly typed list
                // parses on its way to the one the user means - "Alt+A" is a
                // valid chord while "Alt+A, Alt+Left" is being typed - and
                // adopting each prefix would rebind the action and rewrite the
                // settings file at typing speed.
                setting_input_on_submit(
                    "New terminal",
                    "Creates a terminal column.",
                    &draft.keyboard_shortcuts.new_terminal,
                    SettingsField::ShortcutNewTerminal,
                    progress,
                ),
                setting_input_on_submit(
                    "Close focused column",
                    "Closes the active workspace column.",
                    &draft.keyboard_shortcuts.close_focused_column,
                    SettingsField::ShortcutCloseFocusedColumn,
                    progress,
                ),
                setting_input_on_submit(
                    "Search projects",
                    "Moves keyboard focus to project search.",
                    &draft.keyboard_shortcuts.search_projects,
                    SettingsField::ShortcutSearchProjects,
                    progress,
                ),
                setting_input_on_submit(
                    "Previous column",
                    "Moves focus left and may repeat while held.",
                    &draft.keyboard_shortcuts.previous_column,
                    SettingsField::ShortcutPreviousColumn,
                    progress,
                ),
                setting_input_on_submit(
                    "Next column",
                    "Moves focus right and may repeat while held.",
                    &draft.keyboard_shortcuts.next_column,
                    SettingsField::ShortcutNextColumn,
                    progress,
                ),
            ],
        )],
    }
}

fn category_icon(category: SettingsCategory) -> &'static str {
    match category {
        SettingsCategory::General => "preferences-system-symbolic",
        SettingsCategory::Appearance => "preferences-desktop-display-symbolic",
        SettingsCategory::Terminal => "utilities-terminal-symbolic",
        SettingsCategory::Links => "web-browser-symbolic",
        SettingsCategory::Keyboard => "preferences-desktop-keyboard-symbolic",
    }
}

fn settings_icon(name: &'static str, progress: f32) -> widget::icon::Icon {
    let handle: widget::icon::Handle = widget::icon::from_name(name).into();
    handle.icon().size(16).opacity(progress.clamp(0.0, 1.0))
}

fn category_button<'a>(
    item: SettingsCategory,
    selected: SettingsCategory,
    progress: f32,
) -> Element<'a, Message> {
    crate::interaction::hover(
        widget::button::custom(
            widget::row::with_children(vec![
                settings_icon(category_icon(item), progress).into(),
                widget::text::body(item.label()).into(),
            ])
            .align_y(Alignment::Center)
            .spacing(2.0 * design::GRID_UNIT),
        )
        .name(item.label())
        .class(cosmic::theme::Button::MenuItem)
        .selected(item == selected)
        .on_press(Message::SelectSettingsCategory(item))
        .padding([design::GRID_UNIT, 2.0 * design::GRID_UNIT])
        .height(design::ROW_HEIGHT)
        .width(Length::Fill),
    )
}

pub(super) fn settings_dialog<'a>(
    category: SettingsCategory,
    draft: &'a SettingsDraft,
    feedback: Option<&'a SettingsFeedback>,
    window_width: f32,
    progress: f32,
) -> Element<'a, Message> {
    let narrow = window_width < design::OVERLAY_DOCK_BREAKPOINT;
    let gutter = if narrow { 3.0 } else { 5.0 } * design::GRID_UNIT;
    let navigation: Element<'a, Message> = if narrow {
        // Two columns keep every category readable without an overflowing tab strip.
        let rows: Vec<Element<'a, Message>> = SettingsCategory::ALL
            .chunks(2)
            .map(|items| {
                let mut buttons: Vec<_> = items
                    .iter()
                    .map(|item| category_button(*item, category, progress))
                    .collect();
                if items.len() == 1 {
                    buttons.push(iced::widget::Space::new().width(Length::Fill).into());
                }
                widget::row::with_children(buttons)
                    .spacing(design::GRID_UNIT)
                    .width(Length::Fill)
                    .into()
            })
            .collect();
        widget::container(
            widget::column::with_children(rows)
                .spacing(design::GRID_UNIT)
                .width(Length::Fill),
        )
        .padding([2.0 * design::GRID_UNIT, gutter])
        .width(Length::Fill)
        .into()
    } else {
        widget::container(
            widget::column::with_children(
                SettingsCategory::ALL
                    .into_iter()
                    .map(|item| category_button(item, category, progress)),
            )
            .spacing(design::GRID_UNIT)
            .width(Length::Fill),
        )
        .padding([3.0 * design::GRID_UNIT, 2.0 * design::GRID_UNIT])
        .width(Length::Fixed(42.0 * design::GRID_UNIT))
        .height(Length::Fill)
        .into()
    };

    let groups = settings_category_controls(category, draft, progress);
    let controls = groups;
    let content: Element<'a, Message> = widget::container(
        widget::scrollable(
            widget::column::with_children(controls)
                .spacing(5.0 * design::GRID_UNIT)
                .width(Length::Fill)
                .max_width(CONTENT_MAX_WIDTH),
        )
        .class(cosmic::style::iced::Scrollable::Minimal)
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .padding([4.0 * design::GRID_UNIT, gutter])
    .width(Length::Fill)
    .height(Length::Fill)
    .into();
    let body: Element<'a, Message> = if narrow {
        widget::column::with_children(vec![
            navigation,
            widget::divider::horizontal::light().into(),
            content,
        ])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    } else {
        widget::row::with_children(vec![
            navigation,
            widget::divider::vertical::light().into(),
            content,
        ])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    };

    let feedback: Element<'a, Message> = if let Some(feedback) = feedback {
        let (icon, message): (_, Cow<'a, str>) = match feedback {
            SettingsFeedback::Saved => ("emblem-ok-symbolic", "Saved to settings.toml".into()),
            SettingsFeedback::Invalid(message) => {
                ("dialog-warning-symbolic", message.as_str().into())
            }
            SettingsFeedback::Error(message) => (
                "dialog-error-symbolic",
                format!("Could not save settings: {message}").into(),
            ),
        };
        widget::row::with_children(vec![
            settings_icon(icon, progress).into(),
            widget::text::caption(message)
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into(),
        ])
        .align_y(Alignment::Center)
        .spacing(2.0 * design::GRID_UNIT)
        .width(Length::Fill)
        .into()
    } else {
        widget::text::caption(
            "Changes save automatically. Font family and shortcuts apply on Enter.",
        )
        .wrapping(iced::widget::text::Wrapping::Word)
        .width(Length::Fill)
        .into()
    };
    let reset: Element<'a, Message> = widget::button::custom(
        widget::row::with_children(vec![
            settings_icon("edit-undo-symbolic", progress).into(),
            widget::text::body("Reset to defaults").into(),
        ])
        .align_y(Alignment::Center)
        .spacing(8),
    )
    .name("Reset to defaults")
    .on_press(Message::ResetSettings)
    .into();
    let footer: Element<'a, Message> = if narrow {
        widget::column::with_children(vec![feedback, reset])
            .spacing(2.0 * design::GRID_UNIT)
            .width(Length::Fill)
            .into()
    } else {
        widget::row::with_children(vec![feedback, reset])
            .align_y(Alignment::Center)
            .spacing(3.0 * design::GRID_UNIT)
            .width(Length::Fill)
            .into()
    };
    let header = widget::container(
        widget::row::with_children(vec![
            widget::text::title4("Settings").width(Length::Fill).into(),
            widget::button::custom(settings_icon("window-close-symbolic", progress))
                .id(settings_close_id())
                .name("Close settings")
                .width(design::PRIMARY_CONTROL_SIZE)
                .height(design::PRIMARY_CONTROL_SIZE)
                .padding(design::GRID_UNIT)
                .on_press(Message::CloseInspector)
                .into(),
        ])
        .align_y(Alignment::Center)
        .height(design::HEADER_HEIGHT),
    )
    .padding([2.0 * design::GRID_UNIT, gutter])
    .width(Length::Fill);
    let surface: Element<'a, Message> = widget::container(
        widget::column::with_children(vec![
            header.into(),
            widget::divider::horizontal::light().into(),
            body,
            widget::divider::horizontal::light().into(),
            widget::container(footer)
                .padding([3.0 * design::GRID_UNIT, gutter])
                .width(Length::Fill)
                .into(),
        ])
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .class(cosmic::theme::Container::Dialog(false))
    .width(Length::Fill)
    .height(Length::Fill)
    .max_width(900.0)
    .max_height(760.0)
    .into();

    let progress = progress.clamp(0.0, 1.0);
    let margin = if narrow { 3.0 } else { 6.0 } * design::GRID_UNIT;
    let offset = 2.0 * design::GRID_UNIT * (1.0 - progress);
    let centered: Element<'a, Message> =
        widget::container(crate::interaction::fade(surface, progress))
            .center(Length::Fill)
            .padding(Padding {
                top: margin + offset,
                right: margin,
                bottom: margin - offset,
                left: margin,
            })
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    // This is an overlay, not another themed card. It dims both light and dark
    // workspaces, and captures pointer events outside the dialog without closing
    // it or discarding a partially entered draft.
    let scrim: Element<'a, Message> = iced::widget::opaque(
        widget::container(iced::widget::Space::new())
            .class(cosmic::theme::Container::custom(move |_| {
                iced::widget::container::Style {
                    background: Some(iced::Background::Color(iced::Color::from_rgba(
                        0.0,
                        0.0,
                        0.0,
                        0.48 * progress,
                    ))),
                    ..Default::default()
                }
            }))
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .into();
    iced::widget::Stack::with_children(vec![scrim, centered])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
