//! Stamp the standalone editor with the same checkout version as its companions.

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
