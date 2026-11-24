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
        let marker = if ancestor.file_name().is_some_and(|name| name == ".git") {
            ancestor.to_owned()
        } else {
            ancestor.join(".git")
        };
        if crate::git_marker::present(&marker)
            .map_err(|error| format!("could not inspect {}: {error}", marker.display()))?
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
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Placement {
    pub worktree: Option<PathBuf>,
    pub project: Option<PathBuf>,
}

#[cfg(feature = "host")]
pub(crate) fn place(path: &Path, model: &crate::workspace_model::WorkspaceModel) -> Placement {
    place_among(path, &model.projects)
}

/// [`place`] among `projects`, for a client that read the open projects
/// rather than the whole workspace model.
pub(crate) fn place_among(
    path: &Path,
    projects: &[crate::workspace_model::ModelProject],
) -> Placement {
    let Ok(directory) = fs::canonicalize(path) else {
        return Placement::default();
    };
    placed(&directory, &|candidate: &Path| {
        projects
            .iter()
            .any(|project| project.repository == candidate)
    })
}

fn placed(directory: &Path, opened: &dyn Fn(&Path) -> bool) -> Placement {
    let checkout = checkout(directory);
    // An open folder keeps its own identity, as in `resolve`, and the nearest
    // open project wins over an enclosing one: a folder inside the checkout,
    // then the checkout's repository, then a folder that encloses it.
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
            .and_then(|parent| placed(parent, opened).project)
    });
    Placement { worktree, project }
}

/// The working tree that contains `directory` and its Git common directory.
fn checkout(directory: &Path) -> Option<(PathBuf, PathBuf)> {
    directory.ancestors().find_map(|ancestor| {
        git_directories(ancestor).map(|directories| (ancestor.to_path_buf(), directories.common))
    })
}

/// Where Git keeps a checkout's metadata: its private directory, which holds
/// its index and `HEAD`, and the common directory it shares objects and refs
/// with every other worktree of the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitDirectories {
    pub private: PathBuf,
    pub common: PathBuf,
}

/// Reads `checkout`'s `.git` marker as Git's own discovery does, without a
/// Git process. A linked worktree or submodule names its private Git
/// directory, and that directory names the shared one it borrows objects and
/// refs from. `None` when the checkout has no marker, or one that names
/// nothing readable; such a marker is skipped, not trusted.
pub(crate) fn git_directories(checkout: &Path) -> Option<GitDirectories> {
    let marker = checkout.join(".git");
    if !crate::git_marker::present(&marker).ok()? {
        return None;
    }
    let private = if fs::metadata(&marker).ok()?.is_dir() {
        fs::canonicalize(&marker).ok()?
    } else {
        let content = fs::read_to_string(&marker).ok()?;
        fs::canonicalize(checkout.join(line(content.strip_prefix("gitdir: ")?))).ok()?
    };
    let common = match fs::read_to_string(private.join("commondir")) {
        Ok(common) => fs::canonicalize(private.join(line(&common))).ok()?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => private.clone(),
        Err(_) => return None,
    };
    Some(GitDirectories { private, common })
}

