//! Development-only model consumer for the fixed inert Accounts catalog scene.
//! No session factory, skill discovery, writer, client or provider is entered.
use super::*;

pub(in crate::app) struct Observation {
    pub target: Target,
    pub models: Vec<chat::Model>,
    pub selected: Option<String>,
    pub picker_open: bool,
    pub footer_open: bool,
    pub unstarted: bool,
}

pub(in crate::app) fn observe(app: &Cagents, card: CardId) -> Option<Observation> {
    let CardRuntime::Chat(runtime) = app.cards.get(&card)? else {
        return None;
    };
    Some(Observation {
        target: runtime.target,
        models: runtime.snapshot.models.clone(),
        selected: runtime.snapshot.selected_model.clone(),
        picker_open: runtime.model_picker.is_some(),
        footer_open: runtime.footer_picker,
        unstarted: runtime.is_draft()
            && runtime.session.is_unstarted()
            && runtime.handle().is_none()
            && runtime.queued.is_none()
            && runtime.pending.is_none(),
    })
}

/// Mount the original Runtime::draft and its real view, without the optional
/// authored-skill task and persistence performed by ordinary card insertion.
/// The caller has already validated the fixture's five private XDG roots.
pub(in crate::app) fn mount(
    app: &mut Cagents,
    directory: PathBuf,
) -> Result<(CardId, Task<Message>), String> {
    if app.session.is_some() || !app.cards.is_empty() || app.workspace.order().next().is_some() {
        return Err("catalog fixture needs its original empty, writer-free workspace".into());
    }
    let card = CardId::from_uuid(uuid::Uuid::from_u128(194_101));
    let session = chat::Session {
        id: TerminalId::from_uuid(uuid::Uuid::from_u128(194_102)),
        launch: chat::Launch {
            provider: Provider::Claude,
            recipe: LaunchRecipe {
                program: "/inert-accounts-catalog-never-executed".into(),
                arguments: vec![
                    "--permission-mode".into(),
                    "default".into(),
                    "--disable-slash-commands".into(),
                ],
                working_directory: directory,
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let target = app.chat_ui.next_target(card);
    let snapshot = app.chat_draft_snapshot(Provider::Claude);
    let runtime = Runtime::draft(
        target,
        session.clone(),
        snapshot,
        true,
        app.settings.editor_syntax_theme,
    );
    let state = CardState {
        id: card,
        generation: app.durable.generation,
        title: "Model catalog · synthetic draft".into(),
        width_fraction: 1.0,
        existence: Existence::Open,
        exposure: Exposure::Visible,
        residency: Residency::Hot,
        kind: CardKind::Chat(session),
    };
    let _initial_viewport = app
        .workspace
        .add_card(
            WorkspaceCard::new(
                card,
                state.title.clone(),
                CardWidth::Fraction(state.width_fraction),
            )
            .map_err(|error| error.to_string())?,
            0,
            true,
        )
        .map_err(|error| error.to_string())?;
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.push(state);
    durable.focused = Some(card);
    app.register_channel_card(card);
    app.cards.insert(card, CardRuntime::Chat(Box::new(runtime)));
    // The initial state above already records this one visible draft. Avoid
    // the ordinary persistence/lifecycle postlude; retain only widget focus.
    Ok((
        card,
        iced::widget::operation::focus(card_control_id(card, "chat-model-picker"))
            .map(cosmic::Action::App),
    ))
}
