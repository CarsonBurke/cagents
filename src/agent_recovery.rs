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
    let provider = Provider::from_program(&launch.program)?;
    if launch.arguments.len() > 128
        || launch.arguments.iter().map(String::len).sum::<usize>() > MAX_ARGV_BYTES
    {
        return None;
    }
    let mut id = None;
    let mut arguments = Vec::new();
    let mut working_directory = launch.working_directory.clone();
    let mut args = launch.arguments.iter();
    while let Some(argument) = args.next() {
        let (flag, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(flag, value)| {
                (flag, Some(value))
            });
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
    let id = id?;
    provider.append_resume(&mut arguments, id);
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

#[cfg(feature = "host")]
fn process_identity(stat: &[u8]) -> Option<(i32, u64)> {
    let stat = std::str::from_utf8(stat).ok()?;
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let group = fields.nth(2)?.parse().ok()?;
    Some((group, fields.nth(16)?.parse().ok()?))
}

#[cfg(feature = "host")]
#[derive(Debug, PartialEq, Eq)]
enum SessionIdentity {
    Exact(String),
    Ambiguous,
}

#[cfg(feature = "host")]
fn runtime_identity(root: &Path, launch: &LaunchRecipe, start: u64) -> Option<SessionIdentity> {
    let provider = Provider::from_program(&launch.program)?;
    let session_root = provider.session_root(launch)?;
    if provider == Provider::Omp
        && let Some(identity) = omp_terminal_identity(root, launch, start)
    {
        return Some(identity);
    }
    let mut identity = None;
    let expected = session_root.join(provider.session_directory());
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
    let started = std::time::SystemTime::now().checked_sub(age)?;
    if modified < started {
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
        session.starts_with(&session_directory).then_some(())?;
        // Fresh sessions are published before a resumable JSONL exists.
        fs::metadata(session)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
            .then_some(())?;
        provider.session_file_id(session)
    })();
    // A current but unusable marker must block stale resume argv and open files.
    Some(
        identity
            .map(SessionIdentity::Exact)
            .unwrap_or(SessionIdentity::Ambiguous),
    )
}

#[cfg(feature = "host")]
fn process_launch(root: &Path, base: &LaunchRecipe) -> Option<(LaunchRecipe, u64, i32)> {
    read_process_launch(root, Some(base))
}

#[cfg(feature = "host")]
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
            | "XDG_DATA_HOME"
            | "XDG_STATE_HOME"
            | "XDG_CACHE_HOME"
    )
}

