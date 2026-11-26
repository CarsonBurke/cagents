//! Development-only root: cargo-hot patches its selected binary, not library
//! targets. Compile the same application modules here and watch their src root.
//! The production target continues to use the ordinary library unchanged, and
//! the shared `cagents` library is never patched.
#[cfg(not(target_os = "linux"))]
compile_error!("The experimental hot-reload runner supports Linux only");

// The entry point names the application modules through the UI library; here
// they are this root's own.
extern crate self as cagents_ui;
include!("modules.rs");
// The production entry point also states this executable's version.
#[path = "bin/cagents-ui.rs"]
mod ui_entry;

const FRESH_ENV: &str = "CAGENTS_HOT_FRESH_DAEMON";

fn main() -> std::process::ExitCode {
    // SAFETY: before starting any thread/child; never forward restart intent to
    // dialogs, new windows, the daemon or other descendant processes.
    let fresh = unsafe {
        cagents::account::detach_inherited_selections();
        let requested = std::env::var_os(FRESH_ENV).is_some_and(|value| value == "1");
        std::env::remove_var(FRESH_ENV);
        requested
    };
    // Shared parsing/info/internal/render paths return before this callback.
    // All valid UI arguments keep the ordinary runner's restart semantics.
    ui_entry::main_with_before_start(move || {
        if fresh {
            restart_matched_daemon()
        } else {
            Ok(())
        }
    })
}

fn restart_matched_daemon() -> Result<(), Box<dyn std::error::Error>> {
    use std::process::{Command, Stdio};
    let cli = cagents::process::current_executable()?.with_file_name("cagents");
    let version = Command::new(&cli).arg("--version").output()?;
    if !version.status.success()
        || String::from_utf8(version.stdout)?.trim() != format!("cagents {}", cagents::version())
    {
        return Err("hot bundle CLI does not match this development UI".into());
    }
    eprintln!(
        "Restarting the development daemon stops its terminal/chat jobs and all background subagents."
    );
    // The matched CLI returns only once the daemon has exited.
    if !Command::new(cli)
        .args(["daemon", "stop-for-rebuild", "--including-subagents"])
        .stdout(Stdio::null())
        .status()?
        .success()
    {
        return Err("matched CLI could not stop the development daemon".into());
    }
    Ok(())
}
