//! A single I/O executor shared by PTY readers and holder observers.
//!
//! Tasks never perform blocking socket operations or channel sends. The emulator
//! remains on its own terminal worker, while idle streams consume no OS thread.
use std::{io, sync::OnceLock};

pub(crate) fn runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("terminal-reactor")
            .enable_io()
            .enable_time()
            .build()
            .map_err(|error| format!("start shared terminal I/O reactor: {error}"))
    }).as_ref().map_err(Clone::clone)
}

pub(crate) fn initialize() -> io::Result<()> {
    runtime().map_err(io::Error::other)?;
    super::worker::initialize_event_dispatch().map_err(io::Error::other)?;
    Ok(())
}
