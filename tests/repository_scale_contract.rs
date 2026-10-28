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
use std::{
    collections::HashSet,
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

#[path = "support/repository_scale.rs"]
mod scale_support;
use scale_support::{diagnostic, isolated_large_pathspec_test, phase};

struct IsolatedExecutor;

impl ProcessExecutor for IsolatedExecutor {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        static NEXT_COMMAND: AtomicUsize = AtomicUsize::new(1);
        let sequence = NEXT_COMMAND.fetch_add(1, Ordering::Relaxed);
        // Whitelist command classes rather than emitting paths or arguments.
        let operation = command
            .arguments
            .iter()
            .filter_map(|argument| argument.to_str())
            .find(|argument| {
                matches!(
                    *argument,
                    "init" | "add" | "commit" | "rev-parse" | "ls-files" | "reset" | "restore"
                )
            })
            .unwrap_or("other");
        let stdin_bytes = command.stdin.as_ref().map_or(0, Vec::len);
        diagnostic(format_args!(
            "PATHSPEC_GIT_BEGIN sequence={sequence} command={operation} argc={} stdin_bytes={stdin_bytes} timeout_ms={:?}",
            command.arguments.len(),
            command.timeout.map(|timeout| timeout.as_millis())
        ));
        let started = Instant::now();
        let result = SystemProcessExecutor.execute(
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
        );
        match &result {
            Ok(output) => diagnostic(format_args!(
                "PATHSPEC_GIT_END sequence={sequence} command={operation} elapsed_ms={} success={} exit_code={:?} signal={:?}",
                started.elapsed().as_millis(),
                output.success,
                output.exit_code,
                output.signal
            )),
            Err(failure) => diagnostic(format_args!(
                "PATHSPEC_GIT_END sequence={sequence} command={operation} elapsed_ms={} failure={:?}",
                started.elapsed().as_millis(),
                failure.kind
            )),
        }
        result
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
            phase("initialize", 0, || {
                git(root, &["init", "-b", "main"]);
            });
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
            phase("unusual_create", names.len(), || {
                for name in names {
                    fs::write(root.join(name), "base").unwrap();
                }
            });
            let service = service();
            let cancel = CancellationToken::new();
            phase("unusual_stage_initial", selection.len(), || {
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
            });
            phase("unusual_commit", names.len(), || {
                git(root, &["commit", "-m", "initial"]);
            });
            phase("unusual_modify", names.len(), || {
                for name in names {
                    fs::write(root.join(name), "changed").unwrap();
                }
            });
            phase("unusual_stage_changed", selection.len(), || {
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
            });
            phase("unusual_unstage", selection.len(), || {
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
            });
            phase("unusual_tracked_discard", selection.len(), || {
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
            });
            phase("unusual_verify", names.len(), || {
                for name in names {
                    assert_eq!(fs::read_to_string(root.join(name)).unwrap(), "base");
                }
            });

            // 40k names exceed Linux ARG_MAX even before argv pointer overhead.
            let mut selection = Vec::with_capacity(40_000);
            phase("bulk_create", 40_000, || {
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
            });
            phase("bulk_stdin_proof", selection.len(), || {
                assert!(
                    service
                        .stage_command(root, &selection_pathspecs(&selection))
                        .unwrap()
                        .stdin
                        .unwrap()
                        .len()
                        > 2_097_152
                );
            });
            phase("bulk_stage_new", selection.len(), || {
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
            });
            phase("bulk_unstage_new", selection.len(), || {
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
            });
            phase("bulk_untracked_discard", selection.len(), || {
                assert!(matches!(
                    service
                        .discard(
                            &DiscardRequest {
                                worktree: root.into(),
                                tracked: vec![],
                                untracked: selection
                                    .iter()
                                    .map(|entry| entry.path.clone())
                                    .collect(),
                                confirmed: true
                            },
                            &cancel
                        )
                        .unwrap(),
                    MutationOutcome::Applied { .. }
                ));
            });
            phase("bulk_verify_untracked_absence", selection.len(), || {
                assert!(
                    !root
                        .join(
                            "generated_component_file_with_an_extraordinarily_long_name_00000.txt"
                        )
                        .exists()
                );
                // Check every selected name before recreation can overwrite a
                // surviving file and conceal a falsely successful partial discard.
                let remaining: HashSet<_> = fs::read_dir(root)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect();
                for entry in &selection {
                    assert!(
                        !remaining.contains(entry.path.as_os_str()),
                        "untracked discard left selected path {}",
                        entry.path
                    );
                }
            });
            // Exercise the >ARG_MAX tracked restore path as well as unlinking
            // untracked files: all three Git mutation operations use stdin.
            phase("bulk_recreate", selection.len(), || {
                for entry in &selection {
                    fs::write(root.join(entry.path.as_os_str()), "generated").unwrap();
                }
            });
            phase("bulk_stage_baseline", selection.len(), || {
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
            });
            phase("bulk_commit", selection.len(), || {
                git(root, &["commit", "--quiet", "-m", "generated baseline"]);
            });
            phase("bulk_modify", selection.len(), || {
                for entry in &selection {
                    fs::write(root.join(entry.path.as_os_str()), "modified").unwrap();
                }
            });
            phase("bulk_stage_modified", selection.len(), || {
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
            });
            phase("bulk_unstage_modified", selection.len(), || {
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
            });
            phase("bulk_tracked_discard", selection.len(), || {
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
            });
            phase("bulk_verify_tracked", selection.len(), || {
                for entry in &selection {
                    assert_eq!(
                        fs::read(root.join(entry.path.as_os_str())).unwrap(),
                        b"generated"
                    );
                }
            });
        },
    );
}
