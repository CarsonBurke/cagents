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
        options.push(setting_field("Executable", "Use an absolute path to a provider binary or wrapper. Detect searches PATH without starting the provider.", executable.into()).error(context.invalid.get(&field(AgentSetting::Executable)).map(String::as_str)).keywords(format!("agents.{}.executable", settings::agents::provider_key(provider))).keywords(configuration.executable.as_deref().unwrap_or("")));

        let mut arguments: Vec<Element<'a, Message>> = Vec::new();
        for (index, argument) in configuration.arguments.iter().enumerate() {
            let current = configuration.arguments.clone();
            let mut remaining = current.clone();
            remaining.remove(index);
            arguments.push(
                widget::row::with_children(vec![
                    crate::ui_controls::field::text("Argument", argument)
                        .on_input(move |value| {
                            let mut arguments = current.clone();
                            arguments[index] = value;
                            Message::SettingsDraftChanged {
                                field: SettingsField::Agent(provider, AgentSetting::Arguments),
                                value: serde_json::to_string(&arguments)
                                    .expect("argument strings serialize"),
                            }
                        })
                        .width(Length::Fill)
                        .into(),
                    crate::ui_controls::icon_button(Icon::Close, "Remove argument")
                        .on_press(Message::SettingsChanged {
                            field: field(AgentSetting::Arguments),
                            value: serde_json::to_string(&remaining)
                                .expect("argument strings serialize"),
                        })
                        .into(),
                ])
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
            );
        }
        let mut added = configuration.arguments.clone();
        added.push(String::new());
        arguments.push(
            crate::ui_controls::button::quiet("Add argument")
                .on_press(Message::SettingsChanged {
                    field: field(AgentSetting::Arguments),
                    value: serde_json::to_string(&added).expect("argument strings serialize"),
                })
                .into(),
        );
        options.push(setting_field("Extra arguments", "One argument per row, in launch order. Spaces stay inside the argument; no shell parsing is performed.", widget::column::with_children(arguments).spacing(space::XS).into()).error(context.invalid.get(&field(AgentSetting::Arguments)).map(String::as_str)).keywords(format!("agents.{}.arguments", settings::agents::provider_key(provider))).keywords(configuration.arguments.join(" ")));

        let mut environment: Vec<Element<'a, Message>> = Vec::new();
        for (index, (name, value)) in entry.environment.iter().enumerate() {
            let names = entry.environment.clone();
            let values = names.clone();
            let mut remaining = names.clone();
            remaining.remove(index);
            environment.push(
                widget::row::with_children(vec![
                    crate::ui_controls::field::text("NAME", name)
                        .on_input(move |name| {
                            let mut entries = names.clone();
                            entries[index].0 = name;
                            Message::SettingsDraftChanged {
                                field: SettingsField::Agent(provider, AgentSetting::Environment),
                                value: serde_json::to_string(&entries)
                                    .expect("environment strings serialize"),
                            }
                        })
                        .width(Length::FillPortion(2))
                        .into(),
                    crate::ui_controls::field::text("Value", value)
                        .secure(true)
                        .on_input(move |value| {
                            let mut entries = values.clone();
                            entries[index].1 = value;
                            Message::SettingsDraftChanged {
                                field: SettingsField::Agent(provider, AgentSetting::Environment),
                                value: serde_json::to_string(&entries)
                                    .expect("environment strings serialize"),
                            }
                        })
                        .width(Length::FillPortion(3))
                        .into(),
                    crate::ui_controls::icon_button(Icon::Close, "Remove environment variable")
                        .on_press(Message::SettingsChanged {
                            field: field(AgentSetting::Environment),
                            value: serde_json::to_string(&remaining)
                                .expect("environment strings serialize"),
                        })
                        .into(),
                ])
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
            );
        }
        let mut added = entry.environment.clone();
        added.push((String::new(), String::new()));
        environment.push(
            crate::ui_controls::button::quiet("Add environment variable")
                .on_press(Message::SettingsDraftChanged {
                    field: field(AgentSetting::Environment),
                    value: serde_json::to_string(&added).expect("environment strings serialize"),
                })
                .into(),
        );
        options.push(setting_field("Environment", "Values are masked. Account home and credential isolation variables cannot be overridden. Changes apply to future launches.", widget::column::with_children(environment).spacing(space::XS).into()).error(context.invalid.get(&field(AgentSetting::Environment)).map(String::as_str)).keywords(format!("agents.{}.environment variables", settings::agents::provider_key(provider))).keywords(entry.environment.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(" ")));
        groups.push(settings_group(provider.label(), "Provider defaults and launch options. Existing conversations keep their recorded recipe.", options));
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

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
