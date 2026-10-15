//! Preserve discovery information that cannot be reconstructed from a separate Git directory.
use rusqlite::Connection;

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    for column in ["checkout", "primary_checkout"] {
    let present: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('projects') WHERE name = ?1)",
        [column], |row| row.get(0),
    ).map_err(|error| error.to_string())?;
    if !present {
        db.execute_batch(&format!("ALTER TABLE projects ADD COLUMN {column} TEXT;"))
            .map_err(|error| error.to_string())?;
    }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::*;

    fn open(repository: &str) -> ModelEdit {
        ModelEdit::OpenProject { repository: repository.into(), checkout: None, primary_checkout: None, name: "project".into(), kind: ProjectKind::Git }
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
        registry.db.execute_batch("ALTER TABLE projects DROP COLUMN checkout; ALTER TABLE projects DROP COLUMN primary_checkout; PRAGMA user_version = 12;").unwrap();
        drop(registry);
        let restored = Registry::at(&path).unwrap();
        assert!(restored.model().unwrap().projects[0].checkout.is_none());
        assert!(restored.model().unwrap().projects[0].primary_checkout.is_none());
        assert_eq!(restored.db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0)).unwrap(), SCHEMA_VERSION);
    }
    #[test]
    fn separate_primary_survives_linked_open_restart_and_navigation() {
        let directory = tempfile::tempdir().unwrap();
        let primary = directory.path().join("primary checkout");
        let metadata = directory.path().join("separate metadata");
        let linked = directory.path().join("linked checkout");
        let git = |cwd: &std::path::Path, args: &[&std::ffi::OsStr]| {
            let output = std::process::Command::new("git").current_dir(cwd).args(args).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        };
        git(directory.path(), &["init".as_ref(), "--separate-git-dir".as_ref(), metadata.as_os_str(), primary.as_os_str()]);
        git(&primary, &["-c".as_ref(), "user.name=Fixture".as_ref(), "-c".as_ref(), "user.email=fixture@example.invalid".as_ref(), "commit".as_ref(), "--allow-empty".as_ref(), "-m".as_ref(), "fixture".as_ref()]);
        git(&primary, &["worktree".as_ref(), "add".as_ref(), "-b".as_ref(), "linked".as_ref(), linked.as_os_str()]);
        let path = directory.path().join("registry.sqlite3");
        let mut registry = Registry::at(&path).unwrap();
        for checkout in [&primary, &linked] {
            let location = crate::project_location::discover(checkout).unwrap();
            let edit = ModelEdit::ReopenProject {
                repository: location.key, checkout: location.checkout,
                primary_checkout: location.primary_checkout, name: "project".into(), kind: ProjectKind::Git,
            }.canonicalized().unwrap();
            registry.apply(&edit).unwrap();
        }
        let invalid = ModelEdit::ReopenProject {
            repository: metadata.canonicalize().unwrap(), checkout: Some(linked.clone()),
            primary_checkout: Some(linked.clone()), name: "project".into(), kind: ProjectKind::Git,
        }.canonicalized();
        assert!(matches!(invalid.unwrap(), ModelEdit::ReopenProject { primary_checkout: None, .. }), "a linked checkout cannot claim the primary row");
        let before = registry.model().unwrap();
        assert_eq!(before.projects.len(), 1);
        assert_eq!(before.projects[0].checkout, Some(linked.canonicalize().unwrap()));
        assert_eq!(before.projects[0].primary_checkout, Some(primary.canonicalize().unwrap()));
        registry.apply(&open(metadata.to_str().unwrap())).unwrap();
        assert_eq!(registry.model().unwrap().sequence, before.sequence);
        drop(registry);
        let restored = Registry::at(&path).unwrap().model().unwrap();
        assert_eq!(restored.projects, before.projects);
        let project = &restored.projects[0];
        let reply = crate::device_service::execute(crate::device_service::Operation::ProjectNavigation {
            repository: project.repository.clone(), kind: project.kind,
            checkout: project.checkout.clone(), primary_checkout: project.primary_checkout.clone(),
        }).unwrap();
        let crate::device_service::Reply::ProjectNavigation(navigation) = reply else { panic!("navigation response"); };
        let paths: Vec<_> = navigation.worktrees.into_iter().map(|row| row.path).collect();
        assert!(paths.contains(&primary.canonicalize().unwrap()));
        assert!(paths.contains(&linked.canonicalize().unwrap()));
        assert!(!paths.contains(&metadata.canonicalize().unwrap()));
        assert_eq!(navigation.repository, metadata.canonicalize().unwrap());

        std::fs::remove_dir_all(&primary).unwrap();
        let edit = ModelEdit::ReopenProject { repository: project.repository.clone(),
            checkout: project.checkout.clone(), primary_checkout: project.primary_checkout.clone(),
            name: "project".into(), kind: ProjectKind::Git }.canonicalized().unwrap();
        assert!(matches!(edit, ModelEdit::ReopenProject { primary_checkout: None, .. }));
        let read_root = crate::project_location::read_checkout(&project.repository,
            project.checkout.as_deref(), project.primary_checkout.as_deref()).unwrap();
        assert_eq!(read_root, linked.canonicalize().unwrap());
        let service = crate::repository::RepositoryService::system("git");
        let cancellation = crate::process::CancellationToken::new();
        let info = service.discover(&read_root, &cancellation).unwrap();
        let rows = service.list_worktrees_for_repository(&info, &cancellation).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, linked.canonicalize().unwrap());
        let reply = crate::device_service::execute(crate::device_service::Operation::ProjectNavigation {
            repository: project.repository.clone(), kind: project.kind,
            checkout: project.checkout.clone(), primary_checkout: project.primary_checkout.clone(),
        }).unwrap();
        let crate::device_service::Reply::ProjectNavigation(navigation) = reply else { panic!("navigation response"); };
        assert_eq!(navigation.worktrees.len(), 1);
        assert_eq!(navigation.worktrees[0].path, linked.canonicalize().unwrap());
    }

}
