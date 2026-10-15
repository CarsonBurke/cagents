//! Scripted private peer, NOT a Registry19-backed admission issuer.
//! It exercises the actual ControlClient protocol and fixed c4 original Root
//! transfer. Its positive replies are scripted; callers must label that limit.
use crate::{
    fs_safety::Root,
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
    recovery_domain::{EditedRootToken, LocalRecoveryEndpoint, Operation, PlacementToken, Reply, TransferTarget},
    terminal_ipc::{read_frame, write_frame, ClientEnvelope, ClientRequest, ServerEnvelope, ServerMessage, ServerResponse, PROTOCOL_VERSION},
};
use std::{
    io,
    os::{fd::AsRawFd, unix::{ffi::OsStrExt, fs::{MetadataExt, PermissionsExt}, net::UnixListener}},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event { Prepared, Attached, Activated, Validated, Released }

pub(crate) struct ScriptedEditedRootPeer {
    _runtime: tempfile::TempDir,
    endpoint: LocalRecoveryEndpoint,
    events: Arc<Mutex<Vec<Event>>>,
    server: JoinHandle<()>,
}
impl ScriptedEditedRootPeer {
    /// Test worker only. Reads the supplied ORIGINAL held Root, never a guessed
    /// replacement path. No additional Root alias is kept by the scripted peer.
    pub(crate) fn start(original: &Root) -> Self {
        let metadata = original.metadata().unwrap();
        let expected = (metadata.dev(), metadata.ino(), original.path().as_os_str().as_bytes().to_vec());
        let runtime = tempfile::tempdir().unwrap();
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = runtime.path().join("scripted-edited-root.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let registry = Uuid::new_v4();
        let endpoint = LocalRecoveryEndpoint::fixture(socket, Some(registry)).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "scripted private peer did not connect");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("scripted peer accept: {error}"),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let proof = HostSessionProof { boot_id: BootId::new("scripted-boot"), host_session_id: HostSessionId::new("scripted-session"), host_incarnation: HostIncarnation::new("scripted-incarnation") };
            let respond = |stream: &mut _, request_id, response| {
                write_frame(stream, &ServerEnvelope { version: PROTOCOL_VERSION, request_id: Some(request_id), proof: proof.clone(), message: ServerMessage::Response { response } }).unwrap();
            };
            let hello: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(hello.request, ClientRequest::Connect { .. }));
            respond(&mut stream, hello.request_id, ServerResponse::Connected { proof: proof.clone(), resumed: vec![] });
            let catalog: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert_eq!(catalog.request, ClientRequest::RecoveryPlacement { operation: Operation::Catalog });
            respond(&mut stream, catalog.request_id, ServerResponse::RecoveryPlacement { reply: Reply::Catalog { registry, projects: vec![] } });
            let mut token = None;
            let mut attached = false;
            let mut active = false;
            let mut released = false;
            for _ in 0..128 {
                let Some(envelope): Option<ClientEnvelope> = read_frame(&mut stream).unwrap() else { return; };
                let ClientRequest::RecoveryPlacement { operation } = envelope.request else { panic!("unexpected non-recovery operation"); };
                assert!(!released, "scripted peer received work after settled release");
                let (event, reply) = match operation {
                    Operation::PrepareEditedRoot(input) => {
                        assert!(token.is_none(), "fixture must not silently issue a replacement root");
                        assert_eq!(input.registry, registry);
                        assert_eq!((input.directory.device, input.directory.inode, input.directory.path_hint), expected);
                        assert_ne!(input.directory.mount_id, 0);
                        let issued = EditedRootToken { owner: PlacementToken { registry, issuer: proof.clone(), reservation: Uuid::new_v4(), client: 1, revision: 1 } };
                        token = Some(issued.clone());
                        (Event::Prepared, Reply::EditedRootPrepared(issued))
                    }
                    Operation::Attach(TransferTarget::IndependentEditedRoot { lease }) => {
                        assert_eq!(Some(&lease), token.as_ref());
                        assert!(!attached);
                        let [fd] = crate::control::created_root_rights::negative::receive_edited_root(&stream, Instant::now() + Duration::from_secs(5)).unwrap().0;
                        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                        assert_eq!(unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) }, 0);
                        let stat = unsafe { stat.assume_init() };
                        assert_eq!((stat.st_dev, stat.st_ino), (expected.0, expected.1));
                        attached = true;
                        (Event::Attached, Reply::Attached)
                    }
                    Operation::ActivateEditedRoot(lease) => {
                        assert_eq!(Some(&lease), token.as_ref());
                        assert!(attached && !active);
                        active = true;
                        (Event::Activated, Reply::EditedRootActive)
                    }
                    Operation::ValidateEditedRoot(lease) => {
                        assert_eq!(Some(&lease), token.as_ref());
                        assert!(active);
                        (Event::Validated, Reply::EditedRootValidated)
                    }
                    Operation::ReleaseEditedRoot(lease) => {
                        assert_eq!(Some(&lease), token.as_ref());
                        // Pending preparation may retire without attachment.
                        released = true;
                        (Event::Released, Reply::EditedRootReleased)
                    }
                    _ => panic!("unexpected scripted recovery request: {operation:?}"),
                };
                observed.lock().unwrap().push(event);
                respond(&mut stream, envelope.request_id, ServerResponse::RecoveryPlacement { reply });
            }
            panic!("scripted recovery peer exceeded its bounded request count");
        });
        Self { _runtime: runtime, endpoint, events, server }
    }
    pub(crate) fn endpoint(&self) -> LocalRecoveryEndpoint { self.endpoint.clone() }
    pub(crate) fn events(&self) -> Vec<Event> { self.events.lock().unwrap().clone() }
    /// Drop all client/lease/retirement owners BEFORE joining. The peer uses a
    /// finite read timeout, so a mistaken retained socket fails rather than hangs.
    pub(crate) fn finish(self) -> Vec<Event> {
        self.server.join().unwrap();
        self.events.lock().unwrap().clone()
    }
}
