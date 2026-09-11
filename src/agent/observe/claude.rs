use super::*;
use serde_json::Value;
use std::{
    io::{self, Read},
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
    process::{Child, Command, Stdio},
};

pub(super) fn executable(recipe: &LaunchRecipe) -> ProbeResult<PathBuf> {
    let program = Path::new(&recipe.program);
    if program.components().count() > 1 || program.is_absolute() {
        return absolute(recipe, &recipe.program);
    }
    let path = recipe.environment.get("PATH").ok_or(())?;
    std::env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(program))
        .find(|path| {
            std::fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or(())
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Also reaps all error and timeout exits; no detached reader threads.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn nonblocking(pipe: &impl AsRawFd) -> ProbeResult<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(());
    }
    Ok(())
}

fn drain(
    pipe: &mut impl Read,
    mut collect: impl FnMut(&[u8]) -> ProbeResult<()>,
    deadline: Instant,
) -> ProbeResult<bool> {
    let mut buffer = [0; 8192];
    loop {
        if Instant::now() >= deadline {
            return Err(());
        }
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => collect(&buffer[..count])?,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(()),
        }
    }
}

fn listing(recipe: &LaunchRecipe) -> ProbeResult<Vec<Value>> {
    let deadline = Instant::now() + TIMEOUT;
    let mut command = Command::new(executable(recipe)?);
    if Provider::is_interpreter(&recipe.program) {
        command.arg(recipe.arguments.first().ok_or(())?);
    }
    // Never forward prompts/launch flags or inherit the daemon's account environment.
    command
        .args(["agents", "--json"])
        .env_clear()
        .envs(&recipe.environment)
        .current_dir(&recipe.working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGuard(command.spawn().map_err(|_| ())?);
    let mut stdout = child.0.stdout.take().ok_or(())?;
    let mut stderr = child.0.stderr.take().ok_or(())?;
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let mut out = Vec::new();
    let mut error_bytes = 0;
    loop {
        let out_done = drain(
            &mut stdout,
            |bytes| {
                if out.len() + bytes.len() > MAX_BYTES {
                    return Err(());
                }
                out.extend_from_slice(bytes);
                Ok(())
            },
            deadline,
        )?;
        let err_done = drain(
            &mut stderr,
            |bytes| {
                error_bytes += bytes.len();
                if error_bytes > 64 * 1024 {
                    return Err(());
                }
                Ok(())
            },
            deadline,
        )?;
        if let Some(status) = child.0.try_wait().map_err(|_| ())? {
            if !status.success() {
                return Err(());
            }
            if out_done && err_done {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let rows: Vec<Value> = serde_json::from_slice(&out).map_err(|_| ())?;
    (rows.len() <= MAX_ROWS).then_some(rows).ok_or(())
}

pub(super) fn exact_session(recipe: &LaunchRecipe, pid: i32) -> ProbeResult<Option<String>> {
    session_for_process(&listing(recipe)?, pid)
}

fn session_for_process(rows: &[Value], pid: i32) -> ProbeResult<Option<String>> {
    let mut identity = None;
    for row in rows {
        if row["kind"] != "interactive" || row["pid"].as_i64() != Some(i64::from(pid)) {
            continue;
        }
        let id = row["sessionId"]
            .as_str()
            .filter(|id| id.len() == 36 && uuid::Uuid::parse_str(id).is_ok())
            .ok_or(())?;
        if identity.as_deref().is_some_and(|previous| previous != id) {
            return Err(());
        }
        identity = Some(id.to_owned());
    }
    Ok(identity)
}

pub(super) fn census(recipe: &LaunchRecipe) -> ProbeResult<Census> {
    census_rows(&listing(recipe)?)
}

#[cfg(test)]
fn parse(bytes: &[u8]) -> ProbeResult<Census> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| ())?;
    let rows = value.as_array().ok_or(())?;
    if rows.len() > MAX_ROWS {
        return Err(());
    }
    census_rows(rows)
}

fn census_rows(rows: &[Value]) -> ProbeResult<Census> {
    let mut census = Census::new();
    for row in rows {
        let cwd = row["cwd"].as_str().ok_or(())?;
        let state = match row["kind"].as_str() {
            Some("background") => match row["state"].as_str() {
                // A background job between steps is still doing requested work.
                Some("working") => ActivityState::Working,
                Some("blocked") => ActivityState::Waiting,
                Some("done" | "failed" | "stopped") => ActivityState::Idle,
                _ => return Err(()),
            },
            Some("interactive") => {
                if row["pid"].as_u64().is_none_or(|pid| pid == 0) {
                    return Err(());
                }
                match row["status"].as_str() {
                    Some("busy") => ActivityState::Working,
                    Some("waiting") => ActivityState::Waiting,
                    Some("idle") => ActivityState::Idle,
                    _ => return Err(()),
                }
            }
            _ => return Err(()),
        };
        add(&mut census, cwd, state)?;
    }
    Ok(census)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_between_steps_and_exact_cwd_aggregation() {
        let census = parse(
            br#"[
            {"cwd":"/a","kind":"background","state":"working","status":"idle"},
            {"cwd":"/a","kind":"interactive","pid":42,"status":"waiting"},
            {"cwd":"/a/child","kind":"background","state":"blocked"},
            {"cwd":"/b","kind":"background","state":"done","status":"idle"}
        ]"#,
        )
        .unwrap();
        assert_eq!(census[Path::new("/a")], ActivityState::Working);
        assert_eq!(census[Path::new("/a/child")], ActivityState::Waiting);
        assert_eq!(census[Path::new("/b")], ActivityState::Idle);
    }

    #[test]
    fn unsupported_row_invalidates_partial_census() {
        assert!(
            parse(
                br#"[
            {"cwd":"/a","kind":"interactive","pid":42,"status":"busy"},
            {"cwd":"/a","kind":"interactive","pid":43,"status":"new-status"}
        ]"#
            )
            .is_err()
        );
    }

    #[test]
    fn exact_identity_requires_matching_interactive_pid_not_directory_or_recency() {
        let rows = vec![
            serde_json::json!({"kind":"interactive","pid":41,"cwd":"/same","sessionId":"11111111-1111-4111-8111-111111111111"}),
            serde_json::json!({"kind":"interactive","pid":42,"cwd":"/same","sessionId":"22222222-2222-4222-8222-222222222222"}),
            serde_json::json!({"kind":"background","pid":42,"sessionId":"33333333-3333-4333-8333-333333333333"}),
        ];
        assert_eq!(
            session_for_process(&rows, 42).unwrap().as_deref(),
            Some("22222222-2222-4222-8222-222222222222")
        );
        assert_eq!(session_for_process(&rows, 43).unwrap(), None);
        let mut ambiguous = rows;
        ambiguous.push(serde_json::json!({"kind":"interactive","pid":42,"sessionId":"44444444-4444-4444-8444-444444444444"}));
        assert!(session_for_process(&ambiguous, 42).is_err());
    }
}
