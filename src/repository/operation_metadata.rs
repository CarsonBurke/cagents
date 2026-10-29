//! Bounded no-follow reads of Git's operation markers and generated drafts.
use super::*;
use std::os::unix::fs::MetadataExt;

const MAX_OPERATION_REF_BYTES: u64 = 64 * 1024;
const MAX_OPERATION_MESSAGE_BYTES: u64 = 1024 * 1024;

fn invalid(reason: impl fmt::Display) -> RepositoryError {
    RepositoryError::InvalidOutput {
        field: "Git operation metadata",
        reason: reason.to_string(),
    }
}
fn metadata(root: &fs_safety::Root, path: &str) -> Result<Option<fs::Metadata>, RepositoryError> {
    match fs_safety::metadata_beneath(root, Path::new(path)) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(invalid(error)),
    }
}
fn version(m: &fs::Metadata) -> (u64, u64, u64, u32, i64, i64, i64, i64) {
    (
        m.dev(),
        m.ino(),
        m.len(),
        m.mode(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    )
}
fn read(root: &fs_safety::Root, path: &str, limit: u64) -> Result<Option<String>, RepositoryError> {
    let mut file = match fs_safety::open_beneath(root, Path::new(path)) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(invalid(error)),
    };
    let before = file.metadata().map_err(invalid)?;
    if !before.is_file() {
        return Err(invalid(format!("{path} is not a regular file")));
    }
    if before.len() > limit {
        return Err(invalid(format!(
            "{path} exceeds its {limit}-byte read limit"
        )));
    }
    let bytes = fs_safety::read_bounded(&mut file, limit).map_err(invalid)?;
    if bytes.len() as u64 > limit {
        return Err(invalid(format!(
            "{path} exceeds its {limit}-byte read limit"
        )));
    }
    let after = file.metadata().map_err(invalid)?;
    if version(&before) != version(&after) {
        return Err(invalid(format!("{path} changed while being read")));
    }
    let text =
        String::from_utf8(bytes).map_err(|_| invalid(format!("{path} is not valid UTF-8")))?;
    Ok(Some(text.trim().to_owned()).filter(|text| !text.is_empty()))
}

fn optional_message(root: &fs_safety::Root, path: &str) -> Option<String> {
    // A generated draft is only a composer prefill. Keep the strict reader's
    // refusal, but do not let that optional file hide a validated operation.
    read(root, path, MAX_OPERATION_MESSAGE_BYTES).ok().flatten()
}

