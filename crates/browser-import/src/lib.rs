//! Local, read-only browser cookie import. Callers must obtain consent before
//! discovery or scanning. Values never implement Debug or serialization and
//! are erased when the import batch is dropped.

const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

mod chromium;
mod database;
mod discovery;
#[cfg(feature = "desktop-keyring")]
mod keyring;
mod locked;

use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};
use zeroize::Zeroizing;

pub use discovery::discover;

/// A browser product and release channel whose profiles can be imported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Browser {
    Chrome,
    ChromeBeta,
    ChromeDev,
    ChromeCanary,
    Chromium,
    UngoogledChromium,
    Brave,
    BraveBeta,
    BraveNightly,
    Edge,
    EdgeBeta,
    EdgeDev,
    EdgeCanary,
    Vivaldi,
    VivaldiSnapshot,
    Opera,
    OperaBeta,
    OperaDeveloper,
    OperaGx,
    Helium,
    Thorium,
    Firefox,
    LibreWolf,
    Floorp,
    Waterfox,
    Zen,
}

impl Browser {
    pub fn label(self) -> &'static str {
        match self {
            Self::Chrome => "Chrome",
            Self::ChromeBeta => "Chrome Beta",
            Self::ChromeDev => "Chrome Dev",
            Self::ChromeCanary => "Chrome Canary",
            Self::Chromium => "Chromium",
            Self::UngoogledChromium => "ungoogled-chromium",
            Self::Brave => "Brave",
            Self::BraveBeta => "Brave Beta",
            Self::BraveNightly => "Brave Nightly",
            Self::Edge => "Edge",
            Self::EdgeBeta => "Edge Beta",
            Self::EdgeDev => "Edge Dev",
            Self::EdgeCanary => "Edge Canary",
            Self::Vivaldi => "Vivaldi",
            Self::VivaldiSnapshot => "Vivaldi Snapshot",
            Self::Opera => "Opera",
            Self::OperaBeta => "Opera Beta",
            Self::OperaDeveloper => "Opera Developer",
            Self::OperaGx => "Opera GX",
            Self::Helium => "Helium",
            Self::Thorium => "Thorium",
            Self::Firefox => "Firefox",
            Self::LibreWolf => "LibreWolf",
            Self::Floorp => "Floorp",
            Self::Waterfox => "Waterfox",
            Self::Zen => "Zen",
        }
    }

    /// The desktop keyring entry a Chromium-based browser keeps its cookie
    /// key under, or `None` for a Firefox-based browser, which encrypts none.
    ///
    /// Chromium names the entry after the build's branding, not the product:
    /// only Google-branded builds use the Chrome entry, and forks share the
    /// Chromium one unless they patch the name, as Brave does.
    pub fn keyring(self) -> Option<Keyring> {
        match self {
            Self::Chrome | Self::ChromeBeta | Self::ChromeDev | Self::ChromeCanary => {
                Some(Keyring::Chrome)
            }
            Self::Vivaldi | Self::VivaldiSnapshot => Some(Keyring::Chrome),
            Self::Brave | Self::BraveBeta | Self::BraveNightly => Some(Keyring::Brave),
            Self::Chromium
            | Self::UngoogledChromium
            | Self::Edge
            | Self::EdgeBeta
            | Self::EdgeDev
            | Self::EdgeCanary
            | Self::Opera
            | Self::OperaBeta
            | Self::OperaDeveloper
            | Self::OperaGx
            | Self::Helium
            | Self::Thorium => Some(Keyring::Chromium),
            Self::Firefox | Self::LibreWolf | Self::Floorp | Self::Waterfox | Self::Zen => None,
        }
    }

    fn chromium(self) -> bool {
        self.keyring().is_some()
    }
}

/// The name a Chromium-based browser stores its cookie key under on Linux:
/// the Secret Service `application` attribute, or the KWallet folder and entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Keyring {
    Chrome,
    Chromium,
    Brave,
}

impl Keyring {
    fn name(self) -> &'static str {
        match self {
            Self::Chrome => "Chrome",
            Self::Chromium => "Chromium",
            Self::Brave => "Brave",
        }
    }

    /// The Secret Service `application` attribute.
    pub fn application(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Chromium => "chromium",
            Self::Brave => "brave",
        }
    }

    /// The KWallet folder, such as "Chrome Keys".
    pub fn kwallet_folder(self) -> String {
        format!("{} Keys", self.name())
    }

    /// The keyring item's label and KWallet entry, such as "Chrome Safe Storage".
    pub fn entry(self) -> String {
        format!("{} Safe Storage", self.name())
    }
}

