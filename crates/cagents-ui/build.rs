//! Gives the UI executables the checkout's build metadata, as the root
//! package's `build.rs` does for the CLI and the daemon, so the matched set
//! states one version. That script explains where the metadata comes from.

// A runtime env watch reruns the script, but its compiled body must also follow
// the checkout when Cargo shares path-package artifacts across worktrees.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

fn main() {
    // The browser bundles HarfBuzz while system FreeType loads system HarfBuzz.
    // Exporting either bundled font API lets the dynamic loader mix their state.
    // Localize only these definitions in the final Linux browser UI; imported
    // system functions and unrelated runtime exports retain their usual binding.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && std::env::var_os("CARGO_FEATURE_INTEGRATED_BROWSER").is_some()
    {
        let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
        let map = output.join("linux-ui-font-symbols.map");
        std::fs::write(&map, "{ local: hb_*; FT_*; };\n")
            .expect("write Linux browser UI font symbol map");
        // Forward the complete path as one linker argument, including commas.
        // The compiler driver splits -Wl arguments at commas in filesystem paths.
        println!("cargo:rustc-link-arg-bin=cagents-ui=-Xlinker");
        println!("cargo:rustc-link-arg-bin=cagents-ui=--version-script");
        println!("cargo:rustc-link-arg-bin=cagents-ui=-Xlinker");
        println!("cargo:rustc-link-arg-bin=cagents-ui={}", map.display());
    }
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
