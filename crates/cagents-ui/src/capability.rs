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

/// Closed names accepted at the browser presentation boundary. Compositor
/// announcements and backend diagnostics are never shown as arbitrary text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredWaylandProtocol {
    Compositor,
    Subcompositor,
    Viewporter,
}
impl RequiredWaylandProtocol {
    pub const ALL: [Self; 3] = [Self::Compositor, Self::Subcompositor, Self::Viewporter];
    pub const fn name(self) -> &'static str {
        match self {
            Self::Compositor => "wl_compositor",
            Self::Subcompositor => "wl_subcompositor",
            Self::Viewporter => "wp_viewporter",
        }
    }
    pub const fn minimum_version(self) -> u32 {
        1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingWaylandProtocols(u8);
impl MissingWaylandProtocols {
    pub fn from_advertised<'a>(globals: impl IntoIterator<Item = (&'a str, u32)>) -> Self {
        let mut found = 0;
        for (name, version) in globals {
            for (index, required) in RequiredWaylandProtocol::ALL.iter().enumerate() {
                if name == required.name() && version >= required.minimum_version() {
                    found |= 1 << index;
                }
            }
        }
        Self(0b111 & !found)
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn protocols(self) -> impl Iterator<Item = RequiredWaylandProtocol> {
        RequiredWaylandProtocol::ALL
            .into_iter()
            .enumerate()
            .filter_map(move |(index, required)| (self.0 & (1 << index) != 0).then_some(required))
    }
}
impl std::fmt::Display for MissingWaylandProtocols {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, protocol) in self.protocols().enumerate() {
            if index != 0 {
                f.write_str(", ")?;
            }
            f.write_str(protocol.name())?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserUnavailable {
    MissingProtocols(MissingWaylandProtocols),
    TimedOut,
    ParentUnavailable,
    NotWayland,
    InitializationFailed,
}
impl std::fmt::Display for BrowserUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingProtocols(missing) => write!(
                f,
                "Integrated browser unavailable: missing required Wayland protocols: {missing}"
            ),
            Self::TimedOut => {
                f.write_str("Integrated browser unavailable: Wayland capability check timed out")
            }
            Self::ParentUnavailable => {
                f.write_str("Integrated browser unavailable: the parent window is unavailable")
            }
            Self::NotWayland => {
                f.write_str("Integrated browser unavailable: a Wayland parent is required")
            }
            Self::InitializationFailed => f.write_str(
                "Integrated browser unavailable: Wayland capability initialization failed",
            ),
        }
    }
}

#[cfg(test)]
mod wayland_requirement_tests {
    use super::*;
    #[test]
    fn required_globals_are_closed_versioned_and_ordered() {
        let all = RequiredWaylandProtocol::ALL.map(|required| (required.name(), 1));
        assert!(MissingWaylandProtocols::from_advertised(all).is_empty());
        for absent in RequiredWaylandProtocol::ALL {
            let missing = MissingWaylandProtocols::from_advertised(
                all.into_iter().filter(|(name, _)| *name != absent.name()),
            );
            assert_eq!(missing.protocols().collect::<Vec<_>>(), vec![absent]);
            assert_eq!(missing.to_string(), absent.name());
        }
        let missing = MissingWaylandProtocols::from_advertised([
            ("wl_compositor", 6),
            ("wp_viewporter", 0),
            ("secret://credential", 99),
        ]);
        assert_eq!(missing.to_string(), "wl_subcompositor, wp_viewporter");
        assert!(
            !BrowserUnavailable::MissingProtocols(missing)
                .to_string()
                .contains("secret")
        );
        assert!(
            MissingWaylandProtocols::from_advertised(all.map(|(name, _)| (name, 99))).is_empty()
        );
    }
}
