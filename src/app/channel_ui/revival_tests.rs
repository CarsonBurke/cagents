//! Reopening a card closed in a channel returns it to that channel, including
//! one that retired when its last card closed.

use super::*;
use crate::persistence::SavedConversation;
use crate::workspace_model::{
    ModelChannel,
    views::{Change, Pane, Scope, View},
};
use workspace_widget::GlobalShortcut as Command;

const TITLE: &str = "Explain the parser";

struct Fixture {
    app: Cagents,
    directory: tempfile::TempDir,
    project: ProjectId,
    channel: ChannelId,
    card: CardId,
    home: crate::channel_work::Home,
}

/// A project at a real directory with one shared channel, entered, whose only
/// card is a saved conversation the registry's view also holds.
fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().to_path_buf();
    let mut app = super::super::tests::context_inspector_test_app(true);
    let project = ProjectId::new();
    let channel = ChannelId::new();
    let home = crate::channel_work::Home {
        registry: uuid::Uuid::new_v4(),
        channel,
        creation_ordinal: 4,
    };
    let mut session = crate::chat::fresh_session(
        crate::agent::Provider::Claude,
        TerminalId::new(),
        path.clone(),
        &crate::account::Account::system(crate::agent::Provider::Claude),
    )
    .unwrap();
    session.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
    let durable = Arc::make_mut(&mut app.durable);
    let mut chat = durable.cards[0].clone();
    let card = CardId::new();
    chat.id = card;
    chat.kind = CardKind::Chat(session);
    chat.title = TITLE.into();
    durable.cards.push(chat);
    durable.channels.push(ProjectChannel {
        title_source: crate::model::ChannelTitleSource::Manual,
        id: channel,
        project_id: project,
        title: "Investigation".into(),
        ordinal: home.creation_ordinal,
        shared: true,
        cards: vec![card],
        focused: Some(card),
    });
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "project".into(),
        path.clone(),
        vec![crate::sidebar::SidebarWorktree::from_durable(
            crate::model::WorktreeId::new(),
            path.clone(),
            Some("main".into()),
        )],
        Vec::new(),
    ));
    app.workspace_model.identity = home.registry;
    app.workspace_model.channels.push(ModelChannel {
        id: channel,
        repository: path,
        title: "Investigation".into(),
        title_source: crate::model::ChannelTitleSource::Manual,
        ordinal: home.creation_ordinal,
    });
    app.workspace_model.views.push(View {
        scope: Scope::Channel(channel),
        revision: 7,
        panes: vec![Pane::from_card(app.card_state(card).unwrap(), None).unwrap()],
        retired: vec![],
    });
    let panes = app.workspace_model.views[0].panes.clone();
    app.shared_views.initialize(Scope::Channel(channel), panes);
    app.refresh_sidebar_channels();
    drop(app.select_channel(channel));
    assert_eq!(app.durable.active_channel, Some(channel));
    assert!(app.workspace.order().any(|id| id == card));
    Fixture {
        app,
        directory,
        project,
        channel,
        card,
        home,
    }
}

impl Fixture {
    /// Closes the channel's only card the way the user does.
    fn close_card(&mut self) {
        self.app.workspace.request_close(self.card).unwrap();
        drop(self.app.commit_card_removal(self.card, None).unwrap());
        assert!(self.app.channel_work.retire_candidates.contains(&self.home));
    }

    /// The registry's view of the channel settles empty, and the window asks
    /// it to retire the channel.
    fn send_retirement(&mut self) -> async_channel::Receiver<ModelEdit> {
        let (send, received) = async_channel::unbounded();
        self.app.workspace_model_tx = send;
        let view = &mut self.app.workspace_model.views[0];
        view.panes.clear();
        view.retired.push(self.card);
        view.revision = 8;
        self.app.retire_settled_empty_channels();
        assert_eq!(
            received.try_recv().unwrap(),
            ModelEdit::RetireEmptyChannel {
                home: self.home,
                expected_view_revision: 8,
            }
        );
        received
    }