/// One line of a Git metadata file, which Git accepts with CRLF too.
fn line(content: &str) -> &str {
    content.trim_end_matches(['\r', '\n'])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(feature = "ui", feature = "host", target_os = "linux"))]
    use crate::process::{
        CancellationToken, CommandSpec, ProcessExecutor, ProcessOutput, SystemProcessExecutor,
    };

    #[cfg(any(feature = "ui", feature = "host", target_os = "linux"))]
    fn fixture_command(program: impl Into<PathBuf>) -> CommandSpec {
        let mut command = CommandSpec::new(program);
        for (name, _) in std::env::vars_os() {
            if name.as_encoded_bytes().starts_with(b"GIT_") {
                command = command.env_remove(name);
            }
        }
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .timeout(std::time::Duration::from_secs(30))
            .output_limits(64 * 1024, 64 * 1024)
    }

    #[cfg(any(feature = "ui", feature = "host", target_os = "linux"))]
    fn execute_fixture(command: CommandSpec) -> ProcessOutput {
        SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .and_then(|output| output.require_success(&command.program))
            .unwrap_or_else(|error| panic!("{error}: {}", error.reported_detail()))
    }

    #[cfg(any(feature = "ui", feature = "host", target_os = "linux"))]
    fn git(directory: &Path, arguments: &[&std::ffi::OsStr]) {
        execute_fixture(
            fixture_command("git")
                .args(arguments.iter().copied())
                .cwd(directory),
        );
    }

    // discover intentionally honors the caller's Git environment. Run these
    // fixtures in an isolated child so both setup and discovery see the same
    // controlled configuration without mutating the parallel test parent.
    #[cfg(any(feature = "ui", feature = "host", target_os = "linux"))]
    fn isolated_git_case(qualified: &str) -> bool {
        let case = qualified
            .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
            .unwrap_or(qualified);
        if std::env::var("CAGENTS_PROJECT_LOCATION_CASE").as_deref() == Ok(case) {
            return false;
        }
        let output = execute_fixture(
            fixture_command(std::env::current_exe().unwrap())
                .args([
                    case,
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                    "--color=never",
                ])
                .env("CAGENTS_PROJECT_LOCATION_CASE", case)
                .timeout(std::time::Duration::from_secs(90))
                .output_limits(256 * 1024, 256 * 1024),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&format!("test {case} ... ok"))
                && stdout.contains("test result: ok. 1 passed; 0 failed;"),
            "exact child did not run the owning regression: {stdout}"
        );
        print!("{stdout}");
        true
    }

    #[cfg(any(feature = "ui", feature = "host", target_os = "linux"))]
    #[test]
    fn discovery_preserves_trailing_git_directory_whitespace() {
        if isolated_git_case(concat!(
            module_path!(),
            "::discovery_preserves_trailing_git_directory_whitespace"
        )) {
            return;
        }
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
        if isolated_git_case(concat!(
            module_path!(),
            "::placement_preserves_git_marker_and_common_directory_whitespace"
        )) {
            return;
        }
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
            let marker_path = repository.join(".git");
            let original_marker = fs::read(&marker_path).unwrap();
            let enclosing = place(root.path(), &model);
            for prefix in ["gitdir:", "gitdir:\t", "gitdir:  "] {
                fs::write(&marker_path, format!("{prefix}{}\n", key.display())).unwrap();
                assert!(discover(&repository).is_err(), "Git rejects {prefix:?}");
                assert_eq!(place(&repository, &model), enclosing);
            }
            fs::write(&marker_path, original_marker).unwrap();

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
        fs::create_dir(root.path().join(".git")).unwrap();
        assert_eq!(git_directories(root.path()), None);
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
        fs::write(root.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(discover(root.path()).is_err());
        fs::remove_dir_all(root.path().join(".git")).unwrap();
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

    #[test]
    fn git_directories_follow_markers_and_commondir_as_git_does() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let common = root.join("repository/.git");
        let private = common.join("worktrees/linked");
        fs::create_dir_all(&private).unwrap();
        fs::write(common.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        // A linked worktree: a `gitdir: ` file naming its private directory
        // relative to the checkout, whose `commondir` is relative to it.
        let linked = root.join("linked");
        fs::create_dir_all(&linked).unwrap();
        fs::write(
            linked.join(".git"),
            "gitdir: ../repository/.git/worktrees/linked\r\n",
        )
        .unwrap();
        fs::write(private.join("commondir"), "../..\n").unwrap();
        assert_eq!(
            git_directories(&linked),
            Some(GitDirectories {
                private: private.clone(),
                common: common.clone(),
            })
        );

        // Without `commondir`, the private directory is the common one.
        assert_eq!(
            git_directories(&root.join("repository")),
            Some(GitDirectories {
                private: common.clone(),
                common: common.clone(),
            })
        );

        // A `.git` directory may name its common directory too, absolutely.
        let shared = root.join("shared");
        fs::create_dir_all(shared.join(".git")).unwrap();
        fs::write(shared.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            shared.join(".git/commondir"),
            format!("{}\n", common.display()),
        )
        .unwrap();
        assert_eq!(
            git_directories(&shared),
            Some(GitDirectories {
                private: shared.join(".git"),
                common: common.clone(),
            })
        );

        // A `commondir` that exists but cannot be read leaves the answer to
        // Git rather than guessing the private directory is the common one.
        fs::remove_file(private.join("commondir")).unwrap();
        fs::create_dir(private.join("commondir")).unwrap();
        assert_eq!(git_directories(&linked), None);

        // Git requires the prefix exactly.
        fs::write(linked.join(".git"), "gitdir:../repository/.git\n").unwrap();
        assert_eq!(git_directories(&linked), None);
    }
}
