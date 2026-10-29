//! Current completion wording, independent of the cached local timestamp.
//! Only the provider's existing exact turn detail confirms failure/interruption.

use crate::{chat::Snapshot, ui_controls::Icon};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Caption {
    Finished,
    Interrupted,
    Failed,
    FinishedWithError,
}

pub(super) fn caption(snapshot: &Snapshot, local_error: bool) -> Caption {
    match snapshot.detail.as_str() {
        "Turn interrupted" => Caption::Interrupted,
        "Turn failed" => Caption::Failed,
        _ if local_error || snapshot.error.is_some() => Caption::FinishedWithError,
        _ => Caption::Finished,
    }
}

impl Caption {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Finished => "Finished",
            Self::Interrupted => "Interrupted",
            Self::Failed => "Failed",
            Self::FinishedWithError => "Finished with error",
        }
    }

    pub(super) fn icon(self) -> Icon {
        match self {
            Self::Finished => Icon::Check,
            Self::Interrupted => Icon::Cancel,
            Self::Failed | Self::FinishedWithError => Icon::Warning,
        }
    }

    pub(super) fn at(self, clock: &str) -> String {
        format!("{} at {clock}", self.label())
    }
}
