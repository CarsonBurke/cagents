use cosmic::app::Settings;
use tracing_subscriber::EnvFilter;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("cagents=info,warn")),
        )
        .init();

    let settings = Settings::default()
        .size(cosmic::iced::Size::new(1440.0, 900.0))
        .size_limits(
            cosmic::iced::Limits::NONE
                .min_width(900.0)
                .min_height(600.0),
        );
    cosmic::app::run::<cagents::app::Cagents>(settings, ())?;
    Ok(())
}
