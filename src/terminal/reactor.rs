//! Fixed I/O resources for terminal holder observers. No blocking callbacks or
//! PTY parser work runs here; adding an idle stream adds a task, not a thread.
use std::sync::OnceLock;

pub(super) fn runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("terminal-io")
                .enable_io()
                .build()
                .map_err(|error| format!("start shared terminal I/O reactor: {error}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}
