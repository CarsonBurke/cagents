use super::*;

#[test]
fn model_snapshot_keeps_one_sequence_across_independent_commits() {
    let directory = tempfile::tempdir().unwrap();
    let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    let before = registry.model().unwrap();
    let mut other = registry.execution_connection().unwrap();
    let repository = directory.path().to_owned();
    pause_at(Pause::SequenceRead, move || {
        other
            .apply(&ModelEdit::OpenProject {
                repository,
                name: "Concurrent".into(),
                kind: Default::default(),
            })
            .unwrap();
    });
    assert_eq!(registry.model().unwrap(), before);
    let after = registry.model().unwrap();
    assert_eq!(after.sequence, before.sequence + 1);
    assert_eq!(after.projects.len(), 1);
}

#[test]
fn applied_sequence_belongs_to_its_commit_even_if_snapshot_is_newer() {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    let mut other = registry.execution_connection().unwrap();
    let repository = directory.path().to_owned();
    pause_at(Pause::AfterCommit, move || {
        other
            .apply(&ModelEdit::OpenProject {
                repository,
                name: "Concurrent".into(),
                kind: Default::default(),
            })
            .unwrap();
    });
    let applied = registry
        .apply(&ModelEdit::OpenProject {
            repository: directory.path().into(),
            name: "Original".into(),
            kind: Default::default(),
        })
        .unwrap();
    assert_eq!(applied.sequence, 1);
    assert_eq!(applied.model.sequence, 2);
    assert_eq!(applied.model.projects[0].name, "Concurrent");
}
