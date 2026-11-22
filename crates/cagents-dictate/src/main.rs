//! `cagents-dictate`: local speech recognition for cagents.

use std::{path::PathBuf, process::ExitCode};

use cagents_dictate::{
    catalog,
    engine::{
        install::{self, Https},
        serve, transcribe,
    },
    protocol::InstallEvent,
    store,
};
use clap::{Parser, Subcommand};

/// Local speech recognition for cagents. The app starts this itself; the
/// subcommands other than `serve` are for installing models and measuring.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run dictation sessions over JSON lines on standard input and output.
    Serve,
    /// Transcribe a WAV file and report how fast it went.
    Transcribe {
        file: PathBuf,
        #[arg(long, default_value = catalog::DEFAULT)]
        model: String,
        /// Play the file at the pace of speech, as a microphone would, to
        /// measure partials and how soon text appears.
        #[arg(long)]
        realtime: bool,
        /// Print events and a summary as JSON lines.
        #[arg(long)]
        json: bool,
    },
    /// List the models and whether each is installed.
    Models,
    /// Download and verify a model.
    Install {
        model: String,
        /// Report progress as JSON lines.
        #[arg(long)]
        json: bool,
    },
    /// Delete a downloaded model.
    Remove { model: String },
}

fn main() -> ExitCode {
    // A thread that panicked would leave its session without an end, and
    // the client waiting for one; exiting instead tells the client the
    // helper is gone.
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        std::process::abort();
    }));
    match Cli::parse().command {
        Command::Serve => serve::run(),
        Command::Transcribe {
            file,
            model,
            realtime,
            json,
        } => transcribe::run(&file, &model, realtime, json),
        Command::Models => models(),
        Command::Install { model, json } => install(&model, json),
        Command::Remove { model } => remove(&model),
    }
}

fn root() -> Result<PathBuf, ExitCode> {
    store::root().ok_or_else(|| {
        eprintln!("cagents-dictate: no home directory to keep models in");
        ExitCode::FAILURE
    })
}

fn model(id: &str) -> Result<&'static catalog::Model, ExitCode> {
    catalog::find(id).ok_or_else(|| {
        eprintln!("cagents-dictate: no model is called {id}; `cagents-dictate models` lists them");
        ExitCode::FAILURE
    })
}

fn models() -> ExitCode {
    let root = match root() {
        Ok(root) => root,
        Err(code) => return code,
    };
    for model in catalog::MODELS {
        let status = match store::status(&root, model) {
            store::Status::Installed => "installed".to_owned(),
            store::Status::Partial { downloaded } => {
                format!("{} of {} MB downloaded", downloaded >> 20, model.size() >> 20)
            }
            store::Status::Absent => format!("{} MB to download", model.size() >> 20),
        };
        let default = if model.id == catalog::DEFAULT {
            " (default)"
        } else {
            ""
        };
        println!(
            "{}{default}  {}, {}: {status}",
            model.id, model.name, model.languages
        );
    }
    println!("Models are kept in {}", root.display());
    ExitCode::SUCCESS
}

fn install(id: &str, json: bool) -> ExitCode {
    let model = match model(id) {
        Ok(model) => model,
        Err(code) => return code,
    };
    let root = match root() {
        Ok(root) => root,
        Err(code) => return code,
    };
    let mut report = |event: InstallEvent| {
        if json {
            println!("{}", event.to_line());
        } else if let InstallEvent::Progress { done, total } = event {
            eprint!("\r{} of {} MB", done >> 20, total >> 20);
        }
    };
    match install::install(&root, model, &Https::default(), &mut report) {
        Ok(()) => {
            if !json {
                eprintln!("\n{} is installed in {}", model.name, root.display());
            }
            ExitCode::SUCCESS
        }
        Err((kind, message)) => {
            if json {
                println!("{}", InstallEvent::Failed { kind, message }.to_line());
            } else {
                eprintln!("\ncagents-dictate: {message}");
            }
            ExitCode::FAILURE
        }
    }
}

fn remove(id: &str) -> ExitCode {
    let model = match model(id) {
        Ok(model) => model,
        Err(code) => return code,
    };
    let root = match root() {
        Ok(root) => root,
        Err(code) => return code,
    };
    match store::remove(&root, model) {
        Ok(()) => ExitCode::SUCCESS,
        Err(store::LeaseError::Busy) => {
            eprintln!("cagents-dictate: {} is being installed elsewhere", model.name);
            ExitCode::FAILURE
        }
        Err(store::LeaseError::Io(error)) => {
            eprintln!("cagents-dictate: {error}");
            ExitCode::FAILURE
        }
    }
}
