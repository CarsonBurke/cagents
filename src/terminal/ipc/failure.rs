//! Recovery and admission facts carried by a daemon refusal.

use super::{ProtocolError, ProtocolErrorCode};
use crate::account::remote::ListHeld;

pub(super) const TYPED_REFUSALS: u16 = 228;
const INPUT_WORKER_CLOSED: &str = "the terminal worker has closed";
const OUTPUT_COMPLETED: &str = "terminal output has completed";
const SHUTDOWN_COMMITTED: &str =
    "Daemon upgrade/shutdown is committed; no further control mutations are accepted";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Refused,
    Unknown,
}

/// Advice for recovery of the failed operation. Only a proven refusal before
/// admission can authorize automatic replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryAdvice {
    Retry,
    AfterStartupRecovery,
    Reopen,
    AfterRestart,
    None,
}

/// Interprets a refusal independently of the message shown to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailureClass {
    pub reason: ProtocolErrorCode,
    retryable: bool,
}

impl FailureClass {
    pub(crate) fn new(reason: ProtocolErrorCode, retryable: bool) -> Self {
        Self { reason, retryable }
    }

    pub fn admission(self) -> Admission {
        match self.reason {
            ProtocolErrorCode::NotAdmitted
            | ProtocolErrorCode::DaemonBusy
            | ProtocolErrorCode::AccountRegistryBusy
            | ProtocolErrorCode::AccountLifecycleBusy
            | ProtocolErrorCode::StartupRecoveryHeld => Admission::Refused,
            _ => Admission::Unknown,
        }
    }

    pub fn retry_advice(self) -> RetryAdvice {
        match self.reason {
            ProtocolErrorCode::StartupRecoveryHeld => RetryAdvice::AfterStartupRecovery,
            ProtocolErrorCode::UpdateRestartStopped => RetryAdvice::Reopen,
            ProtocolErrorCode::UnprovenChatOwner => RetryAdvice::AfterRestart,
            _ if self.retryable => RetryAdvice::Retry,
            _ => RetryAdvice::None,
        }
    }

    /// The daemon refused the request before admission because it was busy.
    pub fn is_daemon_busy(self) -> bool {
        self.reason == ProtocolErrorCode::DaemonBusy && self.retryable
    }
}

impl ProtocolError {
    pub fn account_list_held(held: ListHeld) -> Self {
        let code = match held {
            ListHeld::Registry => ProtocolErrorCode::AccountRegistryBusy,
            ListHeld::Lifecycle => ProtocolErrorCode::AccountLifecycleBusy,
        };
        Self::new(code, held.to_string(), true)
    }

    pub fn terminal_input_worker_closed() -> Self {
        Self::new(
            ProtocolErrorCode::TerminalInputWorkerClosed,
            INPUT_WORKER_CLOSED,
            true,
        )
    }

    pub fn terminal_output_completed() -> Self {
        Self::new(
            ProtocolErrorCode::TerminalOutputCompleted,
            OUTPUT_COMPLETED,
            false,
        )
    }

    pub fn daemon_shutdown_committed() -> Self {
        Self::new(
            ProtocolErrorCode::DaemonShutdownCommitted,
            SHUTDOWN_COMMITTED,
            false,
        )
    }

    /// Interprets a legacy wire reply at the boundary where the peer's protocol
    /// is known. Generic errors from current daemons never acquire meaning
    /// from their diagnostic text.
    pub(crate) fn adapt_legacy_refusal(&mut self, protocol: u16) {
        if protocol < TYPED_REFUSALS {
            self.code = legacy_refusal(self.code, &self.message, self.retryable);
        }
    }
}

impl ProtocolErrorCode {
    /// The code clients received before typed refusals were introduced.
    pub(crate) fn legacy_code(self) -> Self {
        use ProtocolErrorCode::*;
        match self {
            DaemonBusy => Internal,
            AccountRegistryBusy
            | AccountLifecycleBusy
            | UnprovenChatOwner
            | TerminalCloseSourceConflict => InvalidRequest,
            StartupRecoveryHeld
            | UpdateRestartStopped
            | TerminalInputWorkerClosed
            | TerminalOutputCompleted
            | DaemonShutdownCommitted => TerminalUnavailable,
            TerminalCloseSourceAbsent => TerminalNotFound,
            code => code,
        }
    }
}

// These literals describe replies from shipped daemons. Keep them when the
// current daemon's display text changes.
const LEGACY_BUSY: &str = "The daemon is finishing another operation. Wait for it and retry.";
const LEGACY_REGISTRY_HELD: &str =
    "Another window is saving the account list. Try again in a moment.";
const LEGACY_LIFECYCLE_HELD: &str =
    "Another window is changing accounts, or an agent is starting on one. Try again in a moment.";
const LEGACY_UNPROVEN_OWNER: &str = "This conversation's earlier Claude process ended without recording its close, and cagents cannot prove that nothing else still holds the conversation, so nothing was started in its place. It can be reopened after you restart this computer.";
const EARLIER_UNPROVEN_OWNER_REFUSAL: &str = "This conversation's earlier Claude process ended without recording its close, and cagents cannot prove that nothing else still holds the conversation, so nothing was started in its place. It can be resumed after this computer restarts";
const LEGACY_STARTUP_HOLD: &str = "Startup recovery retained original ownership; inspect available sessions or explicitly Close the original actor: ";
const LEGACY_UPDATE_STOPPED: &str = "This conversation could not be restarted after the update. ";
const LEGACY_CLOSE_CONFLICT: &str = "The terminal changed or still owns work; nothing was closed";
const LEGACY_CLOSE_ABSENT: &str = "The captured terminal is no longer resident; nothing was closed";

