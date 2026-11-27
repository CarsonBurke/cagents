//! The directory an agent's provider works in.
//!
//! It bounds what the provider approves and lets its child write without
//! asking, so it must lie in the agent's project. A path cannot hold that by
//! itself: whatever writes inside the project can replace the directory with a
//! link that leads out of it. So the daemon opens the directory with no link
//! in its path, judges the one it opened, records which it was, and starts
//! every provider there, whoever asks for the start.
//! Agents and automations share this, including what "in the project" means.

use super::SharedRegistry;
use crate::{
    agent_execution::{Agent, DirectoryIdentity, Origin},
    host::automations::target::Identity as AutomationDirectoryIdentity,
    model::AgentId,
    workspace_model::ModelProject,
};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Opens `path` as the directory it names. A link in any component is
/// refused, so the directory is where its path says and nowhere else.
pub(in crate::host) fn open(path: &Path) -> Result<Arc<File>, String> {
    crate::chat::protocol::ExecutionContext::capture_for_start(path)
        .map_err(|error| match link_in(path) {
            Some(link) if link == path => format!(
                "{} is a symbolic link, not the directory itself",
                path.display()
            ),
            Some(link) => format!(
                "{} is reached through the symbolic link {}, so it is not where its path says",
                path.display(),
                link.display()
            ),
            None => error.to_string(),
        })?
        .directory
        .ok_or_else(|| format!("Cannot hold the directory {} open", path.display()))
}

/// The outermost link in `path`, to say why it did not open. The refusal does
/// not depend on it: the open decides, and this only names what it met.
fn link_in(path: &Path) -> Option<PathBuf> {
    let mut ancestors: Vec<_> = path.ancestors().collect();
    ancestors.reverse();
    ancestors
        .into_iter()
        .find(|ancestor| {
            ancestor
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
        })
        .map(Path::to_owned)
}

pub(in crate::host) fn identity(directory: &File) -> Result<DirectoryIdentity, String> {
    directory
        .metadata()
        .map(|metadata| DirectoryIdentity::from_metadata(&metadata))
        .map_err(|error| error.to_string())
}

/// Opens `path` if it still names the directory `expected` identifies.
pub(in crate::host) fn reopen(
    path: &Path,
    expected: DirectoryIdentity,
) -> Result<Arc<File>, String> {
    let directory = open(path)?;
    if identity(&directory)? != expected {
        return Err(changed(path));
    }
    Ok(directory)
}

fn changed(path: &Path) -> String {
    format!(
        "The approved agent directory changed: {} no longer names it",
        path.display()
    )
}

/// Whether `directory` is in the open project `repository`: its checkout, one
/// of its worktrees, or a folder in them. Projects can nest, so a directory is
/// in the nearest open project that contains it, as `sessions` and the CLI
/// name it. An open folder inside a repository is its own project, while a
/// submodule or nested clone belongs to the repository around it.
pub(in crate::host) fn ensure_in_project(
    directory: &Path,
    repository: &Path,
    projects: &[ModelProject],
) -> Result<(), String> {
    // A directory outside a project that is not open is not the mistake to
    // report: the project is.
    if !projects
        .iter()
        .any(|project| project.repository == repository)
    {
        return Err("The target project is not open".into());
    }
    let containing = crate::project_location::place_among(directory, projects).project;
    if containing.as_deref() != Some(repository) {
        return Err(format!(
            "{} is not in the project {}; an agent works only in a directory inside its project",
            directory.display(),
            crate::workspace_model::primary_checkout(repository).display()
        ));
    }
    Ok(())
}

/// Opens `path` for work in the project `repository`.
pub(in crate::host) fn admit(
    path: &Path,
    repository: &Path,
    projects: &[ModelProject],
) -> Result<Arc<File>, String> {
    let directory = open(path)?;
    judge(path, &directory, repository, projects)?;
    Ok(directory)
}

/// Judges `directory`, opened at `path`, for work in the project `repository`
/// and returns which directory it is. The rule reads the path, so the path
/// must name the opened directory both before and after: what was judged is
/// then what is held.
fn judge(
    path: &Path,
    directory: &File,
    repository: &Path,
    projects: &[ModelProject],
) -> Result<DirectoryIdentity, String> {
    ensure_in_project(path, repository, projects)?;
    let judged = identity(directory)?;
    reopen(path, judged)?;
    Ok(judged)
}

/// An agent's directory, opened to start a provider in it.
enum Judged {
    Agent(DirectoryIdentity),
    Automation(AutomationDirectoryIdentity),
}

