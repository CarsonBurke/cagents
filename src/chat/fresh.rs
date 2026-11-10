//! An explicit fresh start preserves configuration, never a saved selector or
//! an original positional prompt. Constructors still validate transport values.
use super::{Provider, Session};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ArgumentArity {
    Switch,
    Value,
    Values,
}

/// Shared with the native Claude constructor so selector-looking option values
/// are consumed as data in both paths. Transport branches keep their own checks.
pub(super) fn claude_arity(flag: &str) -> Option<ArgumentArity> {
    use ArgumentArity::*;
    Some(match flag {
        "--model"
        | "--agent"
        | "--permission-mode"
        | "--settings"
        | "--setting-sources"
        | "--effort"
        | "--system-prompt"
        | "--append-system-prompt"
        | "--max-turns"
        | "--max-budget-usd"
        | "--fallback-model" => Value,
        "--add-dir" | "--mcp-config" | "--plugin-dir" | "--allowedTools" | "--allowed-tools"
        | "--disallowedTools" | "--disallowed-tools" | "--tools" => Values,
        "--bare"
        | "--safe-mode"
        | "--no-chrome"
        | "--chrome"
        | "--strict-mcp-config"
        | "--disable-slash-commands"
        | "--dangerously-skip-permissions"
        | "--allow-dangerously-skip-permissions" => Switch,
        _ => return None,
    })
}

fn arity(provider: Provider, flag: &str) -> Option<ArgumentArity> {
    use ArgumentArity::*;
    match provider {
        Provider::Claude => claude_arity(flag).or(match flag {
            "--input-format" | "--output-format" | "--permission-prompt-tool" => Some(Value),
            "--print"
            | "-p"
            | "--verbose"
            | "--include-partial-messages"
            | "--replay-user-messages" => Some(Switch),
            _ => None,
        }),
        Provider::Codex => match flag {
            "--sock" | "--remote" | "-m" | "--model" | "-s" | "--sandbox" | "-a"
            | "--ask-for-approval" | "--config" | "-c" => Some(Value),
            "app-server"
            | "proxy"
            | "--no-alt-screen"
            | "--dangerously-bypass-approvals-and-sandbox"
            | "--yolo" => Some(Switch),
            _ => None,
        },
        Provider::Omp => {
            if provider.is_value_option(flag) || flag == "--mode" {
                Some(Value)
            } else if provider.is_switch(flag) {
                Some(Switch)
            } else {
                None
            }
        }
    }
}

