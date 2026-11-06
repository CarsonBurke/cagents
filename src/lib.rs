// Native hot patches keep thread-local storage in this unchanged library.
// Both roots include one module list; application types are never mixed.
#[doc(hidden)]
pub mod runtime_state;
include!("modules.rs");

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

// The binary re-executes as a holder or actor, whose own `.init_array` hook may run
// after these in link order, and whose limit is the one under test.
#[cfg(all(test, target_os = "linux"))]
fn is_reexecution() -> bool {
    std::env::args_os().nth(1).is_some_and(|argument| {
        let argument = argument.to_string_lossy();
        argument.starts_with("--cagents-") || argument.starts_with("--internal-")
    })
}

// A fixture re-runs this binary for one `--exact` test and may check that child's
// output. `scripts/test-lib` also passes `--exact`, so its runs skip the report too.
#[cfg(all(test, target_os = "linux"))]
fn is_fixture_child() -> bool {
    is_reexecution()
        || std::env::args_os()
            .skip(1)
            .any(|argument| argument == "--exact")
}

// Sandboxes such as Codex's can leave an empty .git in writable roots, which then
// fails unrelated fixtures with "enclosing Git checkout" refusals. Name the cause on
// stderr before any test runs, so it is not read as many separate defects.
#[cfg(all(test, target_os = "linux"))]
#[used]
#[unsafe(link_section = ".init_array")]
static REPORT_ENCLOSING_GIT: extern "C" fn() = {
    extern "C" fn report() {
        if is_fixture_child() {
            return;
        }
        let mut found = std::collections::BTreeSet::new();
        for root in [std::env::temp_dir(), "/tmp".into(), "/var/tmp".into()] {
            found.extend(
                root.ancestors()
                    .map(|ancestor| ancestor.join(".git"))
                    .filter(|marker| marker.symlink_metadata().is_ok()),
            );
        }
        for marker in found {
            eprintln!(
                "warning: {} encloses test fixtures, and tests refuse recovery storage beneath it. This environment problem is not a test defect. Set TMPDIR outside it; tests hard-coding /tmp or /var/tmp need it gone.",
                marker.display()
            );
        }
    }
    report
};

// libtest gives no setup hook, and with many test threads the descriptor-heavy
// fixtures exhaust the customary soft limit of 1024. Fixtures that test the limit
// lower it themselves.
#[cfg(all(test, target_os = "linux"))]
#[used]
#[unsafe(link_section = ".init_array")]
static RAISE_TEST_DESCRIPTOR_LIMIT: extern "C" fn() = {
    extern "C" fn raise() {
        if is_reexecution() {
            return;
        }
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: both calls only read or write this live rlimit.
        unsafe {
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
                limit.rlim_cur = limit.rlim_cur.max(limit.rlim_max.min(16 * 1024));
                libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
            }
        }
    }
    raise
};

#[cfg(all(test, feature = "host", target_os = "linux"))]
#[path = "../tests/support/retained_fixture.rs"]
pub(crate) mod retained_fixture;
#[cfg(all(test, feature = "host", target_os = "linux"))]
#[path = "../tests/support/retained_fixture_tests.rs"]
mod retained_fixture_tests;
