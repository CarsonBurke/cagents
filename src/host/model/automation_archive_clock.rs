//! Archive discovery changes independently of ordinary live run activity.
use super::*;

impl Registry {
    pub(crate) fn automation_archive_sequence(&self) -> Result<u64, String> {
        self.db
            .query_row(
                "SELECT automation_archive_sequence FROM metadata WHERE key=0",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automation_archive_clock_overflow_refuses_projection_mutation_atomically() {
        let root = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&root.path().join("registry.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                checkout: None, primary_checkout: None,
                repository: root.path().into(),
                name: "Clock limit".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        registry
            .db
            .execute(
                "UPDATE metadata SET automation_archive_sequence=?1 WHERE key=0",
                [i64::MAX],
            )
            .unwrap();
        let before = registry.model().unwrap();
        let error = registry
            .apply(&ModelEdit::SeedView {
                scope: crate::agent_execution::Placement::Worktree {
                    repository: root.path().into(),
                    path: root.path().into(),
                },
                panes: vec![],
                closed: vec![],
            })
            .unwrap_err();
        assert!(!error.is_empty());
        assert_eq!(
            registry.model().unwrap(),
            before,
            "clock overflow cannot partially register a new display or wrap its authority"
        );
    }
}
