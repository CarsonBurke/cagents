//! Pure validation shared by daemon client handshakes.
//!
//! Callers select the protocol and request ID and retain their own connection
//! policy, I/O deadlines and error presentation.

use super::{ProtocolError, ServerEnvelope, ServerMessage, ServerResponse};
use crate::model::HostSessionProof;

#[derive(Debug, PartialEq)]
pub(crate) enum HandshakeError<'a> {
    UnsupportedVersion { actual: u16 },
    RequestIdMismatch { expected: u64, actual: Option<u64> },
    IncompleteProof,
    ProofMismatch,
    Remote(&'a ProtocolError),
    UnexpectedMessage(&'a ServerMessage),
}

/// Validate one connect reply, checking its envelope before its message.
/// Protocol qualification and any expected host remain the caller's policy.
pub(crate) fn validate_handshake(
    envelope: &ServerEnvelope,
    protocol: u16,
    request_id: u64,
) -> Result<&HostSessionProof, HandshakeError<'_>> {
    if envelope.version != protocol {
        return Err(HandshakeError::UnsupportedVersion {
            actual: envelope.version,
        });
    }
    if envelope.request_id != Some(request_id) {
        return Err(HandshakeError::RequestIdMismatch {
            expected: request_id,
            actual: envelope.request_id,
        });
    }
    if !envelope.proof.is_complete() {
        return Err(HandshakeError::IncompleteProof);
    }
    match &envelope.message {
        ServerMessage::Response {
            response: ServerResponse::Connected { proof, .. },
        } if !proof.is_complete() => Err(HandshakeError::IncompleteProof),
        ServerMessage::Response {
            response: ServerResponse::Connected { proof, .. },
        } if proof != &envelope.proof => Err(HandshakeError::ProofMismatch),
        ServerMessage::Response {
            response: ServerResponse::Connected { proof, .. },
        } => Ok(proof),
        ServerMessage::Error(error) => Err(HandshakeError::Remote(error)),
        message => Err(HandshakeError::UnexpectedMessage(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{BootId, HostIncarnation, HostSessionId},
        terminal::ipc::{PROTOCOL_VERSION, ProtocolErrorCode},
    };

    fn proof(session: &str, boot: &str, incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new(session),
            boot_id: BootId::new(boot),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }

    fn connected() -> ServerEnvelope {
        let proof = proof("session", "boot", "incarnation");
        ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(7),
            proof: proof.clone(),
            message: ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof,
                    resumed: Vec::new(),
                },
            },
        }
    }

    #[test]
    fn accepts_matching_proofs_only_at_the_selected_protocol() {
        let mut envelope = connected();
        assert_eq!(
            validate_handshake(&envelope, PROTOCOL_VERSION, 7),
            Ok(&envelope.proof)
        );
        let selected = PROTOCOL_VERSION - 1;
        envelope.version = selected;
        assert_eq!(
            validate_handshake(&envelope, selected, 7),
            Ok(&envelope.proof)
        );
        assert_eq!(
            validate_handshake(&envelope, PROTOCOL_VERSION, 7),
            Err(HandshakeError::UnsupportedVersion { actual: selected })
        );
        envelope.version = PROTOCOL_VERSION;
        assert_eq!(
            validate_handshake(&envelope, selected, 7),
            Err(HandshakeError::UnsupportedVersion {
                actual: PROTOCOL_VERSION
            })
        );
    }

    #[test]
    fn rejects_missing_and_incorrect_request_ids() {
        for actual in [None, Some(0), Some(1), Some(8)] {
            let mut envelope = connected();
            envelope.request_id = actual;
            assert_eq!(
                validate_handshake(&envelope, PROTOCOL_VERSION, 7),
                Err(HandshakeError::RequestIdMismatch {
                    expected: 7,
                    actual
                })
            );
        }
    }

    #[test]
    fn rejects_each_incomplete_envelope_or_body_proof_field() {
        for incomplete in [
            proof("", "boot", "incarnation"),
            proof("session", "", "incarnation"),
            proof("session", "boot", ""),
        ] {
            for in_body in [false, true] {
                let mut envelope = connected();
                if in_body {
                    envelope.message = ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: incomplete.clone(),
                            resumed: Vec::new(),
                        },
                    };
                } else {
                    envelope.proof = incomplete.clone();
                }
                assert_eq!(
                    validate_handshake(&envelope, PROTOCOL_VERSION, 7),
                    Err(HandshakeError::IncompleteProof)
                );
            }
        }
    }

    #[test]
    fn rejects_each_body_envelope_proof_mismatch() {
        for mismatched in [
            proof("other", "boot", "incarnation"),
            proof("session", "other", "incarnation"),
            proof("session", "boot", "other"),
        ] {
            let mut envelope = connected();
            envelope.message = ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: mismatched,
                    resumed: Vec::new(),
                },
            };
            assert_eq!(
                validate_handshake(&envelope, PROTOCOL_VERSION, 7),
                Err(HandshakeError::ProofMismatch)
            );
        }
    }

    #[test]
    fn validates_version_then_correlation_then_proof_before_remote_errors() {
        let mut envelope = connected();
        let error = ProtocolError::new(ProtocolErrorCode::InvalidRequest, "refused", true);
        envelope.message = ServerMessage::Error(error.clone());
        envelope.version = PROTOCOL_VERSION + 1;
        envelope.request_id = None;
        envelope.proof = proof("", "boot", "incarnation");
        assert_eq!(
            validate_handshake(&envelope, PROTOCOL_VERSION, 7),
            Err(HandshakeError::UnsupportedVersion {
                actual: PROTOCOL_VERSION + 1
            })
        );
        envelope.version = PROTOCOL_VERSION;
        assert_eq!(
            validate_handshake(&envelope, PROTOCOL_VERSION, 7),
            Err(HandshakeError::RequestIdMismatch {
                expected: 7,
                actual: None
            })
        );
        envelope.request_id = Some(7);
        assert_eq!(
            validate_handshake(&envelope, PROTOCOL_VERSION, 7),
            Err(HandshakeError::IncompleteProof)
        );
        envelope.proof = proof("session", "boot", "incarnation");
        assert_eq!(
            validate_handshake(&envelope, PROTOCOL_VERSION, 7),
            Err(HandshakeError::Remote(&error))
        );
    }

    #[test]
    fn rejects_streaming_and_non_connected_responses() {
        for message in [
            ServerMessage::HostShuttingDown,
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        ] {
            let mut envelope = connected();
            envelope.message = message;
            assert_eq!(
                validate_handshake(&envelope, PROTOCOL_VERSION, 7),
                Err(HandshakeError::UnexpectedMessage(&envelope.message))
            );
        }
    }
}
