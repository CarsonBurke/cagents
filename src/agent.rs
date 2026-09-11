//! Provider identity and conventions shared by terminal recovery and native chat.
//! Recognition and view activity never authorize conversation recovery.
use crate::model::LaunchRecipe;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

mod claude;
mod codex;
mod omp;

#[cfg(feature = "host")]
pub(crate) mod observe;

// Declaration order is persisted by the binary IPC codec and indexes ADAPTERS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    session_directory: &'static str,
    activity_from_title: fn(&str) -> Option<ActivityState>,
    identity_options: &'static [&'static str],
    cwd_options: &'static [&'static str],
    value_options: &'static [&'static str],
    switches: &'static [&'static str],
    resume_flag: &'static str,
    resume_precedes_options: bool,
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
        // Shared servers and tool commands do not own the foreground conversation.
        if arguments.first().is_some_and(|arg| {
            matches!(
                arg.as_str(),
                "app-server" | "exec" | "review" | "mcp-server" | "proxy" | "agents"
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

    pub(crate) fn session_directory(self) -> &'static str {
        self.adapter().session_directory
    }

    /// Decode only an authoritative provider lifecycle title grammar.
    /// This describes the provider's viewed agent, not aggregate session activity.
    pub fn activity_from_title(self, title: &str) -> Option<ActivityState> {
        (self.adapter().activity_from_title)(title)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        ] {
            assert_eq!(Provider::recognize_launch(&recipe), None);
        }
        // Foreground recognition must not turn a fresh launch into resume authority.
        let fresh = Provider::normalize_launch(launch("bun", &["/opt/omp/omp.js"])).unwrap();
        assert!(crate::model::agent_recovery::resume_recipe(&fresh).is_none());
    }
}
