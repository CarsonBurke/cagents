//! Exact, opt-in agent recovery evidence. Never searches a session directory or
//! selects a recent conversation; unknown launches retain their shell fallback.
#[cfg(feature = "host")]
use std::path::Path;
#[cfg(feature = "host")]
use std::{fs, io::Read};

use serde::{Deserialize, Serialize};

use super::LaunchRecipe;
use crate::agent::Provider;

const MAX_HINT_BYTES: usize = 16 * 1024;
const MAX_ARGV_BYTES: usize = 8 * 1024;

/// Host-only activity evidence, separate from normalized recovery authority.
#[cfg(feature = "host")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentProcess {
    pub pid: i32,
    pub start: u64,
    pub provider: Provider,
    pub launch: LaunchRecipe,
    pub recovery: LaunchRecipe,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentResumeHint {
    version: u8,
    recipe: LaunchRecipe,
}

/// Accept only complete UUIDs, never the CLI's prefix/name/picker shortcuts.
fn exact_id(value: &str) -> bool {
    value.len() == 36 && uuid::Uuid::parse_str(value).is_ok()
}

/// Rebuild an interactive resume invocation without replaying an initial prompt,
/// forking a conversation, or carrying an implicit latest-session selector.
pub fn resume_recipe(launch: &LaunchRecipe) -> Option<LaunchRecipe> {
    interactive_recipe(launch, Selector::Exact)
}

/// A native conversation already records its provider independently of its
/// executable. Keep a configured wrapper's name without inferring providers
/// from arbitrary shell commands. The same conservative argv policy applies.
pub(crate) fn resume_recipe_for_provider(
    launch: &LaunchRecipe,
    provider: Provider,
) -> Option<LaunchRecipe> {
    interactive_recipe_for_provider(launch, Selector::Exact, provider)
}

/// Start an independent conversation with the same supported configuration,
/// never a second writer to the source conversation or a replayed prompt.
pub fn fresh_recipe(launch: &LaunchRecipe) -> Option<LaunchRecipe> {
    let mut recipe = interactive_recipe(launch, Selector::Fresh)?;
    if Provider::from_program(&recipe.program)? == Provider::Omp
        && !recipe.arguments.iter().any(|arg| arg == "--session-dir")
    {
        let root = Provider::Omp.session_root(&recipe)?;
        recipe.arguments.extend([
            "--session-dir".into(),
            root.join("sessions")
                .join("native")
                .to_string_lossy()
                .into_owned(),
        ]);
    }
    Some(recipe)
}

/// Reopen the provider's own conversation picker with the same supported
/// configuration. This recovers a recognised agent whose exact conversation
/// was never established: nothing is guessed, resumed or replayed, and the
/// user chooses the conversation - or a new one - themselves.
///
/// Every selector the launch carried is dropped, whether an exact ID, a picker
/// search term or a latest-session shortcut, since the picker supersedes it.
pub fn picker_recipe(launch: &LaunchRecipe) -> Option<LaunchRecipe> {
    interactive_recipe(launch, Selector::Picker)
}

/// Whether `recipe` is exactly the picker `picker_recipe` builds for it.
pub fn is_picker_recipe(recipe: &LaunchRecipe) -> bool {
    picker_recipe(recipe).as_ref() == Some(recipe)
}

/// Which conversation an interactive recipe rebuilt from a launch selects.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Selector {
    /// None: the provider starts a new conversation.
    Fresh,
    /// The exact conversation the launch names, which it must name.
    Exact,
    /// The provider's picker, superseding any selector the launch carried.
    Picker,
}

fn interactive_recipe(launch: &LaunchRecipe, selector: Selector) -> Option<LaunchRecipe> {
    interactive_recipe_for_provider(launch, selector, Provider::from_program(&launch.program)?)
}

fn interactive_recipe_for_provider(
    launch: &LaunchRecipe,
    selector: Selector,
    provider: Provider,
) -> Option<LaunchRecipe> {
    if launch.arguments.len() > 128
        || launch.arguments.iter().map(String::len).sum::<usize>() > MAX_ARGV_BYTES
    {
        return None;
    }
    let mut id = None;
    let mut arguments = Vec::new();
    let mut working_directory = launch.working_directory.clone();
    let mut args = launch.arguments.iter().peekable();
    while let Some(argument) = args.next() {
        let (flag, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(flag, value)| {
                (flag, Some(value))
            });
        if selector == Selector::Picker {
            let latest = match provider {
                Provider::Codex => flag == "--last",
                _ => matches!(flag, "--continue" | "-c"),
            };
            if latest && inline.is_none() {
                continue;
            }
            if provider.is_identity_option(flag) {
                // Claude's and OMP's selectors take an optional value, Codex's
                // subcommand an optional positional ID or name: whichever it
                // is, the picker supersedes it.
                if inline.is_none() {
                    args.next_if(|value| !value.starts_with('-'));
                }
                continue;
            }
        }
        if provider.is_cwd_option(flag) {
            let value = inline.or_else(|| args.next().map(String::as_str))?;
            if value.is_empty() || value.starts_with('-') || value.contains('\0') {
                return None;
            }
            working_directory = launch.working_directory.join(value);
            continue;
        }
        if provider.is_identity_option(flag) {
            let value = inline.or_else(|| args.next().map(String::as_str))?;
            if id.is_some() || !exact_id(value) {
                return None;
            }
            id = Some(value.to_owned());
            continue;
        }
        if provider.is_value_option(flag) {
            let value = inline.or_else(|| args.next().map(String::as_str))?;
            // Claude explicitly uses an empty source list to disable settings.
            // Dropping it would re-enable configuration on native/CLI resume.
            if (value.is_empty() && !(provider == Provider::Claude && flag == "--setting-sources"))
                || value.starts_with('-')
                || value.contains('\0')
            {
                return None;
            }
            arguments.extend([flag.to_owned(), value.to_owned()]);
        } else if provider.is_switch(flag) && inline.is_none() {
            arguments.push(flag.to_owned());
        } else {
            // Unknown flags or positional prompts may change session identity.
            return None;
        }
    }
    match selector {
        Selector::Fresh => {}
        Selector::Exact => provider.append_resume(&mut arguments, id?),
        Selector::Picker => provider.append_resume_picker(&mut arguments),
    }
    Some(LaunchRecipe {
        arguments,
        working_directory,
        ..launch.clone()
    })
}

/// Bind provider-confirmed identity while discarding already-consumed prompts.
/// Unknown options remain a hard refusal, especially permission/account flags.
pub fn bind_session(launch: &LaunchRecipe, id: &str) -> Option<LaunchRecipe> {
    if !exact_id(id) {
        return None;
    }
    let provider = Provider::from_program(&launch.program)?;
    let mut recipe = launch.clone();
    let mut arguments = Vec::new();
    let mut args = launch.arguments.iter().peekable();
    while let Some(argument) = args.next() {
        let (flag, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(flag, value)| {
                (flag, Some(value))
            });
        if matches!(flag, "--continue" | "-c") && provider != Provider::Codex {
            continue;
        }
        if provider.is_identity_option(flag) {
            if inline.is_none() && args.peek().is_some_and(|value| !value.starts_with('-')) {
                args.next();
            }
        } else if provider.is_cwd_option(flag) || provider.is_value_option(flag) {
            arguments.push(argument.clone());
            if inline.is_none() {
                arguments.push(args.next()?.clone());
            }
        } else if provider.is_switch(flag) && inline.is_none() {
            arguments.push(argument.clone());
        } else if flag.starts_with('-') {
            return None;
        }
        // Positional text/images were consumed by the live CLI, not replayed.
    }
    provider.append_resume(&mut arguments, id.to_owned());
    recipe.arguments = arguments;
    resume_recipe(&recipe)
}

pub fn hint_from_launch(launch: &LaunchRecipe) -> Option<String> {
    let recipe = resume_recipe(launch)?;
    let hint = serde_json::to_string(&AgentResumeHint { version: 1, recipe }).ok()?;
    (hint.len() <= MAX_HINT_BYTES).then_some(hint)
}

pub fn recipe_from_hint(hint: &str) -> Option<LaunchRecipe> {
    if hint.len() > MAX_HINT_BYTES {
        return None;
    }
    let hint: AgentResumeHint = serde_json::from_str(hint).ok()?;
    (hint.version == 1).then_some(())?;
    resume_recipe(&hint.recipe)
}

/// Recognition is not recovery authority. Fresh foreground agents expose a
/// native-chat action while exact process-bound evidence is resolved separately.
pub fn observed_hint_from_launch(launch: &LaunchRecipe) -> Option<String> {
    if let Some(hint) = hint_from_launch(launch) {
        return Some(hint);
    }
    Provider::from_program(&launch.program)?;
    if launch.arguments.len() > 128
        || launch.arguments.iter().map(String::len).sum::<usize>() > MAX_ARGV_BYTES
    {
        return None;
    }
    let hint = serde_json::to_string(&AgentResumeHint {
        version: 2,
        recipe: launch.clone(),
    })
    .ok()?;
    (hint.len() <= MAX_HINT_BYTES).then_some(hint)
}

pub fn observed_launch_from_hint(hint: &str) -> Option<LaunchRecipe> {
    if hint.len() > MAX_HINT_BYTES {
        return None;
    }
    let hint: AgentResumeHint = serde_json::from_str(hint).ok()?;
    match hint.version {
        1 => resume_recipe(&hint.recipe),
        2 => (Provider::from_program(&hint.recipe.program).is_some()
            && hint.recipe.arguments.len() <= 128
            && hint.recipe.arguments.iter().map(String::len).sum::<usize>() <= MAX_ARGV_BYTES)
            .then_some(hint.recipe),
        _ => None,
    }
}

