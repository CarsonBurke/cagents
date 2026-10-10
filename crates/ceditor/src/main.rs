// Keep this owned workspace binary fresh when switching a shared Cargo cache
// between worktrees, without invalidating third-party dependencies.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

mod app;
mod preferences;

use clap::Parser;
use cosmic::{Core, app::cosmic::Cosmic, iced};
use std::path::PathBuf;

#[cfg(feature = "development")]
pub const APPLICATION_ID: &str = "io.github.cagents.ceditor.Development";
#[cfg(not(feature = "development"))]
pub const APPLICATION_ID: &str = "io.github.cagents.ceditor";

#[cfg(feature = "development")]
const PRODUCT: &str = "ceditor-development";
#[cfg(not(feature = "development"))]
const PRODUCT: &str = "ceditor";
/// `ceditor` emits no events of its own; every diagnostic it has comes from
/// the shared library, so that target is the one that has to be enabled. The
/// UI binary uses the same default.
const DEFAULT_LOG_FILTER: &str = "cagents=info,warn";

/// Native file editor and Git diff viewer with optional shared review alerts.
#[derive(Parser)]
#[command(
    version,
    about,
    after_help = "Open existing files or folders. With no paths, restore saved tabs or browse the current directory.\nCtrl+T creates an untitled note; Ctrl+O opens a file; Ctrl+Shift+O opens a folder; Ctrl+S saves (or save-as for untitled notes); Ctrl+W closes a tab.\nCtrl+F finds in the open file; Ctrl+P opens the quick-open palette; Ctrl+Space requests language-server completions.\nSettings and language-server configuration are independent of cagents."
)]
struct Arguments {
    /// Existing files or directories to open, each in its own tab
    paths: Vec<PathBuf>,
    /// Print the independent settings file path without opening a window
    #[arg(long)]
    settings_path: bool,
    #[arg(long, hide = true)]
    notification_window: Option<String>,
}

fn main() -> std::process::ExitCode {
    // `ceditor` previews documents through the same helper, and the helper is
    // whichever executable is running, so this binary answers the internal
    // `--internal-render-page` mode too. It comes before diagnostics because
    // the helper's stdout carries a binary page frame that any log line would
    // corrupt. As in cagents-ui, the mode stays out of the usage text: it is
    // a calling convention between two copies of one binary, not a feature.
    if let Some(code) = cagents::document_render::hook() {
        return code;
    }
    let arguments = Arguments::parse();
    if arguments.settings_path {
        return match settings_path() {
            Ok(path) => {
                println!("{}", path.display());
                std::process::ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("{PRODUCT}: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    // Diagnostics precede ordinary UI startup: a panic in a render or
    // worker thread used to take the window with it and leave nothing behind
    // in ceditor's data directory.
    cagents::instance::install_panic_hook_with_version(PRODUCT, env!("CARGO_PKG_VERSION"));
    cagents::instance::install_tracing(PRODUCT, DEFAULT_LOG_FILTER);
    let result = run(arguments);
    if result.is_err() {
        cagents::instance::report_startup_failure();
    }
    cagents::diagnostics::record(cagents::diagnostics::Event::Exit {
        success: result.is_ok(),
    });
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // Returning the `Result` from `main` instead would render the
            // error with `Debug`, which quotes the message and escapes its
            // newlines; every failure here is meant for a person to read.
            eprintln!("{PRODUCT}: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn settings_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(directories::ProjectDirs::from("io", "cagents", PRODUCT)
        .ok_or("ceditor configuration directory is unavailable")?
        .config_dir()
        .join("settings.toml"))
}

fn run(arguments: Arguments) -> Result<(), Box<dyn std::error::Error>> {
    let settings_path = settings_path()?;
    cagents::worktree_cleanup::prepare_standalone_editor_protection();
    let flags = app::Flags {
        notification_window: arguments.notification_window,
        paths: arguments.paths,
        settings_path,
    };
    type Shell = Cosmic<app::Editor>;
    cosmic::icon_theme::set_default(cosmic::config::icon_theme());
    iced::daemon(
        move || {
            let mut window_settings = iced::window::Settings {
                size: iced::Size::new(1280.0, 860.0),
                min_size: Some(iced::Size::new(720.0, 480.0)),
                decorations: false,
                transparent: true,
                resize_border: 8,
                exit_on_close_request: false,
                ..Default::default()
            };
            window_settings.platform_specific.application_id = APPLICATION_ID.to_owned();
            let (window, open) = iced::window::open(window_settings);
            let mut core = Core::default();
            core.set_main_window_id(Some(window));
            let (shell, initialize) = Shell::init((core, flags.clone()));
            (
                shell,
                iced::Task::batch([
                    open.map(|_| cosmic::Action::App(app::Message::StartupWindowOpened)),
                    initialize,
                ]),
            )
        },
        Shell::update,
        Shell::view,
    )
    .subscription(Shell::subscription)
    .title(Shell::title)
    .style(Shell::style)
    .theme(Shell::theme)
    .scale_factor(Shell::scale_factor)
    .settings(iced::Settings {
        id: Some(APPLICATION_ID.to_owned()),
        default_font: cosmic::font::default(),
        default_text_size: iced::Pixels(14.0),
        antialiasing: true,
        is_daemon: false,
        ..Default::default()
    })
    .run()?;
    Ok(())
}
