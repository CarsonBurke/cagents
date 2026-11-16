//! A CLI carried across an update can still send channel work, which this
//! daemon retired. Its request keeps its position, so it decodes and is refused
//! with an explanation rather than misread as another request.
use super::{ProtocolErrorCode, SharedHost, process_request};
use crate::{
    agent_execution::Lineage,
    host::state::{HostLock, HostState},
    model::{BootId, ChannelId, HostIncarnation, HostSessionId, HostSessionProof, TerminalId},
    terminal_ipc::{ClientEnvelope, ClientRequest, RETIRED_CHANNEL_WORK, read_frame},
};
use std::sync::Arc;

const USER: u64 = 1;
const AGENT: u64 = 2;

fn fixture() -> SharedHost {
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("retired-channel-work-fixture"),
        boot_id: BootId::new("retired-channel-work-boot"),
        host_incarnation: HostIncarnation::new("retired-channel-work-incarnation"),
    });
    state.register_client(USER);
    state.register_client(AGENT);
    // The state Authenticate leaves for an agent connection.
    state
        .agent_clients
        .insert(AGENT, Lineage(TerminalId::new()));
    Arc::new(HostLock::new(state))
}

/// The frame a protocol 128 to 184 CLI sends for `channel work context`:
/// the envelope up to the request's variant, then the retired payload,
/// `authority: Option<String>` and `Request::Context { channel }`, the first
/// variant of the retired request enum.
fn older_channel_work_frame() -> Vec<u8> {
    let mut payload = postcard::to_allocvec(&ClientEnvelope {
        version: 128,
        request_id: 7,
        proof: None,
        request: ClientRequest::RetiredChannelWork,
    })
    .unwrap();
    payload.extend(postcard::to_allocvec(&Some("agent-authority-token".to_string())).unwrap());
    payload.push(0);
    payload.extend(postcard::to_allocvec(&Some(ChannelId::new())).unwrap());
    let mut frame = u32::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
    frame.extend(payload);
    frame
}

#[test]
fn an_older_clis_channel_work_request_decodes_and_is_refused_with_an_explanation() {
    let envelope = read_frame::<ClientEnvelope>(&mut older_channel_work_frame().as_slice())
        .unwrap()
        .unwrap();
    assert_eq!(
        (envelope.version, envelope.request_id, &envelope.request),
        (128, 7, &ClientRequest::RetiredChannelWork)
    );
    let host = fixture();
    for client in [USER, AGENT] {
        let error = process_request(&host, client, envelope.request.clone())
            .expect_err("the retired request is refused");
        assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
        assert_eq!(error.message, RETIRED_CHANNEL_WORK);
        assert!(!error.retryable);
    }
}
