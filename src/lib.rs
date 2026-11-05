// Native hot patches keep thread-local storage in this unchanged library.
// Both roots include one module list; application types are never mixed.
#[doc(hidden)]
pub mod runtime_state;
include!("modules.rs");

#[cfg(all(test, any(feature = "host", feature = "ui")))]
mod sqlite_contracts;

#[cfg(feature = "host")]
pub(crate) mod transcript_cache;

// Sandboxes such as Codex's can leave an empty .git in writable roots, which then
// fails unrelated fixtures with "enclosing Git checkout" refusals. Name the cause on
// stderr before any test runs, so it is not read as many separate defects.
#[cfg(all(test, target_os = "linux"))]
#[used]
#[unsafe(link_section = ".init_array")]
static REPORT_ENCLOSING_GIT: extern "C" fn() = {
    extern "C" fn report() {
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
