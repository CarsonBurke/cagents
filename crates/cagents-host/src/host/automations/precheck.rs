//! A precheck is an explicitly authorized shell command, executed once. Reuse
//! the repository process runner's bounded pipes, deadline and group teardown.

use std::{path::Path, time::Duration};

pub(super) mod worker;

use crate::{
    agent_execution::{AUTHORITY_ENV, MAX_DIAGNOSTIC_BYTES, truncate},
    automations::{FiringState, Precheck, SkipReason},
    process::{CancellationToken, CommandSpec, ProcessExecutor, ProcessFailureKind},
};

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Outcome {
    Passed,
    Stopped {
        state: FiringState,
        skip: Option<SkipReason>,
        diagnostic: String,
    },
}

pub(super) fn run(
    executor: &dyn ProcessExecutor,
    precheck: &Precheck,
    directory: &Path,
    cancellation: &CancellationToken,
) -> Outcome {
    let command = CommandSpec::new("/bin/sh")
        .args(["-c", precheck.command.as_str()])
        .cwd(directory)
        // A daemon-supplied child capability is never inherited by a shell
        // predicate. The eventual accepted agent gets its own real authority.
        .env_remove(AUTHORITY_ENV)
        .timeout(Duration::from_secs(u64::from(precheck.timeout_seconds)))
        .output_limits(MAX_DIAGNOSTIC_BYTES, MAX_DIAGNOSTIC_BYTES);
    match executor.execute(&command, cancellation) {
        Ok(output) if output.success => Outcome::Passed,
        Ok(output) => Outcome::Stopped {
            state: FiringState::Skipped,
            skip: Some(SkipReason::PrecheckExit),
            diagnostic: detail(
                &format!(
                    "Precheck exited with {}",
                    output
                        .exit_code
                        .map_or_else(|| "a signal".into(), |code| format!("status {code}"))
                ),
                &output.stderr,
                &output.stdout,
            ),
        },
        Err(failure) => Outcome::Stopped {
            state: if failure.kind == ProcessFailureKind::Cancelled {
                FiringState::Cancelled
            } else {
                FiringState::Failed
            },
            skip: None,
            diagnostic: detail(&failure.message, &failure.stderr, &failure.stdout),
        },
    }
}

fn detail(message: &str, stderr: &[u8], stdout: &[u8]) -> String {
    let mut text = message.to_owned();
    for bytes in [stderr, stdout] {
        if !bytes.is_empty() {
            text.push('\n');
            text.push_str(&String::from_utf8_lossy(bytes));
        }
    }
    truncate(&mut text, MAX_DIAGNOSTIC_BYTES);
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::SystemProcessExecutor;

    #[test]
    fn nonzero_precheck_is_a_skip_and_runs_in_exact_target() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("marker"), "fixture").unwrap();
        let result = run(
            &SystemProcessExecutor,
            &Precheck {
                command: "test -f marker || exit 3; printf 'not ready' >&2; exit 7".into(),
                timeout_seconds: 5,
            },
            directory.path(),
            &CancellationToken::default(),
        );
        assert!(
            matches!(result, Outcome::Stopped { state: FiringState::Skipped, skip: Some(SkipReason::PrecheckExit), diagnostic } if diagnostic.contains("status 7") && diagnostic.contains("not ready"))
        );
    }

    #[test]
    fn cancellation_is_distinct_from_nonzero_precheck() {
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        assert!(matches!(
            run(
                &SystemProcessExecutor,
                &Precheck {
                    command: "exit 0".into(),
                    timeout_seconds: 5,
                },
                Path::new("/"),
                &cancellation
            ),
            Outcome::Stopped {
                state: FiringState::Cancelled,
                skip: None,
                ..
            }
        ));
    }
}
