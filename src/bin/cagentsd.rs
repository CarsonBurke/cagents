fn command() -> clap::Command {
    clap::Command::new("cagentsd")
        .version(cagents::version())
        .about("Run the cagents terminal and conversation daemon. Prefer `cagents daemon start`.")
        .arg(
            clap::Arg::new("stored-schema")
                .long("stored-schema")
                .help(
                    "Print the schema of the shared-model store on disk, or nothing if there is \
                     none, then exit. Refuses while a daemon runs.",
                )
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            clap::Arg::new("downgrade-store")
                .long("downgrade-store")
                .value_name("SCHEMA")
                .help(
                    "Convert the shared-model store back to SCHEMA for an older daemon, then exit. \
                     Refuses while a daemon runs.",
                )
                .value_parser(clap::value_parser!(u32)),
        )
        .arg(
            clap::Arg::new("ceditor-notifications")
                .long(cagents::instance::CEDITOR_NOTIFICATION_ARGUMENT.trim_start_matches("--"))
                .hide(true)
                .action(clap::ArgAction::SetTrue),
        )
}

cagents::stamp_version!();

fn main() -> std::process::ExitCode {
    if let Some(result) = cagents::host::run_retained_actor_helper() {
        return match result {
            // The startup owner treats this private status as proved teardown.
            Ok(()) => std::process::ExitCode::from(73),
            Err(error) => {
                eprintln!("cagents retained actor: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    // SAFETY: helper mode returned above; no thread exists. Every
    // terminal and conversation this daemon starts inherits its environment.
    unsafe {
        cagents::account::detach_inherited_selections();
        cagents::agent::detach_session_environment();
    }
    if let Some(result) = cagents::host::run_upgrade_inspection_helper() {
        return match result {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("cagents source inspection: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
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
    // daemon lease; the store maintenance flags below hold the lease only for
    // maintenance and never serve. Internal precheck owners keep their
    // separate entrypoint.
    let matches = command().get_matches();
    if matches.get_flag("stored-schema") {
        return match cagents::host::stored_schema() {
            Ok(schema) => {
                if let Some(schema) = schema {
                    println!("{schema}");
                }
                std::process::ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("cagentsd: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    if let Some(&target) = matches.get_one::<u32>("downgrade-store") {
        return match cagents::host::downgrade_store(target) {
            Ok(Some(from)) => {
                println!("converted the shared-model store from schema {from} to {target}");
                std::process::ExitCode::SUCCESS
            }
            Ok(None) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("cagentsd: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    // SAFETY: entry is still single-threaded. After the maintenance flags,
    // whose answers and refusals belong on the caller's stderr, and before
    // admission, so a refused start is in the development log too.
    unsafe { cagents::instance::redirect_development_daemon_log() };
    // SAFETY: entry is still single-threaded; helper-only mode returned above.
    let admission = match unsafe { cagents::host::prepare_daemon_admission() } {
        Ok(admission) => admission,
        Err(error) => {
            eprintln!("cagentsd: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // Both of these precede runtime startup: the daemon is spawned with stdout and stderr
    // on /dev/null, so without a durable sink a panic in the process that owns
    // every user PTY leaves nothing to diagnose, and `RUST_LOG` is inert.
    cagents::instance::install_panic_hook("cagentsd");
    cagents::instance::install_tracing("cagentsd", "cagentsd=info,cagents=info,warn");
    tracing::info!(version = cagents::version(), "starting cagentsd");
    match cagents::host::run(admission) {
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