    /// The registry answers this window's request with the model after it
    /// decided: without the channel if it retired it, unchanged if it kept it.
    fn answer_retirement(&mut self, retired: bool) {
        let sequence = self
            .app
            .workspace_model
            .sequence
            .max(self.app.shared_views.snapshot_sequence)
            + 1;
        self.answer_retirement_at(retired, sequence);
    }

    fn answer_retirement_at(&mut self, retired: bool, sequence: u64) {
        let channel = self.channel;
        let mut model = self.app.workspace_model.clone();
        if retired {
            model.channels.retain(|shared| shared.id != channel);
            model
                .views
                .retain(|view| view.scope != Scope::Channel(channel));
        }
        model.sequence = sequence;
        drop(self.app.apply_workspace_model_update(
            super::super::workspace_model_sync::ModelUpdate::RetirementAnswered { channel, model },
        ));
        assert!(!self.app.channel_work.retiring.contains_key(&channel));
    }

    /// The registry retires the channel, and the window learns it.
    fn retire(&mut self) -> async_channel::Receiver<ModelEdit> {
        let received = self.send_retirement();
        let channel = self.channel;
        self.answer_retirement(true);
        assert!(
            !self
                .app
                .durable
                .channels
                .iter()
                .any(|row| row.id == channel)
        );
        assert_eq!(self.app.durable.active_channel, None);
        assert_eq!(
            self.app.active_workspace.as_deref(),
            Some(self.directory.path())
        );
        while received.try_recv().is_ok() {}
        received
    }

    /// A second project at its own directory, entered.
    fn enter_other_project(&mut self) -> tempfile::TempDir {
        let other = tempfile::tempdir().unwrap();
        let path = other.path().to_path_buf();
        self.app.sidebar_model.projects.push(SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "other".into(),
            path.clone(),
            vec![crate::sidebar::SidebarWorktree::from_durable(
                crate::model::WorktreeId::new(),
                path.clone(),
                Some("main".into()),
            )],
            Vec::new(),
        ));
        drop(self.app.select_worktree(path, false));
        assert_eq!(self.app.active_workspace.as_deref(), Some(other.path()));
        other
    }

    fn reopen_pane(&mut self) {
        drop(
            self.app
                .run_workspace_command(Command::ReopenPane, keyboard::Modifiers::empty()),
        );
    }

    fn saved(&self) -> SavedConversation {
        let card = self.app.card_state(self.card).unwrap();
        let CardKind::Chat(session) = &card.kind else {
            panic!("a saved conversation");
        };
        SavedConversation {
            card: self.card,
            title: card.title.clone(),
            session: session.clone(),
            open: false,
            updated_at_unix_ms: 1,
        }
    }

    /// The card on screen, which must be the conversation resumed again.
    fn resumed(&self) -> &CardState {
        let id = self.app.durable.focused.unwrap();
        assert_ne!(id, self.card, "a closed card is resumed as a new one");
        assert!(self.app.workspace.order().any(|card| card == id));
        let state = self.app.card_state(id).unwrap();
        let (CardKind::Chat(resumed), CardKind::Chat(original)) =
            (&state.kind, &self.app.card_state(self.card).unwrap().kind)
        else {
            panic!("a resumed conversation");
        };
        assert_eq!(resumed.launch, original.launch);
        state
    }

    fn active_channel(&self) -> &ProjectChannel {
        let id = self
            .app
            .durable
            .active_channel
            .expect("a channel is entered");
        self.app
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .unwrap()
    }
}

