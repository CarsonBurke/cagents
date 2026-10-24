//! Exercise the real canonical edit hooks, rather than invoking leaf lifecycle
//! helpers after an edit and accidentally hiding a missing production hook.
use super::*;
use crate::{
    channel_work::{Change, Home, HomeLifecycle, Mutation, Origin, Page, Role, Target},
    github_identity::{GitHubRepoKey, PullRequestKey},
};

fn fixture() -> (Registry, tempfile::TempDir, Home, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
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
    let home = Home {
        registry: model.identity,
        channel: id,
        creation_ordinal: model.channel(id).unwrap().ordinal,
    };
    (registry, directory, home, repository)
}

fn user() -> channel_work::TrustedResolvedOrigin {
    channel_work::TrustedResolvedOrigin {
        origin: Origin::User,
        proof: None,
    }
}

fn claim(home: Home) -> Mutation {
    Mutation {
        home,
        expected_revision: 0,
        operation: Uuid::new_v4(),
        change: Change::Claim {
            target: Target::PullRequest(
                PullRequestKey::new(
                    GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
                    1,
                )
                .unwrap(),
            ),
            role: Role::Steward,
        },
    }
}

#[test]
fn canonical_empty_retirement_preserves_work_history_and_rejects_reused_channel_identity() {
    let (mut registry, _directory, home, repository) = fixture();
    let request = claim(home);
    registry.apply_channel_work(&user(), &request).unwrap();
    let retire = ModelEdit::RetireEmptyChannel {
        home,
        expected_view_revision: 0,
    };
    let retained = registry.apply(&retire).unwrap();
    assert!(!retained.changed);
    assert!(retained.model.channel(home.channel).is_some());
    registry
        .apply(&ModelEdit::DeleteChannel { id: home.channel })
        .unwrap();
    let history = registry.read_channel_work(&home, Page::default()).unwrap();
    assert_eq!(history.lifecycle, HomeLifecycle::Deleted);
    assert!(history.entries.is_empty());
    assert_eq!(history.history.len(), 1);
    assert!(history.history[0].user_ended);
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
    assert_eq!(registry.model().unwrap(), current);
    assert_eq!(
        registry.read_channel_work(&home, Page::default()).unwrap(),
        history
    );
}

#[test]
fn canonical_project_close_preserves_work_and_original_receipt_without_restoring_current_claims() {
    let (mut registry, _directory, home, repository) = fixture();
    let request = claim(home);
    let applied = registry.apply_channel_work(&user(), &request).unwrap();
    registry
        .apply(&ModelEdit::RetitleChannel {
            id: home.channel,
            title: "Last title".into(),
        })
        .unwrap();
    let before = registry.model().unwrap().sequence;
    let closed = registry
        .apply(&ModelEdit::CloseProject { repository })
        .unwrap();
    assert_eq!(closed.model.sequence, before + 1);
    assert!(closed.model.channel(home.channel).is_none());
    let retained = registry.read_channel_work(&home, Page::default()).unwrap();
    retained.validate().unwrap();
    assert_eq!(retained.lifecycle, HomeLifecycle::Deleted);
    assert!(retained.entries.is_empty());
    assert_eq!(retained.history.len(), 1);
    assert!(retained.history[0].claims.iter().all(|claim| !claim.active));
    assert_eq!(
        registry
            .channel_work_receipt(&user(), &home, request.operation)
            .unwrap(),
        Some(applied.receipt)
    );
    let title: String = registry
        .db
        .query_row(
            "SELECT title FROM channel_work_homes WHERE channel=?1 AND incarnation=?2",
            params![home.channel.to_string(), home.creation_ordinal],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(title, "Last title");
}

#[test]
fn canonical_empty_retirement_accepts_only_the_captured_empty_view_and_no_work() {
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
    let retained = registry.read_channel_work(&home, Page::default()).unwrap();
    assert_eq!(retained.lifecycle, HomeLifecycle::Deleted);
    assert!(retained.entries.is_empty() && retained.history.is_empty());
}
