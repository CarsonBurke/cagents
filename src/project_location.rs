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
    pub checkout: Option<PathBuf>,
    pub primary_checkout: Option<PathBuf>,
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
            key: from.clone(),
            kind: project.kind,
            primary_checkout: project.primary_checkout.clone(),
            checkout: project
                .checkout
                .clone()
                .or_else(|| (project.kind == ProjectKind::Folder).then(|| from.clone())),
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
        let key = key.strip_suffix('\n').unwrap_or(&key);
        if key.is_empty() {
            return Err("git reported an empty repository path".into());
        }
        let key = fs::canonicalize(key)
            .map_err(|error| format!("could not resolve Git directory {key}: {error}"))?;
        let checkout = discover_checkout(&directory)?;
        let primary_checkout = checkout
            .as_deref()
            .and_then(|checkout| primary_checkout(checkout, &key));
        return Ok(ProjectLocation {
            key,
            kind: ProjectKind::Git,
            checkout,
            primary_checkout,
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
            key: directory.clone(),
            kind: ProjectKind::Folder,
            checkout: Some(directory),
            primary_checkout: None,
        });
    }
    Err(format!(
        "could not discover project {}: {}",
        directory.display(),
        diagnostic
    ))
}

fn discover_checkout(directory: &Path) -> Result<Option<PathBuf>, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--show-toplevel"])
        .env("LC_ALL", "C")
        .current_dir(directory)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not discover checkout: {error}"))?;
    if !output.status.success() {
        // A bare or metadata-only open has no discoverable checkout. Do not
        // guess from the metadata directory's spelling.
        return Ok(None);
    }
    let root = String::from_utf8(output.stdout)
        .map_err(|_| "git reported a checkout path that is not valid UTF-8")?;
    let root = root.strip_suffix('\n').unwrap_or(&root);
    fs::canonicalize(root)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// Filesystem identity for a verified checkout marker, including linked and
/// separate Git directories. No Git subprocess is needed during navigation.
pub(crate) fn checkout_identity(directory: &Path) -> Option<(PathBuf, PathBuf)> {
    use std::os::unix::ffi::OsStrExt;
    fn reference(bytes: &[u8]) -> &Path {
        Path::new(std::ffi::OsStr::from_bytes(
            bytes.strip_suffix(b"\n").unwrap_or(bytes),
        ))
    }
    let marker = directory.join(".git");
    let admin = if marker.is_dir() {
        fs::canonicalize(marker).ok()?
    } else {
        let content = fs::read(marker).ok()?;
        let path = reference(content.strip_prefix(b"gitdir: ")?);
        fs::canonicalize(directory.join(path)).ok()?
    };
    let common = match fs::read(admin.join("commondir")) {
        Ok(content) => fs::canonicalize(admin.join(reference(&content))).ok()?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => admin.clone(),
        Err(_) => return None,
    };
    Some((admin, common))
}

/// Only a checkout whose private Git directory is the common directory can
/// repair Git's separate-directory primary worktree record.
pub(crate) fn primary_checkout(directory: &Path, repository: &Path) -> Option<PathBuf> {
    let (admin, common) = checkout_identity(directory)?;
    (admin == common && common == repository)
        .then(|| fs::canonicalize(directory).ok())
        .flatten()
}

/// Choose an available checkout on the filesystem-owning worker. A retained
/// primary may disappear independently of usable linked worktrees.
pub(crate) fn read_checkout(
    repository: &Path,
    checkout: Option<&Path>,
    primary: Option<&Path>,
) -> Option<PathBuf> {
    primary
        .and_then(|root| primary_checkout(root, repository))
        .or_else(|| {
            let root = checkout?;
            let (_, common) = checkout_identity(root)?;
            (common == repository)
                .then(|| fs::canonicalize(root).ok())
                .flatten()
        })
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
        let private = ancestor.join(content.strip_prefix("gitdir:")?.trim());
        let private = fs::canonicalize(private).ok()?;
        let common = match fs::read_to_string(private.join("commondir")) {
            Ok(common) => fs::canonicalize(private.join(common.trim())).ok()?,
            Err(_) => private,
        };
        Some((ancestor.to_path_buf(), common))
    })
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
                checkout: Some(root.path().canonicalize().unwrap()),
                primary_checkout: None,
                key: root.path().canonicalize().unwrap(),
                kind: ProjectKind::Folder,
            }
        );
    }

    #[test]
    fn separate_git_directory_keeps_the_verified_checkout() {
        let root = tempfile::tempdir().unwrap();
        let checkout = root.path().join("checkout with spaces");
        let metadata = root.path().join("metadata elsewhere");
        let output = Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["init", "--separate-git-dir"])
            .arg(&metadata)
            .arg(&checkout)
            .output()
            .unwrap();
        assert!(output.status.success());
        let location = discover(&checkout).unwrap();
        assert_eq!(location.key, metadata.canonicalize().unwrap());
        assert_eq!(location.checkout, Some(checkout.canonicalize().unwrap()));
        assert_eq!(
            location.primary_checkout,
            Some(checkout.canonicalize().unwrap())
        );
        assert_eq!(discover(&metadata).unwrap().checkout, None);
    }

    #[cfg(any(feature = "ui", feature = "host"))]
    #[test]
    fn separate_primary_is_normalized_without_substituting_a_linked_checkout() {
        let root = tempfile::tempdir().unwrap();
        let checkout = root.path().join("checkout");
        let metadata = root.path().join("metadata");
        assert!(
            Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["init", "--separate-git-dir"])
                .arg(&metadata)
                .arg(&checkout)
                .output()
                .unwrap()
                .status
                .success()
        );
        assert!(
            Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .arg("-C")
                .arg(&checkout)
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-m",
                    "fixture"
                ])
                .output()
                .unwrap()
                .status
                .success()
        );
        let service = crate::repository::RepositoryService::system("git");
        let cancellation = crate::repository::CancellationToken::new();
        let info = service.discover(&checkout, &cancellation).unwrap();
        let records = service
            .list_worktrees_for_repository(&info, &cancellation)
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, checkout.canonicalize().unwrap());
        assert_eq!(
            records[0].id,
            crate::repository::stable_worktree_id(&records[0].path)
        );
        let linked = root.path().join("linked");
        assert!(
            Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .arg("-C")
                .arg(&checkout)
                .args(["worktree", "add", "-b", "linked"])
                .arg(&linked)
                .output()
                .unwrap()
                .status
                .success()
        );
        let linked_location = discover(&linked).unwrap();
        assert_eq!(
            linked_location.checkout,
            Some(linked.canonicalize().unwrap())
        );
        assert_eq!(linked_location.primary_checkout, None);
        let info = service.discover(&linked, &cancellation).unwrap();
        let records = service
            .list_worktrees_for_repository(&info, &cancellation)
            .unwrap();
        assert!(
            records
                .iter()
                .any(|record| record.path == linked.canonicalize().unwrap())
        );
        assert!(
            !records
                .iter()
                .any(|record| record.path == metadata.canonicalize().unwrap() && !record.bare)
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
            checkout: None,
            primary_checkout: None,
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