#[cfg(feature = "host")]
fn native_runtime_environment_key(name: &str) -> bool {
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

#[cfg(feature = "host")]
fn provider_setting_environment_key(name: &str) -> bool {
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
    PREFIXES.iter().any(|prefix| {
        name.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    }) || [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "API_KEY",
        "PRIVATE_KEY",
        "CREDENTIAL",
        "AUTHENTICATION",
        "AUTHORIZATION",
        "BASE_URL",
        "ENDPOINT",
    ]
    .iter()
    .any(|part| {
        name.as_bytes()
            .windows(part.len())
            .any(|window| window.eq_ignore_ascii_case(part.as_bytes()))
    }) || name
        .split('_')
        .any(|part| part.eq_ignore_ascii_case("AUTH"))
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
            return (provider_setting_environment_key(name)
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

/// Construct durable native execution configuration without copying arbitrary
/// process secrets into a session descriptor. New environment-only credentials
/// require explicit configuration before the source may be stopped.
#[cfg(feature = "host")]
fn preserved_native_environment(
    bytes: &[u8],
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
        } else if provider_setting_environment_key(name) {
            return Err(format!(
                "The source uses environment-only setting {name}, which cannot be safely saved for native resume. Configure it in the provider account or the explicit terminal launch before converting; the source has not been stopped."
            ));
        }
    }
    // Absent explicit variables stay absent; never restore stale launch values.
    Ok(environment)
}

#[cfg(feature = "host")]
pub(crate) fn native_environment(
    process: &AgentProcess,
    explicit: &std::collections::BTreeMap<String, String>,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let root = std::path::PathBuf::from(format!("/proc/{}", process.pid));
    let before = bounded_read(root.join("stat"), 4096)
        .and_then(|bytes| process_identity(&bytes))
        .filter(|(_, start)| *start == process.start)
        .ok_or("The source process changed before reading its execution environment")?;
    let bytes = bounded_read(root.join("environ"), 128 * 1024)
        .ok_or("Cannot read the exact source execution environment; no source was stopped")?;
    let environment = preserved_native_environment(&bytes, explicit)?;
    // Native execution clears inheritance, but terminal recipes historically
    // inherit the daemon environment on return. Refuse unrepresentable absences
    // before changing the source rather than adding secrets or a recipe schema.
    for (name, value) in std::env::vars_os() {
        let Some(name) = name.to_str() else {
            continue;
        };
        if !return_environment_key(process.provider, name) {
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
    Ok(environment)
}

#[cfg(feature = "host")]
fn read_process_launch(
    root: &Path,
    base: Option<&LaunchRecipe>,
) -> Option<(LaunchRecipe, u64, i32)> {
    let before = process_identity(&bounded_read(root.join("stat"), 4096)?)?;
    let bytes = bounded_read(root.join("cmdline"), MAX_ARGV_BYTES)?;
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(&bytes);
    let mut argv = bytes
        .split(|byte| *byte == 0)
        .map(|arg| std::str::from_utf8(arg).map(str::to_owned));
    let program = argv.next()?.ok()?;
    let arguments = argv.collect::<Result<Vec<_>, _>>().ok()?;
    if base.is_none() && arguments.len() > 128 {
        return None;
    }
    let working_directory = fs::read_link(root.join("cwd")).ok()?;
    let mut environment = base
        .map(|base| base.environment.clone())
        .unwrap_or_default();
    // Recovery retains its existing fallback. Activity requires the actual
    // allowlisted environment: missing keys must not inherit terminal overrides.
    let bytes = bounded_read(root.join("environ"), 128 * 1024);
    if base.is_none() && bytes.is_none() {
        return None;
    }
    if let Some(bytes) = bytes {
        for entry in bytes.split(|byte| *byte == 0) {
            let Some(index) = entry.iter().position(|byte| *byte == b'=') else {
                continue;
            };
            let Ok(name) = std::str::from_utf8(&entry[..index]) else {
                continue;
            };
            if observation_environment_key(name) {
                let value = match std::str::from_utf8(&entry[index + 1..]) {
                    Ok(value) => value,
                    Err(_) if base.is_some() => continue,
                    Err(_) => return None,
                };
                environment.insert(name.into(), value.into());
            }
        }
    }
    let launch = LaunchRecipe {
        program,
        arguments,
        working_directory,
        environment,
    };
    (before == process_identity(&bounded_read(root.join("stat"), 4096)?)?)
        .then_some((launch, before.1, before.0))
}

/// Follow the actual PTY foreground group's launcher children, not unrelated
/// processes or the shared Codex server (which can own many conversations).
#[cfg(feature = "host")]
pub(crate) fn foreground_hint(pid: libc::pid_t, base: &LaunchRecipe) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let (group, _) = process_identity(&bounded_read(format!("/proc/{pid}/stat"), 4096)?)?;
    inspect_process(Path::new("/proc"), pid, group, base, &mut 64)
        .ok()
        .flatten()
}

/// Observe raw executable/script/account identity through the same bounded
/// foreground walker as recovery, without resolving conversation authority.
#[cfg(feature = "host")]
pub(crate) fn foreground_agent(pid: libc::pid_t) -> Option<AgentProcess> {
    if pid <= 0 {
        return None;
    }
    let (group, _) = process_identity(&bounded_read(format!("/proc/{pid}/stat"), 4096)?)?;
    inspect_agent_process(Path::new("/proc"), pid, group, &mut 64)
        .ok()
        .flatten()
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
        &|root| read_process_launch(root, None),
        &|root, pid, mut launch, start| {
            let provider = Provider::recognize_launch(&launch).ok_or(())?;
            let recovery = Provider::normalize_launch(launch.clone()).ok_or(())?;
            // Recognition uses argv, execution uses the actual installation.
            // Native Claude installations can have a version-only exe basename.
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
        },
    )
}

// Err means inspection is incomplete or conflicting, never "no agent": a
// forwarding ancestor must not upgrade it to exact evidence from stale argv.
#[cfg(feature = "host")]
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
        &|root| process_launch(root, base),
        &recovery_observation,
    )
}

