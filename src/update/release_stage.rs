//! Stage a verified Darwin release and transfer the same job to its matched CLI.
use super::*;

const PROGRAMS: &[&str] = &["cagents", "cagents-ui", "cagentsd", "cagents-dictate"];

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stage {
    job: String,
    request: [u8; 32],
    previous: PathBuf,
    binaries: String,
}

pub(super) struct VerifiedCandidate {
    pub root: PathBuf,
    binaries: String,
}

impl VerifiedCandidate {
    pub fn digest(&self) -> Result<String, UpdateError> {
        let digest = programs_digest(&self.root)?;
        if digest != self.binaries {
            return Err(invalid("The verified release files changed"));
        }
        Ok(digest)
    }
}

fn path(state: &Path, job: &str) -> PathBuf {
    state.join(format!("{job}.release-stage.json"))
}

pub(super) fn resume(
    installation: &Installation,
    state: &Path,
    job: &UpdateJob,
    requested: &Release,
    request: &[u8; 32],
) -> Result<VerifiedCandidate, UpdateError> {
    let stage: Stage =
        serde_json::from_slice(&read_private(&path(state, &job.id), SMALL_ASSET_LIMIT)?)?;
    if stage.job != job.id
        || stage.request != *request
        || job.version != requested.version
        || installation.version != requested.version
        || fs::read_link(installation.prefix.join("current"))? != stage.previous
    {
        return Err(invalid(
            "The verified release no longer matches the pending installation",
        ));
    }
    let layout = locate(&crate::process::current_executable()?)?;
    let executable = layout.version_root.join("bin/cagents");
    let named = fs::symlink_metadata(&executable)?;
    if layout.prefix != installation.prefix
        || crate::process::mapped_region_identity(
            std::process::id(),
            resume as *const () as usize as u64,
        )? != (named.dev(), named.ino())
    {
        return Err(invalid(
            "The staged release changed before its update worker could start",
        ));
    }
    Ok(VerifiedCandidate {
        root: layout.version_root,
        binaries: stage.binaries,
    })
}

pub(super) fn extract_and_exec(
    installation: &Installation,
    state: &Path,
    job: &UpdateJob,
    release: &Release,
    request: [u8; 32],
    assets: &feed::VerifiedAssets,
    cancellation: &CancellationToken,
) -> Result<Completion, UpdateError> {
    let current = installation.prefix.join("current");
    let previous = fs::read_link(&current)?;
    let source_root = current.canonicalize()?;
    if source_root
        != installation
            .prefix
            .join("versions")
            .join(&installation.version)
    {
        return Err(invalid(
            "The selected installation changed while downloading the release",
        ));
    }
    let stage_dir = tempfile::Builder::new()
        .prefix(".release-stage-")
        .tempdir_in(state)?;
    extract(
        &installation.prefix,
        stage_dir.path(),
        release,
        assets,
        cancellation,
    )?;
    let archive_name = format!("cagents-{}", release.version);
    let source = stage_dir.path().join(&archive_name);
    let digest = programs_digest(&source)?;
    for binary in PROGRAMS {
        File::open(source.join("bin").join(binary))?.sync_all()?;
    }
    File::open(source.join("bin"))?.sync_all()?;
    // Close-on-exec serializes account publication until the kernel has
    // opened the verified candidate image and replaced this worker process.
    let _publication = crate::install_guard::reserve_darwin_publication()?;
    let root = promote(
        installation,
        stage_dir.path(),
        &archive_name,
        release,
        &previous,
        &digest,
    )?;
    save(
        state,
        &Stage {
            job: job.id.clone(),
            request,
            previous,
            binaries: digest,
        },
    )?;
    confirmation::check_cancelled(state, job)?;
    drop(stage_dir);

    let mut command = Command::new(root.join("bin/cagents"));
    command.args(["update-worker", &job.id]);
    crate::install_guard::sanitize_child_environment(&mut command);
    // run_worker owns the validated lock at this reserved descriptor. exec
    // replaces the process and preserves the one shared lock description.
    candidate::inherit(&mut command, WORKER_LOCK_FD);
    let error = command.exec();
    // The pre-exec hook may have cleared this bit before a failed exec.
    unsafe { libc::fcntl(WORKER_LOCK_FD, libc::F_SETFD, libc::FD_CLOEXEC) };
    Err(error.into())
}

fn owned_directory(path: &Path) -> Result<(), UpdateError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o022 != 0
    {
        return Err(invalid("The release directory is not privately owned"));
    }
    Ok(())
}

fn programs_digest(root: &Path) -> Result<String, UpdateError> {
    let bin = root.join("bin");
    owned_directory(root)?;
    owned_directory(&bin)?;
    for name in PROGRAMS {
        let metadata = fs::symlink_metadata(bin.join(name))?;
        if !metadata.is_file() || metadata.mode() & 0o100 == 0 {
            return Err(invalid(
                "The verified release needs a complete executable set",
            ));
        }
    }
    installed::digest(root, PROGRAMS)
}

