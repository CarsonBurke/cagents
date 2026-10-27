//! The permissions surface is anchored to the footer so a blocked tool is
//! reachable even when its transcript entry has scrolled out of view.
use super::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Page {
    #[default]
    Blocked,
    Rules,
    Environment,
}

pub(super) struct Form {
    pub page: Page,
    pub rule: String,
    pub behavior: chat::PermissionBehavior,
    pub environment: String,
}

impl Default for Form {
    fn default() -> Self {
        Self {
            page: Page::Blocked,
            rule: String::new(),
            behavior: chat::PermissionBehavior::Allow,
            environment: String::new(),
        }
    }
}

fn source_label(source: &str) -> &str {
    match source {
        "flagSettings" => "This conversation",
        "userSettings" => "Account settings",
        "projectSettings" => "Project settings",
        "localSettings" => "Local project settings",
        "policySettings" | "managedSettings" => "Managed policy",
        _ => source,
    }
}

pub(super) fn retry_draft(denial: &chat::PermissionDenial) -> Option<String> {
    let input = denial.tool_input.as_ref()?;
    Some(format!(
        "I authorize retrying this exact {} action with the following input:\n{}\nRetry this action; if a permission rule or safety check still blocks it, explain which one.",
        denial.tool_name, input,
    ))
}

fn mode_description(mode: &str) -> &str {
    match mode {
        "manual" | "default" => "Review tool requests before allowing them",
        "acceptEdits" => "Allow file edits; review other tool requests",
        "auto" => "Claude reviews actions; explicit intent can clear soft blocks",
        "plan" => "Explore and plan before making changes",
        "dontAsk" => "Block actions that are not already allowed",
        "bypassPermissions" | "yolo" => "Skip approval checks",
        "readOnly" => "Read files without changing the workspace",
        "workspaceWrite" => "Allow changes inside the workspace",
        _ => "Provider permission policy",
    }
}

pub(super) fn notice(snapshot: &Snapshot) -> Option<String> {
    let approvals = snapshot
        .requests
        .iter()
        .filter(|request| request.kind == chat::RequestKind::Approval)
        .count();
    if approvals > 0 {
        return Some(if approvals == 1 {
            "Approval needed".into()
        } else {
            format!("{approvals} approvals needed")
        });
    }
    match snapshot.permission_denials.as_slice() {
        [] => None,
        [denial] => Some(format!(
            "{} blocked",
            Runtime::short_description(&denial.tool_name, 24)
        )),
        denials => Some(format!("{} permission blocks", denials.len())),
    }
}

fn page_control_id(card: CardId, page: Page) -> widget::Id {
    card_control_id(card, &format!("chat-permission-page-{page:?}"))
}

fn denial_control_id(target: Target, tool: &str, retry: bool) -> widget::Id {
    card_control_id(
        target.card,
        &format!(
            "chat-permission-{}-{}-{tool}",
            target.generation,
            if retry { "retry" } else { "dismiss" }
        ),
    )
}

