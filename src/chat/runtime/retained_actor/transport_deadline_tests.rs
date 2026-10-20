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
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    let start = Instant::now();
    let error = actor
        .call_until::<Value>("state", json!([0]), start + Duration::from_millis(80))
        .unwrap_err();
    assert!(error.contains("deadline expired"), "{error}");
    assert!(start.elapsed() < Duration::from_secs(2));
    let (_accepted, _) = listener.accept().unwrap();
    assert!(original.peer_addr().is_ok());
    assert_eq!(actor.identity().record, record);
}