fn extract(
    prefix: &Path,
    directory: &Path,
    release: &Release,
    assets: &feed::VerifiedAssets,
    cancellation: &CancellationToken,
) -> Result<(), UpdateError> {
    let mut command = Command::new("/bin/sh");
    crate::install_guard::sanitize_child_environment(&mut command);
    let descriptor = |file: &File| crate::descriptor::inherited_file_path(file.as_raw_fd());
    command
        .arg(descriptor(&assets.installer))
        .arg("--prefix")
        .arg(prefix)
        .args(["--version", &release.version, "--extract-update"])
        .arg(directory)
        .env("CAGENTS_ARCHIVE", descriptor(&assets.archive))
        .env("CAGENTS_CHECKSUMS", descriptor(&assets.checksums));
    for asset in [&assets.installer, &assets.archive, &assets.checksums] {
        candidate::inherit(&mut command, asset.as_raw_fd());
    }
    bounded_command_with_stderr(
        &mut command,
        "Staging the verified Apple Silicon release",
        SMALL_ASSET_LIMIT,
        Duration::from_secs(120),
        cancellation,
        Stdio::inherit(),
    )?;
    Ok(())
}

fn promote(
    installation: &Installation,
    stage: &Path,
    archive_name: &str,
    release: &Release,
    previous: &Path,
    digest: &str,
) -> Result<PathBuf, UpdateError> {
    if fs::read_link(installation.prefix.join("current"))? != previous {
        return Err(invalid(
            "The selected installation changed before the release could be staged",
        ));
    }
    let versions = installation.prefix.join("versions");
    owned_directory(&versions)?;
    let root = versions.join(&release.version);
    let source = stage.join(archive_name);
    let marker = format!("{}\n", installation.version);
    let previous_marker = installed::LOCAL_STAGE_MARKER;
    let mut file = private_create(&source.join(previous_marker))?;
    file.write_all(marker.as_bytes())?;
    file.sync_all()?;
    File::open(&source)?.sync_all()?;
    let stage = crate::fs_safety::Root::open(stage)?;
    let destination = crate::fs_safety::Root::open(&versions)?;
    match crate::fs_safety::rename_new_between(
        &stage,
        std::ffi::OsStr::new(archive_name),
        &destination,
        std::ffi::OsStr::new(&release.version),
    ) {
        Ok(()) => crate::fs_safety::sync_directory(&destination)?,
        Err(error) if error.is_already_exists() => {
            // A prior attempt or rollback may already have installed these
            // exact binaries. Keep them and refresh only the staging marker.
            if root.canonicalize()? != root || programs_digest(&root)? != digest {
                return Err(invalid(
                    "These release files already exist as a different installation",
                ));
            }
            let mut replacement = tempfile::NamedTempFile::new_in(&root)?;
            replacement.write_all(marker.as_bytes())?;
            replacement.as_file().sync_all()?;
            replacement
                .persist(root.join(previous_marker))
                .map_err(|error| error.error)?;
            File::open(&root)?.sync_all()?;
        }
        Err(error) => return Err(invalid(error.to_string())),
    }
    Ok(root)
}

fn save(state: &Path, stage: &Stage) -> Result<(), UpdateError> {
    let mut temporary = tempfile::NamedTempFile::new_in(state)?;
    serde_json::to_writer(&mut temporary, stage)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path(state, &stage.job))
        .map_err(|error| error.error)?;
    File::open(state)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn sealed(bytes: &[u8]) -> File {
        let mut file = anonymous_file().unwrap();
        file.write_all(bytes).unwrap();
        let mut file = seal_file(file).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }

    #[test]
    fn extraction_gets_only_the_inherited_verified_assets_and_its_private_directory() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().canonicalize().unwrap();
        let selected = prefix.join("current");
        std::os::unix::fs::symlink("versions/0.2.3", &selected).unwrap();
        let state = state_dir(&prefix).unwrap();
        let stage = tempfile::Builder::new()
            .prefix(".release-stage-")
            .tempdir_in(&state)
            .unwrap();
        let asset = Asset {
            name: "offline fixture".into(),
            size: 0,
            sha256: [0; 32],
        };
        let release = Release {
            version: "0.2.4".into(),
            url: String::new(),
            notes: String::new(),
            metadata: [0; 32],
            installer: asset.clone(),
            archive: asset.clone(),
            checksums: asset,
        };
        let assets = feed::VerifiedAssets {
            installer: sealed(
                br#"set -eu
test "$1" = --prefix
test "${4}" = 0.2.4
test "${5}" = --extract-update
test "$(cat "$CAGENTS_ARCHIVE")" = exact-archive
test "$(cat "$CAGENTS_CHECKSUMS")" = exact-checksums
test "$(readlink "$2/current")" = versions/0.2.3
test "${6%/*}" = "$2/.update"
mkdir "$6/cagents-0.2.4"
printf 'staged\n' > "$6/cagents-0.2.4/receipt"
"#,
            ),
            archive: sealed(b"exact-archive"),
            checksums: sealed(b"exact-checksums"),
        };
        extract(
            &prefix,
            stage.path(),
            &release,
            &assets,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(
            fs::read_link(selected).unwrap(),
            Path::new("versions/0.2.3")
        );
        assert_eq!(
            fs::read(stage.path().join("cagents-0.2.4/receipt")).unwrap(),
            b"staged\n"
        );
    }

    #[test]
    fn matching_bytes_do_not_qualify_a_non_executable_release() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for name in PROGRAMS {
            let program = bin.join(name);
            fs::write(&program, name).unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let candidate = VerifiedCandidate {
            root: directory.path().to_owned(),
            binaries: programs_digest(directory.path()).unwrap(),
        };
        assert_eq!(candidate.digest().unwrap(), candidate.binaries);
        fs::set_permissions(bin.join("cagents-ui"), fs::Permissions::from_mode(0o600)).unwrap();
        assert!(candidate.digest().is_err());
    }
}
