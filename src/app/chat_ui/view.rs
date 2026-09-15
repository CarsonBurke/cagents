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
/// The copy control's label for code block `index`.
///
/// The transcript is rebuilt on every frame of a streaming turn, so the labels
/// a message plausibly needs are spelled out instead of formatted per frame.
fn code_label(index: usize) -> Cow<'static, str> {
    const LABELS: [&str; 8] = [
        "Copy code block 1",
        "Copy code block 2",
        "Copy code block 3",
        "Copy code block 4",
        "Copy code block 5",
        "Copy code block 6",
        "Copy code block 7",
        "Copy code block 8",
    ];
    LABELS.get(index).map_or_else(
        || Cow::Owned(format!("Copy code block {}", index + 1)),
        |label| Cow::Borrowed(*label),
    )
}
/// The icon and tooltip for a submit control.
///
/// `slot` is the control's place among the modes the provider currently
/// allows: the first answers to Ctrl+Enter and the second to Ctrl+Shift+Enter,
/// so the tooltip names the chord the composer is bound to.
fn submit_control(mode: PromptMode, slot: usize) -> (&'static str, &'static str) {
    match (mode, slot) {
        (PromptMode::Send, _) => ("mail-send-symbolic", "Send message — Ctrl+Enter"),
        (PromptMode::Steer, 0) => ("go-jump-symbolic", "Steer the active turn — Ctrl+Enter"),
        (PromptMode::Steer, _) => (
            "go-jump-symbolic",
            "Steer the active turn — Ctrl+Shift+Enter",
        ),
        (PromptMode::Queue, 0) => ("list-add-symbolic", "Queue for the next turn — Ctrl+Enter"),
        (PromptMode::Queue, _) => (
            "list-add-symbolic",
            "Queue for the next turn — Ctrl+Shift+Enter",
        ),
    }
}
/// The submit mode a composer key press asks for, or `None` when the press is
/// ordinary editing.
///
/// `primary` and `alternate` are the modes the send controls accept right now;
/// the chords must reach exactly those and nothing else. Plain Enter and
/// Shift+Enter both break the line, as a composer for prose has to: binding
/// the alternate mode to Shift+Enter would have made one keystroke insert a
/// newline while the agent is idle and queue a turn while it is busy, which is
/// the same key doing two unrelated things. The binding is consulted for every
/// key press in the window, so an unfocused composer must not answer for one.
/// `command` is Ctrl everywhere but macOS, where it is Cmd, so a single arm
/// covers both platforms.
fn submit_chord(
    press: &text_editor::KeyPress,
    primary: Option<PromptMode>,
    alternate: Option<PromptMode>,
) -> Option<PromptMode> {
    if !matches!(press.status, text_editor::Status::Focused { .. })
        || !matches!(press.key, keyboard::Key::Named(keyboard::key::Named::Enter))
        || press.modifiers.alt()
        || !press.modifiers.command()
    {
        return None;
    }
    if press.modifiers.shift() {
        alternate
    } else {
        primary
    }
}
fn command(target: Target, value: Command) -> Message {
    action(Action::Command(target, value))
}