#[cfg(feature = "host")]
fn bounded_read(path: impl AsRef<Path>, limit: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

pub(crate) fn process_identity(stat: &[u8]) -> Option<(i32, u64)> {
    let stat = std::str::from_utf8(stat).ok()?;
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let group = fields.nth(2)?.parse().ok()?;
    Some((group, fields.nth(16)?.parse().ok()?))
}

/// Cheap liveness check for a previously discovered process. This is not launch
/// or conversation evidence: exec can retain PID/start/group, so changed facts
/// and recovery still require the full foreground observation.
#[cfg(feature = "host")]
pub(crate) fn process_in_foreground(process: &AgentProcess, group: i32) -> bool {
    process.pid > 0
        && group > 0
        && bounded_read(format!("/proc/{}/stat", process.pid), 4096)
            .and_then(|stat| process_identity(&stat))
            == Some((group, process.start))
}

#[cfg(feature = "host")]
#[derive(Debug, PartialEq, Eq)]
enum SessionIdentity {
    Exact(String),
    /// The provider explicitly published a never-materialized session boundary.
    Fresh(std::path::PathBuf),
    Ambiguous,
}

#[cfg(feature = "host")]
fn runtime_identity(
    root: &Path,
    pid: i32,
    launch: &LaunchRecipe,
    start: u64,
) -> Option<SessionIdentity> {
    let provider = Provider::from_program(&launch.program)?;
    let session_root = provider.session_root(launch)?;
    if provider == Provider::Omp
        && let Some(identity) = omp_terminal_identity(root, launch, start)
    {
        return Some(identity);
    }
    if provider == Provider::Claude
        && let Some(identity) = claude_process_identity(&session_root, pid, launch, start)
    {
        return Some(identity);
    }
    let mut identity = None;
    // The kernel reports open files by their resolved path, and a session root
    // is often a symbolic link, such as a dotfiles checkout linked into HOME.
    let expected = session_root.join(provider.session_directory());
    let expected = fs::canonicalize(&expected).unwrap_or(expected);
    let entries = match fs::read_dir(root.join("fd")) {
        Ok(entries) => entries,
        Err(_) => return Some(SessionIdentity::Ambiguous),
    };
    for (index, entry) in entries.enumerate() {
        if index == 256 {
            return Some(SessionIdentity::Ambiguous);
        }
        let Ok(entry) = entry else {
            return Some(SessionIdentity::Ambiguous);
        };
        let Ok(path) = fs::read_link(entry.path()) else {
            return Some(SessionIdentity::Ambiguous);
        };
        if !path.starts_with(&expected) {
            continue;
        }
        let Some(info) = bounded_read(root.join("fdinfo").join(entry.file_name()), 4096) else {
            return Some(SessionIdentity::Ambiguous);
        };
        let Some(flags) = std::str::from_utf8(&info).ok().and_then(|info| {
            info.lines()
                .find_map(|line| line.strip_prefix("flags:"))
                .and_then(|flags| u32::from_str_radix(flags.trim(), 8).ok())
        }) else {
            return Some(SessionIdentity::Ambiguous);
        };
        if flags & libc::O_ACCMODE as u32 == libc::O_RDONLY as u32 {
            continue;
        }
        if let Some(id) = provider.session_file_id(&path) {
            if identity.as_ref().is_some_and(|previous| previous != &id) {
                return Some(SessionIdentity::Ambiguous);
            }
            identity = Some(id);
        }
    }
    identity.map(SessionIdentity::Exact)
}

/// The conversation Claude Code says process `pid` holds.
///
/// Claude does not keep its transcript open between writes, so the open-file
/// scan cannot find it. It instead publishes each interactive process's live
/// conversation in `sessions/<pid>.json`, which follows `/resume` and `/clear`
/// where the process's argv does not. The record counts only for the process
/// that wrote it, told apart from a later one with the same pid by its start
/// time. Once it does, it outranks the argv: a conversation with no transcript
/// yet, or one a parked background job now owns, cannot be resumed here.
#[cfg(feature = "host")]
fn claude_process_identity(
    session_root: &Path,
    pid: i32,
    launch: &LaunchRecipe,
    start: u64,
) -> Option<SessionIdentity> {
    // Claude refuses a relative configuration directory, so one never holds
    // this process's record.
    if !session_root.is_absolute() {
        return None;
    }
    let record = bounded_read(
        session_root.join("sessions").join(format!("{pid}.json")),
        MAX_HINT_BYTES,
    )?;
    let record: serde_json::Value = serde_json::from_slice(&record).ok()?;
    let started = record["procStart"]
        .as_str()
        .and_then(|start| start.parse().ok());
    if record["pid"].as_i64() != Some(i64::from(pid)) || started != Some(start) {
        return None;
    }
    let identity = (|| {
        if record["kind"] != "interactive" || !record["parkedJobId"].is_null() {
            return None;
        }
        let id = record["sessionId"].as_str().filter(|id| exact_id(id))?;
        let cwd = fs::canonicalize(&launch.working_directory).ok()?;
        crate::agent::claude_transcript(session_root, &cwd, id)
            .ok()
            .filter(|transcript| transcript.is_file())
            .map(|_| SessionIdentity::Exact(id.to_owned()))
    })();
    Some(identity.unwrap_or(SessionIdentity::Ambiguous))
}

/// When, by the wall clock, a process started that started `start` clock
/// ticks after boot, as `/proc/<pid>/stat` states it.
#[cfg(feature = "host")]
pub(crate) fn process_started(start: u64) -> Option<std::time::SystemTime> {
    // SAFETY: sysconf has no pointer arguments; CLOCK_BOOTTIME writes to ts.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if ticks <= 0 || unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
        return None;
    }
    let uptime = std::time::Duration::new(ts.tv_sec.try_into().ok()?, ts.tv_nsec.try_into().ok()?);
    let age = uptime.checked_sub(std::time::Duration::from_secs_f64(
        start as f64 / ticks as f64,
    ))?;
    std::time::SystemTime::now().checked_sub(age)
}

#[cfg(feature = "host")]
fn omp_terminal_identity(
    root: &Path,
    launch: &LaunchRecipe,
    start: u64,
) -> Option<SessionIdentity> {
    let provider = Provider::Omp;
    let session_root = provider.session_root(launch)?;
    // OMP publishes the exact active session by controlling terminal, including
    // fresh launches whose argv has no --resume. Never enumerate saved sessions.
    let tty = fs::read_link(root.join("fd/0")).ok()?;
    let tty = tty.strip_prefix("/dev/pts").ok()?.file_name()?.to_str()?;
    let marker = provider
        .session_state_root(launch)?
        .join("terminal-sessions")
        .join(format!("pts-{tty}"));
    let modified = fs::metadata(&marker).ok()?.modified().ok()?;
    if modified < process_started(start)? {
        return None;
    }
    let identity = (|| {
        let bytes = bounded_read(&marker, MAX_HINT_BYTES)?;
        let text = std::str::from_utf8(&bytes).ok()?;
        let mut lines = text.lines();
        if Path::new(lines.next()?) != launch.working_directory {
            return None;
        }
        let session = Path::new(lines.next()?);
        let mut session_directory = session_root.join("sessions");
        for (index, arg) in launch.arguments.iter().enumerate() {
            if let Some(directory) = arg.strip_prefix("--session-dir=").or_else(|| {
                (arg == "--session-dir")
                    .then(|| launch.arguments.get(index + 1).map(String::as_str))
                    .flatten()
            }) {
                session_directory = launch.working_directory.join(directory);
            }
        }
        // Whether OMP records the directory as named or resolved is its own
        // choice, so a linked session root is accepted either way.
        let resolved = fs::canonicalize(&session_directory).ok();
        (session.starts_with(&session_directory)
            || resolved.is_some_and(|resolved| session.starts_with(resolved)))
        .then_some(())?;
        if session
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return None;
        }
        let id = provider.session_file_id(session)?;
        match fs::metadata(session) {
            Ok(metadata) if metadata.is_file() && metadata.len() > 0 => {
                Some(SessionIdentity::Exact(id))
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && lines.next() == Some("fresh")
                    && lines.next().is_none() =>
            {
                Some(SessionIdentity::Fresh(session.to_owned()))
            }
            _ => None,
        }
    })();
    // A current but unusable marker must block stale resume argv and open files.
    Some(identity.unwrap_or(SessionIdentity::Ambiguous))
}

/// Absence of a saved file alone is never evidence of a fresh conversation.
/// Only the exact, current controlling-terminal marker can authorize this path.
#[cfg(feature = "host")]
pub(crate) fn fresh_omp_session(process: &AgentProcess) -> Option<std::path::PathBuf> {
    if process.provider != Provider::Omp {
        return None;
    }
    let root = std::path::PathBuf::from(format!("/proc/{}", process.pid));
    if process_identity(&bounded_read(root.join("stat"), 4096)?)?.1 != process.start {
        return None;
    }
    match omp_terminal_identity(&root, &process.recovery, process.start)? {
        SessionIdentity::Fresh(path) => Some(path),
        _ => None,
    }
}

#[cfg(any(feature = "host", feature = "ui"))]
fn observation_environment_key(name: &str) -> bool {
    matches!(
        name,
        "HOME"
            | "PATH"
            | "CODEX_HOME"
            | "CLAUDE_CONFIG_DIR"
            | "PI_CODING_AGENT_DIR"
            | "OMP_PROFILE"
            | "PI_PROFILE"
            | "PI_CONFIG_DIR"
            | "XDG_CONFIG_HOME"
            | "XDG_DATA_HOME"
            | "XDG_STATE_HOME"
            | "XDG_CACHE_HOME"
    )
}

#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn native_runtime_environment_key(name: &str) -> bool {
    observation_environment_key(name)
        || matches!(
            name,
            "LANG"
                | "LANGUAGE"
                | "USER"
                | "LOGNAME"
                | "SHELL"
                | "TMPDIR"
                | "TMP"
                | "TEMP"
                | "LC_ALL"
                | "LC_CTYPE"
                | "LC_MESSAGES"
                | "LC_COLLATE"
                | "LC_MONETARY"
                | "LC_NUMERIC"
                | "LC_TIME"
                | "LC_PAPER"
                | "LC_NAME"
                | "LC_ADDRESS"
                | "LC_TELEPHONE"
                | "LC_MEASUREMENT"
                | "LC_IDENTIFICATION"
                | "XDG_CONFIG_HOME"
                | "XDG_RUNTIME_DIR"
                | "DISPLAY"
                | "WAYLAND_DISPLAY"
                | "BUN_INSTALL"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
                | "NODE_EXTRA_CA_CERTS"
                | "LD_LIBRARY_PATH"
                | "DBUS_SESSION_BUS_ADDRESS"
                | "SSH_AUTH_SOCK"
                | "SSH_AGENT_PID"
        )
}

#[cfg(any(feature = "host", feature = "ui"))]
fn provider_setting_environment_key(provider: Provider, name: &str) -> bool {
    let starts_with = |prefix: &str| {
        name.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    };
    // CLI-specific tuning from a different agent is not this provider's policy.
    // Shared model backends (ANTHROPIC, OPENAI, AWS, ...) remain relevant to OMP.
    if [
        ("CLAUDE_", Provider::Claude),
        ("CODEX_", Provider::Codex),
        ("OMP_", Provider::Omp),
        ("PI_", Provider::Omp),
    ]
    .iter()
    .any(|(prefix, owner)| *owner != provider && starts_with(prefix))
    {
        return false;
    }
    const PREFIXES: &[&str] = &[
        "ANTHROPIC",
        "CLAUDE",
        "CODEX",
        "OMP_",
        "PI_",
        "OPENAI",
        "AZURE",
        "AWS_",
        "GOOGLE_",
        "GCLOUD",
        "GEMINI",
        "VERTEX",
        "BEDROCK",
        "FOUNDRY",
        "COPILOT",
        "GROQ",
        "CEREBRAS",
        "XAI_",
        "OPENROUTER",
        "KILO_",
        "MISTRAL",
        "ZAI_",
        "UMANS",
        "MINIMAX",
        "OPENCODE",
        "CURSOR_",
        "CLINE",
        "COMMAND_CODE",
        "AI_GATEWAY",
        "WAFER",
        "YOLO_",
        "NODE_",
        "BUN_",
        "SSL_",
        "CURL_",
        "GIT_CONFIG",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ];
    let prefixes: &[&str] = match provider {
        Provider::Omp => PREFIXES,
        Provider::Claude => &[
            "ANTHROPIC",
            "CLAUDE",
            "AWS_",
            "GOOGLE_",
            "GCLOUD",
            "VERTEX",
            "BEDROCK",
            "FOUNDRY",
            "AZURE",
        ],
        Provider::Codex => &["CODEX", "OPENAI"],
    };
    if prefixes.iter().any(|prefix| starts_with(prefix)) {
        return true;
    }
    if [
        "NODE_",
        "BUN_",
        "SSL_",
        "CURL_",
        "GIT_CONFIG",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "LD_PRELOAD",
    ]
    .iter()
    .any(|prefix| starts_with(prefix))
    {
        return true;
    }
    // An arbitrary credential-shaped name is not proof that this provider uses
    // it. Never copy unrelated application secrets into a newly launched agent.
    false
}

#[cfg(any(feature = "host", feature = "ui"))]
fn credential_environment_key(name: &str) -> bool {
    [
        "SECRET",
        "PASSWORD",
        "API_KEY",
        "PRIVATE_KEY",
        "CREDENTIAL",
        "AUTHENTICATION",
        "AUTHORIZATION",
    ]
    .iter()
    .any(|part| {
        name.as_bytes()
            .windows(part.len())
            .any(|window| window.eq_ignore_ascii_case(part.as_bytes()))
    }) || name
        .split('_')
        .any(|part| part.eq_ignore_ascii_case("AUTH") || part.eq_ignore_ascii_case("TOKEN"))
}

#[cfg(any(feature = "host", feature = "ui"))]
fn indirect_environment_key(name: &str) -> bool {
    // Git's opaque config payloads may contain extraHeader credentials, helpers,
    // or URL rewrites. Classify the namespace, never the payload's spelling.
    name.get(..10)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("GIT_CONFIG"))
}

#[cfg(feature = "host")]
fn sensitive_environment_key(name: &str) -> bool {
    credential_environment_key(name)
        || indirect_environment_key(name)
        || [
            "BASE_URL",
            "ENDPOINT",
            "PERMISSION",
            "APPROVAL",
            "SANDBOX",
            "BYPASS",
            "AUTO_APPROVE",
            "YOLO",
        ]
        .iter()
        .any(|part| {
            name.as_bytes()
                .windows(part.len())
                .any(|window| window.eq_ignore_ascii_case(part.as_bytes()))
        })
}

