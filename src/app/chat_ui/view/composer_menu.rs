//! One ordered menu supplies both pointer choices and search Enter.
//! Rows carry existing UI actions; capability dispatch remains in chat_ui.
use super::*;

pub(super) struct Row {
    pub key: [u8; 32],
    pub label: String,
    pub description: String,
    pub icon: Option<Icon>,
    pub id: Option<widget::Id>,
    pub accessible_name: Option<&'static str>,
    pub action: Option<Action>,
}

impl Row {
    fn native(
        identity: &str,
        label: &str,
        description: &str,
        icon: Icon,
        action: Option<Action>,
    ) -> Self {
        Self {
            key: composer_action_key("native", identity),
            label: label.into(),
            description: description.into(),
            icon: Some(icon),
            id: None,
            accessible_name: None,
            action,
        }
    }
}

pub(super) fn offered(runtime: &Runtime, available: bool, command_enabled: bool) -> Vec<Row> {
    let target = runtime.target;
    let snapshot = &runtime.snapshot;
    let query = runtime.composer_actions_query.trim().to_lowercase();
    let matches = |words: &str| words.to_lowercase().contains(&query);
    let mut rows = Vec::new();
    if snapshot.capabilities.attachments
        && matches("attach files or images add image upload attachment")
    {
        rows.push(Row::native(
            "attachments",
            "Attach files or images…",
            "Choose files or images to attach to this draft",
            Icon::Attachment,
            (available && runtime.attachment_chooser_pending.is_none())
                .then_some(Action::Attach(target)),
        ));
    }
    for candidate in runtime.composer_action_candidates() {
        let skill = candidate.kind == completion::Kind::Skill;
        let label = if skill {
            format!("Skill · {}", candidate.name)
        } else {
            match candidate.name {
                "plan" => "Plan mode".into(),
                "goal" => "Goal…".into(),
                _ => candidate.label().into_owned(),
            }
        };
        let enabled = available && (skill || candidate.insertion == "/goal" || command_enabled);
        rows.push(Row {
            key: composer_action_key(
                if skill { "skill" } else { "command" },
                &candidate.insertion,
            ),
            label,
            description: candidate.description.into(),
            icon: None,
            id: None,
            accessible_name: None,
            action: enabled
                .then(|| Action::ComposerAction(target, candidate.insertion.into_owned())),
        });
    }
    if !snapshot.capabilities.interrupt_modes.is_empty()
        && matches("steering timing after turn after tools")
    {
        let wait = snapshot.interrupt_mode.as_deref() == Some("wait");
        rows.push(Row::native(
            "steering",
            if wait {
                "Steering: after turn"
            } else {
                "Steering: after tools"
            },
            "Choose when a follow-up can steer the active turn",
            Icon::Jump,
            available.then(|| {
                Action::Command(
                    target,
                    Command::SetInterruptMode {
                        mode: if wait { "immediate" } else { "wait" }.into(),
                    },
                )
            }),
        ));
    }
    if snapshot.capabilities.branch && matches("fork conversation branch") {
        let mut row = Row::native(
            "fork",
            "Fork conversation…",
            "Start another conversation from a completed turn",
            Icon::Branch,
            command_enabled.then_some(Action::BranchPicker(target, true)),
        );
        row.id = Some(card_control_id(target.card, "chat-actions-fork"));
        rows.push(row);
    }
    if snapshot.capabilities.compact
        && matches("compact conversation context summarize earlier turns free context")
    {
        let mut row = Row::native(
            "compact",
            "Compact conversation",
            "Summarize earlier turns to free context",
            Icon::Compress,
            command_enabled.then_some(Action::Command(target, Command::Compact)),
        );
        row.id = Some(card_control_id(target.card, "chat-actions-compact"));
        row.accessible_name =
            Some("Compact the conversation: summarize earlier turns to free context");
        rows.push(row);
    }
    rows
}
