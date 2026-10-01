//! Shared filesystem discovery for project opens in the CLI and native UI.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::model::ProjectKind;

#[derive(Debug, PartialEq, Eq)]
pub struct ProjectLocation {
    pub key: PathBuf,
    pub kind: ProjectKind,
}

/// Preserve an already-open folder identity before interpreting new Git
/// metadata. Otherwise checkouts and linked worktrees resolve to one Git key.
pub(crate) fn resolve(
    path: &Path,
    model: Option<&crate::workspace_model::WorkspaceModel>,
) -> Result<ProjectLocation, String> {
    let from = fs::canonicalize(path).map_err(|error| error.to_string())?;
    if let Some(project) = model.and_then(|model| model.project(&from)) {
        return Ok(ProjectLocation {
            key: from,
            kind: project.kind,
        });
    }
    discover(&from)
}

/// Git projects use the common metadata directory; plain folders use their
/// canonical directory. Only Git's specific no-repository answer permits the
/// latter, and existing metadata must not be hidden by that fallback.
pub fn discover(path: &Path) -> Result<ProjectLocation, String> {
    let directory = fs::canonicalize(path)
        .map_err(|error| format!("could not resolve folder {}: {error}", path.display()))?;
    if !directory.is_dir() {
        return Err(format!("`{}` is not a directory", directory.display()));
    }
    let output = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .env("LC_ALL", "C")
        .current_dir(&directory)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if output.status.success() {
        let key = String::from_utf8(output.stdout)
            .map_err(|_| "git reported a repository path that is not valid UTF-8".to_owned())?;
        let key = key.trim_end();
        if key.is_empty() {
            return Err("git reported an empty repository path".into());
        }
        return Ok(ProjectLocation {
            key: fs::canonicalize(key)
                .map_err(|error| format!("could not resolve Git directory {key}: {error}"))?,
            kind: ProjectKind::Git,
        });
    }
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    let diagnostic = diagnostic.trim();
    let missing = diagnostic
        == "fatal: not a git repository (or any of the parent directories): .git"
        || (diagnostic
            .starts_with("fatal: not a git repository (or any parent up to mount point ")
            && diagnostic.ends_with(
                ")\nStopping at filesystem boundary (GIT_DISCOVERY_ACROSS_FILESYSTEM not set).",
            ));
    if missing && !has_git_metadata(&directory)? {
        // Verify that the selected folder is readable as well as searchable.
        fs::read_dir(&directory)
            .map_err(|error| format!("could not read folder {}: {error}", directory.display()))?;
        return Ok(ProjectLocation {
            key: directory,
            kind: ProjectKind::Folder,
        });
    }
    Err(format!(
        "could not discover project {}: {}",
        directory.display(),
        diagnostic
    ))
}

fn exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("could not inspect {}: {error}", path.display())),
    }
}

