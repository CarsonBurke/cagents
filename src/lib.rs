#![cfg_attr(
    test,
    allow(
        clippy::cloned_ref_to_slice_refs,
        reason = "an assertion's expected `[value.clone()]` reads as the value it expects"
    )
)]
// Native hot patches of the UI keep thread-local storage in this unchanged
// library.
#[doc(hidden)]
pub mod runtime_state;
include!("modules.rs");
#[doc(hidden)]
pub mod test_binary;
mod version_stamp;

#[cfg(all(test, any(feature = "host", feature = "ui")))]
mod sqlite_contracts;

/// For the test harness of a binary with no tests of its own. `cargo test --bin
/// NAME -- FILTER` would run nothing and exit 0, which reads as a pass; the
/// product's tests live in the library. An unfiltered harness still runs, since
/// `--all-targets` builds and runs every binary's.
#[macro_export]
macro_rules! refuse_filtered_test_run {
    () => {
        #[cfg(all(test, target_os = "linux"))]
        #[used]
        #[unsafe(link_section = ".init_array")]
        static REFUSE_FILTERED_TEST_RUN: extern "C" fn() = {
            extern "C" fn refuse() {
                $crate::refuse_filtered_test_run(
                    std::env::args_os()
                        .skip(1)
                        .map(|argument| argument.to_string_lossy().into_owned()),
                );
            }
            refuse
        };
    };
}

#[doc(hidden)]
pub fn refuse_filtered_test_run(mut arguments: impl Iterator<Item = String>) {
    while let Some(argument) = arguments.next() {
        if matches!(
            argument.as_str(),
            "--test-threads"
                | "--skip"
                | "--logfile"
                | "--color"
                | "--format"
                | "--shuffle-seed"
                | "-Z"
        ) {
            arguments.next();
        } else if !argument.starts_with('-') {
            eprintln!(
                "error: this binary has no tests, so filter `{argument}` would pass by running none. Use --lib or --test NAME."
            );
            std::process::exit(2);
        }
    }
}

crate::test_binary_setup!();

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod retained_fixture;
#[cfg(all(test, feature = "host", target_os = "linux"))]
#[path = "../tests/support/retained_fixture_tests.rs"]
mod retained_fixture_tests;
