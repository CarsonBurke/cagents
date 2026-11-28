//! Whether a provider's answer can have changed since its account was last
//! probed.
//!
//! Usage figures change as an account is used, so those probes follow the
//! schedule. Muse reports whether the sign-in works, the plan and the models.
//! Its probe runs when the CLI, sign-in or catalog changes, or after
//! [`UNCHANGED_FOR`] has passed in case something changed elsewhere.
//!
//! The reader's own refreshes, sign-ins and a conversation's report that
//! the account needs attention always probe.

use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{
    account::{Account, UsageStatus},
    agent::Provider,
};

/// How long an answer whose inputs are unchanged is taken to still stand.
pub(super) const UNCHANGED_FOR: u64 = 60 * 60_000;

/// What `account`'s answer depends on, run as `program`, as a fingerprint:
/// `None` for a provider whose answer changes without its inputs changing.
pub(super) fn fingerprint(
    account: &Account,
    program: &Path,
    environment: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    if account.provider != Provider::Muse {
        return None;
    }
    let mut paths = vec![program.to_path_buf()];
    paths.push(muse_sign_in(account, environment)?);
    let catalog = super::muse::data_home(environment)?.join("model-catalog");
    // Sorted before it is cut short, so a long catalog is read as
    // the same entries each time.
    let mut entries = std::fs::read_dir(&catalog)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .take(4096)
                .collect::<Vec<PathBuf>>()
        })
        .unwrap_or_default();
    entries.sort();
    entries.truncate(256);
    paths.push(catalog);
    paths.extend(entries);
    let mut digest = Sha256::new();
    for path in paths {
        digest.update(path.as_os_str().as_encoded_bytes());
        digest.update([0]);
        // Whatever replaces or rewrites a file changes one of these.
        match std::fs::metadata(&path) {
            Ok(metadata) => {
                for value in [
                    metadata.dev(),
                    metadata.ino(),
                    metadata.len(),
                    metadata.mtime() as u64,
                    metadata.mtime_nsec() as u64,
                ] {
                    digest.update(value.to_le_bytes());
                }
            }
            Err(_) => digest.update(b"absent"),
        }
    }
    let digest = digest.finalize();
    Some(
        digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

/// Where Antigravity, kept to file storage, records its sign-in in `home`,
/// its `~/.gemini`. Only the file's metadata is part of the fingerprint;
/// its contents are never read.
pub(super) fn antigravity_sign_in(home: &Path) -> PathBuf {
    home.join("antigravity-cli").join(ANTIGRAVITY_SIGN_IN)
}

/// The name of the file in which Antigravity, kept to file storage, records
/// its sign-in in its data directory: `antigravity-cli`, or whichever one
/// `agy acp --app-data-dir` names. Checked against Antigravity 1.5.18, which
/// reads it from that directory alone.
pub(super) const ANTIGRAVITY_SIGN_IN: &str = "antigravity-oauth-token";

/// Where Muse records its sign-in. Only the file's metadata is part of the
/// fingerprint: cagents never opens it.
pub(super) fn muse_sign_in(
    account: &Account,
    environment: &dyn Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    Some(account.home_directory(environment)?.join("auth.json"))
}

/// Whether an unchanged fingerprint vouches for an answer of `status`.
///
/// Muse's fingerprint covers its sign-in file. A new subscription can lift
/// a limit on its own, and a failed check says nothing, so both are asked again.
pub(super) fn vouches_for(status: UsageStatus) -> bool {
    matches!(
        status,
        UsageStatus::Ok | UsageStatus::Warning | UsageStatus::NeedsLogin
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fingerprint_changes_with_what_the_answer_depends_on() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let environment = |name: &str| match name {
            "HOME" => Some(root.to_string_lossy().into_owned()),
            "XDG_CONFIG_HOME" => Some(root.join("config").to_string_lossy().into_owned()),
            "XDG_DATA_HOME" => Some(root.join("data").to_string_lossy().into_owned()),
            _ => None,
        };
        let program = root.join("muse");
        std::fs::write(&program, b"#!/bin/sh\n").unwrap();
        std::fs::create_dir_all(root.join("config/muse")).unwrap();
        std::fs::create_dir_all(root.join("data/muse/model-catalog")).unwrap();
        let muse = Account::system(Provider::Muse);
        let sign_in = muse_sign_in(&muse, &environment).unwrap();
        let of = || fingerprint(&muse, &program, &environment).unwrap();
        let signed_out = of();
        assert_eq!(of(), signed_out);
        std::fs::write(&sign_in, b"{}").unwrap();
        let signed_in = of();
        assert_ne!(signed_in, signed_out);
        std::fs::write(root.join("data/muse/model-catalog/models-v3.json"), b"{}").unwrap();
        let listed = of();
        assert_ne!(listed, signed_in);
        std::fs::write(&program, b"#!/bin/sh\n# updated\n").unwrap();
        assert_ne!(of(), listed);
        std::fs::remove_file(&sign_in).unwrap();
        assert_ne!(of(), listed);

        assert_eq!(
            fingerprint(&Account::system(Provider::Claude), &program, &environment),
            None
        );
        assert_eq!(
            fingerprint(
                &Account::system(Provider::Antigravity),
                &program,
                &environment
            ),
            None
        );
    }
}
