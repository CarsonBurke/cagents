use std::io::IsTerminal;
use std::os::unix::process::CommandExt;

use cagents::design::space;
use cosmic::{Core, app::cosmic::Cosmic, iced};

const PRODUCT: &str = "cagents-ui";
const DEFAULT_LOG_FILTER: &str = "cagents=info,warn";
use cagents::instance::{
    CONFIRM_DAEMON_STOP_MODE as CONFIRM_DAEMON_STOP, STARTUP_ERROR_MODE as STARTUP_ERROR,
    STARTUP_UPDATE_MODE as STARTUP_UPDATE,
};
/// argv has a hard size limit and a dialog cannot show a novel either.
const MAX_REPORTED_ERROR_BYTES: usize = 2048;
/// The internal `--confirm-daemon-stop`, `--startup-error`, `--startup-update` and
/// `--internal-render-page` modes are deliberately absent: cagents spawns
/// itself with those, and documenting them would invite a user to hand-craft
/// a capability payload.
const USAGE: &str = "\
Usage: cagents-ui [OPTIONS]

Open the cagents workspace window. Prefer `cagents ui`, which starts the
daemon first and reuses an already-running window.

Options:
  --device NAME             Connect to a configured backend device (default local)
  --new-window              Open an additional window instead of reusing one
  --window NAME             Open or focus the window saved under NAME
  --from-window NAME        Adopt the session state of the window named NAME
  -V, --version             Show the compiled version
  -h, --help                Show this help
";

pub(crate) fn main() -> std::process::ExitCode {
    main_with_before_start(|| Ok(()))
}

/// Development hosts may prepare a replacement after argv and helper modes
/// settle. Production startup delegates a no-op and keeps the same behavior.
pub(crate) fn main_with_before_start(
    before_start: impl FnOnce() -> Result<(), Box<dyn std::error::Error>>,
) -> std::process::ExitCode {
    // SAFETY: first statement of main, before any thread exists.
    unsafe { cagents::account::detach_inherited_selections() };
    // Before any mode dispatch. A starting window refuses beside any process
    // still named `cagents-ui`. The helper modes never take the window lease,
    // so they would not otherwise rename themselves, and a window refused at
    // an update fence keeps the old name until it exits. Best effort: a
    // window repeats this when it takes the lease and reports failure there.
    let _ = cagents::instance::mark_lease_capable_process();
    // The document render helper answers before anything else exists. Its
    // stdout is a binary page frame, so a tracing subscriber, a panic hook
    // writing a crash record, the window lease or the launcher handshake
    // putting a single line there would corrupt the page the reader is
    // waiting for. It is also the one mode that must stay cheap: it runs
    // once per page turn.
    if let Some(code) = cagents::document_render::hook() {
        return code;
    }
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    // The standalone dialogs write their result to stdout and the tray shows
    // that text verbatim. They inherit the launcher handshake variable from
    // the daemon that spawned them, so the startup protocol must stay off
    // here: otherwise a "ready"/"error: ..." line lands inside the status the
    // user reads as the outcome of their shutdown request.
    let standalone = arguments.first().is_some_and(|argument| {
        argument == CONFIRM_DAEMON_STOP || argument == STARTUP_ERROR || argument == STARTUP_UPDATE
    });
    if standalone {
        cagents::instance::install_panic_hook(PRODUCT);
        cagents::instance::install_tracing(PRODUCT, DEFAULT_LOG_FILTER);
        return report(run_internal(&arguments));
    }

    // Argv is settled before the handshake below is armed. A mistyped flag is
    // a shell mistake, not a startup failure: routing it through
    // `report_startup_failure` answered the launcher with an error and, on a
    // redirected stderr, opened a detached dialog for a typo.
    let options = match parse(&arguments) {
        Ok(Some(options)) => options,
        Ok(None) => return std::process::ExitCode::SUCCESS,
        Err(error) => return report(Err(error)),
    };
    if let Err(error) = before_start() {
        return report(Err(error));
    }

    // Diagnostics come before every other statement. The startup sequence
    // below - window lease, session store, activation socket, daemon launch,
    // checkpoint restore - is exactly the region whose failures used to be
    // unobservable, because the subscriber was installed after it.
    cagents::instance::install_panic_hook(PRODUCT);
    cagents::instance::install_tracing(PRODUCT, DEFAULT_LOG_FILTER);
    tracing::info!(version = cagents::VERSION, "starting cagents-ui");

    let main_thread = std::thread::current().id();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        // The launcher is blocked reading the startup handshake; a main-thread
        // panic has to release it before this process dies. The crash record
        // itself is written by the hook installed above, which runs next.
        if std::thread::current().id() == main_thread {
            let _ = cagents::instance::report_ui_startup(&Err(format!(
                "UI initialization failed: {panic}"
            )));
        }
        previous_hook(panic);
    }));

    #[cfg(feature = "integrated-browser")]
    let _browser_cleanup = cagents::browser::ProcessShutdown;
    let result = run(options);
    match &result {
        Err(error) if cagents::instance::is_update_in_progress(error.as_ref()) => {
            // Expected coordinator ownership is not a crash. Preserve the
            // original public launch context in a capability-free payload.
            if let Ok(Some(options)) = parse(&arguments) {
                let request = cagents::update::startup::Request::new(
                    options.new_window,
                    options.slot.map(str::to_owned),
                    options.source.map(str::to_owned),
                    options.device.map(str::to_owned),
                    options.device_window_scoped,
                );
                match request {
                    Ok(request) if spawn_startup_update_dialog(&request).is_ok() => (),
                    _ => tracing::error!("could not open the update progress window"),
                }
            }
            let _ = cagents::instance::report_ui_startup(&Err("an update is in progress".into()));
        }
        Err(error) => report_startup_failure(&error.to_string()),
        // The window reports readiness from its first frame, so reaching here
        // with the handshake unclaimed means no window ever opened.
        Ok(()) => {
            let _ = cagents::instance::report_ui_startup(&Err(
                "UI exited before opening its window".into(),
            ));
        }
    }
    report(result)
}

