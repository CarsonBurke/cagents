//! Client-side deadline/acknowledgment contracts over actual private sockets.
//! The peer is this test process, not a provider or a production holder. These
//! tests do not establish the helper's separate Preserve/Stop winner policy.
use super::*;

fn accept_until(listener: &UnixListener, deadline: Instant) -> UnixStream {
    loop {
        assert!(Instant::now() < deadline, "private peer accept expired");
        match listener.accept() {
            Ok((control, _)) => return control,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "private peer accept expired");
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("private peer accept failed: {error}"),
        }
    }
}

#[test]
fn withheld_preserve_ack_keeps_the_proxy_owned_until_exact_epoch_retry_ack() {
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let socket = root.path().join("preserve.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut actor = RemoteActor::fixture();
    Arc::get_mut(&mut actor.0).unwrap().socket = socket;
    let original_identity = actor.identity();
    let original_state = serde_json::to_value(actor.state().as_ref()).unwrap();
    let proof = actor.0.proof.clone();
    let epoch = actor.0.epoch;
    let (observed, requests) = crossbeam_channel::bounded(2);
    let (release, withheld) = crossbeam_channel::bounded::<()>(1);
    thread::scope(|scope| {
        let peer = scope.spawn(move || {
            for attempt in 0..2 {
                let deadline = Instant::now() + Duration::from_secs(2);
                let control = accept_until(&listener, deadline);
                let request: Invocation = deadline_io::receive_until(&control, deadline).unwrap();
                assert_eq!(request.proof, proof);
                assert_eq!(request.epoch, Some(epoch));
                assert_eq!(request.method, "preserve");
                assert_eq!(request.args, Value::Null);
                observed.send(attempt).unwrap();
                if attempt == 0 {
                    // Simulate a lost ACK after an exact request was received.
                    // This is ambiguous to the client, even if a helper may
                    // already have applied its idempotent Preserve transition.
                    if withheld.recv_timeout(Duration::from_secs(2)).is_err() {
                        return;
                    }
                } else {
                    deadline_io::send_until(&control, &Ok::<(), String>(()), deadline).unwrap();
                }
            }
        });
        let started = Instant::now();
        assert!(actor.preserve_until(started + Duration::from_millis(100)).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(requests.recv_timeout(Duration::from_secs(1)).unwrap(), 0);
        assert!(!actor.0.preserved.load(Ordering::Acquire));
        assert!(!actor.0.stop.load(Ordering::Acquire));
        assert!(actor.owns_work());
        assert!(!actor.state().completed);
        assert_eq!(actor.identity(), original_identity);
        assert_eq!(serde_json::to_value(actor.state().as_ref()).unwrap(), original_state);
        release.send(()).unwrap();
        actor.preserve_until(Instant::now() + Duration::from_secs(1)).unwrap();
        assert_eq!(requests.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
        assert!(actor.0.preserved.load(Ordering::Acquire));
        assert!(actor.0.stop.load(Ordering::Acquire));
        assert!(!actor.owns_work());
        assert!(!actor.state().completed, "Preserve ACK is never a cleanup proof");
        assert_eq!(actor.identity(), original_identity);
        peer.join().unwrap();
    });
}

#[test]
fn preserve_peer_incarnation_mismatch_refuses_before_sending_a_request() {
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let socket = root.path().join("preserve.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut actor = RemoteActor::fixture();
    let remote = Arc::get_mut(&mut actor.0).unwrap();
    remote.socket = socket;
    remote.identity.record.holder.start = remote.identity.record.holder.start.wrapping_add(1);
    thread::scope(|scope| {
        let peer = scope.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            let mut control = accept_until(&listener, deadline);
            control.set_nonblocking(true).unwrap();
            let mut byte = [0u8; 1];
            loop {
                assert!(Instant::now() < deadline, "rejected connection stayed open");
                match control.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => panic!("an unverified peer received invocation bytes"),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "rejected connection stayed open");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => panic!("inspect rejected private socket: {error}"),
                }
            }
        });
        let error = actor.preserve_until(Instant::now() + Duration::from_secs(1)).unwrap_err();
        assert!(error.contains("peer identity changed"), "{error}");
        assert!(!actor.0.preserved.load(Ordering::Acquire));
        assert!(!actor.0.stop.load(Ordering::Acquire));
        assert!(actor.owns_work());
        assert!(!actor.state().completed);
        peer.join().unwrap();
    });
}
