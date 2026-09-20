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
            .on_input(move |value| Message::SettingsDraftChanged { field, value })
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
        design::caption(detail)
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
        crate::ui_controls::icon_control(
            crate::ui_controls::symbolic_icon("list-remove-symbolic"),
            format!("Decrease {label}"),
            false,
            current > min,
            (current > min).then(|| change(current - step)),
        )
        .into(),
        widget::container(widget::text::body(format!("{}{}", current, unit)))
            .center_x(72)
            .center_y(design::PRIMARY_CONTROL_SIZE)
            .into(),
        crate::ui_controls::icon_control(
            crate::ui_controls::symbolic_icon("list-add-symbolic"),
            format!("Increase {label}"),
            false,
            current < max,
            (current < max).then(|| change(current + step)),
        )
        .into(),
        crate::ui_controls::icon_control(
            crate::ui_controls::symbolic_icon("edit-undo-symbolic"),
            format!("Reset {label}"),
            false,
            current != default,
            (current != default).then(|| change(default)),
        )
        .into(),
    ])
    .spacing(design::GRID_UNIT)
    .align_y(Alignment::Center)
    .width(Length::Shrink)
    .into();
    widget::column::with_children(vec![
        widget::text::caption_heading(label).into(),
        design::caption(detail)
            .wrapping(iced::widget::text::Wrapping::Word)
            .width(Length::Fill)
            .into(),
        control,
    ])
    .spacing(design::GRID_UNIT)
    .width(Length::Fill)
    .into()
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
                .class(crate::sidebar::sidebar_button_style(selected))
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
            design::caption(description)
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
    .padding([design::GRID_UNIT, 0.0])
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
                    "Managed worktree directory",
                    "Absolute path, or relative to the repository root. Defaults to worktrees/. Each new worktree gets its own subdirectory.",
                    &draft.managed_worktree_directory,
                    SettingsField::ManagedWorktreeDirectory,
                    progress,
                ),
                settings_choice(
                    "Worktree order",
                    "Main stays first. Imported worktrees with unknown creation dates follow dated worktrees, ordered by path.",
                    SettingsField::WorktreeSort,
                    [
                        ("Newest", "newest", draft.worktree_sort == "newest"),
                        ("Oldest", "oldest", draft.worktree_sort == "oldest"),
                        (
                            "Alphabetical",
                            "alphabetical",
                            draft.worktree_sort == "alphabetical",
                        ),
                    ],
                    progress,
                ),
                setting_field(
                    "Pane activation",
                    "Activate the pane under the pointer without scrolling the workspace or pane contents.",
                    widget::toggler(draft.focus_follows_mouse)
                        .label("Activate panes on hover".to_owned())
                        .on_toggle(|enabled| Message::SettingsChanged {
                            field: SettingsField::FocusFollowsMouse,
                            value: enabled.to_string(),
                        })
                        .into(),
                ),
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
                setting_field(
                    "Editor auto-save",
                    "Save edited files after a pause. Conflicts and save errors still require your attention.",
                    widget::toggler(draft.editor_auto_save)
                        .label("Save files automatically".to_owned())
                        .on_toggle(|enabled| Message::SettingsChanged {
                            field: SettingsField::EditorAutoSave,
                            value: enabled.to_string(),
                        })
                        .into(),
                ),
                setting_field(
                    "Line wrapping",
                    "Wrap long lines to the editor width instead of scrolling horizontally.",
                    widget::toggler(draft.editor_wrap_lines)
                        .label("Wrap long lines".to_owned())
                        .on_toggle(|enabled| Message::SettingsChanged {
                            field: SettingsField::EditorWrapLines,
                            value: enabled.to_string(),
                        })
                        .into(),
                ),
                setting_field(
                    "Chat send key",
                    "Enter sends the message and Shift+Enter breaks the line. Turn this off to compose with Enter and send with Ctrl+Enter.",
                    widget::toggler(draft.chat_enter_sends)
                        .label("Enter sends a chat message".to_owned())
                        .on_toggle(|enabled| Message::SettingsChanged {
                            field: SettingsField::ChatEnterSends,
                            value: enabled.to_string(),
                        })
                        .into(),
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
                "Editor",
                "Syntax colors for native editors and the diff surface. Applies immediately to every open column.",
                vec![settings_choice(
                    "Syntax theme",
                    "Match app follows the interface color scheme, including later theme changes.",
                    SettingsField::EditorSyntaxTheme,
                    settings::EditorSyntaxTheme::ALL.iter().map(|theme| {
                        (
                            theme.label(),
                            theme.key(),
                            draft.editor_syntax_theme == *theme,
                        )
                    }),
                    progress,
                )],
            ),
            settings_group(
                "Workspace layout",
                "Keep navigation and context readable without giving up working space.",
                vec![setting_stepper(
                    "New column width",
                    "Fraction of the workspace. Existing column widths are preserved.",
                    &draft.default_column_fraction,
                    SettingsField::DefaultColumnFraction,
                    0.25,
                    1.0,
                    0.05,
                    0.5,
                    "×",
                )],
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
                    setting_input(
                        "Font family",
                        "Leave empty for system monospace. Use an installed fixed-pitch family, including a Nerd Font for Powerline glyphs. Saves after you pause typing.",
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
                    setting_input(
                        "Scroll size",
                        "Base lines moved per mouse-wheel notch. Use scroll speed for high-resolution wheel or trackpad input.",
                        &draft.terminal_scroll_size,
                        SettingsField::TerminalScrollSize,
                        progress,
                    ),
                    setting_stepper(
                        "Scroll speed",
                        "Multiplier for wheel scrolling. One keeps the configured scroll size unchanged.",
                        &draft.terminal_scroll_speed,
                        SettingsField::TerminalScrollSpeed,
                        0.25,
                        4.0,
                        0.25,
                        1.0,
                        "×",
                    ),
                    settings_choice(
                        "Color theme",
                        "Live parser colors, independent of the interface theme. Programs retain their own truecolor and palette overrides.",
                        SettingsField::TerminalTheme,
                        [
                            ("Dark", "dark", draft.terminal_theme == "dark"),
                            ("Light", "light", draft.terminal_theme == "light"),
                            (
                                "Solarized dark",
                                "solarized_dark",
                                draft.terminal_theme == "solarized_dark",
                            ),
                        ],
                        progress,
                    ),
                    settings_choice(
                        "Cursor shape",
                        "Bar is the default. Application follows program cursor requests; explicit shapes override requests without forcing a hidden cursor visible.",
                        SettingsField::TerminalCursorStyle,
                        [
                            (
                                "Application",
                                "application",
                                draft.terminal_cursor_style == "application",
                            ),
                            ("Block", "block", draft.terminal_cursor_style == "block"),
                            ("Bar", "beam", draft.terminal_cursor_style == "beam"),
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
                vec![setting_input(
                    "Scrollback lines",
                    "0–1,000,000 lines per newly started terminal. Existing history is preserved.",
                    &draft.terminal_scrollback_lines,
                    SettingsField::TerminalScrollbackLines,
                    progress,
                )],
            ),
            settings_group(
                "Closing",
                "Idle shells close immediately. Terminal identity is always checked.",
                vec![setting_field(
                    "Active processes",
                    "Ask before closing a terminal with a foreground program or background jobs.",
                    widget::toggler(draft.confirm_terminal_close)
                        .label("Confirm closing active processes".to_owned())
                        .on_toggle(|enabled| Message::SettingsChanged {
                            field: SettingsField::ConfirmTerminalClose,
                            value: enabled.to_string(),
                        })
                        .into(),
                )],
            ),
        ],
        SettingsCategory::Links => vec![settings_group(
            "Link handling",
            "Choose the default browser for links. Modified clicks explicitly choose either browser.",
            vec![
                settings_choice(
                    "Open links in",
                    "Regular clicks use this destination. Ctrl+click and Ctrl+Shift+click override it.",
                    SettingsField::LinkOpenDestination,
                    [
                        (
                            "In-app browser",
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
                    "System browser gesture",
                    "The other gesture opens the in-app browser. Both work regardless of the default above.",
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
            "Each row takes one or more chords separated by commas and saves after you pause typing. Use Ctrl, Alt, or Super, optionally with Shift, plus A-Z, 0-9, Left, or Right. Every chord must be unique.",
            vec![
                setting_input(
                    "New terminal",
                    "Creates a terminal column.",
                    &draft.keyboard_shortcuts.new_terminal,
                    SettingsField::ShortcutNewTerminal,
                    progress,
                ),
                setting_input(
                    "New channel",
                    "Creates an independent native conversation in the project's main worktree. Leave blank to unbind.",
                    &draft.keyboard_shortcuts.new_channel,
                    SettingsField::ShortcutNewChannel,
                    progress,
                ),
                setting_input(
                    "Clone focused pane",
                    "Opens an independent terminal or conversation, or the same browser or repository location. Leave blank to unbind.",
                    &draft.keyboard_shortcuts.clone_pane,
                    SettingsField::ShortcutClonePane,
                    progress,
                ),
                setting_input(
                    "Close focused column",
                    "Closes the active workspace column.",
                    &draft.keyboard_shortcuts.close_focused_column,
                    SettingsField::ShortcutCloseFocusedColumn,
                    progress,
                ),
                setting_input(
                    "Search projects",
                    "Moves keyboard focus to project search.",
                    &draft.keyboard_shortcuts.search_projects,
                    SettingsField::ShortcutSearchProjects,
                    progress,
                ),
                setting_input(
                    "Previous column",
                    "Moves focus left and may repeat while held.",
                    &draft.keyboard_shortcuts.previous_column,
                    SettingsField::ShortcutPreviousColumn,
                    progress,
                ),
                setting_input(
                    "Next column",
                    "Moves focus right and may repeat while held.",
                    &draft.keyboard_shortcuts.next_column,
                    SettingsField::ShortcutNextColumn,
                    progress,
                ),
                setting_input(
                    "Cycle pane width",
                    "Cycles the active pane through 1/3, 2/3, 1/2, and full viewport width; may repeat while held.",
                    &draft.keyboard_shortcuts.cycle_pane_width,
                    SettingsField::ShortcutCyclePaneWidth,
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
    crate::ui_controls::symbolic_icon(name)
        .icon()
        .size(16)
        .opacity(progress.clamp(0.0, 1.0))
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
        .class(crate::sidebar::sidebar_button_style(item == selected))
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
        widget::scrollable(
            widget::container(
                widget::column::with_children(
                    SettingsCategory::ALL
                        .into_iter()
                        .map(|item| category_button(item, category, progress)),
                )
                .spacing(design::GRID_UNIT)
                .width(Length::Fill),
            )
            .padding(design::with_vertical_rail(
                [3.0 * design::GRID_UNIT, 2.0 * design::GRID_UNIT].into(),
            ))
            .width(Length::Fill),
        )
        .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
            crate::design::scrollbar(),
        ))
        .class(cosmic::style::iced::Scrollable::Minimal)
        .width(Length::Fixed(42.0 * design::GRID_UNIT))
        .height(Length::Fill)
        .into()
    };

    let content: Element<'a, Message> = widget::scrollable(
        widget::container(
            widget::column::with_children(settings_category_controls(category, draft, progress))
                .spacing(5.0 * design::GRID_UNIT)
                .width(Length::Fill)
                .max_width(CONTENT_MAX_WIDTH),
        )
        .padding([4.0 * design::GRID_UNIT, gutter])
        .width(Length::Fill),
    )
    .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
        crate::design::scrollbar(),
    ))
    .class(cosmic::style::iced::Scrollable::Minimal)
    .width(Length::Fill)
    .height(Length::Fill)
    .into();
    let body: Element<'a, Message> = if narrow {
        widget::column::with_children(vec![
            navigation,
            crate::design::horizontal_rule().into(),
            content,
        ])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    } else {
        widget::row::with_children(vec![
            navigation,
            crate::design::vertical_rule().into(),
            content,
        ])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    };

    let feedback: Element<'a, Message> = if let Some(feedback) = feedback {
        let (icon, message): (_, Cow<'a, str>) = match feedback {
            SettingsFeedback::Saved => ("emblem-ok-symbolic", "Saved to settings.toml".into()),
            SettingsFeedback::Pending => (
                "document-save-symbolic",
                "Saving after you pause typing…".into(),
            ),
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
            design::caption(message)
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into(),
        ])
        .align_y(Alignment::Center)
        .spacing(2.0 * design::GRID_UNIT)
        .width(Length::Fill)
        .into()
    } else {
        crate::design::caption("Valid changes save automatically after you pause typing.")
            .wrapping(iced::widget::text::Wrapping::Word)
            .width(Length::Fill)
            .into()
    };
    // A destructive, irreversible action does not belong behind an unlabelled
    // 30px glyph whose tooltip needs a 1000ms dwell to appear. This control now
    // only raises the confirmation, which carries the destructive button.
    let reset = widget::button::standard("Reset all settings")
        .id(widget::Id::new("settings.reset"))
        .name("Reset all settings to defaults")
        .leading_icon(crate::ui_controls::symbolic_icon("edit-undo-symbolic"))
        .on_press(Message::ResetSettings)
        .into();
    let footer: Element<'a, Message> = widget::row::with_children(vec![feedback, reset])
        .align_y(Alignment::Center)
        .spacing(3.0 * design::GRID_UNIT)
        .width(Length::Fill)
        .into();
    let header = widget::container(
        widget::row::with_children(vec![
            widget::text::title4("Settings").width(Length::Fill).into(),
            crate::ui_controls::icon_control(
                crate::ui_controls::symbolic_icon("window-close-symbolic"),
                "Close settings",
                false,
                true,
                Some(Message::CloseInspector),
            )
            .id(settings_close_id())
            .into(),
        ])
        .align_y(Alignment::Center)
        .height(design::HEADER_HEIGHT),
    )
    .padding([2.0 * design::GRID_UNIT, gutter])
    .width(Length::Fill);
    let surface = dialog_ui::connected_surface(
        widget::column::with_children(vec![
            header.into(),
            crate::design::horizontal_rule().into(),
            body,
            crate::design::horizontal_rule().into(),
            widget::container(footer)
                .padding([3.0 * design::GRID_UNIT, gutter])
                .width(Length::Fill)
                .into(),
        ])
        .width(Length::Fill)
        .height(Length::Fill),
        900.0,
    );
    let surface = widget::container(surface)
        .max_width(900.0)
        .height(Length::Fill)
        .max_height(760.0);

    dialog_ui::overlay(surface.into(), Some(Message::CloseInspector), progress)
}
