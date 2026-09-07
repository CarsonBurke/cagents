use cosmic::{Core, app::cosmic::Cosmic, iced};
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

    // The pinned COSMIC run helper does not wire its scale_factor callback
    // into iced. Build the daemon through the public shell API so interface
    // scaling changes the actual viewport, not only Core's preference.
    type Shell = Cosmic<cagents::app::Cagents>;
    cosmic::icon_theme::set_default(cosmic::config::icon_theme());
    let settings = iced::Settings {
        id: Some(cagents::instance::APPLICATION_ID.to_owned()),
        default_font: cosmic::font::default(),
        default_text_size: iced::Pixels(14.0),
        antialiasing: true,
        is_daemon: false,
        ..iced::Settings::default()
    };
    let activation_rx = activation.as_ref().map(|(_, rx)| rx.clone());
    let _activation = activation;
    iced::daemon(
        move || {
            let mut window_settings = iced::window::Settings {
                size: iced::Size::new(1440.0, 900.0),
                min_size: Some(iced::Size::new(900.0, 600.0)),
                decorations: false,
                transparent: true,
                resize_border: 8,
                exit_on_close_request: false,
                ..iced::window::Settings::default()
            };
            window_settings.platform_specific.application_id =
                cagents::instance::APPLICATION_ID.to_owned();
            let (window, open) = iced::window::open(window_settings);
            let mut core = Core::default();
            core.set_main_window_id(Some(window));
            let (shell, initialize) = Shell::init((core, activation_rx.clone()));
            (shell, iced::Task::batch([open.discard(), initialize]))
        },
        Shell::update,
        Shell::view,
    )
    .subscription(Shell::subscription)
    .title(Shell::title)
    .style(Shell::style)
    .theme(Shell::theme)
    .scale_factor(Shell::scale_factor)
    .settings(settings)
    .run()?;
    Ok(())
}
