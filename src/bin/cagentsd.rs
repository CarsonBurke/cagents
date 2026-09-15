fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Both of these come first: the daemon is spawned with stdout and stderr
    // on /dev/null, so without a durable sink a panic in the process that owns
    // every user PTY leaves nothing to diagnose, and `RUST_LOG` is inert.
    cagents::instance::install_panic_hook("cagentsd");
    cagents::instance::install_tracing("cagentsd", "cagentsd=info,cagents=info,warn");
    cagents::host::run()?;
    Ok(())
}
