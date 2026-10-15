//! Non-reused project identity fences, with no filesystem or process work.
use super::*;
use crate::project_recovery::ProjectInstance;

pub(super) fn require(db: &Connection, expected: &ProjectInstance) -> Result<(), String> {
    expected.validate()?;
    let identity: String = db
        .query_row("SELECT identity FROM metadata WHERE key=0", [], |row| {
            row.get(0)
        })
        .map_err(|error| error.to_string())?;
    let ordinal: Option<u32> = db
        .query_row(
            "SELECT ordinal FROM projects WHERE repository=?1",
            [repository_key(&expected.repository)?],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if identity != expected.registry.to_string() || ordinal != Some(expected.ordinal) {
        return Err("This project was replaced, removed, or opened by another registry. Its replacement and sessions were preserved; refresh Projects before trying again.".into());
    }
    Ok(())
}
pub(super) fn require_relocation(db:&Connection,expected:&ProjectInstance,relocation:&crate::project_recovery::Relocation)->Result<(),String> {
    require(db,expected)?;
    relocation.validate()?;
    if expected.repository!=relocation.repository { return Err("The located paths do not name the original project instance".into()); }
    let (kind,checkout,primary):(String,Option<String>,Option<String>)=db.query_row("SELECT kind,checkout,primary_checkout FROM projects WHERE repository=?1",[repository_key(&expected.repository)?],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).map_err(|error|error.to_string())?;
    let original=primary.or(checkout).map(PathBuf::from).or_else(|| {
        if kind=="folder" { Some(expected.repository.clone()) }
        else if expected.repository.file_name().is_some_and(|name|name==".git") { expected.repository.parent().map(Path::to_owned) }
        else { None }
    }).ok_or("The original checkout identity is unavailable. Refresh Projects before locating its folder.")?;
    if original!=relocation.old_root { return Err("The original project root changed while locating it. Refresh Projects and choose its folder again.".into()); }
    if relocation.destination!=relocation.repository && db.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE repository=?1)",[repository_key(&relocation.destination)?],|row|row.get::<_,bool>(0)).map_err(|error|error.to_string())? { return Err("The located folder is already open as another project".into()); }
    Ok(())
}

impl Registry {
    pub(crate) fn require_project_relocation(&self, expected:&ProjectInstance, relocation:&crate::project_recovery::Relocation) -> Result<(),String> {
        require_relocation(&self.db,expected,relocation)
    }

    pub(crate) fn project_instance(&self, repository: &Path) -> Result<ProjectInstance, String> {
        let registry: String = self.db.query_row("SELECT identity FROM metadata WHERE key=0", [], |row| row.get(0)).map_err(|error|error.to_string())?;
        let ordinal = self.db.query_row("SELECT ordinal FROM projects WHERE repository=?1", [repository_key(repository)?], |row|row.get(0)).map_err(|error|error.to_string())?;
        Ok(ProjectInstance { registry: registry.parse::<uuid::Uuid>().map_err(|error|error.to_string())?, repository:repository.to_owned(), ordinal })
    }

    #[cfg(test)]
    pub(crate) fn relocation_edit(&self, relocation: crate::project_recovery::Relocation) -> ModelEdit {
        ModelEdit::RelocateProject { project:self.project_instance(&relocation.repository).unwrap(), relocation }
    }

    pub(crate) fn require_project_instance(
        &self,
        expected: &ProjectInstance,
    ) -> Result<(), String> {
        require(&self.db, expected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn open(registry: &mut Registry) {
        registry
            .apply(&ModelEdit::OpenProject {
                repository: "/project".into(),
                name: "Project".into(),
                kind: ProjectKind::Folder,
                checkout: None,
                primary_checkout: None,
            })
            .unwrap();
    }
    #[test]
    fn guarded_removal_refuses_reopened_path_and_recreated_registry_with_same_rank() {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("first.sqlite3")).unwrap();
        open(&mut registry);
        let model = registry.model().unwrap();
        let expected = ProjectInstance {
            registry: model.identity,
            repository: "/project".into(),
            ordinal: model.projects[0].ordinal,
        };
        registry
            .apply(&ModelEdit::CloseProject {
                repository: "/project".into(),
            })
            .unwrap();
        open(&mut registry);
        let before = registry.model().unwrap();
        assert_ne!(before.projects[0].ordinal, expected.ordinal);
        assert!(
            registry
                .apply(&ModelEdit::CloseProjectInstance {
                    project: expected.clone()
                })
                .is_err()
        );
        assert_eq!(registry.model().unwrap(), before);
        let mut recreated = Registry::at(&directory.path().join("second.sqlite3")).unwrap();
        open(&mut recreated);
        let replacement = recreated.model().unwrap();
        assert_eq!(replacement.projects[0].ordinal, expected.ordinal);
        assert_ne!(replacement.identity, expected.registry);
        assert!(
            recreated
                .apply(&ModelEdit::CloseProjectInstance { project: expected })
                .is_err()
        );
        assert_eq!(recreated.model().unwrap(), replacement);
        let valid = ProjectInstance {
            registry: replacement.identity,
            repository: "/project".into(),
            ordinal: replacement.projects[0].ordinal,
        };
        assert!(
            recreated
                .apply(&ModelEdit::CloseProjectInstance { project: valid })
                .unwrap()
                .model
                .projects
                .is_empty()
        );
    }
    #[test]
    fn stale_locate_refuses_reopened_project_and_recreated_registry_before_any_rows_move() {
        use crate::project_recovery::Relocation;
        let directory=tempfile::tempdir().unwrap();
        let mut registry=Registry::at(&directory.path().join("first.sqlite3")).unwrap();
        open(&mut registry);
        let project=registry.project_instance(Path::new("/project")).unwrap();
        let edit=ModelEdit::RelocateProject { project:project.clone(),relocation:Relocation{repository:"/project".into(),destination:"/located".into(),old_root:"/project".into(),new_root:"/located".into()} };
        registry.apply(&ModelEdit::CloseProject{repository:"/project".into()}).unwrap();
        open(&mut registry);
        let before=registry.model().unwrap();
        assert!(registry.apply(&edit).unwrap_err().contains("replaced"));
        assert_eq!(registry.model().unwrap(),before);
        let mut recreated=Registry::at(&directory.path().join("replacement.sqlite3")).unwrap();
        open(&mut recreated);
        assert_eq!(recreated.project_instance(Path::new("/project")).unwrap().ordinal,project.ordinal);
        let before=recreated.model().unwrap();
        assert!(recreated.apply(&edit).unwrap_err().contains("replaced"));
        assert_eq!(recreated.model().unwrap(),before);
    }

    #[test]
    fn stale_same_key_primary_root_cannot_repair_a_later_located_root() {
        let directory=tempfile::tempdir().unwrap();
        let mut registry=Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
        registry.apply(&ModelEdit::OpenProject { repository:"/metadata".into(),name:"Project".into(),kind:ProjectKind::Git,checkout:Some("/new-primary".into()),primary_checkout:Some("/new-primary".into()) }).unwrap();
        let project=registry.project_instance(Path::new("/metadata")).unwrap();
        let relocation=crate::project_recovery::Relocation{repository:"/metadata".into(),destination:"/metadata".into(),old_root:"/old-primary".into(),new_root:"/replacement".into()};
        let before=registry.model().unwrap();
        assert!(registry.require_project_relocation(&project,&relocation).unwrap_err().contains("root changed"));
        assert!(registry.apply(&ModelEdit::RelocateProject{project,relocation}).is_err());
        assert_eq!(registry.model().unwrap(),before);
    }

}
