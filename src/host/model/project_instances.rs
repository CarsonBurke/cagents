//! Non-reused project identity fences, with no filesystem or process work.
use super::*;
use crate::project_recovery::ProjectInstance;

pub(super) fn require(db: &Connection, expected: &ProjectInstance) -> Result<(), String> {
    expected.validate()?;
    let identity: String = db.query_row("SELECT identity FROM metadata WHERE key=0", [], |row| row.get(0)).map_err(|error| error.to_string())?;
    let ordinal: Option<u32> = db.query_row("SELECT ordinal FROM projects WHERE repository=?1", [repository_key(&expected.repository)?], |row| row.get(0)).optional().map_err(|error| error.to_string())?;
    if identity != expected.registry.to_string() || ordinal != Some(expected.ordinal) {
        return Err("This project was replaced, removed, or opened by another registry. Its replacement and sessions were preserved; refresh Projects before trying again.".into());
    }
    Ok(())
}
impl Registry {
    pub(crate) fn require_project_instance(&self, expected: &ProjectInstance) -> Result<(), String> { require(&self.db, expected) }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn open(registry: &mut Registry) {
        registry.apply(&ModelEdit::OpenProject { repository: "/project".into(), name: "Project".into(), kind: ProjectKind::Folder, checkout: None, primary_checkout: None }).unwrap();
    }
    #[test]
    fn guarded_removal_refuses_reopened_path_and_recreated_registry_with_same_rank() {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("first.sqlite3")).unwrap();
        open(&mut registry);
        let model = registry.model().unwrap();
        let expected = ProjectInstance { registry: model.identity, repository: "/project".into(), ordinal: model.projects[0].ordinal };
        registry.apply(&ModelEdit::CloseProject { repository: "/project".into() }).unwrap();
        open(&mut registry);
        let before = registry.model().unwrap();
        assert_ne!(before.projects[0].ordinal, expected.ordinal);
        assert!(registry.apply(&ModelEdit::CloseProjectInstance { project: expected.clone() }).is_err());
        assert_eq!(registry.model().unwrap(), before);
        let mut recreated = Registry::at(&directory.path().join("second.sqlite3")).unwrap();
        open(&mut recreated);
        let replacement = recreated.model().unwrap();
        assert_eq!(replacement.projects[0].ordinal, expected.ordinal);
        assert_ne!(replacement.identity, expected.registry);
        assert!(recreated.apply(&ModelEdit::CloseProjectInstance { project: expected }).is_err());
        assert_eq!(recreated.model().unwrap(), replacement);
        let valid = ProjectInstance { registry: replacement.identity, repository: "/project".into(), ordinal: replacement.projects[0].ordinal };
        assert!(recreated.apply(&ModelEdit::CloseProjectInstance { project: valid }).unwrap().model.projects.is_empty());
    }
}
