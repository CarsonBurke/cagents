use super::*;
use crate::settings_widgets::{Group, Setting, field as setting_field, group as settings_group};

fn setting_input<'a>(
    label: &'static str,
    detail: impl Into<Cow<'a, str>> + 'a,
    value: &'a str,
    field: SettingsField,
) -> Setting<'a, Message> {
    setting_field(
        label,
        detail,
        widget::text_input(label, value)
            .on_input(move |value| Message::SettingsDraftChanged { field, value })
            .width(Length::Fill)
            .into(),
    )
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
) -> Setting<'a, Message> {
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
    setting_field(label, detail, control)
}

fn settings_choice<'a>(
    label: &'static str,
    detail: &'static str,
    field: SettingsField,
    choices: impl IntoIterator<Item = (&'static str, &'static str, bool)>,
) -> Setting<'a, Message> {
    let buttons: Vec<Element<'a, Message>> = choices
        .into_iter()
        .map(|(choice, value, selected)| {
            let marker: Element<'a, Message> = if selected {
                settings_icon("object-select-symbolic").into()
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

/// One provider's default-account choice: the accounts it has, each marked
/// when it is the one new conversations start on.
fn default_account_choice<'a>(
    provider: crate::agent::Provider,
    choices: &[account_ui::DefaultChoice],
) -> Setting<'a, Message> {
    let (label, detail) = match provider {
        crate::agent::Provider::Claude => (
            "Claude",
            "The account new Claude conversations and shell terminals use unless the project has its own choice. Existing sessions keep their account.",
        ),
        _ => (
            "ChatGPT (Codex)",
            "The account new ChatGPT conversations and shell terminals use unless the project has its own choice. Existing sessions keep their account.",
        ),
    };
    let buttons: Vec<Element<'a, Message>> = choices
        .iter()
        .map(|choice| {
            let marker: Element<'a, Message> = if choice.selected {
                settings_icon("object-select-symbolic").into()
            } else {
                iced::widget::Space::new().width(16).height(16).into()
            };
            crate::interaction::hover(
                widget::button::custom(
                    widget::row::with_children(vec![
                        marker,
                        widget::text::body(choice.label.clone()).into(),
                    ])
                    .spacing(2.0 * design::GRID_UNIT)
                    .align_y(Alignment::Center),
                )
                .name(choice.label.clone())
                .class(crate::sidebar::sidebar_button_style(choice.selected))
                .selected(choice.selected)
                .padding([design::GRID_UNIT, 2.0 * design::GRID_UNIT])
                .height(design::ROW_HEIGHT)
                .width(Length::Fill)
                .on_press(Message::AccountUi(account_ui::Action::SetDefault(
                    choice.id.clone(),
                ))),
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

fn settings_category_controls<'a>(
    category: SettingsCategory,
    draft: &'a SettingsDraft,
    accounts: &[(crate::agent::Provider, Vec<account_ui::DefaultChoice>)],
    testing_notification: bool,
) -> Vec<Group<'a, Message>> {
    match category {
        SettingsCategory::Notifications => {
            use crate::notifications::Field;
            let fields = [
                (
                    Field::Enabled,
                    "System notifications",
                    "Show desktop alerts for agent lifecycle events, including while all windows are closed.",
                ),
                (
                    Field::Success,
                    "Successful completion",
                    "Notify when a provider confirms that a turn finished.",
                ),
                (
                    Field::Failure,
                    "Failure or cancellation",
                    "Notify when a turn fails or is cancelled.",
                ),
                (
                    Field::Waiting,
                    "Waiting for you",
                    "Notify when an agent needs input or approval.",
                ),
                (
                    Field::ReviewRequests,
                    "Review requests",
                    "Notify when GitHub requests your review, including requests discovered beyond the first page.",
                ),
                (
                    Field::WhileFocused,
                    "Notify while focused",
                    "Also notify while you are watching that agent. Off by default.",
                ),
            ];
            let mut controls: Vec<_> = fields
                .into_iter()
                .map(|(field, label, detail)| {
                    setting_field(
                        label,
                        detail,
                        crate::settings_widgets::toggle(
                            label,
                            draft.notifications.get(field),
                            move |enabled| Message::SettingsChanged {
                                field: SettingsField::Notifications(field),
                                value: enabled.to_string(),
                            },
                        ),
                    )
                })
                .collect();
            controls.push(setting_field(
                "Test notification",
                "Your desktop controls presentation and sound. Unavailable delivery keeps attention inside cagents; the result appears in notices.",
                widget::button::standard(if testing_notification {
                    "Sending test notification…"
                } else {
                    "Send test notification"
                })
                .id(widget::Id::new("settings.notifications.test"))
                .on_press_maybe(
                    (!testing_notification)
                        .then_some(Message::NotificationUi(notification_ui::Action::Test)),
                )
                .into(),
            ));
            vec![settings_group(
                "Agent notifications",
                "Alerts contain the provider and folder name, never prompts or output.",
                controls,
            )]
        }
        SettingsCategory::Accounts => {
            let mut settings: Vec<Setting<'a, Message>> = accounts
                .iter()
                .filter(|(_, choices)| !choices.is_empty())
                .map(|(provider, choices)| default_account_choice(*provider, choices))
                .collect();
            settings.push(setting_field(
                "Manage accounts",
                "Add, rename, sign in to or remove accounts, and compare what each has left.",
                widget::button::standard("Open Accounts")
                    .on_press(Message::AccountUi(account_ui::Action::OpenPane))
                    .into(),
            ));
            vec![settings_group(
                "Default accounts",
                "Each provider's CLI keeps every login in a home of its own. cagents never reads credentials.",
                settings,
            )]
        }
        SettingsCategory::General => vec![settings_group(
            "Runtime",
            "Control how many live columns cagents keeps active and when background work is suspended.",
            vec![
                setting_field(
                    "Reset all settings",
                    "Restore default preferences; saved conversations and tabs are preserved.",
                    widget::button::standard("Reset all settings")
                        .id(widget::Id::new("settings.reset"))
                        .on_press(Message::ResetSettings)
                        .into(),
                ),
                setting_input(
                    "Managed worktree directory",
                    "Absolute path, or relative to the repository root. Defaults to worktrees/. Each new worktree gets its own subdirectory.",
                    &draft.managed_worktree_directory,
                    SettingsField::ManagedWorktreeDirectory,
                ),
                settings_choice(
                    "Default worktree pane",
                    "Open this pane in an empty worktree. An unused native chat is removed when you switch worktrees.",
                    SettingsField::DefaultWorktreePane,
                    settings::DefaultPane::ALL.map(|pane| {
                        (
                            pane.label(),
                            pane.key(),
                            draft.default_worktree_pane == pane,
                        )
                    }),
                ),
                settings_choice(
                    "Default channel pane",
                    "Open this pane in an empty channel. An unused native chat is removed when you switch channels.",
                    SettingsField::DefaultChannelPane,
                    settings::DefaultPane::ALL
                        .map(|pane| (pane.label(), pane.key(), draft.default_channel_pane == pane)),
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
                ),
                setting_field(
                    "Pane activation",
                    "Focus a pane, without scrolling, once the pointer moves into it and stays in it for a moment. Typing before then keeps focus where it is.",
                    crate::settings_widgets::toggle(
                        "Activate panes on hover",
                        draft.focus_follows_mouse,
                        |enabled| Message::SettingsChanged {
                            field: SettingsField::FocusFollowsMouse,
                            value: enabled.to_string(),
                        },
                    ),
                ),
                setting_input(
                    "Maximum hot columns",
                    "Applies immediately; extra background columns may hibernate.",
                    &draft.max_hot_cards,
                    SettingsField::MaxHotCards,
                ),
                setting_input(
                    "Hibernation delay (ms)",
                    "Applies immediately to background columns.",
                    &draft.hibernation_delay_ms,
                    SettingsField::HibernationDelayMs,
                ),
                setting_field(
                    "Chat send key",
                    "Enter sends the message and Shift+Enter breaks the line. Turn this off to compose with Enter and send with Ctrl+Enter.",
                    crate::settings_widgets::toggle(
                        "Enter sends a chat message",
                        draft.chat_enter_sends,
                        |enabled| Message::SettingsChanged {
                            field: SettingsField::ChatEnterSends,
                            value: enabled.to_string(),
                        },
                    ),
                ),
            ],
        )],
        SettingsCategory::Editor => vec![settings_group(
            "Editor",
            "Shared native editor and diff preferences.",
            crate::settings_widgets::editor_options(
                draft.editor_auto_save,
                draft.editor_wrap_lines,
                draft.editor_syntax_theme,
                |enabled| Message::SettingsChanged {
                    field: SettingsField::EditorAutoSave,
                    value: enabled.to_string(),
                },
                |enabled| Message::SettingsChanged {
                    field: SettingsField::EditorWrapLines,
                    value: enabled.to_string(),
                },
                |theme| Message::SettingsChanged {
                    field: SettingsField::EditorSyntaxTheme,
                    value: theme.key().into(),
                },
            ),
        )],
        SettingsCategory::Appearance => vec![
            settings_group(
                "Interface",
                "Make the workbench comfortable on your display. Changes apply immediately and are saved automatically.",
                vec![
                    setting_field(
                        "App theme",
                        "Colors for the entire application. Terminal and syntax palettes are independent.",
                        widget::dropdown(
                            AppTheme::ALL
                                .iter()
                                .map(|theme| theme.label().to_owned())
                                .collect::<Vec<_>>(),
                            AppTheme::ALL
                                .iter()
                                .position(|theme| theme.key() == draft.app_theme),
                            |index| Message::SettingsChanged {
                                field: SettingsField::AppTheme,
                                value: AppTheme::ALL[index].key().into(),
                            },
                        )
                        .into(),
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
                    setting_field(
                        "Status bar",
                        "A thin bar along the bottom of the window showing how much of each signed-in Claude Code, Codex and Omp account's usage limits is left.",
                        crate::settings_widgets::toggle(
                            "Show the status bar",
                            draft.show_status_bar,
                            |enabled| Message::SettingsChanged {
                                field: SettingsField::ShowStatusBar,
                                value: enabled.to_string(),
                            },
                        ),
                    ),
                ],
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
                    terminal_theme_choice(&draft.terminal_theme, draft.has_custom_terminal_theme),
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
                )],
            ),
            settings_group(
                "Closing",
                "Idle shells close immediately. Terminal identity is always checked.",
                vec![setting_field(
                    "Active processes",
                    "Ask before closing a terminal with a foreground program or background jobs.",
                    crate::settings_widgets::toggle(
                        "Confirm closing active processes",
                        draft.confirm_terminal_close,
                        |enabled| Message::SettingsChanged {
                            field: SettingsField::ConfirmTerminalClose,
                            value: enabled.to_string(),
                        },
                    ),
                )],
            ),
            settings_group(
                "Upgrades",
                "Replacing the daemon checkpoints terminals and conversations, then resumes them.",
                vec![setting_field(
                    "Unresumable agents",
                    "Reopen an agent whose conversation cannot be resumed exactly in its resume picker, instead of refusing the upgrade. Other running jobs still block it.",
                    crate::settings_widgets::toggle(
                        "Relaunch agents on upgrade",
                        draft.relaunch_agents_on_upgrade,
                        |enabled| Message::SettingsChanged {
                            field: SettingsField::RelaunchAgentsOnUpgrade,
                            value: enabled.to_string(),
                        },
                    ),
                )],
            ),
        ],
        SettingsCategory::Links => vec![settings_group(
            "Link handling",
            "Choose the default browser for links. Modified clicks explicitly choose either browser.",
            vec![
                #[cfg(feature = "integrated-browser")]
                setting_field(
                    "Browser logins",
                    "Choose sites from local browser profiles to import into this app session.",
                    widget::button::standard("Import browser logins")
                        .on_press(Message::BrowserImport(browser_import_ui::Action::Open))
                        .into(),
                ),
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
                ),
                settings_choice(
                    "Open pull requests in",
                    "Where a pull request row in Projects opens. The review pane fetches the changes and tracks which files you have viewed.",
                    SettingsField::PullRequestOpenTarget,
                    [
                        (
                            "Review pane",
                            PullRequestOpenTarget::Review.as_str(),
                            draft.pull_request_open_target == PullRequestOpenTarget::Review,
                        ),
                        (
                            "Browser link",
                            PullRequestOpenTarget::Link.as_str(),
                            draft.pull_request_open_target == PullRequestOpenTarget::Link,
                        ),
                    ],
                ),
            ],
        )],
        SettingsCategory::Keyboard => vec![settings_group(
            "Keyboard shortcuts",
            "Each row takes one or more chords separated by commas and saves after you pause typing. Use Ctrl, Alt, or Super, optionally with Shift, plus A-Z, 0-9, Left, Right, Up, Down, Tab, PageUp, or PageDown. Every chord must be unique. Apart from Search projects and the review chords, a bound chord no longer reaches the program in a terminal, so prefer chords a shell leaves alone, such as Ctrl+Shift plus a letter.",
            vec![
                setting_input(
                    "New terminal",
                    "Creates a terminal column.",
                    &draft.keyboard_shortcuts.new_terminal,
                    SettingsField::ShortcutNewTerminal,
                ),
                setting_input(
                    "New channel",
                    "Creates an independent native conversation in the project's main worktree. Leave blank to unbind.",
                    &draft.keyboard_shortcuts.new_channel,
                    SettingsField::ShortcutNewChannel,
                ),
                setting_input(
                    "Clone focused pane",
                    "Opens an independent terminal or conversation, or the same browser or repository location. Leave blank to unbind.",
                    &draft.keyboard_shortcuts.clone_pane,
                    SettingsField::ShortcutClonePane,
                ),
                setting_input(
                    "Close focused column",
                    "Closes the active workspace column. A terminal running a program asks first while close confirmation is on.",
                    &draft.keyboard_shortcuts.close_focused_column,
                    SettingsField::ShortcutCloseFocusedColumn,
                ),
                setting_input(
                    "Search projects",
                    "Moves keyboard focus to project search, or finds in a focused editor. A focused terminal receives this chord instead.",
                    &draft.keyboard_shortcuts.search_projects,
                    SettingsField::ShortcutSearchProjects,
                ),
                setting_input(
                    "Previous column",
                    "Moves focus left and may repeat while held.",
                    &draft.keyboard_shortcuts.previous_column,
                    SettingsField::ShortcutPreviousColumn,
                ),
                setting_input(
                    "Next column",
                    "Moves focus right and may repeat while held.",
                    &draft.keyboard_shortcuts.next_column,
                    SettingsField::ShortcutNextColumn,
                ),
                setting_input(
                    "Cycle pane width",
                    "Cycles the active pane through 1/3, 1/2, 2/3, and full viewport width; may repeat while held.",
                    &draft.keyboard_shortcuts.cycle_pane_width,
                    SettingsField::ShortcutCyclePaneWidth,
                ),
                setting_input(
                    "Previous workspace",
                    "Opens the worktree or channel before this one in the sidebar; may repeat while held.",
                    &draft.keyboard_shortcuts.previous_workspace,
                    SettingsField::ShortcutPreviousWorkspace,
                ),
                setting_input(
                    "Next workspace",
                    "Opens the worktree or channel after this one in the sidebar; may repeat while held.",
                    &draft.keyboard_shortcuts.next_workspace,
                    SettingsField::ShortcutNextWorkspace,
                ),
                setting_input(
                    "Mark reviewed file viewed",
                    "Toggles the reviewed file's viewed mark in the focused pull request pane and moves to the next file.",
                    &draft.keyboard_shortcuts.review_toggle_viewed,
                    SettingsField::ShortcutReviewToggleViewed,
                ),
                setting_input(
                    "Next reviewed file",
                    "Opens the next changed file in the focused pull request pane; may repeat while held.",
                    &draft.keyboard_shortcuts.review_next_file,
                    SettingsField::ShortcutReviewNextFile,
                ),
                setting_input(
                    "Previous reviewed file",
                    "Opens the previous changed file in the focused pull request pane; may repeat while held.",
                    &draft.keyboard_shortcuts.review_previous_file,
                    SettingsField::ShortcutReviewPreviousFile,
                ),
                setting_input(
                    "Next unviewed file",
                    "Opens the next unviewed file in the focused pull request pane; may repeat while held.",
                    &draft.keyboard_shortcuts.review_next_unviewed,
                    SettingsField::ShortcutReviewNextUnviewed,
                ),
                setting_input(
                    "Next changed hunk",
                    "Opens the next changed hunk in the focused pull request pane; may repeat while held.",
                    &draft.keyboard_shortcuts.review_next_change,
                    SettingsField::ShortcutReviewNextChange,
                ),
                setting_input(
                    "Previous changed hunk",
                    "Opens the previous changed hunk in the focused pull request pane; may repeat while held.",
                    &draft.keyboard_shortcuts.review_previous_change,
                    SettingsField::ShortcutReviewPreviousChange,
                ),
            ],
        )],
    }
}

