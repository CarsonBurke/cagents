//! The permission ceiling a hosted agent places on the agents it starts.
//!
//! An agent never grants a child more than it has itself: a child's
//! permission mode and sandbox are equal to or stricter than its spawning
//! session's. Within one provider modes compare exactly. Across providers they
//! compare on a coarse [`Tier`], which each mode reaches by what it lets the
//! agent do without asking anyone. A mode this module cannot rank is refused
//! for an agent caller; a session whose own mode it cannot confirm counts as
//! [`Tier::Ask`]. A person's own connection has no ceiling.

use crate::agent::Provider;

/// What an agent may do without anyone approving it, from strictest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// It reads; edits and commands need approval or cannot happen.
    ReadOnly,
    /// Edits or commands need approval.
    Ask,
    /// Edits apply without asking; risky commands still ask, or stay inside
    /// a sandbox.
    AutoEdit,
    /// Nothing asks.
    Full,
}

/// Claude's modes, which an ACP agent also names its own by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Claude {
    Plan,
    Default,
    AcceptEdits,
    BypassPermissions,
}

/// Codex's approval policy. `on-request` and `on-failure` both run a command
/// in the sandbox without asking and ask before leaving it, so they rank
/// equally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Approval {
    Untrusted,
    OnRequest,
    OnFailure,
    Never,
}

/// Codex's sandbox, from strictest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Sandbox {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// Oh My Pi's approval modes. Its `write` mode is left unranked: whether it
/// asks before writes or approves them is not established here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Omp {
    AlwaysAsk,
    Yolo,
}

/// One provider's rankable permission setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Claude(Claude),
    /// An ACP agent's mode, by the cagents name of what it means, which
    /// ranks as Claude's mode of that name.
    Acp(Claude, &'static str),
    Codex(Approval, Sandbox),
    Omp(Omp),
}

impl Approval {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "untrusted" => Self::Untrusted,
            "on-request" => Self::OnRequest,
            "on-failure" => Self::OnFailure,
            "never" => Self::Never,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::OnRequest => "on-request",
            Self::OnFailure => "on-failure",
            Self::Never => "never",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Untrusted => 0,
            Self::OnRequest | Self::OnFailure => 1,
            Self::Never => 2,
        }
    }
}

impl Sandbox {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "read-only" => Self::ReadOnly,
            "workspace-write" => Self::WorkspaceWrite,
            "danger-full-access" => Self::DangerFullAccess,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

impl Claude {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "plan" => Self::Plan,
            "default" | "manual" => Self::Default,
            "acceptEdits" => Self::AcceptEdits,
            "bypassPermissions" => Self::BypassPermissions,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

/// A Codex mode as cagents names it: the approval policy, optionally followed
/// by `/` and the sandbox, as in `on-request/workspace-write`.
fn codex(value: &str) -> Option<(Approval, Option<Sandbox>)> {
    match value.split_once('/') {
        Some((approval, sandbox)) => {
            Some((Approval::parse(approval)?, Some(Sandbox::parse(sandbox)?)))
        }
        None => Some((Approval::parse(value)?, None)),
    }
}

/// Splits a Codex mode into its approval policy and sandbox, for the
/// provider's own `--ask-for-approval` and `--sandbox` options. A value that is
/// not of that form is the approval policy alone, as given.
pub fn codex_launch(value: &str) -> (&str, Option<&str>) {
    match value.split_once('/') {
        Some((approval, sandbox)) if codex(value).is_some() => (approval, Some(sandbox)),
        _ => (value, None),
    }
}

impl Mode {
    /// `value` as `provider` names a complete mode. A Codex value without a
    /// sandbox is incomplete; see [`codex`].
    fn parse(provider: Provider, value: &str) -> Option<Self> {
        match provider {
            Provider::Claude => Claude::parse(value).map(Self::Claude),
            Provider::Codex => match codex(value)? {
                (approval, Some(sandbox)) => Some(Self::Codex(approval, sandbox)),
                (_, None) => None,
            },
            Provider::Omp => match value {
                "always-ask" => Some(Self::Omp(Omp::AlwaysAsk)),
                "yolo" => Some(Self::Omp(Omp::Yolo)),
                _ => None,
            },
            // A terminal alias, such as Muse's `untrusted`, names its meaning.
            Provider::Antigravity | Provider::Muse => {
                let meaning = provider.acp()?.mode_meaning(value);
                let meaning = *provider
                    .permission_modes()
                    .iter()
                    .find(|mode| **mode == meaning)?;
                Claude::parse(meaning).map(|mode| Self::Acp(mode, meaning))
            }
        }
    }

    fn name(self) -> String {
        match self {
            Self::Claude(mode) => mode.name().into(),
            Self::Acp(_, meaning) => meaning.into(),
            Self::Codex(approval, sandbox) => format!("{}/{}", approval.name(), sandbox.name()),
            Self::Omp(Omp::AlwaysAsk) => "always-ask".into(),
            Self::Omp(Omp::Yolo) => "yolo".into(),
        }
    }

