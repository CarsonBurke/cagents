//! Where releases are published, and how an installed build reads them.
//!
//! Each release carries `release-metadata.json` and its signature. The latest
//! one is read through github.com's public download redirect
//! (`releases/latest/download/…`), and assets from `releases/download/v…/…`.
//! Those are ordinary web downloads, so a check costs no GitHub API quota and
//! needs no sign-in. Trust comes from the signature alone: the metadata names
//! the SHA-256 of every file the updater downloads, including the installer.
use super::*;
use std::collections::BTreeMap;

const PUBLIC_FEED: &str = "https://github.com/CarsonBurke/cagents/releases";
const METADATA: &str = "release-metadata.json";
const SIGNATURE: &str = "release-metadata.json.sig";
const SCHEMA: u32 = 2;
const TARGET: &str = "x86_64-unknown-linux-gnu";
const PRODUCT: &str = "cagents";
const INSTALLER: &str = "install.sh";
const CHECKSUMS: &str = "SHA256SUMS";
/// A private feed for rehearsing updates: an `https://` URL or `file://`
/// directory with the public layout. Only development and rehearsal builds
/// read it, so the environment cannot redirect a published build's updates.
const FEED_OVERRIDE: &str = "CAGENTS_UPDATE_FEED";

pub(super) struct Feed {
    base: String,
    local: bool,
    keys: Vec<signature::TrustedKey>,
}

#[derive(Deserialize)]
struct Metadata {
    schema_version: u32,
    repository: String,
    version: String,
    #[serde(default)]
    notes: String,
    assets: BTreeMap<String, AssetDigest>,
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct AssetDigest {
    size: u64,
    sha256: String,
}

#[derive(Deserialize)]
struct Target {
    target: String,
    products: BTreeMap<String, Product>,
}

#[derive(Deserialize)]
struct Product {
    archive: String,
    #[serde(default)]
    installer: Option<String>,
    checksums: String,
}

impl Feed {
    /// The feed this build reads, or why it cannot update itself: a plain
    /// sentence for the About page rather than a failure to retry.
    pub(super) fn configured() -> Result<Self, String> {
        let keys = signature::trusted_keys().map_err(|error| error.to_string())?;
        if keys.is_empty() {
            return Err("This build cannot verify releases, so it does not offer updates.".into());
        }
        if !curl_available() {
            return Err("Install curl to receive updates.".into());
        }
        let base = match std::env::var(FEED_OVERRIDE) {
            Ok(base) if cfg!(feature = "development") || signature::rehearsal_build() => base,
            _ => PUBLIC_FEED.to_owned(),
        };
        let base = base.trim_end_matches('/').to_owned();
        let local = base.starts_with("file:///");
        if !local && !base.starts_with("https://") {
            return Err(format!(
                "{FEED_OVERRIDE} must be an https:// or file:/// URL."
            ));
        }
        Ok(Self { base, local, keys })
    }

    /// The newest published release, verified, when `newer` says it is an
    /// update. Its version is read before its signature, so a routine check
    /// that finds nothing new makes one small request, and a release from
    /// before signed metadata reads as nothing new rather than a failure.
    pub(super) fn latest(
        &self,
        newer: impl FnOnce(&str) -> Result<bool, UpdateError>,
        cancellation: &CancellationToken,
    ) -> Result<Option<Release>, UpdateError> {
        let metadata = self.fetch_metadata("latest/download", cancellation)?;
        #[derive(Deserialize)]
        struct Announced {
            version: String,
        }
        let announced: Announced = serde_json::from_slice(&metadata)?;
        if !newer(&announced.version)? {
            return Ok(None);
        }
        self.verified(
            "latest/download",
            metadata,
            Some(&announced.version),
            cancellation,
        )
        .map(Some)
    }

    /// The release tagged `v{version}`, verified.
    pub(super) fn release(
        &self,
        version: &str,
        cancellation: &CancellationToken,
    ) -> Result<Release, UpdateError> {
        self.release_at(&format!("download/v{version}"), Some(version), cancellation)
    }