/// Returning `Result` from `main` renders the error with `Debug`, which quotes
/// the message and escapes its newlines; usage text came out as one line of
/// literal `\n`. Every failure here is a message meant for a person to read.
fn report(result: Result<(), Box<dyn std::error::Error>>) -> std::process::ExitCode {
    cagents::diagnostics::record(cagents::diagnostics::Event::Exit {
        success: result.is_ok(),
    });
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{PRODUCT}: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Makes a startup failure observable. Panics were already durable; an
/// ordinary `Err` was reported only down a pipe whose reader prints it into a
/// stderr the desktop session discards.
fn report_startup_failure(message: &str) {
    let message = truncated(message);
    tracing::error!(error = message, "cagents-ui startup failed");
    cagents::instance::report_startup_failure();
    // "Already running" is not a failure the user must be shown: the launcher
    // resolves it by activating the window that already exists.
    let already_running =
        message == cagents::instance::ApplicationLeaseError::AlreadyRunning.to_string();
    // A terminal run already prints the error. A desktop launch has none, so
    // without this the user double-clicks the icon and nothing happens.
    if !already_running && !std::io::stderr().is_terminal() {
        let _ = spawn_startup_error_dialog(message);
    }
    // Reported last: the launcher kills this process as soon as it reads a
    // handshake error, so the dialog has to be detached before then.
    let _ = cagents::instance::report_ui_startup(&Err(message.to_owned()));
}

fn truncated(message: &str) -> &str {
    match message.char_indices().nth(MAX_REPORTED_ERROR_BYTES) {
        Some((end, _)) => &message[..end],
        None => message,
    }
}

/// Re-runs this executable as a detached dialog. The failing process cannot
/// show the dialog itself: it must answer the launcher's handshake, and the
/// launcher kills it the moment that answer is an error.
fn spawn_startup_error_dialog(message: &str) -> std::io::Result<()> {
    std::process::Command::new(std::env::current_exe()?)
        .arg(STARTUP_ERROR)
        .arg(message)
        .env_remove(cagents::instance::UI_LAUNCH_HANDSHAKE_ENV)
        .env_remove(cagents::instance::UI_LAUNCH_RECOVERY_PROGRESS_ENV)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_| ())
}

fn spawn_startup_update_dialog(request: &cagents::update::startup::Request) -> std::io::Result<()> {
    let payload = serde_json::to_string(request).map_err(std::io::Error::other)?;
    std::process::Command::new(std::env::current_exe()?)
        .args([STARTUP_UPDATE, &payload])
        .env_remove(cagents::instance::UI_LAUNCH_HANDSHAKE_ENV)
        .env_remove(cagents::instance::UI_LAUNCH_RECOVERY_PROGRESS_ENV)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_| ())
}