pub(in crate::host) struct Opened {
    pub(in crate::host) directory: Arc<File>,
    /// What to record once a provider runs there: a directory judged now
    /// that is not the recorded one.
    judged: Option<(AgentId, Judged)>,
}

impl Opened {
    /// Records the directory now that the agent's provider has started in
    /// it. Until then the record names where a running provider is, which a
    /// refused start must not change.
    pub(in crate::host) fn started(&self, registry: &SharedRegistry) -> Result<(), String> {
        let Some((agent, judged)) = &self.judged else {
            return Ok(());
        };
        let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        match judged {
            Judged::Agent(identity) => registry.record_agent_directory(*agent, *identity),
            Judged::Automation(identity) => {
                registry.record_automation_agent_directory(*agent, identity)
            }
        }
    }
}

/// Opens `agent`'s directory to start a provider in it. The record names the
/// directory the agent was last judged in, which needs no second judgment.
/// Any other directory at its path gets one, as a new agent's would, and is
/// recorded in its place once the provider runs there: a worktree can be
/// removed and made again, so a directory that is not the recorded one is not
/// by itself an escape. One reached through a link never opens. An agent
/// accepted before directories were recorded is judged the same way.
///
/// An automation's agent runs unattended in the directory its automation was
/// saved with. When available, its filesystem evidence distinguishes a
/// boot that renumbered devices from a replaced target.
pub(in crate::host) fn open_for_start(
    registry: &SharedRegistry,
    agent: &Agent,
) -> Result<Opened, String> {
    let path = &agent.spec.target.directory;
    let directory = open(path)?;
    let judged = if matches!(agent.origin, Origin::Automation { .. }) {
        let current =
            AutomationDirectoryIdentity::capture(&directory).map_err(|error| error.to_string())?;
        let recorded = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .automation_agent_directory(agent.id)?;
        if let Some(recorded) = recorded {
            let matches = if recorded.has_fingerprint() {
                recorded.matches(&current)
            } else {
                // Older records had no boot evidence and re-judge on device
                // changes. A successful start now records current evidence.
                recorded.raw().inode == current.raw().inode
            };
            if !matches {
                return Err(changed(path));
            }
            if recorded == current {
                return Ok(Opened {
                    directory,
                    judged: None,
                });
            }
        }
        Judged::Automation(current)
    } else {
        let found = identity(&directory)?;
        if registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .agent_directory(agent.id)?
            == Some(found)
        {
            return Ok(Opened {
                directory,
                judged: None,
            });
        }
        Judged::Agent(found)
    };
    let projects = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .projects()?;
    judge(path, &directory, &agent.spec.target.repository, &projects)?;
    Ok(Opened {
        directory,
        judged: Some((agent.id, judged)),
    })
}