/// Closing a channel's last card retires it, and reopening that card brings
/// the channel back under its name with the conversation in it, rather than
/// leaving the conversation in the worktree strip the window fell back to.
#[test]
fn reopening_the_last_card_of_a_retired_channel_brings_the_channel_back() {
    let mut fixture = fixture();
    fixture.close_card();
    let published = fixture.retire();

    fixture.reopen_pane();

    let resumed = fixture.resumed().clone();
    assert_eq!(resumed.title, TITLE, "the conversation keeps its own name");
    let channel = fixture.active_channel().clone();
    assert_ne!(
        channel.id, fixture.channel,
        "a retired identity is never reused"
    );
    assert_eq!(channel.title, "Investigation");
    assert_eq!(
        channel.title_source,
        crate::model::ChannelTitleSource::Manual
    );
    assert_eq!(channel.project_id, fixture.project);
    assert_eq!(channel.cards, vec![resumed.id]);
    let created = std::iter::from_fn(|| published.try_recv().ok()).find_map(|edit| match edit {
        ModelEdit::CreateChannel {
            id,
            title,
            title_source,
            ..
        } if id == channel.id => Some((title, title_source)),
        _ => None,
    });
    assert_eq!(
        created,
        Some((
            "Investigation".into(),
            crate::model::ChannelTitleSource::Manual
        )),
        "every window learns the channel under the name it had"
    );
    assert!(fixture.app.recently_closed.is_empty());
    fixture.app.refresh_sidebar_channels();
    assert!(
        fixture
            .app
            .sidebar_model
            .project(fixture.project)
            .unwrap()
            .channels
            .iter()
            .any(|row| row.id == channel.id)
    );

    // Reopening again has nothing left to reopen and stays where it is.
    fixture.reopen_pane();
    assert_eq!(fixture.app.durable.active_channel, Some(channel.id));
    assert_eq!(fixture.app.durable.focused, Some(resumed.id));
}

/// The resume picker returns a conversation to the channel it was closed in,
/// and a second conversation closed in the same retired channel follows the
/// first to its revival instead of making another.
#[test]
fn resuming_from_the_picker_returns_to_the_revived_channel() {
    let mut fixture = fixture();
    let saved = fixture.saved();
    fixture.close_card();
    fixture.retire();

    drop(fixture.app.resume_saved_conversation(saved.clone()));
    let revived = fixture.active_channel().id;
    assert_ne!(revived, fixture.channel);
    assert_eq!(fixture.resumed().title, TITLE);

    // A second card closed in the old channel, known only by the closing.
    let mut other = fixture.app.card_state(fixture.card).unwrap().clone();
    other.id = CardId::new();
    let CardKind::Chat(session) = &mut other.kind else {
        unreachable!()
    };
    session.launch.session_id = Some("0b3c3f71-6a07-4c63-8f5a-2b8f8c1f9d8e".into());
    let other_saved = SavedConversation {
        card: other.id,
        session: session.clone(),
        ..saved
    };
    Arc::make_mut(&mut fixture.app.durable)
        .cards
        .push(other.clone());
    fixture
        .app
        .recently_closed
        .push((other, Some(WorkspaceDestination::Channel(fixture.channel))));
    let path = fixture.directory.path().to_path_buf();
    drop(fixture.app.select_worktree(path, false));
    let channels = fixture.app.durable.channels.len();
    drop(fixture.app.resume_saved_conversation(other_saved));
    assert_eq!(fixture.app.durable.active_channel, Some(revived));
    assert_eq!(fixture.app.durable.channels.len(), channels);
}

/// A channel the user deleted stays deleted: reopening a card that was closed
/// in it opens nothing, and resuming it from the picker uses its worktree.
#[test]
fn a_deleted_channel_is_not_revived() {
    let mut fixture = fixture();
    let saved = fixture.saved();
    fixture.close_card();
    drop(
        fixture
            .app
            .handle_channel_ui(Action::Delete(fixture.channel)),
    );
    assert!(
        !fixture
            .app
            .durable
            .channels
            .iter()
            .any(|channel| channel.id == fixture.channel)
    );
    let channels = fixture.app.durable.channels.len();
    let cards = fixture.app.durable.cards.len();

    fixture.reopen_pane();
    assert_eq!(fixture.app.durable.channels.len(), channels);
    assert_eq!(fixture.app.durable.cards.len(), cards);

    drop(fixture.app.resume_saved_conversation(saved));
    assert_eq!(fixture.app.durable.channels.len(), channels);
    assert_eq!(fixture.app.durable.active_channel, None);
    assert_eq!(
        fixture.app.active_workspace.as_deref(),
        Some(fixture.directory.path())
    );
    assert_eq!(fixture.resumed().title, TITLE);
}

