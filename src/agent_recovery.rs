//! Exact, opt-in agent recovery evidence. Never searches a session directory or
//! selects a recent conversation; unknown launches retain their shell fallback.
use std::path::Path;
#[cfg(feature = "host")]
use std::{fs, io::Read};

use serde::{Deserialize, Serialize};

use super::LaunchRecipe;

const MAX_HINT_BYTES: usize = 16 * 1024;
const MAX_ARGV_BYTES: usize = 8 * 1024;

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
    let agent = Path::new(&launch.program).file_name()?.to_str()?;
    if !matches!(agent, "omp" | "claude" | "codex")
        || launch.arguments.len() > 128
        || launch.arguments.iter().map(String::len).sum::<usize>() > MAX_ARGV_BYTES
    {
        return None;
    }
    let mut id = None;
    let mut arguments = Vec::new();
    let mut args = launch.arguments.iter();
    while let Some(argument) = args.next() {
        let (flag, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(flag, value)| {
                (flag, Some(value))
            });
        let identity = match agent {
            "codex" => flag == "resume",
            "claude" => matches!(flag, "--resume" | "-r" | "--session-id"),
            "omp" => matches!(flag, "--resume" | "-r"),
            _ => false,
        };
        if identity {
            let value = inline.or_else(|| args.next().map(String::as_str))?;
            if id.is_some() || !exact_id(value) {
                return None;
            }
            id = Some(value.to_owned());
            continue;
        }
        let takes_value = match agent {
            "omp" => matches!(
                flag,
                "--profile"
                    | "--session-dir"
                    | "--model"
                    | "--provider"
                    | "--thinking"
                    | "--config"
                    | "--add-dir"
                    | "--approval-mode"
            ),
            "claude" => matches!(
                flag,
                "--model"
                    | "--agent"
                    | "--permission-mode"
                    | "--settings"
                    | "--setting-sources"
                    | "--effort"
            ),
            "codex" => matches!(
                flag,
                "--model"
                    | "-m"
                    | "--profile"
                    | "-p"
                    | "--config"
                    | "-c"
                    | "--sandbox"
                    | "-s"
                    | "--ask-for-approval"
                    | "-a"
                    | "--add-dir"
                    | "--remote"
                    | "--remote-auth-token-env"
            ),
            _ => false,
        };
        let switch = match agent {
            "omp" => matches!(
                flag,
                "--no-lsp"
                    | "--no-tools"
                    | "--no-extensions"
                    | "--no-skills"
                    | "--no-rules"
                    | "--auto-approve"
            ),
            "claude" => matches!(
                flag,
                "--dangerously-skip-permissions" | "--bare" | "--safe-mode" | "--no-chrome"
            ),
            "codex" => matches!(
                flag,
                "--no-alt-screen" | "--search" | "--dangerously-bypass-approvals-and-sandbox"
            ),
            _ => false,
        };
        if takes_value {
            let value = inline.or_else(|| args.next().map(String::as_str))?;
            if value.is_empty() || value.starts_with('-') || value.contains('\0') {
                return None;
            }
            arguments.extend([flag.to_owned(), value.to_owned()]);
        } else if switch && inline.is_none() {
            arguments.push(flag.to_owned());
        } else {
            // Unknown flags or positional prompts may change session identity.
            return None;
        }
    }
    let id = id?;
    if agent == "codex" {
        arguments.splice(0..0, ["resume".into(), id]);
    } else {
        arguments.extend(["--resume".into(), id]);
    }
    Some(LaunchRecipe {
        arguments,
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
/// native-chat action, but still require an explicit exact conversation choice.
pub fn observed_hint_from_launch(launch: &LaunchRecipe) -> Option<String> {
    if let Some(hint) = hint_from_launch(launch) {
        return Some(hint);
    }
    let agent = Path::new(&launch.program).file_name()?.to_str()?;
    if !matches!(agent, "omp" | "claude" | "codex")
        || launch.arguments.len() > 128
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
        2 => {
            let agent = Path::new(&hint.recipe.program).file_name()?.to_str()?;
            (matches!(agent, "omp" | "claude" | "codex")
                && hint.recipe.arguments.len() <= 128
                && hint.recipe.arguments.iter().map(String::len).sum::<usize>() <= MAX_ARGV_BYTES)
                .then_some(hint.recipe)
        }
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

/// Called on foreground-group/title changes observed with real PTY output, never
/// from a timer or presentation polling.
/// The caller obtains the foreground process group from the actual PTY. Reading
/// that group's leader avoids inspecting unrelated processes or conversation text.
#[cfg(feature = "host")]
pub(crate) fn foreground_hint(pid: libc::pid_t, base: &LaunchRecipe) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let root = format!("/proc/{pid}");
    let before = bounded_read(format!("{root}/stat"), 4096)?;
    let bytes = bounded_read(format!("{root}/cmdline"), MAX_ARGV_BYTES)?;
    let mut argv = bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| std::str::from_utf8(arg).map(str::to_owned));
    let program = argv.next()?.ok()?;
    let arguments = argv.collect::<Result<Vec<_>, _>>().ok()?;
    let working_directory = fs::read_link(format!("{root}/cwd")).ok()?;
    let after = bounded_read(format!("{root}/stat"), 4096)?;
    // Fields before starttime include counters that change continuously. Fence
    // identity with starttime (field 22), not a whole-file equality comparison.
    let start = |stat: &[u8]| -> Option<String> {
        let stat = std::str::from_utf8(stat).ok()?;
        Some(
            stat.rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(19)?
                .to_owned(),
        )
    };
    if start(&before)? != start(&after)? {
        return None;
    }
    observed_hint_from_launch(&LaunchRecipe {
        program,
        arguments,
        working_directory,
        environment: base.environment.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn launch(program: &str, arguments: &[&str]) -> LaunchRecipe {
        LaunchRecipe {
            program: program.into(),
            arguments: arguments.iter().map(|arg| (*arg).into()).collect(),
            working_directory: "/tmp".into(),
            environment: BTreeMap::new(),
        }
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
    fn process_argv_capture_recovers_only_the_observed_complete_identity() {
        use std::{
            ffi::CString,
            os::unix::process::CommandExt,
            process::{Child, Command, Stdio},
            time::{Duration, Instant},
        };
        struct Reap(Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let fifo = CString::new(directory.path().join("resume").to_str().unwrap()).unwrap();
        // SAFETY: fifo is a live NUL-terminated path in this test's private directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let id = "a1b2c3d4-1111-4111-8111-123456789abc";
        // cat blocks opening the FIFO, keeping a controlled argv alive without
        // launching a real agent or reading any user session/conversation.
        let child = Reap(
            Command::new("/bin/cat")
                .arg0("codex")
                .args(["resume", id])
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
        assert_eq!(recipe.program, "codex");
        assert_eq!(recipe.arguments, ["resume", id]);
        assert_eq!(recipe.working_directory, directory.path());
        drop(child);
        assert!(foreground_hint(0, &base).is_none());
    }
}
