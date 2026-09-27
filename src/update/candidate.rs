//! Probe the executable that will actually be installed, from verified bytes.
use super::*;

pub(super) struct Staged {
    pub identity: impact::Candidate,
    executable: File,
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
        let mut staged = Self {
            identity: impact::Candidate {
                version,
                protocol: 0,
                digest,
            },
            executable,
        };
        let mut command = staged.command();
        command.args(["--json", "daemon", "protocol-version"]);
        let mut output = bounded_command(
            &mut command,
            "candidate protocol probe",
            SMALL_ASSET_LIMIT,
            Duration::from_secs(10),
            cancellation,
        )?;
        let envelope: serde_json::Value = serde_json::from_reader(&mut output)?;
        staged.identity.protocol = envelope
            .get("data")
            .and_then(|data| data.get("protocol_version"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| invalid("candidate did not report a valid daemon protocol"))?;
        Ok(staged)
    }

    pub fn command(&self) -> Command {
        let fd = self.executable.as_raw_fd();
        let mut command = Command::new(format!("/proc/self/fd/{fd}"));
        inherit(&mut command, fd);
        command
    }

    pub fn verify_compatible_handshake(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(), UpdateError> {
        self.daemon_running(cancellation).map(|_| ())
    }

    pub fn daemon_running(&self, cancellation: &CancellationToken) -> Result<bool, UpdateError> {
        let mut command = self.command();
        command.args(["--json", "daemon", "status"]);
        let mut output = bounded_command(
            &mut command,
            "candidate daemon handshake",
            SMALL_ASSET_LIMIT,
            Duration::from_secs(10),
            cancellation,
        )?;
        let envelope: serde_json::Value = serde_json::from_reader(&mut output)?;
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

fn inherit(command: &mut Command, fd: i32) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
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
        seal_file(&archive).unwrap();
        verify_digest(&mut archive, &digest).unwrap();
        let asset = Asset {
            id: 1,
            name: "fixture".into(),
            size: 1,
        };
        let release = Release {
            version: "9.8.7".into(),
            url: String::new(),
            id: 1,
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
