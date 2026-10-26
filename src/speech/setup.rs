//! Explicit optional setup, never a side effect of discovery or opening chat.
//! Only the two pinned public archives are fetched. Saved archives are verified
//! again before reuse; editable receipts never manufacture a Ready microphone.
use super::{Error, Lease, Prepared, model, provision};
use crate::process::{CancellationToken, CommandSpec, speech_local};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub const DOWNLOAD_BYTES: u64 = model::RUNTIME.bytes + model::PARAKEET.bytes;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    CheckingSaved,
    Downloading { bytes: u64, total: u64 },
    Verifying,
    Extracting,
}

/// Runs only after an explicit setup action, on a blocking worker. The selected
/// project/device and provider credentials are not consulted. The caller chooses
/// the app's local cache root; it must never pass a remote workspace directory.
pub fn prepare(
    root: &Path,
    cancel: &CancellationToken,
    mut progress: impl FnMut(Progress),
) -> Result<Prepared, Error> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err(Error::Unavailable);
    }
    let lease = Arc::new(Lease::acquire()?);
    check_cancel(cancel)?;
    let archives = root.join("archives");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&archives)
        .map_err(|_| Error::SetupStorage)?;
    progress(Progress::CheckingSaved);
    let runtime = saved(&archives, model::RUNTIME, cancel)?;
    let model = saved(&archives, model::PARAKEET, cancel)?;
    let mut downloaded = if runtime.is_some() {
        model::RUNTIME.bytes
    } else {
        0
    } + if model.is_some() {
        model::PARAKEET.bytes
    } else {
        0
    };
    let mut pending_runtime = None;
    let mut pending_model = None;
    if runtime.is_none() || model.is_none() {
        progress(Progress::Downloading {
            bytes: downloaded,
            total: DOWNLOAD_BYTES,
        });
    }
    for (archive, ready, pending) in [
        (model::RUNTIME, runtime.is_some(), &mut pending_runtime),
        (model::PARAKEET, model.is_some(), &mut pending_model),
    ] {
        check_cancel(cancel)?;
        if ready {
            continue;
        }
        let base = downloaded;
        // UI notifications are bounded independently of curl's read/chunk rate.
        let mut last = Instant::now();
        *pending = Some(fetch(&archives, archive, cancel, lease.clone(), |bytes| {
            if last.elapsed() >= Duration::from_millis(150) {
                last = Instant::now();
                progress(Progress::Downloading {
                    bytes: base + bytes,
                    total: DOWNLOAD_BYTES,
                });
            }
        })?);
        downloaded += archive.bytes;
        progress(Progress::Downloading {
            bytes: downloaded,
            total: DOWNLOAD_BYTES,
        });
    }
    check_cancel(cancel)?;
    if pending_runtime.is_some() || pending_model.is_some() {
        progress(Progress::Verifying);
    }
    let runtime = finish(runtime, pending_runtime, model::RUNTIME, cancel)?;
    let model = finish(model, pending_model, model::PARAKEET, cancel)?;
    check_cancel(cancel)?;
    progress(Progress::Extracting);
    let extraction = provision::install_verified(
        &root.join("installations"),
        runtime,
        model,
        cancel,
        lease.clone(),
    )
    .map_err(|_| {
        if cancel.is_cancelled() {
            Error::Cancelled
        } else {
            Error::SetupIntegrity
        }
    })?;
    let installation = model::Installation::from_extraction(extraction, cancel).map_err(|_| {
        if cancel.is_cancelled() {
            Error::Cancelled
        } else {
            Error::SetupIntegrity
        }
    })?;
    check_cancel(cancel)?;
    Ok(Prepared(Arc::new(installation)))
}
fn check_cancel(cancel: &CancellationToken) -> Result<(), Error> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
fn path(root: &Path, archive: model::Archive) -> PathBuf {
    root.join(format!("{}.tar.bz2", archive.sha256))
}
fn saved(
    root: &Path,
    archive: model::Archive,
    cancel: &CancellationToken,
) -> Result<Option<provision::VerifiedArchive>, Error> {
    check_cancel(cancel)?;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path(root, archive))
    {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let verified = provision::verify_file(file, archive, cancel).ok();
    check_cancel(cancel)?;
    Ok(verified)
}
struct Pending {
    file: tempfile::NamedTempFile,
    destination: PathBuf,
}
fn fetch(
    root: &Path,
    archive: model::Archive,
    cancel: &CancellationToken,
    lease: Arc<Lease>,
    progress: impl FnMut(u64),
) -> Result<Pending, Error> {
    let file = tempfile::Builder::new()
        .prefix(".speech-download-")
        .tempfile_in(root)
        .map_err(|_| Error::SetupStorage)?;
    let output = file
        .as_file()
        .try_clone()
        .map_err(|_| Error::SetupStorage)?;
    let command = curl(archive);
    speech_local::download(&command, output, cancel, lease, progress).map_err(|failure| {
        match failure {
            speech_local::Failure::Cancelled => Error::Cancelled,
            speech_local::Failure::CleanupPending => Error::CleanupPending,
            speech_local::Failure::Io => Error::SetupStorage,
            _ => Error::SetupDownload,
        }
    })?;
    check_cancel(cancel)?;
    Ok(Pending {
        file,
        destination: path(root, archive),
    })
}
fn curl(archive: model::Archive) -> CommandSpec {
    // -q MUST be first. No curlrc, netrc, authorization headers, account lookup,
    // shell or arbitrary URL. GitHub's public asset redirect stays HTTPS-only.
    CommandSpec::new("/usr/bin/curl")
        .args([
            "-q",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-redirs",
            "5",
            "--connect-timeout",
            "15",
            "--max-time",
            "900",
            "--max-filesize",
        ])
        .arg(archive.bytes.to_string())
        .args(["--url", archive.url])
        .env("LC_ALL", "C")
        .env_remove("LD_PRELOAD")
        .env_remove("LD_LIBRARY_PATH")
        .timeout(Duration::from_secs(900))
        .output_limits(archive.bytes as usize, 16 * 1024)
}
fn finish(
    ready: Option<provision::VerifiedArchive>,
    pending: Option<Pending>,
    archive: model::Archive,
    cancel: &CancellationToken,
) -> Result<provision::VerifiedArchive, Error> {
    if let Some(ready) = ready {
        return Ok(ready);
    }
    let pending = pending.ok_or(Error::SetupDownload)?;
    publish(pending, archive, cancel)
}
fn publish(
    pending: Pending,
    archive: model::Archive,
    cancel: &CancellationToken,
) -> Result<provision::VerifiedArchive, Error> {
    check_cancel(cancel)?;
    let descriptor = pending
        .file
        .as_file()
        .try_clone()
        .map_err(|_| Error::SetupStorage)?;
    let verified = provision::verify_file(descriptor, archive, cancel).map_err(|_| {
        if cancel.is_cancelled() {
            Error::Cancelled
        } else {
            Error::SetupIntegrity
        }
    })?;
    pending
        .file
        .as_file()
        .sync_all()
        .map_err(|_| Error::SetupStorage)?;
    check_cancel(cancel)?;
    // Rename publishes only complete verified bytes, replacing an invalid cache
    // entry itself rather than following it. Interrupted downloads are TempFiles.
    pending
        .file
        .persist(&pending.destination)
        .map_err(|_| Error::SetupStorage)?;
    if let Some(directory) = pending.destination.parent() {
        File::open(directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| Error::SetupStorage)?;
    }
    check_cancel(cancel)?;
    Ok(verified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    const SMALL: model::Archive = model::Archive {
        url: "https://example.invalid/not-used",
        bytes: 3,
        sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    };
    fn pending(root: &Path, bytes: &[u8]) -> Pending {
        let mut file = tempfile::NamedTempFile::new_in(root).unwrap();
        file.write_all(bytes).unwrap();
        Pending {
            file,
            destination: path(root, SMALL),
        }
    }
    #[test]
    fn only_verified_bytes_publish_and_saved_archive_is_rechecked() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            publish(
                pending(root.path(), b"abd"),
                SMALL,
                &CancellationToken::new()
            )
            .is_err()
        );
        assert!(!path(root.path(), SMALL).exists());
        publish(
            pending(root.path(), b"abc"),
            SMALL,
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(
            saved(root.path(), SMALL, &CancellationToken::new())
                .unwrap()
                .is_some()
        );
        std::fs::write(path(root.path(), SMALL), b"abd").unwrap();
        assert!(
            saved(root.path(), SMALL, &CancellationToken::new())
                .unwrap()
                .is_none()
        );
        publish(
            pending(root.path(), b"abc"),
            SMALL,
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(
            saved(root.path(), SMALL, &CancellationToken::new())
                .unwrap()
                .is_some()
        );
    }
    #[test]
    fn cancellation_drops_partial_without_publishing_and_symlink_cache_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let partial = pending(root.path(), b"ab");
        let partial_path = partial.file.path().to_owned();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            publish(partial, SMALL, &cancel).err(),
            Some(Error::Cancelled)
        );
        assert!(!partial_path.exists());
        assert!(!path(root.path(), SMALL).exists());
        let external = root.path().join("external");
        std::fs::write(&external, b"abc").unwrap();
        std::os::unix::fs::symlink(&external, path(root.path(), SMALL)).unwrap();
        assert!(
            saved(root.path(), SMALL, &CancellationToken::new())
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn exact_public_download_commands_disable_configuration_and_bound_transport() {
        for archive in [model::RUNTIME, model::PARAKEET] {
            let spec = curl(archive);
            assert_eq!(spec.program, Path::new("/usr/bin/curl"));
            assert_eq!(spec.arguments[0], "-q");
            assert_eq!(spec.arguments.last().unwrap(), archive.url);
            assert_eq!(spec.stdout_limit, archive.bytes as usize);
            assert_eq!(spec.timeout, Some(Duration::from_secs(900)));
            assert!(
                spec.arguments
                    .windows(2)
                    .any(|args| args[0] == "--proto-redir" && args[1] == "=https")
            );
        }
    }
}
