//! Exact pinned speech archives only. Verify private archive bytes before any
//! extraction, select five literal regular-file members, then publish atomically.
use super::{
    Lease,
    model::{self, Archive, InstalledFile, InstalledManifest},
};
use crate::process::{CancellationToken, CommandSpec, speech_local};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    sync::Arc,
    time::Duration,
};

pub(super) struct VerifiedExtraction {
    pub(super) files: BTreeMap<String, VerifiedMember>,
    pub(super) directory: tempfile::TempDir,
}
pub(super) struct VerifiedArchive(Arc<File>);
pub(super) struct VerifiedMember {
    pub(super) file: Arc<File>,
    pub(super) receipt: InstalledFile,
}
const RUNTIME_MEMBER: &str = "sherpa-onnx-v2.0.0-linux-x64-static-no-tts/bin/sherpa-onnx-offline";
const MODEL_MEMBERS: [&str; 4] = [
    "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/encoder.int8.onnx",
    "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/decoder.int8.onnx",
    "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/joiner.int8.onnx",
    "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/tokens.txt",
];
#[derive(Debug)]
struct ProcessFailure(speech_local::Failure);
impl std::fmt::Display for ProcessFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The owned speech setup process did not complete")
    }
}
impl std::error::Error for ProcessFailure {}
fn process_failure(failure: speech_local::Failure) -> io::Error {
    io::Error::other(ProcessFailure(failure))
}
/// A bounded return can leave the sole reaper retaining the setup lease. Keep
/// that stopping state visible even if the user also requested cancellation.
pub(super) fn lifecycle_error(
    error: &io::Error,
    cancel: &CancellationToken,
) -> Option<super::Error> {
    let failure = error
        .get_ref()
        .and_then(|error| error.downcast_ref::<ProcessFailure>());
    if failure.is_some_and(|failure| failure.0 == speech_local::Failure::CleanupPending) {
        Some(super::Error::CleanupPending)
    } else if cancel.is_cancelled()
        || error.kind() == io::ErrorKind::Interrupted
        || failure.is_some_and(|failure| failure.0 == speech_local::Failure::Cancelled)
    {
        Some(super::Error::Cancelled)
    } else {
        None
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn check_cancel(cancel: &CancellationToken) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Speech setup cancelled",
        ))
    } else {
        Ok(())
    }
}
fn verified_archive(
    mut input: impl Read,
    archive: Archive,
    cancel: &CancellationToken,
) -> io::Result<VerifiedArchive> {
    let mut file = tempfile::tempfile()?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| invalid("Speech archive size overflows"))?;
        if bytes > archive.bytes {
            return Err(invalid("Speech archive exceeds its pinned size"));
        }
        hash.update(&buffer[..read]);
        file.write_all(&buffer[..read])?;
    }
    if bytes != archive.bytes || format!("{:x}", hash.finalize()) != archive.sha256 {
        return Err(invalid("Speech archive does not match the pinned checksum"));
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(VerifiedArchive(Arc::new(file)))
}
/// The caller retains this already-open cache/download descriptor. Readiness
/// derives from the pinned bytes, never a filename or editable receipt.
pub(super) fn verify_file(
    mut file: File,
    archive: Archive,
    cancel: &CancellationToken,
) -> io::Result<VerifiedArchive> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != archive.bytes {
        return Err(invalid("Speech archive does not match its pinned size"));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .filter(|bytes| *bytes <= archive.bytes)
            .ok_or_else(|| invalid("Speech archive exceeds its pinned size"))?;
        hash.update(&buffer[..read]);
    }
    if bytes != archive.bytes || format!("{:x}", hash.finalize()) != archive.sha256 {
        return Err(invalid("Speech archive does not match the pinned checksum"));
    }
    check_cancel(cancel)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(VerifiedArchive(Arc::new(file)))
}
fn tar(archive: Arc<File>) -> io::Result<CommandSpec> {
    let path = format!("--file=/proc/self/fd/{}", archive.as_raw_fd());
    CommandSpec::new("/usr/bin/tar")
        .arg(path)
        .args(["--use-compress-program=/usr/bin/bzip2", "--no-recursion"])
        .env("LC_ALL", "C")
        .env("PATH", "/usr/bin:/bin")
        .env_remove("TAR_OPTIONS")
        .env_remove("BZIP")
        .env_remove("BZIP2")
        .env_remove("BASH_ENV")
        .env_remove("ENV")
        .env_remove("SHELLOPTS")
        .env_remove("LD_PRELOAD")
        .env_remove("LD_LIBRARY_PATH")
        .timeout(Duration::from_secs(120))
        .output_limits(128 * 1024, 16 * 1024)
        .inherit_file(archive)
}
fn regular_members(output: &[u8], wanted: &[&str]) -> io::Result<()> {
    let output =
        std::str::from_utf8(output).map_err(|_| invalid("Speech archive listing is invalid"))?;
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for line in output.lines() {
        let fields: Vec<_> = line.split_ascii_whitespace().collect();
        // Only literal selected names are admitted. Link targets, quoted/newline
        // names, duplicate entries and every non-regular member are rejected.
        if fields.len() != 6
            || !fields[0].starts_with('-')
            || !wanted.contains(&fields[5])
            || !seen.insert(fields[5])
        {
            return Err(invalid("Speech archive contains an unsafe selected member"));
        }
        let size = fields[2]
            .parse::<u64>()
            .map_err(|_| invalid("Speech archive member size is invalid"))?;
        if size == 0 {
            return Err(invalid("Speech archive member is empty"));
        }
        total = total
            .checked_add(size)
            .ok_or_else(|| invalid("Speech archive member size overflows"))?;
        if total > 2 * 1024 * 1024 * 1024 {
            return Err(invalid("Speech archive expands beyond its limit"));
        }
    }
    if seen.len() != wanted.len() {
        return Err(invalid("Speech archive is missing required members"));
    }
    Ok(())
}
fn extract(
    archive: Arc<File>,
    members: &[&str],
    directory: &Path,
    strip: &str,
    cancel: &CancellationToken,
    lease: Arc<Lease>,
) -> io::Result<()> {
    let listing = tar(archive.clone())?
        .args([
            "--list",
            "--verbose",
            "--numeric-owner",
            "--quoting-style=literal",
        ])
        .arg("--")
        .args(members.iter().copied());
    let output =
        speech_local::provision(&listing, cancel, lease.clone()).map_err(process_failure)?;
    regular_members(&output, members)?;
    check_cancel(cancel)?;
    let extracting = tar(archive)?
        .args([
            "--extract",
            "--keep-old-files",
            "--no-same-owner",
            "--no-same-permissions",
        ])
        .arg(format!("--strip-components={strip}"))
        .arg("--directory")
        .arg(directory.as_os_str())
        .arg("--")
        .args(members.iter().copied());
    speech_local::provision(&extracting, cancel, lease).map_err(process_failure)?;
    Ok(())
}

