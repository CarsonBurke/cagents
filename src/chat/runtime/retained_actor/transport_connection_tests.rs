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
    // The fixture process supplies the same pre-exec tracker as a native
    // retained helper. Never enable process-wide sweeping in the harness.
    #[cfg(target_os = "macos")]
    crate::daemon::adopted_orphans::become_sweeper();
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
    _lease: crate::daemon::HostLease,
    _root: tempfile::TempDir,
}
impl Serving {
    fn start() -> Self {
        Self::start_server(tests::resident_server(None))
    }

    fn start_server((mut server, root): (Server, tempfile::TempDir)) -> Self {
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
            _lease: lease,
            _root: root,
        }
    }
    fn send_request(&self, method: &str, args: Value, deadline: Instant) -> UnixStream {
        let client = UnixStream::connect(&self.socket).unwrap();
        let request = Invocation {
            proof: self.request.proof.clone(),
            epoch: self.request.epoch,
            method: method.into(),
            args,
        };
        deadline_io::send_until(&client, &request, deadline).unwrap();
        client
    }

    fn query<T: DeserializeOwned>(
        &self,
        method: &str,
        args: Value,
        deadline: Instant,
    ) -> Result<T, String> {
        let client = self.send_request(method, args, deadline);
        deadline_io::receive_until(&client, deadline).unwrap()
    }

    /// Ask for the state, which must arrive within `budget`.
    fn state_within(&self, budget: Duration) {
        let state: Value = self
            .query("state", json!([0]), Instant::now() + budget)
            .unwrap();
        assert!(state.is_object(), "{state}");
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
fn reads_finish_while_the_serve_loop_and_store_writer_are_held() {
    isolated("CAGENTS_RETAINED_READ_BYPASS_TEST", || {
        let body = "Copy this transcript λ";
        let fixture = tests::resident_server(Some(body.into()));
        let server = &fixture.0;
        let id = server.source.id;
        let source = server.source.clone();
        let record = server.identity.record.clone();
        let actor = server.actor.clone();
        let journal = server.journal.clone();
        let state_reads = server.state_reads.clone();
        let changed = server.changed.clone();
        let snapshot = actor.snapshot(None).unwrap();
        let copy = snapshot.text_copy_sources.conversation[0].as_ref().unwrap();
        let mut serving = Serving::start_server(fixture);

        // A cold reader gets the existing retry boundary while the writer
        // is held. Attach and the first watch need no transcript hydration.
        let write_store = actor.0.store.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let cold = serving.send_request("presentation", json!([id, null]), deadline);
        let error =
            deadline_io::receive_until::<Result<Value, String>>(&cold, deadline).unwrap_err();
        assert_eq!(error, crate::chat::failure::actor_transport::ENDED_EARLY);
        drop(write_store);
        let established: Snapshot = serving
            .query("snapshot", json!([null]), Instant::now() + TIMEOUT)
            .unwrap();
        assert_eq!(established.items, snapshot.items);

        // The serial request enters the real serve loop and waits for the
        // journal. The later connections cannot already have been accepted.
        let journal = journal.lock().unwrap();
        let serial = serving.send_request("state", json!([0]), Instant::now() + TIMEOUT);
        crate::test_timing::eventually("serve loop entered state", || {
            state_reads.load(Ordering::Acquire) != 0
        });

        let files = Command::LoadFileChanges {
            request_id: 7,
            after: None,
        };
        let mut write_store = actor.0.store.lock().unwrap();
        let before_refresh = changed.load(Ordering::Acquire);
        let budget = Duration::from_secs(2);
        let reply: Option<chat::FileChangeReply> = serving
            .query("file_query", json!([files]), Instant::now() + budget)
            .unwrap();
        let Some(chat::FileChangeReply::Files(summary)) = reply else {
            panic!("missing changed files summary");
        };
        assert_eq!(summary.request_id, 7);
        let page: chat::TextCopyPage = serving
            .query("text_copy", json!([copy, 0]), Instant::now() + budget)
            .unwrap();
        assert_eq!(page.text, body);

        // The transcript publication is already in the journal's lineage.
        // Fetch it while that journal is still inaccessible to the serve loop.
        let deadline = Instant::now() + budget;
        let client = serving.send_request(
            "presentation_binary",
            json!([crate::terminal_ipc::PROTOCOL_VERSION, id, null]),
            deadline,
        );
        let bytes = deadline_io::receive_bytes_until(&client, deadline).unwrap();
        let response: Result<crate::terminal_ipc::ServerResponse, String> =
            postcard::from_bytes(&bytes).unwrap();
        let response = response.unwrap();
        let presented = presentation_metadata(&response).unwrap();
        assert_eq!(presented.items, snapshot.items);

        // A reset discards the previous cached body as soon as it begins,
        // even while the actor has yet to apply the new resident metadata.
        write_store
            .apply(chat::Event::Reset {
                reason: "fixture reset".into(),
            })
            .unwrap();
        let deadline = Instant::now() + budget;
        let after_reset = serving.send_request(
            "presentation_binary",
            json!([crate::terminal_ipc::PROTOCOL_VERSION, id, null]),
            deadline,
        );
        let error = deadline_io::receive_bytes_until(&after_reset, deadline).unwrap_err();
        assert_eq!(error, crate::chat::failure::actor_transport::ENDED_EARLY);
        drop(write_store);
        assert!(!Serving::timed_out(&serial));

        drop(journal);
        let _: Value = receive::<Result<Value, String>>(&serial, TIMEOUT)
            .unwrap()
            .unwrap();
        crate::test_timing::eventually("the helper reannounces the available transcript", || {
            changed.load(Ordering::Acquire) > before_refresh
        });

        let stale = serving.request.epoch.take();
        let attached: Attached = serving
            .query(
                "attach_close",
                json!([source, record]),
                Instant::now() + TIMEOUT,
            )
            .unwrap();
        serving.request.epoch = stale;
        assert!(
            serving
                .query::<Value>("file_query", json!([files]), Instant::now() + TIMEOUT)
                .unwrap_err()
                .contains("superseded")
        );
        serving.request.epoch = Some(attached.epoch);
        assert!(attached.close_only);
        for (method, args) in [
            ("file_query", json!([files])),
            ("text_copy", json!([copy, 0])),
        ] {
            assert_eq!(
                serving.query::<Value>(method, args, Instant::now() + TIMEOUT),
                Err(close_only_refusal())
            );
        }
        let presentation: Option<crate::terminal_ipc::ServerResponse> = serving
            .query("presentation", json!([id, null]), Instant::now() + TIMEOUT)
            .unwrap();
        assert!(
            presentation
                .as_ref()
                .and_then(presentation_metadata)
                .is_some()
        );
        serving.stop();
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
        let connections = connections::Connections::new().unwrap();
        let request = Invocation {
            proof,
            epoch: None,
            method: "state".into(),
            args: json!([0]),
        };
        let next = |stage| {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                if let Some(call) = connections.next() {
                    return call;
                }
                assert!(
                    Instant::now() < deadline,
                    "no call was queued after {stage}"
                );
                thread::sleep(Duration::from_millis(1));
            }
        };

        // Half-closing after the request still waits for the reply.
        let (client, served) = UnixStream::pair().unwrap();
        connections.admit(served);
        send(&client, &request, TIMEOUT).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let (_, reply) = next("write shutdown").into_parts();
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
        assert_eq!(next("peer close").method(), "state");
    });
}
