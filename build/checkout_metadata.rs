// Shared by the build scripts of every package whose executables state their
// version with `cagents::stamp_version!()`. Each includes this file.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Gives this package's executables the checkout's build metadata, as
/// `CAGENTS_CHECKOUT_METADATA`. `checkout` is the root of the workspace.
fn stamp_checkout_metadata(checkout: &Path) {
    let base = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    if let Some(metadata) = git_metadata(&base, checkout) {
        println!("cargo:rustc-env=CAGENTS_CHECKOUT_METADATA={metadata}");
    }
}

fn git(manifest: &Path, arguments: &[&str]) -> Option<String> {
    // `git status` otherwise refreshes the index it reads. The index is a
    // watched input, so rewriting it would make Cargo rebuild this crate
    // again at its next invocation with no change to the checkout.
    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
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

fn git_metadata(base: &str, checkout: &Path) -> Option<String> {
    // A source export has no repository, and then there is nothing to say.
    let head = git(checkout, &["rev-parse", "--git-path", "HEAD"])?;
    let head = if Path::new(&head).is_absolute() {
        PathBuf::from(head)
    } else {
        checkout.join(head)
    };
    // Rerun when the stamp could change, and not otherwise: each rerun
    // recompiles this crate.
    //
    // The commit moves with the HEAD file itself and this checkout's HEAD
    // reflog, which records every commit, checkout and reset here, including a
    // commit on a packed branch. Without a reflog, watch the branch's loose ref
    // instead. packed-refs is not watched: every worktree shares it, and
    // pruning or deleting any branch rewrites it. A ref move without a reflog
    // entry, such as `update-ref`, leaves a stale short SHA until the next
    // rerun.
    println!("cargo:rerun-if-changed={}", head.display());
    // Cargo treats a watched path that does not exist as always changed, so
    // watch only the files that are there.
    let existing = |path: &str| {
        git(checkout, &["rev-parse", "--git-path", path])
            .map(|path| checkout.join(path))
            .filter(|path| path.exists())
    };
    if let Some(reflog) = existing("logs/HEAD") {
        println!("cargo:rerun-if-changed={}", reflog.display());
    } else if let Some(reference) = git(checkout, &["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = existing(&reference)
    {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let sha = git(checkout, &["rev-parse", "--short", "HEAD"])?;
    let dirty = git(
        checkout,
        &["status", "--porcelain", "--untracked-files=no"],
    )
    .is_some_and(|status| !status.is_empty());
    // A clean checkout becomes dirty by an edit, so watch the sources that
    // compile into these executables; edits elsewhere show at the next rerun.
    // A dirty checkout becomes clean by a commit, which moves HEAD, or by
    // restoring or stashing, which rewrites the index. Watching the index
    // while clean, or the sources while dirty, would only recompile this
    // crate with the same stamp: git rewrites the index whenever it refreshes
    // stale file times, and every edit touches the sources.
    if dirty {
        if let Some(index) = existing("index") {
            println!("cargo:rerun-if-changed={}", index.display());
        }
    } else {
        for source in [
            "Cargo.toml",
            "Cargo.lock",
            "build.rs",
            "rust-toolchain.toml",
            ".cargo/config.toml",
            "src",
            "crates",
            "vendor",
            "assets",
            "packaging",
            "fixtures",
            "tests/fixtures",
        ] {
            let path = checkout.join(source);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
    let release_tag = format!("v{base}");
    let at_release = git(checkout, &["tag", "--points-at", "HEAD"])
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