pub(super) fn read_operation_state(
    directory: &Path,
) -> Result<Option<GitOperationState>, RepositoryError> {
    let root = fs_safety::Root::open(directory).map_err(invalid)?;
    let mut rebase = None;
    for path in ["rebase-merge", "rebase-apply"] {
        if let Some(metadata) = metadata(&root, path)? {
            if !metadata.is_dir() {
                return Err(invalid(format!("{path} is not a directory")));
            }
            rebase = Some(path);
            break;
        }
    }
    let (kind, incoming_ref, message) = if let Some(prefix) = rebase {
        let message = optional_message(&root, &format!("{prefix}/message"));
        (
            GitOperationKind::Rebase,
            read(&root, &format!("{prefix}/onto"), MAX_OPERATION_REF_BYTES)?,
            message.or_else(|| optional_message(&root, "MERGE_MSG")),
        )
    } else {
        let mut found = None;
        for (path, kind) in [
            ("MERGE_HEAD", GitOperationKind::Merge),
            ("CHERRY_PICK_HEAD", GitOperationKind::CherryPick),
            ("REVERT_HEAD", GitOperationKind::Revert),
        ] {
            if metadata(&root, path)?.is_some() {
                found = Some((
                    kind,
                    read(&root, path, MAX_OPERATION_REF_BYTES)?,
                    optional_message(&root, "MERGE_MSG"),
                ));
                break;
            }
        }
        let Some(state) = found else {
            return Ok(None);
        };
        state
    };
    Ok(Some(GitOperationState {
        kind,
        incoming_ref,
        message,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn operation_metadata_keeps_complete_bounded_merge_and_rebase_drafts() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("MERGE_HEAD"), "abc\n").unwrap();
        let draft = "m".repeat(MAX_OPERATION_MESSAGE_BYTES as usize);
        fs::write(dir.path().join("MERGE_MSG"), &draft).unwrap();
        let state = read_operation_state(dir.path()).unwrap().unwrap();
        assert_eq!(state.kind, GitOperationKind::Merge);
        assert_eq!(state.incoming_ref.as_deref(), Some("abc"));
        assert_eq!(state.message.as_deref(), Some(draft.as_str()));
        fs::create_dir(dir.path().join("rebase-merge")).unwrap();
        fs::write(dir.path().join("rebase-merge/onto"), "def\n").unwrap();
        fs::write(dir.path().join("rebase-merge/message"), "Subject\n\nBody\n").unwrap();
        let state = read_operation_state(dir.path()).unwrap().unwrap();
        assert_eq!(state.kind, GitOperationKind::Rebase);
        assert_eq!(state.incoming_ref.as_deref(), Some("def"));
        assert_eq!(state.message.as_deref(), Some("Subject\n\nBody"));
    }

    #[test]
    fn refused_optional_drafts_keep_the_operation_and_strict_reader_refusal() {
        for case in 0..4 {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("MERGE_HEAD"), "abc\n").unwrap();
            let message = dir.path().join("MERGE_MSG");
            match case {
                0 => {
                    use std::os::unix::ffi::OsStrExt;
                    let path = std::ffi::CString::new(message.as_os_str().as_bytes()).unwrap();
                    // SAFETY: path is a valid live NUL-terminated fixture path.
                    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
                }
                1 => {
                    let outside = dir.path().join("outside");
                    fs::write(&outside, "secret").unwrap();
                    symlink(outside, &message).unwrap();
                }
                2 => {
                    fs::File::create(&message)
                        .unwrap()
                        .set_len(MAX_OPERATION_MESSAGE_BYTES + 1)
                        .unwrap();
                }
                _ => fs::write(&message, [0xff]).unwrap(),
            }
            let root = fs_safety::Root::open(dir.path()).unwrap();
            let error = read(&root, "MERGE_MSG", MAX_OPERATION_MESSAGE_BYTES)
                .unwrap_err()
                .to_string();
            assert!(error.contains("Git operation metadata"), "{case}: {error}");
            let state = read_operation_state(dir.path()).unwrap().unwrap();
            assert_eq!(state.kind, GitOperationKind::Merge);
            assert_eq!(state.incoming_ref.as_deref(), Some("abc"));
            assert_eq!(state.message, None);
        }
    }

    #[test]
    fn rebase_optional_drafts_use_only_an_independently_readable_fallback() {
        for prefix in ["rebase-merge", "rebase-apply"] {
            for primary in [None, Some("primary"), Some("refused")] {
                for fallback in [None, Some("fallback"), Some("refused")] {
                    let dir = tempfile::tempdir().unwrap();
                    fs::create_dir(dir.path().join(prefix)).unwrap();
                    fs::write(dir.path().join(prefix).join("onto"), "abc\n").unwrap();
                    for (path, draft) in [
                        (dir.path().join(prefix).join("message"), primary),
                        (dir.path().join("MERGE_MSG"), fallback),
                    ] {
                        match draft {
                            None => {}
                            Some("refused") => fs::write(path, [0xff]).unwrap(),
                            Some(text) => fs::write(path, text).unwrap(),
                        }
                    }
                    let state = read_operation_state(dir.path()).unwrap().unwrap();
                    assert_eq!(state.kind, GitOperationKind::Rebase);
                    assert_eq!(state.incoming_ref.as_deref(), Some("abc"));
                    let expected = primary
                        .filter(|draft| *draft != "refused")
                        .or_else(|| fallback.filter(|draft| *draft != "refused"));
                    assert_eq!(
                        state.message.as_deref(),
                        expected,
                        "{prefix}: {primary:?}, {fallback:?}"
                    );
                    fs::write(dir.path().join(prefix).join("onto"), [0xff]).unwrap();
                    assert!(
                        read_operation_state(dir.path()).is_err(),
                        "required ref stays strict"
                    );
                }
            }
        }
    }

    #[test]
    fn operation_metadata_refuses_fifo_head_and_linked_rebase_directory() {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::ffi::OsStrExt;
        let path =
            std::ffi::CString::new(dir.path().join("MERGE_HEAD").as_os_str().as_bytes()).unwrap();
        // SAFETY: path is a valid live NUL-terminated fixture path.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(read_operation_state(dir.path()).is_err());
        fs::remove_file(dir.path().join("MERGE_HEAD")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("rebase-apply")).unwrap();
        assert!(read_operation_state(dir.path()).is_err());
    }
}
