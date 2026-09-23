//! Provider identity and conventions shared by terminal recovery and native chat.
//! Recognition and view activity never authorize conversation recovery.
use crate::model::LaunchRecipe;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

mod claude;
#[cfg(feature = "host")]
pub(crate) use claude::transcript as claude_transcript;
mod codex;
mod omp;

#[cfg(feature = "host")]
pub(crate) mod observe;

/// Where Omp keeps its data - its sessions and its agent database - for
/// `launch`, resolved as Omp resolves it: its profile, its configuration
/// directory, `PI_CODING_AGENT_DIR` and `XDG_DATA_HOME`.
pub fn omp_data_root(launch: &LaunchRecipe, home: &Path) -> Option<PathBuf> {
    omp::data_root(launch, home)
}

// Declaration order is persisted by the binary IPC codec and indexes ADAPTERS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Provider {
    Omp,
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityState {
    #[default]
    Unknown,
    Idle,
    Working,
    Waiting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub provider: Provider,
    pub state: ActivityState,
}

struct ProviderAdapter {
    provider: Provider,
    executable: &'static str,
    label: &'static str,
    recognizes_script: fn(&Path) -> bool,
    session_file_id: fn(&str) -> Option<&str>,
    session_root: fn(&LaunchRecipe, &Path) -> Option<PathBuf>,
    #[cfg(feature = "host")]
    session_directory: &'static str,
    activity_from_title: fn(&str) -> Option<ActivityState>,
    identity_options: &'static [&'static str],
    cwd_options: &'static [&'static str],
    value_options: &'static [&'static str],
    switches: &'static [&'static str],
    resume_flag: &'static str,
    resume_precedes_options: bool,
    #[cfg(feature = "host")]
    forwards_to_native_child: bool,
    #[cfg(feature = "host")]
    create_chat: fn(
        &crate::chat::Launch,
    ) -> Result<
        (
            crate::chat::protocol::ProcessSpec,
            Box<dyn crate::chat::protocol::Adapter>,
        ),
        String,
    >,
}

const ADAPTERS: [ProviderAdapter; 3] = [omp::ADAPTER, claude::ADAPTER, codex::ADAPTER];

