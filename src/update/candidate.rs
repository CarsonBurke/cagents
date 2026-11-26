//! Probe the executable that will actually be installed, from verified bytes.
use super::*;

pub(super) struct Staged {
    pub identity: impact::Candidate,
    executable: Executable,
}

enum Executable {
    Sealed(File),
    #[cfg(target_os = "macos")]
    Installed {
        path: PathBuf,
        original: fs::Metadata,
    },
}

impl Staged {
    pub fn preview(&self, policy: impact::Policy) -> Result<impact::Preview, UpdateError> {
        match observe_impact(&self.identity, policy) {
            Ok(preview) => Ok(preview),
            Err(original) => {
                // A local install may already have replaced the daemon. Its
                // actual candidate handshake proves that only this old UI
                // needs replacement; the old CLI's mismatch is not an outage.
                if self
                    .verify_compatible_handshake(&CancellationToken::new())
                    .is_ok()
                {
                    Ok(impact::Preview {
                        from_version: String::new(),
                        windows: 0,
                        stopped: Vec::new(),
                        stopped_windows: Vec::new(),
                        labels: Default::default(),
                        editors: crate::install_guard::editor_companions(),
                        candidate: self.identity.clone(),
                        replaces_daemon: false,
                        daemon: None,
                    })
                } else {
                    Err(original)
                }
            }
        }
    }
    pub fn from_archive(
        release: &Release,
        archive: &File,
        digest: &[u8; 32],
        cancellation: &CancellationToken,
    ) -> Result<Self, UpdateError> {
        let archive_fd = archive.as_raw_fd();
        let mut extract = Command::new("tar");
        extract
            .args(["-xOz", "--file"])
            .arg(format!("/proc/self/fd/{archive_fd}"))
            .arg("--")
            .arg(format!("cagents-{}/bin/cagents", release.version));
        inherit(&mut extract, archive_fd);
        let executable = bounded_command(
            &mut extract,
            "candidate extraction",
            512 * 1024 * 1024,
            Duration::from_secs(120),
            cancellation,
        )?;
        Self::from_file(
            executable,
            release.version.clone(),
            digest.iter().map(|byte| format!("{byte:02x}")).collect(),
            cancellation,
        )
    }

    pub(super) fn from_file(
        executable: File,
        version: String,
        digest: String,
        cancellation: &CancellationToken,
    ) -> Result<Self, UpdateError> {
        // This is a private anonymous file populated only from checksum-verified
        // release bytes. Its content is already sealed against replacement.
        if unsafe { libc::fchmod(executable.as_raw_fd(), 0o700) } == -1 {
            return Err(io::Error::last_os_error().into());
        }
        Self::probe_protocol(
            Executable::Sealed(executable),
            version,
            digest,
            cancellation,
        )
    }

    pub(super) fn from_installed(
        executable: PathBuf,
        version: String,
        digest: String,
        cancellation: &CancellationToken,
    ) -> Result<Self, UpdateError> {
        if !executable.is_absolute() {
            return Err(invalid("the staged candidate CLI path is not absolute"));
        }
        // Darwin cannot exec a descriptor path. Use the exact owned, staged
        // CLI there without consulting the public selector or PATH.
        #[cfg(target_os = "macos")]
        {
            let original = fs::symlink_metadata(&executable)?;
            Self::probe_protocol(
                Executable::Installed {
                    path: executable,
                    original,
                },
                version,
                digest,
                cancellation,
            )
        }

        #[cfg(not(target_os = "macos"))]
        {
            let source = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(executable)?;
            let mut sealed = anonymous_file()?;
            io::copy(&mut source.take(512 * 1024 * 1024 + 1), &mut sealed)?;
            if sealed.metadata()?.len() > 512 * 1024 * 1024 {
                return Err(invalid("installed CLI exceeds update size limit"));
            }
            Self::from_file(seal_file(sealed)?, version, digest, cancellation)
        }
    }