fn category_icon(category: SettingsCategory) -> &'static str {
    match category {
        SettingsCategory::Notifications => "preferences-system-notifications-symbolic",
        SettingsCategory::General => "preferences-system-symbolic",
        SettingsCategory::Appearance => "preferences-desktop-display-symbolic",
        SettingsCategory::Terminal => "utilities-terminal-symbolic",
        SettingsCategory::Links => "web-browser-symbolic",
        SettingsCategory::Keyboard => "preferences-desktop-keyboard-symbolic",
        SettingsCategory::Editor => "accessories-text-editor-symbolic",
        SettingsCategory::Accounts => "system-users-symbolic",
    }
}

fn settings_icon(name: &'static str) -> widget::icon::Icon {
    crate::ui_controls::symbolic_icon(name).icon().size(16)
}

fn category_button<'a>(item: SettingsCategory, selected: SettingsCategory) -> Element<'a, Message> {
    crate::interaction::hover(
        widget::button::custom(
            widget::row::with_children(vec![
                settings_icon(category_icon(item)).into(),
                // One line, cut short in a pane too narrow for the full
                // label; the button's accessible name keeps all of it.
                widget::text::body(item.label())
                    .wrapping(cosmic::iced::widget::text::Wrapping::None)
                    .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                        cosmic::iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
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

/// Card width below which the category rail folds above the settings. The
/// rail needs its own width plus a readable column beside it.
const RAIL_BREAKPOINT: f32 = 560.0;
/// Card width below which two category columns no longer fit their labels,
/// so the picker collapses to a single dropdown under the search field.
const GRID_BREAKPOINT: f32 = 340.0;
const RAIL_WIDTH: f32 = 42.0 * design::GRID_UNIT;

/// The settings pane's body. Its layout follows the card's width, which the
/// user sets per card, not the window's.
pub(super) fn settings_pane<'a>(
    category: SettingsCategory,
    draft: &'a SettingsDraft,
    query: &'a str,
    feedback: Option<&'a SettingsFeedback>,
    accounts: Vec<(crate::agent::Provider, Vec<account_ui::DefaultChoice>)>,
    testing_notification: bool,
) -> Element<'a, Message> {
    iced::widget::responsive(move |size| {
        settings_layout(
            category,
            draft,
            query,
            feedback,
            &accounts,
            testing_notification,
            size.width,
        )
    })
    .into()
}

fn settings_layout<'a>(
    category: SettingsCategory,
    draft: &'a SettingsDraft,
    query: &'a str,
    feedback: Option<&'a SettingsFeedback>,
    accounts: &[(crate::agent::Provider, Vec<account_ui::DefaultChoice>)],
    testing_notification: bool,
    width: f32,
) -> Element<'a, Message> {
    let narrow = width < RAIL_BREAKPOINT;
    let searching = !query.trim().is_empty();
    let gutter = if narrow { 3.0 } else { 5.0 } * design::GRID_UNIT;

    let mut search = widget::text_input::search_input("Search settings", query)
        .id(crate::settings_widgets::search_id())
        .on_input(Message::SettingsSearchChanged)
        .width(Length::Fill);
    if !query.is_empty() {
        search = search.on_clear(Message::SettingsSearchChanged(String::new()));
    }
    let navigation: Element<'a, Message> = if narrow {
        let mut rows: Vec<Element<'a, Message>> = vec![search.into()];
        // Search results span every category, so the picker gives its room
        // to them while a query is active.
        if !searching && width < GRID_BREAKPOINT {
            let labels = SettingsCategory::ALL.map(SettingsCategory::label).to_vec();
            let selected = SettingsCategory::ALL
                .iter()
                .position(|item| *item == category);
            rows.push(
                widget::dropdown(labels, selected, |index| {
                    Message::SelectSettingsCategory(SettingsCategory::ALL[index])
                })
                .width(Length::Fill)
                .into(),
            );
        } else if !searching {
            // Two columns keep every category readable without an
            // overflowing tab strip.
            rows.extend(SettingsCategory::ALL.chunks(2).map(|items| {
                let mut buttons: Vec<_> = items
                    .iter()
                    .map(|item| category_button(*item, category))
                    .collect();
                if items.len() == 1 {
                    buttons.push(iced::widget::Space::new().width(Length::Fill).into());
                }
                widget::row::with_children(buttons)
                    .spacing(design::GRID_UNIT)
                    .width(Length::Fill)
                    .into()
            }));
        }
        widget::container(
            widget::column::with_children(rows)
                .spacing(design::GRID_UNIT)
                .width(Length::Fill),
        )
        .padding([2.0 * design::GRID_UNIT, gutter])
        .width(Length::Fill)
        .into()
    } else {
        let categories = widget::scrollable(
            widget::container(
                widget::column::with_children(
                    SettingsCategory::ALL
                        .into_iter()
                        .map(|item| category_button(item, category)),
                )
                .spacing(design::GRID_UNIT)
                .width(Length::Fill),
            )
            .padding(design::with_vertical_rail(
                [design::GRID_UNIT, 2.0 * design::GRID_UNIT].into(),
            ))
            .width(Length::Fill),
        )
        .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
            crate::design::scrollbar(),
        ))
        .class(cosmic::style::iced::Scrollable::Minimal)
        .width(Length::Fill)
        .height(Length::Fill);
        widget::column::with_children(vec![
            widget::container(search)
                .padding(iced::Padding {
                    top: 3.0 * design::GRID_UNIT,
                    right: 2.0 * design::GRID_UNIT,
                    bottom: 0.0,
                    left: 2.0 * design::GRID_UNIT,
                })
                .into(),
            categories.into(),
        ])
        .spacing(design::GRID_UNIT)
        .width(Length::Fixed(RAIL_WIDTH))
        .height(Length::Fill)
        .into()
    };

    let categories: Vec<_> = if searching {
        SettingsCategory::ALL.to_vec()
    } else {
        vec![category]
    };
    let mut results = Vec::new();
    for category in categories {
        let filter = if crate::settings_widgets::matches(category.label(), query) {
            ""
        } else {
            query
        };
        results.extend(
            settings_category_controls(category, draft, accounts, testing_notification)
                .into_iter()
                .filter_map(|group| group.view(category.label(), filter)),
        );
    }
    if results.is_empty() {
        results.push(design::body("No matching settings").into());
    }
    if let Some(SettingsFeedback::Invalid(message) | SettingsFeedback::Error(message)) = feedback {
        results.insert(0, design::body(message.as_str()).into());
    }
    let content: Element<'a, Message> = widget::scrollable(
        widget::container(
            widget::column::with_children(results)
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
    if narrow {
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
    }
}

fn terminal_theme_choice<'a>(selected: &str, has_custom: bool) -> Setting<'a, Message> {
    let mut choices: Vec<(&str, &str)> = crate::terminal::TerminalTheme::PRESETS
        .iter()
        .map(|(_, label, id)| (*label, *id))
        .collect();
    if has_custom || selected == "custom" {
        choices.push(("Custom", "custom"));
    }
    let index = choices.iter().position(|(_, id)| *id == selected);
    let labels: Vec<String> = choices
        .iter()
        .map(|(label, _)| (*label).to_owned())
        .collect();
    setting_field(
        "Color theme",
        "Live terminal colors. Add a custom_terminal_theme table to settings.toml to create your own; application color overrides remain intact.",
        widget::dropdown(labels, index, move |index| Message::SettingsChanged {
            field: SettingsField::TerminalTheme,
            value: choices[index].1.to_owned(),
        })
        .into(),
    )
}
