use super::*;
use crate::agent::Provider;

fn select<'a>(
    provider: Provider,
    field: AgentSetting,
    selected: Option<&str>,
    offered: impl IntoIterator<Item = String>,
) -> Setting<'a, Message> {
    let mut values = vec![String::new()];
    for value in offered {
        if !values.contains(&value) {
            values.push(value);
        }
    }
    if let Some(selected) = selected
        && !values.iter().any(|value| value == selected)
    {
        values.push(selected.into());
    }
    let index = selected
        .and_then(|selected| values.iter().position(|value| value == selected))
        .or(Some(0));
    let labels = values
        .iter()
        .map(|value| {
            if value.is_empty() {
                "Provider default".to_owned()
            } else if field == AgentSetting::PermissionMode {
                crate::app::chat_ui::permission_mode_label(value).to_owned()
            } else {
                value.clone()
            }
        })
        .collect::<Vec<_>>();
    let keywords = format!("{} {}", labels.join(" "), values.join(" "));
    setting_field(
        field.label(),
        "Used for new conversations. Each composer can override this choice.",
        crate::ui_controls::field::select(labels, index, move |index| Message::SettingsChanged {
            field: SettingsField::Agent(provider, field),
            value: values[index].clone(),
        })
        .into(),
    )
    .keywords(format!(
        "agents.{}.{}",
        settings::agents::provider_key(provider),
        field.key()
    ))
    .keywords(keywords)
    .keywords(selected.unwrap_or(""))
}

fn argument_id(row: uuid::Uuid) -> widget::Id {
    widget::Id::new(format!("settings.agent.argument.{row}"))
}

fn row_id(kind: &str, row: uuid::Uuid) -> widget::Id {
    widget::Id::new(format!("settings.agent.{kind}.row.{row}"))
}

fn environment_id(row: uuid::Uuid, value: bool) -> widget::Id {
    widget::Id::new(format!(
        "settings.agent.environment.{}.{row}",
        if value { "value" } else { "name" }
    ))
}

fn argument_rows<'a>(provider: Provider, entry: &'a AgentDraft) -> Element<'a, Message> {
    let configuration = &entry.configuration;
    let mut arguments: Vec<Element<'a, Message>> = Vec::new();
    let argument_owner = entry.argument_owner;
    for (index, argument) in configuration.arguments.iter().enumerate() {
        let row = entry.argument_rows[index];
        arguments.push(
            widget::id_container(
                widget::row::with_children(vec![
                    crate::ui_controls::field::text("Argument", argument)
                        .id(argument_id(row))
                        .on_input(move |value| Message::AgentListEdited {
                            provider,
                            owner: argument_owner,
                            edit: AgentListEdit::Argument { row, value },
                        })
                        .width(Length::Fill)
                        .into(),
                    crate::ui_controls::icon_button(Icon::Close, "Remove argument")
                        .on_press(Message::AgentListEdited {
                            provider,
                            owner: argument_owner,
                            edit: AgentListEdit::RemoveArgument(row),
                        })
                        .into(),
                ])
                .spacing(space::S)
                .align_y(Alignment::Center),
                row_id("arguments", row),
            )
            .into(),
        );
    }
    arguments.push(
        widget::id_container(
            crate::ui_controls::button::quiet("Add argument").on_press(Message::AgentListEdited {
                provider,
                owner: argument_owner,
                edit: AgentListEdit::AddArgument,
            }),
            row_id("arguments.add", argument_owner),
        )
        .into(),
    );
    // Named children use the patched core's exact identity reconciliation,
    // including a remove-plus-add batch with no change in list length.
    widget::column::with_children(arguments)
        .spacing(space::XS)
        .into()
}

