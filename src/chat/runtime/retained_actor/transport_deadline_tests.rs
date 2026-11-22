//! Exercise real retained call/adoption paths against a full kernel backlog.
use super::*;
use std::os::unix::net::UnixListener;

#[test]
fn retained_call_and_adoption_bound_the_connect_phase_without_disturbing_owner() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("actor");
    let listener = UnixListener::bind(&socket).unwrap();
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let original = UnixStream::connect(&socket).unwrap();
    let mut actor = RemoteActor::fixture();
    let remote = Arc::get_mut(&mut actor.0).unwrap();
    remote.socket = socket.clone();
    remote.identity.record.boot = crate::daemon::read_boot_id().unwrap();
    remote
        .identity
        .record
        .build
        .as_mut()
        .unwrap()
        .capabilities
        .push(CAPABILITY.into());
    let source = remote.source.clone();
    let record = remote.identity.record.clone();
    let proof = remote.proof.clone();
    for close_only in [false, true] {
        let start = Instant::now();
        let error = RemoteActor::adopt_at_until(
            &record,
            source.clone(),
            proof.clone(),
            close_only,
            &socket,
            start + Duration::from_millis(80),
            |_| {},
        )
        .err()
        .expect("full backlog must refuse adoption");
        assert!(error.contains("deadline expired"), "{error}");
        let method = if close_only { "attach_close" } else { "attach" };
        assert!(error.contains(&format!("`{method}` request")), "{error}");
        // Sooner than the socket timeout an ignored deadline would fall back to.
        assert!(start.elapsed() < TIMEOUT);
    }
    let start = Instant::now();
    let error = actor
        .call_until::<Value>("state", json!([0]), start + Duration::from_millis(80))
        .unwrap_err();
    // The diagnostic names the interrupted request rather than calling every
    // call an inspection.
    assert!(error.contains("deadline expired"), "{error}");
    assert!(error.contains("`state` request"), "{error}");
    assert!(!error.contains("inspection"), "{error}");
    assert!(start.elapsed() < TIMEOUT);
    let (_accepted, _) = listener.accept().unwrap();
    assert!(original.peer_addr().is_ok());
    assert_eq!(actor.identity().record, record);
}

/// A fixture proxy for a helper at `socket` that sends its transcript in the
/// binary codec, as current helpers do, reporting what it notifies.
fn binary_reader(socket: PathBuf) -> (RemoteActor, crossbeam_channel::Receiver<Update>) {
    let mut actor = RemoteActor::fixture_at(socket);
    let (notified, notifications) = crossbeam_channel::unbounded();
    let remote = Arc::get_mut(&mut actor.0).unwrap();
    remote.notify = Arc::new(move |update| {
        let _ = notified.send(update);
    });
    remote.binary_codec.set(true).unwrap();
    (actor, notifications)
}

#[test]
fn a_transcript_read_survives_a_connection_the_live_helper_broke_off() {
    use crate::terminal_ipc::ServerResponse;
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("actor");
    let listener = UnixListener::bind(&socket).unwrap();
    let (actor, _notifications) = binary_reader(socket);
    let chat_id = actor.0.source.id;
    let helper = thread::spawn(move || {
        // The first connection ends before its answer, as when the helper
        // gives up sending to a reader that a loaded computer kept waiting.
        let (first, _) = listener.accept().unwrap();
        deadline_io::receive_until::<Invocation>(&first, Instant::now() + TIMEOUT).unwrap();
        drop(first);
        let (second, _) = listener.accept().unwrap();
        let request: Invocation =
            deadline_io::receive_until(&second, Instant::now() + TIMEOUT).unwrap();
        assert_eq!(request.method, "presentation_binary");
        deadline_io::send_binary_until(
            &second,
            &Ok::<_, String>(Box::new(ServerResponse::ChatSnapshot {
                chat_id,
                snapshot: None,
            })),
            Instant::now() + TIMEOUT,
        )
        .unwrap();
    });
    let response = actor.presentation_wire(chat_id, Some(1)).unwrap();
    helper.join().unwrap();
    assert!(matches!(
        response.decode().unwrap(),
        ServerResponse::ChatSnapshot { snapshot: None, .. }
    ));
    assert_eq!(*actor.0.reread_at.lock().unwrap(), None);
}

