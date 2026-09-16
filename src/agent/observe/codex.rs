use super::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{self, Read, Write},
    os::unix::{fs::MetadataExt, net::UnixStream},
};
use tungstenite::{
    Error as WsError, HandshakeError, Message, WebSocket, client::client_with_config,
    protocol::WebSocketConfig,
};

/// How many of an account's open threads the conversation chooser describes.
const MAX_DESCRIBED: usize = 64;

pub(super) fn endpoint(recipe: &LaunchRecipe, home: &Path) -> ProbeResult<PathBuf> {
    let mut endpoint = home.join("app-server-control/app-server-control.sock");
    let mut args = recipe.arguments.iter();
    if Provider::is_interpreter(&recipe.program) {
        args.next();
    }
    while let Some(arg) = args.next() {
        let remote = match arg.as_str() {
            "--" => break,
            "--remote" => Some(args.next().ok_or(())?.as_str()),
            "--sock" => {
                endpoint = absolute(recipe, args.next().ok_or(())?)?;
                None
            }
            "--remote-auth-token-env" => return Err(()),
            _ if arg.starts_with("--remote-auth-token-env=") => return Err(()),
            _ if arg.starts_with("--remote=") => Some(&arg["--remote=".len()..]),
            _ if Provider::Codex.is_value_option(arg) || Provider::Codex.is_cwd_option(arg) => {
                args.next().ok_or(())?;
                None
            }
            _ => None,
        };
        if let Some(remote) = remote {
            let path = remote.strip_prefix("unix://").ok_or(())?;
            endpoint = if path.is_empty() {
                home.join("app-server-control/app-server-control.sock")
            } else {
                absolute(recipe, path)?
            };
        }
    }
    // An exact endpoint only: no discovery, server startup, TCP or fallback.
    Ok(endpoint)
}

/// Bounds even a peer continuously streaming fragments or handshake headers.
struct Stream {
    socket: UnixStream,
    deadline: Instant,
    received: usize,
}
impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline || self.received >= MAX_BYTES {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let limit = bytes.len().min(MAX_BYTES - self.received);
        let count = self.socket.read(&mut bytes[..limit])?;
        self.received += count;
        Ok(count)
    }
}
impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}

struct Client {
    socket: WebSocket<Stream>,
    deadline: Instant,
    next_id: u64,
}

fn pause(deadline: Instant) -> ProbeResult<()> {
    if Instant::now() >= deadline {
        return Err(());
    }
    std::thread::sleep(Duration::from_millis(5));
    Ok(())
}
fn blocked(error: &WsError) -> bool {
    matches!(error, WsError::Io(error) if error.kind() == io::ErrorKind::WouldBlock)
}

