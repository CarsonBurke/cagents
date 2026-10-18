fn main() {
    println!("cargo:rerun-if-env-changed=CAGENTS_CHECKOUT_ROOT");
    build_helpers::cfg_aliases_setup();
}
