//! Stamps the executables with the version they can report about themselves.
//!
//! `CARGO_PKG_VERSION` is the product version and nothing more: every local
//! build of `0.1.3` and every packaged `0.1.3` carry the same string, so a bug
//! report could not say which build was running. The packaging step names a
//! local archive after the commit it was built from (`0.1.3+local.3d80e6d`),
//! and that metadata has to reach the binary inside it as well.
//!
//! The version is resolved once here, in this order:
//!
//! 1. `CAGENTS_BUILD_METADATA`, when the packaging script sets it, which is
//!    the metadata it also uses for the archive name.
//! 2. The Git checkout the build runs from, when there is one: no metadata at
//!    all when `HEAD` is exactly the release tag for the manifest version and
//!    the tree is clean, otherwise `local.<short sha>` with `.dirty` appended
//!    for uncommitted changes.
//! 3. Nothing, for a source export with no `.git` - a release build.
//!
//! The self-updater verifies the complete installed version against this
//! stamp, then ignores build metadata when comparing it with stable releases.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=CAGENTS_CHECKOUT_ROOT");
    println!("cargo:rerun-if-env-changed=CAGENTS_BUILD_METADATA");
    let base = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let metadata = match std::env::var("CAGENTS_BUILD_METADATA") {
        Ok(metadata) if !metadata.trim().is_empty() => Some(metadata.trim().to_owned()),
        _ => git_metadata(&base),
    };
    let version = match metadata.filter(|metadata| valid_metadata(metadata)) {
        Some(metadata) => format!("{base}+{metadata}"),
        None => base,
    };
    println!("cargo:rustc-env=CAGENTS_VERSION={version}");
}

/// Semantic-versioning build metadata: dot-separated ASCII alphanumerics and
/// hyphens. Anything else would make `--version` print something the
/// packaging step's own version check rejects.
fn valid_metadata(metadata: &str) -> bool {
    !metadata.is_empty()
        && metadata.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn git(manifest: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(manifest)
        .args(arguments)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.trim().to_owned())
}

fn git_metadata(base: &str) -> Option<String> {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    // A source export has no repository, and then there is nothing to say.
    let head = git(&manifest, &["rev-parse", "--git-path", "HEAD"])?;
    let head = if Path::new(&head).is_absolute() {
        PathBuf::from(head)
    } else {
        manifest.join(head)
    };
    // Rebuild when the checkout moves: the HEAD file itself, and the ref it
    // points at when it is symbolic. A packed ref changes through the HEAD
    // file's own directory less predictably, so the index is watched as well;
    // a commit always rewrites it.
    println!("cargo:rerun-if-changed={}", head.display());
    if let Some(reference) = git(&manifest, &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&manifest, &["rev-parse", "--git-path", &reference]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(index) = git(&manifest, &["rev-parse", "--git-path", "index"]) {
        println!("cargo:rerun-if-changed={index}");
    }
    let sha = git(&manifest, &["rev-parse", "--short", "HEAD"])?;
    let dirty = git(
        &manifest,
        &["status", "--porcelain", "--untracked-files=no"],
    )
    .is_some_and(|status| !status.is_empty());
    let release_tag = format!("v{base}");
    let at_release = git(&manifest, &["tag", "--points-at", "HEAD"])
        .is_some_and(|tags| tags.lines().any(|tag| tag == release_tag));
    if at_release && !dirty {
        return None;
    }
    Some(if dirty {
        format!("local.{sha}.dirty")
    } else {
        format!("local.{sha}")
    })
}