impl Client {
    fn connect(key: &Key) -> ProbeResult<Self> {
        let deadline = Instant::now() + TIMEOUT;
        let metadata = std::fs::metadata(&key.endpoint).map_err(|_| ())?;
        let stream = Stream {
            socket: socket::connect(&key.endpoint, metadata.dev(), metadata.ino())
                .map_err(|_| ())?,
            deadline,
            received: 0,
        };
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_BYTES))
            .max_frame_size(Some(MAX_BYTES))
            .max_write_buffer_size(MAX_BYTES);
        let mut handshake = client_with_config("ws://localhost/", stream, Some(config));
        let socket = loop {
            match handshake {
                Ok((socket, _)) => break socket,
                Err(HandshakeError::Interrupted(pending)) => {
                    pause(deadline)?;
                    handshake = pending.handshake();
                }
                Err(HandshakeError::Failure(_)) => return Err(()),
            }
        };
        Ok(Self {
            socket,
            deadline,
            next_id: 0,
        })
    }

    fn send(&mut self, value: Value) -> ProbeResult<()> {
        if Instant::now() >= self.deadline {
            return Err(());
        }
        // WouldBlock means tungstenite queued the message; never send it twice.
        match self.socket.send(Message::Text(value.to_string().into())) {
            Ok(()) => return Ok(()),
            Err(error) if blocked(&error) => {}
            Err(_) => return Err(()),
        }
        loop {
            pause(self.deadline)?;
            match self.socket.flush() {
                Ok(()) => return Ok(()),
                Err(error) if blocked(&error) => {}
                Err(_) => return Err(()),
            }
        }
    }

    fn call(&mut self, method: &str, params: Value) -> ProbeResult<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"id":id,"method":method,"params":params}))?;
        loop {
            if Instant::now() >= self.deadline {
                return Err(());
            }
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    let mut value: Value = serde_json::from_str(&text).map_err(|_| ())?;
                    if value["id"].as_u64() == Some(id) && value.get("method").is_none() {
                        if value.get("error").is_some() {
                            return Err(());
                        }
                        return value
                            .as_object_mut()
                            .and_then(|object| object.remove("result"))
                            .ok_or(());
                    }
                    // Never answer replayed approvals or subscribe to sessions.
                }
                Ok(Message::Ping(_) | Message::Pong(_)) => {}
                Ok(_) => return Err(()),
                Err(error) if blocked(&error) => pause(self.deadline)?,
                Err(_) => return Err(()),
            }
        }
    }

    fn loaded(&mut self) -> ProbeResult<BTreeSet<String>> {
        let mut ids = BTreeSet::new();
        let mut cursor: Option<String> = None;
        let mut cursors = BTreeSet::new();
        for _ in 0..MAX_ROWS {
            let result = self.call("thread/loaded/list", json!({"limit":64,"cursor":cursor}))?;
            let data = result["data"].as_array().ok_or(())?;
            if data.len() > 64 {
                return Err(());
            }
            for id in data {
                let id = id.as_str().filter(|id| !id.is_empty()).ok_or(())?;
                if !ids.insert(id.to_owned()) || ids.len() > MAX_ROWS {
                    return Err(());
                }
            }
            match result.get("nextCursor") {
                None | Some(Value::Null) => return Ok(ids),
                Some(Value::String(next)) if !next.is_empty() && cursors.insert(next.clone()) => {
                    cursor = Some(next.clone());
                }
                _ => return Err(()),
            }
        }
        Err(())
    }
}

