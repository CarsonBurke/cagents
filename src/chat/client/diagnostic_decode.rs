//! Diagnostic replies are inspected while borrowing the framed payload.
//!
//! A wire-size ceiling is not an owned-deserialization capacity ceiling:
//! vectors can grow and unexpected variants can allocate entirely different
//! graphs. Only a bounded expected diagnostic payload is copied into owners.
use super::*;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Deserialize)]
struct Proof<'frame> {
    #[serde(borrow)]
    host_session_id: &'frame str,
    #[serde(borrow)]
    boot_id: &'frame str,
    #[serde(borrow)]
    host_incarnation: &'frame str,
}
impl Proof<'_> {
    fn matches(&self, expected: &HostSessionProof) -> bool {
        self.host_session_id == expected.host_session_id.as_str()
            && self.boot_id == expected.boot_id.as_str()
            && self.host_incarnation == expected.host_incarnation.as_str()
    }
}
#[derive(Deserialize)]
struct Output<'frame> {
    chat_id: TerminalId,
    #[serde(borrow)]
    log_id: &'frame str,
    offset: u64,
    #[serde(borrow)]
    bytes: &'frame [u8],
    next_offset: Option<u64>,
    omitted: u64,
}
#[derive(Deserialize)]
struct Error<'frame> {
    _code: ipc::ProtocolErrorCode,
    #[serde(borrow)]
    message: &'frame str,
    _retryable: bool,
}
#[derive(Clone, Copy)]
struct Tags { response: u32, error: u32, output: u32 }
fn tag<T: Serialize>(value: &T) -> Result<u32, postcard::Error> {
    let mut scratch = [0; 256];
    let payload = postcard::to_slice(value, &mut scratch)?;
    postcard::take_from_bytes::<u32>(payload).map(|(tag, _)| tag)
}
fn tags() -> Result<Tags, postcard::Error> {
    // Derive tags from THIS matched protocol's serializer. Never hardcode
    // enum ordinals or assume another source/version has the same layout.
    static TAGS: OnceLock<Result<Tags, postcard::Error>> = OnceLock::new();
    TAGS.get_or_init(|| Ok(Tags {
        response: tag(&ServerMessage::Response { response: ServerResponse::Accepted })?,
        error: tag(&ServerMessage::Error(ipc::ProtocolError::new(
            ipc::ProtocolErrorCode::TerminalUnavailable, String::new(), false)))?,
        output: tag(&ServerResponse::ChatProviderExitOutput {
            chat_id: TerminalId::new(), log_id: String::new(), offset: 0,
            bytes: Vec::new(), next_offset: None, omitted: 0,
        })?,
    })).clone()
}

