//! macOS desktop notifications through `osascript`'s `display notification`.
//!
//! Notification Center accepts notices only from an app bundle, which the
//! daemon is not, and the daemon stays free of Objective-C. `osascript` posts
//! for it instead, and that costs what the Linux portal gives:
//!
//! - Notices are attributed to Script Editor, whose notification permission
//!   macOS asks for on first use. A refusal still reads as delivered, because
//!   `display notification` reports no outcome.
//! - A click opens Script Editor, not the conversation, so no notice is
//!   activated and the open action is never offered.
//! - A posted notice cannot be withdrawn: it stays in Notification Center
//!   until the person dismisses it, even after it is acknowledged here.
//! - Urgency has no equivalent, so a request for input looks like a finish.
//!
//! Title and body reach the script as arguments, never as script source, so
//! no agent text can be read as AppleScript.
use std::{convert::Infallible, path::Path, sync::Weak, time::Duration};

use uuid::Uuid;

use super::Shared;
use crate::{
    notifications::{Notice, TestOutcome},
    process::{CommandSpec, ProcessExecutor, ProcessFailureKind, SystemProcessExecutor},
    repository::CancellationToken,
};

const OSASCRIPT: &str = "/usr/bin/osascript";
/// `osascript` starts an AppleScript component on each call; this bounds a
/// call that never returns, as the portal's timeout does on Linux.
const POST_TIMEOUT: Duration = Duration::from_secs(5);
const SCRIPT: [&str; 3] = [
    "on run argv",
    "display notification (item 2 of argv) with title (item 1 of argv)",
    "end run",
];

/// No notification service runs on macOS for an owner to change.
pub(super) type PortalOwner = Infallible;

/// Clicks are never routed back here; see the module documentation. Braced,
/// not a unit struct, so the shared `Activation::default()` builds either
/// platform's.
#[derive(Default)]
pub(super) struct Activation {}

impl Activation {
    pub(super) fn receives_actions(&self) -> bool {
        false
    }

    pub(super) fn portal_changed(&mut self, owner: PortalOwner) -> bool {
        match owner {}
    }
}

/// `osascript` keeps no connection between posts.
pub(super) struct Poster;

pub(super) async fn connect(
    _shared: Weak<Shared>,
    _application: crate::notifications::Application,
    _activation: &mut Activation,
) -> Result<Poster, String> {
    if Path::new(OSASCRIPT).is_file() {
        Ok(Poster)
    } else {
        Err(format!(
            "Desktop notifications need {OSASCRIPT}, which is missing"
        ))
    }
}

pub(super) async fn send(
    _poster: &Poster,
    notice: &Notice,
    _application_action: bool,
) -> Result<(), String> {
    // Content reaching this point has already passed broker privacy policy.
    let message = crate::notifications::presentation::message(&notice.event, true);
    post(message.title, message.body)
        .await
        .map_err(|failure| format!("Desktop notifications unavailable: {failure}"))
}

pub(super) async fn send_test(_poster: &Poster) -> TestOutcome {
    match post(
        "cagents notifications work".to_owned(),
        "Agent completion and attention alerts appear here.".to_owned(),
    )
    .await
    {
        Ok(()) => TestOutcome::Sent,
        Err(Failure::TimedOut) => TestOutcome::TimedOut,
        Err(failure) => TestOutcome::Unavailable(failure.to_string()),
    }
}

/// Nothing to withdraw: `osascript` returns no handle to a posted notice.
pub(super) async fn remove(_poster: &Poster, _id: Uuid) -> Result<(), String> {
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error("Notification Center did not answer in time")]
    TimedOut,
    #[error("{0}")]
    Failed(String),
}

async fn post(title: String, body: String) -> Result<(), Failure> {
    let command = command(&title, &body);
    tokio::task::spawn_blocking(move || {
        SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .and_then(|output| output.require_success(Path::new(OSASCRIPT)))
            .map(drop)
            .map_err(|failure| match failure.kind {
                ProcessFailureKind::TimedOut => Failure::TimedOut,
                _ => Failure::Failed(failure.to_string()),
            })
    })
    .await
    .map_err(|error| Failure::Failed(error.to_string()))?
}

/// `--` ends `osascript`'s options, so a title that begins with `-` is still
/// the script's first argument.
fn command(title: &str, body: &str) -> CommandSpec {
    SCRIPT
        .into_iter()
        .fold(CommandSpec::new(OSASCRIPT), |command, line| {
            command.arg("-e").arg(line)
        })
        .arg("--")
        .arg(title)
        .arg(body)
        .timeout(POST_TIMEOUT)
        .output_limits(4096, 4096)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Posts a real notice, so this runs only when asked for on a Mac with a
    /// desktop session: `CAGENTS_POST_TEST_NOTIFICATION=1`.
    #[test]
    fn text_that_looks_like_options_or_script_is_posted_verbatim() {
        if std::env::var_os("CAGENTS_POST_TEST_NOTIFICATION").is_none() {
            return;
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                post(
                    "-e \"quit\"".to_owned(),
                    "\" & (do shell script \"touch /tmp/cagents-injected\") & \"".to_owned(),
                )
                .await
                .unwrap();
            });
        assert!(!Path::new("/tmp/cagents-injected").exists());
    }

    #[test]
    fn notice_text_is_passed_as_arguments_after_the_script() {
        let command = command("-title", "body");
        let arguments: Vec<_> = command
            .arguments
            .iter()
            .map(|argument| argument.to_str().unwrap())
            .collect();
        assert_eq!(
            arguments,
            [
                "-e", SCRIPT[0], "-e", SCRIPT[1], "-e", SCRIPT[2], "--", "-title", "body"
            ]
        );
    }
}
