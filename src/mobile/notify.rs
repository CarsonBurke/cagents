//! Local mobile relay configuration. The subscription topic lives with the
//! SSH pairing, outside settings, daemon IPC, and normal CLI output.
use std::{
    fs,
    io::{self, Read, Write},
    net::IpAddr,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use url::{Host, Url};

pub const DEFAULT_SERVER: &str = "https://ntfy.sh/";
const MAX_SETTINGS: u64 = 4096;

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Detail {
    #[default]
    Generic,
    Titles,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    #[serde(deserialize_with = "deserialize_origin")]
    pub server: Url,
    pub detail: Detail,
    pub finished: bool,
    /// Distinguishes a disable/re-enable from a period of uninterrupted consent
    /// even when both writes land between observations by the daemon.
    #[doc(hidden)]
    pub revision: Option<uuid::Uuid>,
    /// The selected SSH key's fingerprint. The topic is resolved from the
    /// current pairing before delivery.
    pub phone: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            server: Url::parse(DEFAULT_SERVER).expect("fixed relay origin"),
            detail: Detail::Generic,
            finished: false,
            revision: None,
            phone: None,
        }
    }
}

/// Only the HTTPS origin of a public relay. The daemon additionally validates
/// the actual addresses returned by DNS before opening a socket.
pub fn relay_origin(input: &str) -> Result<Url, &'static str> {
    const INVALID: &str = "Enter the HTTPS origin of a public relay, such as https://ntfy.sh";
    // Check the original spelling too. URL normalization can discard an empty
    // userinfo or a path with only dot segments.
    let authority = input
        .strip_prefix("https://")
        .ok_or(INVALID)?
        .trim_end_matches('/');
    if authority.is_empty()
        || authority.contains(['/', '@', '\\', '?', '#', '%'])
        || authority
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
        || input.ends_with("//")
    {
        return Err(INVALID);
    }
    let url = Url::parse(input).map_err(|_| INVALID)?;
    let allowed = match url.host() {
        Some(Host::Domain(host)) => {
            let host = host.trim_end_matches('.');
            host.contains('.')
                && ![
                    "localhost",
                    "local",
                    "internal",
                    "lan",
                    "home",
                    "test",
                    "invalid",
                    "onion",
                ]
                .iter()
                .any(|suffix| host.ends_with(&format!(".{suffix}")))
        }
        Some(Host::Ipv4(ip)) => public_address(ip.into()),
        Some(Host::Ipv6(ip)) => public_address(ip.into()),
        None => false,
    };
    allowed.then_some(url).ok_or(INVALID)
}

fn deserialize_origin<'de, D: serde::Deserializer<'de>>(decoder: D) -> Result<Url, D::Error> {
    let input = String::deserialize(decoder)?;
    relay_origin(&input).map_err(serde::de::Error::custom)
}

pub fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a >= 224
                || ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_documentation()
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && ((b == 0 && c == 0) || (b == 88 && c == 99)))
                || (a == 198 && (18..=19).contains(&b)))
        }
        IpAddr::V6(ip) => {
            let [a, b, ..] = ip.segments();
            // Global unicast only, excluding special-purpose, documentation
            // and 6to4 ranges. This also excludes IPv4 mapping and NAT64.
            a & 0xe000 == 0x2000
                && !(a == 0x2001 && (b < 0x0200 || b == 0x0db8))
                && a != 0x2002
                && !(a == 0x3fff && b < 0x1000)
        }
    }
}

pub fn settings_path() -> Option<PathBuf> {
    crate::instance::project_dirs().map(|dirs| dirs.config_dir().join("mobile-notify.json"))
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self, &'static str> {
        let file = match crate::secure_file::open_existing_private_file_readonly(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(_) => return Err("Could not read private mobile notification settings"),
        };
        let mut bytes = Vec::new();
        file.take(MAX_SETTINGS + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Could not read mobile notification settings")?;
        const INVALID: &str = "Mobile notification settings are invalid";
        if bytes.len() as u64 > MAX_SETTINGS {
            return Err(INVALID);
        }
        serde_json::from_slice(&bytes).map_err(|_| INVALID)
    }

    pub fn update(
        path: &Path,
        change: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<Self, String> {
        let directory = path.parent().ok_or("Cannot find the settings directory")?;
        // Keep permissions on the existing application configuration directory.
        fs::create_dir_all(directory).map_err(|_| "Could not create the settings directory")?;
        crate::secure_file::reject_symlink_components(directory)
            .map_err(|_| "Could not open the settings directory safely")?;
        crate::secure_file::validate_owned_directory(directory)
            .map_err(|_| "Could not open the settings directory safely")?;
        let _lock = crate::instance::owned_lock(&path.with_extension("lock"), libc::LOCK_EX)
            .map_err(|_| "Could not lock mobile notification settings")?;
        let previous = Self::load(path)?;
        let mut settings = previous.clone();
        change(&mut settings)?;
        if settings == previous {
            return Ok(settings);
        }
        settings.revision = Some(uuid::Uuid::new_v4());
        let encoded = serde_json::to_vec(&settings)
            .map_err(|_| "Could not encode mobile notification settings")?;
        let write = || -> io::Result<()> {
            use std::os::unix::fs::PermissionsExt;
            let mut file = tempfile::Builder::new()
                .permissions(fs::Permissions::from_mode(0o600))
                .tempfile_in(directory)?;
            file.write_all(&encoded)?;
            file.as_file().sync_all()?;
            file.persist(path)?;
            fs::File::open(directory)?.sync_all()
        };
        write().map_err(|_| "Could not save mobile notification settings")?;
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_scope_rejects_credentials_paths_and_nonpublic_destinations() {
        for invalid in [
            "http://ntfy.sh",
            "https://@ntfy.sh",
            "https://ntfy.sh/tag/..",
            "https://ntfy.sh/#",
            "https://ntfy.sh/?x",
            "https://ntfy.sh//",
            "https://ntfy.sh\\foo",
            "https://ntfy.sh ",
            "https://localhost",
            "https://relay.local",
            "https://127.0.0.1",
            "https://127.1",
            "https://10.1.2.3",
            "https://169.254.169.254",
            "https://100.64.1.2",
            "https://[::1]",
            "https://[::ffff:127.0.0.1]",
            "https://[2002:7f00:1::]",
        ] {
            assert!(relay_origin(invalid).is_err(), "{invalid}");
        }
        for valid in [
            "https://ntfy.sh",
            "https://relay.example.org:8443/",
            "https://[2606:4700::1]",
        ] {
            assert!(relay_origin(valid).is_ok(), "{valid}");
        }
    }
}