/// How the browser was installed, which decides where its profiles live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Install {
    System,
    Flatpak,
    Snap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub browser: Browser,
    pub install: Install,
    /// The name the browser shows for this profile, such as "Work".
    pub name: String,
    pub database: PathBuf,
}

impl Profile {
    /// "Chrome / Work", or "Chrome (Flatpak) / Work" beside a system install.
    pub fn title(&self) -> String {
        match self.install {
            Install::System => format!("{} / {}", self.browser.label(), self.name),
            Install::Flatpak => format!("{} (Flatpak) / {}", self.browser.label(), self.name),
            Install::Snap => format!("{} (Snap) / {}", self.browser.label(), self.name),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    /// Exact cookie host, without its leading domain-cookie dot. Selecting a
    /// host never implicitly selects an unrelated suffix or sibling host.
    pub host: String,
    pub cookies: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Unreadable,
    /// The file is not a readable SQLite database, or is corrupt.
    Damaged,
    Busy,
    /// A cookie database layout this importer has not been checked against,
    /// usually from a newer browser version.
    UnsupportedSchema,
    /// A cookie encryption scheme this importer cannot decrypt.
    UnsupportedEncryption,
    /// The whole profile is beyond the import limits.
    TooLarge,
    /// The selected sites hold more cookie data than one import takes.
    TooManySelected,
    KeyringDenied,
    /// The keyring has no entry under the browser's name.
    KeyMissing,
    Decryption,
    Cancelled,
}

/// Each message says what happened and what to do, without technical
/// detail. None may include a path, a value, or a keyring secret.
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unreadable => "Couldn't read this profile's cookies. Check that its folder is readable and the browser isn't mid-update, then retry.",
            Self::Busy => "The browser kept changing its cookies while they were read. Wait a moment or close the browser, then retry.",
            Self::UnsupportedSchema => "This profile stores cookies in a format this app doesn't recognize yet, possibly from a newer browser version. No cookies were imported from it.",
            Self::UnsupportedEncryption => "This browser encrypts its cookies in a way this app can't read yet. No cookies were imported from it.",
            Self::Damaged => "This profile's cookie file is damaged or isn't a cookie database. Open the browser once so it can repair it, then retry.",
            Self::TooLarge => "This profile is too large to import.",
            Self::TooManySelected => "Too many cookies are selected from this profile. Select fewer sites, then retry.",
            Self::KeyringDenied => "Your desktop keyring didn't allow access to this browser's cookie key. Unlock the keyring and allow access when asked, then retry.",
            Self::KeyMissing => "Couldn't find this browser's cookie key. Check that your desktop keyring is running and unlocked, and that the browser has been opened at least once, then retry.",
            Self::Decryption => "This profile's cookies were saved with a key your desktop keyring no longer has, so none were imported. Sign in again in that browser to save new cookies, then retry.",
            Self::Cancelled => "The import was cancelled.",
        })
    }
}

impl std::error::Error for Error {}

/// No Debug/Serialize: even accidentally logging a batch must not expose a
/// bearer credential. The UI receives only site counts and redacted errors.
pub struct Cookie {
    pub host: String,
    pub domain: bool,
    pub path: String,
    pub name: String,
    pub value: Zeroizing<String>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
    pub expires_unix: Option<i64>,
}

/// Always explicit: the engine would treat a cookie without the attribute as
/// unrestricted, which is broader than Chromium's default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SameSite {
    None,
    Lax,
    Strict,
}

pub struct Batch {
    pub cookies: Vec<Cookie>,
    /// Partitioned/container cookies are deliberately not broadened into the
    /// ordinary jar. Expired and invalid cookies are excluded too.
    pub skipped: usize,
    /// Selected cookies whose value could not be decrypted, such as ones
    /// written under an earlier keyring key. The rest still import.
    pub undecryptable: usize,
}

/// Supplies the stored passwords for a keyring name. More than one may
/// answer, such as a stale item beside the current one; decryption keeps
/// the one that verifies.
pub trait Keys {
    fn passwords(&mut self, keyring: Keyring) -> Result<Vec<Zeroizing<Vec<u8>>>, Error>;
}