impl Session {
    /// Prepare only an explicit new-conversation choice. A malformed, foreign
    /// or ambiguous saved selector leaves the source untouched and unoffered.
    pub(crate) fn fresh_start(&self) -> Option<Self> {
        let provider = self.launch.provider;
        let selected = self.launch.session_id.as_deref();
        if selected.is_some_and(|id| {
            id.is_empty()
                || id == "latest"
                || id.contains('\0')
                || (provider != Provider::Codex
                    && (id.len() != 36
                        || uuid::Uuid::parse_str(id).is_err()
                        || (provider == Provider::Claude
                            && id.bytes().any(|byte| byte.is_ascii_uppercase()))))
        }) {
            return None;
        }
        let original = &self.launch.recipe.arguments;
        if self.owned_codex_listener.is_some() && !self.owns_codex_listener() {
            return None;
        }
        let owned_address = self
            .owned_codex_listener
            .as_ref()
            .filter(|_| self.owns_codex_listener())
            .map(|owned| format!("unix://{}", owned.socket.display()));
        if original.len() > 128
            || original.iter().map(String::len).sum::<usize>() > 64 * 1024
            || original.iter().any(|argument| argument.contains('\0'))
        {
            return None;
        }
        let mut arguments = Vec::with_capacity(original.len());
        let mut index = 0;
        while let Some(argument) = original.get(index) {
            let (flag, inline) = argument
                .split_once('=')
                .map_or((argument.as_str(), None), |(flag, value)| {
                    (flag, Some(value))
                });
            if provider.is_identity_option(flag) {
                if provider == Provider::Codex && inline.is_some() {
                    return None;
                }
                let value = match inline {
                    Some(value) => value,
                    None => {
                        index += 1;
                        original.get(index)?.as_str()
                    }
                };
                if Some(value) != selected {
                    return None;
                }
            } else {
                let begin = index;
                match arity(provider, flag)? {
                    ArgumentArity::Switch if inline.is_none() => {}
                    ArgumentArity::Value => {
                        let value = match inline {
                            Some(value) => value,
                            None => {
                                index += 1;
                                original.get(index)?.as_str()
                            }
                        };
                        if value.is_empty()
                            && !(provider == Provider::Claude && flag == "--setting-sources")
                        {
                            return None;
                        }
                        // Codex's native constructor supports an inline address
                        // only for --remote; other inline forms are ambiguous.
                        if provider == Provider::Codex && inline.is_some() && flag != "--remote" {
                            return None;
                        }
                        if owned_address.as_ref().is_some_and(|address| {
                            flag == "--sock" || (flag == "--remote" && value != address)
                        }) {
                            return None;
                        }
                    }
                    ArgumentArity::Values => {
                        if inline.is_none() {
                            let first = index + 1;
                            while original
                                .get(index + 1)
                                .is_some_and(|value| !value.starts_with('-'))
                            {
                                index += 1;
                            }
                            if index < first {
                                return None;
                            }
                        }
                    }
                    _ => return None,
                }
                arguments.extend_from_slice(&original[begin..=index]);
                if owned_address.is_some() && flag == "--remote" {
                    *arguments.last_mut()? = if inline.is_some() {
                        "--remote=unix://".into()
                    } else {
                        "unix://".into()
                    };
                }
            }
            index += 1;
        }
        let mut fresh = self.clone();
        // Ownership matching needs the original descriptor and remote address.
        fresh.reset_owned_codex_listener();
        fresh.launch.recipe.arguments = arguments;
        fresh.launch.session_id = None;
        fresh.proof = None;
        fresh.terminal = None;
        Some(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const THREAD: &str = "33333333-4444-4aaa-8bbb-cccccccccccc";

    fn session(provider: Provider, arguments: &[&str]) -> Session {
        Session {
            id: crate::model::TerminalId::new(),
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
            launch: super::super::Launch {
                provider,
                session_id: Some(THREAD.into()),
                recipe: crate::model::LaunchRecipe {
                    program: provider.executable().into(),
                    arguments: arguments
                        .iter()
                        .map(|argument| (*argument).into())
                        .collect(),
                    working_directory: "/fixture/project".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
        }
    }

    #[test]
    fn the_explicit_provider_keeps_a_custom_executable_verbatim() {
        let mut source = session(
            Provider::Claude,
            &["--resume", THREAD, "--system-prompt=resume"],
        );
        source.launch.recipe.program = "/fixture/bin/custom-native-provider".into();
        let original = source.clone();
        let fresh = source.fresh_start().unwrap();
        assert_eq!(fresh.launch.provider, Provider::Claude);
        assert_eq!(fresh.launch.recipe.program, source.launch.recipe.program);
        assert_eq!(fresh.launch.recipe.arguments, ["--system-prompt=resume"]);
        assert_eq!(source, original);
    }

    #[test]
    fn claude_removes_only_exact_selectors_and_preserves_literal_native_options() {
        let source = session(
            Provider::Claude,
            &[
                "--resume",
                THREAD,
                "-r",
                THREAD,
                "--model",
                "--resume",
                "--system-prompt",
                "resume",
                "--settings={\"feature\":\"--resume\"}",
                "--setting-sources=",
                "--tools=--resume",
                "--allowedTools",
                "Bash",
                "resume",
            ],
        );
        let original = source.clone();
        let fresh = source.fresh_start().unwrap();
        assert!(fresh.is_unstarted());
        assert_eq!(
            fresh.launch.recipe.arguments,
            [
                "--model",
                "--resume",
                "--system-prompt",
                "resume",
                "--settings={\"feature\":\"--resume\"}",
                "--setting-sources=",
                "--tools=--resume",
                "--allowedTools",
                "Bash",
                "resume",
            ]
        );
        assert_eq!(source, original);
    }

    #[test]
    fn codex_resume_word_as_an_option_value_is_not_a_thread_selector() {
        let source = session(
            Provider::Codex,
            &["--model", "resume", "resume", THREAD, "resume", THREAD],
        );
        assert_eq!(
            source.fresh_start().unwrap().launch.recipe.arguments,
            ["--model", "resume"]
        );
        let source = session(
            Provider::Omp,
            &["--system-prompt", "--resume", "-r", THREAD],
        );
        assert_eq!(
            source.fresh_start().unwrap().launch.recipe.arguments,
            ["--system-prompt", "--resume"]
        );
    }

    #[test]
    fn owned_codex_remote_normalization_preserves_remote_tokens_used_as_values() {
        let mut source = session(
            Provider::Codex,
            &[
                "--remote",
                "unix://",
                "--model",
                "--remote",
                "--sandbox",
                "read-only",
            ],
        );
        source.launch.session_id = None;
        let proof = crate::model::HostSessionProof {
            host_session_id: "fixture".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "fixture-owner".into(),
        };
        assert!(source.claim_owned_codex_listener(std::path::Path::new("/fixture/owned"), &proof));
        source.launch.session_id = Some(THREAD.into());
        source
            .launch
            .recipe
            .arguments
            .extend(["resume".into(), THREAD.into()]);
        let original = source.clone();
        let fresh = source.fresh_start().unwrap();
        assert_eq!(
            fresh.launch.recipe.arguments,
            [
                "--remote",
                "unix://",
                "--model",
                "--remote",
                "--sandbox",
                "read-only"
            ]
        );
        assert_eq!(source, original);
        // Owned metadata cannot reinterpret a preceding option payload as its
        // allocator address. Preserve that mismatched source for diagnosis.
        source
            .launch
            .recipe
            .arguments
            .splice(0..0, ["--model".into(), "--remote".into()]);
        let original = source.clone();
        assert!(!source.owns_codex_listener());
        assert!(source.fresh_start().is_none());
        assert_eq!(source, original);
    }

    #[test]
    fn lowercase_uuid_requirement_belongs_only_to_the_claude_constructor() {
        let upper = THREAD.to_ascii_uppercase();
        for provider in [Provider::Claude, Provider::Omp] {
            let mut source = session(provider, &["--resume", &upper]);
            source.launch.session_id = Some(upper.clone());
            assert_eq!(source.fresh_start().is_some(), provider == Provider::Omp);
        }
    }

    #[test]
    fn unsafe_saved_selectors_and_positional_prompts_leave_the_source_unchanged() {
        for (provider, arguments) in [
            (Provider::Claude, vec!["--resume", "another-thread"]),
            (Provider::Claude, vec!["--session-id="]),
            (Provider::Claude, vec!["--resume"]),
            (Provider::Claude, vec!["--continue"]),
            (Provider::Claude, vec!["old prompt"]),
            (Provider::Claude, vec!["--model"]),
            (Provider::Claude, vec!["--tools", "--resume", THREAD]),
            (Provider::Codex, vec!["resume", "another-thread"]),
            (Provider::Codex, vec!["resume"]),
            (Provider::Codex, vec!["resume=thread"]),
            (Provider::Codex, vec!["--last"]),
            (Provider::Omp, vec!["--unknown", "--resume", THREAD]),
        ] {
            let source = session(provider, &arguments);
            let original = source.clone();
            assert!(
                source.fresh_start().is_none(),
                "{provider:?}: {arguments:?}"
            );
            assert_eq!(source, original);
        }
    }
}
