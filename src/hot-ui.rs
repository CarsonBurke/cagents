//! Development-only root: cargo-hot patches its selected binary, not library
//! targets. Compile the same application modules here and watch their src root.
//! The production target continues to use the ordinary library unchanged.
#[cfg(not(target_os = "linux"))]
compile_error!("The experimental hot-reload runner supports Linux only");

extern crate cagents as app_support;
use app_support::runtime_state;
extern crate self as cagents;
include!("modules.rs");
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
    use std::time::{Duration, Instant};
    let cli = std::env::current_exe()?.with_file_name("cagents");
    let version = Command::new(&cli).arg("--version").output()?;
    if !version.status.success()
        || String::from_utf8(version.stdout)?.trim() != format!("cagents {}", cagents::VERSION)
    {
        return Err("hot bundle CLI does not match this development UI".into());
    }
    eprintln!(
        "Restarting the development daemon stops its terminal/chat jobs and all background subagents."
    );
    if !Command::new(cli)
        .args(["daemon", "stop-for-rebuild", "--including-subagents"])
        .stdout(Stdio::null())
        .status()?
        .success()
    {
        return Err("matched CLI refused the development daemon restart".into());
    }
    let socket = cagents::instance::runtime_directory()?
        .join("cagents-development")
        .join("cagentsd.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.try_exists()? {
        if Instant::now() >= deadline {
            return Err("development daemon did not stop within five seconds".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}
