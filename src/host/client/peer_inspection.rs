//! Peer-read staging deliberately releases control before cache/RPC work.
//! Publication observes canonical permission after the cache read, then the
//! host serializes its resident attachment check and private page enqueue.
use super::*;

fn admit_request(
    host: &SharedHost,
    client_id: ClientId,
    request: &ClientRequest,
) -> Result<(), ProtocolError> {
    ensure_request_open(host, request)?;
    ensure_startup_recovery_request(host, request)?;
    super::super::agent_execution::terminals::admit(host, client_id, request)?;
    super::super::account_lifecycle::admit(host, request)?;
    super::super::worktree_removal::admit(host, request)
}

/// False means the bounded queue closed/refused delivery; its existing
/// QueueFull/EOF policy owns that outcome, so do not append a second response.
pub(super) fn process(
    host: &SharedHost,
    client_id: ClientId,
    request_id: u64,
    delivery: &Arc<DeliveryQueue>,
    request: ClientRequest,
) -> Result<bool, ProtocolError> {
    let invalid = |message| ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false);
    let gate = control_gate(host);
    {
        let _control = gate.read().unwrap_or_else(|p| p.into_inner());
        admit_request(host, client_id, &request)?;
    }
    let ClientRequest::Agent {
        authority,
        operation: crate::agent_execution::Operation::InspectChat { session, cursor },
    } = &request
    else {
        return Err(invalid("This route requires a peer chat inspection".into()));
    };
    // No host, registry or global admission guard survives this potentially
    // blocking Local/Remote cache read. The codec checks explicit SQL release.
    let prepared = super::super::agent_execution::inspection::prepare(
        host,
        authority,
        *session,
        cursor.clone(),
    )
    .map_err(invalid)?;

    let _control = gate.read().unwrap_or_else(|p| p.into_inner());
    admit_request(host, client_id, &request)?;
    let mut delivery_failed = false;
    let result = prepared.admit(host, |page, proof| {
        delivery
            .push(ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(request_id),
                proof: proof.clone(),
                message: ServerMessage::Response {
                    response: ServerResponse::Agent(crate::agent_execution::Reply::ChatPage(page)),
                },
            })
            .map_err(|_| {
                delivery_failed = true;
                "The peer inspection delivery queue is closed".to_owned()
            })
    });
    if delivery_failed {
        return Ok(false);
    }
    result.map(|()| true).map_err(invalid)
}
