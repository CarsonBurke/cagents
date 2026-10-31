//! Finite external lifetime for the existing no-tool Goal Responses model.
//! This owns no SDK, daemon, profile, account, credentials or GUI process.
use super::{ModelMode, ModelRequest, serve};
use serde_json::json;
use std::{
    io::{self, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::fd::RawFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const LIFETIME: Duration = Duration::from_secs(180);
const REQUEST_TIME: Duration = Duration::from_secs(20);
const CONNECTIONS: usize = 16;
const CONCURRENT: usize = 2;

struct Handler {
    socket: TcpStream,
    started: Instant,
    task: JoinHandle<()>,
}

/// The opt-in prevents an accidental `--ignored` run from leaving a fixture
/// waiting for an operator. Root must invoke the compiled test binary directly.
pub(super) fn run() -> Result<(), String> {
    if std::env::var("CAGENTS_GOAL_GUI_MODEL").as_deref() != Ok("1") {
        return Err("set CAGENTS_GOAL_GUI_MODEL=1 for the external fixture only".into());
    }
    run_owned(libc::STDIN_FILENO, &mut io::stdout().lock(), LIFETIME)
}

fn run_owned(input: RawFd, output: &mut impl Write, lifetime: Duration) -> Result<(), String> {
    // A terminal or Cargo's /dev/null cannot stand in for the original owner's
    // retained pipe. This reads no data other than the EOF release protocol.
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(input, metadata.as_mut_ptr()) } != 0 {
        return Err("external fixture stdin could not be inspected".into());
    }
    let metadata = unsafe { metadata.assume_init() };
    if metadata.st_mode & libc::S_IFMT != libc::S_IFIFO {
        return Err("external fixture requires an original-owner stdin pipe".into());
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|_| "model bind failed")?;
    listener
        .set_nonblocking(true)
        .map_err(|_| "model nonblocking setup failed")?;
    let port = listener
        .local_addr()
        .map_err(|_| "model address unavailable")?
        .port();
    let deadline = Instant::now() + lifetime;
    let seen = Arc::new(Mutex::new(Vec::<ModelRequest>::new()));
    let over_limit = Arc::new(AtomicBool::new(false));
    let mut active = Vec::<Handler>::new();
    let mut accepted = 0;
    let mut joined = 0;
    let mut failure = None;
    writeln!(
        output,
        "GOAL_GUI_MODEL_READY {}",
        json!({
            "base_url": format!("http://127.0.0.1:{port}/v1"),
            "mode": "synthetic-no-tools", "lifetime_seconds": lifetime.as_secs(),
            "max_requests": 8, "max_connections": CONNECTIONS,
            "max_concurrent": CONCURRENT, "request_seconds": REQUEST_TIME.as_secs(),
            "max_body_bytes": 2 * 1024 * 1024, "max_header_bytes": 32 * 1024,
            "release": "owned-stdin-eof-after-sdk-retirement"
        })
    )
    .and_then(|_| output.flush())
    .map_err(|_| "ready record write failed")?;

    loop {
        let mut index = 0;
        while index < active.len() {
            if active[index].task.is_finished() {
                let handler = active.swap_remove(index);
                joined += 1;
                if handler.task.join().is_err() {
                    failure = Some("model request handler failed");
                }
            } else {
                index += 1;
            }
        }
        if failure.is_some() {
            break;
        }
        if over_limit.load(Ordering::Acquire) {
            failure = Some("model exceeded eight-request bound");
            break;
        }
        if Instant::now() >= deadline {
            failure = Some("model lifetime expired before owner release");
            break;
        }
        if active
            .iter()
            .any(|handler| handler.started.elapsed() >= REQUEST_TIME)
        {
            failure = Some("model request exceeded absolute time bound");
            break;
        }
        // Poll only the owner's pipe, not a second command/control endpoint.
        let mut descriptor = libc::pollfd {
            fd: input,
            events: libc::POLLIN,
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut descriptor, 1, 25) };
        if polled < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            failure = Some("owner pipe poll failed");
            break;
        }
        if descriptor.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            failure = Some("owner pipe became invalid");
            break;
        }
        if descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut byte = 0u8;
            let count = unsafe { libc::read(input, (&mut byte as *mut u8).cast(), 1) };
            if count != 0 {
                failure = Some("owner release must be EOF without command bytes");
            }
            break;
        }
        match listener.accept() {
            Ok((socket, address)) => {
                if !address.ip().is_loopback()
                    || accepted == CONNECTIONS
                    || active.len() == CONCURRENT
                {
                    failure = Some("model connection bound exceeded");
                    drop(socket);
                    break;
                }
                accepted += 1;
                let pin = match socket.try_clone() {
                    Ok(pin) => pin,
                    Err(_) => {
                        failure = Some("model socket retention failed");
                        break;
                    }
                };
                let seen = seen.clone();
                let over_limit = over_limit.clone();
                let started = Instant::now();
                let task =
                    thread::spawn(move || serve(socket, &seen, ModelMode::Goal, &over_limit));
                active.push(Handler {
                    socket: pin,
                    started,
                    task,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => {
                failure = Some("model accept failed");
                break;
            }
        }
    }
    // Retire the only listener first. A completed response may close its socket
    // just before its thread returns, so EOF drains these exact handlers within
    // their original deadlines rather than relying on a scheduling sleep.
    drop(listener);
    while failure.is_none() && active.iter().any(|handler| !handler.task.is_finished()) {
        if Instant::now() >= deadline
            || active
                .iter()
                .any(|handler| handler.started.elapsed() >= REQUEST_TIME)
        {
            failure = Some("model handlers did not retire before their original deadline");
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    // On failure, shut down only this fixture's own HTTP sockets so blocked
    // readers can join; never signal a provider or call its cleanup successful.
    if failure.is_some() {
        for handler in &active {
            let _ = handler.socket.shutdown(Shutdown::Both);
        }
    }
    for handler in active {
        joined += 1;
        if handler.task.join().is_err() {
            failure.get_or_insert("model request handler failed");
        }
    }
    let requests = seen.lock().map_or(0, |requests| requests.len());
    if over_limit.load(Ordering::Acquire) {
        failure.get_or_insert("model exceeded eight-request bound");
    }
    writeln!(
        output,
        "GOAL_GUI_MODEL_RETIRED {}",
        json!({
            "normal": failure.is_none(), "reason": failure.unwrap_or("owned-stdin-eof"),
            "listener_closed": true, "active_handlers": 0,
            "accepted_connections": accepted, "joined_handlers": joined,
            "model_requests": requests, "over_limit": over_limit.load(Ordering::Acquire)
        })
    )
    .and_then(|_| output.flush())
    .map_err(|_| "retirement record write failed")?;
    failure.map_or(Ok(()), |reason| Err(reason.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::File,
        io::{BufRead, BufReader, Read},
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        os::unix::net::UnixStream,
    };

    fn start() -> (File, BufReader<UnixStream>, JoinHandle<Result<(), String>>) {
        let mut pipe = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let input = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
        let release = File::from(unsafe { OwnedFd::from_raw_fd(pipe[1]) });
        let (mut output, records) = UnixStream::pair().unwrap();
        records
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let task = thread::spawn(move || {
            run_owned(input.as_raw_fd(), &mut output, Duration::from_secs(20))
        });
        (release, BufReader::new(records), task)
    }
    fn record(records: &mut BufReader<UnixStream>, prefix: &str) -> serde_json::Value {
        let mut line = String::new();
        records.read_line(&mut line).unwrap();
        serde_json::from_str(line.strip_prefix(prefix).unwrap().trim()).unwrap()
    }
    fn request(url: &str) -> String {
        let address = url
            .strip_prefix("http://")
            .unwrap()
            .strip_suffix("/v1")
            .unwrap();
        let mut socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let body = br#"{"input":[]}"#;
        write!(
            socket,
            "POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        socket.write_all(body).unwrap();
        let mut reply = String::new();
        socket.read_to_string(&mut reply).unwrap();
        reply
    }

    #[test]
    fn external_model_uses_shared_no_tool_response_then_joins_before_normal_eof_return() {
        let (release, mut records, task) = start();
        let ready = record(&mut records, "GOAL_GUI_MODEL_READY ");
        let reply = request(ready["base_url"].as_str().unwrap());
        assert!(reply.contains("GOAL_FIXTURE_OBSERVED_NO_TOOLS"));
        assert!(!reply.contains("function_call"));
        drop(release);
        let retired = record(&mut records, "GOAL_GUI_MODEL_RETIRED ");
        assert_eq!(retired["normal"], true);
        assert_eq!(retired["model_requests"], 1);
        assert_eq!(retired["accepted_connections"], retired["joined_handlers"]);
        assert_eq!(retired["active_handlers"], 0);
        assert!(task.join().unwrap().is_ok());
    }

    #[test]
    fn external_model_refuses_ninth_request_and_reports_failure_after_all_handlers_join() {
        let (release, mut records, task) = start();
        let ready = record(&mut records, "GOAL_GUI_MODEL_READY ");
        let url = ready["base_url"].as_str().unwrap();
        for _ in 0..8 {
            assert!(request(url).starts_with("HTTP/1.1 200"));
        }
        assert!(request(url).starts_with("HTTP/1.1 503"));
        let retired = record(&mut records, "GOAL_GUI_MODEL_RETIRED ");
        assert_eq!(retired["normal"], false);
        assert_eq!(retired["over_limit"], true);
        assert_eq!(retired["model_requests"], 8);
        assert_eq!(retired["active_handlers"], 0);
        assert_eq!(retired["accepted_connections"], retired["joined_handlers"]);
        assert!(task.join().unwrap().is_err());
        drop(release);
    }

    #[test]
    fn an_oversized_request_line_cannot_be_reported_as_normal_owner_eof() {
        let (release, mut records, task) = start();
        let ready = record(&mut records, "GOAL_GUI_MODEL_READY ");
        let address = ready["base_url"]
            .as_str()
            .unwrap()
            .strip_prefix("http://")
            .unwrap()
            .strip_suffix("/v1")
            .unwrap();
        let mut socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket.write_all(&vec![b'A'; 16 * 1024 + 1]).unwrap();
        // The accepted handler must actually consume/refuse the oversized line
        // before EOF release; do not let release win the listener accept race.
        let mut bytes = Vec::new();
        if let Err(error) = socket.read_to_end(&mut bytes) {
            assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
        }
        assert!(bytes.is_empty());
        drop(release);
        let retired = record(&mut records, "GOAL_GUI_MODEL_RETIRED ");
        assert_eq!(retired["normal"], false);
        assert_eq!(retired["model_requests"], 0);
        assert_eq!(retired["accepted_connections"], 1);
        assert_eq!(retired["joined_handlers"], 1);
        assert_eq!(retired["active_handlers"], 0);
        assert!(task.join().unwrap().is_err());
    }
}
