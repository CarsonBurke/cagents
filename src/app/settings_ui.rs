use super::*;
use crate::settings_widgets::{Group, Setting, field as setting_field, group as settings_group};

fn monospace_font_setting<'a>(family: &str, field: SettingsField) -> Setting<'a, Message> {
    static FAMILIES: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
        let fonts = cosmic::iced::advanced::graphics::text::font_system()
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let mut names: Vec<_> = fonts
            .db()
            .faces()
            .filter(|face| face.monospaced)
            .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    });
    let mut values = vec![String::new()];
    values.extend(FAMILIES.iter().cloned());
    if !values.iter().any(|value| value == family) {
        values.push(family.to_owned());
    }
    let selected = values.iter().position(|value| value == family);
    let labels: Vec<_> = values
        .iter()
        .map(|value| {
            if value.is_empty() {
                "System monospace".to_owned()
            } else {
                value.clone()
            }
        })
        .collect();
    setting_field(
        "Installed monospace fonts",
        "Choose a fixed-pitch font installed on this computer.",
        crate::ui_controls::field::select(labels, selected, move |index| {
            Message::SettingsChanged {
                field,
                value: values[index].clone(),
            }
        })
        .into(),
    )
}

fn setting_input<'a>(
    detail: impl Into<Cow<'a, str>> + 'a,
    value: &'a str,
    field: SettingsField,
    invalid: &'a HashMap<SettingsField, String>,
) -> Setting<'a, Message> {
    let label = field.label();
    let error = invalid.get(&field).map(String::as_str);
    let mut input = crate::ui_controls::field::text(label, value)
        .on_input(move |value| Message::SettingsDraftChanged { field, value })
        .width(Length::Fill);
    if let Some(error) = error {
        input = input.error(error);
    }
    setting_field(label, detail, input.into()).error(error)
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
        crate::ui_controls::icon_button(Icon::Minus, format!("Decrease {label}"))
            .on_press_maybe((current > min).then(|| change(current - step)))
            .into(),
        widget::container(design::body(format!("{}{}", current, unit)))
            .center_x(72)
            .center_y(design::PRIMARY_CONTROL_SIZE)
            .into(),
        crate::ui_controls::icon_button(Icon::Plus, format!("Increase {label}"))
            .on_press_maybe((current < max).then(|| change(current + step)))
            .into(),
        crate::ui_controls::icon_button(Icon::Undo, format!("Reset {label}"))
            .on_press_maybe((current != default).then(|| change(default)))
            .into(),
    ])
    .spacing(space::XS)
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
                settings_icon(Icon::Select).into()
            } else {
                iced::widget::Space::new().width(16).height(16).into()
            };
            crate::interaction::hover(
                widget::button::custom(
                    widget::row::with_children(vec![marker, design::body(choice).into()])
                        .spacing(space::S)
                        .align_y(Alignment::Center),
                )
                .name(choice)
                .class(crate::sidebar::sidebar_button_style(selected))
                .selected(selected)
                .padding([space::XS, space::S])
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
            .spacing(space::XS)
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
            "Automatic uses a signed-in plan and switches when its allowance runs out. Choosing an account pins new conversations and terminals; project choices take precedence.",
        ),
        _ => (
            "ChatGPT (Codex)",
            "Automatic uses a signed-in plan and switches when its allowance runs out. Choosing an account pins new conversations and terminals; project choices take precedence.",
        ),
    };
    let selected = Some(
        choices
            .iter()
            .position(|choice| choice.selected)
            .map_or(0, |index| index + 1),
    );
    let labels =
        std::iter::once("Automatic — use another signed-in plan when exhausted".to_owned())
            .chain(choices.iter().map(|choice| choice.label.clone()))
            .collect::<Vec<_>>();
    let ids = std::iter::once(None)
        .chain(choices.iter().map(|choice| Some(choice.id.clone())))
        .collect::<Vec<_>>();
    setting_field(
        label,
        detail,
        crate::ui_controls::field::select(labels, selected, move |index| {
            Message::AccountUi(match &ids[index] {
                Some(id) => account_ui::Action::SetDefault(id.clone()),
                None => account_ui::Action::SetAutomatic(provider),
            })
        })
        .into(),
    )
}