#[cfg(any(feature = "host", feature = "ui"))]
fn persistable_environment_entry(name: &str, value: &str) -> bool {
    if credential_environment_key(name) || indirect_environment_key(name) {
        return false;
    }
    // A proxy/base URL can contain credentials even when its variable name does
    // not. Keep those values only in the host's ephemeral execution environment.
    !url::Url::parse(value).is_ok_and(|url| {
        !url.username().is_empty() || url.password().is_some() || url.query().is_some()
    })
}

/// A fresh native process uses the selected provider's configuration, never the
/// launching harness's tool bridge, artifact scope, or terminal/session identity.
#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn fresh_environment_key(
    provider: Provider,
    name: &str,
    value: &str,
    persistable: bool,
) -> bool {
    if [
        "PI_TOOL_BRIDGE_",
        "PI_EVAL_",
        "PI_SESSION_",
        "PI_AGENT_",
        "OMP_SESSION_",
        "OMP_AGENT_",
        "CAGENTS_",
        "ORCA_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
        || name == "PI_ARTIFACTS_DIR"
    {
        return false;
    }
    if !native_runtime_environment_key(name) && !provider_setting_environment_key(provider, name) {
        return false;
    }
    !persistable || persistable_environment_entry(name, value)
}

#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn fresh_native_environment(
    provider: Provider,
    persistable: bool,
) -> std::collections::BTreeMap<String, String> {
    if crate::device::is_remote() {
        return crate::device::environment(provider, persistable);
    }
    std::env::vars()
        .filter(|(name, value)| fresh_environment_key(provider, name, value, persistable))
        .collect()
}

#[cfg(feature = "host")]
fn return_environment_key(provider: Provider, name: &str) -> bool {
    if [
        "HOME",
        "PATH",
        "NODE_OPTIONS",
        "NODE_EXTRA_CA_CERTS",
        "NODE_TLS_REJECT_UNAUTHORIZED",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
    ]
    .iter()
    .any(|key| name.eq_ignore_ascii_case(key))
    {
        return true;
    }
    let prefixes: &[&str] = match provider {
        Provider::Claude => &[
            "ANTHROPIC",
            "CLAUDE",
            "AWS_",
            "GOOGLE_",
            "GCLOUD",
            "FOUNDRY",
            "AZURE",
        ],
        Provider::Codex => &["CODEX", "OPENAI"],
        Provider::Omp => {
            return (provider_setting_environment_key(provider, name)
                && !native_runtime_environment_key(name))
                || matches!(
                    name,
                    "OMP_PROFILE"
                        | "PI_PROFILE"
                        | "PI_CONFIG_DIR"
                        | "PI_CODING_AGENT_DIR"
                        | "XDG_CONFIG_HOME"
                        | "XDG_DATA_HOME"
                        | "XDG_STATE_HOME"
                        | "XDG_CACHE_HOME"
                );
        }
    };
    prefixes.iter().any(|prefix| {
        name.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    })
}

#[cfg(feature = "host")]
fn check_return_environment_variable(
    provider: Provider,
    environment: &std::collections::BTreeMap<String, String>,
    name: &str,
    inherited: &str,
) -> Result<(), String> {
    if return_environment_key(provider, name)
        && !environment.contains_key(name)
        && (!inherited.is_empty() || name == "PATH")
    {
        return Err(format!(
            "The source leaves {name} unset, but the daemon configures it. A later return to the CLI could change accounts or execution settings. Align the source and daemon configuration before converting; the source has not been stopped."
        ));
    }
    Ok(())
}

/// Construct exact native execution configuration without restoring stale launch
/// values. New credential/account settings require explicit authority before the
/// source may be stopped; persistence is filtered separately for every source.
#[cfg(feature = "host")]
fn preserved_native_environment(
    bytes: &[u8],
    provider: Provider,
    explicit: &std::collections::BTreeMap<String, String>,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut environment = std::collections::BTreeMap::new();
    for entry in bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let index = entry
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or("The source environment contains an invalid entry")?;
        let name = std::str::from_utf8(&entry[..index])
            .map_err(|_| "The source environment contains an unsupported variable name")?;
        let value = std::str::from_utf8(&entry[index + 1..])
            .map_err(|_| "The source environment contains a non-UTF-8 value")?;
        if environment
            .get(name)
            .is_some_and(|previous| previous != value)
        {
            return Err("The source environment contains conflicting duplicate variables".into());
        }
        if native_runtime_environment_key(name) {
            environment.insert(name.to_owned(), value.to_owned());
        } else if let Some(original) = explicit.get(name) {
            if original != value {
                return Err(format!(
                    "Source environment variable {name} changed after its explicit launch. Update the explicit launch configuration before converting; the source has not been stopped."
                ));
            }
            environment.insert(name.to_owned(), original.clone());
        } else if provider_setting_environment_key(provider, name) {
            if sensitive_environment_key(name)
                || matches!(name, "NODE_OPTIONS" | "BUN_OPTIONS" | "LD_PRELOAD")
            {
                return Err(format!(
                    "The source uses credential, account, or permission setting {name}, which requires explicit launch configuration before native handoff; the source has not been stopped."
                ));
            }
            // Ordinary provider configuration can retain its exact execution
            // value; this does not authorize persisting any embedded credentials.
            environment.insert(name.to_owned(), value.to_owned());
        }
    }
    // Absent explicit variables stay absent; never restore stale launch values.
    Ok(environment)
}