impl Runtime {
    /// The enabled controls in the Blocked page's visual order. Settings
    /// pages use ordinary traversal through their editable fields instead.
    pub(super) fn permission_controls(&self, enabled: bool) -> Vec<widget::Id> {
        let card = self.target.card;
        let mut controls = vec![card_control_id(card, "chat-permission-close")];
        if self.session.launch.provider == Provider::Claude && !self.is_draft() {
            controls.extend(
                [Page::Blocked, Page::Rules, Page::Environment]
                    .map(|page| page_control_id(card, page)),
            );
        }
        if enabled {
            for request in self
                .snapshot
                .requests
                .iter()
                .filter(|request| request.kind == chat::RequestKind::Approval)
            {
                controls.extend(
                    view::ordered_answers(request)
                        .into_iter()
                        .map(|choice| view::answer_control_id(self.target, request, choice)),
                );
            }
            for denial in self.snapshot.permission_denials.iter().rev() {
                if denial.tool_input.is_some() {
                    controls.push(denial_control_id(self.target, &denial.tool_use_id, true));
                }
                controls.push(denial_control_id(self.target, &denial.tool_use_id, false));
            }
            if !(self.session.launch.provider == Provider::Codex && self.snapshot.busy) {
                controls.extend(
                    self.snapshot
                        .capabilities
                        .permission_modes
                        .iter()
                        .map(|mode| card_control_id(card, &format!("chat-permission-mode-{mode}"))),
                );
            }
        }
        controls
    }
    pub(super) fn permissions_view(&self, enabled: bool, width: f32) -> Element<'_, Message> {
        let target = self.target;
        let snapshot = &self.snapshot;
        let mut content = vec![
            widget::row::with_children(vec![
                design::body("Permissions").width(Length::Fill).into(),
                crate::ui_controls::icon_button(Icon::Close, "Close permissions")
                    .compact()
                    .id(card_control_id(target.card, "chat-permission-close"))
                    .on_press(action(Action::PermissionPicker(target, false)))
                    .tone(crate::design::Tone::Themed(crate::design::muted_icon_color))
                    .into(),
            ])
            .align_y(Alignment::Center)
            .into(),
        ];
        if let Some(error) = self.error.as_ref().or(snapshot.error.as_ref()) {
            content.push(
                design::caption(error.as_str())
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .into(),
            );
        }
        if self.pending.is_some() {
            content.push(design::caption("Waiting for provider confirmation…").into());
        }
        if self.session.launch.provider == Provider::Claude && !self.is_draft() {
            content.push(
                widget::row::with_children(
                    [
                        (Page::Blocked, "Blocked"),
                        (Page::Rules, "Rules"),
                        (Page::Environment, "Auto context"),
                    ]
                    .into_iter()
                    .map(|(page, label)| {
                        widget::button::custom(design::caption(label))
                            .id(page_control_id(target.card, page))
                            .class(crate::ui_controls::control_style(
                                self.permissions_form.page == page,
                                None,
                            ))
                            .on_press(action(Action::PermissionPage(target, page)))
                            .into()
                    })
                    .collect::<Vec<_>>(),
                )
                .spacing(step::GAP_XS)
                .wrap()
                .into(),
            );
        }
        if self.permissions_form.page != Page::Blocked {
            content.extend(self.permission_settings_view(enabled));
            return permissions_surface(content, width, target.card);
        }
        let approvals: Vec<_> = snapshot
            .requests
            .iter()
            .filter(|request| request.kind == chat::RequestKind::Approval)
            .collect();
        for request in approvals {
            content.push(view::request_view_with_redirect(
                target,
                request,
                &self.answers,
                enabled,
                view::RequestSurface::Permissions,
                (self.session.launch.provider == Provider::Claude)
                    .then(|| !self.composer.text().trim().is_empty()),
            ));
        }
        for denial in snapshot.permission_denials.iter().rev() {
            let mut details = vec![
                design::body(format!("{} blocked", denial.tool_name)).into(),
                view::body(if denial.reason.is_empty() {
                    "Claude did not provide a denial reason."
                } else {
                    denial.reason.as_str()
                }),
            ];
            if let Some(agent) = &denial.agent_id {
                details.push(design::caption(format!("Subagent: {agent}")).into());
            }
            if let Some(input) = &denial.tool_input {
                details.push(
                    widget::container(widget::scrollable(view::body(input.as_str())))
                        .max_height(140)
                        .into(),
                );
            } else {
                details.push(design::caption("Exact input is unavailable. Review the tool in the transcript before authorizing a retry.").into());
            }
            details.push(
                widget::row::with_children(vec![
                    widget::button::custom(design::caption("Review retry"))
                        .id(denial_control_id(target, &denial.tool_use_id, true))
                        .class(crate::ui_controls::control_style(false, None))
                        .on_press_maybe((enabled && denial.tool_input.is_some()).then(|| {
                            action(Action::PermissionRetry(target, denial.tool_use_id.clone()))
                        }))
                        .into(),
                    widget::button::custom(design::caption("Dismiss"))
                        .id(denial_control_id(target, &denial.tool_use_id, false))
                        .class(crate::ui_controls::control_style(false, None))
                        .on_press_maybe(enabled.then(|| {
                            action(Action::PermissionDismiss(
                                target,
                                denial.tool_use_id.clone(),
                            ))
                        }))
                        .into(),
                ])
                .spacing(step::GAP_S)
                .into(),
            );
            content.push(
                widget::container(widget::column::with_children(details).spacing(step::GAP_S))
                    .padding(step::GAP_S)
                    .class(cosmic::theme::Container::Card)
                    .into(),
            );
        }
        if !snapshot.permission_denials.is_empty() {
            content.push(design::caption("Review retry adds the exact action to your draft. Send it to authorize a retry. Deny rules and hard safety blocks still apply.").into());
        }
        if !snapshot.capabilities.permission_modes.is_empty() {
            content.push(design::caption("Permission mode").into());
            for mode in &snapshot.capabilities.permission_modes {
                content.push(
                    widget::button::custom(
                        widget::column::with_children(vec![
                            design::body(view::permission_mode_label(mode)).into(),
                            design::caption(mode_description(mode))
                                .class(cosmic::theme::Text::Custom(design::muted_text))
                                .into(),
                        ])
                        .spacing(step::GAP_XS),
                    )
                    .id(card_control_id(
                        target.card,
                        &format!("chat-permission-mode-{mode}"),
                    ))
                    .width(Length::Fill)
                    .class(crate::ui_controls::control_style(
                        snapshot.permission_mode.as_ref() == Some(mode),
                        None,
                    ))
                    .on_press_maybe(
                        (enabled
                            && !(self.session.launch.provider == Provider::Codex && snapshot.busy))
                            .then(|| action(Action::PermissionMode(target, mode.clone()))),
                    )
                    .into(),
                );
            }
        }
        permissions_surface(content, width, target.card)
    }

    fn permission_settings_view(&self, enabled: bool) -> Vec<Element<'_, Message>> {
        let target = self.target;
        let form = &self.permissions_form;
        let mut content = vec![design::caption("Changes apply to this running conversation. Rules from account, project and managed settings remain in effect.").into()];
        content.push(
            widget::button::custom(design::caption("Refresh settings"))
                .class(crate::ui_controls::control_style(false, None))
                .on_press_maybe(
                    enabled.then(|| action(Action::Command(target, Command::RefreshPermissions))),
                )
                .into(),
        );
        let Some(settings) = &self.snapshot.permissions else {
            content.push(design::caption("Permission settings have not loaded.").into());
            return content;
        };
        if form.page == Page::Rules {
            content.push(design::caption("Rules request Allow, Ask or Deny for matching tools. Deny and Ask take precedence; Auto may still review shell commands.").into());
            content.push(
                widget::row::with_children(
                    [
                        chat::PermissionBehavior::Allow,
                        chat::PermissionBehavior::Ask,
                        chat::PermissionBehavior::Deny,
                    ]
                    .into_iter()
                    .map(|behavior| {
                        widget::button::custom(design::caption(behavior.as_str()))
                            .class(crate::ui_controls::control_style(
                                form.behavior == behavior,
                                None,
                            ))
                            .on_press(action(Action::PermissionBehavior(target, behavior)))
                            .into()
                    })
                    .collect::<Vec<_>>(),
                )
                .spacing(step::GAP_XS)
                .into(),
            );
            content.push(
                crate::ui_controls::field::text(
                    "e.g. Bash(npm test) or WebFetch(domain:example.com)",
                    form.rule.as_str(),
                )
                .on_input(move |text| action(Action::PermissionRuleText(target, text)))
                .into(),
            );
            content.push(
                widget::button::custom(design::caption("Add rule for this conversation"))
                    .class(crate::ui_controls::control_style(false, None))
                    .on_press_maybe((enabled && !form.rule.trim().is_empty()).then(|| {
                        action(Action::PermissionSetting(
                            target,
                            chat::PermissionSettingsUpdate::AddRule {
                                behavior: form.behavior,
                                rule: form.rule.trim().into(),
                            },
                        ))
                    }))
                    .into(),
            );
            if settings.rules.is_empty() {
                content.push(design::caption("No explicit tool rules.").into());
            }
            for rule in &settings.rules {
                let mut row = vec![
                    widget::column::with_children(vec![
                        view::body(format!("{} · {}", rule.behavior.as_str(), rule.rule)),
                        design::caption(source_label(&rule.source))
                            .class(cosmic::theme::Text::Custom(design::muted_text))
                            .into(),
                    ])
                    .width(Length::Fill)
                    .into(),
                ];
                if rule.source == "flagSettings" {
                    row.push(
                        widget::button::custom(design::caption("Remove"))
                            .class(crate::ui_controls::control_style(false, None))
                            .on_press_maybe(enabled.then(|| {
                                action(Action::PermissionSetting(
                                    target,
                                    chat::PermissionSettingsUpdate::RemoveRule {
                                        behavior: rule.behavior,
                                        rule: rule.rule.clone(),
                                    },
                                ))
                            }))
                            .into(),
                    );
                }
                content.push(
                    widget::row::with_children(row)
                        .spacing(step::GAP_S)
                        .align_y(Alignment::Center)
                        .into(),
                );
            }
        } else {
            content.push(design::caption("Tell Auto which repositories, services or destinations belong to your environment. This adds context to its safety review; hard blocks still apply.").into());
            content.push(
                crate::ui_controls::field::text(
                    "e.g. Source control: github.com/my-org and its repositories",
                    form.environment.as_str(),
                )
                .on_input(move |text| action(Action::PermissionEnvironmentText(target, text)))
                .into(),
            );
            content.push(
                widget::button::custom(design::caption("Add context for this conversation"))
                    .class(crate::ui_controls::control_style(false, None))
                    .on_press_maybe((enabled && !form.environment.trim().is_empty()).then(|| {
                        action(Action::PermissionSetting(
                            target,
                            chat::PermissionSettingsUpdate::AddEnvironment {
                                text: form.environment.trim().into(),
                            },
                        ))
                    }))
                    .into(),
            );
            if settings.environment.is_empty() {
                content.push(design::caption("No custom environment context is listed.").into());
            }
            for entry in &settings.environment {
                let mut row = vec![
                    widget::column::with_children(vec![
                        view::body(entry.text.as_str()),
                        design::caption(source_label(&entry.source))
                            .class(cosmic::theme::Text::Custom(design::muted_text))
                            .into(),
                    ])
                    .width(Length::Fill)
                    .into(),
                ];
                if entry.source == "flagSettings" {
                    row.push(
                        widget::button::custom(design::caption("Remove"))
                            .class(crate::ui_controls::control_style(false, None))
                            .on_press_maybe(enabled.then(|| {
                                action(Action::PermissionSetting(
                                    target,
                                    chat::PermissionSettingsUpdate::RemoveEnvironment {
                                        text: entry.text.clone(),
                                    },
                                ))
                            }))
                            .into(),
                    );
                }
                content.push(
                    widget::row::with_children(row)
                        .spacing(step::GAP_S)
                        .align_y(Alignment::Center)
                        .into(),
                );
            }
        }
        content
    }
}

