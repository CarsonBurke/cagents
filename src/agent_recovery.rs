//! Exact, opt-in agent recovery evidence. Never searches a session directory or
//! selects a recent conversation; unknown launches retain their shell fallback.
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
            if value.is_empty() || value.starts_with('-') || value.contains('\0') {
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
    if identity.is_some() || provider != Provider::Omp {
        return identity.map(SessionIdentity::Exact);
    }
    // OMP publishes the exact active session by controlling terminal, including
    // fresh launches whose argv has no --resume. Never enumerate saved sessions.
    let tty = fs::read_link(root.join("fd/0")).ok()?;
    let tty = tty.strip_prefix("/dev/pts").ok()?.file_name()?.to_str()?;
    let marker = session_root
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
    let bytes = bounded_read(&marker, MAX_HINT_BYTES)?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let mut lines = text.lines();
    if Path::new(lines.next()?) != launch.working_directory {
        return None;
    }
    let session = Path::new(lines.next()?);
    session
        .starts_with(session_root.join("sessions"))
        .then_some(())?;
    // A fresh OMP terminal publishes its ID before writing a resumable session.
    // Do not authorize a stop-and-resume handoff until that file exists.
    if !fs::metadata(session).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0) {
        // Current binding is a refusal, not absence of evidence: otherwise a
        // prior --resume argument could select the old conversation.
        return Some(SessionIdentity::Ambiguous);
    }
    provider
        .session_file_id(session)
        .map(SessionIdentity::Exact)
}

#[cfg(feature = "host")]
fn process_launch(root: &Path, base: &LaunchRecipe) -> Option<(LaunchRecipe, u64, i32)> {
    read_process_launch(root, Some(base))
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
        .filter(|arg| base.is_none() || !arg.is_empty())
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
            if matches!(
                name,
                "HOME"
                    | "PATH"
                    | "CODEX_HOME"
                    | "CLAUDE_CONFIG_DIR"
                    | "PI_CODING_AGENT_DIR"
                    | "OMP_PROFILE"
            ) {
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
            return Err(());
        };
        // Runtime binding is current; stale resume argv must not win after an
        // in-process session switch. Never replay a prompt or unknown option.
        if let Some(mut recipe) = resume_recipe(&launch) {
            let flag = Provider::from_program(&recipe.program)
                .ok_or(())?
                .resume_flag();
            let index = recipe
                .arguments
                .iter()
                .position(|arg| arg == flag)
                .ok_or(())?;
            recipe.arguments[index + 1] = id;
            launch = recipe;
        } else {
            let flag = Provider::from_program(&launch.program)
                .ok_or(())?
                .resume_flag();
            launch.arguments.extend([flag.into(), id]);
        }
    }
    observed_hint_from_launch(&launch).map(Some).ok_or(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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
            &[script, "--resume", "a1b2c3d4-1111-4111-8111-123456789abc"],
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
        assert_eq!(
            inspect_process(directory.path(), 100, 100, &parent, &mut 64),
            Err(())
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
        assert!(inspect_process(proc_root, 100, 100, &recipe, &mut 64).is_err());
        fs::create_dir_all(session.parent().unwrap()).unwrap();
        fs::write(&session, "").unwrap();
        assert!(inspect_process(proc_root, 100, 100, &recipe, &mut 64).is_err());
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
        assert!(runtime_identity(&root, &recipe, 0).is_none());
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
        let hint = loop {
            if let Some(hint) = foreground_hint(child.0.id() as libc::pid_t, &base) {
                break hint;
            }
            assert!(
                Instant::now() < deadline,
                "child argv never became available"
            );
            std::thread::yield_now();
        };
        let recipe = recipe_from_hint(&hint).unwrap();
        assert_eq!(Path::new(&recipe.program), native);
        assert_eq!(recipe.arguments, ["resume", id]);
        assert_eq!(recipe.working_directory, directory.path());
        drop(child);
        assert!(foreground_hint(0, &base).is_none());
    }
}
