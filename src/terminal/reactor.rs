//! Fixed I/O resources for terminal holder observers. No blocking callbacks or
//! PTY parser work runs here; adding an idle stream adds a task, not a thread.
use super::service_init::ServiceInit;

pub(super) fn runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RUNTIME: ServiceInit<tokio::runtime::Runtime> = ServiceInit::new();
    RUNTIME.get_or_try_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("terminal-io")
            .enable_io()
            .build()
            .map_err(|error| format!("start shared terminal I/O reactor: {error}"))
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn reactor_can_start_after_file_descriptors_become_available() {
        const CHILD: &str = "CAGENTS_TEST_REACTOR_DESCRIPTOR_LIMIT";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "terminal::reactor::tests::reactor_can_start_after_file_descriptors_become_available",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: the pointers refer to initialized rlimit values. This
        // isolated test process is the only one changing its soft limit.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        let unavailable = libc::rlimit {
            rlim_cur: 0,
            rlim_max: limit.rlim_max,
        };
        assert_eq!(
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &unavailable) },
            0
        );
        let first = runtime();
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);

        assert!(first.is_err());
        let recovered = runtime()
            .expect("the terminal reactor can retry after the descriptor limit is restored");
        assert!(std::ptr::eq(recovered, runtime().unwrap()));
    }
}
