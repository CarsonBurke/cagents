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
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

#[path = "support/repository_scale.rs"]
mod scale_support;
use scale_support::{fixture_git_environment, isolated_large_pathspec_test, phase};

struct IsolatedExecutor;

fn git_subcommand(command: &CommandSpec) -> &'static str {
    let mut arguments = command.arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace") => {
                arguments.next();
            }
            Some(option) if option.starts_with('-') => {}
            Some("add") => return "add",
            Some("cat-file") => return "cat-file",
            Some("check-attr") => return "check-attr",
            Some("check-ignore") => return "check-ignore",
            Some("clean") => return "clean",
            Some("commit") => return "commit",
            Some("config") => return "config",
            Some("diff") => return "diff",
            Some("init") => return "init",
            Some("ls-files") => return "ls-files",
            Some("ls-tree") => return "ls-tree",
            Some("read-tree") => return "read-tree",
            Some("reset") => return "reset",
            Some("restore") => return "restore",
            Some("rev-list") => return "rev-list",
            Some("rev-parse") => return "rev-parse",
            Some("show") => return "show",
            Some("sparse-checkout") => return "sparse-checkout",
            Some("status") => return "status",
            Some("symbolic-ref") => return "symbolic-ref",
            Some("update-index") => return "update-index",
            _ => return "other",
        }
    }
    "unknown"
}

impl ProcessExecutor for IsolatedExecutor {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        static COMMANDS: AtomicU64 = AtomicU64::new(0);
        let sequence = COMMANDS
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .ok()
            .map(|previous| previous + 1);
        // At most 1024 command pairs plus 53 later powers of two, even if
        // an unexpected retry loop runs until the unchanged outer deadline.
        let report = sequence.filter(|sequence| *sequence <= 1024 || sequence.is_power_of_two());
        let subcommand = git_subcommand(command);
        let argv_count = command.arguments.len() + 1;
        let stdin_bytes = command.stdin.as_ref().map_or(0, Vec::len);
        let started = Instant::now();
        if let Some(sequence) = report {
            eprintln!(
                "pathspec git={sequence} command={subcommand} begin argv_count={argv_count} stdin_bytes={stdin_bytes} elapsed_ms=0"
            );
        }
        let result = SystemProcessExecutor.execute(
            &fixture_git_environment(command.clone())
                .env("GIT_AUTHOR_NAME", "Cagents Contract")
                .env("GIT_AUTHOR_EMAIL", "contract@example.invalid")
                .env("GIT_COMMITTER_NAME", "Cagents Contract")
                .env("GIT_COMMITTER_EMAIL", "contract@example.invalid"),
            cancellation,
        );
        if let Some(sequence) = report {
            let elapsed = started.elapsed().as_millis();
            match &result {
                Ok(output) => eprintln!(
                    "pathspec git={sequence} command={subcommand} end argv_count={argv_count} stdin_bytes={stdin_bytes} elapsed_ms={elapsed} success={} exit={:?} signal={:?}",
                    output.success, output.exit_code, output.signal
                ),
                Err(failure) => eprintln!(
                    "pathspec git={sequence} command={subcommand} end argv_count={argv_count} stdin_bytes={stdin_bytes} elapsed_ms={elapsed} failure={:?} exit={:?} signal={:?} started={}",
                    failure.kind, failure.exit_code, failure.signal, failure.started
                ),
            }
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
            let current = phase("repository init");
            git(root, &["init", "-b", "main"]);
            drop(current);
            let current = phase("literal generation");
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
            drop(current);
            let current = phase("literal baseline stage");
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
            drop(current);
            let current = phase("literal baseline commit");
            git(root, &["commit", "-m", "initial"]);
            drop(current);
            let current = phase("literal modification");
            for name in names {
                fs::write(root.join(name), "changed").unwrap();
            }
            drop(current);
            let current = phase("literal modified stage");
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
            drop(current);
            let current = phase("literal unstage");
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
            drop(current);
            let current = phase("literal discard");
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
            drop(current);
            let current = phase("literal verify");
            for name in names {
                assert_eq!(fs::read_to_string(root.join(name)).unwrap(), "base");
            }
            drop(current);

            // 40k names exceed Linux ARG_MAX even before argv pointer overhead.
            let current = phase("40k generation");
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
            drop(current);
            let current = phase("40k pathspec size verify");
            assert!(
                service
                    .stage_command(root, &selection_pathspecs(&selection))
                    .unwrap()
                    .stdin
                    .unwrap()
                    .len()
                    > 2_097_152
            );
            drop(current);
            let current = phase("40k initial stage");
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
            drop(current);
            let current = phase("40k initial unstage");
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
            drop(current);
            let current = phase("40k untracked discard");
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
            drop(current);
            let current = phase("40k untracked verify");
            assert!(
                !root
                    .join("generated_component_file_with_an_extraordinarily_long_name_00000.txt")
                    .exists()
            );
            drop(current);
            // Exercise the >ARG_MAX tracked restore path as well as unlinking
            // untracked files: all three Git mutation operations use stdin.
            let current = phase("40k baseline generation");
            for entry in &selection {
                fs::write(root.join(entry.path.as_os_str()), "generated").unwrap();
            }
            drop(current);
            let current = phase("40k baseline stage");
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
            drop(current);
            let current = phase("40k baseline commit");
            git(root, &["commit", "--quiet", "-m", "generated baseline"]);
            drop(current);
            let current = phase("40k modification");
            for entry in &selection {
                fs::write(root.join(entry.path.as_os_str()), "modified").unwrap();
            }
            drop(current);
            let current = phase("40k modified stage");
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
            drop(current);
            let current = phase("40k tracked unstage");
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
            drop(current);
            let current = phase("40k tracked discard");
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
            drop(current);
            let current = phase("40k tracked verify");
            for entry in &selection {
                assert_eq!(
                    fs::read(root.join(entry.path.as_os_str())).unwrap(),
                    b"generated"
                );
            }
            drop(current);
        },
    );
}