/// Reopened before the channel retires, the card returns to the same channel,
/// which then no longer retires, and its name reaches the shared view instead
/// of the provider's generic one.
#[test]
fn reopening_before_retirement_keeps_the_channel() {
    let mut fixture = fixture();
    fixture.close_card();
    let (send, published) = async_channel::unbounded();
    fixture.app.workspace_model_tx = send;

    fixture.reopen_pane();

    assert_eq!(fixture.app.durable.active_channel, Some(fixture.channel));
    let resumed = fixture.resumed().clone();
    assert_eq!(fixture.active_channel().cards, vec![resumed.id]);
    assert!(fixture.app.channel_work.retire_candidates.is_empty());
    fixture.app.workspace_model.views[0].panes.clear();
    fixture.app.workspace_model.views[0].revision = 8;
    fixture.app.retire_settled_empty_channels();
    assert!(
        !std::iter::from_fn(|| published.try_recv().ok())
            .any(|edit| matches!(edit, ModelEdit::RetireEmptyChannel { .. })),
        "a channel that took its card back is not retired"
    );

    let named = fixture
        .app
        .shared_views
        .pending_changes()
        .rev()
        .find_map(|(scope, change)| match change {
            Change::Insert { pane, .. } | Change::Update { pane, .. }
                if *scope == Scope::Channel(fixture.channel) && pane.id == resumed.id =>
            {
                Some(pane.title.clone())
            }
            _ => None,
        });
    assert_eq!(named.as_deref(), Some(TITLE));
}

/// Once the registry has been asked to retire the channel, a pane published
/// into it would arrive after the retirement, so reopening brings the channel
/// back rather than racing it.
#[test]
fn reopening_while_retirement_is_in_flight_revives_the_channel() {
    let mut fixture = fixture();
    fixture.close_card();
    let _published = fixture.send_retirement();
    assert!(
        fixture
            .app
            .durable
            .channels
            .iter()
            .any(|channel| channel.id == fixture.channel)
    );

    fixture.reopen_pane();

    let revived = fixture.active_channel().clone();
    assert_ne!(revived.id, fixture.channel);
    assert_eq!(revived.cards, vec![fixture.resumed().id]);

    // The registry's answer then retires only the old, empty channel.
    let channel = fixture.channel;
    fixture
        .app
        .workspace_model
        .views
        .retain(|view| view.scope != Scope::Channel(channel));
    drop(fixture.app.retire_peer_channel(channel));
    assert_eq!(fixture.app.durable.active_channel, Some(revived.id));
    assert_eq!(fixture.active_channel().cards, revived.cards);
}

/// A registry that keeps the channel (it holds work, say) changes nothing in
/// its view, so once the answer is overdue the channel is entered as it is.
#[test]
fn a_refused_retirement_reopens_into_the_surviving_channel() {
    let mut fixture = fixture();
    fixture.close_card();
    let _published = fixture.send_retirement();
    let sent = fixture
        .app
        .channel_work
        .retiring
        .get_mut(&fixture.channel)
        .unwrap();
    sent.1 = std::time::Instant::now() - super::super::channel_work::RETIREMENT_ANSWERED;

    fixture.reopen_pane();

    assert_eq!(fixture.app.durable.active_channel, Some(fixture.channel));
    assert_eq!(fixture.app.durable.channels.len(), 1);
}

