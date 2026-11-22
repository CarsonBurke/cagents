//! Whether a provider's answer can have changed since its account was last
//! probed.
//!
//! Claude and Codex report usage figures that change as the account is
//! used, so their probes follow the schedule. Antigravity and Muse report
//! none: their answer is whether the sign-in works, the plan and the models,
//! which change when the CLI, its sign-in or its model list does. Each of
//! those leaves a trace on this machine except Antigravity's sign-in, which
//! it keeps in the system keyring. So a probe, which for Antigravity starts
//! a whole `agy acp` server, is spent only when one of them changed, or once
//! [`UNCHANGED_FOR`] has passed for what no file shows: a sign-in or plan
//! changed elsewhere.
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
    let mut paths = vec![program.to_path_buf()];
    match account.provider {
        Provider::Claude | Provider::Codex | Provider::Omp => return None,
        Provider::Antigravity => {
            // Its default model.
            paths.push(
                account
                    .home_directory(environment)?
                    .join("antigravity-cli/settings.json"),
            );
        }
        Provider::Muse => {
            // Its sign-in, then the catalog its models are read from.
            paths.push(account.home_directory(environment)?.join("auth.json"));
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
        }
    }
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

/// Whether an unchanged fingerprint vouches for an answer of `status`.
///
/// A working sign-in stands while nothing it depends on changed. A sign-in
/// that does not work stands only where the fingerprint covers it: Muse's,
/// in its `auth.json`, but not Antigravity's, in the keyring, so a
/// signed-out Antigravity is asked again on schedule, the moment it is most
/// likely to be signed in from outside cagents. A login refused for having
/// no allowance changes with a subscription bought elsewhere, and an answer
/// that could not be had says nothing, so both are asked again.
pub(super) fn vouches_for(provider: Provider, status: UsageStatus) -> bool {
    match status {
        UsageStatus::Ok | UsageStatus::Warning => true,
        UsageStatus::NeedsLogin => provider == Provider::Muse,
        UsageStatus::Limited | UsageStatus::Unavailable => false,
    }
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
        let of = || fingerprint(&muse, &program, &environment).unwrap();
        let signed_out = of();
        assert_eq!(of(), signed_out);
        std::fs::write(root.join("config/muse/auth.json"), b"{}").unwrap();
        let signed_in = of();
        assert_ne!(signed_in, signed_out);
        std::fs::write(root.join("data/muse/model-catalog/models-v3.json"), b"{}").unwrap();
        let listed = of();
        assert_ne!(listed, signed_in);
        std::fs::write(&program, b"#!/bin/sh\n# updated\n").unwrap();
        assert_ne!(of(), listed);
        std::fs::remove_file(root.join("config/muse/auth.json")).unwrap();
        assert_ne!(of(), listed);

        assert_eq!(
            fingerprint(&Account::system(Provider::Claude), &program, &environment),
            None
        );
        let agy = Account::system(Provider::Antigravity);
        let before = fingerprint(&agy, &program, &environment).unwrap();
        std::fs::create_dir_all(root.join(".gemini/antigravity-cli")).unwrap();
        std::fs::write(root.join(".gemini/antigravity-cli/settings.json"), b"{}").unwrap();
        assert_ne!(fingerprint(&agy, &program, &environment).unwrap(), before);
    }

    #[test]
    fn a_signed_out_antigravity_is_asked_again_and_muse_is_not() {
        assert!(vouches_for(Provider::Muse, UsageStatus::NeedsLogin));
        assert!(vouches_for(Provider::Antigravity, UsageStatus::Ok));
        assert!(!vouches_for(Provider::Antigravity, UsageStatus::NeedsLogin));
        // A subscription bought elsewhere leaves no trace here.
        assert!(!vouches_for(Provider::Muse, UsageStatus::Limited));
    }
}
