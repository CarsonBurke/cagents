use tracing_subscriber::EnvFilter;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("cagentsd=info,cagents=info,warn")),
        )
        .init();
    cagents::host::run()?;
    Ok(())
}
