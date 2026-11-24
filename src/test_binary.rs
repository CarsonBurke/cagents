//! Process setup shared by every library test binary in the workspace.
//!
//! libtest gives no setup hook, so each test binary installs these as
//! `.init_array` constructors with [`test_binary_setup!`](crate::test_binary_setup).
//! The constructors are expanded into the test binary itself: a static in
//! this library would not be linked into a binary that never names it.

/// Installs the shared test-process setup. Invoke it once at the root of each
/// crate whose library tests run.
#[doc(hidden)]
#[macro_export]
macro_rules! test_binary_setup {
    () => {
        // Name enclosing Git checkout markers before fixtures refuse to run
        // beneath them, so one fixture-root problem appears as one diagnostic.
        #[cfg(all(test, target_os = "linux"))]
        #[used]
        #[unsafe(link_section = ".init_array")]
        static REPORT_ENCLOSING_GIT: extern "C" fn() = {
            extern "C" fn report() {
                $crate::test_binary::report_enclosing_git();
            }
            report
        };

        // Test-support code paths that differ between a test binary and a
        // daemon built with test support, as integration tests run it, ask.
        #[cfg(all(test, target_os = "linux"))]
        #[used]
        #[unsafe(link_section = ".init_array")]
        static MARK_TEST_BINARY: extern "C" fn() = {
            extern "C" fn mark() {
                $crate::test_binary::mark_test_binary();
            }
            mark
        };

        // With many test threads the descriptor-heavy fixtures exhaust the
        // customary soft limit of 1024. Fixtures that test the limit lower it
        // themselves.
        #[cfg(all(test, target_os = "linux"))]
        #[used]
        #[unsafe(link_section = ".init_array")]
        static RAISE_TEST_DESCRIPTOR_LIMIT: extern "C" fn() = {
            extern "C" fn raise() {
                $crate::test_binary::raise_descriptor_limit();
            }
            raise
        };
    };
}

#[cfg(any(test, feature = "test-support"))]
static TEST_BINARY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
pub fn mark_test_binary() {
    TEST_BINARY.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether this process is a library test binary of the workspace, rather
/// than an executable, including one built with test support, such as the
/// `cagentsd` integration tests run. Shared code that a test binary must not
/// run as the product does, such as writing the person's state, asks this
/// rather than `cfg!(test)`, which the daemon crate's tests do not set here.
pub fn is_test_binary() -> bool {
    #[cfg(any(test, feature = "test-support"))]
    {
        cfg!(test) || TEST_BINARY.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        false
    }
}

// The binary re-executes as a holder or actor, whose own `.init_array` hook may run
// after these in link order, and whose limit is the one under test.
#[cfg(all(any(test, feature = "test-support"), target_os = "linux"))]
fn is_reexecution() -> bool {
    std::env::args_os().nth(1).is_some_and(|argument| {
        let argument = argument.to_string_lossy();
        argument.starts_with("--cagents-") || argument.starts_with("--internal-")
    })
}

// A fixture re-runs this binary for one `--exact` test and may check that child's
// output. `scripts/test-lib` also passes `--exact`, so its runs skip the report too.
#[cfg(all(any(test, feature = "test-support"), target_os = "linux"))]
fn is_fixture_child() -> bool {
    is_reexecution()
        || std::env::args_os()
            .skip(1)
            .any(|argument| argument == "--exact")
}

#[cfg(all(any(test, feature = "test-support"), target_os = "linux"))]
pub fn report_enclosing_git() {
    if is_fixture_child() {
        return;
    }
    let mut found = std::collections::BTreeSet::new();
    for root in [std::env::temp_dir(), "/tmp".into(), "/var/tmp".into()] {
        found.extend(
            root.ancestors()
                .map(|ancestor| ancestor.join(".git"))
                .filter(|marker| crate::git_marker::present(marker).is_ok_and(|present| present)),
        );
    }
    for marker in found {
        eprintln!(
            "warning: {} encloses test fixtures, and tests refuse recovery storage beneath it. This environment problem is not a test defect. Set TMPDIR outside it; tests hard-coding /tmp or /var/tmp need it gone.",
            marker.display()
        );
    }
}

#[cfg(all(any(test, feature = "test-support"), target_os = "linux"))]
pub fn raise_descriptor_limit() {
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