fn permissions_surface(
    content: Vec<Element<'_, Message>>,
    width: f32,
    card: CardId,
) -> Element<'_, Message> {
    widget::container(
        widget::scrollable(
            widget::column::with_children(content)
                .spacing(step::GAP_M)
                .padding(step::GAP_S),
        )
        .direction(iced::widget::scrollable::Direction::Vertical(
            design::scrollbar(),
        )),
    )
    .id(card_control_id(card, "chat-permissions-surface"))
    .width(width.clamp(1.0, 520.0))
    .max_height(440)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denial() -> chat::PermissionDenial {
        chat::PermissionDenial {
            tool_use_id: "blocked".into(),
            tool_name: "Bash".into(),
            tool_input: Some(serde_json::json!({"command":"git push origin topic"}).to_string()),
            reason: "Denied by Auto".into(),
            reason_type: Some("classifier".into()),
            agent_id: None,
        }
    }

    #[test]
    fn retry_requires_exact_input_and_never_runs_a_tool() {
        let mut blocked = denial();
        let draft = retry_draft(&blocked).unwrap();
        assert!(draft.contains("git push origin topic"));
        assert!(draft.starts_with("I authorize retrying this exact Bash action"));
        blocked.tool_input = None;
        assert_eq!(retry_draft(&blocked), None);
    }

    #[test]
    fn footer_keeps_blocks_visible_while_idle_or_busy_and_prioritizes_pending_approvals() {
        let mut snapshot = Snapshot {
            permission_denials: vec![denial()],
            ..Snapshot::default()
        };
        assert_eq!(notice(&snapshot).as_deref(), Some("Bash blocked"));
        snapshot.busy = true;
        assert_eq!(notice(&snapshot).as_deref(), Some("Bash blocked"));
        snapshot.requests.push(chat::Request {
            kind: crate::chat::RequestKind::Approval,
            id: "approval".into(),
            title: "Allow Bash?".into(),
            description: String::new(),
            fields: vec![],
            actions: vec![],
        });
        assert_eq!(notice(&snapshot).as_deref(), Some("Approval needed"));
        assert_eq!(notice(&Snapshot::default()), None);
        snapshot.permission_denials.clear();
        for kind in [chat::RequestKind::Question, chat::RequestKind::Consent] {
            snapshot.requests[0].kind = kind;
            assert_eq!(
                notice(&snapshot),
                None,
                "fieldless human prompts are not tool approvals"
            );
            assert!(!super::super::permissions_available(&snapshot));
        }
        snapshot.requests[0].kind = chat::RequestKind::Approval;
        snapshot.requests[0].fields.push(chat::Field {
            id: "scope".into(),
            label: "Scope".into(),
            description: String::new(),
            choices: vec![],
            multiple: false,
            secret: false,
            initial: String::new(),
        });
        assert_eq!(notice(&snapshot).as_deref(), Some("Approval needed"));
        assert!(super::super::permissions_available(&snapshot));
    }
}