    fn probe_protocol(
        executable: Executable,
        version: String,
        digest: String,
        cancellation: &CancellationToken,
    ) -> Result<Self, UpdateError> {
        let envelope = executable.probe(
            &["--json", "daemon", "protocol-version"],
            "candidate protocol probe",
            cancellation,
        )?;
        let protocol = envelope
            .get("data")
            .and_then(|data| data.get("protocol_version"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| invalid("candidate did not report a valid daemon protocol"))?;
        Ok(Self {
            identity: impact::Candidate {
                version,
                protocol,
                digest,
            },
            executable,
        })
    }

    pub fn verify_compatible_handshake(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(), UpdateError> {
        self.daemon_running(cancellation).map(|_| ())
    }

    pub fn daemon_running(&self, cancellation: &CancellationToken) -> Result<bool, UpdateError> {
        let envelope = self.executable.probe(
            &["--json", "daemon", "status"],
            "candidate daemon handshake",
            cancellation,
        )?;
        let data = envelope
            .get("data")
            .ok_or_else(|| invalid("candidate did not return a valid daemon status"))?;
        let running = data
            .get("running")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| invalid("candidate did not return a valid daemon status"))?;
        if running
            && data
                .get("daemon_version")
                .and_then(serde_json::Value::as_str)
                .is_none()
        {
            return Err(invalid(
                "The terminal service is starting; wait for its compatibility handshake before updating",
            ));
        }
        Ok(running)
    }
}

impl Executable {
    fn probe(
        &self,
        arguments: &[&str],
        operation: &'static str,
        cancellation: &CancellationToken,
    ) -> Result<serde_json::Value, UpdateError> {
        match self {
            Self::Sealed(executable) => {
                let fd = executable.as_raw_fd();
                let mut command = Command::new(format!("/proc/self/fd/{fd}"));
                inherit(&mut command, fd);
                command.args(arguments);
                let output = bounded_command(
                    &mut command,
                    operation,
                    SMALL_ASSET_LIMIT,
                    Duration::from_secs(10),
                    cancellation,
                )?;
                Ok(serde_json::from_reader(output)?)
            }
            #[cfg(target_os = "macos")]
            Self::Installed { path, original } => {
                if !same_companion_executable(original, &fs::symlink_metadata(path)?) {
                    return Err(invalid("The staged candidate CLI changed before preflight"));
                }
                let output = companion_read(
                    path,
                    arguments,
                    None,
                    "The staged candidate CLI",
                    operation,
                    cancellation,
                )?;
                if !same_companion_executable(original, &fs::symlink_metadata(path)?) {
                    return Err(invalid("The staged candidate CLI changed during preflight"));
                }
                Ok(output)
            }
        }
    }
}

pub(super) fn inherit(command: &mut Command, fd: i32) {
    // SAFETY: the caller keeps the File alive through spawn; only an
    // async-signal-safe descriptor operation executes in the child.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn installed_candidate_probes_its_owned_physical_cli() {
        let directory = tempfile::tempdir().unwrap();
        let cli = directory.path().join("cagents");
        fs::write(
            &cli,
            r#"#!/bin/sh
case "$*" in
    '--json daemon protocol-version') printf '%s\n' '{"data":{"protocol_version":123}}' ;;
    '--json daemon status') printf '%s\n' '{"data":{"running":false}}' ;;
    *) exit 42 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        let cancellation = CancellationToken::new();
        let staged = Staged::from_installed(
            cli.clone(),
            "9.8.7".into(),
            "bound digest".into(),
            &cancellation,
        )
        .unwrap();
        assert_eq!(staged.identity.protocol, 123);
        assert_eq!(staged.identity.digest, "bound digest");
        assert!(!staged.daemon_running(&cancellation).unwrap());
        cancellation.cancel();
        assert!(matches!(
            staged.daemon_running(&cancellation),
            Err(UpdateError::Cancelled)
        ));

        let other = directory.path().join("other");
        fs::rename(&cli, &other).unwrap();
        fs::copy(&other, &cli).unwrap();
        assert!(staged.daemon_running(&CancellationToken::new()).is_err());
        fs::remove_file(&cli).unwrap();
        std::os::unix::fs::symlink("other", cli).unwrap();
        assert!(staged.daemon_running(&CancellationToken::new()).is_err());
    }

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "self-update is Linux-only: it relies on memfd seals and /proc"
    )]
    fn protocol_comes_from_verified_candidate_execution() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("cagents-9.8.7/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            bin.join("cagents"),
            "#!/bin/sh\n[ \"$*\" = '--json daemon protocol-version' ] || exit 42\nprintf '%s\\n' '{\"data\":{\"protocol_version\":123}}'\n",
        ).unwrap();
        let archive_path = directory.path().join("release.tar.gz");
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&archive_path)
                .arg("-C")
                .arg(directory.path())
                .arg("cagents-9.8.7")
                .status()
                .unwrap()
                .success()
        );
        let bytes = fs::read(&archive_path).unwrap();
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let mut archive = anonymous_file().unwrap();
        archive.write_all(&bytes).unwrap();
        let mut archive = seal_file(archive).unwrap();
        verify_digest(&mut archive, &digest).unwrap();
        let asset = Asset {
            name: "fixture".into(),
            size: 1,
            sha256: digest,
        };
        let release = Release {
            version: "9.8.7".into(),
            url: String::new(),
            notes: String::new(),
            metadata: [1; 32],
            archive: asset.clone(),
            installer: asset.clone(),
            checksums: asset,
        };
        let candidate =
            Staged::from_archive(&release, &archive, &digest, &CancellationToken::new()).unwrap();
        assert_eq!(candidate.identity.protocol, 123);
        assert_eq!(candidate.identity.version, "9.8.7");
        assert_eq!(candidate.identity.digest.len(), 64);
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            Staged::from_archive(&release, &archive, &digest, &cancellation),
            Err(UpdateError::Cancelled)
        ));
    }
}