    fn tier(self) -> Tier {
        match self {
            Self::Acp(mode, _) => Self::Claude(mode).tier(),
            Self::Claude(Claude::Plan) => Tier::ReadOnly,
            Self::Claude(Claude::Default) => Tier::Ask,
            Self::Claude(Claude::AcceptEdits) => Tier::AutoEdit,
            Self::Claude(Claude::BypassPermissions) => Tier::Full,
            // A read-only sandbox writes nothing, whoever would be asked.
            Self::Codex(_, Sandbox::ReadOnly) => Tier::ReadOnly,
            // `untrusted` asks before any edit and any command it does not
            // know to be read-only.
            Self::Codex(Approval::Untrusted, _) => Tier::Ask,
            // The others edit the workspace and run sandboxed commands
            // without asking; `never` also never leaves the sandbox.
            Self::Codex(_, Sandbox::WorkspaceWrite) => Tier::AutoEdit,
            Self::Codex(_, Sandbox::DangerFullAccess) => Tier::Full,
            Self::Omp(Omp::AlwaysAsk) => Tier::Ask,
            Self::Omp(Omp::Yolo) => Tier::Full,
        }
    }

    /// Whether this mode allows no more than `ceiling`, the mode of a
    /// session of the same provider.
    fn within(self, ceiling: Self) -> bool {
        match (self, ceiling) {
            (Self::Claude(mode), Self::Claude(ceiling))
            | (Self::Acp(mode, _), Self::Acp(ceiling, _)) => mode <= ceiling,
            (Self::Codex(approval, sandbox), Self::Codex(ceiling_approval, ceiling_sandbox)) => {
                approval.rank() <= ceiling_approval.rank() && sandbox <= ceiling_sandbox
            }
            (Self::Omp(mode), Self::Omp(ceiling)) => mode <= ceiling,
            _ => self.tier() <= ceiling.tier(),
        }
    }

    /// The provider's rankable modes, from strictest, each the strictest of
    /// its tier.
    fn strictest(provider: Provider) -> Vec<Mode> {
        use Approval::*;
        use Sandbox::*;
        match provider {
            Provider::Claude => vec![
                Self::Claude(Claude::Plan),
                Self::Claude(Claude::Default),
                Self::Claude(Claude::AcceptEdits),
                Self::Claude(Claude::BypassPermissions),
            ],
            Provider::Codex => vec![
                Self::Codex(Untrusted, ReadOnly),
                Self::Codex(Untrusted, WorkspaceWrite),
                Self::Codex(OnRequest, WorkspaceWrite),
                Self::Codex(OnRequest, DangerFullAccess),
            ],
            Provider::Omp => vec![Self::Omp(Omp::AlwaysAsk), Self::Omp(Omp::Yolo)],
            // Only the modes the agent has, each its own tier.
            Provider::Antigravity | Provider::Muse => {
                let mut modes: Vec<_> = provider
                    .permission_modes()
                    .iter()
                    .filter_map(|mode| Self::parse(provider, mode))
                    .collect();
                modes.sort_by_key(|mode| mode.tier());
                modes
            }
        }
    }
}

/// The most a spawning agent session may give the agents it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ceiling {
    /// The session's provider, when it runs one cagents recognises.
    provider: Option<Provider>,
    /// The session's own mode, when it is known and rankable.
    mode: Option<Mode>,
}

impl Ceiling {
    /// A native conversation's provider-confirmed mode, as its snapshot
    /// names it.
    pub fn confirmed(provider: Provider, mode: Option<&str>) -> Self {
        let mode = mode.and_then(|mode| match provider {
            // The native thread's names for its approval and sandbox pairs.
            Provider::Codex => match mode {
                "readOnly" => Some(Mode::Codex(Approval::OnRequest, Sandbox::ReadOnly)),
                "workspaceWrite" => Some(Mode::Codex(Approval::OnRequest, Sandbox::WorkspaceWrite)),
                "yolo" => Some(Mode::Codex(Approval::Never, Sandbox::DangerFullAccess)),
                _ => None,
            },
            _ => Mode::parse(provider, mode),
        });
        Self {
            provider: Some(provider),
            mode,
        }
    }

    /// A session whose mode cannot be confirmed, such as a plain shell.
    pub fn unknown() -> Self {
        Self {
            provider: None,
            mode: None,
        }
    }

    /// A provider CLI's mode as its terminal launched it. A person can change
    /// it in the terminal afterwards, as with Claude's Shift+Tab, which the
    /// launch does not show.
    #[cfg(feature = "host")]
    pub fn launched(recipe: &crate::model::LaunchRecipe) -> Self {
        let Some((provider, arguments)) = Provider::launched(recipe) else {
            return Self::unknown();
        };
        Self {
            provider: Some(provider),
            mode: launched_mode(provider, recipe, arguments),
        }
    }

    /// A native conversation's mode: the one its provider confirmed, or,
    /// while it has confirmed none cagents can rank, the stricter of asking
    /// and the mode it was launched in. A native Codex thread names only
    /// three of its approval and sandbox pairs, so a child started in
    /// another, such as `untrusted/read-only`, keeps that ceiling rather than
    /// rising to asking.
    #[cfg(feature = "host")]
    pub fn conversation(
        provider: Provider,
        confirmed: Option<&str>,
        recipe: &crate::model::LaunchRecipe,
    ) -> Self {
        let confirmed = Self::confirmed(provider, confirmed);
        if confirmed.mode.is_some() {
            return confirmed;
        }
        let launched = Self {
            provider: Some(provider),
            mode: launched_mode(provider, recipe, &recipe.arguments),
        };
        if launched.mode.is_some() && launched.tier() <= Tier::Ask {
            launched
        } else {
            confirmed
        }
    }

