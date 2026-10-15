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

fn argument_rows<'a>(provider: Provider, entry: &'a AgentDraft) -> Element<'a, Message> {
    let configuration = &entry.configuration;
    let mut arguments: Vec<(uuid::Uuid, Element<'a, Message>)> = Vec::new();
    let argument_owner = entry.argument_owner;
    for (index, argument) in configuration.arguments.iter().enumerate() {
        let row = entry.argument_rows[index];
        arguments.push((
            row,
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
            .align_y(Alignment::Center)
            .into(),
        ));
    }
    arguments.push((
        argument_owner,
        crate::ui_controls::button::quiet("Add argument")
            .on_press(Message::AgentListEdited {
                provider,
                owner: argument_owner,
                edit: AgentListEdit::AddArgument,
            })
            .into(),
    ));
    iced::widget::keyed_column(arguments)
        .spacing(space::XS)
        .into()
}

fn environment_rows<'a>(provider: Provider, entry: &'a AgentDraft) -> Element<'a, Message> {
    let mut environment: Vec<(uuid::Uuid, Element<'a, Message>)> = Vec::new();
    let environment_owner = entry.environment_owner;
    for (index, (name, value)) in entry.environment.iter().enumerate() {
        let row = entry.environment_rows[index];
        environment.push((
            row,
            widget::row::with_children(vec![
                crate::ui_controls::field::text("NAME", name)
                    .on_input(move |value| Message::AgentListEdited {
                        provider,
                        owner: environment_owner,
                        edit: AgentListEdit::EnvironmentName { row, value },
                    })
                    .width(Length::FillPortion(2))
                    .into(),
                crate::ui_controls::field::text("Value", value)
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
            .align_y(Alignment::Center)
            .into(),
        ));
    }
    environment.push((
        environment_owner,
        crate::ui_controls::button::quiet("Add environment variable")
            .on_press(Message::AgentListEdited {
                provider,
                owner: environment_owner,
                edit: AgentListEdit::AddEnvironment,
            })
            .into(),
    ));
    iced::widget::keyed_column(environment)
        .spacing(space::XS)
        .into()
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
    for provider in Provider::ALL {
        let entry = draft
            .agents
            .get(&provider)
            .expect("all provider drafts exist");
        let configuration = &entry.configuration;

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
        if let Some(reason) = &entry.launch_error {
            options.push(setting_field(
                "Launch unavailable",
                "Repair these preferences before starting a new conversation.",
                design::caption(reason.as_str())
                    .class(cosmic::theme::Text::Custom(design::destructive_text))
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
                .on_press(Message::DetectAgentExecutable(provider))
                .into(),
        ])
        .spacing(space::S)
        .align_y(Alignment::Center);
        options.push(setting_field("Executable", "Use an absolute path to a provider binary or wrapper. Detect searches the provider's configured PATH, or the shell PATH when absent, without starting the provider.", executable.into()).error(context.invalid.get(&field(AgentSetting::Executable)).map(String::as_str)).keywords(format!("agents.{}.executable", settings::agents::provider_key(provider))).keywords(configuration.executable.as_deref().unwrap_or("")));

        options.push(setting_field("Extra arguments", "One argument per row, in launch order. Spaces stay inside the argument; no shell parsing is performed.", argument_rows(provider, entry)).error(context.invalid.get(&field(AgentSetting::Arguments)).map(String::as_str)).keywords(format!("agents.{}.arguments", settings::agents::provider_key(provider))).keywords(configuration.arguments.join(" ")));

        options.push(setting_field("Environment", "Values are masked. Account home and credential isolation variables cannot be overridden. Changes apply to future launches.", environment_rows(provider, entry)).error(context.invalid.get(&field(AgentSetting::Environment)).map(String::as_str)).keywords(format!("agents.{}.environment variables", settings::agents::provider_key(provider))).keywords(entry.environment.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(" ")));
        groups.push(settings_group(provider.label(), "Provider defaults and launch options. Existing conversations keep their recorded recipe.", options));
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_agent_keyed_rows_keep_focus_on_the_survivor_not_its_old_position() {
        use cosmic::iced::advanced::{
            Layout, layout, renderer::Headless, widget::Tree, widget::operation,
        };
        struct Focused(Vec<String>);
        impl operation::Operation for Focused {
            fn traverse(&mut self, operation: &mut dyn FnMut(&mut dyn operation::Operation)) {
                operation(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                if state.is_focused()
                    && let Some(id) = id
                {
                    self.0.push(id.to_string());
                }
            }
        }
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
        for remove_focused in [false, true] {
            let draft = AgentDraft::new(
                settings::AgentLaunchSettings {
                    arguments: vec!["first".into(), "second".into()],
                    ..Default::default()
                },
                None,
            );
            let (removed, survivor) = (draft.argument_rows[0], draft.argument_rows[1]);
            let focused = if remove_focused { removed } else { survivor };
            let mut before = argument_rows(Provider::Claude, &draft);
            let mut tree = Tree::new(before.as_widget());
            let node = before.as_widget_mut().layout(&mut tree, &renderer, &limits);
            before.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut operation::focusable::focus::<()>(argument_id(focused)),
            );
            let mut initial = Focused(Vec::new());
            before
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut initial);
            assert_eq!(initial.0, [argument_id(focused).to_string()]);
            drop(before);
            let mut updated = draft.clone();
            let (_, value) = updated
                .edit_list(
                    updated.argument_owner,
                    AgentListEdit::RemoveArgument(removed),
                )
                .unwrap();
            updated.set_arguments(serde_json::from_str(&value).unwrap());
            let mut after = argument_rows(Provider::Claude, &updated);
            after.as_widget_mut().diff(&mut tree);
            let node = after.as_widget_mut().layout(&mut tree, &renderer, &limits);
            let mut final_focus = Focused(Vec::new());
            after.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut final_focus,
            );
            if remove_focused {
                assert!(
                    final_focus.0.is_empty(),
                    "removed field focus must not transfer to a different row"
                );
            } else {
                assert_eq!(
                    final_focus.0,
                    [argument_id(survivor).to_string()],
                    "survivor retains its own native state after moving"
                );
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
