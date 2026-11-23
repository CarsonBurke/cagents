use super::*;
use std::sync::mpsc;

// The peer sends a complete, valid Connected response a byte at a time, each
// within `interval`, rather than causing EOF or an invalid frame. It stops
// once the client has given up. A client whose deadline each byte renewed
// would read the whole frame and connect.
fn trickling_connected_peer(
    mut stream: UnixStream,
    interval: Duration,
    given_up: mpsc::Receiver<()>,
) -> thread::JoinHandle<()> {
    stream.set_write_timeout(Some(IO_TIMEOUT)).unwrap();
    thread::spawn(move || {
        let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        assert_eq!(connect.version, PROTOCOL_VERSION);
        assert_eq!(connect.request_id, 1);
        assert_eq!(connect.proof, None);
        assert!(matches!(
            connect.request,
            ClientRequest::Connect {
                expected_host: None,
                ref resume,
            } if resume.is_empty()
        ));

        let expected = proof("session", "boot", "incarnation");
        let mut frame = Vec::new();
        write_frame(
            &mut frame,
            &connected_envelope(1, expected.clone(), expected),
        )
        .unwrap();
        for byte in frame {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            if !matches!(
                given_up.recv_timeout(interval),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                break;
            }
        }
    })
}

/// Connect through a trickling peer and return how the handshake ended.
fn handshake_through_trickle(
    interval: Duration,
    connect: impl FnOnce(UnixStream) -> Result<ControlClient, ControlError> + Send + 'static,
) -> Result<(), ControlError> {
    let (stream, peer_stream) = UnixStream::pair().unwrap();
    let (give_up, given_up) = mpsc::channel();
    let peer = trickling_connected_peer(peer_stream, interval, given_up);
    let result = crate::test_timing::returns("the handshake", move || connect(stream).map(drop));
    let _ = give_up.send(());
    peer.join().unwrap();
    result
}

#[test]
fn default_handshake_deadline_bounds_a_progressing_partial_connected_frame() {
    // This exercises the real local default path, without changing device
    // selection. The ordinary budget is lowered so that waiting it out is
    // cheap; each byte still arrives well within it.
    assert!(!crate::device::is_remote());
    let budget = ordinary_budget::QUICK;
    match handshake_through_trickle(budget * 3 / 5, move |stream| {
        let _budget = ordinary_budget::quick();
        ControlClient::from_stream(stream)
    }) {
        Err(ControlError::ReplyTimeout { timeout, .. }) => assert!(timeout <= budget),
        Ok(()) => panic!(
            "default handshake must reject a progressing Connected frame after its original budget"
        ),
        Err(error) => panic!("expected the default handshake deadline, got {error}"),
    }
}

#[test]
fn explicit_handshake_deadline_is_not_renewed_by_partial_frame_progress() {
    let budget = Duration::from_millis(100);
    let result = handshake_through_trickle(Duration::from_millis(60), move |stream| {
        ControlClient::from_stream_with_deadline(stream, Instant::now() + budget)
    });
    match result {
        Err(ControlError::ReplyTimeout { timeout, .. }) => assert!(timeout <= budget),
        Ok(()) => {
            panic!("explicit handshake deadline must not renew on partial-frame progress")
        }
        Err(error) => panic!("expected the explicit handshake deadline, got {error}"),
    }
}
