#![cfg(any(feature = "host", feature = "ui"))]

//! Large real-Git pathspec coverage, kept in `--all-targets` without making every
//! library-suite run create and mutate 40,000 files. Run this target explicitly
//! when changing repository mutation/pathspec behavior.

use cagents::process;
use cagents::repository::{
    CancellationToken, CommandSpec, DiscardRequest, MutationOutcome, PathSelection,
    ProcessExecutor, ProcessFailure, ProcessOutput, RepoPath, RepositoryService, StageRequest,
    SystemProcessExecutor, UnstageRequest, selection_pathspecs,
};
use std::{fs, path::Path, sync::Arc};

#[path = "support/repository_scale.rs"]
mod scale_support;
use scale_support::isolated_large_pathspec_test;

struct IsolatedExecutor;

impl ProcessExecutor for IsolatedExecutor {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        SystemProcessExecutor.execute(
            &command
                .clone()
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Cagents Contract")
                .env("GIT_AUTHOR_EMAIL", "contract@example.invalid")
                .env("GIT_COMMITTER_NAME", "Cagents Contract")
                .env("GIT_COMMITTER_EMAIL", "contract@example.invalid"),
            cancellation,
        )
    }
}

fn service() -> RepositoryService<IsolatedExecutor> {
    RepositoryService::new("git", Arc::new(IsolatedExecutor))
}

fn path(value: &str) -> RepoPath {
    RepoPath::parse(value).unwrap()
}

fn git(directory: &Path, arguments: &[&str]) {
    // Keep the old fixture's read-command setup while using only public APIs.
    let command = CommandSpec::new("git")
        .arg("-C")
        .arg(directory)
        .args([
            "-c",
            "core.fsmonitor=false",
            "--no-optional-locks",
            "-c",
            "diff.autoRefreshIndex=false",
        ])
        .args(arguments.iter().copied())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR");
    IsolatedExecutor
        .execute(&command, &CancellationToken::new())
        .expect("Git fixture command executes")
        .require_success(Path::new("git"))
        .expect("Git fixture command succeeds");
}

#[test]
fn large_literal_pathspec_mutations_roundtrip_without_argv_limits() {
    isolated_large_pathspec_test(
        concat!(
            module_path!(),
            "::large_literal_pathspec_mutations_roundtrip_without_argv_limits"
        ),
        |root| {
            git(root, &["init", "-b", "main"]);
            let names = [
                "space name",
                "line\nname",
                "-leading",
                "[glob]*",
                ":(glob)magic",
            ];
            let selection: Vec<_> = names
                .iter()
                .map(|name| PathSelection {
                    path: path(name),
                    old_path: None,
                })
                .collect();
            for name in names {
                fs::write(root.join(name), "base").unwrap();
            }
            let service = service();
            let cancel = CancellationToken::new();
            assert!(matches!(
                service
                    .stage(
                        &StageRequest {
                            worktree: root.into(),
                            selection: selection.clone()
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            git(root, &["commit", "-m", "initial"]);
            for name in names {
                fs::write(root.join(name), "changed").unwrap();
            }
            assert!(matches!(
                service
                    .stage(
                        &StageRequest {
                            worktree: root.into(),
                            selection: selection.clone()
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            assert!(matches!(
                service
                    .unstage(
                        &UnstageRequest {
                            worktree: root.into(),
                            selection: selection.clone()
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            assert!(matches!(
                service
                    .discard(
                        &DiscardRequest {
                            worktree: root.into(),
                            tracked: selection,
                            untracked: vec![],
                            confirmed: true
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            for name in names {
                assert_eq!(fs::read_to_string(root.join(name)).unwrap(), "base");
            }

            // 40k names exceed Linux ARG_MAX even before argv pointer overhead.
            let mut selection = Vec::with_capacity(40_000);
            for index in 0..40_000 {
                let name = format!(
                    "generated_component_file_with_an_extraordinarily_long_name_{index:05}.txt"
                );
                fs::write(root.join(&name), "generated").unwrap();
                selection.push(PathSelection {
                    path: path(&name),
                    old_path: None,
                });
            }
            assert!(
                service
                    .stage_command(root, &selection_pathspecs(&selection))
                    .unwrap()
                    .stdin
                    .unwrap()
                    .len()
                    > 2_097_152
            );
            let outcome = service
                .stage(
                    &StageRequest {
                        worktree: root.into(),
                        selection: selection.clone(),
                    },
                    &cancel,
                )
                .unwrap();
            assert!(
                matches!(&outcome, MutationOutcome::Applied { .. }),
                "40k stage operation: {outcome:?}"
            );
            assert!(matches!(
                service
                    .unstage(
                        &UnstageRequest {
                            worktree: root.into(),
                            selection: selection.clone()
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            assert!(matches!(
                service
                    .discard(
                        &DiscardRequest {
                            worktree: root.into(),
                            tracked: vec![],
                            untracked: selection.iter().map(|entry| entry.path.clone()).collect(),
                            confirmed: true
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            assert!(
                !root
                    .join("generated_component_file_with_an_extraordinarily_long_name_00000.txt")
                    .exists()
            );
            // Exercise the >ARG_MAX tracked restore path as well as unlinking
            // untracked files: all three Git mutation operations use stdin.
            for entry in &selection {
                fs::write(root.join(entry.path.as_os_str()), "generated").unwrap();
            }
            assert!(matches!(
                service
                    .stage(
                        &StageRequest {
                            worktree: root.into(),
                            selection: selection.clone()
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            git(root, &["commit", "--quiet", "-m", "generated baseline"]);
            for entry in &selection {
                fs::write(root.join(entry.path.as_os_str()), "modified").unwrap();
            }
            assert!(matches!(
                service
                    .stage(
                        &StageRequest {
                            worktree: root.into(),
                            selection: selection.clone()
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            assert!(matches!(
                service
                    .unstage(
                        &UnstageRequest {
                            worktree: root.into(),
                            selection: selection.clone()
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            assert!(matches!(
                service
                    .discard(
                        &DiscardRequest {
                            worktree: root.into(),
                            tracked: selection.clone(),
                            untracked: vec![],
                            confirmed: true
                        },
                        &cancel
                    )
                    .unwrap(),
                MutationOutcome::Applied { .. }
            ));
            for entry in &selection {
                assert_eq!(
                    fs::read(root.join(entry.path.as_os_str())).unwrap(),
                    b"generated"
                );
            }
        },
    );
}
