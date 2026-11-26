//! The `cagents` command line, which these tests drive beside the daemon.
//!
//! It is the shared library's executable, and Cargo names only this package's
//! executables to its tests. The build gate links it beside `cagentsd` before
//! any host test build (scripts/cargo-host-test-helper.py), so even a run
//! narrowed to one of these tests drives a command line built from the same
//! source as the daemon.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

pub fn cagents() -> &'static Path {
    static CLI: OnceLock<PathBuf> = OnceLock::new();
    CLI.get_or_init(|| {
        let cli = Path::new(env!("CARGO_BIN_EXE_cagentsd")).with_file_name("cagents");
        assert!(
            cli.is_file(),
            "{} is missing: run the host lane through the build gate, which builds it beside cagentsd (mise run test:host)",
            cli.display()
        );
        cli
    })
}