/// What the settings pane shows besides the draft being edited, gathered
/// from the rest of the application once per frame.
pub(super) struct SettingsContext<'a> {
    pub feedback: Option<&'a SettingsFeedback>,
    /// Refused values, by the field that refused them.
    pub invalid: &'a HashMap<SettingsField, String>,
    pub accounts: Vec<(crate::agent::Provider, Vec<account_ui::DefaultChoice>)>,
    pub updates: update_ui::UpdateSummary,
    pub testing_notification: bool,
}

fn settings_category_controls<'a>(
    category: SettingsCategory,
    draft: &'a SettingsDraft,
    context: &SettingsContext<'a>,
) -> Vec<Group<'a, Message>> {
    let invalid = context.invalid;
    let testing_notification = context.testing_notification;
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
                    "Notify when an agent needs input or approval, or its outcome could not be confirmed.",
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
                (
                    Field::MessagePreviews,
                    "Show message previews",
                    "Include conversation names and response or request excerpts on the desktop. These may be visible on a shared or locked screen. Off by default.",
                ),
            ];
            let mut controls: Vec<_> = fields
                .into_iter()
                .map(|(field, label, detail)| {
                    setting_field(
                        label,
                        detail,
                        crate::settings_widgets::toggle_enabled(
                            label,
                            draft.notifications.get(field),
                            matches!(field, Field::Enabled | Field::MessagePreviews)
                                || draft.notifications.get(Field::Enabled),
                            move |enabled| Message::SettingsChanged {
                                field: SettingsField::Notifications(field),
                                value: enabled.to_string(),
                            },
                        ),
                    )
                    .label_in_control()
                })
                .collect();
            controls.push(setting_field(
                "Test notification",
                "Your desktop controls presentation and sound. Unavailable delivery keeps attention inside cagents; the result appears in notices.",
                crate::ui_controls::button::secondary(if testing_notification {
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
                "Private alerts show the provider and folder. Message previews are available when you choose to enable them.",
                controls,
            )]
        }
        SettingsCategory::About => {
            let updates = &context.updates;
            let version = if crate::instance::NAME == "development" {
                format!("{} (development build)", crate::VERSION)
            } else {
                crate::VERSION.to_owned()
            };
            let status = design::body(updates.status.clone())
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill);
            let mut update_row: Vec<Element<'a, Message>> = vec![if updates.failed {
                status
                    .class(cosmic::theme::Text::Custom(design::destructive_text))
                    .into()
            } else {
                status.into()
            }];
            if let Some((_, url)) = &updates.available {
                update_row.push(update_ui::release_notes_button(url));
                update_row.push(
                    // `Apply` is refused while this window is blocked, so the
                    // button is disabled and the reason shown below it.
                    crate::ui_controls::button::primary("Review update")
                        .on_press_maybe(
                            updates
                                .apply_blocker
                                .is_none()
                                .then_some(Message::UpdateUi(update_ui::Action::Apply)),
                        )
                        .into(),
                );
            } else {
                update_row.push(
                    crate::ui_controls::button::secondary("Check for updates")
                        .id(widget::Id::new("settings.updates.check"))
                        .on_press_maybe(
                            updates
                                .can_check
                                .then_some(Message::UpdateUi(update_ui::Action::CheckNow)),
                        )
                        .into(),
                );
            }
            vec![settings_group(
                "cagents",
                "The build you are running and how it stays current.",
                vec![
                    setting_field(
                        "Window version",
                        "The application build running in this window.",
                        design::body(version).into(),
                    ),
                    setting_field(
                        "Updates",
                        match updates.apply_blocker {
                            Some(reason) => format!("{} {reason}", updates.detail),
                            None => updates.detail.clone(),
                        },
                        widget::row::with_children(update_row)
                            .align_y(Alignment::Center)
                            .spacing(space::S)
                            .width(Length::Fill)
                            .into(),
                    ),
                    setting_field(
                        SettingsField::CheckForUpdates.label(),
                        "Look for a new release at startup and every six hours. A check that cannot reach the server is retried quietly; only one you ask for reports a failure.",
                        crate::settings_widgets::toggle(
                            "Check for updates automatically",
                            draft.check_for_updates,
                            |enabled| Message::SettingsChanged {
                                field: SettingsField::CheckForUpdates,
                                value: enabled.to_string(),
                            },
                        ),
                    ),
                ],
            )]
        }
        SettingsCategory::Accounts => {
            let mut settings: Vec<Setting<'a, Message>> = context
                .accounts
                .iter()
                .filter(|(_, choices)| !choices.is_empty())
                .map(|(provider, choices)| default_account_choice(*provider, choices))
                .collect();
            settings.push(setting_field(
                "Manage accounts",
                "Add, rename, sign in to or remove accounts, and compare what each has left.",
                crate::ui_controls::button::secondary("Open Accounts")
                    .on_press(Message::AccountUi(account_ui::Action::OpenPane))
                    .into(),
            ));
            vec![settings_group(
                "Default accounts",
                "Each provider's CLI keeps every login in a home of its own. cagents never reads credentials.",
                settings,
            )]
        }
        SettingsCategory::General => {
            let reset = setting_field(
                "Reset all settings",
                "Restore default preferences; saved conversations and tabs are preserved.",
                crate::ui_controls::button::destructive("Reset all settings")
                    .id(widget::Id::new("settings.reset"))
                    .on_press(Message::ResetSettings)
                    .into(),
            );
            let directory = setting_input(
                "Absolute path, or relative to the repository root. Defaults to .worktrees/. Each new worktree gets its own subdirectory.",
                &draft.managed_worktree_directory,
                SettingsField::ManagedWorktreeDirectory,
                invalid,
            );
            let worktree_pane = settings_choice(
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
            );
            let cleanup = setting_field(
                "Merged worktree cleanup",
                "Delete clean, inactive GitHub worktrees after their exact current commit is merged. Cleanup waits until hosted processes have ended, preserves branches, and never discards changes.",
                crate::settings_widgets::toggle(
                    "Delete merged worktrees automatically",
                    draft.auto_delete_merged_worktrees,
                    |enabled| Message::SettingsChanged {
                        field: SettingsField::AutoDeleteMergedWorktrees,
                        value: enabled.to_string(),
                    },
                ),
            );
            let channel_pane = settings_choice(
                "Default channel pane",
                "Open this pane in an empty channel. An unused native chat is removed when you switch channels.",
                SettingsField::DefaultChannelPane,
                settings::DefaultPane::ALL
                    .map(|pane| (pane.label(), pane.key(), draft.default_channel_pane == pane)),
            );
            let order = settings_choice(
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
            );
            let activation = setting_field(
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
            );
            let hot_panes = setting_input(
                "At least 1 pane. Extra background panes may hibernate.",
                &draft.max_hot_cards,
                SettingsField::MaxHotCards,
                invalid,
            );
            let hibernation = setting_input(
                "Milliseconds before an inactive pane hibernates; 0 suspends immediately.",
                &draft.hibernation_delay_ms,
                SettingsField::HibernationDelayMs,
                invalid,
            );
            let stale = setting_input(
                "A card with no commits, file changes or agent work for this many days moves to Stale.",
                &draft.work_stale_after_days,
                SettingsField::WorkStaleAfterDays,
                invalid,
            );
            vec![
                settings_group(
                    "Worktrees",
                    "Locations, ordering and the first pane in a worktree.",
                    vec![directory, worktree_pane, cleanup, order],
                ),
                settings_group(
                    "Work board",
                    "How the board sorts work that has gone quiet.",
                    vec![stale],
                ),
                settings_group(
                    "Workspace",
                    "Channels and pane focus.",
                    vec![channel_pane, activation],
                ),
                settings_group(
                    "Performance",
                    "Balance active panes and background resource use.",
                    vec![hot_panes, hibernation],
                ),
                settings_group(
                    "Reset",
                    "Restore preferences without removing conversations or panes.",
                    vec![reset],
                ),
            ]
        }
        SettingsCategory::NativeChat => vec![settings_group(
            "Native chat",
            "Message composition and conversation presentation.",
            vec![
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
                setting_field(
                    "Tool groups",
                    "New tool-call groups start collapsed. Individual disclosure choices are remembered while paging.",
                    crate::settings_widgets::toggle(
                        "Expand tool groups by default",
                        draft.chat_expand_tool_groups,
                        |enabled| Message::SettingsChanged {
                            field: SettingsField::ChatExpandToolGroups,
                            value: enabled.to_string(),
                        },
                    ),
                ),
            ],
        )],
        SettingsCategory::Editor => vec![
            settings_group(
                "Font",
                "Leave a field blank to follow the terminal font. Ctrl+=, Ctrl+− and Ctrl+0 zoom the focused editor or terminal.",
                vec![
                    setting_input(
                        "Monospace font family, or blank to follow the terminal.",
                        &draft.editor_font_family,
                        SettingsField::EditorFontFamily,
                        invalid,
                    ),
                    setting_input(
                        "8–40 logical pixels, or blank to follow the terminal.",
                        &draft.editor_font_size,
                        SettingsField::EditorFontSize,
                        invalid,
                    ),
                ],
            ),
            settings_group("Editor", "Shared native editor and diff preferences.", {
                let mut options = crate::settings_widgets::editor_options(
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
                );
                options.push(setting_field(
                    "Hide unchanged diff lines",
                    "Fold unchanged context between hunks. Turn off to show the complete file.",
                    crate::settings_widgets::toggle(
                        "Hide unchanged diff lines",
                        draft.diff_hide_unchanged,
                        |enabled| Message::SettingsChanged {
                            field: SettingsField::DiffHideUnchanged,
                            value: enabled.to_string(),
                        },
                    ),
                ));
                options
            }),
        ],
        SettingsCategory::Appearance => vec![
            settings_group(
                "Interface",
                "Make the workbench comfortable on your display. Changes apply immediately and are saved automatically.",
                vec![
                    setting_field(
                        "App theme",
                        "Colors for the entire application. Terminal and syntax palettes are independent.",
                        crate::ui_controls::field::select(
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
                    setting_field(
                        "Worktree changes icons",
                        "Show a changes badge beside worktrees with uncommitted changes in Projects.",
                        crate::settings_widgets::toggle(
                            "Show worktree changes icons",
                            draft.show_worktree_changes_icons,
                            |enabled| Message::SettingsChanged {
                                field: SettingsField::ShowWorktreeChangesIcons,
                                value: enabled.to_string(),
                            },
                        ),
                    ),
                ],
            ),
            settings_group(
                "Workspace layout",
                "Keep navigation and context readable without giving up working space.",
                vec![
                    setting_stepper(
                        "New terminal/browser width",
                        "Fraction of the workspace. Existing pane widths are preserved.",
                        &draft.default_column_fraction,
                        SettingsField::DefaultColumnFraction,
                        0.25,
                        1.0,
                        0.05,
                        0.5,
                        "×",
                    ),
                    setting_stepper(
                        "Native pane minimum width",
                        "Fraction of the workspace used by new native panes when sharing space. A lone pane fills the workspace; resize or cycle its width to override.",
                        &draft.default_native_pane_fraction,
                        SettingsField::DefaultNativePaneFraction,
                        0.25,
                        1.0,
                        0.05,
                        0.5,
                        "",
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
                    setting_input(
                        "Leave empty for system monospace. Use an installed fixed-pitch family, including a Nerd Font for Powerline glyphs. Saves after you pause typing.",
                        &draft.terminal_font_family,
                        SettingsField::TerminalFontFamily,
                        invalid,
                    ),
                    monospace_font_setting(
                        &draft.terminal_font_family,
                        SettingsField::TerminalFontFamily,
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
                        "Breathing room between terminal text and the pane edge.",
                        &draft.terminal_padding,
                        SettingsField::TerminalPadding,
                        0.0,
                        24.0,
                        2.0,
                        8.0,
                        " px",
                    ),
                    setting_input(
                        "1–10 lines per mouse-wheel notch. Use scroll speed for high-resolution wheel or trackpad input.",
                        &draft.terminal_scroll_size,
                        SettingsField::TerminalScrollSize,
                        invalid,
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
                    setting_field(
                        "Multiline paste",
                        "Warn before pasting multiple lines when the program does not support bracketed paste.",
                        crate::settings_widgets::toggle(
                            "Confirm multiline paste",
                            draft.confirm_multiline_paste,
                            |enabled| Message::SettingsChanged {
                                field: SettingsField::ConfirmMultilinePaste,
                                value: enabled.to_string(),
                            },
                        ),
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
                    ),
                ],
            ),
            settings_group(
                "History",
                "Keep enough output to investigate a task without retaining unlimited terminal history.",
                vec![setting_input(
                    "0–1,000,000 lines per newly started terminal. Existing history is preserved.",
                    &draft.terminal_scrollback_lines,
                    SettingsField::TerminalScrollbackLines,
                    invalid,
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
                setting_input(
                    "Use {query} where the search terms belong, for example https://duckduckgo.com/?q={query}.",
                    &draft.browser_search_url,
                    SettingsField::BrowserSearchUrl,
                    invalid,
                ),
                #[cfg(feature = "integrated-browser")]
                setting_field(
                    "Browser logins",
                    "Choose sites from local browser profiles to import into this app session.",
                    crate::ui_controls::button::secondary("Import browser logins")
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
            "",
            vec![
                shortcut_input(
                    "Toggle Projects sidebar",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.toggle_projects,
                    SettingsField::ShortcutToggleProjects,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Rename focused pane",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.rename_pane,
                    SettingsField::ShortcutRenamePane,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Open command palette",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.command_palette,
                    SettingsField::ShortcutCommandPalette,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Choose chat account",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.chat_account,
                    SettingsField::ShortcutChatAccount,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Choose chat model",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.chat_model,
                    SettingsField::ShortcutChatModel,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Choose chat permission mode",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.chat_permissions,
                    SettingsField::ShortcutChatPermissions,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "New thread",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.new_native_thread,
                    SettingsField::ShortcutNewThread,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Toggle inspector",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.toggle_inspector,
                    SettingsField::ShortcutToggleInspector,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Open settings",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.open_settings,
                    SettingsField::ShortcutOpenSettings,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Move pane left",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.move_pane_left,
                    SettingsField::ShortcutMovePaneLeft,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Move pane right",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.move_pane_right,
                    SettingsField::ShortcutMovePaneRight,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Reopen closed pane",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.reopen_pane,
                    SettingsField::ShortcutReopenPane,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Focus pane 1",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.focus_pane_1,
                    SettingsField::ShortcutFocusPane1,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Focus pane 2",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.focus_pane_2,
                    SettingsField::ShortcutFocusPane2,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Focus pane 3",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.focus_pane_3,
                    SettingsField::ShortcutFocusPane3,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Focus pane 4",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.focus_pane_4,
                    SettingsField::ShortcutFocusPane4,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Focus pane 5",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.focus_pane_5,
                    SettingsField::ShortcutFocusPane5,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Focus pane 6",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.focus_pane_6,
                    SettingsField::ShortcutFocusPane6,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Focus pane 7",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.focus_pane_7,
                    SettingsField::ShortcutFocusPane7,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Focus pane 8",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.focus_pane_8,
                    SettingsField::ShortcutFocusPane8,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Focus pane 9",
                    "Leave blank to unbind.",
                    &draft.keyboard_shortcuts.focus_pane_9,
                    SettingsField::ShortcutFocusPane9,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "New terminal",
                    "Creates a terminal column.",
                    &draft.keyboard_shortcuts.new_terminal,
                    SettingsField::ShortcutNewTerminal,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "New channel",
                    "Creates an independent native conversation in the project's main worktree. Leave blank to unbind.",
                    &draft.keyboard_shortcuts.new_channel,
                    SettingsField::ShortcutNewChannel,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Clone focused pane",
                    "Opens an independent terminal or conversation, or the same browser or repository location. Leave blank to unbind.",
                    &draft.keyboard_shortcuts.clone_pane,
                    SettingsField::ShortcutClonePane,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Close focused column",
                    "Closes the active workspace column. A terminal running a program asks first while close confirmation is on.",
                    &draft.keyboard_shortcuts.close_focused_column,
                    SettingsField::ShortcutCloseFocusedColumn,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Search projects",
                    "Moves keyboard focus to project search from any pane.",
                    &draft.keyboard_shortcuts.search_projects,
                    SettingsField::ShortcutSearchProjects,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Find in pane",
                    "Finds in the focused conversation, terminal, editor, or settings pane.",
                    &draft.keyboard_shortcuts.find_in_pane,
                    SettingsField::ShortcutFindInPane,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Previous column",
                    "Moves focus left and may repeat while held.",
                    &draft.keyboard_shortcuts.previous_column,
                    SettingsField::ShortcutPreviousColumn,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Next column",
                    "Moves focus right and may repeat while held.",
                    &draft.keyboard_shortcuts.next_column,
                    SettingsField::ShortcutNextColumn,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Cycle pane width",
                    "Cycles the active pane through 1/3, 1/2, 2/3, and full viewport width; may repeat while held.",
                    &draft.keyboard_shortcuts.cycle_pane_width,
                    SettingsField::ShortcutCyclePaneWidth,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Previous workspace",
                    "Opens the worktree or channel before this one in the sidebar; may repeat while held.",
                    &draft.keyboard_shortcuts.previous_workspace,
                    SettingsField::ShortcutPreviousWorkspace,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Next workspace",
                    "Opens the worktree or channel after this one in the sidebar; may repeat while held.",
                    &draft.keyboard_shortcuts.next_workspace,
                    SettingsField::ShortcutNextWorkspace,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Mark reviewed file viewed",
                    "Toggles the reviewed file's viewed mark in the focused pull request pane and moves to the next file.",
                    &draft.keyboard_shortcuts.review_toggle_viewed,
                    SettingsField::ShortcutReviewToggleViewed,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Next reviewed file",
                    "Opens the next changed file in the focused pull request pane; may repeat while held.",
                    &draft.keyboard_shortcuts.review_next_file,
                    SettingsField::ShortcutReviewNextFile,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Previous reviewed file",
                    "Opens the previous changed file in the focused pull request pane; may repeat while held.",
                    &draft.keyboard_shortcuts.review_previous_file,
                    SettingsField::ShortcutReviewPreviousFile,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Next unviewed file",
                    "Opens the next unviewed file in the focused pull request pane; may repeat while held.",
                    &draft.keyboard_shortcuts.review_next_unviewed,
                    SettingsField::ShortcutReviewNextUnviewed,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Next changed hunk",
                    "Opens the next changed hunk in the focused pull request pane; may repeat while held.",
                    &draft.keyboard_shortcuts.review_next_change,
                    SettingsField::ShortcutReviewNextChange,
                    &draft.keyboard_shortcuts,
                ),
                shortcut_input(
                    "Previous changed hunk",
                    "Opens the previous changed hunk in the focused pull request pane; may repeat while held.",
                    &draft.keyboard_shortcuts.review_previous_change,
                    SettingsField::ShortcutReviewPreviousChange,
                    &draft.keyboard_shortcuts,
                ),
            ],
        )],
    }
}

fn category_icon(category: SettingsCategory) -> Icon {
    match category {
        SettingsCategory::Notifications => Icon::NotificationSettings,
        SettingsCategory::General => Icon::Settings,
        SettingsCategory::Appearance => Icon::Display,
        SettingsCategory::Terminal => Icon::Terminal,
        SettingsCategory::Links => Icon::WebBrowser,
        SettingsCategory::Keyboard => Icon::Keyboard,
        SettingsCategory::NativeChat => Icon::Chat,
        SettingsCategory::Editor => Icon::TextEditor,
        SettingsCategory::Accounts => Icon::Users,
        SettingsCategory::About => Icon::Info,
    }
}

fn settings_icon(name: Icon) -> widget::icon::Icon {
    name.handle().icon().size(16)
}

fn category_button<'a>(
    item: SettingsCategory,
    selected: SettingsCategory,
    refused: bool,
) -> Element<'a, Message> {
    let mut row = vec![
        settings_icon(category_icon(item)).into(),
        // One line, cut short in a pane too narrow for the full
        // label; the button's accessible name keeps all of it.
        design::body(item.label())
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                cosmic::iced::advanced::text::EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill)
            .into(),
    ];
    if refused {
        // Points back to a refused value from wherever the user went next.
        row.push(refused_marker());
    }
    crate::interaction::hover(
        widget::button::custom(
            widget::row::with_children(row)
                .align_y(Alignment::Center)
                .spacing(space::S),
        )
        .name(if refused {
            Cow::Owned(format!(
                "{}, has a setting that was not saved",
                item.label()
            ))
        } else {
            Cow::Borrowed(item.label())
        })
        .class(crate::sidebar::sidebar_button_style(item == selected))
        .selected(item == selected)
        .on_press(Message::SelectSettingsCategory(item))
        .padding([space::XS, space::S])
        .height(design::ROW_HEIGHT)
        .width(Length::Fill),
    )
}

/// A small dot in the error tone, beside a category holding a refused value.
fn refused_marker<'a>() -> Element<'a, Message> {
    widget::container(iced::widget::Space::new().width(6).height(6))
        .class(cosmic::theme::Container::custom(|theme| {
            cosmic::widget::container::Style {
                background: Some(design::destructive_color(theme).into()),
                border: cosmic::iced::Border {
                    radius: 3.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        }))
        .into()
}

/// Whether the last edits reached the settings file, beside the page title.
fn save_status<'a>(
    feedback: Option<&'a SettingsFeedback>,
    refused: usize,
) -> Option<Element<'a, Message>> {
    let refused_text = |text: String| {
        design::caption(text)
            .class(cosmic::theme::Text::Custom(design::destructive_text))
            .into()
    };
    Some(match feedback? {
        SettingsFeedback::Saved => design::caption("Saved").into(),
        SettingsFeedback::Pending => design::caption("Saving…").into(),
        SettingsFeedback::Invalid => refused_text(match refused {
            1 => "1 setting not saved".to_owned(),
            count => format!("{count} settings not saved"),
        }),
        SettingsFeedback::Error(_) => refused_text("Not saved".to_owned()),
    })
}

/// Card width below which the category rail folds above the settings. The
/// rail needs its own width plus a readable column beside it.
const RAIL_BREAKPOINT: f32 = 560.0;
/// Card width below which two category columns no longer fit their labels,
/// so the picker collapses to a single dropdown under the search field.
const RAIL_WIDTH: f32 = 42.0 * design::GRID_UNIT;

/// The settings pane's body. Its layout follows the card's width, which the
/// user sets per card, not the window's.
pub(super) fn settings_pane<'a>(
    category: SettingsCategory,
    draft: &'a SettingsDraft,
    query: &'a str,
    context: SettingsContext<'a>,
) -> Element<'a, Message> {
    let pane: Element<'a, Message> = crate::ui_controls::responsive(move |size| {
        settings_layout(category, draft, query, &context, size.width)
    })
    .into();
    pane.map(account_ui::pane_message)
}