    fn release_at(
        &self,
        directory: &str,
        expected: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<Release, UpdateError> {
        let metadata = self.fetch_metadata(directory, cancellation)?;
        self.verified(directory, metadata, expected, cancellation)
    }

    fn fetch_metadata(
        &self,
        directory: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<u8>, UpdateError> {
        read_all(self.fetch(
            &format!("{directory}/{METADATA}"),
            METADATA_LIMIT,
            Duration::from_secs(60),
            cancellation,
        )?)
    }

    /// `metadata` from `directory`, once its signature there verifies.
    fn verified(
        &self,
        directory: &str,
        metadata: Vec<u8>,
        expected: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<Release, UpdateError> {
        let signature = read_all(self.fetch(
            &format!("{directory}/{SIGNATURE}"),
            SMALL_ASSET_LIMIT,
            Duration::from_secs(60),
            cancellation,
        )?)?;
        let signature = String::from_utf8(signature)
            .map_err(|_| invalid("the release signature is malformed"))?;
        signature::verify(&metadata, &signature, &self.keys)?;
        let release = self.parse(&metadata)?;
        if expected.is_some_and(|version| version != release.version) {
            return Err(invalid(
                "the release's signed metadata names a different version",
            ));
        }
        Ok(release)
    }

    fn parse(&self, bytes: &[u8]) -> Result<Release, UpdateError> {
        let metadata: Metadata = serde_json::from_slice(bytes)?;
        if metadata.schema_version != SCHEMA {
            return Err(invalid(format!(
                "release metadata schema {} is not one this build reads",
                metadata.schema_version
            )));
        }
        if metadata.repository != REPOSITORY {
            return Err(invalid("release metadata names another repository"));
        }
        let version = metadata.version;
        stable_version(&version)?;
        let product = metadata
            .targets
            .iter()
            .find(|target| target.target == TARGET)
            .and_then(|target| target.products.get(PRODUCT))
            .ok_or_else(|| invalid(format!("the release has no {TARGET} build")))?;
        let archive_name = format!("{PRODUCT}-{version}-{TARGET}.tar.gz");
        if product.archive != archive_name
            || product.installer.as_deref() != Some(INSTALLER)
            || product.checksums != CHECKSUMS
        {
            return Err(invalid("release metadata names unexpected files"));
        }
        let asset = |name: &str, limit: u64| -> Result<Asset, UpdateError> {
            let digest = metadata
                .assets
                .get(name)
                .ok_or_else(|| invalid(format!("release metadata lacks {name}")))?;
            if digest.size == 0 || digest.size > limit {
                return Err(invalid(format!("release asset {name} has an invalid size")));
            }
            Ok(Asset {
                name: name.to_owned(),
                size: digest.size,
                sha256: parse_digest(&digest.sha256)?,
            })
        };
        Ok(Release {
            url: if self.base == PUBLIC_FEED {
                format!("{PUBLIC_FEED}/tag/v{version}")
            } else {
                format!("{}/download/v{version}", self.base)
            },
            notes: metadata.notes,
            metadata: Sha256::digest(bytes).into(),
            archive: asset(&archive_name, ARCHIVE_LIMIT)?,
            checksums: asset(CHECKSUMS, SMALL_ASSET_LIMIT)?,
            installer: asset(INSTALLER, SMALL_ASSET_LIMIT)?,
            version,
        })
    }

    /// One of `release`'s assets, sealed, after its size and SHA-256 match
    /// what the signed metadata says.
    pub(super) fn download(
        &self,
        release: &Release,
        asset: &Asset,
        cancellation: &CancellationToken,
    ) -> Result<File, UpdateError> {
        let mut file = self.fetch(
            &format!("download/v{}/{}", release.version, asset.name),
            asset.size,
            Duration::from_secs(15 * 60),
            cancellation,
        )?;
        if file.metadata()?.len() != asset.size {
            return Err(invalid(format!("download size differs for {}", asset.name)));
        }
        verify_digest(&mut file, &asset.sha256)?;
        Ok(file)
    }

    fn fetch(
        &self,
        path: &str,
        limit: u64,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<File, UpdateError> {
        let mut command = Command::new("curl");
        // `--disable` must come first: it keeps a ~/.curlrc out of the request.
        command
            .args([
                "--disable",
                "--silent",
                "--fail",
                "--location",
                "--max-redirs",
                "10",
                "--proto",
                if self.local { "=file" } else { "=https" },
                "--proto-redir",
                "=https",
                "--connect-timeout",
                "30",
                "--user-agent",
                "cagents-updater",
                "--max-filesize",
            ])
            .arg(limit.to_string())
            .arg("--")
            .arg(format!("{}/{path}", self.base))
            .env_remove("CURL_HOME");
        bounded_command(
            &mut command,
            "release download",
            limit,
            timeout,
            cancellation,
        )
    }
}

fn read_all(mut file: File) -> Result<Vec<u8>, UpdateError> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn parse_digest(text: &str) -> Result<[u8; 32], UpdateError> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return Err(invalid("invalid SHA-256 digest"));
    }
    let mut digest = [0u8; 32];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        let high = (pair[0] as char).to_digit(16);
        let low = (pair[1] as char).to_digit(16);
        let (Some(high), Some(low)) = (high, low) else {
            return Err(invalid("invalid SHA-256 digest"));
        };
        digest[index] = ((high << 4) | low) as u8;
    }
    Ok(digest)
}

fn curl_available() -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| {
            fs::metadata(directory.join("curl"))
                .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed() -> Feed {
        Feed {
            base: PUBLIC_FEED.into(),
            local: false,
            keys: Vec::new(),
        }
    }

    fn metadata(version: &str) -> serde_json::Value {
        let digest = |size: u64| serde_json::json!({ "size": size, "sha256": "ab".repeat(32) });
        serde_json::json!({
            "schema_version": 2,
            "repository": "CarsonBurke/cagents",
            "version": version,
            "source_commit": "0".repeat(40),
            "protocol": 195,
            "notes": "- Updates install with one click",
            "assets": {
                format!("cagents-{version}-x86_64-unknown-linux-gnu.tar.gz"): digest(100),
                "SHA256SUMS": digest(200),
                "install.sh": digest(300),
            },
            "targets": [{
                "target": "x86_64-unknown-linux-gnu",
                "products": {
                    "cagents": {
                        "archive": format!("cagents-{version}-x86_64-unknown-linux-gnu.tar.gz"),
                        "installer": "install.sh",
                        "checksums": "SHA256SUMS",
                    }
                }
            }]
        })
    }

    fn parse(value: &serde_json::Value) -> Result<Release, UpdateError> {
        feed().parse(&serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn signed_metadata_names_every_file_the_update_downloads() {
        let release = parse(&metadata("0.2.0")).unwrap();
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.url, format!("{PUBLIC_FEED}/tag/v0.2.0"));
        assert_eq!(release.notes, "- Updates install with one click");
        assert_eq!(
            release.archive.name,
            "cagents-0.2.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(release.archive.size, 100);
        assert_eq!(release.installer.size, 300);
        assert_eq!(release.checksums.sha256, [0xab; 32]);
    }

    #[test]
    fn metadata_for_another_schema_repository_target_or_file_set_is_refused() {
        let cases: [(&str, fn(&mut serde_json::Value)); 7] = [
            ("schema", |value| value["schema_version"] = 1.into()),
            ("repository", |value| {
                value["repository"] = "someone/else".into()
            }),
            ("prerelease", |value| value["version"] = "0.2.0-rc.1".into()),
            ("target", |value| {
                value["targets"][0]["target"] = "aarch64-apple-darwin".into()
            }),
            ("installer", |value| {
                value["targets"][0]["products"]["cagents"]["installer"] = "other.sh".into()
            }),
            ("missing digest", |value| {
                value["assets"]
                    .as_object_mut()
                    .unwrap()
                    .remove("install.sh");
            }),
            ("oversized", |value| {
                value["assets"]["SHA256SUMS"]["size"] = (SMALL_ASSET_LIMIT + 1).into()
            }),
        ];
        for (case, change) in cases {
            let mut value = metadata("0.2.0");
            change(&mut value);
            assert!(parse(&value).is_err(), "{case} was accepted");
        }
    }

    /// A `file://` feed holding one signed release, read through real curl.
    #[test]
    fn a_signed_release_is_discovered_and_downloaded_and_tampering_is_refused() {
        if !curl_available() {
            eprintln!("skipping: curl is not installed");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let version = "0.2.0";
        let files: [(String, &[u8]); 3] = [
            (
                format!("cagents-{version}-x86_64-unknown-linux-gnu.tar.gz"),
                b"archive bytes",
            ),
            ("SHA256SUMS".into(), b"checksum bytes"),
            ("install.sh".into(), b"installer bytes"),
        ];
        let mut value = metadata(version);
        let release_dir = root.path().join(format!("download/v{version}"));
        let latest_dir = root.path().join("latest/download");
        fs::create_dir_all(&release_dir).unwrap();
        fs::create_dir_all(&latest_dir).unwrap();
        for (name, bytes) in &files {
            fs::write(release_dir.join(name), bytes).unwrap();
            value["assets"][name] = serde_json::json!({
                "size": bytes.len(),
                "sha256": format!("{:x}", Sha256::digest(bytes)),
            });
        }
        let bytes = serde_json::to_vec(&value).unwrap();
        let signer = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let (key, signature) = signature::sign_for_test(&signer, &bytes);
        for directory in [&release_dir, &latest_dir] {
            fs::write(directory.join(METADATA), &bytes).unwrap();
            fs::write(directory.join(SIGNATURE), &signature).unwrap();
        }
        let feed = Feed {
            base: format!("file://{}", root.path().display()),
            local: true,
            keys: vec![key],
        };
        let cancellation = CancellationToken::new();
        let newer = |version: &str| -> Result<bool, UpdateError> { Ok(version == "0.2.0") };
        let latest = feed.latest(newer, &cancellation).unwrap().unwrap();
        assert_eq!(latest.version, version);
        assert_eq!(feed.release(version, &cancellation).unwrap(), latest);
        let mut archive = feed
            .download(&latest, &latest.archive, &cancellation)
            .unwrap();
        let mut downloaded = Vec::new();
        archive.read_to_end(&mut downloaded).unwrap();
        assert_eq!(downloaded, b"archive bytes");

        // Same size, different bytes: the signed digest refuses it.
        fs::write(release_dir.join("install.sh"), b"installer bytez").unwrap();
        assert!(
            feed.download(&latest, &latest.installer, &cancellation)
                .is_err()
        );
        // Metadata changed after signing is refused before anything downloads.
        let mut forged = value.clone();
        forged["notes"] = "forged".into();
        fs::write(
            latest_dir.join(METADATA),
            serde_json::to_vec(&forged).unwrap(),
        )
        .unwrap();
        assert!(feed.latest(newer, &cancellation).is_err());
        // Nothing newer is decided from the version alone: a release from
        // before signed metadata, with no signature, is simply not an update.
        fs::write(
            latest_dir.join(METADATA),
            br#"{"schema_version":1,"version":"0.1.3"}"#,
        )
        .unwrap();
        fs::remove_file(latest_dir.join(SIGNATURE)).unwrap();
        assert_eq!(feed.latest(newer, &cancellation).unwrap(), None);
        // Signed metadata served under another release's tag is not that release.
        let replayed = root.path().join("download/v0.2.1");
        fs::create_dir_all(&replayed).unwrap();
        fs::write(replayed.join(METADATA), &bytes).unwrap();
        fs::write(replayed.join(SIGNATURE), &signature).unwrap();
        assert!(
            feed.release("0.2.1", &cancellation)
                .unwrap_err()
                .to_string()
                .contains("different version")
        );
        let untrusted = Feed {
            keys: vec![
                signature::sign_for_test(&ed25519_dalek::SigningKey::from_bytes(&[8; 32]), b"").0,
            ],
            ..feed
        };
        assert!(untrusted.release(version, &cancellation).is_err());
    }

    #[test]
    fn a_digest_must_be_exactly_sixty_four_hex_digits() {
        assert_eq!(parse_digest(&"0f".repeat(32)).unwrap(), [0x0f; 32]);
        assert!(parse_digest(&"0f".repeat(31)).is_err());
        assert!(parse_digest(&"zz".repeat(32)).is_err());
    }
}
