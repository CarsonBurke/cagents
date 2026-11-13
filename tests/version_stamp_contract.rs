//! An executable's version is stamped before the constructors that serve in
//! place of `main`, as the terminal and native holders' do: a holder records
//! the version of the executable it was started from.

use std::sync::OnceLock;

cagents::stamp_version!();

static SEEN_BEFORE_MAIN: OnceLock<&'static str> = OnceLock::new();

extern "C" fn read_version_before_main() {
    let _ = SEEN_BEFORE_MAIN.set(cagents::version());
}

// The holders' constructors are in the unprioritized `.init_array` too.
#[used]
#[unsafe(link_section = ".init_array")]
static READ_VERSION_BEFORE_MAIN: extern "C" fn() = read_version_before_main;

#[test]
fn a_constructor_serving_before_main_sees_the_stamped_version() {
    let seen = *SEEN_BEFORE_MAIN.get().expect("the constructor ran");
    assert!(!seen.ends_with("+unstamped"), "{seen}");
    assert_eq!(seen, cagents::version());
}