pub(super) fn preview(key: &Key, id: &str) -> ProbeResult<(String, PathBuf)> {
    let mut client = Client::connect(key)?;
    let result = client.call("initialize", json!({
        "clientInfo":{"name":"cagents_selection","title":"cagents conversation selection","version":env!("CARGO_PKG_VERSION")},
        "capabilities":{"experimentalApi":true}
    }))?;
    socket::verify_account(&result, &key.home).map_err(|_| ())?;
    client.send(json!({"method":"initialized","params":{}}))?;
    if !client.loaded()?.contains(id) {
        return Err(());
    }
    let result = client.call("thread/read", json!({"threadId":id,"includeTurns":false}))?;
    let thread = &result["thread"];
    if thread["id"].as_str() != Some(id) || !client.loaded()?.contains(id) {
        return Err(());
    }
    let cwd = PathBuf::from(thread["cwd"].as_str().ok_or(())?);
    if !cwd.is_absolute() {
        return Err(());
    }
    let title = thread["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .or_else(|| thread["preview"].as_str())
        .unwrap_or("Untitled conversation");
    Ok((title.chars().take(240).collect(), cwd))
}

/// Every thread this account's shared server holds open, for user selection.
/// This reads metadata only: it never resumes, subscribes to or writes a thread.
pub(super) fn candidates(key: &Key) -> ProbeResult<Vec<(String, String, PathBuf)>> {
    let mut client = Client::connect(key)?;
    let result = client.call("initialize", json!({
        "clientInfo":{"name":"cagents_selection","title":"cagents conversation selection","version":env!("CARGO_PKG_VERSION")},
        "capabilities":{"experimentalApi":true}
    }))?;
    socket::verify_account(&result, &key.home).map_err(|_| ())?;
    client.send(json!({"method":"initialized","params":{}}))?;
    let ids = client.loaded()?;
    let mut candidates = Vec::with_capacity(ids.len().min(MAX_DESCRIBED));
    // Each description costs a round trip inside this probe's single deadline,
    // and a reader choosing from a list has no use for a longer one. A thread
    // past this is still reachable by its identifier.
    for id in ids.iter().take(MAX_DESCRIBED) {
        let result = client.call("thread/read", json!({"threadId":id,"includeTurns":false}))?;
        let thread = &result["thread"];
        if thread["id"].as_str() != Some(id.as_str()) {
            return Err(());
        }
        let cwd = PathBuf::from(thread["cwd"].as_str().ok_or(())?);
        // A relative cwd cannot be shown or compared against the terminal's own
        // directory, so drop that one row rather than losing the whole listing.
        if !cwd.is_absolute() {
            continue;
        }
        let title = thread["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .or_else(|| thread["preview"].as_str())
            .unwrap_or("Untitled conversation");
        candidates.push((id.clone(), title.chars().take(240).collect(), cwd));
    }
    // Pagination is not an atomic listing. Membership changes invalidate this sweep.
    if client.loaded()? != ids {
        return Err(());
    }
    Ok(candidates)
}

pub(super) fn census(key: &Key) -> ProbeResult<Census> {
    let mut client = Client::connect(key)?;
    let result = client.call("initialize", json!({
        "clientInfo":{"name":"cagents_activity","title":"cagents worktree activity","version":env!("CARGO_PKG_VERSION")},
        "capabilities":{"experimentalApi":true}
    }))?;
    socket::verify_account(&result, &key.home).map_err(|_| ())?;
    client.send(json!({"method":"initialized","params":{}}))?;
    let ids = client.loaded()?;
    let mut census = Census::new();
    for id in &ids {
        let result = client.call("thread/read", json!({"threadId":id,"includeTurns":false}))?;
        let thread = &result["thread"];
        if thread["id"].as_str() != Some(id.as_str()) {
            return Err(());
        }
        add(
            &mut census,
            thread["cwd"].as_str().ok_or(())?,
            state(&thread["status"])?,
        )?;
    }
    // Pagination is not an atomic census. Membership changes invalidate this sweep.
    if client.loaded()? != ids {
        return Err(());
    }
    Ok(census)
}

fn state(status: &Value) -> ProbeResult<ActivityState> {
    match status["type"].as_str() {
        Some("idle") => Ok(ActivityState::Idle),
        Some("active") => {
            let flags = status["activeFlags"].as_array().ok_or(())?;
            let mut waiting = false;
            for flag in flags {
                match flag.as_str() {
                    Some("waitingOnApproval" | "waitingOnUserInput") => waiting = true,
                    _ => return Err(()),
                }
            }
            Ok(if waiting {
                ActivityState::Waiting
            } else {
                ActivityState::Working
            })
        }
        Some("systemError") => Ok(ActivityState::Unknown),
        // A loaded ID became unloaded while reading; no complete live census.
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_flags_distinguish_work_from_user_wait() {
        assert_eq!(
            state(&json!({"type":"active","activeFlags":[]})),
            Ok(ActivityState::Working)
        );
        assert_eq!(
            state(&json!({"type":"active","activeFlags":["waitingOnApproval"]})),
            Ok(ActivityState::Waiting)
        );
        assert_eq!(
            state(&json!({"type":"active","activeFlags":["waitingOnUserInput"]})),
            Ok(ActivityState::Waiting)
        );
        assert_eq!(
            state(&json!({"type":"systemError"})),
            Ok(ActivityState::Unknown)
        );
        assert!(state(&json!({"type":"notLoaded"})).is_err());
        assert!(state(&json!({"type":"active","activeFlags":["futureFlag"]})).is_err());
    }
}
