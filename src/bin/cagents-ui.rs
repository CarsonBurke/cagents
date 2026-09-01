use cosmic::app::Settings;
use tracing_subscriber::EnvFilter;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _application_lease = cagents::instance::ApplicationLease::acquire_for_ui()?;
    let (activation_tx, activation_rx) = async_channel::unbounded();
    let activation = match cagents::instance::UiActivationListener::bind(activation_tx) {
        Ok(listener) => Some((listener, activation_rx)),
        Err(error) => {
            eprintln!("cagents-ui: could not listen for later launches: {error}");
            None
        }
    };

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
    let activation_rx = activation.as_ref().map(|(_, rx)| rx.clone());
    let _activation = activation;
    cosmic::app::run::<cagents::app::Cagents>(settings, activation_rx)?;
    Ok(())
}
