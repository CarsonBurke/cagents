use cosmic::{Core, app::cosmic::Cosmic, iced};
use tracing_subscriber::EnvFilter;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let previous_hook = std::panic::take_hook();
    let main_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |panic| {
        let thread = std::thread::current();
        let message = panic
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic>");
        let backtrace = std::backtrace::Backtrace::force_capture();
        let _ = cagents::instance::report_ui_panic(
            message,
            thread.name(),
            panic.location(),
            &backtrace,
        );
        if thread.id() == main_thread {
            let _ = cagents::instance::report_ui_startup(&Err(format!(
                "UI initialization failed: {panic}"
            )));
        }
        previous_hook(panic);
    }));
    let result = run();
    if let Err(error) = &result {
        let _ = cagents::instance::report_ui_startup(&Err(error.to_string()));
    } else {
        let _ = cagents::instance::report_ui_startup(&Err(
            "UI exited before opening its window".into()
        ));
    }
    result
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1).peekable();
    if arguments
        .peek()
        .is_some_and(|argument| argument == "--confirm-daemon-stop")
    {
        arguments.next();
        let payload = arguments
            .next()
            .ok_or("missing shutdown confirmation capability")?;
        if payload.len() > 4096 || arguments.next().is_some() {
            return Err("invalid shutdown confirmation capability arguments".into());
        }
        let capability: cagents::terminal::ipc::ShutdownCapability =
            serde_json::from_str(&payload)?;
        return cagents::app::run_daemon_stop_confirmation(capability);
    }
    let mut new_window = false;
    let mut slot = None;
    let mut source = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--new-window" => new_window = true,
            "--window" => slot = Some(arguments.next().ok_or("missing window name")?),
            "--from-window" => source = Some(arguments.next().ok_or("missing source window name")?),
            _ => return Err(format!("unknown UI argument: {argument}").into()),
        }
    }
    if new_window && slot.is_some() {
        return Err("--new-window and --window are mutually exclusive".into());
    }
    let fixture =
        cfg!(feature = "latency-harness") && std::env::var_os("CAGENTS_LATENCY_OUTPUT").is_some();
    let startup = (|| -> Result<_, Box<dyn std::error::Error>> {
        let lease = cagents::instance::WindowLease::acquire(new_window, slot.as_deref())?;
        cagents::session::SessionRuntime::initialize_window(
            source
                .as_deref()
                .unwrap_or(cagents::instance::PRIMARY_WINDOW),
        )?;
        let (activation_tx, activation_rx) = async_channel::unbounded();
        let listener = cagents::instance::UiActivationListener::bind(activation_tx)?;
        if !fixture {
            cagents::terminal::ensure_daemon_ready()?;
        }
        let restored = cagents::app::Cagents::load_startup()?;
        Ok((lease, (listener, activation_rx), restored))
    })();
    let (_application_lease, activation, restored) = startup?;
    let restored = std::cell::RefCell::new(Some(restored));
    let activation = Some(activation);

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("cagents=info,warn")),
        )
        .init();

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
            let startup = restored.borrow_mut().take().expect("UI initialized once");
            let (shell, initialize) = Shell::init((core, (activation_rx.clone(), startup)));
            let opened = open
                .map(cagents::app::Message::StartupWindowOpened)
                .map(cosmic::Action::App);
            (shell, iced::Task::batch([opened, initialize]))
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
