fn main() -> std::process::ExitCode {
    // Both of these come first: the daemon is spawned with stdout and stderr
    // on /dev/null, so without a durable sink a panic in the process that owns
    // every user PTY leaves nothing to diagnose, and `RUST_LOG` is inert.
    cagents::instance::install_panic_hook("cagentsd");
    cagents::instance::install_tracing("cagentsd", "cagentsd=info,cagents=info,warn");
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