impl Provider {
    fn adapter(self) -> &'static ProviderAdapter {
        &ADAPTERS[self as usize]
    }

    /// Every provider in declaration order, for menus and for recognizing the
    /// placeholder titles a card carries before its conversation names itself.
    pub const ALL: [Self; 3] = [Self::Omp, Self::Claude, Self::Codex];

    pub fn executable(self) -> &'static str {
        self.adapter().executable
    }

    pub fn label(self) -> &'static str {
        self.adapter().label
    }

    /// Recognize an exact executable basename, not a shell command or JS wrapper.
    pub fn from_program(program: &str) -> Option<Self> {
        let name = Path::new(program).file_name()?.to_str()?;
        ADAPTERS
            .iter()
            .find(|adapter| adapter.executable == name)
            .map(|adapter| adapter.provider)
    }

    /// Recognize only known foreground executable/JavaScript entrypoints.
    /// Options and identity still require recovery's separate exact validation.
    pub fn recognize_launch(launch: &LaunchRecipe) -> Option<Self> {
        let (provider, arguments) = if Self::is_interpreter(&launch.program) {
            let script = Path::new(launch.arguments.first()?);
            let provider = ADAPTERS
                .iter()
                .find(|adapter| (adapter.recognizes_script)(script))?
                .provider;
            (provider, &launch.arguments[1..])
        } else {
            (
                Self::from_program(&launch.program)?,
                launch.arguments.as_slice(),
            )
        };
        // Shared servers, tool commands and account management do not own the
        // foreground conversation: a terminal signing in is not an agent.
        if arguments.first().is_some_and(|arg| {
            matches!(
                arg.as_str(),
                "app-server"
                    | "exec"
                    | "review"
                    | "mcp-server"
                    | "proxy"
                    | "agents"
                    | "auth"
                    | "login"
                    | "logout"
            ) || arg.starts_with("__omp_")
        }) {
            return None;
        }
        Some(provider)
    }

    /// Preserve the existing process-recovery normalization without granting an ID.
    pub fn normalize_launch(mut launch: LaunchRecipe) -> Option<LaunchRecipe> {
        let provider = Self::recognize_launch(&launch)?;
        if Self::is_interpreter(&launch.program) {
            launch.program = provider.executable().into();
            launch.arguments.remove(0);
        }
        Some(launch)
    }

    pub(crate) fn is_interpreter(program: &str) -> bool {
        matches!(
            Path::new(program)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("node" | "nodejs" | "bun")
        )
    }

    #[cfg(feature = "host")]
    pub(crate) fn forwards_to_native_child(self) -> bool {
        self.adapter().forwards_to_native_child
    }

    pub fn resume_flag(self) -> &'static str {
        self.adapter().resume_flag
    }

    pub(crate) fn is_identity_option(self, flag: &str) -> bool {
        self.adapter().identity_options.contains(&flag)
    }

    pub(crate) fn is_cwd_option(self, flag: &str) -> bool {
        self.adapter().cwd_options.contains(&flag)
    }

    pub(crate) fn is_value_option(self, flag: &str) -> bool {
        self.adapter().value_options.contains(&flag)
    }

    /// Read only the canonical selector position in an already normalized recipe.
    #[cfg(any(feature = "host", feature = "ui", test))]
    pub(crate) fn resume_id(self, arguments: &[String]) -> Option<&str> {
        let (selector, id) = if self.adapter().resume_precedes_options {
            match arguments {
                [selector, id, ..] => (selector, id),
                _ => return None,
            }
        } else {
            match arguments {
                [.., selector, id] => (selector, id),
                _ => return None,
            }
        };
        (selector == self.resume_flag()).then_some(id.as_str())
    }

    pub(crate) fn is_switch(self, flag: &str) -> bool {
        self.adapter().switches.contains(&flag)
    }

    pub(crate) fn append_resume(self, arguments: &mut Vec<String>, id: String) {
        let resume = [self.resume_flag().into(), id];
        if self.adapter().resume_precedes_options {
            arguments.splice(0..0, resume);
        } else {
            arguments.extend(resume);
        }
    }

    /// A filename alone is a convention, never proof that a process owns a session.
    pub fn session_file_id(self, path: &Path) -> Option<String> {
        (path.extension()?.to_str()? == "jsonl").then_some(())?;
        let id = (self.adapter().session_file_id)(path.file_stem()?.to_str()?)?;
        (id.len() == 36 && uuid::Uuid::parse_str(id).is_ok()).then(|| id.to_owned())
    }

    /// Resolve account/profile roots exclusively from the preserved launch.
    pub fn session_root(self, launch: &LaunchRecipe) -> Option<PathBuf> {
        let home = Path::new(launch.environment.get("HOME")?);
        (self.adapter().session_root)(launch, home)
    }

    #[cfg(feature = "host")]
    pub(crate) fn session_state_root(self, launch: &LaunchRecipe) -> Option<PathBuf> {
        if self == Self::Omp {
            omp::state_root(launch, Path::new(launch.environment.get("HOME")?))
        } else {
            self.session_root(launch)
        }
    }

    #[cfg(feature = "host")]
    pub(crate) fn session_directory(self) -> &'static str {
        self.adapter().session_directory
    }

    /// Decode only an authoritative provider lifecycle title grammar.
    /// This describes the provider's viewed agent, not aggregate session activity.
    pub fn activity_from_title(self, title: &str) -> Option<ActivityState> {
        (self.adapter().activity_from_title)(title)
    }

    /// Presentation metadata from this provider's own PTY, never a session ID
    /// or permission to attach to a conversation.
    pub fn conversation_title<'a>(self, title: &'a str, cwd: &std::path::Path) -> Option<&'a str> {
        let label = match self {
            Self::Omp => {
                self.activity_from_title(title)?;
                let rest = title.strip_prefix("π ")?;
                rest[rest.chars().next()?.len_utf8()..].trim()
            }
            Self::Codex => codex_title_without_spinner(title).trim(),
            // Claude exposes names through its PID-scoped inventory. Arbitrary
            // OSC titles are not an alternative to that authoritative source.
            Self::Claude => return None,
        };
        let directory = cwd.file_name().and_then(|name| name.to_str());
        // Codex's default title ends in the project name, separated from the
        // thread name by " | ". Only remove a suffix matching this PTY's cwd.
        let label = if self == Self::Codex {
            label
                .rsplit_once(" | ")
                .filter(|(_, project)| {
                    Some(*project) == directory || Some(*project) == cwd.to_str()
                })
                .map_or(label, |(name, _)| name)
        } else {
            label
        };
        if label.is_empty()
            || Some(label) == cwd.to_str()
            || Some(label) == directory
            || [
                "codex",
                "omp",
                "Ready",
                "Working",
                "Idle",
                "Action Required",
            ]
            .iter()
            .any(|default| label.eq_ignore_ascii_case(default))
            || label.starts_with("[ ! ] Action Required")
            || label.starts_with("[ . ] Action Required")
        {
            return None;
        }
        Some(label)
    }

    #[cfg(feature = "host")]
    pub(crate) fn create_chat(
        self,
        launch: &crate::chat::Launch,
    ) -> Result<
        (
            crate::chat::protocol::ProcessSpec,
            Box<dyn crate::chat::protocol::Adapter>,
        ),
        String,
    > {
        (self.adapter().create_chat)(launch)
    }
}