fn settings_layout<'a>(
    category: SettingsCategory,
    draft: &'a SettingsDraft,
    query: &'a str,
    context: &SettingsContext<'a>,
    width: f32,
) -> Element<'a, Message> {
    let (feedback, invalid) = (context.feedback, context.invalid);
    let narrow = width < RAIL_BREAKPOINT;
    let refused = |item: SettingsCategory| invalid.keys().any(|field| field.category() == item);
    let searching = !query.trim().is_empty();
    let gutter = if narrow {
        space::M
    } else {
        5.0 * design::GRID_UNIT
    };

    let mut search = crate::ui_controls::field::search("Search settings", query)
        .id(crate::settings_widgets::search_id())
        .on_input(Message::SettingsSearchChanged)
        .width(Length::Fill);
    if !query.is_empty() {
        search = search.trailing_icon(crate::ui_controls::field::clear(
            Message::SettingsSearchChanged(String::new()),
        ));
    }
    let navigation: Element<'a, Message> = if narrow {
        let mut rows: Vec<Element<'a, Message>> = vec![search.into()];
        // Search results span every category, so the picker gives its room
        // to them while a query is active.
        if !searching {
            let labels = SettingsCategory::ALL.map(SettingsCategory::label).to_vec();
            let selected = SettingsCategory::ALL
                .iter()
                .position(|item| *item == category);
            rows.push(
                crate::ui_controls::field::select(labels, selected, |index| {
                    Message::SelectSettingsCategory(SettingsCategory::ALL[index])
                })
                .width(Length::Fill)
                .into(),
            );
        }
        widget::container(
            widget::column::with_children(rows)
                .spacing(space::XS)
                .width(Length::Fill),
        )
        .padding([space::S, gutter])
        .width(Length::Fill)
        .into()
    } else {
        let categories = widget::scrollable(
            widget::container(
                widget::column::with_children(
                    SettingsCategory::ALL
                        .into_iter()
                        .map(|item| category_button(item, category, refused(item))),
                )
                .spacing(space::XS)
                .width(Length::Fill),
            )
            .padding(design::with_vertical_rail([space::XS, space::S].into()))
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
                    top: space::M,
                    right: space::S,
                    bottom: 0.0,
                    left: space::S,
                })
                .into(),
            categories.into(),
        ])
        .spacing(space::XS)
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
            settings_category_controls(category, draft, context)
                .into_iter()
                .filter_map(|group| group.view(category.label(), filter)),
        );
    }
    if results.is_empty() {
        results.push(design::body("No matching settings").into());
    }
    if let Some(SettingsFeedback::Error(message)) = feedback {
        // Not any one field's fault, so it leads the page rather than
        // sitting under a control.
        results.insert(
            0,
            design::body(format!("Settings could not be saved: {message}"))
                .class(cosmic::theme::Text::Custom(design::destructive_text))
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into(),
        );
    }
    let mut header = vec![
        design::heading(if searching {
            "Search results"
        } else {
            category.label()
        })
        .width(Length::Fill)
        .into(),
    ];
    header.extend(save_status(feedback, invalid.len()));
    let header = widget::container(
        widget::container(
            widget::row::with_children(header)
                .align_y(Alignment::Center)
                .spacing(space::S)
                .width(Length::Fill),
        )
        .max_width(CONTENT_MAX_WIDTH),
    )
    .padding(iced::Padding {
        top: space::M,
        right: gutter,
        bottom: 0.0,
        left: gutter,
    })
    .width(Length::Fill);
    let scroll = widget::scrollable(
        widget::container(
            widget::column::with_children(results)
                .spacing(5.0 * design::GRID_UNIT)
                .width(Length::Fill)
                .max_width(CONTENT_MAX_WIDTH),
        )
        .padding([space::M, gutter])
        .width(Length::Fill),
    )
    .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
        crate::design::scrollbar(),
    ))
    .id(crate::settings_widgets::content_id())
    .class(cosmic::style::iced::Scrollable::Minimal)
    .width(Length::Fill)
    .height(Length::Fill);
    // The title and save state stay put while the page scrolls under them.
    let content: Element<'a, Message> =
        widget::column::with_children(vec![header.into(), scroll.into()])
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
        crate::ui_controls::field::select(labels, index, move |index| Message::SettingsChanged {
            field: SettingsField::TerminalTheme,
            value: choices[index].1.to_owned(),
        })
        .into(),
    )
}

