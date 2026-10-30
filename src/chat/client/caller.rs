use super::*;
use crate::control::CallerContext;

/// Bind the captured caller on this proven stream before any native operation.
/// The ordinary User handshake keeps its original request sequence.
pub(super) fn authenticate(
    stream: &mut UnixStream,
    codec: &mut FrameCodec,
    proof: &HostSessionProof,
    next_request: &mut u64,
    caller: &CallerContext,
) -> Result<(), String> {
    let Some(authority) = caller.authentication() else {
        return Ok(());
    };
    let request_id = *next_request;
    *next_request = request_id
        .checked_add(1)
        .ok_or("Native conversation request sequence exhausted")?;
    codec
        .write(
            stream,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id,
                proof: Some(proof.clone()),
                request: ClientRequest::Authenticate { authority },
            },
        )
        .map_err(|error| error.to_string())?;
    match receive(stream, codec, request_id, Some(proof))? {
        ServerResponse::Accepted => Ok(()),
        _ => Err("The daemon did not accept the native conversation caller scope".into()),
    }
}
