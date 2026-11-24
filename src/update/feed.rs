//! Where releases are published, and how an installed build reads them.
//!
//! Each release carries `release-metadata-2.json` and its signature. The latest
//! one is read through github.com's public download redirect
//! (`releases/latest/download/…`), and assets from `releases/download/v…/…`.
//! Those are ordinary web downloads, so a check costs no GitHub API quota and
//! needs no sign-in. Trust comes from the signature alone: the metadata names
//! the SHA-256 of every file the updater downloads, including the installer.
//!
//! Releases up to 0.2.4 carry their metadata as `release-metadata.json`, the
//! only name 0.2.3 reads, and 0.2.3 trusts only the key 0.2.4 is signed with.
//! Every later release keeps 0.2.4's own signed metadata under that name, so
//! 0.2.3 finds 0.2.4 as its update, and from 0.2.4 reaches the rest, which
//! keys 0.2.3 does not know may sign.
use super::*;
use std::collections::BTreeMap;

const PUBLIC_FEED: &str = "https://github.com/CarsonBurke/cagents/releases";
const METADATA: &str = "release-metadata-2.json";
const SIGNATURE: &str = "release-metadata-2.json.sig";
/// What a release from before `release-metadata-2.json` carries instead.
const LEGACY_METADATA: &str = "release-metadata.json";
const LEGACY_SIGNATURE: &str = "release-metadata.json.sig";
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
    /// update. Its version is read before anything else, so a routine check
    /// that finds nothing new makes one small request, and a release from
    /// before signed metadata reads as nothing new rather than a failure.
    pub(super) fn latest(
        &self,
        newer: impl FnOnce(&str) -> Result<bool, UpdateError>,
        cancellation: &CancellationToken,
    ) -> Result<Option<Release>, UpdateError> {
        let (metadata, _) = self.fetch_metadata("latest/download", cancellation)?;
        #[derive(Deserialize)]
        struct Announced {
            version: String,
        }
        let announced: Announced = serde_json::from_slice(&metadata)?;
        if !newer(&announced.version)? {
            return Ok(None);
        }
        // The tagged copy, so a release published between two requests
        // cannot pair one release's metadata with another's signature.
        self.release(&announced.version, cancellation).map(Some)
    }

    /// The release tagged `v{version}`, verified.
    pub(super) fn release(
        &self,
        version: &str,
        cancellation: &CancellationToken,
    ) -> Result<Release, UpdateError> {
        let directory = format!("download/v{version}");
        let (metadata, signature) = self.fetch_metadata(&directory, cancellation)?;
        let signature = match self.fetch(
            &format!("{directory}/{signature}"),
            "reading the release's signature",
            SMALL_ASSET_LIMIT,
            Duration::from_secs(60),
            cancellation,
        ) {
            Ok(file) => read_all(file)?,
            // Its metadata was just read from the same place, so a missing
            // signature is a release published without one.
            Err(error) if self.missing(&error) => {
                return Err(invalid(format!(
                    "cagents {version} is not signed, so this build will not install it"
                )));
            }
            Err(error) => return Err(error),
        };
        let signature = String::from_utf8(signature)
            .map_err(|_| invalid("the release signature is malformed"))?;
        signature::verify(&metadata, &signature, &self.keys)?;
        let release = self.parse(&metadata)?;
        if release.version != version {
            return Err(invalid(
                "the release's signed metadata names a different version",
            ));
        }
        Ok(release)
    }

    /// The metadata in `directory`, and the name of the signature that goes
    /// with it: `release-metadata-2.json`, or what a release from before it
    /// carries.
    fn fetch_metadata(
        &self,
        directory: &str,
        cancellation: &CancellationToken,
    ) -> Result<(Vec<u8>, &'static str), UpdateError> {
        for (metadata, signature) in [(METADATA, SIGNATURE), (LEGACY_METADATA, LEGACY_SIGNATURE)] {
            match self.fetch(
                &format!("{directory}/{metadata}"),
                "checking for a new release",
                METADATA_LIMIT,
                Duration::from_secs(60),
                cancellation,
            ) {
                Ok(file) => return Ok((read_all(file)?, signature)),
                Err(error) if self.missing(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Err(invalid(
            "no published release of cagents offers in-app updates yet",
        ))
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
            "downloading the release",
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
        operation: &'static str,
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
        bounded_command(&mut command, operation, limit, timeout, cancellation).map_err(|error| {
            match error {
                UpdateError::Exited { status, .. } if !self.missing_status(status) => {
                    curl_failure(operation, status)
                }
                error => error,
            }
        })
    }

    /// Whether the feed answered that the file does not exist: an HTTP error
    /// under `--fail`, or an unreadable file in a file feed.
    fn missing(&self, error: &UpdateError) -> bool {
        matches!(error, UpdateError::Exited { status, .. } if self.missing_status(*status))
    }

    fn missing_status(&self, status: std::process::ExitStatus) -> bool {
        match status.code() {
            Some(22) => true,
            Some(37) => self.local,
            _ => false,
        }
    }
}

/// curl's failures, in words for the person checking for an update.
fn curl_failure(operation: &'static str, status: std::process::ExitStatus) -> UpdateError {
    match status.code() {
        Some(5..=7) => invalid(format!(
            "{operation} failed: cannot reach github.com; check the network connection"
        )),
        Some(28) => UpdateError::Timeout(operation),
        Some(35 | 51 | 53 | 54 | 58 | 59 | 60 | 77 | 80 | 83 | 90 | 91) => invalid(format!(
            "{operation} failed: no secure connection to github.com could be made"
        )),
        Some(56 | 52 | 18) => invalid(format!(
            "{operation} failed: the connection to github.com broke off; try again"
        )),
        Some(63) => invalid(format!("{operation} exceeds its size limit")),
        _ => UpdateError::Exited { operation, status },
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
        assert_eq!(
            archive.write_all(b"unverified bytes").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );

        // Same size, different bytes: the signed digests refuse them.
        fs::write(release_dir.join(&latest.archive.name), b"archive bytez").unwrap();
        assert_eq!(
            feed.download(&latest, &latest.archive, &cancellation)
                .unwrap_err()
                .to_string(),
            "release SHA-256 verification failed; installation was not changed"
        );
        fs::write(release_dir.join("install.sh"), b"installer bytez").unwrap();
        assert!(
            feed.download(&latest, &latest.installer, &cancellation)
                .is_err()
        );
        // Metadata changed after signing is refused before anything downloads.
        let mut forged = value.clone();
        forged["notes"] = "forged".into();
        fs::write(
            release_dir.join(METADATA),
            serde_json::to_vec(&forged).unwrap(),
        )
        .unwrap();
        assert!(feed.latest(newer, &cancellation).is_err());
        // A release published without a signature says so.
        fs::write(release_dir.join(METADATA), &bytes).unwrap();
        fs::remove_file(release_dir.join(SIGNATURE)).unwrap();
        assert_eq!(
            feed.latest(newer, &cancellation).unwrap_err().to_string(),
            "cagents 0.2.0 is not signed, so this build will not install it"
        );
        fs::write(release_dir.join(SIGNATURE), &signature).unwrap();
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

    /// Writes a signed release of `version` into `root/download/v{version}`
    /// under `names`, signed by `signers`, and returns its metadata.
    fn publish(
        root: &std::path::Path,
        version: &str,
        names: &[(&str, &str)],
        signers: &[&ed25519_dalek::SigningKey],
    ) -> Vec<u8> {
        let directory = root.join(format!("download/v{version}"));
        fs::create_dir_all(&directory).unwrap();
        let mut value = metadata(version);
        let archive = format!("cagents-{version}-x86_64-unknown-linux-gnu.tar.gz");
        for (name, bytes) in [
            (archive.as_str(), &b"archive"[..]),
            ("SHA256SUMS", b"sums"),
            ("install.sh", b"installer"),
        ] {
            fs::write(directory.join(name), bytes).unwrap();
            value["assets"][name] = serde_json::json!({
                "size": bytes.len(),
                "sha256": format!("{:x}", Sha256::digest(bytes)),
            });
        }
        let bytes = serde_json::to_vec(&value).unwrap();
        let signature: String = signers
            .iter()
            .map(|signer| signature::sign_for_test(signer, &bytes).1)
            .collect();
        for (metadata, sig) in names {
            fs::write(directory.join(metadata), &bytes).unwrap();
            fs::write(directory.join(sig), &signature).unwrap();
        }
        bytes
    }

    fn file_feed(root: &std::path::Path, signer: &ed25519_dalek::SigningKey) -> Feed {
        Feed {
            base: format!("file://{}", root.display()),
            local: true,
            keys: vec![signature::sign_for_test(signer, b"").0],
        }
    }

    /// The published layout from 0.2.5 on: the release's own metadata as
    /// `release-metadata-2.json`, and 0.2.4's, signed by the key 0.2.3
    /// trusts, as `release-metadata.json`.
    #[test]
    fn builds_that_trust_only_the_old_key_are_led_to_the_release_that_trusts_the_new_one() {
        if !curl_available() {
            eprintln!("skipping: curl is not installed");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let old = ed25519_dalek::SigningKey::from_bytes(&[21; 32]);
        let new = ed25519_dalek::SigningKey::from_bytes(&[22; 32]);
        let both = [(METADATA, SIGNATURE), (LEGACY_METADATA, LEGACY_SIGNATURE)];
        let bridge = publish(root.path(), "0.2.4", &both, &[&old]);
        publish(root.path(), "0.2.5", &[(METADATA, SIGNATURE)], &[&new]);
        let latest = root.path().join("latest/download");
        fs::create_dir_all(&latest).unwrap();
        let current = root.path().join("download/v0.2.5");
        for name in [METADATA, SIGNATURE] {
            fs::copy(current.join(name), latest.join(name)).unwrap();
        }
        let bridged = root.path().join("download/v0.2.4");
        for name in [LEGACY_METADATA, LEGACY_SIGNATURE] {
            fs::copy(bridged.join(name), current.join(name)).unwrap();
            fs::copy(bridged.join(name), latest.join(name)).unwrap();
        }
        let cancellation = CancellationToken::new();
        let newer = |installed: &'static str| {
            move |version: &str| -> Result<bool, UpdateError> {
                Ok(version_key(version) > version_key(installed))
            }
        };

        // What 0.2.3 reads: only the legacy names, only the old key.
        let legacy = fs::read(latest.join(LEGACY_METADATA)).unwrap();
        assert_eq!(legacy, bridge);
        let announced: serde_json::Value = serde_json::from_slice(&legacy).unwrap();
        assert_eq!(announced["version"], "0.2.4");
        let tagged = root.path().join("download/v0.2.4");
        signature::verify(
            &fs::read(tagged.join(LEGACY_METADATA)).unwrap(),
            &fs::read_to_string(tagged.join(LEGACY_SIGNATURE)).unwrap(),
            &[signature::sign_for_test(&old, b"").0],
        )
        .unwrap();

        // 0.2.4, which trusts the new key, finds 0.2.5.
        let feed = file_feed(root.path(), &new);
        let found = feed.latest(newer("0.2.4"), &cancellation).unwrap().unwrap();
        assert_eq!(found.version, "0.2.5");
        assert_eq!(feed.latest(newer("0.2.5"), &cancellation).unwrap(), None);
    }

    /// A feed whose newest release predates `release-metadata-2.json` still
    /// reads, through the name that release carries.
    #[test]
    fn a_release_from_before_generation_2_metadata_still_reads() {
        if !curl_available() {
            eprintln!("skipping: curl is not installed");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let key = ed25519_dalek::SigningKey::from_bytes(&[23; 32]);
        publish(
            root.path(),
            "0.2.3",
            &[(LEGACY_METADATA, LEGACY_SIGNATURE)],
            &[&key],
        );
        let latest = root.path().join("latest/download");
        fs::create_dir_all(&latest).unwrap();
        for name in [LEGACY_METADATA, LEGACY_SIGNATURE] {
            fs::copy(
                root.path().join("download/v0.2.3").join(name),
                latest.join(name),
            )
            .unwrap();
        }
        let feed = file_feed(root.path(), &key);
        let cancellation = CancellationToken::new();
        let found = feed
            .latest(|version| Ok(version == "0.2.3"), &cancellation)
            .unwrap()
            .unwrap();
        assert_eq!(found.version, "0.2.3");
    }

    fn version_key(version: &str) -> Vec<u32> {
        version
            .split('.')
            .map(|part| part.parse().unwrap())
            .collect()
    }

    #[test]
    fn only_a_missing_file_reads_as_unpublished_and_other_curl_failures_say_why() {
        use std::os::unix::process::ExitStatusExt;
        let status = |code: i32| std::process::ExitStatus::from_raw(code << 8);
        let exited = |code: i32| UpdateError::Exited {
            operation: "reading the release's signature",
            status: status(code),
        };
        let public = feed();
        let local = Feed {
            local: true,
            ..feed()
        };
        assert!(public.missing(&exited(22)));
        assert!(local.missing(&exited(22)) && local.missing(&exited(37)));
        // A network failure on the signature is not an unsigned release.
        for code in [6, 7, 35, 37, 56] {
            assert!(!public.missing(&exited(code)), "curl exit {code}");
        }
        assert_eq!(
            curl_failure("checking for a new release", status(6)).to_string(),
            "checking for a new release failed: cannot reach github.com; check the network connection"
        );
        assert!(matches!(
            curl_failure("checking for a new release", status(28)),
            UpdateError::Timeout("checking for a new release")
        ));
        assert!(
            curl_failure("downloading the release", status(60))
                .to_string()
                .contains("no secure connection")
        );
        assert_eq!(
            curl_failure("downloading the release", status(99)).to_string(),
            "downloading the release failed (exit status: 99)"
        );
    }

    #[test]
    fn a_digest_must_be_exactly_sixty_four_hex_digits() {
        assert_eq!(parse_digest(&"0f".repeat(32)).unwrap(), [0x0f; 32]);
        assert!(parse_digest(&"0f".repeat(31)).is_err());
        assert!(parse_digest(&"zz".repeat(32)).is_err());
    }
}
