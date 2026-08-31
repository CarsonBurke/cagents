use cosmic::app::Settings;
use tracing_subscriber::EnvFilter;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _application_lease = cagents::instance::ApplicationLease::acquire_for_ui()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("cagents=info,warn")),
        )
        .init();
    cagents::terminal::ensure_daemon_ready()?;

    let settings = Settings::default()
        .exit_on_close(false)
        .size(cosmic::iced::Size::new(1440.0, 900.0))
        .size_limits(
            cosmic::iced::Limits::NONE
                .min_width(900.0)
                .min_height(600.0),
        );
    cosmic::app::run::<cagents::app::Cagents>(settings, ())?;
    Ok(())
}
