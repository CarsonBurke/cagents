fn command() -> clap::Command {
    clap::Command::new("cagentsd")
        .version(cagents::VERSION)
        .about("Run the cagents terminal and conversation daemon. Prefer `cagents daemon start`.")
        .arg(
            clap::Arg::new("ceditor-notifications")
                .long(cagents::instance::CEDITOR_NOTIFICATION_ARGUMENT.trim_start_matches("--"))
                .hide(true)
                .action(clap::ArgAction::SetTrue),
        )
}

fn main() -> std::process::ExitCode {
    // SAFETY: first statement of main, before any thread exists. Every
    // terminal and conversation this daemon starts inherits its environment.
    unsafe { cagents::account::detach_inherited_selections() };
    if let Some(result) = cagents::host::run_automation_helper() {
        return match result {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("cagents precheck: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    // Informational flags exit before opening runtime state or taking its
    // daemon lease. Internal precheck owners keep their separate entrypoint.
    command().get_matches();
    // Both of these come first: the daemon is spawned with stdout and stderr
    // on /dev/null, so without a durable sink a panic in the process that owns
    // every user PTY leaves nothing to diagnose, and `RUST_LOG` is inert.
    cagents::instance::install_panic_hook("cagentsd");
    cagents::instance::install_tracing("cagentsd", "cagentsd=info,cagents=info,warn");
    tracing::info!(version = cagents::VERSION, "starting cagentsd");
    match cagents::host::run() {
        Ok(()) => {
            cagents::diagnostics::record(cagents::diagnostics::Event::Exit { success: true });
            std::process::ExitCode::SUCCESS
        }
        // Returning `Result` from `main` renders this with `Debug`, which
        // wraps the message in its error variant and escapes the quotes
        // around the socket path. Someone running the daemon by hand to find
        // out why it will not start has to read the text.
        Err(error) => {
            cagents::instance::report_startup_failure();
            cagents::diagnostics::record(cagents::diagnostics::Event::Exit { success: false });
            eprintln!("cagentsd: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn informational_flags_exit_before_daemon_startup() {
        for (argument, kind) in [
            ("--help", clap::error::ErrorKind::DisplayHelp),
            ("--version", clap::error::ErrorKind::DisplayVersion),
        ] {
            let error = super::command()
                .try_get_matches_from(["cagentsd", argument])
                .unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(error.exit_code(), 0);
        }
    }

    #[test]
    fn unknown_arguments_cannot_silently_start_the_daemon() {
        let error = super::command()
            .try_get_matches_from(["cagentsd", "--unknown"])
            .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        assert_ne!(error.exit_code(), 0);
    }

    #[test]
    fn ceditor_launcher_mode_remains_accepted_and_hidden() {
        let matches = super::command()
            .try_get_matches_from(["cagentsd", cagents::instance::CEDITOR_NOTIFICATION_ARGUMENT])
            .unwrap();
        assert!(matches.get_flag("ceditor-notifications"));
        assert!(
            !super::command()
                .render_help()
                .to_string()
                .contains("--ceditor-notifications")
        );
        assert!(super::command().try_get_matches_from(["cagentsd"]).is_ok());
    }
}
