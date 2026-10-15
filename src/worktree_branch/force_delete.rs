//! Keep native checkout refusal and bind force consent inside Git's ref lock.
use super::git_command;
use crate::repository::{CancellationToken, ProcessExecutor, SystemProcessExecutor};
use std::{io::Write, os::unix::fs::PermissionsExt, path::Path};

const PROBE: &str = "#!/bin/sh\nset -eu\n/bin/cat >/dev/null\nif [ \"$1\" = prepared ]; then printf prepared > \"$CAGENTS_DELETE_PROOF\"; fi\n";
const GUARD: &str = r##"#!/bin/sh
set -eu
input=$(/bin/cat)
if [ -n "$CAGENTS_DELETE_PREVIOUS_HOOK" ] && [ -x "$CAGENTS_DELETE_PREVIOUS_HOOK" ]; then
    printf '%s\n' "$input" | GIT_CONFIG_PARAMETERS="$CAGENTS_DELETE_PREVIOUS_CONFIG" "$CAGENTS_DELETE_PREVIOUS_HOOK" "$@"
fi
if [ "$1" = prepared ]; then
    while IFS=' ' read -r old new ref; do
        [ "$ref" = "$CAGENTS_DELETE_REF" ] || continue
        case "$new" in ''|*[!0]*) exit 1;; esac
        current=$("$CAGENTS_DELETE_GIT" rev-parse --verify --end-of-options "$ref") || exit 1
        if [ "$current" != "$CAGENTS_DELETE_OID" ]; then
            printf '%s\n' 'Branch changed after confirmation; deletion refused' >&2
            exit 1
        fi
        printf prepared > "$CAGENTS_DELETE_PROOF"
    done <<EOF_INPUT
$input
EOF_INPUT
fi
"##;

fn run(
    command: crate::repository::CommandSpec,
    cancel: &CancellationToken,
) -> Result<crate::process::ProcessOutput, String> {
    SystemProcessExecutor
        .execute(&command, cancel)
        .and_then(|output| output.require_success(&command.program))
        .map_err(|error| error.reported_detail())
}

pub(super) fn delete(
    repository: &Path,
    branch: &str,
    full_ref: &str,
    oid: &str,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let previous = run(
        git_command(repository).args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "hooks/reference-transaction",
        ]),
        cancel,
    )?;
    let previous = String::from_utf8(previous.stdout).map_err(|error| error.to_string())?;
    let previous = previous.strip_suffix('\n').unwrap_or(&previous);
    let temporary = tempfile::Builder::new()
        .prefix("cagents-branch-delete-")
        .tempdir()
        .map_err(|error| error.to_string())?;
    let hook = temporary.path().join("reference-transaction");
    let proof = temporary.path().join("prepared");
    std::fs::write(&hook, PROBE).map_err(|error| error.to_string())?;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    let mut hooks = std::ffi::OsString::from("core.hooksPath=");
    hooks.push(temporary.path());
    // Prove the installed Git calls a rejecting-capable prepared hook before
    // any destructive effect. Verification/prepare/abort never updates the ref,
    // and this probe deliberately does not invoke repository policy hooks.
    let input = format!("start\nverify {full_ref} {oid}\nprepare\nabort\n");
    run(
        git_command(repository)
            .args([
                std::ffi::OsString::from("-c"),
                hooks.clone(),
                "update-ref".into(),
                "--stdin".into(),
            ])
            .env("CAGENTS_DELETE_PROOF", &proof)
            .stdin(input.into_bytes()),
        cancel,
    )?;
    if std::fs::read(&proof).map_err(|_| {
        "Git did not prove reference-transaction support; branch retained".to_owned()
    })? != b"prepared"
    {
        return Err("Git did not prove reference-transaction support; branch retained".into());
    }
    std::fs::remove_file(&proof).map_err(|error| error.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&hook)
        .map_err(|error| error.to_string())?;
    file.write_all(GUARD.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    drop(file);
    // Git propagates this command's -c hooksPath through its environment.
    // Restore the original command configuration only for the chained hook,
    // so policy hooks can delegate to sibling hooks and child Git commands.
    // Preserve opaque inherited parameters and COUNT entries; add only the
    // same fsmonitor safeguard that ordinary repository commands receive.
    let mut previous_config = std::env::var_os("GIT_CONFIG_PARAMETERS").unwrap_or_default();
    if !previous_config.is_empty() {
        previous_config.push(" ");
    }
    previous_config.push("'core.fsmonitor=false'");
    let command = git_command(repository)
        .args([
            std::ffi::OsString::from("-c"),
            hooks,
            "branch".into(),
            "-D".into(),
            "--".into(),
            branch.into(),
        ])
        .env("CAGENTS_DELETE_PROOF", &proof)
        .env("CAGENTS_DELETE_PREVIOUS_HOOK", previous)
        .env("CAGENTS_DELETE_PREVIOUS_CONFIG", previous_config)
        .env("CAGENTS_DELETE_REF", full_ref)
        .env("CAGENTS_DELETE_OID", oid)
        .env("CAGENTS_DELETE_GIT", "git");
    let output = SystemProcessExecutor
        .execute(&command, cancel)
        .and_then(|output| output.require_success(&command.program));
    if let Err(error) = output {
        let current = SystemProcessExecutor.execute(
            &git_command(repository).args(["show-ref", "--verify", "--quiet", full_ref]),
            cancel,
        );
        return Err(
            if current
                .as_ref()
                .is_ok_and(|current| current.exit_code == Some(1))
            {
                format!(
                    "Branch was deleted; Git could not confirm complete branch cleanup: {}",
                    error.reported_detail()
                )
            } else {
                error.reported_detail()
            },
        );
    }
    if std::fs::read(&proof).map_err(|error| {
        format!("Branch was deleted, but its transaction proof could not be inspected: {error}")
    })? != b"prepared"
    {
        return Err("Branch was deleted, but its transaction proof was not retained".into());
    }
    // Native branch deletion owns configuration cleanup. Inspect only this
    // exact subsection, preserving dotted sibling branch names.
    let section = format!("branch.{branch}");
    let escaped: String = section
        .chars()
        .flat_map(|character| {
            if "\\.^$|?*+()[]{}".contains(character) {
                vec!['\\', character]
            } else {
                vec![character]
            }
        })
        .collect();
    let pattern = format!("^{escaped}\\.[^.]+$");
    let check = git_command(repository).args(["config", "--local", "--get-regexp", &pattern]);
    let output = SystemProcessExecutor
        .execute(&check, cancel)
        .map_err(|error| {
            format!(
                "Branch was deleted; its configuration cleanup could not be inspected: {}",
                error.reported_detail()
            )
        })?;
    if output.exit_code != Some(1) {
        return Err("Branch was deleted, but its configuration cleanup could not be confirmed; remaining metadata was preserved".into());
    }
    Ok(())
}