/// Codex's configured leading activity item is rendered by the shared app
/// spinner instead. This is presentation only; it never infers agent activity.
pub(crate) fn codex_title_without_spinner(title: &str) -> &str {
    let Some(first) = title.chars().next() else {
        return title;
    };
    if "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(first) {
        let rest = &title[first.len_utf8()..];
        if rest.is_empty() {
            rest
        } else {
            rest.strip_prefix(' ').unwrap_or(title)
        }
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_titles_omit_activity_and_ignore_launch_placeholders() {
        let cwd = std::path::Path::new("/repo/project");
        assert_eq!(
            Provider::Omp.conversation_title("π ◕ Fix parser", cwd),
            Some("Fix parser")
        );
        assert_eq!(Provider::Omp.conversation_title("π > project", cwd), None);
        assert_eq!(
            Provider::Omp.conversation_title("π > /repo/project", cwd),
            None
        );
        assert_eq!(
            Provider::Omp.conversation_title("arbitrary shell title", cwd),
            None
        );
        assert_eq!(
            Provider::Codex.conversation_title("⠋ Fix parser", cwd),
            Some("Fix parser")
        );
        assert_eq!(
            Provider::Codex.conversation_title("⠋ Fix parser | project", cwd),
            Some("Fix parser")
        );
        assert_eq!(Provider::Codex.conversation_title("⠋ project", cwd), None);
        assert_eq!(Provider::Codex.conversation_title("codex", cwd), None);
        assert_eq!(Provider::Claude.conversation_title("Fix parser", cwd), None);
    }

    fn launch(program: &str, arguments: &[&str]) -> LaunchRecipe {
        LaunchRecipe {
            program: program.into(),
            arguments: arguments.iter().map(|arg| (*arg).into()).collect(),
            working_directory: "/tmp".into(),
            environment: Default::default(),
        }
    }

    #[test]
    fn recognition_distinguishes_foreground_wrappers_from_scripts_and_servers() {
        for (program, script, provider) in [
            ("node", "/opt/@openai/codex/bin/codex.js", Provider::Codex),
            (
                "nodejs",
                "/opt/@anthropic-ai/claude-code/cli.js",
                Provider::Claude,
            ),
            ("bun", "/opt/omp/omp.js", Provider::Omp),
        ] {
            assert_eq!(
                Provider::recognize_launch(&launch(program, &[script])),
                Some(provider)
            );
            assert_eq!(
                Provider::recognize_launch(&launch(program, &[script, "app-server"])),
                None
            );
        }
        for recipe in [
            launch("sh", &["-c", "omp"]),
            launch("node", &["/tmp/cli.js", "claude"]),
            launch("node", &["/tmp/tool.js", "codex"]),
            launch("omp", &["__omp_worker"]),
            launch("codex", &["exec"]),
            launch("claude", &["auth", "login"]),
            launch("codex", &["login"]),
            launch("/usr/bin/codex", &["logout"]),
        ] {
            assert_eq!(Provider::recognize_launch(&recipe), None);
        }
        // Foreground recognition must not turn a fresh launch into resume authority.
        let fresh = Provider::normalize_launch(launch("bun", &["/opt/omp/omp.js"])).unwrap();
        assert!(crate::model::agent_recovery::resume_recipe(&fresh).is_none());
    }
}
