//! A slow client of a serving helper delays no other client.
use super::*;
use std::io::Write;

/// Run `test` in a child process with its own runtime directory and daemon
/// lease, so the helper's real caller gate admits this process.
pub(super) fn isolated(variable: &str, test: impl FnOnce()) {
    if std::env::var_os(variable).is_none() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let name = thread::current().name().unwrap().to_owned();
        let output = ChildCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--include-ignored", "--nocapture"])
            .env(variable, "1")
            .env("XDG_RUNTIME_DIR", root.path())
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("HOME", root.path().join("home"))
            .env("CAGENTS_LOCAL_DEBUG", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        return;
    }
    test();
}

pub(super) fn leased_proof(proof: &mut HostSessionProof) -> crate::daemon::HostLease {
    proof.boot_id = crate::daemon::read_boot_id().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    crate::daemon::HostLease::acquire(&paths, proof).unwrap()
}

/// A resident helper serving on its own thread, and a request it answers.
struct Serving {
    request: Invocation,
    socket: std::path::PathBuf,
    startup: UnixStream,
    serving: thread::JoinHandle<Result<(), String>>,
    journal: Arc<Mutex<Journal>>,
    _lease: crate::daemon::HostLease,
    _root: tempfile::TempDir,
}
impl Serving {
    fn start() -> Self {
        let (mut server, root) = tests::resident_server(None);
        let lease = leased_proof(&mut server.proof);
        // Uninstalled, so hanging up the startup stream ends `serve`.
        server.installed = false;
        let request = Invocation {
            proof: server.proof.clone(),
            epoch: Some(server.epoch),
            method: "state".into(),
            args: json!([0]),
        };
        let socket = server.socket.clone();
        let journal = server.journal.clone();
        let (control, startup) = UnixStream::pair().unwrap();
        let serving = thread::spawn(move || server.serve(control, None));
        let _: Attached = receive::<Result<Attached, String>>(&startup, TIMEOUT)
            .unwrap()
            .unwrap();
        Self {
            request,
            socket,
            startup,
            serving,
            journal,
            _lease: lease,
            _root: root,
        }
    }
    /// Ask for the state, which must arrive within `budget`.
    fn state_within(&self, budget: Duration) {
        let deadline = Instant::now() + budget;
        let client = UnixStream::connect(&self.socket).unwrap();
        deadline_io::send_until(&client, &self.request, deadline).unwrap();
        let state = deadline_io::receive_until::<Result<Value, String>>(&client, deadline)
            .unwrap()
            .unwrap();
        assert!(state.is_object(), "{state}");
    }
    /// Send `method` with `args` under the attachment's epoch, or `epoch`.
    fn ask(&self, method: &str, args: Value, epoch: Option<uuid::Uuid>) -> UnixStream {
        let client = UnixStream::connect(&self.socket).unwrap();
        let request = Invocation {
            proof: self.request.proof.clone(),
            epoch: epoch.or(self.request.epoch),
            method: method.into(),
            args,
        };
        deadline_io::send_until(&client, &request, Instant::now() + TIMEOUT).unwrap();
        client
    }
    fn answer(client: &UnixStream, budget: Duration) -> Result<Value, String> {
        deadline_io::receive_until::<Result<Value, String>>(client, Instant::now() + budget)
            .unwrap()
    }
    /// A client that sends half a frame header and then stops.
    fn stall(&self) -> UnixStream {
        let mut stalled = UnixStream::connect(&self.socket).unwrap();
        stalled.write_all(&[1, 0]).unwrap();
        stalled
    }
    /// Whether the helper has given up on a stalled client: it answers a
    /// client whose request did not arrive in time with an error frame.
    fn timed_out(stalled: &UnixStream) -> bool {
        let mut ready = libc::pollfd {
            fd: stalled.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut ready, 1, 0) };
        assert!(polled >= 0, "{}", std::io::Error::last_os_error());
        polled > 0
    }
    fn stop(self) {
        drop(self.startup);
        self.serving.join().unwrap().unwrap();
        assert!(!self.socket.exists());
    }
}

#[test]
fn a_stalled_client_does_not_delay_another_clients_request() {
    isolated("CAGENTS_RETAINED_SERVE_STALL_TEST", || {
        let serving = Serving::start();
        let stalled = serving.stall();
        for _ in 0..3 {
            // Before, the helper spent the stalled client's whole five-second
            // read budget first, so it answered only after giving up on it.
            serving.state_within(TIMEOUT);
            assert!(
                !Serving::timed_out(&stalled),
                "answered only after the stalled client timed out"
            );
        }
        serving.stop();
        drop(stalled);
    });
}

#[test]
fn a_full_helper_answers_a_waiting_client_once_a_stalled_one_times_out() {
    isolated("CAGENTS_RETAINED_SERVE_CAPACITY_TEST", || {
        let serving = Serving::start();
        let stalled: Vec<_> = (0..connections::CAPACITY)
            .map(|_| serving.stall())
            .collect();
        // Every worker is held, so this client waits in the listen backlog
        // until a stalled client's read deadline frees one. The stalled
        // clients stay connected, so a helper that never freed a worker
        // would not answer.
        let started = Instant::now();
        serving.state_within(TIMEOUT + crate::test_timing::EVENTUALLY);
        assert!(
            stalled.iter().any(Serving::timed_out),
            "answered while every worker was held"
        );
        // A helper that held a worker past its client's read budget would
        // answer only after another one.
        let elapsed = started.elapsed();
        assert!(elapsed < TIMEOUT * 2, "answered after {elapsed:?}");
        // The freed workers serve again.
        serving.state_within(TIMEOUT);
        serving.stop();
        drop(stalled);
    });
}