    pub fn tier(&self) -> Tier {
        self.mode.map_or(Tier::Ask, Mode::tier)
    }

    /// Whether the session may approve a tool request for an agent it
    /// started: only when it runs with nothing asking itself.
    pub fn may_approve(&self) -> bool {
        self.tier() == Tier::Full
    }

    /// How this session's mode reads in a refusal.
    fn describe(&self) -> String {
        match self.mode {
            Some(mode) => format!("This agent runs in {} mode", mode.name()),
            None => "This agent's own permission mode isn't confirmed, so it counts as asking for approval".into(),
        }
    }

    /// The refusal of an agent approving its child's tool request.
    pub fn approval_refusal(&self) -> String {
        format!(
            "{}, {} it can't approve its child's tool requests; answer it in cagents.",
            self.describe(),
            if self.mode.is_some() { "so" } else { "and" },
        )
    }

    /// The mode a child of `provider` launches in when this session asks for
    /// `requested`: the request itself when it is within this ceiling, and
    /// otherwise a refusal. An omitted mode is pinned: a child of the same
    /// provider inherits this session's mode, and any other gets the strictest
    /// mode of its provider within this session's tier, short of one where
    /// nothing asks. A Codex request without a sandbox takes this session's
    /// sandbox when it is a Codex session too, and that pinned mode's
    /// otherwise.
    pub fn child(&self, provider: Provider, requested: Option<&str>) -> Result<String, String> {
        let own = self.mode.filter(|_| self.provider == Some(provider));
        let Some(requested) = requested else {
            if let Some(own) = own {
                return Ok(own.name());
            }
            return self.pin(provider).map(Mode::name);
        };
        let unrankable = || {
            format!(
                "cagents can't compare {} mode {requested} with this agent's own, so an agent can't start a child in it; start it yourself.",
                provider.label()
            )
        };
        let mode = match provider {
            Provider::Codex => {
                let (approval, sandbox) = codex(requested).ok_or_else(unrankable)?;
                let sandbox = match (sandbox, own) {
                    (Some(sandbox), _) => sandbox,
                    (None, Some(Mode::Codex(_, own))) => own,
                    (None, _) => match self.pin(provider)? {
                        Mode::Codex(_, sandbox) => sandbox,
                        _ => unreachable!("a Codex pin is a Codex mode"),
                    },
                };
                Mode::Codex(approval, sandbox)
            }
            _ => Mode::parse(provider, requested).ok_or_else(unrankable)?,
        };
        let allowed = match own {
            Some(own) => mode.within(own),
            None => mode.tier() <= self.tier(),
        };
        if !allowed {
            let child = if Some(provider) == self.provider {
                String::new()
            } else {
                format!("{} ", provider.label())
            };
            return Err(format!(
                "{}, {} it can't start a {child}child in {}.",
                self.describe(),
                if self.mode.is_some() { "so" } else { "and" },
                if provider == Provider::Codex {
                    mode.name()
                } else {
                    requested.into()
                }
            ));
        }
        Ok(match provider {
            Provider::Codex => mode.name(),
            _ => requested.into(),
        })
    }