#[cfg(feature = "host")]
fn walk_foreground<T: PartialEq>(
    proc_root: &Path,
    pid: i32,
    group: i32,
    remaining: &mut usize,
    read_launch: &impl Fn(&Path) -> Option<(LaunchRecipe, u64, i32)>,
    observe: &impl Fn(&Path, i32, LaunchRecipe, u64) -> Result<Option<T>, ()>,
) -> Result<Option<T>, ()> {
    *remaining = remaining.checked_sub(1).ok_or(())?;
    let root = proc_root.join(pid.to_string());
    let (raw, start, observed_group) = read_launch(&root).ok_or(())?;
    if observed_group != group {
        return Ok(None);
    }
    let interpreter = Provider::is_interpreter(&raw.program);
    let native_agent = Provider::from_program(&raw.program).is_some();
    let recognized = Provider::recognize_launch(&raw);
    if native_agent && recognized.is_none() {
        return Ok(None);
    }
    // Claude and OMP run directly inside their JS interpreters and own their
    // tool children just like native agents. Only Codex's recognized JS
    // launcher forwards ownership to a native child.
    let forwarding_launcher =
        interpreter && recognized.is_some_and(Provider::forwards_to_native_child);
    if forwarding_launcher || recognized.is_none() {
        let mut child_hint = None;
        let bytes = bounded_read(root.join(format!("task/{pid}/children")), 4096).ok_or(())?;
        for child in std::str::from_utf8(&bytes)
            .map_err(|_| ())?
            .split_whitespace()
        {
            if let Some(hint) = walk_foreground(
                proc_root,
                child.parse().map_err(|_| ())?,
                group,
                remaining,
                read_launch,
                observe,
            )? {
                if child_hint
                    .as_ref()
                    .is_some_and(|previous| previous != &hint)
                {
                    return Err(());
                }
                child_hint = Some(hint);
            }
        }
        if child_hint.is_some() {
            return Ok(child_hint);
        }
    }
    if recognized.is_none() {
        return Ok(None);
    }
    let observation = observe(&root, pid, raw, start)?;
    if process_identity(&bounded_read(root.join("stat"), 4096).ok_or(())?).ok_or(())?
        != (group, start)
    {
        return Err(());
    }
    Ok(observation)
}

