//! Finds the build metadata the checkout gives the executables.
//!
//! `CARGO_PKG_VERSION` is the product version and nothing more: every local
//! build of `0.1.3` and every packaged `0.1.3` carry the same string, so a bug
//! report could not say which build was running. The packaging step names a
//! local archive after the commit it was built from (`0.1.3+local.3d80e6d`),
//! and that metadata has to reach the binary inside it as well.
//!
//! An executable states its version with `cagents::stamp_version!()`, which
//! reads, in this order:
//!
//! 1. `CAGENTS_BUILD_METADATA`, when the packaging script or the build gate
//!    sets it, which is the metadata the packaging step also uses for the
//!    archive name.
//! 2. `CAGENTS_CHECKOUT_METADATA`, which this script sets from the Git
//!    checkout the build runs from, when there is one: nothing when `HEAD` is
//!    exactly the release tag for the manifest version and the tree is clean,
//!    otherwise `local.<short sha>` with `.dirty` appended for uncommitted
//!    changes.
//! 3. Nothing, for a source export with no `.git` - a release build.
//!
//! Only the executables read either variable. A build script's rerun, or a
//! change to a variable it watches, recompiles every target of the package,
//! including the whole library, so this script never reads
//! `CAGENTS_BUILD_METADATA`: Cargo tracks it in the dep-info of each executable
//! that reads it, and a new value relinks only those. A driver that supplies
//! the metadata for every build also sets `CAGENTS_BUILD_METADATA_SUPPLIED=1`,
//! whose value does not change between commits, and then this script does not
//! consult the checkout at all. Otherwise its watches below rerun it, and
//! recompile the library, when the checkout's stamp could change.
//!
//! The self-updater verifies the complete installed version against this
//! stamp, then ignores build metadata when comparing it with stable releases.

// A runtime env watch reruns the script, but its compiled body must also follow
// the checkout when Cargo shares path-package artifacts across worktrees.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

fn main() {
    println!("cargo:rerun-if-env-changed=CAGENTS_CHECKOUT_ROOT");
    println!("cargo:rerun-if-env-changed=CAGENTS_BUILD_METADATA_SUPPLIED");
    if std::env::var_os("CAGENTS_BUILD_METADATA_SUPPLIED").is_some_and(|value| value == "1") {
        return;
    }
    let checkout = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    stamp_checkout_metadata(&checkout);
}

include!("build/checkout_metadata.rs");
