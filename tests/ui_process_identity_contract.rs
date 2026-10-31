#![cfg(target_os = "linux")]
//! A starting window refuses to run beside any process still named
//! `cagents-ui`, which is how it recognizes a UI that predates the
//! session-store lease. Helper modes of the current executable never take that
//! lease, so they must rename themselves before dispatch or they block every
//! window for as long as they are open (#2021).

use std::{
    ffi::CString,
    fs,
    io::Write,
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn a_running_helper_mode_is_not_mistaken_for_a_legacy_window() {
    let directory = tempfile::tempdir().unwrap();
    let document = directory.path().join("document.pdf");
    let path = CString::new(document.as_os_str().as_bytes()).unwrap();
    // SAFETY: `path` is a live NUL-terminated string for the whole call.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);

    // The render helper opens the document after dispatching on its mode. A
    // FIFO without a writer holds it there, alive, until this test writes.
    let mut helper = Command::new(env!("CARGO_BIN_EXE_cagents-ui"))
        .args(["--internal-render-page"])
        .arg(&document)
        .args(["0", "original", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Opening the write end without blocking succeeds only once a reader is
    // waiting in open, which proves the helper is past its mode dispatch.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut writer = loop {
        match fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&document)
        {
            Ok(writer) => break writer,
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                assert!(
                    Instant::now() < deadline,
                    "helper never opened the document"
                );
                assert!(helper.try_wait().unwrap().is_none(), "helper exited early");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };

    let name = fs::read_to_string(format!("/proc/{}/comm", helper.id())).unwrap();
    writer.write_all(b"not a pdf").unwrap();
    drop(writer);
    helper.wait().unwrap();
    assert_ne!(name.trim_end(), "cagents-ui");
}
