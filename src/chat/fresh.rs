//! An explicit fresh start preserves configuration, never a saved selector or
//! an original positional prompt. Constructors still validate transport values.
use super::{Provider, Session};

pub(super) const CODEX_REASONING_EFFORT_KEY: &str = "model_reasoning_effort";

/// The only Codex config override with an equivalent native fresh-thread field.
pub(super) fn codex_config_effort(value: &str) -> Option<&str> {
    value
        .strip_prefix(CODEX_REASONING_EFFORT_KEY)
        .and_then(|rest| rest.strip_prefix('='))
        .map(|effort| {
            effort
                .strip_prefix('"')
                .and_then(|effort| effort.strip_suffix('"'))
                .unwrap_or(effort)
        })
        .filter(|effort| crate::agent_execution::valid_reasoning_effort(effort))
}

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
        // No native conversation of theirs exists to start afresh.
        Provider::Antigravity | Provider::Muse => None,
    }
}

fn legacy_codex_pane_remote(session: &Session, address: &str) -> bool {
    let Some(path) = address
        .strip_prefix("unix://")
        .filter(|path| !path.is_empty())
    else {
        return false;
    };
    let socket = session.launch.recipe.working_directory.join(path);
    socket
        .file_name()
        .is_some_and(|name| name == "control.sock")
        && socket
            .parent()
            .and_then(std::path::Path::file_name)
            .is_some_and(|name| name == format!("cagents-codex-{}", session.id).as_str())
}

