//! Pure recorded-conversation placement over a worker's physical path snapshot.
//! Filesystem reads and protection acquisition belong to the selection worker.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) enum Destination {
    Channel(crate::model::ChannelId),
    Worktree(PathBuf),
    NewChannel { project: ProjectId, root: PathBuf },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Project {
    id: ProjectId,
    repository: PathBuf,
    scope: PathBuf,
}

impl Project {
    fn ownership_root(&self) -> &Path {
        // Some project descriptors use /repo/.git while the primary working
        // tree is /repo. Match the known working-tree scope, not its admin dir.
        if sidebar::is_primary_worktree(&self.repository, &self.scope) {
            &self.scope
        } else {
            &self.repository
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) struct Snapshot {
    original: Option<(crate::model::ChannelId, Option<PathBuf>)>,
    worktrees: Vec<PathBuf>,
    projects: Vec<Project>,
    fallback: Option<(Destination, Option<PathBuf>)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) struct Resolved {
    pub destination: Destination,
    /// Exact lexical scope to select after its physical lease is installed.
    pub root: PathBuf,
    pub physical_root: PathBuf,
}

/// Conservative recorded-account partition shared with saved draft metadata.
/// An unknown selector never falls back to a provider-only conversation key.
pub(in crate::app) fn conversation_key(session: &chat::Session) -> Option<String> {
    chat::conversation_metadata_key(session)
}

// Identical to select_channel's lexical scope choice; this performs no IO.
fn project_scope(project: &sidebar::SidebarProject) -> PathBuf {
    project
        .worktrees
        .iter()
        .find(|worktree| sidebar::is_primary_worktree(&project.repository, &worktree.path))
        .map(|worktree| worktree.path.clone())
        .unwrap_or_else(|| {
            project
                .repository
                .parent()
                .filter(|parent| sidebar::is_primary_worktree(&project.repository, parent))
                .unwrap_or(&project.repository)
                .to_path_buf()
        })
}

impl Snapshot {
    /// Capture only immutable ownership metadata, never filesystem state.
    pub(in crate::app) fn capture(
        app: &Cagents,
        original: CardId,
        session: &chat::Session,
    ) -> Self {
        let channel_root = |id| {
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == id)
                .and_then(|channel| app.sidebar_model.project(channel.project_id))
                .map(project_scope)
        };
        let original = app
            .durable
            .channels
            .iter()
            .find(|channel| channel.cards.contains(&original))
            .map(|channel| channel.id)
            .or_else(|| {
                conversation_key(session)
                    .and_then(|key| app.durable.conversation_channels.get(&key).copied())
                    .filter(|id| app.durable.channels.iter().any(|channel| channel.id == *id))
            })
            .map(|id| (id, channel_root(id)));
        let mut worktrees = app
            .known_worktree_roots
            .iter()
            .cloned()
            .chain(app.sidebar_model.projects.iter().flat_map(|project| {
                project
                    .worktrees
                    .iter()
                    .map(|worktree| worktree.path.clone())
            }))
            .collect::<Vec<_>>();
        worktrees.sort();
        worktrees.dedup();
        let mut projects = app
            .sidebar_model
            .projects
            .iter()
            .map(|project| Project {
                id: project.id,
                repository: project.repository.clone(),
                scope: project_scope(project),
            })
            .collect::<Vec<_>>();
        projects.sort_by_key(|project| (project.repository.clone(), project.id));
        let fallback = app
            .durable
            .active_channel
            .map(|id| (Destination::Channel(id), channel_root(id)))
            .or_else(|| {
                app.active_workspace
                    .as_ref()
                    .map(|path| (Destination::Worktree(path.clone()), Some(path.clone())))
            });
        Self {
            original,
            worktrees,
            projects,
            fallback,
        }
    }

    pub(in crate::app) fn still_current(
        &self,
        app: &Cagents,
        original: CardId,
        session: &chat::Session,
    ) -> bool {
        self == &Self::capture(app, original, session)
    }

    /// Watch every possible owner, not only the path eventually selected.
    pub(in crate::app) fn required_paths(&self, recorded: &Path) -> Vec<PathBuf> {
        let mut paths = vec![recorded.to_path_buf()];
        paths.extend(self.worktrees.iter().cloned());
        paths.extend(
            self.projects
                .iter()
                .flat_map(|project| [project.repository.clone(), project.scope.clone()]),
        );
        paths.extend(self.original.iter().filter_map(|(_, root)| root.clone()));
        paths.extend(self.fallback.iter().filter_map(|(_, root)| root.clone()));
        paths.sort();
        paths.dedup();
        paths
    }
}

/// The worker supplies fresh, successfully canonicalized mappings and its
/// directory preflight. Missing mappings are unknown, never lexical authority.
/// Run again after acquiring the chosen physical root and compare both results
/// and mappings before installing a lease or creating any channel.
pub(in crate::app) fn resolve(
    recorded: &Path,
    allow_missing_directory: bool,
    snapshot: &Snapshot,
    canonical: &[(PathBuf, PathBuf)],
    directory_available: bool,
) -> Result<Resolved, String> {
    if !allow_missing_directory && !directory_available {
        return Err("This conversation's project folder is unavailable. Restore the folder before resuming.".into());
    }
    let physical = |root: &Path| {
        canonical
            .iter()
            .find(|(lexical, _)| lexical == root)
            .map(|(_, physical)| physical)
    };
    let directory = physical(recorded);
    let choose = |destination: Destination, root: Option<&Path>| -> Result<Resolved, String> {
        let root = root.ok_or("The recorded channel's project is no longer available")?;
        let physical_root = physical(root)
            .ok_or("The recorded owner cannot be resolved. Restore its folder before resuming.")?
            .clone();
        Ok(Resolved {
            destination,
            root: root.to_path_buf(),
            physical_root,
        })
    };
    if let Some((channel, root)) = &snapshot.original {
        return choose(Destination::Channel(*channel), root.as_deref());
    }
    let matches = |root: &Path| {
        directory
            .zip(physical(root))
            .is_some_and(|(cwd, owner)| cwd.starts_with(owner))
            || directory.is_none() && allow_missing_directory && recorded.starts_with(root)
    };
    let rank = |root: &Path| {
        (
            physical(root).map_or_else(
                || root.components().count(),
                |path| path.components().count(),
            ),
            recorded.starts_with(root),
            root.to_path_buf(),
        )
    };
    // An unresolved known lexical owner must not silently file into current scope.
    if let Some(root) = snapshot
        .worktrees
        .iter()
        .filter(|root| recorded.starts_with(root))
        .max_by_key(|root| root.components().count())
        && physical(root).is_none()
    {
        return choose(Destination::Worktree(root.clone()), Some(root));
    }
    if let Some(root) = snapshot
        .worktrees
        .iter()
        .filter(|root| matches(root))
        .max_by_key(|root| rank(root))
    {
        return choose(Destination::Worktree(root.clone()), Some(root));
    }
    if let Some(project) = snapshot
        .projects
        .iter()
        .filter(|project| recorded.starts_with(project.ownership_root()))
        .max_by_key(|project| project.ownership_root().components().count())
        && physical(project.ownership_root()).is_none()
    {
        return Err(
            "The recorded project cannot be resolved. Restore its folder before resuming.".into(),
        );
    }
    if let Some(project) = snapshot
        .projects
        .iter()
        .filter(|project| matches(project.ownership_root()))
        .max_by_key(|project| rank(project.ownership_root()))
    {
        return choose(
            Destination::NewChannel {
                project: project.id,
                root: project.scope.clone(),
            },
            Some(&project.scope),
        );
    }
    if let Some((destination, root)) = &snapshot.fallback {
        return choose(destination.clone(), root.as_deref());
    }
    choose(
        Destination::Worktree(recorded.to_path_buf()),
        Some(recorded),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            original: None,
            worktrees: Vec::new(),
            projects: Vec::new(),
            fallback: None,
        }
    }
    fn classify(recorded: &Path, snapshot: &Snapshot) -> Resolved {
        let canonical = snapshot
            .required_paths(recorded)
            .into_iter()
            .filter_map(|lexical| {
                lexical
                    .canonicalize()
                    .ok()
                    .map(|physical| (lexical, physical))
            })
            .collect::<Vec<_>>();
        resolve(recorded, false, snapshot, &canonical, recorded.is_dir()).unwrap()
    }
    #[cfg(unix)]
    #[test]
    fn physical_recorded_cwd_matches_deepest_known_alias_worktree() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("project");
        let worktree = root.join("linked");
        let nested = worktree.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let alias = directory.path().join("linked-alias");
        std::os::unix::fs::symlink(&worktree, &alias).unwrap();
        let mut scope = snapshot();
        scope.worktrees.push(alias.clone());
        scope.projects.push(Project {
            id: ProjectId::new(),
            scope: root.clone(),
            repository: root,
        });
        let resolved = classify(&nested, &scope);
        assert_eq!(resolved.destination, Destination::Worktree(alias.clone()));
        assert_eq!(resolved.root, alias);
        assert_eq!(resolved.physical_root, worktree);
    }
    #[test]
    fn unknown_recorded_cwd_uses_current_scope_without_rewriting_cwd() {
        let recorded = tempfile::tempdir().unwrap();
        let current = tempfile::tempdir().unwrap();
        let mut scope = snapshot();
        scope.fallback = Some((
            Destination::Worktree(current.path().into()),
            Some(current.path().into()),
        ));
        let resolved = classify(recorded.path(), &scope);
        assert_eq!(
            resolved.destination,
            Destination::Worktree(current.path().into())
        );
        assert_eq!(resolved.physical_root, current.path());
    }
    #[test]
    fn missing_known_owner_errors_instead_of_using_current_fallback() {
        let mut scope = snapshot();
        let alias = PathBuf::from("/known/alias");
        scope.worktrees.push(alias.clone());
        scope.fallback = Some((
            Destination::Worktree("/current".into()),
            Some("/current".into()),
        ));
        let canonical = vec![("/current".into(), "/physical/current".into())];
        assert!(resolve(&alias.join("history"), true, &scope, &canonical, false).is_err());
    }
    #[test]
    fn git_admin_project_spelling_matches_its_real_primary_working_tree() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("project");
        let admin = root.join(".git");
        let recorded = root.join("src");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&recorded).unwrap();
        let current = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        app.sidebar_model
            .projects
            .push(sidebar::SidebarProject::new(
                project,
                crate::repository::RepositoryId::new(),
                "Recorded project".into(),
                admin,
                Vec::new(),
                Vec::new(),
            ));
        app.known_worktree_roots.clear();
        app.active_workspace = Some(current.path().to_owned());
        let session = chat::fresh_session_deferred_policy(
            crate::agent::Provider::Claude,
            TerminalId::new(),
            recorded.clone(),
            &crate::account::Account::system(crate::agent::Provider::Claude),
        )
        .unwrap();
        let scope = Snapshot::capture(&app, CardId::new(), &session);
        let resolved = classify(&recorded, &scope);
        assert_eq!(
            resolved.destination,
            Destination::NewChannel {
                project,
                root: root.clone()
            }
        );
        assert_eq!(resolved.root, root);
        assert_eq!(resolved.physical_root, root.canonicalize().unwrap());
    }

    #[test]
    fn project_matching_creates_only_an_intent_for_its_exact_channel_scope() {
        let project = ProjectId::new();
        let mut scope = snapshot();
        scope.projects.push(Project {
            id: project,
            repository: "/repo/.bare".into(),
            scope: "/repo/main".into(),
        });
        let canonical = vec![
            ("/recorded/subdir".into(), "/physical/repo/subdir".into()),
            ("/repo/.bare".into(), "/physical/repo".into()),
            ("/repo/main".into(), "/physical/main".into()),
        ];
        let result = resolve(
            Path::new("/recorded/subdir"),
            false,
            &scope,
            &canonical,
            true,
        )
        .unwrap();
        assert_eq!(
            result.destination,
            Destination::NewChannel {
                project,
                root: "/repo/main".into()
            }
        );
        assert_eq!(result.physical_root, Path::new("/physical/main"));
    }
}
