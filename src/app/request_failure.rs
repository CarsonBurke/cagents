//! Present permission refusals without exposing transport vocabulary.
use super::TransientNotice;
use crate::{automations::Request, control::ControlError, terminal_ipc::ProtocolErrorCode};

#[derive(Clone, Copy, Debug)]
pub(super) enum Activity {
    ViewAutomations,
    ChangeAutomations,
    ViewApprovals,
    DecideApproval,
    ViewLifecycle,
    ViewWork,
    PinWork,
}

impl Activity {
    pub(super) fn automation(request: &Request) -> Self {
        match request {
            Request::List { .. }
            | Request::Show { .. }
            | Request::History { .. }
            | Request::ShowFiring { .. }
            | Request::ShowRun { .. } => Self::ViewAutomations,
            Request::Proposal { .. } | Request::PendingProposals { .. } => Self::ViewApprovals,
            Request::ResolveProposal { .. } => Self::DecideApproval,
            Request::Audit { .. } => Self::ViewLifecycle,
            Request::Create { .. }
            | Request::Edit { .. }
            | Request::SetEnabled { .. }
            | Request::Remove { .. }
            | Request::RunNow { .. }
            | Request::Cancel { .. }
            | Request::Propose { .. } => Self::ChangeAutomations,
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::ViewAutomations => "view automations",
            Self::ChangeAutomations => "change automations",
            Self::ViewApprovals => "view automation requests",
            Self::DecideApproval => "approve or decline automation requests",
            Self::ViewLifecycle => "view this automation's history",
            Self::ViewWork => "view Work",
            Self::PinWork => "change Work",
        }
    }

    fn refusal(self, scoped: bool, remote: bool) -> String {
        let subject = if scoped {
            "this window's agent session"
        } else {
            "this window"
        };
        let reason = if remote {
            format!("{subject} can't {} on another device", self.verb())
        } else {
            format!("{subject} isn't allowed to {}", self.verb())
        };
        match self {
            Self::PinWork => format!("Couldn't pin this card: {reason}."),
            _ => {
                let mut message = reason;
                message.replace_range(..1, "T");
                message.push('.');
                message
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Failure {
    pub(super) message: String,
    pub(super) details: Option<String>,
}

impl Failure {
    pub(super) fn control(error: ControlError, activity: Activity, scoped: bool) -> Self {
        if matches!(
            &error,
            ControlError::Remote {
                code: ProtocolErrorCode::PermissionDenied,
                ..
            }
        ) {
            Self {
                message: activity.refusal(scoped, false),
                details: Some(error.to_string()),
            }
        } else {
            // Preserve the existing formatting for failures outside this case.
            let message = match activity {
                Activity::ViewAutomations | Activity::ChangeAutomations => error.reason(),
                _ => error.to_string(),
            };
            Self {
                message,
                details: None,
            }
        }
    }

    pub(super) fn remote(activity: Activity) -> String {
        activity.refusal(true, true)
    }

    pub(super) fn into_notice(self) -> TransientNotice {
        let notice = TransientNotice::error(self.message);
        match self.details {
            Some(details) => notice.with_details(details),
            None => notice,
        }
    }
}

pub(super) fn message(error: ControlError, activity: Activity, scoped: bool) -> String {
    Failure::control(error, activity, scoped).message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused() -> ControlError {
        ControlError::Remote {
            code: ProtocolErrorCode::PermissionDenied,
            message: "Invalid agent session authority".into(),
            retryable: false,
        }
    }

    #[test]
    fn permission_refusals_explain_each_task_and_keep_raw_diagnostics_in_details() {
        for (activity, expected) in [
            (
                Activity::ViewAutomations,
                "This window's agent session isn't allowed to view automations.",
            ),
            (
                Activity::ChangeAutomations,
                "This window's agent session isn't allowed to change automations.",
            ),
            (
                Activity::ViewApprovals,
                "This window's agent session isn't allowed to view automation requests.",
            ),
            (
                Activity::DecideApproval,
                "This window's agent session isn't allowed to approve or decline automation requests.",
            ),
            (
                Activity::ViewLifecycle,
                "This window's agent session isn't allowed to view this automation's history.",
            ),
            (
                Activity::PinWork,
                "Couldn't pin this card: this window's agent session isn't allowed to change Work.",
            ),
        ] {
            let failure = Failure::control(refused(), activity, true);
            assert_eq!(failure.message, expected);
            assert!(!failure.message.contains("PermissionDenied"));
            assert!(!failure.message.contains("Invalid agent session authority"));
            let notice = failure.into_notice();
            assert_eq!(notice.message, expected);
            assert!(
                notice
                    .details
                    .unwrap()
                    .contains("Invalid agent session authority")
            );
        }
    }

    #[test]
    fn other_failures_keep_their_format_and_user_windows_do_not_claim_an_agent_session() {
        let ordinary = Failure::control(refused(), Activity::PinWork, false);
        assert_eq!(
            ordinary.message,
            "Couldn't pin this card: this window isn't allowed to change Work."
        );
        let failure = Failure::control(
            ControlError::Remote {
                code: ProtocolErrorCode::InvalidRequest,
                message: "The definition changed; refresh before saving".into(),
                retryable: false,
            },
            Activity::ChangeAutomations,
            true,
        );
        assert_eq!(
            failure.message,
            "The definition changed; refresh before saving"
        );
        assert!(failure.details.is_none());
    }
}