fn environment_rows<'a>(provider: Provider, entry: &'a AgentDraft) -> Element<'a, Message> {
    let mut environment: Vec<Element<'a, Message>> = Vec::new();
    let environment_owner = entry.environment_owner;
    for (index, (name, value)) in entry.environment.iter().enumerate() {
        let row = entry.environment_rows[index];
        environment.push(
            widget::id_container(
                widget::row::with_children(vec![
                    crate::ui_controls::field::text("NAME", name)
                        .id(environment_id(row, false))
                        .on_input(move |value| Message::AgentListEdited {
                            provider,
                            owner: environment_owner,
                            edit: AgentListEdit::EnvironmentName { row, value },
                        })
                        .width(Length::FillPortion(2))
                        .into(),
                    crate::ui_controls::field::text("Value", value)
                        .id(environment_id(row, true))
                        .secure(true)
                        .on_input(move |value| Message::AgentListEdited {
                            provider,
                            owner: environment_owner,
                            edit: AgentListEdit::EnvironmentValue { row, value },
                        })
                        .width(Length::FillPortion(3))
                        .into(),
                    crate::ui_controls::icon_button(Icon::Close, "Remove environment variable")
                        .on_press(Message::AgentListEdited {
                            provider,
                            owner: environment_owner,
                            edit: AgentListEdit::RemoveEnvironment(row),
                        })
                        .into(),
                ])
                .spacing(space::S)
                .align_y(Alignment::Center),
                row_id("environment", row),
            )
            .into(),
        );
    }
    environment.push(
        widget::id_container(
            crate::ui_controls::button::quiet("Add environment variable").on_press(
                Message::AgentListEdited {
                    provider,
                    owner: environment_owner,
                    edit: AgentListEdit::AddEnvironment,
                },
            ),
            row_id("environment.add", environment_owner),
        )
        .into(),
    );
    widget::column::with_children(environment)
        .spacing(space::XS)
        .into()
}

fn repair_control<'a>(
    provider: Provider,
    configuration: &settings::AgentLaunchSettings,
    observed: settings::agents::UnreadablePolicyReset,
    title: &'static str,
    action: String,
    detail: &'static str,
    feedback: Option<&SettingsFeedback>,
) -> Setting<'a, Message> {
    let key = if observed.all_providers() {
        "agents".into()
    } else if let Some(field) = observed.field() {
        format!(
            "agents.{}.{field}",
            settings::agents::provider_key(provider)
        )
    } else {
        format!("agents.{}", settings::agents::provider_key(provider))
    };
    let pending = configuration.repair_requested(&observed);
    let failed = matches!(feedback, Some(SettingsFeedback::Error(_)));
    let label = if pending && !failed {
        "Saving repair…".into()
    } else if pending {
        format!("Retry: {action}")
    } else {
        action
    };
    let id = widget::Id::new(format!(
        "settings.agent-repair.{}.{}",
        settings::agents::provider_key(provider),
        observed.field().unwrap_or("container")
    ));
    let control = widget::button::custom(
        design::heading(label.clone())
            .wrapping(iced::widget::text::Wrapping::Word)
            .width(Length::Fill),
    )
    .name(label)
    .id(id)
    .class(cosmic::theme::Button::Destructive)
    .padding([space::S, space::M])
    .width(Length::Fill)
    .on_press_maybe(
        (!pending || failed).then_some(Message::RepairUnreadableAgentPolicy { provider, observed }),
    );
    setting_field(title, detail, control.into()).keywords(key)
}

fn retained_arguments<'a>(entry: &'a AgentDraft) -> Setting<'a, Message> {
    let rows: Vec<Element<'a, Message>> = if entry.configuration.arguments.is_empty() {
        vec![design::body("No readable arguments.").into()]
    } else {
        entry
            .configuration
            .arguments
            .iter()
            .enumerate()
            .map(|(index, argument)| {
                design::body(format!(
                    "{}. {}",
                    index + 1,
                    if argument.is_empty() {
                        "(empty argument)"
                    } else {
                        argument
                    }
                ))
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into()
            })
            .collect()
    };
    setting_field(
        "Retained arguments",
        "Read only until repair. Removing unreadable entries keeps these arguments in this order.",
        widget::column::with_children(rows)
            .spacing(space::XS)
            .width(Length::Fill)
            .into(),
    )
}