#[cfg(feature = "host")]
fn recovery_observation(
    root: &Path,
    _pid: i32,
    raw: LaunchRecipe,
    start: u64,
) -> Result<Option<String>, ()> {
    let mut launch = Provider::normalize_launch(raw).ok_or(())?;
    if let Some(identity) = runtime_identity(root, &launch, start) {
        let SessionIdentity::Exact(id) = identity else {
            // Recognition still exposes activation, but never stale resume authority.
            let hint = serde_json::to_string(&AgentResumeHint {
                version: 2,
                recipe: launch,
            })
            .map_err(|_| ())?;
            return Ok((hint.len() <= MAX_HINT_BYTES).then_some(hint));
        };
        launch = bind_session(&launch, &id).ok_or(())?;
    }
    observed_hint_from_launch(&launch).map(Some).ok_or(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[cfg(feature = "host")]
    #[test]
    fn credential_detection_distinguishes_authentication_from_author_metadata() {
        assert!(!provider_setting_environment_key("CARGO_PKG_AUTHORS"));
        assert!(!provider_setting_environment_key("GIT_AUTHOR_NAME"));
        for key in [
            "CUSTOM_AUTH",
            "CUSTOM_AUTHORIZATION",
            "CUSTOM_AUTHENTICATION",
        ] {
            assert!(provider_setting_environment_key(key));
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
            &explicit,
        ).unwrap();
        assert_eq!(environment["ANTHROPIC_API_KEY"], "explicit-key");
        assert_eq!(environment["ANTHROPIC_BASE_URL"], "https://source.invalid");
        assert_eq!(environment["CLAUDE_CONFIG_DIR"], "/current-account");
        assert_eq!(environment["SSL_CERT_DIR"], "/current-ca");
        assert_eq!(environment["LD_LIBRARY_PATH"], "/source/lib");
        assert!(!environment.contains_key("REMOVED_SETTING"));
        let unset = preserved_native_environment(b"HOME=/source\0", &explicit).unwrap();
        assert!(!unset.contains_key("ANTHROPIC_API_KEY"));
        assert!(!unset.contains_key("ANTHROPIC_BASE_URL"));
        assert!(!unset.contains_key("SSL_CERT_DIR"));
        let new_secret = preserved_native_environment(
            b"HOME=/source\0ANTHROPIC_API_KEY=new-shell-secret\0",
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(!new_secret.contains("new-shell-secret"));
        assert!(
            preserved_native_environment(
                b"HOME=/source\0ANTHROPIC_API_KEY=changed-secret\0",
                &explicit
            )
            .is_err()
        );
        assert!(
            preserved_native_environment(
                b"HOME=/source\0CLAUDE_CODE_PERMISSION_MODE=bypassPermissions\0",
                &BTreeMap::new()
            )
            .is_err()
        );
        assert!(
            preserved_native_environment(b"HOME=/source\0HOME=/other\0", &BTreeMap::new()).is_err()
        );
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
            program: program.into(),
            arguments: arguments.iter().map(|arg| (*arg).into()).collect(),
            working_directory: "/tmp".into(),
            environment: BTreeMap::new(),
        }
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
            runtime_identity(&root, &recipe, 0),
            Some(SessionIdentity::Exact(id.into()))
        );
        let hint = inspect_process(proc_root, 100, 100, &recipe, &mut 64)
            .unwrap()
            .unwrap();
        assert_eq!(recipe_from_hint(&hint).unwrap().arguments, ["--resume", id]);
        fs::File::options()
            .write(true)
            .open(&marker)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        assert!(runtime_identity(&root, &recipe, 0).is_none());
        fs::write(
            &marker,
            format!("/another-worktree\n{}\n", session.display()),
        )
        .unwrap();
        assert_eq!(
            runtime_identity(&root, &recipe, 0),
            Some(SessionIdentity::Ambiguous)
        );
        fs::remove_file(&marker).unwrap();
        // A different terminal's valid marker is not a fallback.
        fs::write(
            agent.join("terminal-sessions/pts-124"),
            format!("/tmp\n{}\n", session.display()),
        )
        .unwrap();
        assert!(runtime_identity(&root, &recipe, 0).is_none());
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
        assert!(runtime_identity(&root, &recipe, 0).is_none());
        fs::write(root.join("fdinfo/3"), "flags:\t0100001\n").unwrap();
        assert_eq!(
            runtime_identity(&root, &recipe, 0),
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
            runtime_identity(&root, &recipe, 0),
            Some(SessionIdentity::Ambiguous)
        );
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
            if let Some(recipe) = foreground_hint(child.0.id() as libc::pid_t, &base)
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
        assert!(foreground_hint(0, &base).is_none());
    }
}
