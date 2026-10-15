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
            if command.get_about().is_none_or(|text| text.to_string().trim().is_empty()) {
                missing.push(format!("{path}: command description"));
            }
            for argument in command.get_arguments() {
                if argument.get_help().is_none_or(|text| text.to_string().trim().is_empty()) {
                    missing.push(format!("{path}: argument {}", argument.get_id()));
                }
            }
            for child in command.get_subcommands() {
                visit(child, &format!("{path} {}", child.get_name()), missing);
            }
        }
        let mut command = Cli::command(); command.build();
        let mut missing = Vec::new(); visit(&command, "cagents", &mut missing);
        assert!(missing.is_empty(), "Missing help:\n{}", missing.join("\n"));
    }
    #[test]
    fn common_shell_scripts_cover_nested_agent_commands() {
        for shell in [clap_complete::Shell::Fish, clap_complete::Shell::Bash, clap_complete::Shell::Zsh,
            clap_complete::Shell::Elvish, clap_complete::Shell::PowerShell] {
            let output = execute(shell);
            assert!(!output.human.is_empty());
            assert!(output.human.contains("spawn"), "{shell} lacks agent spawn");
        }
    }
    #[test]
    fn providers_accept_both_spellings_and_limits_are_never_silently_clamped() {
        for provider in ["claude", "Claude", "CODEX", "omp"] {
            assert!(Cli::try_parse_from(["cagents", "agent", "spawn", "hello", "--provider", provider]).is_ok());
        }
        for mode in ["always-ask", "write", "yolo"] {
            assert!(Cli::try_parse_from(["cagents", "agent", "spawn", "hello", "--provider", "omp", "--permission-mode", mode]).is_ok());
        }
        assert!(Cli::try_parse_from(["cagents", "automation", "list", "--limit", "101"]).is_err());
        assert!(Cli::try_parse_from(["cagents", "automation", "history", "--limit", "0"]).is_err());
    }
}
