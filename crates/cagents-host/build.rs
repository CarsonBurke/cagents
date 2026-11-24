//! Gives `cagentsd` the checkout's build metadata, as the root package's
//! `build.rs` does for the CLI, so the matched set states one version. That
//! script explains where the metadata comes from.

// A runtime env watch reruns the script, but its compiled body must also follow
// the checkout when Cargo shares path-package artifacts across worktrees.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

fn main() {
    println!("cargo:rerun-if-env-changed=CAGENTS_CHECKOUT_ROOT");
    println!("cargo:rerun-if-env-changed=CAGENTS_BUILD_METADATA_SUPPLIED");
    if std::env::var_os("CAGENTS_BUILD_METADATA_SUPPLIED").is_some_and(|value| value == "1") {
        return;
    }
    let manifest = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    stamp_checkout_metadata(&manifest.join("../.."));
}

include!("../../build/checkout_metadata.rs");