#[test]
fn a_transcript_read_the_live_helper_never_answers_asks_the_reader_to_read_again() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("actor");
    // The helper is alive and takes the request, but never answers it.
    let _listener = UnixListener::bind(&socket).unwrap();
    let (actor, notifications) = binary_reader(socket);
    let failure = actor
        .presentation_by(
            actor.0.source.id,
            None,
            Instant::now() + Duration::from_millis(200),
        )
        .unwrap_err();
    assert!(failure.transient);
    assert!(
        failure
            .message
            .starts_with(chat::failure::actor_transport::DEADLINE),
        "{}",
        failure.message
    );
    assert!(
        failure.message.contains("`presentation_binary` request"),
        "{}",
        failure.message
    );
    // The reader is asked to read again once the retry is due, and only
    // once.
    let due = actor
        .0
        .reread_at
        .lock()
        .unwrap()
        .expect("a read again is due");
    assert!(due > Instant::now() + PRESENTATION_RETRY / 2);
    actor.ask_for_due_reread();
    assert!(notifications.try_recv().is_err(), "asked before it was due");
    *actor.0.reread_at.lock().unwrap() = Some(Instant::now());
    assert_eq!(actor.retry_after(false), Some(Duration::ZERO));
    actor.ask_for_due_reread();
    assert!(matches!(notifications.try_recv(), Ok(Update::Presentation)));
    actor.ask_for_due_reread();
    assert!(notifications.try_recv().is_err());
    assert_eq!(*actor.0.reread_at.lock().unwrap(), None);
}

#[test]
fn a_transcript_read_from_a_helper_that_is_gone_is_not_retried() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("actor");
    let _listener = UnixListener::bind(&socket).unwrap();
    let (mut actor, notifications) = binary_reader(socket);
    // Another process now has the holder's process ID.
    Arc::get_mut(&mut actor.0)
        .unwrap()
        .identity
        .record
        .holder
        .start += 1;
    let started = Instant::now();
    let failure = actor
        .presentation_by(actor.0.source.id, None, started + Duration::from_secs(10))
        .unwrap_err();
    assert!(!failure.transient, "{}", failure.message);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(*actor.0.reread_at.lock().unwrap(), None);
    assert!(notifications.try_recv().is_err());
}

#[test]
fn a_changed_files_query_survives_a_connection_the_live_helper_broke_off() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("actor");
    let listener = UnixListener::bind(&socket).unwrap();
    let (actor, _notifications) = binary_reader(socket);
    let query = chat::Command::LoadFileChanges {
        request_id: 1,
        after: None,
    };
    let helper = thread::spawn(move || {
        let (first, _) = listener.accept().unwrap();
        deadline_io::receive_until::<Invocation>(&first, Instant::now() + TIMEOUT).unwrap();
        drop(first);
        let (second, _) = listener.accept().unwrap();
        let request: Invocation =
            deadline_io::receive_until(&second, Instant::now() + TIMEOUT).unwrap();
        assert_eq!(request.method, "file_query");
        deadline_io::send_until(
            &second,
            &Ok::<Value, String>(Value::Null),
            Instant::now() + TIMEOUT,
        )
        .unwrap();
    });
    assert!(actor.file_query(&query).unwrap().is_none());
    helper.join().unwrap();
}

#[test]
fn a_changed_files_query_the_live_helper_never_answers_fails_without_rereading_the_transcript() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("actor");
    let _listener = UnixListener::bind(&socket).unwrap();
    let (actor, notifications) = binary_reader(socket);
    let failure = actor
        .file_query_by(
            &chat::Command::LoadFileChanges {
                request_id: 1,
                after: None,
            },
            Instant::now() + Duration::from_millis(200),
        )
        .unwrap_err();
    assert!(failure.transient);
    assert!(
        failure.message.contains("`file_query` request"),
        "{}",
        failure.message
    );
    // Only the panel that asked shows this failure.
    assert_eq!(*actor.0.reread_at.lock().unwrap(), None);
    assert!(notifications.try_recv().is_err());
}
