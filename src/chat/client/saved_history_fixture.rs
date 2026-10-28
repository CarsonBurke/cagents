//! In-process framed daemon fixture, never a Host custody/provider proof.
use super::*;

pub(crate) struct Peer {
    wire: Arc<Mutex<(UnixStream, FrameCodec)>>,
    snapshot: Arc<Mutex<Snapshot>>,
    pub(crate) requests: Arc<Mutex<Vec<&'static str>>>,
    thread: Option<thread::JoinHandle<()>>,
    proof: HostSessionProof,
    id: crate::model::TerminalId,
}

impl Peer {
    pub(crate) fn new(
        source: Session,
        resolved: Session,
        attached: Session,
        snapshot: Snapshot,
        delayed_loading: bool,
    ) -> (UnixStream, Self) {
        let (client, mut server) = UnixStream::pair().unwrap();
        let writer = server.try_clone().unwrap();
        let proof = resolved.proof.clone().unwrap();
        let wire = Arc::new(Mutex::new((writer, FrameCodec::new(ipc::MAX_FRAME_BYTES))));
        let snapshots = Arc::new(Mutex::new(snapshot));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let peer_wire = wire.clone();
        let peer_snapshot = snapshots.clone();
        let peer_requests = requests.clone();
        let peer_proof = proof.clone();
        let id = source.id;
        let worker = thread::spawn(move || {
            let respond = |request_id, message| {
                let mut wire = peer_wire.lock().unwrap();
                let (stream, codec) = &mut *wire;
                codec
                    .write(
                        stream,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id,
                            proof: peer_proof.clone(),
                            message,
                        },
                    )
                    .is_ok()
            };
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            while let Ok(Some(envelope)) = codec.read::<ClientEnvelope>(&mut server) {
                let response = match envelope.request {
                    ClientRequest::Connect { .. } => {
                        peer_requests.lock().unwrap().push("Connect");
                        ServerResponse::Connected {
                            proof: peer_proof.clone(),
                            resumed: Vec::new(),
                        }
                    }
                    ClientRequest::ResolveUpgradeChat { source: saved } => {
                        assert_eq!(saved, source);
                        peer_requests.lock().unwrap().push("Resolve");
                        ServerResponse::UpgradeChatResolved {
                            session: resolved.clone(),
                        }
                    }
                    ClientRequest::ChatAttach { session } => {
                        assert_eq!(session, resolved);
                        peer_requests.lock().unwrap().push("Attach");
                        let mut initial = peer_snapshot.lock().unwrap().clone();
                        initial.history_loading = delayed_loading;
                        if !respond(
                            Some(envelope.request_id),
                            ServerMessage::Response {
                                response: ServerResponse::ChatAttached {
                                    session: attached.clone(),
                                    snapshot: initial,
                                },
                            },
                        ) {
                            break;
                        }
                        if delayed_loading {
                            thread::sleep(Duration::from_millis(20));
                            peer_snapshot.lock().unwrap().generation += 1;
                            if !respond(None, ServerMessage::ChatChanged { chat_id: id }) {
                                break;
                            }
                        }
                        continue;
                    }
                    ClientRequest::ChatRead { .. } => {
                        peer_requests.lock().unwrap().push("Read");
                        ServerResponse::ChatSnapshot {
                            chat_id: id,
                            snapshot: Some(peer_snapshot.lock().unwrap().clone()),
                        }
                    }
                    ClientRequest::ChatVisibility { .. } => ServerResponse::Accepted,
                    other => {
                        panic!("read-only admission issued a provider/lifecycle request: {other:?}")
                    }
                };
                if !respond(
                    Some(envelope.request_id),
                    ServerMessage::Response { response },
                ) {
                    break;
                }
            }
        });
        (
            client,
            Self {
                wire,
                snapshot: snapshots,
                requests,
                thread: Some(worker),
                proof,
                id,
            },
        )
    }

    pub(crate) fn publish(&self, snapshot: Snapshot) {
        *self.snapshot.lock().unwrap() = snapshot;
        let mut wire = self.wire.lock().unwrap();
        let (stream, codec) = &mut *wire;
        codec
            .write(
                stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: None,
                    proof: self.proof.clone(),
                    message: ServerMessage::ChatChanged { chat_id: self.id },
                },
            )
            .unwrap();
    }

    pub(crate) fn message(&self, message: ServerMessage) {
        let mut wire = self.wire.lock().unwrap();
        let (stream, codec) = &mut *wire;
        codec
            .write(
                stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: None,
                    proof: self.proof.clone(),
                    message,
                },
            )
            .unwrap();
    }

    pub(crate) fn disconnect(&self) {
        let _ = self.wire.lock().unwrap().0.shutdown(Shutdown::Both);
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.disconnect();
        if let Some(worker) = self.thread.take() {
            worker.join().unwrap();
        }
    }
}