#[test]
fn a_half_closed_client_gets_its_reply_and_frees_its_worker() {
    isolated("CAGENTS_RETAINED_HALF_CLOSED_CALL_TEST", || {
        let mut proof = RemoteActor::fixture().0.proof.clone();
        let _lease = leased_proof(&mut proof);
        let connections = connections::Connections::new(Default::default()).unwrap();
        let request = Invocation {
            proof,
            epoch: None,
            method: "state".into(),
            args: json!([0]),
        };
        let next = || {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                if let Some(call) = connections.next() {
                    return call;
                }
                assert!(Instant::now() < deadline, "no call was queued");
                thread::sleep(Duration::from_millis(1));
            }
        };

        // Half-closing after the request still waits for the reply.
        let (client, served) = UnixStream::pair().unwrap();
        connections.admit(served);
        send(&client, &request, TIMEOUT).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let (_, reply) = next().into_parts();
        reply
            .send(connections::Answer::Value(Ok(json!("answered"))))
            .wait();
        assert_eq!(
            receive::<Result<Value, String>>(&client, TIMEOUT).unwrap(),
            Ok(json!("answered"))
        );

        // The finished worker waits, idle, for the next client.
        let deadline = Instant::now() + TIMEOUT;
        while connections.workers() != (1, 1) {
            assert!(Instant::now() < deadline, "{:?}", connections.workers());
            thread::sleep(Duration::from_millis(1));
        }

        // It takes the next client, whose request still runs after it hangs
        // up: an input's sender awaits its receipt, not this reply.
        let (client, served) = UnixStream::pair().unwrap();
        connections.admit(served);
        assert_eq!(connections.workers(), (0, 1));
        send(&client, &request, TIMEOUT).unwrap();
        drop(client);
        assert_eq!(next().method(), "state");
    });
}

#[test]
fn a_read_is_answered_while_the_serve_loop_is_busy() {
    isolated("CAGENTS_RETAINED_READ_BESIDE_LOOP_TEST", || {
        let serving = Serving::start();
        let files = json!([Command::LoadFileChanges {
            request_id: 1,
            after: None
        }]);
        // The loop waits for the journal to answer for the state, as it
        // waits for a busy journal or store on a loaded computer.
        let journal = serving.journal.lock().unwrap();
        let state = serving.ask("state", json!([0]), None);
        thread::sleep(Duration::from_millis(200));
        // Before, the query waited in the loop's queue behind the state.
        let query = serving.ask("file_query", files.clone(), None);
        let answered = Serving::answer(&query, TIMEOUT / 2).unwrap();
        assert!(answered.is_object(), "{answered}");
        // A superseded attachment is still refused beside the loop.
        let stale = serving.ask("file_query", files, Some(uuid::Uuid::new_v4()));
        assert!(
            Serving::answer(&stale, TIMEOUT / 2)
                .unwrap_err()
                .contains("superseded")
        );
        drop(journal);
        assert!(Serving::answer(&state, TIMEOUT).unwrap().is_object());
        serving.stop();
    });
}

#[test]
fn reads_follow_the_attachment_the_serve_loop_publishes() {
    let (mut server, _root) = tests::resident_server(Some("A retained answer".into()));
    let reads = Arc::new(reads::Reads::default());
    let serving = reads::Serving(reads.clone());
    reads.publish(&server);
    let ask = |reads: &reads::Reads, method: &str, epoch, args| {
        match reads.answer(Invocation {
            proof: server.proof.clone(),
            epoch: Some(epoch),
            method: method.into(),
            args,
        }) {
            Some(connections::Answer::Value(answer)) => Some(answer),
            Some(_) => panic!("{method} answered other than as a value"),
            None => None,
        }
    };
    let files = json!([Command::LoadFileChanges {
        request_id: 1,
        after: None
    }]);
    let transcript = json!([server.source.id, null]);
    let first = server.epoch;
    assert!(ask(&reads, "file_query", first, files.clone()).unwrap().is_ok());
    assert!(reads::is_read("presentation_binary") && reads::is_read("text_copy"));
    assert!(
        !reads::is_read("state") && !reads::is_read("input"),
        "the serve loop answers everything but reads"
    );

    // An attachment that supersedes the first, close-only: the first is
    // refused, and the new one may read the transcript but not query files.
    server.epoch = uuid::Uuid::new_v4();
    server.close_only = true;
    reads.publish(&server);
    assert!(
        ask(&reads, "presentation", first, transcript.clone())
            .unwrap()
            .unwrap_err()
            .contains("superseded")
    );
    assert_eq!(
        ask(&reads, "file_query", server.epoch, files.clone()).unwrap(),
        Err(close_only_refusal())
    );
    let presentation = ask(&reads, "presentation", server.epoch, transcript.clone()).unwrap();
    assert!(presentation.unwrap().is_object());

    // Once the loop stops, a read goes unanswered, as the loop would leave it.
    drop(serving);
    assert!(ask(&reads, "presentation", server.epoch, transcript).is_none());
    server.actor.close().unwrap();
}