fn retained_environment<'a>(entry: &'a AgentDraft) -> Setting<'a, Message> {
    let rows: Vec<Element<'a, Message>> = if entry.environment.is_empty() {
        vec![design::body("No readable environment entries.").into()]
    } else {
        entry
            .environment
            .iter()
            .map(|(name, _)| {
                design::body(format!("{name}  ••••••"))
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into()
            })
            .collect()
    };
    setting_field(
        "Retained environment",
        "Read only until repair. Values stay masked. Removing unreadable entries keeps these names and their saved values.",
        widget::column::with_children(rows)
            .spacing(space::XS)
            .width(Length::Fill)
            .into(),
    )
}

pub(super) fn controls<'a>(
    draft: &'a SettingsDraft,
    context: &SettingsContext<'a>,
) -> Vec<Group<'a, Message>> {
    let shell_error = draft.default_shell_unavailable.then_some(
        "Configured shell is unavailable. New terminals cannot start until this path is repaired.",
    );
    let mut groups = vec![settings_group(
        "Terminals",
        "Choose the shell used for a new terminal and terminal spawn without a command.",
        vec![
            setting_input(
                "Absolute executable path. Leave blank to use your login shell.",
                &draft.default_shell,
                SettingsField::DefaultShell,
                context.invalid,
            )
            .error(shell_error.or_else(|| {
                context
                    .invalid
                    .get(&SettingsField::DefaultShell)
                    .map(String::as_str)
            })),
        ],
    )];
    if let Some((provider, entry, observed)) = draft.agents.iter().find_map(|(&provider, entry)| {
        entry
            .configuration
            .unreadable_container()
            .filter(settings::agents::UnreadablePolicyReset::all_providers)
            .map(|observed| (provider, entry, observed))
    }) {
        groups.push(settings_group("Provider preferences", "Provider discovery and new conversations are unavailable until this section is repaired.", vec![repair_control(
            provider, &entry.configuration, observed, "Unreadable provider preferences",
            "Reset unreadable provider preferences".into(),
            "Reset only provider preferences to defaults: detected executables, no extra arguments or environment overrides, and automatic conversation defaults. Other app settings, accounts and existing conversations stay saved. Provider operations remain unavailable until the repair is saved.",
            context.feedback,
        )]));
        return groups;
    }
    for provider in Provider::ALL {
        let entry = draft
            .agents
            .get(&provider)
            .expect("all provider drafts exist");
        let configuration = &entry.configuration;
        if let Some(observed) = configuration.unreadable_container() {
            groups.push(settings_group(provider.label(), "Provider discovery and new conversations are unavailable until these preferences are repaired.", vec![repair_control(
                provider, configuration, observed, "Unreadable provider preferences",
                format!("Reset unreadable {} preferences", provider.label()),
                "Reset only this provider to a detected executable, no extra arguments or environment overrides, and automatic conversation defaults. Other providers, app settings, accounts and existing conversations stay saved. Provider operations remain unavailable until the repair is saved.",
                context.feedback,
            )]));
            continue;
        }

        let models = context
            .agent_models
            .iter()
            .find(|(candidate, _)| *candidate == provider)
            .map(|(_, models)| *models)
            .unwrap_or(&[]);
        let field = |field| SettingsField::Agent(provider, field);
        let mut options = vec![select(
            provider,
            AgentSetting::Model,
            configuration.model.as_deref(),
            models.iter().map(|model| model.id.clone()),
        )];
        let unavailable =
            "The configured executable is unavailable. Choose an executable path or use Detect.";
        if let Some(reason) = entry
            .launch_error
            .as_deref()
            .or(entry.executable_unavailable.then_some(unavailable))
        {
            options.push(setting_field(
                "Launch unavailable",
                "Repair these preferences before starting a new conversation.",
                design::caption(reason)
                    .class(cosmic::theme::Text::Custom(design::destructive_text))
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
            ));
        }
        let efforts = models
            .iter()
            .filter(|model| Some(model.id.as_str()) == configuration.model.as_deref())
            .flat_map(|model| model.reasoning.iter().cloned())
            .collect::<Vec<_>>();
        options.push(select(
            provider,
            AgentSetting::Effort,
            configuration.effort.as_deref(),
            efforts,
        ));
        let modes = settings::agents::permission_modes(provider);
        if !modes.is_empty() {
            options.push(select(
                provider,
                AgentSetting::PermissionMode,
                configuration.permission_mode.as_deref(),
                modes.iter().map(|mode| (*mode).into()),
            ));
        }
        if let Some(observed) = configuration.unreadable_field("executable") {
            options.push(repair_control(provider, configuration, observed, "Unreadable executable pin", "Use detected executable".into(), "Reset this unreadable saved executable pin. After the repair is saved, discovery and future launches may use the provider executable on PATH. Other launch preferences stay saved.", context.feedback));
        } else {
            let executable = widget::row::with_children(vec![
                crate::ui_controls::field::text(
                    "Use detected provider CLI",
                    configuration.executable.as_deref().unwrap_or(""),
                )
                .on_input(move |value| Message::SettingsDraftChanged {
                    field: SettingsField::Agent(provider, AgentSetting::Executable),
                    value,
                })
                .width(Length::Fill)
                .into(),
                crate::ui_controls::button::secondary("Detect")
                    .on_press_maybe(
                        configuration
                            .validate_readable_execution(provider)
                            .is_ok()
                            .then_some(Message::DetectAgentExecutable(provider)),
                    )
                    .into(),
            ])
            .spacing(space::S)
            .align_y(Alignment::Center);
            options.push(setting_field("Executable", "Use an absolute path to a provider binary or wrapper. Detect searches the provider's configured PATH, or the shell PATH when absent, without starting the provider.", executable.into()).error(context.invalid.get(&field(AgentSetting::Executable)).map(String::as_str)).keywords(format!("agents.{}.executable", settings::agents::provider_key(provider))).keywords(configuration.executable.as_deref().unwrap_or("")));
        }

        if let Some(observed) = configuration.unreadable_field("arguments") {
            options.push(retained_arguments(entry));
            options.push(repair_control(provider, configuration, observed, "Unreadable extra arguments", "Remove unreadable arguments".into(), "Remove only arguments that cannot be read. Valid saved arguments stay in their original order and can be edited after repair. Other launch preferences stay saved. Provider operations remain unavailable until the repair is saved.", context.feedback));
        } else {
            options.push(setting_field("Extra arguments", "One argument per row, in launch order. Spaces stay inside the argument; no shell parsing is performed.", argument_rows(provider, entry)).error(context.invalid.get(&field(AgentSetting::Arguments)).map(String::as_str)).keywords(format!("agents.{}.arguments", settings::agents::provider_key(provider))).keywords(configuration.arguments.join(" ")));
        }

        if let Some(observed) = configuration.unreadable_field("environment") {
            options.push(retained_environment(entry));
            options.push(repair_control(provider, configuration, observed, "Unreadable environment entries", "Remove unreadable environment entries".into(), "Remove only environment entries that cannot be read. Valid saved names and values stay saved and can be edited after repair. Other launch preferences stay saved. Provider operations remain unavailable until the repair is saved.", context.feedback));
        } else {
            options.push(setting_field("Environment", "Values are masked. Account home and credential isolation variables cannot be overridden. Changes apply to future launches.", environment_rows(provider, entry)).error(context.invalid.get(&field(AgentSetting::Environment)).map(String::as_str)).keywords(format!("agents.{}.environment variables", settings::agents::provider_key(provider))).keywords(entry.environment.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(" ")));
        }
        groups.push(settings_group(provider.label(), "Provider defaults and launch options. Existing conversations keep their recorded recipe.", options));
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_states(
        tree: &iced::advanced::widget::Tree,
    ) -> std::collections::BTreeMap<String, (bool, widget::text_input::Cursor)> {
        let mut states = std::collections::BTreeMap::new();
        fn visit(
            tree: &iced::advanced::widget::Tree,
            states: &mut std::collections::BTreeMap<String, (bool, widget::text_input::Cursor)>,
        ) {
            if tree.tag == iced::advanced::widget::tree::Tag::of::<widget::text_input::State>() {
                let state = tree.state.downcast_ref::<widget::text_input::State>();
                states.insert(
                    tree.id.as_ref().expect("explicit input ID").to_string(),
                    (state.is_focused(), state.cursor()),
                );
            }
            for child in &tree.children {
                visit(child, states);
            }
        }
        visit(tree, &mut states);
        states
    }

    #[test]
    fn settings_agent_named_rows_preserve_native_state_across_actual_reducer_batches() {
        use cosmic::iced::advanced::{
            Layout, layout, renderer::Headless, widget::Tree, widget::operation,
        };
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cosmic::Renderer::new(
                cosmic::iced::Font::MONOSPACE,
                iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .expect("headless renderer");
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(700.0, 500.0));
        // Cover each environment cell, with both focused-removal and focused-
        // survival. Messages are reduced without an intermediate widget diff.
        for mode in 0..3 {
            for remove_focused in [false, true] {
                for mutation in 0..3 {
                    let mut app = crate::app::tests::agent_list_test_app();
                    let provider = Provider::Claude;
                    let draft = app.settings_draft.agents[&provider].clone();
                    let (kind, rows, owner) = if mode == 0 {
                        ("arguments", &draft.argument_rows, draft.argument_owner)
                    } else {
                        (
                            "environment",
                            &draft.environment_rows,
                            draft.environment_owner,
                        )
                    };
                    let removed = rows[0];
                    let survivor = rows[1];
                    let input = |row| {
                        if mode == 0 {
                            argument_id(row)
                        } else {
                            environment_id(row, mode == 2)
                        }
                    };
                    let focused = input(if remove_focused { removed } else { survivor });
                    let mut before = if mode == 0 {
                        argument_rows(provider, &draft)
                    } else {
                        environment_rows(provider, &draft)
                    };
                    let mut tree = Tree::new(before.as_widget());
                    // UserInterface::build diffs even a fresh tree before its
                    // first layout. This initializes the native value paragraph
                    // before focus/caret operations and later reconciliation.
                    before.as_widget_mut().diff(&mut tree);
                    let node = before.as_widget_mut().layout(&mut tree, &renderer, &limits);
                    before.as_widget_mut().operate(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &mut operation::focusable::focus::<()>(focused.clone()),
                    );
                    // Assign distinguishable native caret state to every cell,
                    // not just the one whose focus survives a moved row.
                    for (index, row) in rows.iter().enumerate() {
                        for id in if mode == 0 {
                            vec![argument_id(*row)]
                        } else {
                            vec![environment_id(*row, false), environment_id(*row, true)]
                        } {
                            before.as_widget_mut().operate(
                                &mut tree,
                                Layout::new(&node),
                                &renderer,
                                &mut operation::text_input::move_cursor_to::<()>(id, index),
                            );
                        }
                    }
                    let original = input_states(&tree);
                    assert!(original[&focused.to_string()].0);
                    drop(before);
                    if mutation == 2 {
                        // The real external/reset adoption path replaces list
                        // owners even when all displayed values are equal.
                        app.settings_draft = SettingsDraft::from_settings(&app.settings);
                    } else {
                        let edit = if mode == 0 {
                            AgentListEdit::RemoveArgument(removed)
                        } else {
                            AgentListEdit::RemoveEnvironment(removed)
                        };
                        drop(app.update(Message::AgentListEdited {
                            provider,
                            owner,
                            edit,
                        }));
                        if mutation == 1 {
                            let edit = if mode == 0 {
                                AgentListEdit::AddArgument
                            } else {
                                AgentListEdit::AddEnvironment
                            };
                            drop(app.update(Message::AgentListEdited {
                                provider,
                                owner,
                                edit,
                            }));
                        }
                    }
                    let updated = &app.settings_draft.agents[&provider];
                    let (new_rows, new_owner) = if mode == 0 {
                        (&updated.argument_rows, updated.argument_owner)
                    } else {
                        (&updated.environment_rows, updated.environment_owner)
                    };
                    assert_eq!(new_rows.len(), if mutation == 0 { 1 } else { 2 });
                    let mut after = if mode == 0 {
                        argument_rows(provider, updated)
                    } else {
                        environment_rows(provider, updated)
                    };
                    after.as_widget_mut().diff(&mut tree);
                    let _ = after.as_widget_mut().layout(&mut tree, &renderer, &limits);
                    let expected_rows = new_rows
                        .iter()
                        .map(|row| row_id(kind, *row).to_string())
                        .chain(std::iter::once(
                            row_id(&format!("{kind}.add"), new_owner).to_string(),
                        ))
                        .collect::<Vec<_>>();
                    assert_eq!(
                        tree.children
                            .iter()
                            .map(|child| child.id.as_ref().unwrap().to_string())
                            .collect::<Vec<_>>(),
                        expected_rows,
                        "physical state order must equal current rendered row order"
                    );
                    let actual = input_states(&tree);
                    for (id, state) in &actual {
                        if mutation != 2
                            && let Some(previous) = original.get(id)
                        {
                            assert_eq!(
                                state, previous,
                                "surviving input keeps its own focus AND caret: {id}"
                            );
                        } else {
                            assert!(!state.0, "new row/list cannot inherit removed focus: {id}");
                            assert_eq!(
                                state.1,
                                widget::text_input::Cursor::default(),
                                "new row/list cannot inherit removed caret: {id}"
                            );
                        }
                    }
                    assert!(!actual.contains_key(&input(removed).to_string()));
                    let focused_ids = actual
                        .iter()
                        .filter(|(_, state)| state.0)
                        .map(|(id, _)| id.clone())
                        .collect::<Vec<_>>();
                    assert_eq!(
                        focused_ids,
                        if mutation != 2 && !remove_focused {
                            vec![input(survivor).to_string()]
                        } else {
                            Vec::new()
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn stale_provider_binary_does_not_hide_the_edited_environment_refusal() {
        let mut current = AppSettings::default();
        current.agents.insert(
            "claude".into(),
            settings::AgentLaunchSettings {
                executable: Some("/missing/pinned-provider".into()),
                ..Default::default()
            },
        );
        let field = SettingsField::Agent(Provider::Claude, AgentSetting::Environment);
        for (raw, expected) in [
            (r#"[["NAME","first"],["NAME","second"]]"#, "unique"),
            (r#"[["1INVALID","value"]]"#, "Environment names"),
            (r#"[["CAGENTS_AUTO_ACCOUNT","value"]]"#, "reserved"),
        ] {
            let SettingsDecision::Invalid(message) = settings_update(&current, field, raw) else {
                panic!("invalid environment must be refused");
            };
            assert!(message.contains(expected), "{message}");
            assert!(
                !message.contains("executable"),
                "an unchanged stale sibling must not replace the edited field's reason"
            );
        }
        assert!(matches!(
            settings_update(&current, field, r#"[["VALID_NAME","value"]]"#),
            SettingsDecision::Persist(_)
        ));
    }
}