    /// The strictest mode of `provider` within this session's tier. An
    /// omitted mode never becomes one where nothing asks except by
    /// inheritance, so the pin stops at [`Tier::AutoEdit`].
    fn pin(&self, provider: Provider) -> Result<Mode, String> {
        let tier = self.tier().min(Tier::AutoEdit);
        let modes = Mode::strictest(provider);
        modes
            .iter()
            .rev()
            .map(|mode| mode.tier())
            .find(|candidate| *candidate <= tier)
            .and_then(|best| modes.iter().find(|mode| mode.tier() == best))
            .copied()
            .ok_or_else(|| {
                format!(
                    "{}, and {} has no mode that strict, so it can't start one.",
                    self.describe(),
                    provider.label(),
                )
            })
    }
}

/// The mode a provider CLI's command line and environment start it in, when
/// they name one cagents can rank.
#[cfg(feature = "host")]
fn launched_mode(
    provider: Provider,
    recipe: &crate::model::LaunchRecipe,
    arguments: &[String],
) -> Option<Mode> {
    let environment = |name: &str| {
        (!recipe.unset_environment.contains(name))
            .then(|| recipe.environment.get(name).cloned())
            .flatten()
    };
    let options = launched_options(provider, arguments)?;
    let last = |flags: &[&str]| {
        options
            .iter()
            .rev()
            .find(|(flag, _)| flags.contains(flag))
            .map(|(_, value)| *value)
    };
    let switch = |flags: &[&str]| options.iter().any(|(flag, _)| flags.contains(flag));
    match provider {
        Provider::Claude => {
            if switch(&["--dangerously-skip-permissions"]) {
                return Some(Mode::Claude(Claude::BypassPermissions));
            }
            let named = last(&["--permission-mode"])
                .flatten()
                .map(str::to_owned)
                .or_else(|| environment("CLAUDE_CODE_PERMISSION_MODE"));
            // Settings the command line names or leaves out decide the rest,
            // which the account's own settings then do not show.
            if named.is_none() && switch(&["--settings", "--setting-sources"]) {
                return None;
            }
            let named = named.or_else(|| crate::chat::claude_settings_default_mode(recipe))?;
            Mode::parse(provider, &named)
        }
        Provider::Codex => {
            if switch(&["--yolo", "--dangerously-bypass-approvals-and-sandbox"]) {
                return Some(Mode::Codex(Approval::Never, Sandbox::DangerFullAccess));
            }
            // A profile or configuration override may set either, so only
            // a command line naming both is known. The reasoning effort an
            // agent's launch names is the one override that sets neither.
            if switch(&["-p", "--profile"])
                || options.iter().any(|(flag, value)| {
                    matches!(*flag, "-c" | "--config")
                        && value
                            .and_then(crate::chat::fresh::codex_config_effort)
                            .is_none()
                })
            {
                return None;
            }
            let full_auto = switch(&["--full-auto"]);
            let approval = last(&["-a", "--ask-for-approval"])
                .map(|value| value.and_then(Approval::parse))
                .unwrap_or(full_auto.then_some(Approval::OnRequest))?;
            let sandbox = last(&["-s", "--sandbox"])
                .map(|value| value.and_then(Sandbox::parse))
                .unwrap_or(full_auto.then_some(Sandbox::WorkspaceWrite))?;
            Some(Mode::Codex(approval, sandbox))
        }
        Provider::Omp => {
            if switch(&["--auto-approve"]) {
                return Some(Mode::Omp(Omp::Yolo));
            }
            let named = last(&["--approval-mode"])
                .flatten()
                .map(str::to_owned)
                .or_else(|| environment("OMP_APPROVAL_MODE"))?;
            Mode::parse(provider, &named)
        }
        Provider::Antigravity | Provider::Muse => {
            // Its bypass is a switch beside the mode, which the mode does not
            // show.
            if switch(&["--dangerously-skip-permissions"]) {
                return None;
            }
            let acp = provider.acp()?;
            let named = last(&[acp.mode_option]).flatten();
            Mode::parse(
                provider,
                acp.mode_meaning(named.unwrap_or(acp.default_mode)),
            )
        }
    }
}

/// The options a provider command line passes, as `(flag, value)`, the last
/// of each counting as in each CLI. A positional argument, such as a prompt or
/// a subcommand, is skipped, and everything after `--` is the prompt's. An
/// option this does not know leaves unknown whether the next argument is its
/// value or an option of its own, so the whole command line is unknown.
#[cfg(feature = "host")]
fn launched_options(provider: Provider, arguments: &[String]) -> Option<Vec<(&str, Option<&str>)>> {
    use crate::chat::fresh::{ArgumentArity, claude_arity};
    let arity = |flag: &str| {
        let acp = provider.acp();
        if provider.is_switch(flag) || provider.is_latest_selector(flag) {
            Some(ArgumentArity::Switch)
        } else if provider.is_value_option(flag)
            || provider.is_cwd_option(flag)
            || acp.is_some_and(|acp| flag == acp.mode_option || flag == acp.effort_option)
        {
            Some(ArgumentArity::Value)
        } else if provider == Provider::Claude {
            claude_arity(flag).or(match flag {
                "-p" | "--print" | "--verbose" => Some(ArgumentArity::Switch),
                "--output-format" | "--input-format" => Some(ArgumentArity::Value),
                _ => None,
            })
        } else if provider == Provider::Codex && matches!(flag, "--full-auto" | "--yolo") {
            Some(ArgumentArity::Switch)
        } else if provider == Provider::Omp && flag == "--mode" {
            Some(ArgumentArity::Value)
        } else {
            None
        }
    };
    let mut options = Vec::new();
    let mut arguments = arguments.iter().map(String::as_str).peekable();
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            break;
        }
        if !argument.starts_with('-') || argument == "-" {
            continue;
        }
        let (flag, inline) = argument
            .split_once('=')
            .map_or((argument, None), |(flag, value)| (flag, Some(value)));
        // A conversation selector's value is optional, as in `claude --resume`.
        if provider.is_identity_option(flag) {
            if inline.is_none() {
                arguments.next_if(|next| !next.starts_with('-'));
            }
            continue;
        }
        let value = match (arity(flag)?, inline) {
            (_, Some(value)) => Some(value),
            (ArgumentArity::Switch, None) => None,
            (ArgumentArity::Value, None) => Some(arguments.next()?),
            (ArgumentArity::Values, None) => {
                let first = arguments.next_if(|next| !next.starts_with('-'));
                while arguments.next_if(|next| !next.starts_with('-')).is_some() {}
                first
            }
        };
        options.push((flag, value));
    }
    Some(options)
}

/// A session's ceiling as its own daemon states it to a linked device, which
/// then holds the session's capability there to it. The names are the ones
/// [`Mode`] reads, so the device ranks the same mode the session runs in.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatedCeiling {
    pub provider: Option<Provider>,
    pub mode: Option<String>,
}

impl StatedCeiling {
    /// The bytes a device proof signs for this ceiling, so that no one
    /// relaying the proof can raise it.
    pub fn signed_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(2 + self.mode.as_ref().map_or(0, String::len));
        bytes.push(self.provider.map_or(0, |provider| provider as u8 + 1));
        match &self.mode {
            Some(mode) => {
                bytes.push(1);
                bytes.extend_from_slice(mode.as_bytes());
            }
            None => bytes.push(0),
        }
        bytes
    }
}