/// After a restart forgets which scope a card closed from, the registry's
/// view still records which channel removed it.
#[test]
fn a_shared_view_names_the_channel_a_card_was_removed_from() {
    let mut fixture = fixture();
    let saved = fixture.saved();
    fixture.close_card();
    fixture.app.recently_closed.clear();
    fixture.app.workspace_model.views[0].panes.clear();
    fixture.app.workspace_model.views[0]
        .retired
        .push(fixture.card);
    let path = fixture.directory.path().to_path_buf();
    drop(fixture.app.select_worktree(path.clone(), false));
    assert_eq!(fixture.app.durable.active_channel, None);

    drop(fixture.app.resume_saved_conversation(saved.clone()));
    assert_eq!(fixture.app.durable.active_channel, Some(fixture.channel));

    // A pane that moved between views was removed from each it left, which
    // names no single one; the conversation then opens in its worktree.
    let mut fixture = self::fixture();
    let saved = SavedConversation {
        card: fixture.card,
        ..fixture.saved()
    };
    fixture.close_card();
    fixture.app.recently_closed.clear();
    fixture.app.workspace_model.views[0]
        .retired
        .push(fixture.card);
    fixture.app.workspace_model.views.push(View {
        scope: Scope::Worktree {
            repository: fixture.directory.path().into(),
            path: fixture.directory.path().into(),
        },
        revision: 1,
        panes: vec![],
        retired: vec![fixture.card],
    });
    let path = fixture.directory.path().to_path_buf();
    drop(fixture.app.select_worktree(path, false));
    drop(fixture.app.resume_saved_conversation(saved));
    assert_eq!(fixture.app.durable.active_channel, None);
}

/// A conversation this window never saw close, resumed from a channel in its
/// own directory, opens in that channel; one from elsewhere opens in its own
/// worktree.
#[test]
fn an_unknown_conversation_opens_in_the_channel_it_was_resumed_from() {
    let mut fixture = fixture();
    let found = SavedConversation {
        card: CardId::new(),
        ..fixture.saved()
    };
    drop(fixture.app.resume_saved_conversation(found.clone()));
    assert_eq!(fixture.app.durable.active_channel, Some(fixture.channel));
    let resumed = fixture.app.durable.focused.unwrap();
    assert!(fixture.active_channel().cards.contains(&resumed));

    let elsewhere = tempfile::tempdir().unwrap();
    let mut found = SavedConversation {
        card: CardId::new(),
        ..found
    };
    found.session.launch.session_id = Some("6c0d1f1e-7d84-4a8f-9f61-2f0a3b2c1d4e".into());
    found.session.launch.recipe.working_directory = elsewhere.path().into();
    drop(fixture.app.resume_saved_conversation(found));
    assert_eq!(fixture.app.durable.active_channel, None);
    assert_eq!(
        fixture.app.active_workspace.as_deref(),
        Some(elsewhere.path())
    );
}

/// The registry's answer arrives as soon as it decides: a kept channel is
/// entered again at once, rather than duplicated while the window would
/// otherwise still be waiting to learn whether it retired.
#[test]
fn a_kept_channel_answered_at_once_is_not_duplicated() {
    let mut fixture = fixture();
    fixture.close_card();
    let _published = fixture.send_retirement();
    fixture.answer_retirement(false);
    assert_eq!(fixture.app.durable.channels.len(), 1);

    fixture.reopen_pane();

    assert_eq!(fixture.app.durable.active_channel, Some(fixture.channel));
    assert_eq!(fixture.app.durable.channels.len(), 1);
    assert_eq!(fixture.active_channel().cards, vec![fixture.resumed().id]);
}

/// A channel deleted in another window, which this one learns only as its
/// absence, stays deleted, just as one deleted here does.
#[test]
fn a_channel_deleted_elsewhere_is_not_revived() {
    let mut fixture = fixture();
    let saved = fixture.saved();
    fixture.close_card();
    let channel = fixture.channel;
    fixture
        .app
        .workspace_model
        .channels
        .retain(|shared| shared.id != channel);
    fixture
        .app
        .workspace_model
        .views
        .retain(|view| view.scope != Scope::Channel(channel));
    drop(fixture.app.retire_peer_channel(channel));
    assert!(fixture.app.durable.channels.is_empty());
    let cards = fixture.app.durable.cards.len();

    fixture.reopen_pane();
    assert!(fixture.app.durable.channels.is_empty());
    assert_eq!(fixture.app.durable.cards.len(), cards);

    drop(fixture.app.resume_saved_conversation(saved));
    assert!(fixture.app.durable.channels.is_empty());
    assert_eq!(
        fixture.app.active_workspace.as_deref(),
        Some(fixture.directory.path())
    );
    assert_eq!(fixture.resumed().title, TITLE);
}

