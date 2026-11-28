use std::{
    ffi::OsStr,
    fs::{self, File},
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::account::probe::{find_executable, run_provider_command};

const RELEASES: &str = "https://downloads.claude.ai/claude-code-releases";
// Anthropic's published Claude Code release key, fingerprint
// 31DD DE24 DDFA B679 F42D 7BD2 BAA9 29FF 1A7E CACE.
const SIGNING_KEY: &[u8] = include_bytes!("claude-code.gpg");
const MAX_BINARY_BYTES: u64 = 768 * 1024 * 1024;

pub(super) struct Source {
    curl: PathBuf,
    gpgv: PathBuf,
    target: &'static str,
}

impl Source {
    pub(super) fn discover(path: Option<&OsStr>) -> Result<Self, String> {
        let target = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") if cfg!(target_env = "gnu") => "linux-x64",
            ("linux", "aarch64") if cfg!(target_env = "gnu") => "linux-arm64",
            ("macos", "aarch64") => "darwin-arm64",
            ("macos", "x86_64") => "darwin-x64",
            _ => return Err("Managed Claude Code updates are unavailable on this platform".into()),
        };
        let tool = |name| {
            find_executable(name, path)
                .ok_or_else(|| format!("{name} is required to verify Claude Code releases"))
        };
        Ok(Self {
            curl: tool("curl")?,
            gpgv: tool("gpgv")?,
            target,
        })
    }

    pub(super) async fn version(&self, channel: &str, stage: &Path) -> Result<Version, String> {
        let path = stage.join("channel");
        self.fetch(channel, &path, 256, 12).await?;
        let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
        parse_version(text.trim())
    }

    pub(super) async fn stage(&self, version: &Version, stage: &Path) -> Result<PathBuf, String> {
        let manifest = stage.join("manifest.json");
        let signature = stage.join("manifest.json.sig");
        let key = stage.join("claude-code.gpg");
        self.fetch(
            &format!("{version}/manifest.json"),
            &manifest,
            64 * 1024,
            12,
        )
        .await?;
        self.fetch(
            &format!("{version}/manifest.json.sig"),
            &signature,
            8 * 1024,
            12,
        )
        .await?;
        fs::write(&key, SIGNING_KEY).map_err(|error| error.to_string())?;
        let mut verify = Command::new(&self.gpgv);
        verify
            .arg("--homedir")
            .arg(stage)
            .arg("--keyring")
            .arg(key)
            .arg(signature)
            .arg(&manifest);
        let (status, _) = run_provider_command(verify, Duration::from_secs(5)).await?;
        if !status.success() {
            return Err(format!(
                "Claude Code {version} has an invalid release signature"
            ));
        }
        let artifact = artifact(&manifest, version, self.target)?;
        let binary = stage.join("claude");
        self.fetch(
            &format!("{version}/{}/claude", self.target),
            &binary,
            artifact.size,
            180,
        )
        .await?;
        artifact.verify(&binary)?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o500))
            .map_err(|error| error.to_string())?;
        Ok(binary)
    }

    async fn fetch(
        &self,
        release_path: &str,
        output: &Path,
        max_bytes: u64,
        seconds: u64,
    ) -> Result<(), String> {
        let mut command = Command::new(&self.curl);
        command
            .args([
                "-q",
                "--fail",
                "--silent",
                "--location",
                "--proto",
                "=https",
            ])
            .args(["--proto-redir", "=https", "--max-redirs", "3", "--max-time"])
            .arg(seconds.to_string())
            .args(["--connect-timeout", "5", "--max-filesize"])
            .arg(max_bytes.to_string())
            .arg("--output")
            .arg(output)
            .arg(format!("{RELEASES}/{release_path}"));
        let (status, _) = run_provider_command(command, Duration::from_secs(seconds + 1)).await?;
        if !status.success() {
            return Err(format!(
                "Could not download Claude Code {release_path} ({status})"
            ));
        }
        if fs::metadata(output)
            .map_err(|error| error.to_string())?
            .len()
            > max_bytes
        {
            return Err(format!(
                "Claude Code {release_path} exceeded its download size limit"
            ));
        }
        Ok(())
    }
}

pub(super) fn parse_version(value: &str) -> Result<Version, String> {
    let version = Version::parse(value).map_err(|_| "Invalid Claude Code release version")?;
    if value.len() > 64 || !version.pre.is_empty() || !version.build.is_empty() {
        return Err("Expected a numbered, stable Claude Code release".into());
    }
    Ok(version)
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    platforms: std::collections::BTreeMap<String, Artifact>,
}

#[derive(Deserialize)]
struct Artifact {
    binary: String,
    checksum: String,
    size: u64,
}

fn artifact(path: &Path, version: &Version, target: &str) -> Result<Artifact, String> {
    let manifest: Manifest =
        serde_json::from_reader(File::open(path).map_err(|error| error.to_string())?)
            .map_err(|_| "Invalid Claude Code release manifest")?;
    if parse_version(&manifest.version)? != *version {
        return Err("Claude Code release manifest names a different version".into());
    }
    let entry = manifest
        .platforms
        .into_iter()
        .find_map(|(platform, entry)| (platform == target).then_some(entry))
        .ok_or_else(|| format!("Claude Code {version} has no artifact for {target}"))?;
    if entry.binary != "claude"
        || entry.size == 0
        || entry.size > MAX_BINARY_BYTES
        || entry.checksum.len() != 64
        || !entry.checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("Invalid Claude Code artifact in the release manifest".into());
    }
    Ok(entry)
}

impl Artifact {
    fn verify(&self, binary: &Path) -> Result<(), String> {
        let check = || {
            let mut file = File::open(binary)?;
            let mut digest = Sha256::new();
            let size = io::copy(&mut file, &mut digest)?;
            Ok::<_, io::Error>((size, format!("{:x}", digest.finalize())))
        };
        let (size, checksum) = check().map_err(|error| error.to_string())?;
        if size != self.size || !checksum.eq_ignore_ascii_case(&self.checksum) {
            return Err("Claude Code download does not match its signed manifest".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_binds_the_requested_version_platform_and_bytes() {
        let stage = tempfile::tempdir().unwrap();
        let manifest = stage.path().join("manifest.json");
        let binary = stage.path().join("claude");
        fs::write(&binary, b"hello").unwrap();
        fs::write(
            &manifest,
            r#"{"version":"2.0.1","platforms":{"linux-x64":{"binary":"claude","size":5,"checksum":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}}}"#,
        )
        .unwrap();
        let version = Version::new(2, 0, 1);
        assert!(artifact(&manifest, &Version::new(2, 0, 2), "linux-x64").is_err());
        assert!(artifact(&manifest, &version, "darwin-arm64").is_err());
        let artifact = artifact(&manifest, &version, "linux-x64").unwrap();
        artifact.verify(&binary).unwrap();
        fs::write(&binary, b"world").unwrap();
        assert!(artifact.verify(&binary).is_err());
    }
}