/// The standalone modes cagents spawns for itself. They must not touch the
/// window lease, the session store, or the launcher handshake.
fn run_internal(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = arguments.iter().map(String::as_str).peekable();
    if arguments.peek() == Some(&STARTUP_UPDATE) {
        arguments.next();
        let payload = arguments.next().ok_or("missing startup update context")?;
        if payload.len() > 4096 || arguments.next().is_some() {
            return Err("invalid startup update context".into());
        }
        let request: cagents::update::startup::Request = serde_json::from_str(payload)?;
        return cagents::update::startup::run(request);
    }
    if arguments.peek() == Some(&CONFIRM_DAEMON_STOP) {
        arguments.next();
        let payload = arguments
            .next()
            .ok_or("missing shutdown confirmation capability")?;
        if payload.len() > 4096 || arguments.next().is_some() {
            return Err("invalid shutdown confirmation capability arguments".into());
        }
        let capability: cagents::terminal::ipc::ShutdownCapability = serde_json::from_str(payload)?;
        return cagents::app::run_daemon_stop_confirmation(capability);
    }
    arguments.next();
    let message = arguments.next().ok_or("missing startup error message")?;
    if arguments.next().is_some() {
        return Err("invalid startup error arguments".into());
    }
    run_startup_error_dialog(message)
}

struct Options<'a> {
    new_window: bool,
    slot: Option<&'a str>,
    source: Option<&'a str>,
    device: Option<&'a str>,
    device_window_scoped: bool,
}

/// `Ok(None)` means the request was answered without opening a window, which
/// informational flags do.
fn parse<'a>(arguments: &'a [String]) -> Result<Option<Options<'a>>, Box<dyn std::error::Error>> {
    let mut arguments = arguments.iter().map(String::as_str);
    let mut options = Options {
        new_window: false,
        slot: None,
        source: None,
        device: None,
        device_window_scoped: false,
    };
    while let Some(argument) = arguments.next() {
        match argument {
            "--new-window" => options.new_window = true,
            "--device" => options.device = Some(arguments.next().ok_or("missing device name")?),
            "--device-window-scoped" => options.device_window_scoped = true,
            "--window" => options.slot = Some(arguments.next().ok_or("missing window name")?),
            "--from-window" => {
                options.source = Some(arguments.next().ok_or("missing source window name")?);
            }
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "--version" | "-V" => {
                println!("{PRODUCT} {}", cagents::VERSION);
                return Ok(None);
            }
            // Naming the accepted arguments matters more here than for most
            // binaries: this one is normally launched by `cagents ui`, so
            // anyone typing it directly has no usage text to fall back on.
            _ => return Err(format!("unknown UI argument: {argument}\n\n{USAGE}").into()),
        }
    }
    if !options.device_window_scoped && options.slot.is_some_and(|slot| slot.starts_with("device-"))
    {
        return Err("window names beginning device- are reserved for device windows".into());
    }
    if options.new_window && options.slot.is_some() {
        return Err("--new-window and --window are mutually exclusive".into());
    }
    Ok(Some(options))
}