fn legacy_refusal(code: ProtocolErrorCode, message: &str, retryable: bool) -> ProtocolErrorCode {
    use ProtocolErrorCode::*;
    match (code, retryable, message) {
        (Internal | NotAdmitted | InvalidRequest, true, LEGACY_BUSY) => DaemonBusy,
        (InvalidRequest, true, LEGACY_REGISTRY_HELD) => AccountRegistryBusy,
        (InvalidRequest, true, LEGACY_LIFECYCLE_HELD) => AccountLifecycleBusy,
        (InvalidRequest, false, LEGACY_UNPROVEN_OWNER | EARLIER_UNPROVEN_OWNER_REFUSAL) => {
            UnprovenChatOwner
        }
        (TerminalUnavailable, false, _) if message.starts_with(LEGACY_STARTUP_HOLD) => {
            StartupRecoveryHeld
        }
        (TerminalUnavailable, _, _) if message.starts_with(LEGACY_UPDATE_STOPPED) => {
            UpdateRestartStopped
        }
        (TerminalUnavailable, _, SHUTDOWN_COMMITTED) => DaemonShutdownCommitted,
        (TerminalUnavailable, true, INPUT_WORKER_CLOSED) => TerminalInputWorkerClosed,
        (TerminalUnavailable, false, OUTPUT_COMPLETED) => TerminalOutputCompleted,
        (InvalidRequest, false, LEGACY_CLOSE_CONFLICT) => TerminalCloseSourceConflict,
        (TerminalNotFound, false, LEGACY_CLOSE_ABSENT) => TerminalCloseSourceAbsent,
        _ => code,
    }
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;

    #[test]
    fn historical_refusals_keep_their_recovery_and_wire_fields() {
        use ProtocolErrorCode::*;
        let cases = [
            (Internal, LEGACY_BUSY.into(), true, DaemonBusy),
            (NotAdmitted, LEGACY_BUSY.into(), true, DaemonBusy),
            (
                InvalidRequest,
                LEGACY_REGISTRY_HELD.into(),
                true,
                AccountRegistryBusy,
            ),
            (
                InvalidRequest,
                LEGACY_LIFECYCLE_HELD.into(),
                true,
                AccountLifecycleBusy,
            ),
            (
                InvalidRequest,
                LEGACY_UNPROVEN_OWNER.into(),
                false,
                UnprovenChatOwner,
            ),
            (
                InvalidRequest,
                EARLIER_UNPROVEN_OWNER_REFUSAL.into(),
                false,
                UnprovenChatOwner,
            ),
            (
                TerminalUnavailable,
                format!("{LEGACY_STARTUP_HOLD}old owner"),
                false,
                StartupRecoveryHeld,
            ),
            (
                TerminalUnavailable,
                format!(
                    "{LEGACY_UPDATE_STOPPED}provider stopped. It isn't running, and nothing was resent."
                ),
                true,
                UpdateRestartStopped,
            ),
            (
                TerminalUnavailable,
                INPUT_WORKER_CLOSED.into(),
                true,
                TerminalInputWorkerClosed,
            ),
            (
                TerminalUnavailable,
                OUTPUT_COMPLETED.into(),
                false,
                TerminalOutputCompleted,
            ),
            (
                TerminalUnavailable,
                SHUTDOWN_COMMITTED.into(),
                false,
                DaemonShutdownCommitted,
            ),
            (
                TerminalUnavailable,
                SHUTDOWN_COMMITTED.into(),
                true,
                DaemonShutdownCommitted,
            ),
            (
                InvalidRequest,
                LEGACY_CLOSE_CONFLICT.into(),
                false,
                TerminalCloseSourceConflict,
            ),
            (
                TerminalNotFound,
                LEGACY_CLOSE_ABSENT.into(),
                false,
                TerminalCloseSourceAbsent,
            ),
        ];
        for (legacy, message, retryable, typed) in cases {
            let mut unknown =
                ProtocolError::new(legacy, format!("The provider quoted: {message}"), retryable);
            unknown.adapt_legacy_refusal(TYPED_REFUSALS - 1);
            assert_eq!(unknown.code, legacy);

            let mut error = ProtocolError::new(legacy, &message, retryable);
            error.adapt_legacy_refusal(TYPED_REFUSALS);
            assert_eq!(error.code, legacy);
            error.adapt_legacy_refusal(TYPED_REFUSALS - 1);
            assert_eq!(error, ProtocolError::new(typed, message, retryable));
        }
    }

    #[test]
    fn retry_advice_does_not_prove_admission_and_explicit_refusal_replaces_reason() {
        use ProtocolErrorCode::*;
        let uncertain = ProtocolError::new(TerminalUnavailable, super::super::DAEMON_BUSY, true);
        assert_eq!(uncertain.failure_class().retry_advice(), RetryAdvice::Retry);
        assert_eq!(uncertain.failure_class().admission(), Admission::Unknown);
        assert!(!uncertain.is_daemon_busy());

        let busy = ProtocolError::daemon_busy();
        assert_eq!(busy.failure_class().admission(), Admission::Refused);
        assert!(busy.is_daemon_busy());

        let refused = busy.clone().not_admitted();
        assert_eq!(refused.code, NotAdmitted);
        assert_eq!(refused.message, busy.message);
        assert_eq!(refused.retryable, busy.retryable);
        assert_eq!(refused.failure_class().admission(), Admission::Refused);
        assert!(!refused.is_daemon_busy());
    }
}
