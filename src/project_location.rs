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
        // Git writes one output newline; whitespace before it belongs to the
        // directory name, including a newline at the end of that name.
        let key = key.strip_suffix('\n').unwrap_or(&key);
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
    placed(&directory, model)
}

#[cfg(feature = "host")]
fn placed(directory: &Path, model: &crate::workspace_model::WorkspaceModel) -> Placement {
    let checkout = checkout(directory);
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
    let worktree = checkout.map(|(worktree, _)| worktree);
    // A submodule or a clone nested in an open repository is not opened
    // itself; it belongs to the project of the checkout around it.
    let project = project.or_else(|| {
        worktree
            .as_deref()
            .and_then(Path::parent)
            .and_then(|parent| placed(parent, model).project)
    });
    Placement { worktree, project }
}

/// The working tree that contains `directory` and its Git common directory.
#[cfg(feature = "host")]
fn checkout(directory: &Path) -> Option<(PathBuf, PathBuf)> {
    directory.ancestors().find_map(|ancestor| {
        let marker = ancestor.join(".git");
        let metadata = fs::metadata(&marker).ok()?;
        if metadata.is_dir() {
            return Some((ancestor.to_path_buf(), fs::canonicalize(&marker).ok()?));
        }
        // A linked worktree or submodule names its private Git directory, and
        // that directory names the shared one it borrows objects and refs from.
        // A marker that names nothing readable is skipped, not trusted.
        let content = fs::read_to_string(&marker).ok()?;
        let private = ancestor.join(
            content
                .strip_prefix("gitdir: ")?
                .trim_end_matches(['\r', '\n']),
        );
        let private = fs::canonicalize(private).ok()?;
        let common = match fs::read_to_string(private.join("commondir")) {
            Ok(common) => {
                fs::canonicalize(private.join(common.trim_end_matches(['\r', '\n']))).ok()?
            }
            Err(_) => private,
        };
        Some((ancestor.to_path_buf(), common))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(directory: &Path, arguments: &[&std::ffi::OsStr]) {
        let mut command = Command::new("git");
        for (name, _) in std::env::vars_os() {
            if name.as_encoded_bytes().starts_with(b"GIT_") {
                command.env_remove(name);
            }
        }
        let output = command
            .args(arguments)
            .current_dir(directory)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn discovery_preserves_trailing_git_directory_whitespace() {
        let root = tempfile::tempdir().unwrap();
        for name in ["bare-space ", "bare-tab\t", "bare-newline\n"] {
            let directory = root.path().join(name);
            // A real decoy also catches silently adopting the trimmed path.
            for path in [directory.as_path(), &root.path().join(name.trim_end())] {
                git(
                    root.path(),
                    &[
                        "init".as_ref(),
                        "--quiet".as_ref(),
                        "--bare".as_ref(),
                        "--template=".as_ref(),
                        path.as_os_str(),
                    ],
                );
            }
            assert_eq!(
                discover(&directory).unwrap(),
                ProjectLocation {
                    key: directory.canonicalize().unwrap(),
                    kind: ProjectKind::Git,
                }
            );
        }
    }

    #[cfg(feature = "host")]
    #[test]
    fn placement_preserves_git_marker_and_common_directory_whitespace() {
        let root = tempfile::tempdir().unwrap();
        for suffix in [" ", "\t"] {
            let repository = root.path().join(format!("repository{suffix}"));
            let metadata = root.path().join(format!("metadata{suffix}"));
            git(
                root.path(),
                &[
                    "init".as_ref(),
                    "--quiet".as_ref(),
                    "--template=".as_ref(),
                    "--separate-git-dir".as_ref(),
                    metadata.as_os_str(),
                    repository.as_os_str(),
                ],
            );
            let key = metadata.canonicalize().unwrap();
            let mut model = crate::workspace_model::WorkspaceModel::default();
            model.projects.push(crate::workspace_model::ModelProject {
                repository: key.clone(),
                name: "repository".into(),
                kind: ProjectKind::Git,
                ordinal: 1,
            });
            assert_eq!(discover(&repository).unwrap().key, key);
            assert_eq!(
                place(&repository, &model),
                Placement {
                    worktree: Some(repository.canonicalize().unwrap()),
                    project: Some(key.clone()),
                }
            );

            git(
                &repository,
                &[
                    "-c".as_ref(),
                    "user.name=Fixture".as_ref(),
                    "-c".as_ref(),
                    "user.email=fixture@example.invalid".as_ref(),
                    "commit".as_ref(),
                    "--quiet".as_ref(),
                    "--allow-empty".as_ref(),
                    "-m".as_ref(),
                    "root".as_ref(),
                ],
            );
            let linked = root.path().join(format!("linked{suffix}"));
            git(
                &repository,
                &[
                    "worktree".as_ref(),
                    "add".as_ref(),
                    "--quiet".as_ref(),
                    "--detach".as_ref(),
                    linked.as_os_str(),
                ],
            );
            let marker = fs::read_to_string(linked.join(".git")).unwrap();
            let private = Path::new(
                marker
                    .strip_prefix("gitdir: ")
                    .unwrap()
                    .trim_end_matches(['\r', '\n']),
            );
            // Git accepts an absolute commondir, with CRLF as its terminator.
            fs::write(private.join("commondir"), format!("{}\r\n", key.display())).unwrap();
            assert_eq!(discover(&linked).unwrap().key, key);
            assert_eq!(
                place(&linked, &model),
                Placement {
                    worktree: Some(linked.canonicalize().unwrap()),
                    project: Some(key),
                }
            );
        }
    }

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
                project: Some(key.clone()),
            }
        );
        assert_eq!(place(root.path(), &model), Placement::default());
        // A submodule's marker names a private directory with no `commondir`,
        // so it is a checkout of its own inside the open one.
        let module = repository.join("vendor/lib");
        fs::create_dir_all(&module).unwrap();
        let private = repository.join(".git/modules/lib");
        fs::create_dir_all(&private).unwrap();
        fs::write(module.join(".git"), "gitdir: ../../.git/modules/lib\n").unwrap();
        assert_eq!(
            place(&module, &model),
            Placement {
                worktree: Some(module.canonicalize().unwrap()),
                project: Some(key),
            }
        );
    }
}