impl Session {
    /// Prepare only an explicit new-conversation choice. A malformed, foreign
    /// or ambiguous saved selector leaves the source untouched and unoffered.
    pub fn fresh_start(&self) -> Option<Self> {
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
        let mut codex_endpoints = 0;
        let mut allocation_remote_index = None;
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
                let mut reset_remote = false;
                let mut allocate_remote = false;
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
                        if provider == Provider::Codex && matches!(flag, "--config" | "-c") {
                            codex_config_effort(value)?;
                        }
                        if owned_address.as_ref().is_some_and(|address| {
                            flag == "--sock" || (flag == "--remote" && value != address)
                        }) {
                            return None;
                        }
                        // Legacy panes predate allocator metadata. Reset only
                        // a real remote option naming this pane's old socket;
                        // recognizing its shape grants no listener authority.
                        if provider == Provider::Codex {
                            if matches!(flag, "--remote" | "--sock") {
                                codex_endpoints += 1;
                            }
                            reset_remote = flag == "--remote"
                                && (owned_address.is_some()
                                    || legacy_codex_pane_remote(self, value));
                            allocate_remote =
                                flag == "--remote" && (reset_remote || value == "unix://");
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
                let output_index = arguments.len();
                arguments.extend_from_slice(&original[begin..=index]);
                if allocate_remote {
                    allocation_remote_index = Some(output_index);
                }
                if reset_remote {
                    *arguments.last_mut()? = if inline.is_some() {
                        "--remote=unix://".into()
                    } else {
                        "unix://".into()
                    };
                }
            }
            index += 1;
        }
        if provider == Provider::Codex {
            // Allocation reads the first raw remote; the native constructor
            // parses endpoint options. A fresh allocation request must mean
            // the same thing at both boundaries.
            let allocator_index = super::codex_remote_argument(&arguments)
                .filter(|(_, address)| *address == "unix://")
                .map(|(index, _)| index);
            if (allocation_remote_index.is_some() || allocator_index.is_some())
                && (codex_endpoints != 1 || allocation_remote_index != allocator_index)
            {
                return None;
            }
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
    fn legacy_codex_fresh_replaces_only_its_pane_remote_and_keeps_reasoning_effort() {
        for location in ["absolute", "relative", "working-directory"] {
            for inline in [false, true] {
                for (flag, effort) in [
                    ("--config", "model_reasoning_effort=high"),
                    ("-c", "model_reasoning_effort=\"xhigh\""),
                ] {
                    let mut source = session(
                        Provider::Codex,
                        &[
                            "--model",
                            "fixture-model",
                            flag,
                            effort,
                            "resume",
                            THREAD,
                            "--sandbox",
                            "read-only",
                        ],
                    );
                    let directory = format!("cagents-codex-{}", source.id);
                    let address = match location {
                        "absolute" => format!("unix:///fixture/run/{directory}/control.sock"),
                        "relative" => format!("unix://{directory}/control.sock"),
                        "working-directory" => {
                            source.launch.recipe.working_directory =
                                format!("/fixture/run/{directory}").into();
                            "unix://control.sock".into()
                        }
                        _ => unreachable!(),
                    };
                    let remote = if inline {
                        vec![format!("--remote={address}")]
                    } else {
                        vec!["--remote".into(), address]
                    };
                    source.launch.recipe.arguments.splice(4..4, remote);
                    source
                        .launch
                        .recipe
                        .environment
                        .insert("CODEX_HOME".into(), "/fixture/selected-account".into());
                    source
                        .launch
                        .recipe
                        .unset_environment
                        .insert("OPENAI_API_KEY".into());
                    source.proof = Some(crate::model::HostSessionProof {
                        host_session_id: "fixture".into(),
                        boot_id: "fixture-boot".into(),
                        host_incarnation: "fixture-owner".into(),
                    });
                    assert!(source.owned_codex_listener.is_none());
                    let original = source.clone();
                    let fresh = source.fresh_start().unwrap();
                    let mut expected = vec!["--model", "fixture-model", flag, effort];
                    if inline {
                        expected.push("--remote=unix://");
                    } else {
                        expected.extend(["--remote", "unix://"]);
                    }
                    expected.extend(["--sandbox", "read-only"]);
                    let mut expected_recipe = source.launch.recipe.clone();
                    expected_recipe.arguments = expected.into_iter().map(String::from).collect();
                    assert_eq!(
                        fresh.launch.recipe, expected_recipe,
                        "{location}, inline={inline}, {flag} {effort}"
                    );
                    assert!(fresh.is_unstarted());
                    assert!(fresh.has_valid_native_continuity());
                    let mut allocated = fresh.clone();
                    assert!(allocated.claim_owned_codex_listener(
                        std::path::Path::new("/fixture/fresh-owned"),
                        source.proof.as_ref().unwrap(),
                    ));
                    assert!(allocated.owns_codex_listener());
                    assert_eq!(source, original);
                }
            }
        }
    }

    #[test]
    fn legacy_codex_fresh_preserves_custom_remotes() {
        let mut source = session(Provider::Codex, &["resume", THREAD]);
        let other_pane = format!(
            "unix:///fixture/cagents-codex-{}/control.sock",
            crate::model::TerminalId::new()
        );
        let custom_socket = format!("unix:///fixture/cagents-codex-{}/custom.sock", source.id);
        for address in [
            "unix://",
            "unix:///fixture/shared/control.sock",
            "unix://custom/control.sock",
            &other_pane,
            &custom_socket,
        ] {
            for inline in [false, true] {
                let mut arguments = if inline {
                    vec![format!("--remote={address}")]
                } else {
                    vec!["--remote".into(), address.into()]
                };
                arguments.extend(["-c".into(), "model_reasoning_effort=high".into()]);
                let expected = arguments.clone();
                arguments.extend(["resume".into(), THREAD.into()]);
                source.launch.recipe.arguments = arguments;
                let original = source.clone();
                assert_eq!(
                    source.fresh_start().unwrap().launch.recipe.arguments,
                    expected
                );
                assert_eq!(source, original);
            }
        }
        for options in [
            vec![
                "--sock".into(),
                format!("/fixture/cagents-codex-{}/control.sock", source.id),
            ],
            vec![
                "--remote".into(),
                "unix:///fixture/custom/first.sock".into(),
                "--remote".into(),
                "unix:///fixture/custom/second.sock".into(),
            ],
        ] {
            let expected = options.clone();
            source.launch.recipe.arguments = options;
            source
                .launch
                .recipe
                .arguments
                .extend(["resume".into(), THREAD.into()]);
            let original = source.clone();
            assert_eq!(
                source.fresh_start().unwrap().launch.recipe.arguments,
                expected
            );
            assert_eq!(source, original);
        }
    }

    #[test]
    fn legacy_codex_remote_reset_preserves_option_payloads() {
        let mut source = session(Provider::Codex, &["resume", THREAD]);
        let old_pane = format!("unix:///fixture/cagents-codex-{}/control.sock", source.id);
        for value in ["--remote", "--sock", "--remote=unix://"] {
            source.launch.recipe.arguments =
                ["--remote", &old_pane, "--model", value, "resume", THREAD]
                    .map(String::from)
                    .into();
            let original = source.clone();
            assert_eq!(
                source.fresh_start().unwrap().launch.recipe.arguments,
                ["--remote", "unix://", "--model", value]
            );
            assert_eq!(source, original);
        }

        source.launch.recipe.arguments =
            ["--model", "--sock", "--remote", &old_pane, "resume", THREAD]
                .map(String::from)
                .into();
        let original = source.clone();
        assert_eq!(
            source.fresh_start().unwrap().launch.recipe.arguments,
            ["--model", "--sock", "--remote", "unix://"]
        );
        assert_eq!(source, original);

        source.launch.recipe.arguments = vec![
            "--model".into(),
            format!("--remote={old_pane}"),
            "--remote".into(),
            "unix:///fixture/custom/control.sock".into(),
        ];
        let expected = source.launch.recipe.arguments.clone();
        source
            .launch
            .recipe
            .arguments
            .extend(["resume".into(), THREAD.into()]);
        let original = source.clone();
        assert_eq!(
            source.fresh_start().unwrap().launch.recipe.arguments,
            expected
        );
        assert_eq!(source, original);

        source.launch.recipe.arguments = [
            "--remote",
            "unix:///fixture/custom.sock",
            "--model",
            "--remote=unix://",
            "resume",
            THREAD,
        ]
        .map(String::from)
        .into();
        let original = source.clone();
        assert_eq!(
            source.fresh_start().unwrap().launch.recipe.arguments,
            [
                "--remote",
                "unix:///fixture/custom.sock",
                "--model",
                "--remote=unix://"
            ]
        );
        assert_eq!(source, original);
    }

    #[test]
    fn a_fresh_codex_allocation_refuses_ambiguous_endpoint_options() {
        let mut source = session(Provider::Codex, &["resume", THREAD]);
        let old_pane = format!("unix:///fixture/cagents-codex-{}/control.sock", source.id);
        for options in [
            vec![
                "--remote",
                old_pane.as_str(),
                "--remote",
                "unix:///fixture/custom.sock",
            ],
            vec![
                "--remote",
                "unix:///fixture/custom.sock",
                "--remote",
                old_pane.as_str(),
            ],
            vec!["--remote", old_pane.as_str(), "--remote", old_pane.as_str()],
            vec![
                "--remote",
                old_pane.as_str(),
                "--sock",
                "/fixture/custom.sock",
            ],
            vec![
                "--sock",
                "/fixture/custom.sock",
                "--remote",
                old_pane.as_str(),
            ],
            vec!["--model", "--remote", "--remote", old_pane.as_str()],
            vec!["--model", "--remote=unix://", "--remote", old_pane.as_str()],
            vec![
                "--remote",
                "unix://",
                "--remote",
                "unix:///fixture/custom.sock",
            ],
            vec![
                "--remote",
                "unix:///fixture/custom.sock",
                "--remote",
                "unix://",
            ],
            vec!["--remote", "unix://", "--sock", "/fixture/custom.sock"],
            vec!["--sock", "/fixture/custom.sock", "--remote", "unix://"],
            vec![
                "--model",
                "--remote=unix://",
                "--remote",
                "unix:///fixture/custom.sock",
            ],
            vec!["--model", "--remote=unix://"],
        ] {
            source.launch.recipe.arguments = options.into_iter().map(String::from).collect();
            source
                .launch
                .recipe
                .arguments
                .extend(["resume".into(), THREAD.into()]);
            let original = source.clone();
            assert!(
                source.fresh_start().is_none(),
                "{:?}",
                source.launch.recipe.arguments
            );
            assert_eq!(source, original);
        }

        // Typed allocations must not turn duplicate bare requests into a
        // private first endpoint followed by a shared last endpoint either.
        let mut owned = session(Provider::Codex, &["--remote", "unix://"]);
        owned.launch.session_id = None;
        let proof = crate::model::HostSessionProof {
            host_session_id: "fixture".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "fixture-owner".into(),
        };
        assert!(owned.claim_owned_codex_listener(std::path::Path::new("/fixture/owned"), &proof));
        let address = owned.launch.recipe.arguments[1].clone();
        owned.launch.session_id = Some(THREAD.into());
        owned.launch.recipe.arguments.extend([
            "--remote".into(),
            address,
            "resume".into(),
            THREAD.into(),
        ]);
        let original = owned.clone();
        assert!(owned.owns_codex_listener());
        assert!(owned.fresh_start().is_none());
        assert_eq!(owned, original);
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
    fn codex_config_values_require_an_equivalent_fresh_thread_override() {
        for flag in ["--config", "-c"] {
            for value in [
                "model_reasoning_effort=high",
                "model_reasoning_effort=\"low\"",
            ] {
                let source = session(Provider::Codex, &[flag, value]);
                assert_eq!(
                    source.fresh_start().unwrap().launch.recipe.arguments,
                    [flag, value]
                );
            }
            for value in [
                "model=\"o3\"",
                "model_reasoning_effort=invalid token",
                "model_reasoning_effort=\"high",
            ] {
                let source = session(Provider::Codex, &[flag, value]);
                let original = source.clone();
                assert!(source.fresh_start().is_none());
                assert_eq!(source, original);
            }
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
