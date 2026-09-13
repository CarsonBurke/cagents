mod app;
mod preferences;

use clap::Parser;
use cosmic::{Core, app::cosmic::Cosmic, iced};
use std::path::PathBuf;

#[cfg(feature = "development")]
pub const APPLICATION_ID: &str = "io.github.cagents.ceditor.Development";
#[cfg(not(feature = "development"))]
pub const APPLICATION_ID: &str = "io.github.cagents.ceditor";

/// Native file editor and Git diff viewer. No cagents daemon is started.
#[derive(Parser)]
#[command(
    version,
    about,
    after_help = "Open existing files or folders. With no paths, restore saved tabs or browse the current directory.\nCtrl+T creates an untitled note; Ctrl+O opens a file; Ctrl+Shift+O opens a folder; Ctrl+S saves (or save-as for untitled notes); Ctrl+W closes a tab.\nSettings and language-server configuration are independent of cagents."
)]
struct Arguments {
    /// Existing files or directories to open, each in its own tab
    paths: Vec<PathBuf>,
    /// Print the independent settings file path without opening a window
    #[arg(long)]
    settings_path: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    let product = if cfg!(feature = "development") {
        "ceditor-development"
    } else {
        "ceditor"
    };
    let settings_path = directories::ProjectDirs::from("io", "cagents", product)
        .ok_or("ceditor configuration directory is unavailable")?
        .config_dir()
        .join("settings.toml");
    if arguments.settings_path {
        println!("{}", settings_path.display());
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ceditor=info,cagents=warn,warn".into()),
        )
        .init();
    let flags = app::Flags {
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