fn shortcut_input<'a>(
    label: &'static str,
    detail: impl Into<Cow<'a, str>>,
    value: &'a str,
    field: SettingsField,
    draft: &ShortcutDraft,
) -> Setting<'a, Message> {
    let action = field
        .shortcut_action()
        .expect("shortcut setting has command");
    let chords = settings::parse_chords(value);
    let mut controls: Vec<Element<'a, Message>> = chords
        .iter()
        .enumerate()
        .map(|(index, chord)| {
            let mut remaining = chords.clone();
            remaining.remove(index);
            widget::container(
                widget::row::with_children(vec![
                    design::body(chord.clone()).into(),
                    crate::ui_controls::icon_button(
                        Icon::Close,
                        format!("Remove {chord} shortcut for {label}"),
                    )
                    .on_press(Message::SettingsDraftChanged {
                        field,
                        value: settings::format_chords(&remaining),
                    })
                    .into(),
                ])
                .spacing(space::XS)
                .align_y(Alignment::Center),
            )
            .padding([0.0, space::XS])
            .class(design::surface(
                Some(design::raised),
                [design::RADIUS_S; 4],
                false,
            ))
            .into()
        })
        .collect();
    let record: Element<'a, Message> =
        crate::ui_controls::icon_button(Icon::Keyboard, format!("Record shortcut for {label}"))
            .on_press(Message::Palette(palette_ui::Action::Record(field)))
            .into();
    let default = workspace_widget::shortcut_chords(action, &KeyboardShortcuts::default());
    let reset: Element<'a, Message> =
        crate::ui_controls::icon_button(Icon::Undo, format!("Reset {label} shortcut"))
            .on_press_maybe((value != default).then(|| Message::SettingsDraftChanged {
                field,
                value: default,
            }))
            .into();
    controls.push(
        widget::row::with_children(vec![record, reset])
            .spacing(space::XS)
            .into(),
    );
    let mut message = None;
    for chord in &chords {
        match settings::parse_shortcut(chord) {
            Err(error) => {
                message = Some(error.to_owned());
                break;
            }
            Ok(binding) => {
                if let Some((other, _)) =
                    draft.shortcut_values().into_iter().find(|(other, value)| {
                        *other != action
                            && settings::parse_chords(value).iter().any(|chord| {
                                settings::parse_shortcut(chord)
                                    .is_ok_and(|candidate| candidate == binding)
                            })
                    })
                {
                    message = Some(format!(
                        "Conflicts with {}",
                        workspace_widget::shortcut_descriptor(other).label
                    ));
                    break;
                }
            }
        }
    }
    let mut children = vec![
        widget::flex_row(controls)
            .spacing(space::XS as u16)
            .align_items(Alignment::Center)
            .into(),
    ];
    if let Some(message) = message {
        children.push(
            design::caption(message)
                .class(cosmic::theme::Text::Color(design::warning_color(
                    &cosmic::theme::active(),
                )))
                .into(),
        );
    }
    setting_field(
        label,
        detail,
        widget::column::with_children(children)
            .spacing(space::XS)
            .into(),
    )
}