fn has_git_metadata(directory: &Path) -> Result<bool, String> {
    if ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return Ok(true);
    }
    for ancestor in directory.ancestors() {
        if ancestor.file_name().is_some_and(|name| name == ".git")
            || exists(&ancestor.join(".git"))?
            || (exists(&ancestor.join("HEAD"))?
                && (exists(&ancestor.join("objects"))? || exists(&ancestor.join("refs"))?))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Where a directory sits: the checkout containing it and the open project it
/// belongs to. Read from the filesystem as Git discovers a repository, without
/// running Git, so listing many sessions stays cheap.
#[cfg(feature = "host")]
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Placement {
    pub worktree: Option<PathBuf>,
    pub project: Option<PathBuf>,
}

#[cfg(feature = "host")]
pub(crate) fn place(path: &Path, model: &crate::workspace_model::WorkspaceModel) -> Placement {
    let Ok(directory) = fs::canonicalize(path) else {
        return Placement::default();
    };
    let checkout = checkout(&directory);
    // An open folder keeps its own identity, as in `resolve`, and the nearest
    // open project wins over an enclosing one: a folder inside the checkout,
    // then the checkout's repository, then a folder that encloses it.
    let opened = |candidate: &Path| model.project(candidate).is_some();
    let (inner, outer) = match &checkout {
        Some((worktree, _)) => directory
            .ancestors()
            .partition::<Vec<_>, _>(|ancestor| ancestor.starts_with(worktree)),
        None => (Vec::new(), directory.ancestors().collect()),
    };
    let project = inner
        .into_iter()
        .find(|ancestor| opened(ancestor))
        .map(Path::to_path_buf)
        .or_else(|| {
            checkout
                .as_ref()
                .map(|(_, common)| common.clone())
                .filter(|common| opened(common))
        })
        .or_else(|| {
            outer
                .into_iter()
                .find(|ancestor| opened(ancestor))
                .map(Path::to_path_buf)
        });
    Placement {
        worktree: checkout.map(|(worktree, _)| worktree),
        project,
    }
}

/// The working tree that contains `directory` and its Git common directory.
#[cfg(feature = "host")]
fn checkout(directory: &Path) -> Option<(PathBuf, PathBuf)> {
    for ancestor in directory.ancestors() {
        let marker = ancestor.join(".git");
        let Ok(metadata) = fs::metadata(&marker) else {
            continue;
        };
        if metadata.is_dir() {
            return Some((ancestor.to_path_buf(), fs::canonicalize(&marker).ok()?));
        }
        // A linked worktree or submodule names its private Git directory, and
        // that directory names the shared one it borrows objects and refs from.
        let content = fs::read_to_string(&marker).ok()?;
        let private = ancestor.join(content.strip_prefix("gitdir:")?.trim());
        let private = fs::canonicalize(private).ok()?;
        let common = match fs::read_to_string(private.join("commondir")) {
            Ok(common) => fs::canonicalize(private.join(common.trim())).ok()?,
            Err(_) => private,
        };
        return Some((ancestor.to_path_buf(), common));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_folders_have_canonical_folder_identity() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("folder")).unwrap();
        let selected = root.path().join("folder/..");
        assert_eq!(
            discover(&selected).unwrap(),
            ProjectLocation {
                key: root.path().canonicalize().unwrap(),
                kind: ProjectKind::Folder,
            }
        );
    }

    #[test]
    fn broken_git_metadata_is_not_reinterpreted_as_a_folder() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        assert!(discover(root.path()).is_err());
        fs::remove_dir(root.path().join(".git")).unwrap();
        fs::write(
            root.path().join(".git"),
            "gitdir: /missing/cagents-test-git\n",
        )
        .unwrap();
        assert!(discover(root.path()).is_err());
    }

    #[cfg(feature = "host")]
    #[test]
    fn placement_follows_linked_worktrees_to_their_open_project_without_git() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        fs::create_dir_all(repository.join("src")).unwrap();
        let git = |arguments: &[&str]| {
            let status = Command::new("git")
                .args(arguments)
                .current_dir(&repository)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {arguments:?}");
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "root",
        ]);
        git(&["worktree", "add", "-q", "../linked"]);
        let key = repository.join(".git").canonicalize().unwrap();
        let mut model = crate::workspace_model::WorkspaceModel::default();
        assert_eq!(
            place(&repository.join("src"), &model),
            Placement {
                worktree: Some(repository.canonicalize().unwrap()),
                project: None,
            },
            "a checkout outside every open project has no project"
        );
        model.projects.push(crate::workspace_model::ModelProject {
            repository: key.clone(),
            name: "repository".into(),
            kind: ProjectKind::Git,
            ordinal: 1,
        });
        let linked = root.path().join("linked").canonicalize().unwrap();
        assert_eq!(
            place(&linked, &model),
            Placement {
                worktree: Some(linked),
                project: Some(key),
            }
        );
        assert_eq!(place(root.path(), &model), Placement::default());
    }
}
