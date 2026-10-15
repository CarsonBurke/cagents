//! Preserve discovery information that cannot be reconstructed from a separate Git directory.
use rusqlite::Connection;

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    let present: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('projects') WHERE name = 'checkout')",
        [], |row| row.get(0),
    ).map_err(|error| error.to_string())?;
    if !present {
        db.execute_batch("ALTER TABLE projects ADD COLUMN checkout TEXT;")
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::*;

    fn open(repository: &str) -> ModelEdit {
        ModelEdit::OpenProject { repository: repository.into(), checkout: None, name: "project".into(), kind: ProjectKind::Git }
    }

    #[test]
    fn checkout_survives_metadata_only_upserts_and_registry_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        let mut registry = Registry::at(&path).unwrap();
        let mut edit = open("/metadata/project.git");
        if let ModelEdit::OpenProject { checkout, .. } = &mut edit {
            *checkout = Some("/checkouts/project".into());
        }
        registry.apply(&edit).unwrap();
        let before = registry.model().unwrap();
        registry.apply(&open("/metadata/project.git")).unwrap();
        let after = registry.model().unwrap();
        assert_eq!(after.projects, before.projects);
        assert_eq!(after.sequence, before.sequence);
        drop(registry);
        let restored = Registry::at(&path).unwrap().model().unwrap();
        assert_eq!(restored.projects, before.projects);
    }

    #[test]
    fn version_twelve_rows_migrate_without_guessing_a_checkout() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        let mut registry = Registry::at(&path).unwrap();
        registry.apply(&open("/metadata/project.git")).unwrap();
        registry.db.execute_batch("ALTER TABLE projects DROP COLUMN checkout; PRAGMA user_version = 12;").unwrap();
        drop(registry);
        let restored = Registry::at(&path).unwrap();
        assert!(restored.model().unwrap().projects[0].checkout.is_none());
        assert_eq!(restored.db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0)).unwrap(), SCHEMA_VERSION);
    }
}