fn run(options: Options<'_>) -> Result<(), Box<dyn std::error::Error>> {
    let Options {
        new_window,
        slot,
        source,
        device,
        device_window_scoped,
    } = options;
    let _device_cleanup = cagents::device::Cleanup;
    cagents::device::select(device)?;
    cagents::device::prepare()?;
    if !cagents::device::is_remote() {
        if let Some(executable) = cagents::control::staged_ui_executable()? {
            // exec preserves the launcher handshake pipe and window arguments.
            let mut command = std::process::Command::new(executable);
            command.args(std::env::args_os().skip(1));
            return Err(command.exec().into());
        }
    }
    let unique_slot = uuid::Uuid::new_v4().to_string();
    let remote_new_window = new_window && cagents::device::is_remote();
    let new_window = new_window && !remote_new_window;
    let slot = if remote_new_window {
        Some(unique_slot.as_str())
    } else {
        slot
    };
    let scoped_slot = if device_window_scoped && !remote_new_window {
        slot.map(str::to_owned)
    } else {
        cagents::device::window_slot(slot)?
    };
    let slot = scoped_slot.as_deref();
    let fixture =
        cfg!(feature = "latency-harness") && std::env::var_os("CAGENTS_LATENCY_OUTPUT").is_some();
    let startup = (|| -> Result<_, Box<dyn std::error::Error>> {
        let lease = cagents::instance::WindowLease::acquire(new_window, slot)?;
        let device_source = if device_window_scoped && source.is_some() {
            source.map(str::to_owned)
        } else {
            cagents::device::window_slot(source)?
        };
        cagents::session::SessionRuntime::initialize_window(
            device_source
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
                // The startup task runs alongside window creation. Supply the
                // icon here so no update can be dropped before the window opens.
                icon: Some(cagents::app::application_window_icon()),
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

/// Minimal standalone shell that tells the user why nothing opened. It runs in
/// its own detached process, so it must not touch the session store or the
/// window lease the failed launch was contending for.
///
/// It can also try again. Several of the failures that reach it are
/// transient - a daemon that lost a startup race, a socket directory that was
/// momentarily unavailable, a foreign process that has since gone - and an
/// installer that has just replaced the running window with this dialog
/// leaves the user no cagents window at all once it closes. Retry re-runs the
/// launcher, which is the whole remedy for a transient failure.
struct StartupError {
    core: Core,
    message: String,
    /// Why the last retry could not even start the launcher, shown in place
    /// of a dialog that silently did nothing.
    retry_failure: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum StartupErrorMessage {
    Retry,
    Close,
}

impl cosmic::Application for StartupError {
    type Executor = cosmic::executor::Default;
    type Flags = String;
    type Message = StartupErrorMessage;
    const APP_ID: &'static str = "dev.cagents.Cagents.StartupError";

    fn init(core: Core, message: String) -> (Self, cosmic::app::Task<StartupErrorMessage>) {
        (
            Self {
                core,
                message,
                retry_failure: None,
            },
            cosmic::app::Task::none(),
        )
    }

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn on_close_requested(&self, _: iced::window::Id) -> Option<StartupErrorMessage> {
        Some(StartupErrorMessage::Close)
    }

    fn update(&mut self, message: StartupErrorMessage) -> cosmic::app::Task<StartupErrorMessage> {
        match message {
            StartupErrorMessage::Close => iced::exit().map(cosmic::Action::App),
            StartupErrorMessage::Retry => match relaunch() {
                Ok(()) => iced::exit().map(cosmic::Action::App),
                Err(error) => {
                    self.retry_failure = Some(error);
                    cosmic::app::Task::none()
                }
            },
        }
    }

    fn view(&self) -> cosmic::Element<'_, StartupErrorMessage> {
        let mut rows = vec![
            cagents::design::title("cagents could not start").into(),
            cagents::design::body(&self.message).into(),
            cagents::design::body(
                "This may be temporary. Retry starts cagents again; Close leaves no window open.",
            )
            .into(),
            cagents::design::body(
                "Run `cagents diagnostics` to inspect retained private startup evidence.",
            )
            .into(),
        ];
        if let Some(failure) = &self.retry_failure {
            rows.push(cagents::design::body(format!("Retry failed: {failure}")).into());
        }
        rows.push(
            cosmic::widget::row::with_children(vec![
                cagents::ui_controls::button::primary("Retry")
                    .on_press(StartupErrorMessage::Retry)
                    .into(),
                cagents::ui_controls::button::secondary("Close")
                    .on_press(StartupErrorMessage::Close)
                    .into(),
            ])
            .spacing(space::S)
            .into(),
        );
        cosmic::widget::container(cosmic::widget::column::with_children(rows).spacing(space::L))
            .padding(space::XL)
            .width(iced::Length::Fill)
            .height(iced::Length::Fill)
            .into()
    }
}

/// Starts the launcher that produced this dialog, detached from it.
///
/// The launcher is the `cagents` executable installed beside this one: it is
/// what the desktop entry runs, it starts the daemon before the window, and
/// it reuses a window that did manage to open in the meantime instead of
/// opening a second one. Running `cagents-ui` directly again would skip all
/// three.
fn relaunch() -> Result<(), String> {
    let launcher = cagents::companion::Product::Cli
        .beside_current()
        .map_err(|error| error.to_string())?;
    if !launcher.is_file() {
        return Err(format!(
            "the cagents launcher was not found beside this executable at {}",
            launcher.display()
        ));
    }
    std::process::Command::new(launcher)
        .arg("ui")
        .env_remove(cagents::instance::UI_LAUNCH_HANDSHAKE_ENV)
        .env_remove(cagents::instance::UI_LAUNCH_RECOVERY_PROGRESS_ENV)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn run_startup_error_dialog(message: &str) -> Result<(), Box<dyn std::error::Error>> {
    type Shell = Cosmic<StartupError>;
    let message = message.to_owned();
    cosmic::icon_theme::set_default(cosmic::config::icon_theme());
    iced::daemon(
        move || {
            let (window, open) = iced::window::open(iced::window::Settings {
                size: iced::Size::new(620.0, 280.0),
                exit_on_close_request: false,
                // The startup task runs alongside window creation. Supply the
                // icon here so no update can be dropped before the window opens.
                icon: Some(cagents::app::application_window_icon()),
                ..iced::window::Settings::default()
            });
            let mut core = Core::default();
            core.set_main_window_id(Some(window));
            let (shell, task) = Shell::init((core, message.clone()));
            (shell, iced::Task::batch([open.discard(), task]))
        },
        Shell::update,
        Shell::view,
    )
    .subscription(Shell::subscription)
    .title(|_: &Shell, _| "cagents".to_owned())
    .style(Shell::style)
    .theme(Shell::theme)
    .settings(iced::Settings {
        is_daemon: false,
        ..iced::Settings::default()
    })
    .run()?;
    Ok(())
}