impl Cagents {
    pub(in crate::app) fn chat_binding_view(&self, card: CardId) -> Option<Element<'_, Message>> {
        let binding = self.chat_ui.bindings.get(&card)?;
        let valid =
            binding.input.trim().len() == 36 && uuid::Uuid::parse_str(binding.input.trim()).is_ok();
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::heading("Select the exact Codex conversation").into(),
            body(
                "The shared server does not identify which thread this terminal is viewing. Copy the full thread ID from /status in the intended CLI. Preview is read-only; nothing is resumed or sent.",
            ),
            iced::widget::text_input("Full conversation UUID", &binding.input)
                .on_input(move |input| action(Action::BindingInput(card, input)))
                .on_submit_maybe(
                    (valid && !binding.loading).then_some(action(Action::BindingPreview(card))),
                )
                .into(),
        ];
        if let Some(preview) = &binding.preview {
            rows.push(body(format!(
                "Conversation: {}\nID: {}\nDirectory: {}",
                preview.title,
                preview.session_id,
                preview.working_directory.display()
            )));
            rows.push(body("Confirm only if this is the conversation you intend to control. This attaches to its existing shared backend with its current permissions. The source terminal stays running; no duplicate agent is launched."));
        }
        if let Some(error) = &binding.error {
            rows.push(body(error.as_str()));
        }
        rows.push(
            widget::column::with_children(vec![
                widget::button::text(if binding.loading {
                    "Reading conversation…"
                } else {
                    "Preview conversation"
                })
                .on_press_maybe(
                    (valid && !binding.loading).then_some(action(Action::BindingPreview(card))),
                )
                .into(),
                widget::button::text("Confirm selected conversation")
                    .on_press_maybe(
                        (binding.preview.is_some() && !binding.loading)
                            .then_some(action(Action::BindingConfirm(card))),
                    )
                    .into(),
                widget::button::text("Cancel")
                    .on_press(action(Action::BindingCancel(card)))
                    .into(),
            ])
            .spacing(8)
            .into(),
        );
        Some(
            widget::container(
                widget::scrollable(widget::column::with_children(rows).spacing(8).padding(12))
                    .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
                        crate::design::scrollbar(),
                    ))
                    .height(Length::Shrink),
            )
            .max_height(320)
            .width(Length::Fill)
            .into(),
        )
    }

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
        // One theme lookup and one settings value per frame, rather than one
        // per rendered message.
        let markdown_settings = markdown::Settings::with_text_size(
            design::TEXT_BODY,
            markdown::Style::from(if cosmic::theme::is_dark() {
                iced::Theme::Dark
            } else {
                iced::Theme::Light
            }),
        );
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
            transcript.push(self.item_view(item, cached, markdown_settings));
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
            .direction(iced::widget::scrollable::Direction::Vertical(
                design::scrollbar(),
            ))
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
                    crate::design::caption(if snapshot.exited {
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
                widget::container(
                    widget::scrollable(
                        widget::column::with_children(
                            snapshot
                                .requests
                                .iter()
                                .map(|request| self.request_view(request, available))
                                .collect::<Vec<_>>(),
                        )
                        .spacing(12),
                    )
                    .direction(
                        cosmic::iced::widget::scrollable::Direction::Vertical(
                            crate::design::scrollbar(),
                        ),
                    ),
                )
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
                    widget::container(
                        widget::scrollable(widget::column::with_children(commands).spacing(0))
                            .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
                                crate::design::scrollbar(),
                            )),
                    )
                    .max_height(120)
                    .into(),
                );
            }
        }
        for (index, attachment) in self.attachments.iter().enumerate() {
            content.push(
                widget::row::with_children(vec![
                    crate::design::caption(attachment.path.display().to_string())
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
        let has_draft = !draft.trim().is_empty() || !self.attachments.is_empty();
        // The submit modes the send controls expose, in the order the keyboard
        // reaches them: the first is Ctrl+Enter, the second Ctrl+Shift+Enter.
        let modes: &[PromptMode] = if snapshot.busy {
            match (snapshot.capabilities.steer, snapshot.capabilities.queue) {
                (true, true) => &[PromptMode::Steer, PromptMode::Queue],
                (true, false) => &[PromptMode::Steer],
                (false, true) => &[PromptMode::Queue],
                (false, false) => &[],
            }
        } else {
            &[PromptMode::Send]
        };
        let placeholder = match (modes.first(), modes.get(1)) {
            (Some(PromptMode::Send), _) => "Message · Enter for a new line · Ctrl+Enter to send",
            (Some(PromptMode::Steer), Some(_)) => {
                "Message · Enter for a new line · Ctrl+Enter to steer · Ctrl+Shift+Enter to queue"
            }
            (Some(PromptMode::Steer), None) => {
                "Message · Enter for a new line · Ctrl+Enter to steer"
            }
            (Some(PromptMode::Queue), _) => "Message · Enter for a new line · Ctrl+Enter to queue",
            (None, _) => "Message · Enter for a new line · this turn accepts no further input",
        };
        // A chord only submits what a send control would accept right now.
        let submit = if available && has_draft {
            (modes.first().copied(), modes.get(1).copied())
        } else {
            (None, None)
        };
        content.push(if transitioning {
            widget::container(widget::scrollable(body(draft)).direction(
                cosmic::iced::widget::scrollable::Direction::Vertical(crate::design::scrollbar()),
            ))
            .height(104)
            .width(Length::Fill)
            .padding(8)
            .into()
        } else {
            iced::widget::text_editor::TextEditor::new(&self.composer)
                .id(card_control_id(target.card, "chat-composer"))
                .placeholder(placeholder)
                .size(design::TEXT_BODY)
                .height(104)
                .padding(8)
                .on_action(move |event| action(Action::Composer(target, event)))
                .key_binding(move |press| {
                    match submit_chord(&press, submit.0, submit.1) {
                        Some(mode) => Some(text_editor::Binding::Custom(action(Action::Submit(
                            target, mode,
                        )))),
                        // Everything else, Enter included, edits the draft.
                        None => text_editor::Binding::from_key_press(press),
                    }
                })
                .into()
        });
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
        for (slot, mode) in modes.iter().enumerate() {
            let (name, label) = submit_control(*mode, slot);
            actions.push(icon(
                name,
                label,
                available && has_draft,
                Action::Submit(target, *mode),
            ));
        }
        if snapshot.busy {
            actions.push(icon(
                "media-playback-stop-symbolic",
                "Cancel active turn",
                available,
                Action::Command(target, Command::Interrupt),
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
        let detail = if transitioning {
            "Session transition in progress"
        } else if self.attachment_chooser_pending {
            "Choosing attachments"
        } else if self.pending.is_some() {
            "Awaiting acknowledgement"
        } else {
            &snapshot.detail
        };
        let status: Element<'_, Message> = if detail.is_empty() {
            let (name, label) = if snapshot.waiting || !snapshot.requests.is_empty() {
                ("agent-waiting-symbolic", "Waiting for your input")
            } else if snapshot.busy {
                ("agent-working-symbolic", "Working")
            } else {
                ("emblem-ok-symbolic", "Ready")
            };
            crate::ui_controls::icon_control(
                crate::ui_controls::symbolic_icon(name),
                label,
                false,
                false,
                None,
            )
            .into()
        } else {
            crate::design::caption(detail).into()
        };
        content.push(
            widget::row::with_children(vec![
                status,
                crate::design::caption(format!(
                    "{} in / {} out{}{}",
                    usage.input_tokens, usage.output_tokens, context, cost
                ))
                .into(),
            ])
            .spacing(8)
            .align_y(Alignment::Center)
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
        settings: markdown::Settings,
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
                    rows.push(crate::design::caption("Thinking").into());
                }
                if !thinking || self.expanded.contains(&item.id) {
                    if self.raw.contains(&item.id) {
                        rows.push(body(text.as_str()));
                    } else {
                        rows.push(
                            markdown::view(cached.markdown.items(), settings)
                                .map(|uri| action(Action::OpenUrl(uri.to_string())))
                                .into(),
                        );
                        for index in 0..cached.code.codes.len() {
                            rows.push(icon(
                                "edit-copy-symbolic",
                                code_label(index),
                                true,
                                Action::Copy(target, item.id.clone(), CopyPart::Code(index)),
                            ));
                        }
                    }
                }
            }
            ItemContent::Tool { input, output, .. } => {
                if let Some(detail) = &cached.detail {
                    heading.push(crate::design::caption(detail.as_str()).into());
                }
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
                            crate::design::caption("Input").width(Length::Fill).into(),
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
                            crate::design::caption("Output").width(Length::Fill).into(),
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
                            crate::design::caption(
                                "Image could not be decoded or exceeds the display size bound.",
                            )
                            .into(),
                        );
                    }
                } else if !path.is_empty() {
                    rows.push(crate::design::caption(path.as_str()).into());
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
            rows.push(crate::design::caption(field.label.as_str()).into());
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
                        rows.push(crate::design::caption(choice.description.as_str()).into());
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
                rows.push(crate::design::caption(choice.description.as_str()).into());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn press(modifiers: keyboard::Modifiers, focused: bool) -> text_editor::KeyPress {
        let key = keyboard::Key::Named(keyboard::key::Named::Enter);
        text_editor::KeyPress {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            modifiers,
            text: None,
            status: if focused {
                text_editor::Status::Focused { is_hovered: false }
            } else {
                text_editor::Status::Active
            },
        }
    }

    /// The composer is the product's primary input: a chord must reach the
    /// send controls, and a bare Enter must never send a half-written message.
    #[test]
    fn only_the_documented_chords_submit() {
        let send = Some(PromptMode::Send);
        let queue = Some(PromptMode::Queue);

        assert_eq!(
            submit_chord(&press(keyboard::Modifiers::COMMAND, true), send, None),
            send
        );
        assert_eq!(
            submit_chord(
                &press(
                    keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT,
                    true
                ),
                send,
                queue
            ),
            queue
        );
        // No alternate mode on offer: the chord falls back to a line break.
        assert_eq!(
            submit_chord(
                &press(
                    keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT,
                    true
                ),
                send,
                None
            ),
            None
        );
        for modifiers in [
            keyboard::Modifiers::default(),
            // Shift+Enter must stay a line break whether or not the provider
            // is offering an alternate mode, or one keystroke would mean two
            // unrelated things depending on whether the agent is busy.
            keyboard::Modifiers::SHIFT,
            keyboard::Modifiers::ALT,
            keyboard::Modifiers::COMMAND | keyboard::Modifiers::ALT,
        ] {
            assert_eq!(
                submit_chord(&press(modifiers, true), send, queue),
                None,
                "{modifiers:?} must edit the draft rather than submit it"
            );
        }
        // The binding is offered every key press in the window.
        assert_eq!(
            submit_chord(&press(keyboard::Modifiers::COMMAND, false), send, queue),
            None
        );
        // Nothing to submit: the chord falls back to inserting a line.
        assert_eq!(
            submit_chord(&press(keyboard::Modifiers::COMMAND, true), None, None),
            None
        );
    }

    /// Every submit control's tooltip has to name the chord bound to it, or the
    /// keyboard path stays invisible.
    #[test]
    fn submit_tooltips_name_their_chord() {
        for (mode, slot, chord) in [
            (PromptMode::Send, 0, "Ctrl+Enter"),
            (PromptMode::Steer, 0, "Ctrl+Enter"),
            (PromptMode::Queue, 0, "Ctrl+Enter"),
            (PromptMode::Steer, 1, "Ctrl+Shift+Enter"),
            (PromptMode::Queue, 1, "Ctrl+Shift+Enter"),
        ] {
            let (_, label) = submit_control(mode, slot);
            assert!(
                label.ends_with(chord),
                "{mode:?} in slot {slot} is bound to {chord} but reads {label:?}"
            );
        }
    }
}
