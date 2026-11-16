//! Exercise retirement through the registry's canonical edit path.
use super::super::*;
use crate::workspace_model::ChannelIncarnation;

fn fixture() -> (Registry, tempfile::TempDir, ChannelIncarnation, PathBuf) {
    let parent = std::env::temp_dir().canonicalize().unwrap();
    let directory = tempfile::tempdir_in(parent).unwrap();
    let mut registry = Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
    let repository = directory.path().join("project");
    fs::create_dir(&repository).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: repository.clone(),
            name: "Project".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let id = ChannelId::new();
    registry
        .apply(&ModelEdit::CreateChannel {
            id,
            repository: repository.clone(),
            title: "Original".into(),
            title_source: Default::default(),
        })
        .unwrap();
    let model = registry.model().unwrap();
    let home = ChannelIncarnation {
        registry: model.identity,
        channel: id,
        creation_ordinal: model.channel(id).unwrap().ordinal,
    };
    (registry, directory, home, repository)
}

#[test]
fn retirement_accepts_only_the_captured_empty_view() {
    let (mut registry, _directory, home, _repository) = fixture();
    let stale = registry
        .apply(&ModelEdit::RetireEmptyChannel {
            home,
            expected_view_revision: 5,
        })
        .unwrap();
    assert!(!stale.changed);
    assert!(stale.model.channel(home.channel).is_some());
    let applied = registry
        .apply(&ModelEdit::RetireEmptyChannel {
            home,
            expected_view_revision: 0,
        })
        .unwrap();
    assert!(applied.changed);
    assert!(applied.model.channel(home.channel).is_none());
}

#[test]
fn retirement_rejects_a_reused_channel_identity_and_another_registry() {
    let (mut registry, _directory, home, repository) = fixture();
    registry
        .apply(&ModelEdit::DeleteChannel { id: home.channel })
        .unwrap();
    let retire = ModelEdit::RetireEmptyChannel {
        home,
        expected_view_revision: 0,
    };
    assert!(registry.apply(&retire).is_err(), "the channel is gone");
    registry
        .apply(&ModelEdit::CreateChannel {
            id: home.channel,
            repository,
            title: "Replacement".into(),
            title_source: Default::default(),
        })
        .unwrap();
    let current = registry.model().unwrap();
    assert_ne!(
        current.channel(home.channel).unwrap().ordinal,
        home.creation_ordinal
    );
    assert!(registry.apply(&retire).is_err());
    let foreign = ModelEdit::RetireEmptyChannel {
        home: ChannelIncarnation {
            registry: Uuid::new_v4(),
            creation_ordinal: current.channel(home.channel).unwrap().ordinal,
            ..home
        },
        expected_view_revision: 0,
    };
    assert!(registry.apply(&foreign).is_err());
    assert_eq!(registry.model().unwrap(), current);
}

/// An open agent placed in an empty channel is a session waiting for a window
/// to show it, whether its adoption is still to come or a restore left it
/// undone. Retiring the channel would hide it. One whose pane was closed there
/// is what the person chose to end, and leaves the channel empty.
#[test]
fn retirement_keeps_a_channel_holding_a_detached_agent() {
    let (mut registry, _directory, home, _repository) = fixture();
    let agent = crate::agent_execution::Agent::fixture_placed_in(home.channel);
    registry
        .db
        .execute(
            "INSERT INTO agents(id, origin, record, closed) VALUES (?1, '\"User\"', ?2, 0)",
            params![agent.id.to_string(), serde_json::to_string(&agent).unwrap()],
        )
        .unwrap();
    let retire = ModelEdit::RetireEmptyChannel {
        home,
        expected_view_revision: 0,
    };
    let kept = registry.apply(&retire).unwrap();
    assert!(!kept.changed);
    assert!(kept.model.channel(home.channel).is_some());

    // Another channel's agent says nothing about this one.
    let mut elsewhere = agent.clone();
    elsewhere.placement = crate::agent_execution::Placement::Channel(ChannelId::new());
    registry
        .db
        .execute(
            "UPDATE agents SET record = ?2 WHERE id = ?1",
            params![
                agent.id.to_string(),
                serde_json::to_string(&elsewhere).unwrap()
            ],
        )
        .unwrap();
    // Its record names this channel only in its spec, which the exact
    // placement check sees past.
    assert!(registry.apply(&retire).unwrap().changed);
}

#[test]
fn retirement_accepts_an_agent_whose_pane_was_closed_there() {
    let (mut registry, _directory, home, _repository) = fixture();
    let agent = crate::agent_execution::Agent::fixture_placed_in(home.channel);
    registry
        .db
        .execute(
            "INSERT INTO agents(id, origin, record, closed) VALUES (?1, '\"User\"', ?2, 0)",
            params![agent.id.to_string(), serde_json::to_string(&agent).unwrap()],
        )
        .unwrap();
    // The person closed its pane there, which the view retires.
    let card = crate::model::CardId::from_uuid(agent.id.as_uuid());
    let scope = crate::workspace_model::views::Scope::Channel(home.channel);
    let seeded = registry
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: Vec::new(),
            closed: vec![card],
        })
        .unwrap();
    let view = seeded
        .model
        .views
        .iter()
        .find(|view| view.scope == scope)
        .unwrap();
    assert_eq!(view.retired, [card]);
    let applied = registry
        .apply(&ModelEdit::RetireEmptyChannel {
            home,
            expected_view_revision: view.revision,
        })
        .unwrap();
    assert!(applied.changed);
    assert!(applied.model.channel(home.channel).is_none());
}

#[test]
fn the_retired_channel_work_edit_is_refused() {
    let (mut registry, _directory, _home, _repository) = fixture();
    let before = registry.model().unwrap();
    let error = registry
        .apply(&ModelEdit::RetiredChannelWorkChanged)
        .unwrap_err();
    assert_eq!(error, crate::terminal_ipc::RETIRED_CHANNEL_WORK);
    assert_eq!(registry.model().unwrap(), before);
}
