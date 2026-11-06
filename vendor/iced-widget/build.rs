// Track the build-script compilation as well as its runtime rerun watch.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

fn main() {
    println!("cargo:rerun-if-env-changed=CAGENTS_CHECKOUT_ROOT");
    build_helpers::cfg_aliases_setup();
}