/// A scan reads host names and counts only; decryption happens only for sites
/// selected in the subsequent import. SQLite's backup API includes the WAL.
pub fn scan(profile: &Profile, cancel: &AtomicBool) -> Result<Vec<Site>, Error> {
    database::scan(profile, cancel)
}

pub fn extract(
    profile: &Profile,
    selected: &BTreeSet<String>,
    keys: &mut impl Keys,
    cancel: &AtomicBool,
    now_unix: i64,
) -> Result<Batch, Error> {
    database::extract(profile, selected, keys, cancel, now_unix)
}

fn cancelled(cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// Boundary-aware host matching for callers offering a scoped selection.
pub fn matches_host(host: &str, scope: &str) -> bool {
    let host = host.trim_start_matches('.');
    let scope = scope.trim_start_matches('.');
    !scope.is_empty()
        && (host.eq_ignore_ascii_case(scope)
            || host.len() > scope.len()
                && host
                    .get(host.len() - scope.len()..)
                    .is_some_and(|tail| tail.eq_ignore_ascii_case(scope))
                && host.as_bytes()[host.len() - scope.len() - 1] == b'.')
}

#[cfg(feature = "desktop-keyring")]
pub use keyring::DesktopKeys;

#[cfg(test)]
mod tests {
    #[test]
    fn scoped_hosts_respect_label_boundaries() {
        assert!(super::matches_host(".sub.example.com", "example.com"));
        assert!(super::matches_host("EXAMPLE.COM", "example.com"));
        assert!(!super::matches_host("notexample.com", "example.com"));
        assert!(!super::matches_host(
            "example.com.attacker.test",
            "example.com"
        ));
        assert!(!super::matches_host("é.test", "a.test"));
        assert!(!super::matches_host("example.com", ""));
    }

    /// Each browser's key is read under the name its build writes, which
    /// follows branding: forks share Chromium's unless they patch it.
    #[test]
    fn keyring_names_follow_each_browsers_branding() {
        use super::{Browser::*, Keyring};
        for (browser, keyring) in [
            (Chrome, Some(Keyring::Chrome)),
            (ChromeBeta, Some(Keyring::Chrome)),
            (ChromeDev, Some(Keyring::Chrome)),
            (ChromeCanary, Some(Keyring::Chrome)),
            (Vivaldi, Some(Keyring::Chrome)),
            (VivaldiSnapshot, Some(Keyring::Chrome)),
            (Brave, Some(Keyring::Brave)),
            (BraveNightly, Some(Keyring::Brave)),
            (Chromium, Some(Keyring::Chromium)),
            (UngoogledChromium, Some(Keyring::Chromium)),
            (Helium, Some(Keyring::Chromium)),
            (Thorium, Some(Keyring::Chromium)),
            (Edge, Some(Keyring::Chromium)),
            (EdgeCanary, Some(Keyring::Chromium)),
            (Opera, Some(Keyring::Chromium)),
            (OperaGx, Some(Keyring::Chromium)),
            (Firefox, None),
            (LibreWolf, None),
            (Floorp, None),
            (Waterfox, None),
            (Zen, None),
        ] {
            assert_eq!(browser.keyring(), keyring, "{}", browser.label());
        }
        for (keyring, application, folder, entry) in [
            (
                Keyring::Chrome,
                "chrome",
                "Chrome Keys",
                "Chrome Safe Storage",
            ),
            (
                Keyring::Chromium,
                "chromium",
                "Chromium Keys",
                "Chromium Safe Storage",
            ),
            (Keyring::Brave, "brave", "Brave Keys", "Brave Safe Storage"),
        ] {
            assert_eq!(keyring.application(), application);
            assert_eq!(keyring.kwallet_folder(), folder);
            assert_eq!(keyring.entry(), entry);
        }
    }

    #[test]
    fn errors_name_no_paths_or_values() {
        use super::Error::*;
        for error in [
            Unreadable,
            Damaged,
            Busy,
            UnsupportedSchema,
            UnsupportedEncryption,
            TooLarge,
            TooManySelected,
            KeyringDenied,
            KeyMissing,
            Decryption,
            Cancelled,
        ] {
            let message = error.to_string();
            assert!(message.ends_with('.'), "{message}");
            assert!(
                !message.contains('/') && !message.contains("v1"),
                "{message}"
            );
        }
    }
}
