//! Real Store/actor reads through the authenticated retained invocation. The
//! only wire-only peer below deliberately violates framing to test the bound.
use super::*;
use crate::chat::runtime::{PeerCursor, PeerPage, PeerReader};

fn isolated() -> bool {
    const CHILD: &str = "CAGENTS_PEER_PAGE_BRIDGE_TEST";
    if std::env::var_os(CHILD).is_some() {
        return false;
    }
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let name = thread::current().name().unwrap().to_owned();
    let output = ChildCommand::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env(CHILD, "1")
        .env("XDG_RUNTIME_DIR", root.path())
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("HOME", root.path().join("home"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

fn remote_for(server: &Server) -> RemoteActor {
    let mut remote = RemoteActor::fixture();
    let inner = Arc::get_mut(&mut remote.0).unwrap();
    inner.identity = server.identity.clone();
    inner.source = server.source.clone();
    inner.proof = server.proof.clone();
    inner.epoch = server.epoch;
    inner.socket = server.socket.clone();
    remote
}

struct Serving {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Serving {
    fn new(mut fixture: Fixture) -> Self {
        fixture.server.listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !stopping.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "finite bridge fixture expired");
                match fixture.server.listener.accept() {
                    Ok((control, _)) => {
                        // The production parser, current daemon-lease gate and
                        // exact server proof/epoch dispatcher produce every
                        // success/refusal. No page or authentication is stubbed.
                        let response =
                            receive::<Invocation>(&control, TIMEOUT).and_then(|request| {
                                authorize(&control, &request.proof)?;
                                fixture.server.invoke(request)
                            });
                        crate::descriptor_frame::send_json_with_fds_until(
                            &control,
                            &response,
                            &[],
                            Instant::now() + TIMEOUT,
                            PEER_RESPONSE_BYTES,
                            0,
                            crate::descriptor_frame::FrameByteOrder::LittleEndian,
                        )
                        .unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept retained read: {error}"),
                }
            }
            // Fixture::drop performs the original actor/provider cleanup.
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
    fn finish(mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}
impl Drop for Serving {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn owned_fixture(body: Option<String>) -> (Fixture, crate::daemon::HostLease) {
    let mut fixture = resident_fixture_with_body(body);
    fixture.server.proof.boot_id = crate::daemon::read_boot_id().unwrap();
    fixture.server.source.proof = Some(fixture.server.proof.clone());
    fixture.server.identity.record.boot = fixture.server.proof.boot_id.clone();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let lease = crate::daemon::HostLease::acquire(&paths, &fixture.server.proof).unwrap();
    (fixture, lease)
}

fn same_page(local: &PeerPage, remote: &PeerPage) {
    assert_eq!(
        serde_json::to_value(local).unwrap(),
        serde_json::to_value(remote).unwrap()
    );
    assert!(serde_json::to_vec(remote).unwrap().len() < PEER_RESPONSE_BYTES);
}

#[test]
fn local_and_authenticated_remote_peer_pages_match_without_ui_or_provider_effects() {
    if isolated() {
        return;
    }
    let body = "answer λ🦀\n".repeat(2000);
    let (fixture, _lease) = owned_fixture(Some(body.clone()));
    let local = fixture.server.actor.clone();
    local
        .0
        .store
        .lock()
        .unwrap()
        .apply(chat::Event::Item(chat::Item {
            id: "peer-tool".into(),
            role: chat::Role::Assistant,
            content: chat::ItemContent::Tool {
                name: "Read fixture".into(),
                input: "PRIVATE-TOOL-INPUT".into(),
                output: "tool output λ".into(),
                status: chat::ToolStatus::Succeeded,
            },
            started_at: Some(10),
            at: Some(11),
        }))
        .unwrap();
    let before = local.snapshot(None).unwrap();
    let visible = local.0.visible.load(Ordering::Acquire);
    let observe = local.0.observe_next.load(Ordering::Acquire);
    let remote = remote_for(&fixture.server);
    let (local_handle, remote_handle) =
        crate::chat::runtime::Handle::fixture_peer_page_pair(local.clone(), remote);
    let serving = Serving::new(fixture);
    let mut local_reader = PeerReader::default();
    let mut remote_reader = PeerReader::default();
    let mut after: Option<PeerCursor> = None;
    let mut text = String::new();
    let mut ended = false;
    for _ in 0..8 {
        let expected = local_handle
            .peer_page(&mut local_reader, after.clone())
            .unwrap();
        let actual = remote_handle.peer_page(&mut remote_reader, after).unwrap();
        same_page(&expected, &actual);
        assert_eq!(
            serde_json::to_value(&local_reader).unwrap(),
            serde_json::to_value(&remote_reader).unwrap()
        );
        text.extend(actual.parts.iter().map(|part| part.text.as_str()));
        assert!(
            !serde_json::to_string(&actual)
                .unwrap()
                .contains("PRIVATE-TOOL-INPUT")
        );
        after = actual.next;
        if actual.at_end {
            ended = true;
            break;
        }
    }
    assert!(ended, "bounded fixture must reach cached end");
    assert_eq!(text, format!("{body}tool output λ"));
    let after = local.snapshot(None).unwrap();
    assert_eq!(after, before, "cache reads invent no UI publication");
    assert_eq!(local.0.visible.load(Ordering::Acquire), visible);
    assert_eq!(local.0.observe_next.load(Ordering::Acquire), observe);
    assert!(local.0.commands.is_empty(), "no history/input/ACK commands");
    serving.finish();
}

#[test]
fn remote_empty_reader_pin_survives_end_and_rejects_reset_stale_epoch_and_close_only() {
    if isolated() {
        return;
    }
    let (fixture, _lease) = owned_fixture(None);
    let local = fixture.server.actor.clone();
    let mut wrong_epoch = remote_for(&fixture.server);
    Arc::get_mut(&mut wrong_epoch.0).unwrap().epoch = uuid::Uuid::new_v4();
    let mut wrong_proof = remote_for(&fixture.server);
    Arc::get_mut(&mut wrong_proof.0)
        .unwrap()
        .proof
        .host_incarnation = crate::model::HostIncarnation::new("not-the-owning-daemon");
    let mut close_only = remote_for(&fixture.server);
    Arc::get_mut(&mut close_only.0).unwrap().close_only = true;
    let remote = remote_for(&fixture.server);
    let serving = Serving::new(fixture);
    let mut reader = PeerReader::default();
    let empty = serde_json::to_value(&reader).unwrap();
    for refused in [&wrong_epoch, &wrong_proof, &close_only] {
        assert!(refused.peer_page(&mut reader, None).is_err());
        assert_eq!(serde_json::to_value(&reader).unwrap(), empty);
    }
    let page = remote.peer_page(&mut reader, None).unwrap();
    assert!(page.parts.is_empty() && page.at_end);
    let pinned = serde_json::to_value(&reader).unwrap();
    assert_ne!(pinned, empty, "even empty reads pin original incarnation");
    local
        .0
        .store
        .lock()
        .unwrap()
        .apply(chat::Event::Reset {
            reason: "actual cache reset".into(),
        })
        .unwrap();
    // The public host clears an at-end cursor. Reusing the same Reader with
    // None still refuses, rather than adopting the replacement incarnation.
    assert!(remote.peer_page(&mut reader, None).is_err());
    assert_eq!(serde_json::to_value(&reader).unwrap(), pinned);
    let fresh = remote.peer_page(&mut PeerReader::default(), None).unwrap();
    assert_ne!(fresh.incarnation, page.incarnation);
    assert!(fresh.parts.is_empty());
    assert!(local.0.commands.is_empty());
    serving.finish();
}

#[test]
fn remote_peer_page_refuses_oversized_frame_before_body_and_keeps_reader_pin() {
    use std::io::Write;
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let socket = root.path().join("bounded-peer.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut remote = RemoteActor::fixture();
    Arc::get_mut(&mut remote.0).unwrap().socket = socket;
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + TIMEOUT;
        let (mut control, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("accept oversized-frame fixture: {error}"),
            }
        };
        let request: Invocation = receive(&control, TIMEOUT).unwrap();
        assert_eq!(request.method, "peer_page");
        // No oversized allocation or body is necessary to test the receiver.
        control
            .write_all(
                &u32::try_from(PEER_RESPONSE_BYTES + 1)
                    .unwrap()
                    .to_le_bytes(),
            )
            .unwrap();
    });
    let mut reader = PeerReader::default();
    let before = serde_json::to_value(&reader).unwrap();
    let result = remote.peer_page(&mut reader, None);
    worker.join().unwrap();
    assert!(result.unwrap_err().contains("oversized JSON frame"));
    assert_eq!(serde_json::to_value(&reader).unwrap(), before);
}