/// Opens the directory of the open agent whose conversation `session` starts
/// or reopens, when it is an agent's. A window, an update or an account switch
/// may start that conversation as the agent service does, and the provider
/// then serves the agent: it starts in the agent's directory or not at all.
pub(in crate::host) fn for_conversation(
    registry: &SharedRegistry,
    session: &crate::chat::Session,
) -> Result<Option<Opened>, String> {
    let agent = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        // A reopened conversation runs under a new ID in its predecessor's
        // lineage, or in place under the ID its agent is bound to. One that
        // several agents claim is adopted by none of them.
        let mut agents = registry.lineage_agents(session.id)?;
        match agents.len() {
            0 => registry.agent_for_session(session.id)?,
            1 => agents.pop(),
            _ => None,
        }
    };
    // An agent in a terminal runs where its terminal does.
    let Some(agent) = agent.filter(|agent| {
        !matches!(
            agent.session,
            Some(crate::agent_execution::HostedSession::Terminal(_))
        )
    }) else {
        return Ok(None);
    };
    let path = &session.launch.recipe.working_directory;
    if *path == agent.spec.target.directory {
        return open_for_start(registry, &agent).map(Some);
    }
    // A conversation the agent took over where it already ran keeps that
    // directory. It is judged as the agent's own is; the record stays the
    // agent's own, which the agent service's prompts still require.
    let projects = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .projects()?;
    Ok(Some(Opened {
        directory: admit(path, &agent.spec.target.repository, &projects)?,
        judged: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProjectKind;

    fn project(repository: &Path, kind: ProjectKind) -> ModelProject {
        ModelProject {
            repository: repository.to_owned(),
            name: "fixture".into(),
            kind,
            ordinal: 0,
        }
    }

    fn root() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = crate::fixture_tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        (directory, root)
    }

    /// #4327: the escape is a link put where the approved directory was, at
    /// its own name or at a directory above it. Neither opens, and a new
    /// directory at the old name is not the one that was approved.
    #[test]
    fn a_link_or_replacement_at_the_approved_path_is_not_the_approved_directory() {
        let (_held, root) = root();
        let work = root.join("project/nested/work");
        std::fs::create_dir_all(&work).unwrap();
        let outside = root.join("outside");
        std::fs::create_dir_all(outside.join("work")).unwrap();
        let projects = [project(&root.join("project"), ProjectKind::Folder)];
        let approved = admit(&work, &root.join("project"), &projects).unwrap();
        let recorded = identity(&approved).unwrap();
        assert!(reopen(&work, recorded).is_ok());

        // The directory itself becomes a link out of the project.
        std::fs::rename(&work, root.join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, &work).unwrap();
        assert!(open(&work).is_err());
        assert!(reopen(&work, recorded).is_err());
        assert!(admit(&work, &root.join("project"), &projects).is_err());
        // The descriptor already held still names the approved directory.
        assert_eq!(identity(&approved).unwrap(), recorded);

        // A directory above it becomes one, so the old spelling resolves.
        std::fs::remove_file(&work).unwrap();
        std::fs::rename(root.join("project/nested"), root.join("nested")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("project/nested")).unwrap();
        assert!(work.is_dir(), "the path still names a directory");
        assert!(reopen(&work, recorded).is_err());
        assert!(admit(&work, &root.join("project"), &projects).is_err());

        // Another real directory at the same path was never approved.
        std::fs::remove_file(root.join("project/nested")).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let error = reopen(&work, recorded).unwrap_err();
        assert!(error.contains("directory changed"), "{error}");
        // Put back, it is the approved directory again.
        std::fs::remove_dir(&work).unwrap();
        std::fs::rename(root.join("moved"), &work).unwrap();
        assert!(reopen(&work, recorded).is_ok());
    }

    /// The one rule agents and automations share. A folder project stays a
    /// folder after `git init`, a repository owns its linked worktrees and the
    /// clones nested in it, and the nearest open project wins.
    #[test]
    fn a_directory_is_in_the_nearest_open_project_that_contains_it() {
        use crate::process::{CancellationToken, CommandSpec, ProcessExecutor};
        let (_held, root) = root();
        let checkout = root.join("checkout");
        std::fs::create_dir_all(checkout.join("src")).unwrap();
        let git = |directory: &Path, arguments: &[&str]| {
            crate::process::SystemProcessExecutor
                .execute(
                    &CommandSpec::new("git")
                        .args(
                            [
                                "-c",
                                "user.name=Fixture",
                                "-c",
                                "user.email=fixture@example.invalid",
                            ]
                            .into_iter()
                            .chain(arguments.iter().copied()),
                        )
                        .cwd(directory),
                    &CancellationToken::new(),
                )
                .unwrap()
        };
        git(&checkout, &["init", "--quiet"]);
        git(
            &checkout,
            &["commit", "--quiet", "--allow-empty", "-m", "initial"],
        );
        let linked = root.join("linked");
        git(
            &checkout,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ],
        );
        let nested = checkout.join("vendor/clone");
        std::fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "--quiet"]);

        let repository = checkout.join(".git");
        let as_git = [project(&repository, ProjectKind::Git)];
        for inside in [&checkout, &checkout.join("src"), &linked, &nested] {
            assert_eq!(
                ensure_in_project(inside, &repository, &as_git),
                Ok(()),
                "{inside:?}"
            );
        }
        let error = ensure_in_project(&root, &repository, &as_git).unwrap_err();
        assert!(error.contains("is not in the project"), "{error}");

        // Opened as a folder before it had a repository, the checkout is the
        // project, and the worktree Git keeps outside it is not in it.
        let as_folder = [project(&checkout, ProjectKind::Folder)];
        assert_eq!(
            ensure_in_project(&checkout.join("src"), &checkout, &as_folder),
            Ok(())
        );
        assert!(ensure_in_project(&linked, &checkout, &as_folder).is_err());

        // A folder opened inside the repository is its own project.
        let both = [
            project(&repository, ProjectKind::Git),
            project(&checkout.join("src"), ProjectKind::Folder),
        ];
        assert!(ensure_in_project(&checkout.join("src"), &repository, &both).is_err());
        assert_eq!(
            ensure_in_project(&checkout.join("src"), &checkout.join("src"), &both),
            Ok(())
        );

        assert_eq!(
            ensure_in_project(&checkout, &root.join("closed"), &as_git).unwrap_err(),
            "The target project is not open"
        );
    }
}
