use super::*;

fn create(id: ChannelId) -> ModelEdit {
    ModelEdit::CreateChannel {
        id,
        repository: "/repo".into(),
        title: "Channel".into(),
        title_source: Default::default(),
        checkout: None,
    }
}

fn setup() -> (Registry, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    registry
        .apply_delta(&ModelEdit::OpenProject {
            repository: "/repo".into(),
            name: "repo".into(),
            kind: Default::default(),
        })
        .unwrap();
    (registry, directory)
}

#[test]
fn lost_create_reply_does_not_resurrect_a_channel_deleted_before_retry() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model.sqlite3");
    let mut publisher = Registry::at(&path).unwrap();
    publisher
        .apply_delta(&ModelEdit::OpenProject {
            repository: "/repo".into(),
            name: "repo".into(),
            kind: Default::default(),
        })
        .unwrap();
    let id = ChannelId::new();
    let pending = create(id);
    // Commit the real request but discard its reply, leaving it uncertain for A.
    assert!(publisher.apply_delta(&pending).unwrap().changed);
    drop(publisher);

    let mut other_window = Registry::at(&path).unwrap();
    assert!(other_window.model().unwrap().channel(id).is_some());
    let deleted = other_window
        .apply_delta(&ModelEdit::DeleteChannel { id })
        .unwrap();
    assert!(deleted.changed);
    let after_delete = other_window.model().unwrap();
    assert_eq!(after_delete.projects.len(), 1, "keep the parent open");
    assert!(after_delete.channel(id).is_none());
    drop(other_window);

    // Reconnect, read the absent channel, then replay A's identical pending edit.
    let mut publisher = Registry::at(&path).unwrap();
    assert_eq!(publisher.model().unwrap(), after_delete);
    let replay = publisher.apply_delta(&pending).unwrap();
    assert!(
        !replay.changed,
        "retrying a committed creation must not undo a later deletion"
    );
    assert_eq!(replay.receipt.sequence, deleted.receipt.sequence);
    assert_eq!(replay.receipt.effects.removed_channels, vec![id]);
    assert!(replay.receipt.effects.channels.is_empty());
    assert_eq!(publisher.model().unwrap(), after_delete);
}

#[test]
fn deleted_identity_stays_retired_after_project_reopen_but_a_new_identity_can_open() {
    let (mut registry, directory) = setup();
    let id = ChannelId::new();
    let pending = create(id);
    registry.apply_delta(&pending).unwrap();
    let ordinal = registry.model().unwrap().channel(id).unwrap().ordinal;
    registry
        .apply_delta(&ModelEdit::DeleteChannel { id })
        .unwrap();
    registry
        .apply_delta(&ModelEdit::CloseProject {
            repository: "/repo".into(),
        })
        .unwrap();
    registry
        .apply_delta(&ModelEdit::ReopenProject {
            repository: "/repo".into(),
            name: "repo".into(),
            kind: Default::default(),
        })
        .unwrap();
    let before_retry = registry.model().unwrap();
    drop(registry);
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    let replay = registry.apply_delta(&pending).unwrap();
    assert!(!replay.changed);
    assert_eq!(replay.receipt.sequence, before_retry.sequence);
    assert_eq!(registry.model().unwrap(), before_retry);

    let fresh = ChannelId::new();
    assert!(registry.apply_delta(&create(fresh)).unwrap().changed);
    let model = registry.model().unwrap();
    assert!(model.channel(id).is_none());
    assert_eq!(model.channel(fresh).unwrap().ordinal, ordinal + 1);
}

#[test]
fn unknown_and_repeated_deletes_remain_noops() {
    let (mut registry, _directory) = setup();
    let id = ChannelId::new();
    let sequence = registry.model().unwrap().sequence;
    let unknown = registry
        .apply_delta(&ModelEdit::DeleteChannel { id })
        .unwrap();
    assert!(!unknown.changed);
    assert_eq!(unknown.receipt.sequence, sequence);
    assert!(registry.apply_delta(&create(id)).unwrap().changed);
    let deleted = registry
        .apply_delta(&ModelEdit::DeleteChannel { id })
        .unwrap();
    assert!(deleted.changed);
    let duplicate = registry
        .apply_delta(&ModelEdit::DeleteChannel { id })
        .unwrap();
    assert!(!duplicate.changed);
    assert_eq!(duplicate.receipt.sequence, deleted.receipt.sequence);
    assert_eq!(duplicate.receipt.effects.removed_channels, vec![id]);
    assert!(!registry.apply_delta(&create(id)).unwrap().changed);
    assert_eq!(
        registry
            .db
            .query_row("SELECT count(*) FROM deleted_channels", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn schema28_upgrade_preserves_live_channels_and_installs_the_deletion_fence() {
    let (mut registry, directory) = setup();
    let path = directory.path().join("model.sqlite3");
    let id = ChannelId::new();
    let pending = create(id);
    registry.apply_delta(&pending).unwrap();
    let before_upgrade = registry.model().unwrap();
    drop(registry);
    assert_eq!(
        Registry::downgrade_at(&path, 28).unwrap(),
        Some(SCHEMA_VERSION)
    );
    assert_eq!(Registry::stored_schema_at(&path).unwrap(), Some(28));

    let mut registry = Registry::at(&path).unwrap();
    assert_eq!(registry.model().unwrap(), before_upgrade);
    let deleted = registry
        .apply_delta(&ModelEdit::DeleteChannel { id })
        .unwrap();
    drop(registry);
    let mut registry = Registry::at(&path).unwrap();
    let replay = registry.apply_delta(&pending).unwrap();
    assert!(!replay.changed);
    assert_eq!(replay.receipt.sequence, deleted.receipt.sequence);
    assert!(registry.model().unwrap().channel(id).is_none());
}
