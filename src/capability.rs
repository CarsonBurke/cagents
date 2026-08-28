use url::Url;

use crate::model::BrowserSession;

/// Browser support compiled into this cagents build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserCapability {
    Integrated,
    ExternalOnly,
}

impl BrowserCapability {
    pub const fn current() -> Self {
        if cfg!(feature = "integrated-browser") {
            Self::Integrated
        } else {
            Self::ExternalOnly
        }
    }
}

/// Runtime treatment for a browser descriptor restored from durable storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserRestoreAction<'a> {
    HydrateIntegrated,
    OfferExternal(&'a Url),
}

pub fn browser_restore_action(session: &BrowserSession) -> BrowserRestoreAction<'_> {
    match BrowserCapability::current() {
        BrowserCapability::Integrated => BrowserRestoreAction::HydrateIntegrated,
        BrowserCapability::ExternalOnly => {
            BrowserRestoreAction::OfferExternal(&session.current_url)
        }
    }
}