pub(super) fn install(
    root: &Path,
    runtime: impl Read,
    model: impl Read,
    cancel: &CancellationToken,
    lease: Arc<Lease>,
) -> io::Result<VerifiedExtraction> {
    // Both inputs must be authentic before creating any executable installation.
    let runtime = verified_archive(runtime, model::RUNTIME, cancel)?;
    let model = verified_archive(model, model::PARAKEET, cancel)?;
    install_verified(root, runtime, model, cancel, lease)
}
pub(super) fn install_verified(
    root: &Path,
    runtime: VerifiedArchive,
    model: VerifiedArchive,
    cancel: &CancellationToken,
    lease: Arc<Lease>,
) -> io::Result<VerifiedExtraction> {
    check_cancel(cancel)?;
    std::fs::create_dir_all(root)?;
    let staging = tempfile::Builder::new()
        .prefix(".speech-install-")
        .tempdir_in(root)?;
    let bin = staging.path().join("runtime/bin");
    let model_dir = staging.path().join("model");
    std::fs::create_dir_all(&bin)?;
    std::fs::create_dir(&model_dir)?;
    extract(
        runtime.0,
        &[RUNTIME_MEMBER],
        &bin,
        "2",
        cancel,
        lease.clone(),
    )?;
    extract(model.0, &MODEL_MEMBERS, &model_dir, "1", cancel, lease)?;
    check_cancel(cancel)?;
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(staging.path())?;
    let mut files = BTreeMap::new();
    for name in std::iter::once("runtime/bin/sherpa-onnx-offline").chain(
        MODEL_MEMBERS
            .iter()
            .map(|name| name.rsplit('/').next().unwrap())
            .map(|name| match name {
                "encoder.int8.onnx" => "model/encoder.int8.onnx",
                "decoder.int8.onnx" => "model/decoder.int8.onnx",
                "joiner.int8.onnx" => "model/joiner.int8.onnx",
                _ => "model/tokens.txt",
            }),
    ) {
        let mut file = model::open_beneath(&directory, Path::new(name), false)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 2 * 1024 * 1024 * 1024 {
            return Err(invalid("Extracted speech member is invalid"));
        }
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            check_cancel(cancel)?;
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
        }
        file.set_permissions(std::fs::Permissions::from_mode(
            if name.starts_with("runtime/") {
                0o500
            } else {
                0o400
            },
        ))?;
        files.insert(
            name.into(),
            VerifiedMember {
                file: Arc::new(file),
                receipt: InstalledFile {
                    bytes: metadata.len(),
                    sha256: format!("{:x}", hash.finalize()),
                },
            },
        );
    }
    let manifest = InstalledManifest {
        version: 1,
        runtime_sha256: model::RUNTIME.sha256.into(),
        model_sha256: model::PARAKEET.sha256.into(),
        files: files
            .iter()
            .map(|(name, member)| (name.clone(), member.receipt.clone()))
            .collect(),
    };
    let mut receipt = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(staging.path().join("installed.json"))?;
    serde_json::to_writer(&mut receipt, &manifest).map_err(io::Error::other)?;
    receipt.flush()?;
    receipt.sync_all()?;
    check_cancel(cancel)?;
    // Publication is the returned verified owner, not a discoverable marker.
    // Keep the private directory alive through Prepared and every Session, then
    // remove it automatically. Cancelled setup/retries cannot accumulate copies.
    Ok(VerifiedExtraction {
        files,
        directory: staging,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn setup_cleanup_owner_survives_error_conversion_and_concurrent_cancel() {
        let cancel = CancellationToken::new();
        let pending = process_failure(speech_local::Failure::CleanupPending);
        assert_eq!(
            lifecycle_error(&pending, &cancel),
            Some(super::super::Error::CleanupPending)
        );
        cancel.cancel();
        assert_eq!(
            lifecycle_error(&pending, &cancel),
            Some(super::super::Error::CleanupPending)
        );
        let cancelled = process_failure(speech_local::Failure::Cancelled);
        assert_eq!(
            lifecycle_error(&cancelled, &CancellationToken::new()),
            Some(super::super::Error::Cancelled)
        );
    }
    #[test]
    fn archive_checks_reject_wrong_and_excess_bytes_before_extracting() {
        let expected = Archive {
            url: "https://example.invalid/test",
            bytes: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        assert!(verified_archive(&b"abc"[..], expected, &CancellationToken::new()).is_ok());
        assert!(verified_archive(&b"abd"[..], expected, &CancellationToken::new()).is_err());
        assert!(verified_archive(&b"abcd"[..], expected, &CancellationToken::new()).is_err());
    }
    #[test]
    fn selected_archive_members_must_be_exact_unique_regular_files() {
        let good = format!("-rwxr-xr-x 1000/1000 12 2026-10-23 10:10 {RUNTIME_MEMBER}\n");
        assert!(regular_members(good.as_bytes(), &[RUNTIME_MEMBER]).is_ok());
        for unsafe_listing in [
            good.replace("-rwx", "lrwx"),
            good.replace(RUNTIME_MEMBER, "../../escape"),
            format!("{good}{good}"),
            good.replace(RUNTIME_MEMBER, &format!("{RUNTIME_MEMBER} -> /outside")),
        ] {
            assert!(regular_members(unsafe_listing.as_bytes(), &[RUNTIME_MEMBER]).is_err());
        }
    }
}