/// Validate transient host authority against the actual process, then split
/// execution from durable recovery. A returned PTY may reuse its selected
/// credentials, but observing that PTY must never promote them into a recipe.
#[cfg(feature = "host")]
fn native_environment_parts(
    bytes: &[u8],
    provider: Provider,
    explicit: &std::collections::BTreeMap<String, String>,
    transient: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<
    (
        std::collections::BTreeMap<String, String>,
        std::collections::BTreeMap<String, String>,
    ),
    String,
> {
    let authority;
    let selected = if let Some(transient) = transient {
        authority = transient
            .iter()
            .chain(explicit)
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        &authority
    } else {
        explicit
    };
    let execution = preserved_native_environment(bytes, provider, selected)?;
    let durable = execution
        .iter()
        .filter(|(name, value)| {
            (explicit.get(*name) == Some(*value)
                || fresh_environment_key(provider, name, value, false))
                && persistable_environment_entry(name, value)
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    Ok((durable, execution))
}

#[cfg(feature = "host")]
fn check_environment_removals(bytes: &[u8], recipe: &LaunchRecipe) -> Result<(), String> {
    for name in crate::account::launch_credential_overrides(recipe) {
        if bytes.split(|byte| *byte == 0).any(|entry| {
            entry
                .strip_prefix(name.as_bytes())
                .is_some_and(|rest| rest.starts_with(b"="))
        }) {
            return Err(format!(
                "The source reintroduced {name}, which overrides its selected account. Remove that override or choose the system account before switching to chat; the source has not been stopped."
            ));
        }
    }
    Ok(())
}

#[cfg(feature = "host")]
pub(crate) fn native_environment(
    process: &AgentProcess,
    explicit: &std::collections::BTreeMap<String, String>,
    transient: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<
    (
        std::collections::BTreeMap<String, String>,
        std::collections::BTreeMap<String, String>,
    ),
    String,
> {
    let root = std::path::PathBuf::from(format!("/proc/{}", process.pid));
    let before = bounded_read(root.join("stat"), 4096)
        .and_then(|bytes| process_identity(&bytes))
        .filter(|(_, start)| *start == process.start)
        .ok_or("The source process changed before reading its execution environment")?;
    let bytes = bounded_read(root.join("environ"), 128 * 1024)
        .ok_or("Cannot read the exact source execution environment; no source was stopped")?;
    check_environment_removals(&bytes, &process.recovery)?;
    let unset_environment = crate::account::launch_credential_overrides(&process.recovery);
    let (durable, environment) =
        native_environment_parts(&bytes, process.provider, explicit, transient)?;
    // A host-owned return clears daemon inheritance and carries its proven
    // transient context. Other sources keep the existing conservative fence.
    for (name, value) in std::env::vars_os() {
        let Some(name) = name.to_str() else {
            continue;
        };
        if transient.is_some()
            || unset_environment.contains(name)
            || !return_environment_key(process.provider, name)
        {
            continue;
        }
        let value = value
            .to_str()
            .ok_or("Daemon execution configuration is not UTF-8; no source was stopped")?;
        check_return_environment_variable(process.provider, &environment, name, value)?;
    }
    if bounded_read(root.join("stat"), 4096).and_then(|bytes| process_identity(&bytes))
        != Some(before)
    {
        return Err("The source process changed while reading its execution environment".into());
    }
    Ok((durable, environment))
}

#[cfg(feature = "host")]
struct ForegroundLaunch {
    /// Recovery view: the launcher's environment stays as a fallback for keys
    /// the observed process does not expose. `None` when unrequested.
    inherited: Option<LaunchRecipe>,
    /// Activity view: only the process's own allowlisted environment. `None`
    /// when unrequested, or when that environment could not be observed
    /// exactly - which must never be completed from the launcher's.
    exact: Option<LaunchRecipe>,
    start: u64,
    group: i32,
}

/// Read one process's argv, working directory and allowlisted environment
/// once, then build every requested view from it. The views differ only in
/// their environment, so resolving them from separate reads doubled the
/// largest read in a descent - `environ`, up to 128 KiB - for no new fact.
#[cfg(feature = "host")]
fn read_process_launch(root: &Path, request: ForegroundRequest<'_>) -> Option<ForegroundLaunch> {
    let before = process_identity(&bounded_read(root.join("stat"), 4096)?)?;
    let bytes = bounded_read(root.join("cmdline"), MAX_ARGV_BYTES)?;
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(&bytes);
    let mut argv = bytes
        .split(|byte| *byte == 0)
        .map(|arg| std::str::from_utf8(arg).map(str::to_owned));
    let program = argv.next()?.ok()?;
    let arguments = argv.collect::<Result<Vec<_>, _>>().ok()?;
    let working_directory = fs::read_link(root.join("cwd")).ok()?;
    let environ = bounded_read(root.join("environ"), 128 * 1024);
    let mut inherited = request.hint.map(|base| base.environment.clone());
    // Recovery retains its existing fallback. Activity requires the actual
    // allowlisted environment: missing keys must not inherit terminal
    // overrides, and an argv that long is not an agent invocation.
    let mut exact = (request.agent && arguments.len() <= 128 && environ.is_some())
        .then(std::collections::BTreeMap::new);
    if let Some(bytes) = &environ {
        for entry in bytes.split(|byte| *byte == 0) {
            let Some(index) = entry.iter().position(|byte| *byte == b'=') else {
                continue;
            };
            let Ok(name) = std::str::from_utf8(&entry[..index]) else {
                continue;
            };
            if !observation_environment_key(name) {
                continue;
            }
            match std::str::from_utf8(&entry[index + 1..]) {
                Ok(value) => {
                    if let Some(environment) = &mut inherited {
                        environment.insert(name.into(), value.into());
                    }
                    if let Some(environment) = &mut exact {
                        environment.insert(name.into(), value.into());
                    }
                }
                // A key the process holds but we cannot decode is a gap in
                // exact evidence, not a value to fall back on.
                Err(_) => exact = None,
            }
        }
    }
    // A terminal export cannot implicitly change its recorded account policy.
    // Native handoff checks that the actual process still respects this policy;
    // recovery keeps it even when /proc is temporarily unreadable.
    let unset_environment = request
        .hint
        .map(|base| base.unset_environment.clone())
        .unwrap_or_default();
    let inherited = inherited.map(|environment| LaunchRecipe {
        unset_environment: unset_environment.clone(),
        program: program.clone(),
        arguments: arguments.clone(),
        working_directory: working_directory.clone(),
        environment,
    });
    let exact = exact.map(|environment| LaunchRecipe {
        unset_environment,
        program,
        arguments,
        working_directory,
        environment,
    });
    (before == process_identity(&bounded_read(root.join("stat"), 4096)?)?).then_some(
        ForegroundLaunch {
            inherited,
            exact,
            start: before.1,
            group: before.0,
        },
    )
}

/// Which halves of a foreground observation a descent must resolve. A half
/// that is not requested costs neither its per-node reads nor its observation.
#[cfg(feature = "host")]
#[derive(Clone, Copy)]
struct ForegroundRequest<'a> {
    /// Recovery authority, resolved against this launcher recipe's environment
    /// as the fallback for keys the observed process does not expose. `None`
    /// skips the recovery half entirely.
    hint: Option<&'a LaunchRecipe>,
    /// Exact, process-bound activity evidence.
    agent: bool,
}

/// Outcome of one half of a descent. `Failed` is what the single-purpose
/// walkers returned as `Err(())`: inspection was incomplete or conflicting,
/// never "no agent", so a forwarding ancestor must not upgrade it to exact
/// evidence from stale argv. The halves fail independently, because a process
/// whose `environ` cannot be decoded defeats exact activity evidence while
/// recovery can still complete from the launcher's environment.
#[cfg(feature = "host")]
enum Half<T> {
    Missing,
    Found(T),
    Failed,
}

#[cfg(feature = "host")]
impl<T: PartialEq> Half<T> {
    fn failed(&self) -> bool {
        matches!(self, Self::Failed)
    }

    /// Fold in a child's outcome. Two children that disagree are a conflict,
    /// exactly as when each child's evidence was compared in turn, and a
    /// failure anywhere under this node stays a failure.
    fn merge(&mut self, child: Self) {
        *self = match (std::mem::replace(self, Self::Failed), child) {
            (Self::Failed, _) | (_, Self::Failed) => Self::Failed,
            (Self::Missing, other) => other,
            (found, Self::Missing) => found,
            (Self::Found(previous), Self::Found(next)) if previous == next => Self::Found(previous),
            (Self::Found(_), Self::Found(_)) => Self::Failed,
        };
    }

    fn resolve(self) -> Result<Option<T>, ()> {
        match self {
            Self::Missing => Ok(None),
            Self::Found(value) => Ok(Some(value)),
            Self::Failed => Err(()),
        }
    }
}

#[cfg(feature = "host")]
fn observed_half<T>(result: Result<Option<T>, ()>) -> Half<T> {
    match result {
        Ok(Some(value)) => Half::Found(value),
        Ok(None) => Half::Missing,
        Err(()) => Half::Failed,
    }
}

#[cfg(feature = "host")]
struct ForegroundEvidence {
    hint: Half<String>,
    agent: Half<AgentProcess>,
}

#[cfg(feature = "host")]
impl ForegroundEvidence {
    fn missing() -> Self {
        Self {
            hint: Half::Missing,
            agent: Half::Missing,
        }
    }

    fn failed() -> Self {
        Self {
            hint: Half::Failed,
            agent: Half::Failed,
        }
    }

    /// Nothing deeper in the subtree can change a requested half that already
    /// failed, so the descent stops instead of spending its node budget.
    fn settled(&self, request: ForegroundRequest<'_>) -> bool {
        (request.hint.is_none() || self.hint.failed()) && (!request.agent || self.agent.failed())
    }

    fn merge(&mut self, child: Self) {
        self.hint.merge(child.hint);
        self.agent.merge(child.agent);
    }
}

#[cfg(feature = "host")]
fn foreground_group(pid: libc::pid_t) -> Option<i32> {
    if pid <= 0 {
        return None;
    }
    let (group, _) = process_identity(&bounded_read(format!("/proc/{pid}/stat"), 4096)?)?;
    Some(group)
}

/// Both kinds of foreground evidence, resolved from a single descent.
#[cfg(feature = "host")]
#[derive(Debug, Default)]
pub(crate) struct ForegroundObservation {
    /// Recovery authority for the foreground conversation, if it has any.
    pub(crate) hint: Option<String>,
    /// Raw executable/script/account identity of the foreground agent.
    pub(crate) agent: Option<AgentProcess>,
}

/// Follow the actual PTY foreground group's launcher children once, answering
/// both questions from the same walk.
///
/// Recovery authority and activity identity used to descend independently.
/// Every node stats, reads `cmdline`, resolves `cwd`, reads up to 128 KiB of
/// `environ` and lists its children, so the second descent doubled the cost of
/// an observation without being able to see anything the first could not.
#[cfg(feature = "host")]
pub(crate) fn foreground_observation(
    pid: libc::pid_t,
    base: &LaunchRecipe,
) -> ForegroundObservation {
    let Some(group) = foreground_group(pid) else {
        return ForegroundObservation::default();
    };
    let evidence = walk_foreground(
        Path::new("/proc"),
        pid,
        group,
        &mut 64,
        ForegroundRequest {
            hint: Some(base),
            agent: true,
        },
    );
    ForegroundObservation {
        hint: evidence.hint.resolve().ok().flatten(),
        agent: evidence.agent.resolve().ok().flatten(),
    }
}

/// Observe raw executable/script/account identity through the same bounded
/// foreground walker as recovery, without resolving conversation authority.
#[cfg(feature = "host")]
pub(crate) fn foreground_agent(pid: libc::pid_t) -> Option<AgentProcess> {
    #[cfg(test)]
    FOREGROUND_AGENT_OBSERVATIONS.with(|count| count.set(count.get() + 1));
    let group = foreground_group(pid)?;
    inspect_agent_process(Path::new("/proc"), pid, group, &mut 64)
        .ok()
        .flatten()
}

#[cfg(all(test, feature = "host"))]
thread_local! {
    /// Owner-thread discovery count for the unchanged-observation regression.
    pub(crate) static FOREGROUND_AGENT_OBSERVATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(feature = "host")]
fn inspect_agent_process(
    proc_root: &Path,
    pid: i32,
    group: i32,
    remaining: &mut usize,
) -> Result<Option<AgentProcess>, ()> {
    walk_foreground(
        proc_root,
        pid,
        group,
        remaining,
        ForegroundRequest {
            hint: None,
            agent: true,
        },
    )
    .agent
    .resolve()
}

/// The recovery half of the shared descent, for tests that pin its exact
/// conflict and incompleteness semantics against a planted `/proc` tree.
#[cfg(all(test, feature = "host"))]
fn inspect_process(
    proc_root: &Path,
    pid: i32,
    group: i32,
    base: &LaunchRecipe,
    remaining: &mut usize,
) -> Result<Option<String>, ()> {
    walk_foreground(
        proc_root,
        pid,
        group,
        remaining,
        ForegroundRequest {
            hint: Some(base),
            agent: true,
        },
    )
    .hint
    .resolve()
}

#[cfg(feature = "host")]
fn agent_observation(
    root: &Path,
    pid: i32,
    mut launch: LaunchRecipe,
    start: u64,
) -> Result<Option<AgentProcess>, ()> {
    let provider = Provider::recognize_launch(&launch).ok_or(())?;
    let recovery = Provider::normalize_launch(launch.clone()).ok_or(())?;
    // Recognition uses argv, execution uses the actual installation. Native
    // Claude installations can have a version-only exe basename.
    launch.program = fs::read_link(root.join("exe"))
        .map_err(|_| ())?
        .to_str()
        .ok_or(())?
        .to_owned();
    Ok(Some(AgentProcess {
        pid,
        start,
        provider,
        launch,
        recovery,
    }))
}

#[cfg(feature = "host")]
fn walk_foreground(
    proc_root: &Path,
    pid: i32,
    group: i32,
    remaining: &mut usize,
    request: ForegroundRequest<'_>,
) -> ForegroundEvidence {
    let Some(left) = remaining.checked_sub(1) else {
        return ForegroundEvidence::failed();
    };
    *remaining = left;
    let root = proc_root.join(pid.to_string());
    let Some(launch) = read_process_launch(&root, request) else {
        return ForegroundEvidence::failed();
    };
    let mut evidence = ForegroundEvidence::missing();
    if request.agent && launch.exact.is_none() {
        // Activity evidence is exact or it is nothing: an environment we
        // cannot read exactly is a gap in this subtree, never an absence.
        evidence.agent = Half::Failed;
    }
    if launch.group != group {
        return evidence;
    }
    // Structure decisions read argv, which every view of the launch shares.
    let Some(raw) = launch.inherited.as_ref().or(launch.exact.as_ref()) else {
        return ForegroundEvidence::failed();
    };
    let interpreter = Provider::is_interpreter(&raw.program);
    let native_agent = Provider::from_program(&raw.program).is_some();
    let recognized = Provider::recognize_launch(raw);
    if native_agent && recognized.is_none() {
        return evidence;
    }
    // Claude and OMP run directly inside their JS interpreters and own their
    // tool children just like native agents. Only Codex's recognized JS
    // launcher forwards ownership to a native child.
    let forwarding_launcher =
        interpreter && recognized.is_some_and(Provider::forwards_to_native_child);
    if forwarding_launcher || recognized.is_none() {
        let Some(bytes) = bounded_read(root.join(format!("task/{pid}/children")), 4096) else {
            return ForegroundEvidence::failed();
        };
        let Ok(children) = std::str::from_utf8(&bytes) else {
            return ForegroundEvidence::failed();
        };
        for child in children.split_whitespace() {
            let Ok(child) = child.parse() else {
                return ForegroundEvidence::failed();
            };
            evidence.merge(walk_foreground(proc_root, child, group, remaining, request));
            if evidence.settled(request) {
                return evidence;
            }
        }
    }
    if recognized.is_none() {
        return evidence;
    }
    // A half a child already answered is never re-observed here: the deepest
    // owner of the conversation still wins, exactly as before.
    let observe_hint = request.hint.is_some() && matches!(evidence.hint, Half::Missing);
    let observe_agent = request.agent && matches!(evidence.agent, Half::Missing);
    if !observe_hint && !observe_agent {
        return evidence;
    }
    let ForegroundLaunch {
        inherited,
        exact,
        start,
        ..
    } = launch;
    if observe_hint {
        evidence.hint = match inherited {
            Some(launch) => observed_half(recovery_observation(&root, pid, launch, start)),
            None => Half::Failed,
        };
    }
    if observe_agent {
        evidence.agent = match exact {
            Some(launch) => observed_half(agent_observation(&root, pid, launch, start)),
            None => Half::Failed,
        };
    }
    // An observation is only evidence while it still describes the same
    // process: a reused pid would otherwise donate someone else's identity.
    // Halves a child answered are already settled and stay untouched.
    if process_identity(&bounded_read(root.join("stat"), 4096).unwrap_or_default())
        != Some((group, start))
    {
        if observe_hint {
            evidence.hint = Half::Failed;
        }
        if observe_agent {
            evidence.agent = Half::Failed;
        }
    }
    evidence
}

#[cfg(feature = "host")]
fn recovery_observation(
    root: &Path,
    pid: i32,
    raw: LaunchRecipe,
    start: u64,
) -> Result<Option<String>, ()> {
    let launch = Provider::normalize_launch(raw).ok_or(())?;
    let Some(identity) = runtime_identity(root, pid, &launch, start) else {
        return observed_hint_from_launch(&launch).map(Some).ok_or(());
    };
    // An option the resume cannot carry is no reason to forget the agent.
    let bound = match identity {
        SessionIdentity::Exact(id) => bind_session(&launch, &id),
        SessionIdentity::Fresh(_) | SessionIdentity::Ambiguous => None,
    };
    if let Some(bound) = bound {
        return observed_hint_from_launch(&bound).map(Some).ok_or(());
    }
    // Recognition still exposes activation, but never stale resume authority.
    let hint = serde_json::to_string(&AgentResumeHint {
        version: 2,
        recipe: launch,
    })
    .map_err(|_| ())?;
    Ok((hint.len() <= MAX_HINT_BYTES).then_some(hint))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn cloned_agent_launch_drops_identity_without_dropping_configuration() {
        let id = "a1b2c3d4-1111-4111-8111-123456789abc";
        let original = launch("claude", &["--resume", id, "--model", "sonnet", "--bare"]);
        let fresh = fresh_recipe(&original).unwrap();
        assert_eq!(fresh.arguments, ["--model", "sonnet", "--bare"]);
        assert_eq!(resume_recipe(&original).unwrap().arguments[3], "--resume");
        assert!(fresh_recipe(&launch("claude", &["--continue"])).is_none());
        assert!(fresh_recipe(&launch("claude", &["send this prompt again"])).is_none());
        assert!(fresh_recipe(&launch("claude", &["--unknown-permission-mode"])).is_none());
        let mut omp_launch = launch("omp", &[]);
        omp_launch
            .environment
            .insert("HOME".into(), "/home/test".into());
        let omp = fresh_recipe(&omp_launch).unwrap();
        assert_eq!(omp.arguments[0], "--session-dir");
        assert!(omp.arguments[1].ends_with("/sessions/native"));
    }

    #[cfg(feature = "host")]
    #[test]
    fn native_handoff_preserves_the_omp_xdg_configuration_directory() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config");
        let data = directory.path().join("data");
        let state = directory.path().join("state");
        for root in [&config, &data, &state] {
            fs::create_dir_all(root.join("omp")).unwrap();
        }
        let source = format!(
            "HOME=/source\0XDG_CONFIG_HOME={}\0XDG_DATA_HOME={}\0XDG_STATE_HOME={}\0",
            config.display(),
            data.display(),
            state.display()
        );
        let environment =
            preserved_native_environment(source.as_bytes(), Provider::Omp, &BTreeMap::new())
                .unwrap();
        let mut recipe = launch("omp", &[]);
        recipe.environment = environment;
        assert_eq!(
            Provider::Omp.authored_content_root(&recipe),
            Some(config.join("omp"))
        );
        assert_eq!(Provider::Omp.session_root(&recipe), Some(data.join("omp")));
        assert_eq!(
            Provider::Omp.session_state_root(&recipe),
            Some(state.join("omp"))
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn credential_detection_distinguishes_authentication_from_author_metadata() {
        assert!(!sensitive_environment_key("CARGO_PKG_AUTHORS"));
        assert!(!sensitive_environment_key("GIT_AUTHOR_NAME"));
        for key in [
            "CUSTOM_AUTH",
            "CUSTOM_AUTHORIZATION",
            "CUSTOM_AUTHENTICATION",
        ] {
            assert!(sensitive_environment_key(key));
        }
    }

    #[cfg(feature = "host")]
    #[test]
    fn native_environment_preserves_explicit_credentials_but_not_stale_or_new_secrets() {
        let explicit = BTreeMap::from([
            ("ANTHROPIC_API_KEY".into(), "explicit-key".into()),
            ("ANTHROPIC_BASE_URL".into(), "https://source.invalid".into()),
            ("CLAUDE_CONFIG_DIR".into(), "/old-account".into()),
            ("SSL_CERT_DIR".into(), "/old-ca".into()),
            ("REMOVED_SETTING".into(), "stale".into()),
        ]);
        let environment = preserved_native_environment(
            b"HOME=/source\0PATH=/source/bin\0ANTHROPIC_API_KEY=explicit-key\0ANTHROPIC_BASE_URL=https://source.invalid\0CLAUDE_CONFIG_DIR=/current-account\0SSL_CERT_DIR=/current-ca\0LD_LIBRARY_PATH=/source/lib\0",
            Provider::Claude,
            &explicit,
        ).unwrap();
        assert_eq!(environment["ANTHROPIC_API_KEY"], "explicit-key");
        assert_eq!(environment["ANTHROPIC_BASE_URL"], "https://source.invalid");
        assert_eq!(environment["CLAUDE_CONFIG_DIR"], "/current-account");
        assert_eq!(environment["SSL_CERT_DIR"], "/current-ca");
        assert_eq!(environment["LD_LIBRARY_PATH"], "/source/lib");
        assert!(!environment.contains_key("REMOVED_SETTING"));
        let unset =
            preserved_native_environment(b"HOME=/source\0", Provider::Claude, &explicit).unwrap();
        assert!(!unset.contains_key("ANTHROPIC_API_KEY"));
        assert!(!unset.contains_key("ANTHROPIC_BASE_URL"));
        assert!(!unset.contains_key("SSL_CERT_DIR"));
        let new_secret = preserved_native_environment(
            b"HOME=/source\0ANTHROPIC_API_KEY=new-shell-secret\0",
            Provider::Claude,
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(!new_secret.contains("new-shell-secret"));
        let indirect = preserved_native_environment(
            b"HOME=/source\0GIT_CONFIG_COUNT=1\0GIT_CONFIG_KEY_0=http.extraHeader\0GIT_CONFIG_VALUE_0=Authorization: Bearer indirect-secret\0",
            Provider::Omp,
            &BTreeMap::new(),
        ).unwrap_err();
        assert!(!indirect.contains("indirect-secret"));
        assert!(
            preserved_native_environment(
                b"HOME=/source\0ANTHROPIC_API_KEY=changed-secret\0",
                Provider::Claude,
                &explicit
            )
            .is_err()
        );
        assert!(
            preserved_native_environment(
                b"HOME=/source\0CLAUDE_CODE_PERMISSION_MODE=bypassPermissions\0",
                Provider::Claude,
                &BTreeMap::new()
            )
            .is_err()
        );
        assert!(
            preserved_native_environment(
                b"HOME=/source\0HOME=/other\0",
                Provider::Claude,
                &BTreeMap::new()
            )
            .is_err()
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn fresh_native_configuration_keeps_credentials_transient_and_private_scopes_out() {
        for (name, value) in [
            ("ANTHROPIC_API_KEY", "private-key"),
            ("OPENAI_API_KEY", "other-backend-key"),
            ("HTTPS_PROXY", "http://user:password@proxy.invalid"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "http.extraHeader"),
            ("GIT_CONFIG_VALUE_0", "Authorization: Bearer private-token"),
            (
                "GIT_CONFIG_PARAMETERS",
                "'http.extraHeader'='Authorization: Bearer private-token'",
            ),
        ] {
            assert!(fresh_environment_key(Provider::Omp, name, value, false));
            assert!(!fresh_environment_key(Provider::Omp, name, value, true));
        }
        for (name, value) in [
            ("PI_CACHE_RETENTION", "long"),
            ("OMP_APPROVAL_MODE", "manual"),
            ("ANTHROPIC_BASE_URL", "https://account.invalid"),
            ("HTTPS_PROXY", "http://proxy.invalid:8080"),
        ] {
            assert!(fresh_environment_key(Provider::Omp, name, value, true));
        }
        for name in [
            "PI_TOOL_BRIDGE_TOKEN",
            "PI_TOOL_BRIDGE_URL",
            "PI_ARTIFACTS_DIR",
            "OMP_SESSION_ID",
            "ORCA_API_TOKEN",
            "CAGENTS_TERMINAL_ID",
            "CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION",
            "UNRELATED_SERVICE_API_KEY",
        ] {
            assert!(!fresh_environment_key(
                Provider::Omp,
                name,
                "private",
                false
            ));
            assert!(!fresh_environment_key(Provider::Omp, name, "private", true));
        }
    }

    #[cfg(feature = "host")]
    #[test]
    fn ordinary_source_authenticated_proxy_is_execution_only() {
        let (durable, execution) = native_environment_parts(
            b"HOME=/source\0HTTPS_PROXY=http://user:password@proxy.invalid:8080\0OMP_MAX_TOKENS=8192\0",
            Provider::Omp,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        assert_eq!(
            durable,
            BTreeMap::from([
                ("HOME".into(), "/source".into()),
                ("OMP_MAX_TOKENS".into(), "8192".into()),
            ])
        );
        assert_eq!(
            execution["HTTPS_PROXY"],
            "http://user:password@proxy.invalid:8080"
        );
        assert_eq!(execution["OMP_MAX_TOKENS"], "8192");
    }

    #[cfg(feature = "host")]
    #[test]
    fn explicit_authority_never_makes_credentials_durable() {
        let explicit = BTreeMap::from([
            ("HOME".into(), "/source".into()),
            ("ANTHROPIC_API_KEY".into(), "explicit-key".into()),
            (
                "HTTPS_PROXY".into(),
                "http://user:password@proxy.invalid:8080".into(),
            ),
            (
                "CUSTOM_ENDPOINT".into(),
                "https://service.invalid/?token=private".into(),
            ),
            (
                "GIT_CONFIG_PARAMETERS".into(),
                "'http.extraHeader'='Authorization: Bearer git-token'".into(),
            ),
            ("OMP_APPROVAL_MODE".into(), "manual".into()),
            ("CUSTOM_SETTING".into(), "selected".into()),
        ]);
        let observed = b"HOME=/source\0ANTHROPIC_API_KEY=explicit-key\0HTTPS_PROXY=http://user:password@proxy.invalid:8080\0CUSTOM_ENDPOINT=https://service.invalid/?token=private\0GIT_CONFIG_PARAMETERS='http.extraHeader'='Authorization: Bearer git-token'\0OMP_APPROVAL_MODE=manual\0CUSTOM_SETTING=selected\0";
        // Explicit CLI launches and returned PTYs must use the same persistence
        // boundary, even when a secret was already in the old launch recipe.
        for transient in [None, Some(&explicit)] {
            let (durable, execution) =
                native_environment_parts(observed, Provider::Omp, &explicit, transient).unwrap();
            assert_eq!(execution, explicit);
            assert_eq!(
                durable,
                BTreeMap::from([
                    ("HOME".into(), "/source".into()),
                    ("OMP_APPROVAL_MODE".into(), "manual".into()),
                    ("CUSTOM_SETTING".into(), "selected".into()),
                ])
            );
        }
    }

    #[cfg(feature = "host")]
    #[test]
    fn source_proxy_must_match_selected_authority_without_restoring_stale_values() {
        let authority = BTreeMap::from([(
            "HTTPS_PROXY".into(),
            "http://user:password@proxy.invalid:8080".into(),
        )]);
        let empty = BTreeMap::new();
        for (explicit, transient) in [(&authority, None), (&empty, Some(&authority))] {
            let failure = native_environment_parts(
                b"HTTPS_PROXY=http://other:changed-secret@proxy.invalid:8080\0",
                Provider::Omp,
                explicit,
                transient,
            )
            .unwrap_err();
            assert!(!failure.contains("changed-secret"));
            let (durable, execution) =
                native_environment_parts(b"", Provider::Omp, explicit, transient).unwrap();
            assert!(!durable.contains_key("HTTPS_PROXY"));
            assert!(!execution.contains_key("HTTPS_PROXY"));
        }
    }

    #[cfg(feature = "host")]
    #[test]
    fn returned_terminal_credentials_are_authorized_only_for_execution() {
        let transient = BTreeMap::from([
            ("ANTHROPIC_API_KEY".into(), "gui-key".into()),
            (
                "HTTPS_PROXY".into(),
                "http://user:password@proxy.invalid".into(),
            ),
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into()),
            (
                "GIT_CONFIG_VALUE_0".into(),
                "Authorization: Bearer git-token".into(),
            ),
        ]);
        let observed = b"HOME=/selected\0ANTHROPIC_API_KEY=gui-key\0HTTPS_PROXY=http://user:password@proxy.invalid\0GIT_CONFIG_COUNT=1\0GIT_CONFIG_KEY_0=http.extraHeader\0GIT_CONFIG_VALUE_0=Authorization: Bearer git-token\0";
        let (durable, execution) =
            native_environment_parts(observed, Provider::Omp, &BTreeMap::new(), Some(&transient))
                .unwrap();
        assert_eq!(
            durable,
            BTreeMap::from([("HOME".into(), "/selected".into())])
        );
        for (name, value) in &transient {
            assert_eq!(execution.get(name), Some(value));
        }
        assert!(native_environment_parts(observed, Provider::Omp, &BTreeMap::new(), None).is_err());
        let changed = b"HOME=/selected\0ANTHROPIC_API_KEY=another-account\0";
        let failure =
            native_environment_parts(changed, Provider::Omp, &BTreeMap::new(), Some(&transient))
                .unwrap_err();
        assert!(!failure.contains("another-account"));
    }

    #[cfg(feature = "host")]
    #[test]
    fn native_environment_scopes_agent_settings_and_preserves_benign_configuration() {
        let source = b"HOME=/source\0CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION=12\0CLAUDE_CODE_PERMISSION_MODE=bypassPermissions\0CODEX_SANDBOX_MODE=read-only\0PI_CACHE_RETENTION=long\0OMP_MAX_TOKENS=8192\0";
        let omp = preserved_native_environment(source, Provider::Omp, &BTreeMap::new()).unwrap();
        assert_eq!(omp["PI_CACHE_RETENTION"], "long");
        assert_eq!(omp["OMP_MAX_TOKENS"], "8192");
        assert!(!omp.contains_key("CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION"));
        assert!(!omp.contains_key("CLAUDE_CODE_PERMISSION_MODE"));
        assert!(!omp.contains_key("CODEX_SANDBOX_MODE"));
        for name in [
            "CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION",
            "CLAUDE_CODE_PERMISSION_MODE",
            "CODEX_SANDBOX_MODE",
        ] {
            assert!(
                check_return_environment_variable(Provider::Omp, &omp, name, "inherited").is_ok()
            );
        }
        let claude = preserved_native_environment(
            b"CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION=12\0OPENAI_API_KEY=unrelated-secret\0PI_CACHE_RETENTION=long\0",
            Provider::Claude,
            &BTreeMap::new(),
        ).unwrap();
        assert_eq!(claude["CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION"], "12");
        assert!(!claude.contains_key("OPENAI_API_KEY"));
        assert!(!claude.contains_key("PI_CACHE_RETENTION"));
        assert!(preserved_native_environment(source, Provider::Claude, &BTreeMap::new()).is_err());
        assert!(preserved_native_environment(source, Provider::Codex, &BTreeMap::new()).is_err());
        assert!(
            preserved_native_environment(
                b"OMP_APPROVAL_MODE=auto\0",
                Provider::Omp,
                &BTreeMap::new()
            )
            .is_err()
        );
        let explicit = BTreeMap::from([("OMP_APPROVAL_MODE".into(), "auto".into())]);
        let omp =
            preserved_native_environment(b"OMP_APPROVAL_MODE=auto\0", Provider::Omp, &explicit)
                .unwrap();
        assert_eq!(omp["OMP_APPROVAL_MODE"], "auto");
    }

    #[cfg(feature = "host")]
    #[test]
    fn return_to_cli_refuses_missing_account_overrides_but_not_unrelated_runtime_variables() {
        let mut source = BTreeMap::from([("HOME".into(), "/source".into())]);
        assert!(
            check_return_environment_variable(
                Provider::Claude,
                &source,
                "ANTHROPIC_API_KEY",
                "daemon-key"
            )
            .is_err()
        );
        assert!(
            check_return_environment_variable(
                Provider::Claude,
                &source,
                "ANTHROPIC_BASE_URL",
                "https://daemon.invalid"
            )
            .is_err()
        );
        assert!(
            check_return_environment_variable(
                Provider::Claude,
                &source,
                "CLAUDE_CODE_PERMISSION_MODE",
                "bypassPermissions"
            )
            .is_err()
        );
        assert!(
            check_return_environment_variable(
                Provider::Claude,
                &source,
                "BUN_INSTALL",
                "/daemon/bun"
            )
            .is_ok()
        );
        assert!(
            check_return_environment_variable(
                Provider::Claude,
                &source,
                "OPENAI_API_KEY",
                "other-provider-key"
            )
            .is_ok()
        );
        source.insert("ANTHROPIC_API_KEY".into(), "explicit-source-key".into());
        assert!(
            check_return_environment_variable(
                Provider::Claude,
                &source,
                "ANTHROPIC_API_KEY",
                "different-daemon-key"
            )
            .is_ok()
        );
        assert!(
            check_return_environment_variable(Provider::Omp, &source, "OMP_PROFILE", "other")
                .is_err()
        );
    }

    #[cfg(feature = "host")]
    fn process_fixture(proc_root: &Path, pid: i32, recipe: &LaunchRecipe, children: &[i32]) {
        use std::os::unix::fs::symlink;
        let root = proc_root.join(pid.to_string());
        fs::create_dir_all(root.join("fd")).unwrap();
        fs::create_dir_all(root.join("fdinfo")).unwrap();
        fs::create_dir_all(root.join(format!("task/{pid}"))).unwrap();
        let mut fields = ["0"; 20];
        fields[0] = "S";
        fields[2] = "100";
        fs::write(
            root.join("stat"),
            format!("{pid} (agent) {}", fields.join(" ")),
        )
        .unwrap();
        let argv = std::iter::once(recipe.program.as_str())
            .chain(recipe.arguments.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("\0");
        fs::write(root.join("cmdline"), argv).unwrap();
        symlink(&recipe.working_directory, root.join("cwd")).unwrap();
        fs::write(
            root.join(format!("task/{pid}/children")),
            children
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(" "),
        )
        .unwrap();
    }
    #[cfg(feature = "host")]
    fn activity_fixture(proc_root: &Path, recipe: &LaunchRecipe, executable: &Path) {
        process_fixture(proc_root, 100, recipe, &[]);
        std::os::unix::fs::symlink(executable, proc_root.join("100/exe")).unwrap();
        let environment = recipe
            .environment
            .iter()
            .map(|(key, value)| format!("{key}={value}\0"))
            .collect::<String>();
        fs::write(proc_root.join("100/environ"), environment).unwrap();
    }

    #[cfg(feature = "host")]
    #[test]
    fn observed_child_retains_selected_account_policy_even_when_environment_changes() {
        let directory = tempfile::tempdir().unwrap();
        let mut base = launch(
            "claude",
            &["--resume", "a1b2c3d4-1111-4111-8111-123456789abc"],
        );
        base.unset_environment =
            ["ANTHROPIC_API_KEY".into(), "CLAUDE_CODE_USE_BEDROCK".into()].into();
        assert!(check_environment_removals(b"HOME=/fixture\0", &base).is_ok());
        assert!(check_environment_removals(b"CLAUDE_CODE_USE_BEDROCK=1\0", &base).is_err());
        assert!(check_environment_removals(b"ANTHROPIC_API_KEY=fixture-only\0", &base).is_err());
        assert!(
            check_environment_removals(b"ANTHROPIC_API_KEY_SUFFIX=fixture-only\0", &base).is_ok()
        );
        process_fixture(directory.path(), 100, &base, &[]);
        let root = directory.path().join("100");
        for (bytes, expected) in [
            (
                b"HOME=/fixture\0".as_slice(),
                base.unset_environment.clone(),
            ),
            (
                b"HOME=/fixture\0CLAUDE_CODE_USE_BEDROCK=1\0".as_slice(),
                base.unset_environment.clone(),
            ),
        ] {
            fs::write(root.join("environ"), bytes).unwrap();
            let observed = read_process_launch(
                &root,
                ForegroundRequest {
                    hint: Some(&base),
                    agent: true,
                },
            )
            .unwrap();
            assert_eq!(observed.inherited.unwrap().unset_environment, expected);
            assert_eq!(observed.exact.unwrap().unset_environment, expected);
        }
        fs::remove_file(root.join("environ")).unwrap();
        let observed = read_process_launch(
            &root,
            ForegroundRequest {
                hint: Some(&base),
                agent: true,
            },
        )
        .unwrap();
        assert_eq!(
            observed.inherited.unwrap().unset_environment,
            base.unset_environment
        );
        assert!(
            observed.exact.is_none(),
            "unreadable environment cannot authorize a native handoff"
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn native_handoff_represents_selected_account_absence_despite_daemon_credentials() {
        const CHILD: &str = "CAGENTS_ENV_REMOVAL_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "model::agent_recovery::tests::native_handoff_represents_selected_account_absence_despite_daemon_credentials", "--nocapture"])
                .env_clear().env(CHILD, "1").env("HOME", "/fixture")
                .env("PATH", "/usr/bin:/bin").env("ANTHROPIC_API_KEY", "daemon-fixture")
                .status().unwrap().success());
            return;
        }
        let mut recipe = launch("claude", &[]);
        recipe.environment = [
            ("HOME".into(), "/fixture".into()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("CLAUDE_CONFIG_DIR".into(), "/selected".into()),
        ]
        .into();
        recipe.unset_environment.insert("ANTHROPIC_API_KEY".into());
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("10")
            .env_clear()
            .envs(&recipe.environment)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        // `spawn` returns as soon as the child stops sharing this process's
        // memory. Until its exec completes, `/proc` shows it with this image
        // and environment, then with the new image and no environment yet.
        let own = fs::read_link("/proc/self/exe").unwrap();
        let executed = || {
            fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|image| image != own)
                && fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| !environ.is_empty())
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !executed() {
            assert!(
                std::time::Instant::now() < deadline,
                "the fixture never finished exec"
            );
            std::thread::yield_now();
        }
        let start = process_identity(&fs::read(format!("/proc/{pid}/stat")).unwrap())
            .unwrap()
            .1;
        let process = AgentProcess {
            pid,
            start,
            provider: Provider::Claude,
            launch: recipe.clone(),
            recovery: recipe.clone(),
        };
        let result = native_environment(&process, &recipe.environment, None);
        child.kill().unwrap();
        child.wait().unwrap();
        let (durable, execution) = result.unwrap();
        assert_eq!(
            durable.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/selected")
        );
        assert!(!execution.contains_key("ANTHROPIC_API_KEY"));
    }

    #[cfg(feature = "host")]
    #[test]
    fn activity_retains_actual_interpreter_and_only_the_process_account_environment() {
        let directory = tempfile::tempdir().unwrap();
        let script = "/opt/node_modules/@anthropic-ai/claude-code/cli.js";
        let mut raw = launch(
            "node",
            &[
                script,
                "--setting-sources",
                "",
                "--resume",
                "a1b2c3d4-1111-4111-8111-123456789abc",
            ],
        );
        raw.environment = BTreeMap::from([
            ("HOME".into(), "/home/actual".into()),
            ("PATH".into(), "/actual/bin".into()),
            ("UNRELATED_TOKEN".into(), "synthetic-test-value".into()),
        ]);
        activity_fixture(directory.path(), &raw, Path::new("/actual/bin/node"));
        let mut base = raw.clone();
        base.environment
            .insert("CLAUDE_CONFIG_DIR".into(), "/stale/claude".into());
        base.environment
            .insert("CODEX_HOME".into(), "/stale/codex".into());
        let recovery = inspect_process(directory.path(), 100, 100, &base, &mut 64)
            .unwrap()
            .unwrap();
        let recovered = observed_launch_from_hint(&recovery).unwrap();
        assert_eq!(recovered.program, "claude");
        assert_eq!(recovered.environment["CLAUDE_CONFIG_DIR"], "/stale/claude");
        assert_eq!(
            recovered.arguments,
            [
                "--setting-sources",
                "",
                "--resume",
                "a1b2c3d4-1111-4111-8111-123456789abc"
            ]
        );

        let process = inspect_agent_process(directory.path(), 100, 100, &mut 64)
            .unwrap()
            .unwrap();
        assert_eq!(process.provider, Provider::Claude);
        assert_eq!(process.launch.program, "/actual/bin/node");
        assert_eq!(process.launch.arguments, raw.arguments);
        assert_eq!(
            process.launch.environment,
            BTreeMap::from([
                ("HOME".into(), "/home/actual".into()),
                ("PATH".into(), "/actual/bin".into()),
            ])
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn activity_recognizes_native_argv_but_executes_the_versioned_binary() {
        let directory = tempfile::tempdir().unwrap();
        activity_fixture(
            directory.path(),
            &launch("claude", &[]),
            Path::new("/home/actual/.local/share/claude/versions/2.1.0"),
        );
        let process = inspect_agent_process(directory.path(), 100, 100, &mut 64)
            .unwrap()
            .unwrap();
        assert_eq!(process.provider, Provider::Claude);
        assert_eq!(
            process.launch.program,
            "/home/actual/.local/share/claude/versions/2.1.0"
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn activity_refuses_unreadable_or_non_utf8_account_evidence() {
        let directory = tempfile::tempdir().unwrap();
        activity_fixture(
            directory.path(),
            &launch("claude", &[]),
            Path::new("/opt/claude"),
        );
        let environ = directory.path().join("100/environ");
        fs::remove_file(&environ).unwrap();
        assert!(inspect_agent_process(directory.path(), 100, 100, &mut 64).is_err());
        fs::write(&environ, b"HOME=/home/actual\0CLAUDE_CONFIG_DIR=\xff\0").unwrap();
        assert!(inspect_agent_process(directory.path(), 100, 100, &mut 64).is_err());
        fs::write(&environ, b"HOME=/home/actual\0").unwrap();
        fs::remove_file(directory.path().join("100/exe")).unwrap();
        assert!(inspect_agent_process(directory.path(), 100, 100, &mut 64).is_err());
    }

    #[cfg(feature = "host")]
    #[test]
    fn interpreted_controlling_agents_keep_their_session_over_agent_tool_children() {
        let controlling_id = "a1b2c3d4-1111-4111-8111-123456789abc";
        let tool_id = "a1b2c3d4-2222-4111-8111-123456789abc";
        for (interpreter, entrypoint, provider) in [
            (
                "node",
                "/opt/node_modules/@anthropic-ai/claude-code/cli.js",
                "claude",
            ),
            ("bun", "/opt/omp/omp.js", "omp"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let parent = launch(interpreter, &[entrypoint, "--resume", controlling_id]);
            process_fixture(directory.path(), 100, &parent, &[101]);
            process_fixture(
                directory.path(),
                101,
                &launch("codex", &["resume", tool_id]),
                &[],
            );
            let hint = inspect_process(directory.path(), 100, 100, &parent, &mut 64)
                .unwrap()
                .unwrap();
            let recovered = recipe_from_hint(&hint).unwrap();
            assert_eq!(recovered.program, provider);
            assert_eq!(recovered.arguments, ["--resume", controlling_id]);
        }
    }

    #[cfg(feature = "host")]
    #[test]
    fn ambiguous_or_incomplete_native_child_blocks_launcher_resume_fallback() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let ancestor_id = "a1b2c3d4-1111-4111-8111-123456789abc";
        let active_id = "a1b2c3d4-2222-4111-8111-123456789abc";
        let other_id = "a1b2c3d4-3333-4111-8111-123456789abc";
        let mut parent = launch(
            "node",
            &[
                "/opt/node_modules/@openai/codex/bin/codex.js",
                "resume",
                ancestor_id,
            ],
        );
        parent
            .environment
            .insert("HOME".into(), directory.path().to_str().unwrap().into());
        process_fixture(directory.path(), 100, &parent, &[101]);
        process_fixture(directory.path(), 101, &launch("codex", &[]), &[]);
        // A recognized child without an exact identity must not inherit the
        // launcher's stale resume argument.
        let hint = inspect_process(directory.path(), 100, 100, &parent, &mut 64)
            .unwrap()
            .unwrap();
        assert!(recipe_from_hint(&hint).is_none());
        assert_eq!(observed_launch_from_hint(&hint).unwrap().program, "codex");
        for (fd, id) in [(3, active_id), (4, other_id)] {
            symlink(
                directory
                    .path()
                    .join(format!(".codex/sessions/rollout-2026-09-09-{id}.jsonl")),
                directory.path().join(format!("101/fd/{fd}")),
            )
            .unwrap();
            fs::write(
                directory.path().join(format!("101/fdinfo/{fd}")),
                "flags:\t0100001\n",
            )
            .unwrap();
        }
        let ambiguous = inspect_process(directory.path(), 100, 100, &parent, &mut 64)
            .unwrap()
            .unwrap();
        assert!(recipe_from_hint(&ambiguous).is_none());
        assert_eq!(
            observed_launch_from_hint(&ambiguous).unwrap().program,
            "codex"
        );
        // An unreadable/exited child is incomplete inspection, not proof that
        // the launcher has no native child.
        fs::remove_file(directory.path().join("101/cmdline")).unwrap();
        assert_eq!(
            inspect_process(directory.path(), 100, 100, &parent, &mut 64),
            Err(())
        );
    }

    fn launch(program: &str, arguments: &[&str]) -> LaunchRecipe {
        LaunchRecipe {
            unset_environment: Default::default(),
            program: program.into(),
            arguments: arguments.iter().map(|arg| (*arg).into()).collect(),
            working_directory: "/tmp".into(),
            environment: BTreeMap::new(),
        }
    }
    #[test]
    fn selected_account_removals_survive_all_resume_paths() {
        let mut original = launch(
            "claude",
            &["--resume", "a1b2c3d4-1111-4111-8111-123456789abc"],
        );
        original.unset_environment =
            ["ANTHROPIC_API_KEY".into(), "CLAUDE_CODE_OAUTH_TOKEN".into()].into();
        for recipe in [
            resume_recipe(&original).unwrap(),
            fresh_recipe(&original).unwrap(),
            picker_recipe(&original).unwrap(),
            bind_session(&original, "a1b2c3d4-1111-4111-8111-987654321abc").unwrap(),
            recipe_from_hint(&hint_from_launch(&original).unwrap()).unwrap(),
        ] {
            assert_eq!(recipe.unset_environment, original.unset_environment);
        }
    }

    #[test]
    fn picker_recipe_drops_every_selector_and_opens_the_provider_picker() {
        let id = "a1b2c3d4-1111-4111-8111-123456789abc";
        let claude = picker_recipe(&launch(
            "claude",
            &["--resume", id, "--model", "sonnet", "--bare"],
        ))
        .unwrap();
        assert_eq!(
            claude.arguments,
            ["--model", "sonnet", "--bare", "--resume"]
        );
        assert!(is_picker_recipe(&claude));
        // A picker search term, a latest-session shortcut and an assigned ID
        // are all selectors the picker supersedes.
        for arguments in [
            &["-r", "search term"][..],
            &["--continue"],
            &["-c"],
            &["--session-id", id],
            &[&format!("--resume={id}")],
            &["--resume"],
        ] {
            assert_eq!(
                picker_recipe(&launch("claude", arguments))
                    .unwrap()
                    .arguments,
                ["--resume"],
                "{arguments:?}"
            );
        }
        let codex = picker_recipe(&launch("codex", &["resume", id, "-c", "model=o3"])).unwrap();
        assert_eq!(codex.arguments, ["resume", "-c", "model=o3"]);
        assert!(is_picker_recipe(&codex));
        assert_eq!(
            picker_recipe(&launch("codex", &["-m", "o3", "resume", id]))
                .unwrap()
                .arguments,
            ["resume", "-m", "o3"]
        );
        // A value that happens to spell a selector is still that option's value.
        assert_eq!(
            picker_recipe(&launch("codex", &["-p", "resume", "resume", id]))
                .unwrap()
                .arguments,
            ["resume", "-p", "resume"]
        );
        assert_eq!(
            picker_recipe(&launch("codex", &["resume", "--last", "-m", "o3"]))
                .unwrap()
                .arguments,
            ["resume", "-m", "o3"]
        );
        let omp = picker_recipe(&launch(
            "omp",
            &["--session-dir", "/sessions", "--resume", id],
        ))
        .unwrap();
        assert_eq!(omp.arguments, ["--session-dir", "/sessions", "--resume"]);
        assert!(is_picker_recipe(&omp));
        // Prompts and unknown options stay refused: nothing is ever replayed.
        assert!(picker_recipe(&launch("claude", &["--model", "sonnet", "do it again"])).is_none());
        assert!(picker_recipe(&launch("claude", &["--unknown-permission-mode"])).is_none());
        assert!(picker_recipe(&launch("/bin/sh", &[])).is_none());
        // Neither a fresh nor an exact recipe passes for a picker.
        assert!(!is_picker_recipe(&launch("claude", &["--model", "sonnet"])));
        assert!(!is_picker_recipe(&launch("claude", &["--resume", id])));
    }

    #[test]
    fn npm_launchers_resolve_exact_sessions_without_selecting_server_processes() {
        let id = "a1b2c3d4-1111-4111-8111-123456789abc";
        let codex = Provider::normalize_launch(launch(
            "/usr/bin/node",
            &[
                "/opt/node_modules/@openai/codex/bin/codex.js",
                "--remote",
                "unix:///tmp/existing.sock",
                "resume",
                id,
            ],
        ))
        .unwrap();
        let recipe = resume_recipe(&codex).unwrap();
        assert_eq!(recipe.program, "codex");
        assert_eq!(
            recipe.arguments,
            ["resume", id, "--remote", "unix:///tmp/existing.sock"]
        );
        assert!(
            Provider::normalize_launch(launch("node", &["/tmp/tool.js", "codex", "resume", id]))
                .is_none()
        );
        assert!(
            Provider::normalize_launch(launch(
                "codex",
                &["app-server", "--listen", "unix:///tmp/s"]
            ))
            .is_none()
        );
        assert!(
            Provider::normalize_launch(launch("omp", &["__omp_worker_js_eval_process"])).is_none()
        );
        let claude = Provider::normalize_launch(launch(
            "node",
            &[
                "/opt/node_modules/@anthropic-ai/claude-code/cli.js",
                "--resume",
                id,
            ],
        ))
        .unwrap();
        assert_eq!(resume_recipe(&claude).unwrap().arguments, ["--resume", id]);
    }

    #[cfg(feature = "host")]
    #[test]
    fn runtime_identity_is_bound_to_this_terminal_not_a_recent_session() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("proc/100");
        fs::create_dir_all(root.join("fd")).unwrap();
        symlink("/dev/pts/123", root.join("fd/0")).unwrap();
        let agent = directory.path().join(".omp/agent");
        fs::create_dir_all(agent.join("terminal-sessions")).unwrap();
        let id = "a1b2c3d4-1111-4111-8111-123456789abc";
        let session = agent.join(format!("sessions/work/2026-09-09_{id}.jsonl"));
        let marker = agent.join("terminal-sessions/pts-123");
        let mut recipe = launch("omp", &["--resume", "a1b2c3d4-2222-4111-8111-123456789abc"]);
        recipe
            .environment
            .insert("HOME".into(), directory.path().to_str().unwrap().into());
        let proc_root = root.parent().unwrap();
        process_fixture(proc_root, 100, &recipe, &[]);
        fs::write(&marker, format!("/tmp\n{}\nfresh\n", session.display())).unwrap();
        let fresh = inspect_process(proc_root, 100, 100, &recipe, &mut 64)
            .unwrap()
            .unwrap();
        assert!(recipe_from_hint(&fresh).is_none());
        assert_eq!(observed_launch_from_hint(&fresh).unwrap().program, "omp");
        assert_eq!(
            runtime_identity(&root, 100, &recipe, 0),
            Some(SessionIdentity::Fresh(session.clone()))
        );
        // A deleted established session must not be replaced with an empty chat.
        fs::write(&marker, format!("/tmp\n{}\n", session.display())).unwrap();
        assert_eq!(
            runtime_identity(&root, 100, &recipe, 0),
            Some(SessionIdentity::Ambiguous)
        );
        fs::write(&marker, format!("/tmp\n{}\nfresh\n", session.display())).unwrap();
        fs::create_dir_all(session.parent().unwrap()).unwrap();
        fs::write(&session, "").unwrap();
        let empty = inspect_process(proc_root, 100, 100, &recipe, &mut 64)
            .unwrap()
            .unwrap();
        assert!(recipe_from_hint(&empty).is_none());
        fs::write(
            &session,
            format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\n"),
        )
        .unwrap();
        assert_eq!(
            runtime_identity(&root, 100, &recipe, 0),
            Some(SessionIdentity::Exact(id.into()))
        );
        let hint = inspect_process(proc_root, 100, 100, &recipe, &mut 64)
            .unwrap()
            .unwrap();
        assert_eq!(recipe_from_hint(&hint).unwrap().arguments, ["--resume", id]);
        // Through a linked home, the marker may name the resolved session.
        let linked = directory.path().join("linked-home");
        symlink(directory.path(), &linked).unwrap();
        let mut through_link = recipe.clone();
        through_link
            .environment
            .insert("HOME".into(), linked.to_str().unwrap().into());
        fs::write(
            &marker,
            format!("/tmp\n{}\n", fs::canonicalize(&session).unwrap().display()),
        )
        .unwrap();
        assert_eq!(
            runtime_identity(&root, 100, &through_link, 0),
            Some(SessionIdentity::Exact(id.into()))
        );
        fs::File::options()
            .write(true)
            .open(&marker)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        assert!(runtime_identity(&root, 100, &recipe, 0).is_none());
        fs::write(
            &marker,
            format!("/another-worktree\n{}\n", session.display()),
        )
        .unwrap();
        assert_eq!(
            runtime_identity(&root, 100, &recipe, 0),
            Some(SessionIdentity::Ambiguous)
        );
        fs::remove_file(&marker).unwrap();
        // A different terminal's valid marker is not a fallback.
        fs::write(
            agent.join("terminal-sessions/pts-124"),
            format!("/tmp\n{}\n", session.display()),
        )
        .unwrap();
        assert!(runtime_identity(&root, 100, &recipe, 0).is_none());
    }

    #[cfg(feature = "host")]
    #[test]
    fn conflicting_open_conversations_never_choose_an_arbitrary_descriptor() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("proc");
        fs::create_dir_all(root.join("fd")).unwrap();
        fs::create_dir_all(root.join("fdinfo")).unwrap();
        let first = "a1b2c3d4-1111-4111-8111-123456789abc";
        let second = "a1b2c3d4-2222-4111-8111-123456789abc";
        let mut recipe = launch("codex", &[]);
        recipe
            .environment
            .insert("HOME".into(), directory.path().to_str().unwrap().into());
        symlink(
            directory
                .path()
                .join(format!(".codex/sessions/rollout-2026-09-09-{first}.jsonl")),
            root.join("fd/3"),
        )
        .unwrap();
        fs::write(root.join("fdinfo/3"), "flags:\t0100000\n").unwrap();
        assert!(runtime_identity(&root, 100, &recipe, 0).is_none());
        fs::write(root.join("fdinfo/3"), "flags:\t0100001\n").unwrap();
        assert_eq!(
            runtime_identity(&root, 100, &recipe, 0),
            Some(SessionIdentity::Exact(first.into()))
        );
        symlink(
            directory
                .path()
                .join(format!(".codex/sessions/rollout-2026-09-09-{second}.jsonl")),
            root.join("fd/4"),
        )
        .unwrap();
        fs::write(root.join("fdinfo/4"), "flags:\t0100001\n").unwrap();
        assert_eq!(
            runtime_identity(&root, 100, &recipe, 0),
            Some(SessionIdentity::Ambiguous)
        );
    }

    /// The kernel names an open file by its resolved path, so a session root
    /// linked into HOME from elsewhere, such as a dotfiles checkout, must be
    /// compared resolved too.
    #[cfg(feature = "host")]
    #[test]
    fn a_linked_session_root_still_identifies_the_open_conversation() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("proc");
        fs::create_dir_all(root.join("fd")).unwrap();
        fs::create_dir_all(root.join("fdinfo")).unwrap();
        let dotfiles = directory.path().join("dotfiles/codex");
        fs::create_dir_all(dotfiles.join("sessions/2026/09/23")).unwrap();
        symlink(&dotfiles, directory.path().join(".codex")).unwrap();
        let id = "a1b2c3d4-1111-4111-8111-123456789abc";
        let mut recipe = launch("codex", &[]);
        recipe
            .environment
            .insert("HOME".into(), directory.path().to_str().unwrap().into());
        symlink(
            dotfiles.join(format!("sessions/2026/09/23/rollout-2026-09-23-{id}.jsonl")),
            root.join("fd/3"),
        )
        .unwrap();
        fs::write(root.join("fdinfo/3"), "flags:\t0100001\n").unwrap();
        assert_eq!(
            runtime_identity(&root, 100, &recipe, 0),
            Some(SessionIdentity::Exact(id.into()))
        );
    }

    /// Claude keeps no transcript open, so its conversation is the one its
    /// process record names: after `/resume`, not the one in its argv.
    #[cfg(feature = "host")]
    #[test]
    fn claude_is_identified_by_its_own_process_record() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let work = directory.path().join("work");
        // A configuration directory linked in from elsewhere, as dotfiles are.
        let config = directory.path().join("dotfiles/claude");
        fs::create_dir_all(config.join("sessions")).unwrap();
        fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(&config, home.join(".claude")).unwrap();
        fs::create_dir_all(&work).unwrap();
        let launched = "a1b2c3d4-1111-4111-8111-123456789abc";
        let current = "a1b2c3d4-2222-4111-8111-123456789abc";
        let mut recipe = launch("claude", &["--resume", launched]);
        recipe.working_directory = work.clone();
        recipe
            .environment
            .insert("HOME".into(), home.to_str().unwrap().into());
        let proc_root = directory.path().join("proc");
        process_fixture(&proc_root, 100, &recipe, &[]);
        let root = proc_root.join("100");
        let record = config.join("sessions/100.json");
        let write_record = |pid: i32, start: &str, extra: serde_json::Value| {
            let mut fields = serde_json::json!({
                "pid": pid,
                "procStart": start,
                "sessionId": current,
                "cwd": work,
                "kind": "interactive",
            });
            fields
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            fs::write(&record, fields.to_string()).unwrap()
        };
        let hint = |recipe: &LaunchRecipe| {
            inspect_process(&proc_root, 100, 100, recipe, &mut 64)
                .unwrap()
                .unwrap()
        };

        // A conversation with nothing said yet has no transcript to resume,
        // and the one the argv names is no longer this process's.
        write_record(100, "0", serde_json::json!({}));
        assert_eq!(
            runtime_identity(&root, 100, &recipe, 0),
            Some(SessionIdentity::Ambiguous)
        );
        assert_eq!(recipe_from_hint(&hint(&recipe)), None);
        assert_eq!(
            observed_launch_from_hint(&hint(&recipe)).map(|launch| launch.arguments),
            Some(recipe.arguments.clone())
        );

        let project: String = work
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let history = config.join("projects").join(project);
        fs::create_dir_all(&history).unwrap();
        fs::write(history.join(format!("{current}.jsonl")), "{}\n").unwrap();
        assert_eq!(
            runtime_identity(&root, 100, &recipe, 0),
            Some(SessionIdentity::Exact(current.into()))
        );
        let resumed = recipe_from_hint(&hint(&recipe)).unwrap();
        assert_eq!(resumed.arguments, ["--resume", current]);
        assert_eq!(resumed.working_directory, work);

        // An option the resume cannot carry still leaves the agent recognised.
        fs::write(
            root.join("cmdline"),
            ["claude", "--unknown-option", "--resume", launched].join("\0"),
        )
        .unwrap();
        assert_eq!(recipe_from_hint(&hint(&recipe)), None);
        assert_eq!(
            observed_launch_from_hint(&hint(&recipe)).map(|launch| launch.arguments),
            Some(vec![
                "--unknown-option".into(),
                "--resume".into(),
                launched.into()
            ])
        );

        // A conversation a parked background job owns, or a process that is
        // not interactive, is not one to resume in this terminal.
        for extra in [
            serde_json::json!({"parkedJobId": "job"}),
            serde_json::json!({"kind": "print"}),
        ] {
            write_record(100, "0", extra);
            assert_eq!(
                runtime_identity(&root, 100, &recipe, 0),
                Some(SessionIdentity::Ambiguous)
            );
        }

        // A record left by an earlier process with the same pid, or by
        // another process, names someone else's conversation.
        write_record(100, "7", serde_json::json!({}));
        assert_eq!(runtime_identity(&root, 100, &recipe, 0), None);
        write_record(101, "0", serde_json::json!({}));
        assert_eq!(runtime_identity(&root, 100, &recipe, 0), None);
        // Claude writes its start time as a string; anything else is not its record.
        write_record(100, "0", serde_json::json!({"procStart": 0}));
        assert_eq!(runtime_identity(&root, 100, &recipe, 0), None);
    }

    #[test]
    fn codex_recovery_retains_the_exact_remote_endpoint() {
        let id = "a1b2c3d4-1111-4111-8111-123456789abc";
        let recipe = launch(
            "codex",
            &["--remote", "unix:///tmp/account.sock", "resume", id],
        );
        let recovered = resume_recipe(&recipe).unwrap();
        assert_eq!(
            recovered.arguments,
            ["resume", id, "--remote", "unix:///tmp/account.sock"]
        );
    }

    #[test]
    fn recovery_keeps_exact_identity_without_replaying_or_forking() {
        let id = "a1b2c3d4-1111-4111-8111-123456789abc";
        for (agent, flag) in [
            ("omp", "--resume"),
            ("claude", "--session-id"),
            ("codex", "resume"),
        ] {
            let hint = hint_from_launch(&launch(agent, &[flag, id])).unwrap();
            let recipe = recipe_from_hint(&hint).unwrap();
            let expected = if agent == "codex" {
                "resume"
            } else {
                "--resume"
            };
            assert_eq!(recipe.arguments, [expected, id]);
            assert!(hint_from_launch(&launch(agent, &[flag, id, "send this again"])).is_none());
            assert!(hint_from_launch(&launch(agent, &[flag, "a1b2c3d4"])).is_none());
        }
        for args in [
            vec!["--continue"],
            vec!["--resume", id, "--fork-session"],
            vec!["--resume", id, "--resume", id],
        ] {
            assert!(hint_from_launch(&launch("claude", &args)).is_none());
        }
        assert!(hint_from_launch(&launch("codex", &["resume", "--last"])).is_none());
        assert!(recipe_from_hint("tmux:old-compatible-hint").is_none());
        assert!(hint_from_launch(&launch("sh", &["-c", "claude --continue"])).is_none());
    }

    #[test]
    fn live_binding_preserves_permissions_without_replaying_consumed_prompts() {
        let id = "a1b2c3d4-1111-4111-8111-123456789abc";
        let claude = bind_session(
            &launch(
                "claude",
                &[
                    "--permission-mode",
                    "plan",
                    "--setting-sources",
                    "",
                    "already sent",
                ],
            ),
            id,
        )
        .unwrap();
        assert_eq!(
            claude.arguments,
            [
                "--permission-mode",
                "plan",
                "--setting-sources",
                "",
                "--resume",
                id
            ]
        );
        let codex = bind_session(
            &launch(
                "codex",
                &[
                    "--remote",
                    "unix:///tmp/account.sock",
                    "--sandbox",
                    "read-only",
                    "already sent",
                ],
            ),
            id,
        )
        .unwrap();
        assert_eq!(
            codex.arguments,
            [
                "resume",
                id,
                "--remote",
                "unix:///tmp/account.sock",
                "--sandbox",
                "read-only"
            ]
        );
        let omp = bind_session(
            &launch(
                "omp",
                &[
                    "--approval-mode",
                    "always-ask",
                    "--model",
                    "model",
                    "@prompt.md",
                    "already sent",
                ],
            ),
            id,
        )
        .unwrap();
        assert_eq!(
            omp.arguments,
            [
                "--approval-mode",
                "always-ask",
                "--model",
                "model",
                "--resume",
                id
            ]
        );
        assert!(
            bind_session(
                &launch("claude", &["--unknown-permission-mode", "open"]),
                id
            )
            .is_none()
        );
        assert!(bind_session(&launch("claude", &["--fork-session"]), id).is_none());
        assert!(bind_session(&launch("omp", &[]), "a1b2c3d4").is_none());
    }

    #[cfg(feature = "host")]
    #[test]
    fn foreground_launcher_recovers_the_native_childs_exact_identity() {
        use std::{
            ffi::CString,
            os::unix::process::CommandExt,
            process::{Child, Command, Stdio},
            time::{Duration, Instant},
        };
        struct Reap(Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                // SAFETY: the child owns a private process group created below.
                unsafe {
                    libc::kill(-(self.0.id() as i32), libc::SIGKILL);
                }
                let _ = self.0.wait();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let fifo = CString::new(directory.path().join("resume").to_str().unwrap()).unwrap();
        // SAFETY: fifo is a live NUL-terminated path in this test's private directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let id = "a1b2c3d4-1111-4111-8111-123456789abc";
        let native = directory.path().join("codex");
        std::os::unix::fs::symlink("/bin/cat", &native).unwrap();
        // cat blocks opening the FIFO, keeping a controlled argv alive without
        // launching a real agent or reading any user session/conversation.
        let child = Reap(
            Command::new("/bin/sh")
                .args(["-c", "\"$1\" resume \"$2\" & wait", "launcher"])
                .arg(&native)
                .arg(id)
                .process_group(0)
                .current_dir(directory.path())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let base = launch("/bin/sh", &[]);
        let deadline = Instant::now() + Duration::from_secs(2);
        let recipe = loop {
            if let Some(recipe) = foreground_observation(child.0.id() as libc::pid_t, &base)
                .hint
                .as_deref()
                .and_then(recipe_from_hint)
            {
                break recipe;
            }
            assert!(
                Instant::now() < deadline,
                "child argv never became available"
            );
            std::thread::yield_now();
        };
        assert_eq!(Path::new(&recipe.program), native);
        assert_eq!(recipe.arguments, ["resume", id]);
        assert_eq!(recipe.working_directory, directory.path());
        drop(child);
        assert!(foreground_observation(0, &base).hint.is_none());
    }
}
