use super::*;

// The peer sends a complete, valid response, rather than causing EOF or an
// invalid frame. Each fragment makes progress within the ordinary idle timeout.
fn progressing_connected_peer(
    mut stream: UnixStream,
    interval: Duration,
) -> thread::JoinHandle<[Instant; 3]> {
    stream.set_read_timeout(Some(IO_TIMEOUT * 2)).unwrap();
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
        let origin = Instant::now();
        stream.write_all(&frame[..2]).unwrap();
        let first = Instant::now();
        thread::sleep((origin + interval).saturating_duration_since(Instant::now()));
        stream.write_all(&frame[2..5]).unwrap();
        let second = Instant::now();
        thread::sleep((origin + interval * 2).saturating_duration_since(Instant::now()));
        stream.write_all(&frame[5..]).unwrap();
        [first, second, Instant::now()]
    })
}

#[test]
fn default_handshake_deadline_bounds_a_progressing_partial_connected_frame() {
    // This parent exercises the real local default, without changing device
    // selection or the production five-second budget.
    assert!(!crate::device::is_remote());
    let (stream, peer_stream) = UnixStream::pair().unwrap();
    // A timeout drops the constructor's FD. Keep the endpoint alive so the
    // peer still completes its valid frame; an EOF/write error cannot pass.
    let retained_endpoint = stream.try_clone().unwrap();
    let peer = progressing_connected_peer(peer_stream, Duration::from_secs(3));
    let started = Instant::now();
    let result = ControlClient::from_stream(stream);
    let constructor_elapsed = started.elapsed();
    let peer_result = peer.join();
    drop(retained_endpoint);

    // Join and release the retained endpoint before evaluating either outcome,
    // including the old implementation's unexpected successful constructor.
    let writes = peer_result.expect("the valid Connected peer must finish");
    assert!(writes[1].duration_since(writes[0]) < IO_TIMEOUT);
    assert!(writes[2].duration_since(writes[1]) < IO_TIMEOUT);
    assert!(writes[2].duration_since(writes[0]) > IO_TIMEOUT);
    match result {
        Err(ControlError::ReplyTimeout { timeout, .. }) => {
            assert!(timeout <= IO_TIMEOUT);
        }
        Ok(client) => {
            drop(client);
            panic!(
                "default handshake must reject a progressing Connected frame after its original five-second budget; constructor took {constructor_elapsed:?}"
            );
        }
        Err(error) => panic!(
            "expected the default handshake deadline, got {error} after {constructor_elapsed:?}"
        ),
    }
}

#[test]
fn explicit_handshake_deadline_is_not_renewed_by_partial_frame_progress() {
    let (stream, peer_stream) = UnixStream::pair().unwrap();
    let retained_endpoint = stream.try_clone().unwrap();
    let peer = progressing_connected_peer(peer_stream, Duration::from_millis(60));
    let budget = Duration::from_millis(100);
    let started = Instant::now();
    let deadline = started + budget;
    let result = ControlClient::from_stream_with_deadline(stream, deadline);
    let constructor_elapsed = started.elapsed();
    let peer_result = peer.join();
    drop(retained_endpoint);

    let writes = peer_result.expect("the valid Connected peer must finish");
    // Distinguish an original absolute deadline from a renewed per-read budget.
    // Scheduling that misses these windows fails the fixture, not the oracle.
    assert!(writes[0] < deadline);
    assert!(writes[1] < deadline);
    assert!(writes[2] > deadline);
    assert!(writes[1].duration_since(writes[0]) < budget);
    assert!(writes[2].duration_since(writes[1]) < budget);
    match result {
        Err(ControlError::ReplyTimeout { timeout, .. }) => {
            assert!(timeout <= budget);
        }
        Ok(client) => {
            drop(client);
            panic!(
                "explicit handshake deadline must not renew on partial-frame progress; constructor took {constructor_elapsed:?}"
            );
        }
        Err(error) => panic!(
            "expected the explicit handshake deadline, got {error} after {constructor_elapsed:?}"
        ),
    }
}
