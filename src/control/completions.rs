use super::*;
use clap::CommandFactory;

pub(super) fn script(shell: clap_complete::Shell) -> String {
    let mut output = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "cagents", &mut output);
    String::from_utf8(output).expect("completion generators emit UTF-8")
}

pub(super) fn execute(shell: clap_complete::Shell) -> CommandOutput {
    let script = script(shell);
    CommandOutput {
        command: "completions",
        data: json!({"shell": shell.to_string(), "script": script}),
        human: script,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_entire_command_tree_has_help_text() {
        fn visit(command: &clap::Command, path: &str, missing: &mut Vec<String>) {
            if command
                .get_about()
                .is_none_or(|text| text.to_string().trim().is_empty())
            {
                missing.push(format!("{path}: command description"));
            }
            for argument in command.get_arguments() {
                if argument
                    .get_help()
                    .is_none_or(|text| text.to_string().trim().is_empty())
                {
                    missing.push(format!("{path}: argument {}", argument.get_id()));
                }
            }
            for child in command.get_subcommands() {
                visit(child, &format!("{path} {}", child.get_name()), missing);
            }
        }
        let mut command = Cli::command();
        command.build();
        let mut missing = Vec::new();
        visit(&command, "cagents", &mut missing);
        assert!(missing.is_empty(), "Missing help:\n{}", missing.join("\n"));
    }
    #[test]
    fn common_shell_scripts_cover_nested_agent_commands() {
        for shell in [
            clap_complete::Shell::Fish,
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Elvish,
            clap_complete::Shell::PowerShell,
        ] {
            let output = execute(shell);
            assert!(!output.human.is_empty());
            assert!(output.human.contains("spawn"), "{shell} lacks agent spawn");
        }
    }
    #[test]
    fn completions_bypass_device_and_service_routing() {
        let cli = Cli::try_parse_from([
            "cagents",
            "--device",
            "missing-device",
            "completions",
            "fish",
        ])
        .unwrap();
        assert!(!staged_command_requires_daemon(&cli.command));
        assert!(!routes_to_staged_owner(&cli.command));
        assert_eq!(
            run_from([
                "cagents",
                "--device",
                "missing-device",
                "completions",
                "fish"
            ]),
            ExitCode::SUCCESS
        );
    }
    #[test]
    fn automation_page_sizes_are_explicit_and_bounded() {
        for command in ["list", "history"] {
            for limit in ["1", "100"] {
                assert!(
                    Cli::try_parse_from(["cagents", "automation", command, "--limit", limit])
                        .is_ok()
                );
            }
            for limit in ["0", "101"] {
                assert!(
                    Cli::try_parse_from(["cagents", "automation", command, "--limit", limit])
                        .is_err()
                );
            }
        }
        let help = Cli::try_parse_from(["cagents", "automation", "list", "--help"])
            .unwrap_err()
            .to_string();
        assert!(!help.contains("a cursor"));
        assert!(help.contains("--after"));
    }
    #[test]
    fn spawn_help_explains_provider_settings() {
        let help = Cli::try_parse_from(["cagents", "agent", "spawn", "--help"])
            .unwrap_err()
            .to_string();
        for text in [
            "Provider model ID",
            "provider/model",
            "acceptEdits",
            "on-request",
            "always-ask",
        ] {
            assert!(help.contains(text), "missing {text}: {help}");
        }
    }
}