impl Ceiling {
    /// This ceiling as a linked device is told it.
    pub fn stated(&self) -> StatedCeiling {
        StatedCeiling {
            provider: self.provider,
            mode: self.mode.map(Mode::name),
        }
    }

    /// The ceiling another daemon stated. A mode this one cannot read is
    /// refused rather than counted as asking, which could be more than the
    /// session has.
    pub fn from_stated(stated: &StatedCeiling) -> Result<Self, String> {
        let unreadable = || "The device could not read this session's permission mode".to_owned();
        let mode = match (&stated.mode, stated.provider) {
            (None, _) => None,
            (Some(_), None) => return Err(unreadable()),
            (Some(name), Some(provider)) => Some(match provider {
                // Named by meaning only, so an agent's own alias of one
                // meaning cannot be read as another.
                Provider::Antigravity | Provider::Muse => {
                    let meaning = *provider
                        .permission_modes()
                        .iter()
                        .find(|meaning| **meaning == name)
                        .ok_or_else(unreadable)?;
                    Mode::Acp(Claude::parse(meaning).ok_or_else(unreadable)?, meaning)
                }
                _ => Mode::parse(provider, name).ok_or_else(unreadable)?,
            }),
        };
        Ok(Self {
            provider: stated.provider,
            mode,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stated_ceiling_reads_back_as_the_same_ceiling() {
        let providers = [
            Provider::Omp,
            Provider::Claude,
            Provider::Codex,
            Provider::Antigravity,
            Provider::Muse,
        ];
        let mut ceilings = vec![Ceiling::unknown()];
        for provider in providers {
            ceilings.push(Ceiling {
                provider: Some(provider),
                mode: None,
            });
            for mode in Mode::strictest(provider) {
                ceilings.push(Ceiling {
                    provider: Some(provider),
                    mode: Some(mode),
                });
            }
        }
        for ceiling in ceilings {
            assert_eq!(
                Ceiling::from_stated(&ceiling.stated()).as_ref(),
                Ok(&ceiling),
                "{ceiling:?}"
            );
        }
        for unreadable in [
            StatedCeiling {
                provider: Some(Provider::Codex),
                mode: Some("never".into()),
            },
            StatedCeiling {
                provider: None,
                mode: Some("bypassPermissions".into()),
            },
            StatedCeiling {
                provider: Some(Provider::Claude),
                mode: Some("yolo".into()),
            },
        ] {
            assert!(Ceiling::from_stated(&unreadable).is_err(), "{unreadable:?}");
        }
    }

    #[test]
    fn every_rankable_mode_reaches_its_tier() {
        use Tier::*;
        for (provider, mode, tier) in [
            (Provider::Claude, "plan", ReadOnly),
            (Provider::Claude, "default", Ask),
            (Provider::Claude, "manual", Ask),
            (Provider::Claude, "acceptEdits", AutoEdit),
            (Provider::Claude, "bypassPermissions", Full),
            (Provider::Codex, "untrusted/read-only", ReadOnly),
            (Provider::Codex, "on-request/read-only", ReadOnly),
            (Provider::Codex, "on-failure/read-only", ReadOnly),
            (Provider::Codex, "never/read-only", ReadOnly),
            (Provider::Codex, "untrusted/workspace-write", Ask),
            (Provider::Codex, "on-request/workspace-write", AutoEdit),
            (Provider::Codex, "on-failure/workspace-write", AutoEdit),
            (Provider::Codex, "never/workspace-write", AutoEdit),
            (Provider::Codex, "untrusted/danger-full-access", Ask),
            (Provider::Codex, "on-request/danger-full-access", Full),
            (Provider::Codex, "on-failure/danger-full-access", Full),
            (Provider::Codex, "never/danger-full-access", Full),
            (Provider::Omp, "always-ask", Ask),
            (Provider::Omp, "yolo", Full),
            (Provider::Antigravity, "plan", ReadOnly),
            (Provider::Antigravity, "default", Ask),
            (Provider::Antigravity, "acceptEdits", AutoEdit),
            (Provider::Muse, "manual", Ask),
            (Provider::Muse, "untrusted", Ask),
            (Provider::Muse, "bypassPermissions", Full),
            (Provider::Muse, "never", Full),
        ] {
            assert_eq!(
                Mode::parse(provider, mode).map(Mode::tier),
                Some(tier),
                "{provider:?} {mode}"
            );
        }
    }

    #[test]
    fn modes_cagents_cannot_rank_are_not_modes() {
        for (provider, mode) in [
            (Provider::Claude, "auto"),
            (Provider::Claude, "dontAsk"),
            (Provider::Claude, "Default"),
            (Provider::Claude, ""),
            (Provider::Codex, "on-request"),
            (Provider::Codex, "yolo"),
            (Provider::Codex, "never/full"),
            (Provider::Codex, "sometimes/read-only"),
            (Provider::Omp, "write"),
            (Provider::Antigravity, "auto"),
            (Provider::Antigravity, "bypassPermissions"),
            (Provider::Muse, "default"),
            (Provider::Muse, "auto"),
            (Provider::Muse, "on-request"),
            (Provider::Muse, "dontAsk"),
            (Provider::Muse, "plan"),
        ] {
            assert_eq!(Mode::parse(provider, mode), None, "{provider:?} {mode}");
        }
    }

    /// Every mode this module names is one its provider takes, so a pinned
    /// or inherited mode always launches.
    #[test]
    fn every_named_mode_is_one_its_provider_takes() {
        for provider in Provider::ALL {
            for mode in Mode::strictest(provider) {
                assert_eq!(
                    provider.check_permission_mode(&mode.name()),
                    Ok(()),
                    "{provider:?} {mode:?}"
                );
            }
        }
        for sandbox in [
            Sandbox::ReadOnly,
            Sandbox::WorkspaceWrite,
            Sandbox::DangerFullAccess,
        ] {
            assert!(
                Provider::Codex
                    .permission_sandboxes()
                    .contains(&sandbox.name())
            );
        }
        assert_eq!(Provider::Codex.permission_sandboxes().len(), 3);
        // A Muse child of a Claude agent that asks is pinned to asking, and
        // a Muse agent's own mode is inherited by its own name.
        let claude = Ceiling::confirmed(Provider::Claude, Some("acceptEdits"));
        assert_eq!(claude.child(Provider::Muse, None).as_deref(), Ok("manual"));
        assert!(claude.child(Provider::Muse, Some("never")).is_err());
        assert_eq!(
            claude.child(Provider::Muse, Some("untrusted")).as_deref(),
            Ok("untrusted")
        );
        assert!(
            Ceiling::confirmed(Provider::Claude, Some("plan"))
                .child(Provider::Muse, None)
                .is_err()
        );
        let muse = Ceiling::confirmed(Provider::Muse, Some("bypassPermissions"));
        assert_eq!(muse.tier(), Tier::Full);
        assert_eq!(
            muse.child(Provider::Muse, None).as_deref(),
            Ok("bypassPermissions")
        );
    }

    #[test]
    fn native_conversations_confirm_their_modes() {
        for (provider, mode, tier) in [
            (Provider::Claude, Some("plan"), Tier::ReadOnly),
            (Provider::Claude, Some("acceptEdits"), Tier::AutoEdit),
            (Provider::Claude, Some("bypassPermissions"), Tier::Full),
            (Provider::Claude, Some("auto"), Tier::Ask),
            (Provider::Claude, Some("dontAsk"), Tier::Ask),
            (Provider::Claude, None, Tier::Ask),
            (Provider::Codex, Some("readOnly"), Tier::ReadOnly),
            (Provider::Codex, Some("workspaceWrite"), Tier::AutoEdit),
            (Provider::Codex, Some("yolo"), Tier::Full),
            (Provider::Codex, None, Tier::Ask),
            (Provider::Antigravity, Some("acceptEdits"), Tier::AutoEdit),
        ] {
            assert_eq!(
                Ceiling::confirmed(provider, mode).tier(),
                tier,
                "{provider:?} {mode:?}"
            );
        }
    }

    #[test]
    fn a_child_of_the_same_provider_compares_exactly() {
        let claude = |mode| Ceiling::confirmed(Provider::Claude, Some(mode));
        let child = |ceiling: &Ceiling, mode| ceiling.child(Provider::Claude, Some(mode));
        assert_eq!(child(&claude("default"), "plan").as_deref(), Ok("plan"));
        assert_eq!(child(&claude("default"), "manual").as_deref(), Ok("manual"));
        assert_eq!(
            child(&claude("default"), "bypassPermissions"),
            Err(
                "This agent runs in default mode, so it can't start a child in bypassPermissions."
                    .into()
            )
        );
        assert!(child(&claude("default"), "acceptEdits").is_err());
        assert!(child(&claude("acceptEdits"), "acceptEdits").is_ok());
        assert!(child(&claude("bypassPermissions"), "bypassPermissions").is_ok());
        for unrankable in ["auto", "dontAsk", "--dangerously-skip-permissions"] {
            assert_eq!(
                child(&claude("bypassPermissions"), unrankable),
                Err(format!(
                    "cagents can't compare Claude Code mode {unrankable} with this agent's own, so an agent can't start a child in it; start it yourself."
                ))
            );
        }

        // Codex compares the approval policy and the sandbox each.
        let codex = |mode| Ceiling::confirmed(Provider::Codex, Some(mode));
        let child = |ceiling: &Ceiling, mode| ceiling.child(Provider::Codex, Some(mode));
        assert_eq!(
            child(&codex("workspaceWrite"), "untrusted").as_deref(),
            Ok("untrusted/workspace-write")
        );
        assert_eq!(
            child(&codex("workspaceWrite"), "on-failure").as_deref(),
            Ok("on-failure/workspace-write")
        );
        assert_eq!(
            child(&codex("workspaceWrite"), "on-request/read-only").as_deref(),
            Ok("on-request/read-only")
        );
        assert_eq!(
            child(&codex("workspaceWrite"), "never"),
            Err("This agent runs in on-request/workspace-write mode, so it can't start a child in never/workspace-write.".into())
        );
        assert!(child(&codex("workspaceWrite"), "on-request/danger-full-access").is_err());
        // Never leaving the sandbox does not make a read-only one writable.
        assert!(child(&codex("readOnly"), "never").is_err());
        assert!(child(&codex("yolo"), "never/danger-full-access").is_ok());
    }

    #[test]
    fn an_omitted_mode_is_inherited_from_the_same_provider() {
        for (provider, mode, inherited) in [
            (Provider::Claude, "plan", "plan"),
            (Provider::Claude, "bypassPermissions", "bypassPermissions"),
            (Provider::Codex, "readOnly", "on-request/read-only"),
            (Provider::Codex, "yolo", "never/danger-full-access"),
        ] {
            assert_eq!(
                Ceiling::confirmed(provider, Some(mode))
                    .child(provider, None)
                    .as_deref(),
                Ok(inherited),
                "{provider:?} {mode}"
            );
        }
    }

    #[test]
    fn another_provider_compares_and_pins_on_the_tier() {
        let claude = |mode| Ceiling::confirmed(Provider::Claude, Some(mode));
        for (mode, pinned) in [
            ("plan", "untrusted/read-only"),
            ("default", "untrusted/workspace-write"),
            ("acceptEdits", "on-request/workspace-write"),
            // Only an inherited or explicit mode is one where nothing asks.
            ("bypassPermissions", "on-request/workspace-write"),
        ] {
            assert_eq!(
                claude(mode).child(Provider::Codex, None).as_deref(),
                Ok(pinned),
                "{mode}"
            );
        }
        assert_eq!(
            claude("default").child(Provider::Codex, Some("never")),
            Err("This agent runs in default mode, so it can't start a Codex child in never/workspace-write.".into())
        );
        assert_eq!(
            claude("default")
                .child(Provider::Codex, Some("untrusted"))
                .as_deref(),
            Ok("untrusted/workspace-write")
        );
        assert_eq!(
            claude("bypassPermissions")
                .child(Provider::Codex, Some("never/danger-full-access"))
                .as_deref(),
            Ok("never/danger-full-access")
        );
        assert_eq!(
            claude("plan")
                .child(Provider::Codex, Some("never"))
                .as_deref(),
            Ok("never/read-only")
        );

        let codex = |mode| Ceiling::confirmed(Provider::Codex, Some(mode));
        for (mode, pinned) in [
            ("readOnly", "plan"),
            ("workspaceWrite", "acceptEdits"),
            ("yolo", "acceptEdits"),
        ] {
            assert_eq!(
                codex(mode).child(Provider::Claude, None).as_deref(),
                Ok(pinned),
                "{mode}"
            );
        }
        assert!(
            codex("workspaceWrite")
                .child(Provider::Claude, Some("bypassPermissions"))
                .is_err()
        );
        assert!(
            codex("yolo")
                .child(Provider::Claude, Some("bypassPermissions"))
                .is_ok()
        );
        // Oh My Pi has nothing read-only to pin.
        assert_eq!(
            claude("plan").child(Provider::Omp, None),
            Err("This agent runs in plan mode, and Oh My Pi has no mode that strict, so it can't start one.".into())
        );
    }

    #[test]
    fn a_session_of_unknown_mode_counts_as_asking() {
        let unknown = Ceiling::unknown();
        assert_eq!(unknown.tier(), Tier::Ask);
        assert!(!unknown.may_approve());
        assert_eq!(
            unknown.child(Provider::Claude, None).as_deref(),
            Ok("default")
        );
        assert_eq!(
            unknown.child(Provider::Codex, None).as_deref(),
            Ok("untrusted/workspace-write")
        );
        assert_eq!(
            unknown.child(Provider::Claude, Some("acceptEdits")),
            Err("This agent's own permission mode isn't confirmed, so it counts as asking for approval, and it can't start a Claude Code child in acceptEdits.".into())
        );
        assert!(unknown.child(Provider::Claude, Some("plan")).is_ok());
    }

    #[test]
    fn only_a_session_where_nothing_asks_may_approve() {
        for (provider, mode, approves) in [
            (Provider::Claude, "default", false),
            (Provider::Claude, "acceptEdits", false),
            (Provider::Claude, "bypassPermissions", true),
            (Provider::Codex, "workspaceWrite", false),
            (Provider::Codex, "yolo", true),
        ] {
            assert_eq!(
                Ceiling::confirmed(provider, Some(mode)).may_approve(),
                approves,
                "{provider:?} {mode}"
            );
        }
        assert_eq!(
            Ceiling::confirmed(Provider::Claude, Some("default")).approval_refusal(),
            "This agent runs in default mode, so it can't approve its child's tool requests; answer it in cagents."
        );
    }

    #[test]
    fn a_codex_mode_launches_as_its_two_options() {
        assert_eq!(codex_launch("never"), ("never", None));
        assert_eq!(
            codex_launch("untrusted/workspace-write"),
            ("untrusted", Some("workspace-write"))
        );
        assert_eq!(codex_launch("never/everything"), ("never/everything", None));
    }

    #[cfg(feature = "host")]
    #[test]
    fn a_terminal_launch_names_its_mode() {
        let recipe = |program: &str, arguments: &[&str]| crate::model::LaunchRecipe {
            program: program.into(),
            arguments: arguments.iter().map(|&argument| argument.into()).collect(),
            working_directory: "/".into(),
            environment: [("CLAUDE_CODE_PERMISSION_MODE".into(), "plan".into())].into(),
            unset_environment: Default::default(),
        };
        let cases: [(&str, &[&str], Tier); 22] = [
            ("claude", &["--dangerously-skip-permissions"], Tier::Full),
            (
                "claude",
                &["--permission-mode", "acceptEdits"],
                Tier::AutoEdit,
            ),
            (
                "claude",
                &["--permission-mode=bypassPermissions"],
                Tier::Full,
            ),
            ("claude", &[], Tier::ReadOnly),
            ("claude", &["--permission-mode", "auto"], Tier::Ask),
            ("codex", &["--yolo"], Tier::Full),
            ("codex", &["--full-auto"], Tier::AutoEdit),
            ("codex", &["-a", "never", "-s", "read-only"], Tier::ReadOnly),
            (
                "codex",
                &["--ask-for-approval=untrusted", "--sandbox=workspace-write"],
                Tier::Ask,
            ),
            // The configuration chooses what the command line leaves out.
            ("codex", &["-a", "never"], Tier::Ask),
            (
                "codex",
                &[
                    "-a",
                    "never",
                    "-s",
                    "read-only",
                    "-c",
                    "sandbox_mode=danger-full-access",
                ],
                Tier::Ask,
            ),
            ("omp", &["--auto-approve"], Tier::Full),
            ("omp", &["--approval-mode", "always-ask"], Tier::Ask),
            ("bash", &[], Tier::Ask),
            // A prompt or an option's value is not an option, even when it
            // reads like one.
            (
                "codex",
                &["-a", "untrusted", "-s", "workspace-write", "--", "--yolo"],
                Tier::Ask,
            ),
            (
                "claude",
                &["--", "--dangerously-skip-permissions"],
                Tier::ReadOnly,
            ),
            (
                "claude",
                &["--append-system-prompt", "--dangerously-skip-permissions"],
                Tier::ReadOnly,
            ),
            (
                "claude",
                &["--add-dir", "a", "b", "--permission-mode", "acceptEdits"],
                Tier::AutoEdit,
            ),
            // A conversation selector's value is optional.
            (
                "claude",
                &["--resume", "--permission-mode", "acceptEdits"],
                Tier::AutoEdit,
            ),
            (
                "codex",
                &["resume", "thread", "-a", "never", "-s", "read-only"],
                Tier::ReadOnly,
            ),
            // The reasoning effort an agent's launch names sets neither.
            (
                "codex",
                &[
                    "-a",
                    "untrusted",
                    "-s",
                    "read-only",
                    "--config",
                    "model_reasoning_effort=low",
                ],
                Tier::ReadOnly,
            ),
            // An option of unknown arity leaves what follows it unknown.
            (
                "claude",
                &["--unknown", "--permission-mode", "plan"],
                Tier::Ask,
            ),
        ];
        for (program, arguments, tier) in cases {
            assert_eq!(
                Ceiling::launched(&recipe(program, arguments)).tier(),
                tier,
                "{program} {arguments:?}"
            );
        }

        // Settings the command line chooses or leaves out are not the
        // account's, which here would bypass permissions.
        let config = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join("settings.json"),
            r#"{"permissions":{"defaultMode":"bypassPermissions"}}"#,
        )
        .unwrap();
        let account = |arguments: &[&str]| crate::model::LaunchRecipe {
            program: "claude".into(),
            arguments: arguments.iter().map(|&argument| argument.into()).collect(),
            working_directory: config.path().into(),
            environment: [(
                "CLAUDE_CONFIG_DIR".into(),
                config.path().display().to_string(),
            )]
            .into(),
            unset_environment: ["CLAUDE_CODE_PERMISSION_MODE".into()].into(),
        };
        for (arguments, tier) in [
            (&[][..], Tier::Full),
            (&["--setting-sources", ""][..], Tier::Ask),
            (&["--settings", "strict.json"][..], Tier::Ask),
            (
                &["--setting-sources", "", "--permission-mode", "plan"][..],
                Tier::ReadOnly,
            ),
        ] {
            assert_eq!(
                Ceiling::launched(&account(arguments)).tier(),
                tier,
                "{arguments:?}"
            );
        }
    }

    /// A native conversation names only the modes its provider confirms, so
    /// while it confirms none cagents ranks, its launch caps it below asking.
    #[cfg(feature = "host")]
    #[test]
    fn an_unconfirmed_conversation_keeps_a_strict_launch() {
        let recipe = |arguments: &[&str]| crate::model::LaunchRecipe {
            program: "codex".into(),
            arguments: arguments.iter().map(|&argument| argument.into()).collect(),
            working_directory: "/".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        // As an agent's launch names it, with a reasoning effort.
        let read_only = recipe(&[
            "--config",
            "model_reasoning_effort=low",
            "--ask-for-approval",
            "untrusted",
            "--sandbox",
            "read-only",
        ]);
        let ceiling = Ceiling::conversation(Provider::Codex, None, &read_only);
        assert_eq!(ceiling.tier(), Tier::ReadOnly);
        assert_eq!(
            ceiling.child(Provider::Codex, None).as_deref(),
            Ok("untrusted/read-only")
        );
        assert!(
            ceiling
                .child(Provider::Codex, Some("untrusted/workspace-write"))
                .is_err()
        );
        // A launch looser than asking is capped at asking.
        let full = recipe(&["-a", "never", "-s", "danger-full-access"]);
        assert_eq!(
            Ceiling::conversation(Provider::Codex, None, &full).tier(),
            Tier::Ask
        );
        // A confirmed mode is the provider's word, whatever the launch said.
        assert_eq!(
            Ceiling::conversation(Provider::Codex, Some("workspaceWrite"), &read_only).tier(),
            Tier::AutoEdit
        );
    }
}