/// Reopening reaches back only within the project on screen: a channel that
/// retired in another project is not revived from here.
#[test]
fn reopening_in_another_project_leaves_a_retired_channel_alone() {
    let mut fixture = fixture();
    fixture.close_card();
    fixture.retire();
    let _other = fixture.enter_other_project();
    let cards = fixture.app.durable.cards.len();

    fixture.reopen_pane();

    assert!(fixture.app.durable.channels.is_empty());
    assert_eq!(fixture.app.durable.cards.len(), cards);
    assert_eq!(fixture.app.recently_closed.len(), 1);

    // Back in its own project, it still reopens into its channel.
    let path = fixture.directory.path().to_path_buf();
    drop(fixture.app.select_worktree(path, false));
    fixture.reopen_pane();
    assert_eq!(fixture.active_channel().title, "Investigation");
}

/// A pane that reopens outside the window leaves nothing to bring a channel
/// back for, so a revival made for it is retired again and remains available
/// to the next card closed in that channel.
#[cfg(not(feature = "integrated-browser"))]
#[test]
fn a_pane_reopened_outside_the_window_does_not_leave_an_empty_channel() {
    let mut fixture = fixture();
    let saved = fixture.saved();
    let mut browser = fixture.app.card_state(fixture.card).unwrap().clone();
    browser.id = CardId::new();
    let url = url::Url::parse("https://example.com/").unwrap();
    browser.kind = CardKind::Browser(crate::model::BrowserSession {
        id: crate::model::BrowserId::new(),
        current_url: url.clone(),
        history: vec![url],
        history_index: 0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        fidelity: crate::model::BrowserRestoreFidelity::HistoryAndScroll,
    });
    fixture.close_card();
    fixture.retire();
    fixture.app.recently_closed.push((
        browser,
        Some(WorkspaceDestination::Channel(fixture.channel)),
    ));

    let path = fixture.directory.path().to_path_buf();
    fixture.reopen_pane();

    assert_eq!(fixture.app.recently_closed.len(), 1);
    assert_eq!(
        fixture.app.active_workspace.as_deref(),
        Some(path.as_path())
    );
    assert!(
        fixture
            .app
            .durable
            .channels
            .iter()
            .all(|channel| channel.cards.is_empty()
                && Some(channel.id) != fixture.app.durable.active_channel),
        "no empty revival stays entered"
    );

    // The conversation closed before it still brings the channel back.
    drop(fixture.app.resume_saved_conversation(saved));
    assert_eq!(fixture.active_channel().title, "Investigation");
    assert_eq!(fixture.active_channel().cards, vec![fixture.resumed().id]);
}

/// Another window's edit can reach this one before the registry's answer,
/// so the answer's model is older than the channels the window folded since.
/// The answer still settles the retirement, and the card reopens into a
/// revival rather than into the channel the registry already deleted.
#[test]
fn a_retirement_answered_behind_newer_metadata_still_retires_the_channel() {
    let mut fixture = fixture();
    fixture.close_card();
    let _published = fixture.send_retirement();
    let answered = fixture
        .app
        .workspace_model
        .sequence
        .max(fixture.app.shared_views.snapshot_sequence)
        + 1;
    fixture.app.workspace_model.sequence = answered + 1;

    fixture.answer_retirement_at(true, answered);

    assert!(fixture.app.durable.channels.is_empty());
    assert!(
        fixture
            .app
            .workspace_model
            .channel(fixture.channel)
            .is_none()
    );
    fixture.reopen_pane();
    let revived = fixture.active_channel().clone();
    assert_ne!(revived.id, fixture.channel);
    assert_eq!(revived.title, "Investigation");
    assert_eq!(revived.cards, vec![fixture.resumed().id]);
}
