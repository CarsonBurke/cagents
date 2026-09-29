fn main() -> std::process::ExitCode {
    // SAFETY: first statement of main, before any thread exists. Every
    // terminal and conversation this daemon starts inherits its environment.
    unsafe { cagents::account::detach_inherited_selections() };
    if let Some(result) = cagents::host::run_automation_helper() {
        return match result {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("cagents precheck: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    // Both of these come first: the daemon is spawned with stdout and stderr
    // on /dev/null, so without a durable sink a panic in the process that owns
    // every user PTY leaves nothing to diagnose, and `RUST_LOG` is inert.
    cagents::instance::install_panic_hook("cagentsd");
    cagents::instance::install_tracing("cagentsd", "cagentsd=info,cagents=info,warn");
    tracing::info!(version = cagents::VERSION, "starting cagentsd");
    match cagents::host::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // Returning `Result` from `main` renders this with `Debug`, which
        // wraps the message in its error variant and escapes the quotes
        // around the socket path. Someone running the daemon by hand to find
        // out why it will not start has to read the text.
        Err(error) => {
            eprintln!("cagentsd: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