/// `None` means an authenticated diagnostic reply was consumed/routed. Ordinary
/// publications retain their existing owned decoder and its separate owners.
pub(super) fn decode(
    frame: &[u8], expected: &HostSessionProof, chat: TerminalId,
    pending: &provider_exit::Pending,
) -> Result<Option<ServerEnvelope>, postcard::Error> {
    let (version, rest) = postcard::take_from_bytes::<u16>(frame)?;
    let (request, rest) = postcard::take_from_bytes::<Option<u64>>(rest)?;
    let Some(request) = request.filter(|request| provider_exit::owns_request(*request)) else {
        return postcard::from_bytes(frame).map(Some);
    };
    let (proof, message) = postcard::take_from_bytes::<Proof<'_>>(rest)?;
    if version != PROTOCOL_VERSION || !proof.matches(expected) {
        return Err(postcard::Error::DeserializeBadEncoding);
    }
    let Some(mut owner) = pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&request) else {
        // The full frame has arrived; no payload graph is decoded or allocated
        // for a locally retired request, including a late Error or Snapshot.
        return Ok(None);
    };
    if owner.reply.is_none() {
        // A posted cancelled query now actually settled on the wire. There is
        // no owned payload; dropping its admission here cannot precede one.
        return Ok(None);
    }
    let result = reply(message, chat, &mut owner);
    if let Some(sender) = owner.reply.take() {
        let _ = sender.try_send(result);
    } else {
        drop(result);
    }
    // For errors, the bounded result was delivered to an active request's
    // metadata owner, or destroyed above BEFORE returning this reservation.
    drop(owner);
    Ok(None)
}
fn reply(
    message: &[u8], chat: TerminalId, owner: &mut provider_exit::PendingReply,
) -> Result<provider_exit::Page, String> {
    let tags = tags().map_err(|_| "Could not inspect the saved output reply".to_owned())?;
    let (kind, rest) = postcard::take_from_bytes::<u32>(message)
        .map_err(|_| "The saved output reply is invalid".to_owned())?;
    if kind == tags.error {
        let error: Error<'_> = postcard::from_bytes(rest)
            .map_err(|_| "The saved output error reply is invalid".to_owned())?;
        let mut end = error.message.len().min(512);
        while !error.message.is_char_boundary(end) { end -= 1; }
        return Err(error.message[..end].to_owned());
    }
    if kind != tags.response {
        return Err("Unexpected provider output reply".into());
    }
    let (response, rest) = postcard::take_from_bytes::<u32>(rest)
        .map_err(|_| "The saved output reply is invalid".to_owned())?;
    if response != tags.output {
        // Refuse without deserializing an unexpected Snapshot/tool/tree graph.
        return Err("Unexpected provider output reply".into());
    }
    let output: Output<'_> = postcard::from_bytes(rest)
        .map_err(|_| "The saved output reply is invalid".to_owned())?;
    if output.chat_id != chat || output.log_id.is_empty() || output.log_id.len() > 256
        || output.bytes.len() > provider_exit::MAX_PAGE_BYTES {
        return Err("The daemon returned an oversized or mismatched saved output reply".into());
    }
    // Only these bounded fields allocate. The exact request's response charge
    // already exists; transfer it with the decoded body until actual teardown.
    let page = provider_exit::Page {
        log_id: output.log_id.to_owned(), offset: output.offset,
        bytes: output.bytes.to_vec(), next_offset: output.next_offset,
        omitted: output.omitted, _ownership: owner.ownership.take(),
    };
    #[cfg(test)]
    OWNED_PAGE_CAPACITY.fetch_max(page.log_id.capacity() + page.bytes.capacity(), Ordering::AcqRel);
    Ok(page)
}
#[cfg(test)]
static OWNED_PAGE_CAPACITY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(chat: TerminalId, log_id: String, bytes: Vec<u8>, request: u64) -> (Vec<u8>, HostSessionProof) {
        let proof=super::super::tests::host_proof("borrowed-diagnostic-peer");
        let envelope=ServerEnvelope {version:PROTOCOL_VERSION,request_id:Some(request),proof:proof.clone(),message:ServerMessage::Response{
            response:ServerResponse::ChatProviderExitOutput {chat_id:chat,log_id,offset:0,bytes,next_offset:None,omitted:0}}};
        (postcard::to_allocvec(&envelope).unwrap(),proof)
    }
    fn pending(request:u64)->(provider_exit::Pending,crossbeam_channel::Receiver<Result<provider_exit::Page,String>>) {
        let (reply,response)=crossbeam_channel::bounded(1);
        let pending=Arc::new(Mutex::new(BTreeMap::new()));
        pending.lock().unwrap().insert(request,provider_exit::PendingReply {reply:Some(reply),ownership:None});
        (pending,response)
    }
    #[test]
    fn expected_diagnostic_borrowed_layout_matches_current_wire_and_allocates_only_bounded_fields() {
        let _guard=diagnostic_read::diagnostic_test_guard();
        OWNED_PAGE_CAPACITY.store(0,Ordering::Release);
        let request=provider_exit::REQUEST_NAMESPACE|17;let chat=TerminalId::new();
        // Exercise every u8 value: borrowed bytes must be wire-equivalent to
        // the actual protocol's plain Vec<u8> sequence, including varints.
        let bytes=(0..=255).cycle().take(256*1024).collect::<Vec<u8>>();
        let (frame,proof)=packet(chat,"exact-log".into(),bytes.clone(),request);let(pending,response)=pending(request);
        assert!(decode(&frame,&proof,chat,&pending).unwrap().is_none());
        let page=response.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
        assert_eq!(page.log_id,"exact-log");assert_eq!(page.bytes,bytes);
        assert!(page.log_id.capacity()+page.bytes.capacity()<=provider_exit::MAX_PAGE_BYTES+256);
        assert!(pending.lock().unwrap().is_empty());
    }
    #[test]
    fn malformed_large_log_and_growing_byte_vector_are_rejected_before_owned_page_allocation() {
        let _guard=diagnostic_read::diagnostic_test_guard();
        let request=provider_exit::REQUEST_NAMESPACE|18;let chat=TerminalId::new();
        let(frame,proof)=packet(chat,"x".repeat(6_900_000),vec![1;1_100_000],request);
        assert!(frame.len()<ipc::MAX_FRAME_BYTES);
        let old:ServerEnvelope=postcard::from_bytes(&frame).unwrap();
        let ServerMessage::Response{response:ServerResponse::ChatProviderExitOutput{log_id,bytes,..}}=old.message else {panic!("expected wire page")};
        let old_capacity=log_id.capacity()+bytes.capacity();
        println!("diagnostic-owned-decode-counterexample frame_bytes={} old_log_capacity={} old_vec_capacity={} old_aggregate={} (actual layouts, notRSS)",frame.len(),log_id.capacity(),bytes.capacity(),old_capacity);
        assert!(old_capacity>ipc::MAX_FRAME_BYTES+64*1024,"pinned serde cautious preallocation must reproduce the aggregate capacity hole");
        drop(log_id);drop(bytes);
        let(pending,response)=pending(request);OWNED_PAGE_CAPACITY.store(0,Ordering::Release);
        assert!(decode(&frame,&proof,chat,&pending).unwrap().is_none());
        let error=response.recv_timeout(Duration::from_secs(1)).unwrap().err().unwrap();
        assert!(error.capacity()<=512);assert_eq!(OWNED_PAGE_CAPACITY.load(Ordering::Acquire),0,"no owned page fields allocated before refusal");
    }
    #[test]
    fn unexpected_allocation_heavy_variant_is_not_owned_decoded_for_diagnostic_namespace() {
        let _guard=diagnostic_read::diagnostic_test_guard();
        let request=provider_exit::REQUEST_NAMESPACE|19;let chat=TerminalId::new();
        let proof=super::super::tests::host_proof("borrowed-diagnostic-peer");
        let envelope=ServerEnvelope{version:PROTOCOL_VERSION,request_id:Some(request),proof:proof.clone(),message:ServerMessage::Response{response:ServerResponse::ChatSnapshot{
            chat_id:chat,snapshot:Some(Snapshot{provider_exit:Some(super::super::super::ProviderExitDiagnostic{summary:"failure".into(),output:"x".repeat(7*1024*1024),log:None}),..Default::default()})}}};
        let frame=postcard::to_allocvec(&envelope).unwrap();assert!(frame.len()<ipc::MAX_FRAME_BYTES);drop(envelope);
        let(pending,response)=pending(request);OWNED_PAGE_CAPACITY.store(0,Ordering::Release);
        assert!(decode(&frame,&proof,chat,&pending).unwrap().is_none());
        assert_eq!(response.recv_timeout(Duration::from_secs(1)).unwrap().err().unwrap(),"Unexpected provider output reply");
        assert_eq!(OWNED_PAGE_CAPACITY.load(Ordering::Acquire),0);
    }
    #[test]
    fn diagnostic_error_is_compacted_from_a_borrow_and_retired_reply_never_builds_owned_fields() {
        let _guard=diagnostic_read::diagnostic_test_guard();
        let request=provider_exit::REQUEST_NAMESPACE|20;let chat=TerminalId::new();
        let proof=super::super::tests::host_proof("borrowed-diagnostic-peer");
        let frame=postcard::to_allocvec(&ServerEnvelope{version:PROTOCOL_VERSION,request_id:Some(request),proof:proof.clone(),message:ServerMessage::Error(ipc::ProtocolError::new(
            ipc::ProtocolErrorCode::TerminalUnavailable,"€".repeat(1024*1024),false))}).unwrap();
        let(pending,response)=pending(request);assert!(decode(&frame,&proof,chat,&pending).unwrap().is_none());
        let error=response.recv_timeout(Duration::from_secs(1)).unwrap().err().unwrap();assert!(error.len()<=512 && error.capacity()<=512);
        let(frame,proof)=packet(chat,"x".repeat(6_900_000),vec![1;1_100_000],request);
        OWNED_PAGE_CAPACITY.store(0,Ordering::Release);assert!(decode(&frame,&proof,chat,&pending).unwrap().is_none());
        assert_eq!(OWNED_PAGE_CAPACITY.load(Ordering::Acquire),0);
    }
    #[test]
    fn borrowed_diagnostic_prefix_authenticates_before_skipping_and_preserves_ordinary_frames() {
        let _guard=diagnostic_read::diagnostic_test_guard();
        let request=provider_exit::REQUEST_NAMESPACE|21;let chat=TerminalId::new();
        let(frame,proof)=packet(chat,"log".into(),vec![1,2,3],request);let(pending,_)=pending(request);
        let wrong=super::super::tests::host_proof("wrong-diagnostic-peer");assert!(decode(&frame,&wrong,chat,&pending).is_err());assert_eq!(pending.lock().unwrap().len(),1);
        let ordinary=ServerEnvelope{version:PROTOCOL_VERSION,request_id:None,proof:proof.clone(),message:ServerMessage::ChatActivity{chat_id:chat,state:ActivityState::Working,generation:7}};
        let raw=postcard::to_allocvec(&ordinary).unwrap();assert_eq!(decode(&raw,&proof,chat,&pending).unwrap(),Some(ordinary));
    }
}
