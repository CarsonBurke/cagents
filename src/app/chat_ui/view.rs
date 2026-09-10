use super::*;

fn action(action: Action) -> Message {
    Message::ChatUi(action)
}
fn icon<'a>(
    name: &'static str,
    label: impl Into<Cow<'a, str>>,
    enabled: bool,
    message: Action,
) -> Element<'a, Message> {
    crate::ui_controls::icon_control(
        widget::icon::from_name(name).handle(),
        label,
        false,
        enabled,
        Some(action(message)),
    )
    .into()
}
fn body<'a>(text: impl Into<Cow<'a, str>> + 'a) -> Element<'a, Message> {
    widget::selectable_text(text)
        .size(design::TEXT_BODY)
        .width(Length::Fill)
        .into()
}
fn command(target: Target, value: Command) -> Message {
    action(Action::Command(target, value))
}

impl Cagents {
    pub(in crate::app) fn chat_view(&self, card: CardId) -> Element<'_, Message> {
        match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => {
                runtime.view(self.chat_start_pending(card) || self.chat_ui.closed.contains(&card))
            }
            _ => widget::text::body("Connecting to native chat…").into(),
        }
    }
}

impl Runtime {
    fn view(&self, transitioning: bool) -> Element<'_, Message> {
        let target = self.target;
        let available =
            !transitioning && !self.disconnected && !self.snapshot.exited && self.pending.is_none();
        let snapshot = &self.snapshot;
        let mut transcript = Vec::new();
        if snapshot.has_earlier {
            transcript.push(
                widget::button::text("Load earlier history")
                    .on_press_maybe(
                        (!transitioning && !self.disconnected && self.pending.is_none())
                            .then_some(command(target, Command::LoadEarlier)),
                    )
                    .into(),
            );
        }
        if snapshot.items.is_empty() {
            transcript.push(body(if snapshot.detail.is_empty() {
                "Your conversation appears here. Send a message to begin."
            } else {
                &snapshot.detail
            }));
        }
        for item in &snapshot.items {
            let Some(cached) = self.cache.get(&item.id) else {
                continue;
            };
            transcript.push(self.item_view(item, cached));
        }
        let mut content = vec![
            widget::column::with_children(
                snapshot
                    .title
                    .iter()
                    .map(|title| design::heading(title.as_str()).into()),
            )
            .into(),
            widget::scrollable(
                widget::column::with_children(transcript)
                    .spacing(16)
                    .width(Length::Fill),
            )
            .id(card_control_id(target.card, "chat-transcript"))
            .anchor_bottom()
            .height(Length::Fill)
            .width(Length::Fill)
            .into(),
        ];
        if snapshot.has_later {
            content.push(
                widget::button::text("Jump to latest")
                    .on_press_maybe(
                        (!transitioning && !self.disconnected && self.pending.is_none())
                            .then_some(command(target, Command::Refresh)),
                    )
                    .into(),
            );
        }
        if transitioning {
            content.push(body(
                "Switching or closing this session… Editing and session controls are disabled. You can still read and copy the conversation.",
            ));
        }
        if let Some(error) = self.error.as_ref().or(snapshot.error.as_ref()) {
            content.push(body(error.as_str()));
        }
        if self.disconnected || snapshot.exited {
            content.push(
                widget::row::with_children(vec![
                    widget::text::caption(if snapshot.exited {
                        "Agent process exited. The transcript and draft are retained."
                    } else {
                        "Disconnected. The daemon may still be working; your draft is retained."
                    })
                    .width(Length::Fill)
                    .into(),
                    icon(
                        "view-refresh-symbolic",
                        "Reconnect to this session",
                        !transitioning
                            && self.pending.is_none()
                            && !self.attachment_chooser_pending,
                        Action::Reconnect(target),
                    ),
                ])
                .spacing(8)
                .align_y(Alignment::Center)
                .into(),
            );
        }
        if let Some((url, instructions)) = &snapshot.open_url {
            content.push(
                widget::column::with_children(vec![
                    body(instructions.as_str()),
                    widget::button::text("Open authorization page")
                        .on_press(action(Action::OpenUrl(url.clone())))
                        .into(),
                ])
                .spacing(4)
                .into(),
            );
        }
        // Provider requests stay on the native surface until their explicit resolution.
        // No action is synthesized from a default, a timeout, or closing a pane.
        if !snapshot.requests.is_empty() {
            content.push(
                widget::container(widget::scrollable(
                    widget::column::with_children(
                        snapshot
                            .requests
                            .iter()
                            .map(|request| self.request_view(request, available))
                            .collect::<Vec<_>>(),
                    )
                    .spacing(12),
                ))
                .max_height(260)
                .into(),
            );
        }
        let mut controls = Vec::new();
        if snapshot.capabilities.model_selection && !snapshot.models.is_empty() {
            let labels = snapshot
                .models
                .iter()
                .map(|model| model.label.clone())
                .collect::<Vec<_>>();
            let selected = snapshot
                .models
                .iter()
                .position(|model| Some(&model.id) == snapshot.selected_model.as_ref());
            let ids = snapshot
                .models
                .iter()
                .map(|model| model.id.clone())
                .collect::<Vec<_>>();
            controls.push(if transitioning {
                widget::button::standard(
                    selected.map_or_else(|| "Model".to_owned(), |index| labels[index].clone()),
                )
                .into()
            } else {
                widget::dropdown(labels, selected, move |index| {
                    action(Action::Model(target, ids[index].clone()))
                })
                .into()
            });
        }
        if snapshot.capabilities.reasoning_selection
            && let Some(model) = snapshot
                .models
                .iter()
                .find(|model| Some(&model.id) == snapshot.selected_model.as_ref())
            && !model.reasoning.is_empty()
        {
            let selected = model
                .reasoning
                .iter()
                .position(|reasoning| Some(reasoning) == snapshot.reasoning.as_ref());
            let reasoning = model.reasoning.clone();
            let model_id = model.id.clone();
            controls.push(if transitioning {
                widget::button::standard(
                    selected.map_or("Reasoning", |index| model.reasoning[index].as_str()),
                )
                .into()
            } else {
                widget::dropdown(model.reasoning.clone(), selected, move |index| {
                    action(Action::Reasoning(
                        target,
                        model_id.clone(),
                        reasoning[index].clone(),
                    ))
                })
                .into()
            });
        }
        if !snapshot.capabilities.permission_modes.is_empty() {
            let selected = snapshot
                .capabilities
                .permission_modes
                .iter()
                .position(|mode| Some(mode) == self.permission_mode.as_ref());
            let modes = snapshot.capabilities.permission_modes.clone();
            controls.push(if transitioning {
                widget::button::standard(selected.map_or("Permissions", |index| {
                    snapshot.capabilities.permission_modes[index].as_str()
                }))
                .into()
            } else {
                widget::dropdown(
                    snapshot.capabilities.permission_modes.clone(),
                    selected,
                    move |index| action(Action::PermissionMode(target, modes[index].clone())),
                )
                .into()
            });
        }
        if !controls.is_empty() {
            content.push(
                widget::row::with_children(controls)
                    .spacing(6)
                    .align_y(Alignment::Center)
                    .into(),
            );
        }
        let draft = self.composer.text();
        if let Some(provided) = &snapshot.composer
            && !provided.trim().is_empty()
            && !draft.contains(provided.as_str())
        {
            content.push(
                widget::button::text("Append provider's suggested draft")
                    .on_press_maybe(
                        (!transitioning).then_some(action(Action::AppendProviderDraft(target))),
                    )
                    .into(),
            );
        }
        if draft.trim_start().starts_with('/') && !snapshot.commands.is_empty() {
            let query = draft.trim().trim_start_matches('/');
            let commands = snapshot
                .commands
                .iter()
                .filter(|command| command.name.trim_start_matches('/').starts_with(query))
                .map(|command| {
                    widget::button::text(format!(
                        "/{}  {}",
                        command.name.trim_start_matches('/'),
                        command.description
                    ))
                    .on_press_maybe(
                        (!transitioning)
                            .then_some(action(Action::Slash(target, command.name.clone()))),
                    )
                    .into()
                })
                .collect::<Vec<Element<'_, Message>>>();
            if !commands.is_empty() {
                content.push(
                    widget::container(widget::scrollable(
                        widget::column::with_children(commands).spacing(0),
                    ))
                    .max_height(120)
                    .into(),
                );
            }
        }
        for (index, attachment) in self.attachments.iter().enumerate() {
            content.push(
                widget::row::with_children(vec![
                    widget::text::caption(attachment.path.display().to_string())
                        .width(Length::Fill)
                        .into(),
                    icon(
                        "edit-delete-symbolic",
                        "Remove attachment",
                        !transitioning,
                        Action::RemoveAttachment(target, index),
                    ),
                ])
                .align_y(Alignment::Center)
                .spacing(4)
                .into(),
            );
        }
        // Keep the named composer in a fixed sibling slot as provider controls,
        // requests, and attachments arrive. The pinned Iced child reconciler
        // finds named siblings but does not move their trees to new indices.
        let mut content = vec![
            widget::column::with_children(content)
                .spacing(8)
                .height(Length::Fill)
                .into(),
        ];
        content.push(if transitioning {
            widget::container(widget::scrollable(body(draft.clone())))
                .height(104)
                .width(Length::Fill)
                .padding(8)
                .into()
        } else {
            iced::widget::text_editor::TextEditor::new(&self.composer)
                .id(card_control_id(target.card, "chat-composer"))
                .placeholder("Message · Enter for a new line · use Send to submit")
                .size(design::TEXT_BODY)
                .height(104)
                .padding(8)
                .on_action(move |event| action(Action::Composer(target, event)))
                .into()
        });
        let has_draft = !draft.trim().is_empty() || !self.attachments.is_empty();
        let mut actions = Vec::new();
        if snapshot.capabilities.attachments {
            actions.push(icon(
                "mail-attachment-symbolic",
                "Attach files",
                available && !self.attachment_chooser_pending,
                Action::Attach(target),
            ));
        }
        if snapshot.capabilities.compact {
            actions.push(icon(
                "view-restore-symbolic",
                "Compact conversation context",
                available && !snapshot.busy,
                Action::Command(target, Command::Compact),
            ));
        }
        if !snapshot.commands.is_empty() {
            let commands = snapshot
                .commands
                .iter()
                .map(|command| command.name.clone())
                .collect::<Vec<_>>();
            actions.push(if transitioning {
                widget::button::standard("Slash commands").into()
            } else {
                widget::dropdown(
                    snapshot
                        .commands
                        .iter()
                        .map(|command| {
                            format!(
                                "/{} — {}",
                                command.name.trim_start_matches('/'),
                                command.description
                            )
                        })
                        .collect::<Vec<_>>(),
                    None,
                    move |index| action(Action::Slash(target, commands[index].clone())),
                )
                .into()
            });
        }
        actions.push(iced::widget::Space::new().width(Length::Fill).into());
        if snapshot.busy {
            if snapshot.capabilities.steer {
                actions.push(icon(
                    "go-jump-symbolic",
                    "Steer the active turn",
                    available && has_draft,
                    Action::Submit(target, PromptMode::Steer),
                ));
            }
            if snapshot.capabilities.queue {
                actions.push(icon(
                    "list-add-symbolic",
                    "Queue for the next turn",
                    available && has_draft,
                    Action::Submit(target, PromptMode::Queue),
                ));
            }
            actions.push(icon(
                "media-playback-stop-symbolic",
                "Cancel active turn",
                available,
                Action::Command(target, Command::Interrupt),
            ));
        } else {
            actions.push(icon(
                "mail-send-symbolic",
                "Send message",
                available && has_draft,
                Action::Submit(target, PromptMode::Send),
            ));
        }
        content.push(
            widget::row::with_children(actions)
                .spacing(4)
                .align_y(Alignment::Center)
                .into(),
        );
        let usage = &snapshot.usage;
        let context = if usage.context_limit > 0 {
            format!(
                " · context {}/{} ({:.0}%)",
                usage.context_tokens,
                usage.context_limit,
                100.0 * usage.context_tokens as f64 / usage.context_limit as f64
            )
        } else if usage.context_tokens > 0 {
            format!(" · context {}", usage.context_tokens)
        } else {
            String::new()
        };
        let cost = usage
            .cost_microusd
            .map(|cost| format!(" · ${:.4}", cost as f64 / 1_000_000.0))
            .unwrap_or_default();
        content.push(
            widget::text::caption(format!(
                "{} · {} in / {} out{}{}",
                if transitioning {
                    "Session transition in progress"
                } else if self.attachment_chooser_pending {
                    "Choosing attachments"
                } else if self.pending.is_some() {
                    "Awaiting acknowledgement"
                } else if snapshot.detail.is_empty() {
                    if snapshot.busy { "Working" } else { "Ready" }
                } else {
                    &snapshot.detail
                },
                usage.input_tokens,
                usage.output_tokens,
                context,
                cost
            ))
            .into(),
        );
        widget::container(
            widget::column::with_children(content)
                .spacing(8)
                .height(Length::Fill)
                .width(Length::Fill),
        )
        .padding(12)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn item_view<'a>(
        &'a self,
        item: &'a chat::Item,
        cached: &'a CachedItem,
    ) -> Element<'a, Message> {
        let target = self.target;
        let mut rows = Vec::new();
        let label = match item.role {
            chat::Role::User => "You",
            chat::Role::Assistant => self.session.launch.provider.label(),
            chat::Role::System => "System",
        };
        let mut heading = vec![design::heading(label).width(Length::Fill).into()];
        match &item.content {
            ItemContent::Text { text, thinking } => {
                if *thinking {
                    heading.push(icon(
                        "pan-down-symbolic",
                        if self.expanded.contains(&item.id) {
                            "Collapse thinking"
                        } else {
                            "Expand thinking"
                        },
                        true,
                        Action::Toggle(target, item.id.clone()),
                    ));
                }
                heading.push(icon(
                    "text-x-generic-symbolic",
                    "Toggle selectable Markdown source",
                    true,
                    Action::Raw(target, item.id.clone()),
                ));
                heading.push(icon(
                    "edit-copy-symbolic",
                    "Copy message",
                    true,
                    Action::Copy(target, item.id.clone(), CopyPart::Text),
                ));
                rows.push(
                    widget::row::with_children(heading)
                        .align_y(Alignment::Center)
                        .spacing(4)
                        .into(),
                );
                if *thinking {
                    rows.push(widget::text::caption("Thinking").into());
                }
                if !thinking || self.expanded.contains(&item.id) {
                    if self.raw.contains(&item.id) {
                        rows.push(body(text.as_str()));
                    } else {
                        let theme = if cosmic::theme::is_dark() {
                            iced::Theme::Dark
                        } else {
                            iced::Theme::Light
                        };
                        rows.push(
                            markdown::view(
                                &cached.markdown,
                                markdown::Settings::with_text_size(
                                    design::TEXT_BODY,
                                    markdown::Style::from(theme),
                                ),
                            )
                            .map(|uri| action(Action::OpenUrl(uri.to_string())))
                            .into(),
                        );
                        for index in 0..cached.codes.len() {
                            rows.push(icon(
                                "edit-copy-symbolic",
                                format!("Copy code block {}", index + 1),
                                true,
                                Action::Copy(target, item.id.clone(), CopyPart::Code(index)),
                            ));
                        }
                    }
                }
            }
            ItemContent::Tool {
                name,
                input,
                output,
                status,
            } => {
                let status = match status {
                    chat::ToolStatus::Running => "Running",
                    chat::ToolStatus::Succeeded => "Completed",
                    chat::ToolStatus::Failed => "Failed",
                    chat::ToolStatus::Denied => "Denied",
                };
                heading.push(widget::text::caption(format!("{name} · {status}")).into());
                heading.push(icon(
                    "pan-down-symbolic",
                    "Expand or collapse tool details",
                    true,
                    Action::Toggle(target, item.id.clone()),
                ));
                rows.push(
                    widget::row::with_children(heading)
                        .align_y(Alignment::Center)
                        .spacing(4)
                        .into(),
                );
                if self.expanded.contains(&item.id) {
                    rows.push(
                        widget::row::with_children(vec![
                            widget::text::caption("Input").width(Length::Fill).into(),
                            icon(
                                "edit-copy-symbolic",
                                "Copy tool input",
                                true,
                                Action::Copy(target, item.id.clone(), CopyPart::Input),
                            ),
                        ])
                        .into(),
                    );
                    rows.push(body(input.as_str()));
                    rows.push(
                        widget::row::with_children(vec![
                            widget::text::caption("Output").width(Length::Fill).into(),
                            icon(
                                "edit-copy-symbolic",
                                "Copy tool output",
                                true,
                                Action::Copy(target, item.id.clone(), CopyPart::Output),
                            ),
                        ])
                        .into(),
                    );
                    rows.push(body(output.as_str()));
                }
            }
            ItemContent::Image { path, description } => {
                rows.push(widget::row::with_children(heading).into());
                if let Some(handle) = &cached.image {
                    rows.push(
                        iced::widget::image(handle.clone())
                            .width(Length::Fill)
                            .height(240)
                            .content_fit(iced::ContentFit::Contain)
                            .into(),
                    );
                }
                rows.push(body(description.as_str()));
                if path.starts_with("http://") || path.starts_with("https://") {
                    rows.push(
                        widget::button::text("Open image")
                            .on_press(action(Action::OpenUrl(path.clone())))
                            .into(),
                    );
                } else if path.starts_with("data:") {
                    if cached.image.is_none() {
                        rows.push(
                            widget::text::caption(
                                "Image could not be decoded or exceeds the display size bound.",
                            )
                            .into(),
                        );
                    }
                } else if !path.is_empty() {
                    rows.push(widget::text::caption(path.as_str()).into());
                }
            }
        }
        widget::column::with_children(rows)
            .spacing(4)
            .width(Length::Fill)
            .into()
    }

    fn request_view<'a>(
        &'a self,
        request: &'a chat::Request,
        available: bool,
    ) -> Element<'a, Message> {
        let target = self.target;
        let mut rows = vec![
            design::heading(request.title.as_str()).into(),
            body(request.description.as_str()),
        ];
        for field in &request.fields {
            rows.push(widget::text::caption(field.label.as_str()).into());
            if !field.description.is_empty() {
                rows.push(body(field.description.as_str()));
            }
            let values = self.answers.get(&(request.id.clone(), field.id.clone()));
            {
                let value = values
                    .and_then(|values| {
                        values
                            .iter()
                            .find(|value| !field.choices.iter().any(|choice| &choice.id == *value))
                    })
                    .map(String::as_str)
                    .unwrap_or("");
                let request_id = request.id.clone();
                let field_id = field.id.clone();
                rows.push(
                    iced::widget::text_input(
                        if field.choices.is_empty() {
                            field.label.as_str()
                        } else {
                            "Other answer (if supported)"
                        },
                        value,
                    )
                    .id(widget::Id::new(format!(
                        "chat.{}.{}.{}",
                        target.card, request.id, field.id
                    )))
                    .secure(field.secret)
                    .on_input_maybe(available.then_some(move |value| {
                        action(Action::Field(
                            target,
                            request_id.clone(),
                            field_id.clone(),
                            value,
                        ))
                    }))
                    .into(),
                );
            }
            if !field.choices.is_empty() {
                for choice in &field.choices {
                    let selected = values.is_some_and(|values| values.contains(&choice.id));
                    let message = action(Action::Choice(
                        target,
                        request.id.clone(),
                        field.id.clone(),
                        choice.id.clone(),
                    ));
                    rows.push(
                        widget::button::custom(widget::text(choice.label.as_str()))
                            .name(choice.label.as_str())
                            .class(cosmic::theme::Button::MenuItem)
                            .selected(selected)
                            .on_press_maybe(available.then_some(message))
                            .into(),
                    );
                    if !choice.description.is_empty() {
                        rows.push(widget::text::caption(choice.description.as_str()).into());
                    }
                }
            }
        }
        for choice in &request.actions {
            rows.push(
                widget::button::standard(choice.label.as_str())
                    .on_press_maybe(available.then_some(action(Action::Answer(
                        target,
                        request.id.clone(),
                        choice.id.clone(),
                    ))))
                    .into(),
            );
            if !choice.description.is_empty() {
                rows.push(widget::text::caption(choice.description.as_str()).into());
            }
        }
        widget::container(
            widget::column::with_children(rows)
                .spacing(6)
                .width(Length::Fill),
        )
        .class(cosmic::theme::Container::Secondary)
        .padding(10)
        .width(Length::Fill)
        .into()
    }
}
