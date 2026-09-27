//! Local, read-only browser cookie import. Callers must obtain consent before
//! discovery or scanning. Values never implement Debug or serialization and
//! are erased when the import batch is dropped.

mod chromium;
mod database;
mod discovery;

use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};
use zeroize::Zeroizing;

pub use discovery::discover;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Browser {
    Chrome,
    Chromium,
    Brave,
    Firefox,
    Zen,
}

impl Browser {
    pub fn label(self) -> &'static str {
        match self {
            Self::Chrome => "Chrome",
            Self::Chromium => "Chromium",
            Self::Brave => "Brave",
            Self::Firefox => "Firefox",
            Self::Zen => "Zen",
        }
    }

    fn chromium(self) -> bool {
        matches!(self, Self::Chrome | Self::Chromium | Self::Brave)
    }

    fn application(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Chromium => "chromium",
            Self::Brave => "brave",
            _ => "",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub browser: Browser,
    pub name: String,
    pub database: PathBuf,
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
    Busy,
    Unsupported,
    TooLarge,
    KeyringDenied,
    Decryption,
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unreadable => "The cookies database could not be read. Check its permissions and retry.",
            Self::Busy => "The browser's cookies database is busy. Close that browser and retry.",
            Self::Unsupported => "This profile uses a cookie format that is not supported.",
            Self::TooLarge => "This profile exceeds the bounded import size.",
            Self::KeyringDenied => "The desktop keyring did not provide this browser's key. Unlock it and retry.",
            Self::Decryption => "Some encrypted cookies could not be verified. Unlock the browser's keyring and retry.",
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SameSite {
    Unspecified,
    None,
    Lax,
    Strict,
}

pub struct Batch {
    pub cookies: Vec<Cookie>,
    /// Partitioned/container cookies are deliberately not broadened into the
    /// ordinary jar. Expired and invalid cookies are excluded too.
    pub skipped: usize,
}

pub trait Keys {
    fn password(&mut self, browser: Browser) -> Result<Zeroizing<Vec<u8>>, Error>;
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
pub use chromium::DesktopKeys;

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
}
