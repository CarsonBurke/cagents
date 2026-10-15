//! Data and one-use host audience fence for the original UI journal phase.
//!
//! A challenge/response is wire data, never a creation capability. Only the
//! consumed host-issued transaction can authenticate its bytes; repository
//! validation must still check the complete durable row and original six roles.
//! The owning host reader must close the connection on every phase failure.
use crate::model::{DirectoryStartRequirement, HostSessionProof, OperationId};
use serde::{Deserialize, Serialize};
use std::{io, time::Instant};

pub(crate) const MAX_CREATION_ROW_BYTES: usize = 1024 * 1024;
pub(crate) type RoleIdentities = [[u64; 2]; 6];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UiJournalChallenge {
    pub request_id: u64,
    pub nonce: OperationId,
    pub proof: HostSessionProof,
    pub requirement: DirectoryStartRequirement,
    pub window: String,
    pub database: [u64; 2],
    pub roles: RoleIdentities,
}
impl UiJournalChallenge {
    pub(crate) fn validate_shape(&self) -> io::Result<()> {
        crate::instance::validate_window_slot(&self.window)?;
        self.requirement.validate().map_err(refused)?;
        if !self.proof.is_complete()
            || [
                self.proof.host_session_id.as_str(),
                self.proof.boot_id.as_str(),
                self.proof.host_incarnation.as_str(),
            ]
            .iter()
            .any(|value| value.len() > 256)
        {
            return Err(refused(
                "The journal challenge has no bounded complete host proof",
            ));
        }
        Ok(())
    }
}

/// Untrusted private-phase data. There is intentionally no conversion from
/// this object to repository/start authority without an issued transaction.
pub(crate) struct UiJournalResponse {
    challenge: UiJournalChallenge,
    row: Vec<u8>,
}
impl UiJournalResponse {
    /// The decoder must enforce its length prefix BEFORE allocating the row.
    /// The UI producer supplies only its original-carrier bounded codec bytes.
    pub(crate) fn from_wire(challenge: UiJournalChallenge, row: Vec<u8>) -> io::Result<Self> {
        challenge.validate_shape()?;
        if row.is_empty() || row.len() > MAX_CREATION_ROW_BYTES {
            return Err(refused(
                "The original creation journal response exceeds its byte bound",
            ));
        }
        Ok(Self { challenge, row })
    }
    pub(crate) fn into_wire(self) -> (UiJournalChallenge, Vec<u8>) {
        (self.challenge, self.row)
    }
}

#[cfg(all(target_os = "linux", feature = "host"))]
mod host {
    use super::*;
    use crate::{fs_safety::Root, instance::OriginalUiJournalPeer, process::CancellationToken};
    use std::os::unix::net::UnixStream;

    /// Own this only inside the original connection's special request phase.
    /// No Clone/Deserialize or caller-selected audience constructor exists.
    pub(crate) struct IssuedUiJournalChallenge {
        audience: OriginalUiJournalPeer,
        challenge: UiJournalChallenge,
        deadline: Instant,
        cancellation: CancellationToken,
    }
    impl IssuedUiJournalChallenge {
        pub(crate) fn issue(
            stream: &UnixStream,
            request_id: u64,
            proof: HostSessionProof,
            requirement: DirectoryStartRequirement,
            roles: RoleIdentities,
            deadline: Instant,
            cancellation: CancellationToken,
        ) -> io::Result<Self> {
            active(deadline, &cancellation)?;
            let audience = OriginalUiJournalPeer::capture(stream, deadline)?;
            let (device, inode) = audience.database_identity()?;
            let challenge = UiJournalChallenge {
                request_id,
                nonce: OperationId::new(),
                proof,
                requirement,
                window: audience.slot().to_owned(),
                database: [device, inode],
                roles,
            };
            challenge.validate_shape()?;
            active(deadline, &cancellation)?;
            Ok(Self {
                audience,
                challenge,
                deadline,
                cancellation,
            })
        }
        /// Queue this through the connection's existing sole framed writer.
        /// Never write the challenge concurrently on the reader's socket.
        pub(crate) fn challenge(&self) -> &UiJournalChallenge {
            &self.challenge
        }
        /// Consumes the exact original request phase once. `current_proof`
        /// comes from the owning host, never the echoed response or caller.
        pub(crate) fn accept(
            self,
            response: UiJournalResponse,
            roots: &[Root; 6],
            current_request: u64,
            current_proof: &HostSessionProof,
            current_requirement: &DirectoryStartRequirement,
        ) -> io::Result<AuthenticatedCreationOperation> {
            active(self.deadline, &self.cancellation)?;
            validate_echo(
                &self.challenge,
                &response.challenge,
                current_request,
                current_proof,
                current_requirement,
            )?;
            let actual = role_identities(roots)?;
            if actual != self.challenge.roles {
                return Err(refused(
                    "The received original creation roles differ from the challenged roles",
                ));
            }
            self.audience.validate(self.deadline)?;
            active(self.deadline, &self.cancellation)?;
            Ok(AuthenticatedCreationOperation {
                audience: self.audience,
                challenge: self.challenge,
                row: response.row,
                deadline: self.deadline,
                cancellation: self.cancellation,
            })
        }
    }

    /// Authenticated original-peer bytes only, not validated creation/start
    /// authority. Repository receiver must decode and validate every row/role
    /// invariant, then CONSUME this object under the final source admission.
    pub(crate) struct AuthenticatedCreationOperation {
        audience: OriginalUiJournalPeer,
        challenge: UiJournalChallenge,
        row: Vec<u8>,
        deadline: Instant,
        cancellation: CancellationToken,
    }
    impl AuthenticatedCreationOperation {
        pub(crate) fn encoded_operation(&self) -> &[u8] {
            &self.row
        }
        pub(crate) fn operation(&self) -> OperationId {
            self.challenge.requirement.operation
        }
        pub(crate) fn generation(&self) -> u64 {
            self.challenge.requirement.generation
        }
        pub(crate) fn validate_current(
            &self,
            current_request: u64,
            current_proof: &HostSessionProof,
            current_requirement: &DirectoryStartRequirement,
            roots: &[Root; 6],
        ) -> io::Result<()> {
            active(self.deadline, &self.cancellation)?;
            validate_echo(
                &self.challenge,
                &self.challenge,
                current_request,
                current_proof,
                current_requirement,
            )?;
            if role_identities(roots)? != self.challenge.roles {
                return Err(refused(
                    "The authenticated creation role identities changed",
                ));
            }
            self.audience.validate(self.deadline)?;
            active(self.deadline, &self.cancellation)
        }
    }
    fn role_identities(roots: &[Root; 6]) -> io::Result<RoleIdentities> {
        use std::os::unix::fs::MetadataExt;
        let mut result = [[0; 2]; 6];
        for (root, identity) in roots.iter().zip(&mut result) {
            let metadata = root.metadata()?;
            *identity = [metadata.dev(), metadata.ino()];
        }
        Ok(result)
    }
    fn active(deadline: Instant, cancellation: &CancellationToken) -> io::Result<()> {
        if cancellation.is_cancelled() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        Ok(())
    }
}
#[cfg(all(target_os = "linux", feature = "host"))]
pub(crate) use host::{AuthenticatedCreationOperation, IssuedUiJournalChallenge};

fn validate_echo(
    expected: &UiJournalChallenge,
    actual: &UiJournalChallenge,
    current_request: u64,
    current_proof: &HostSessionProof,
    current_requirement: &DirectoryStartRequirement,
) -> io::Result<()> {
    if actual != expected
        || current_request != expected.request_id
        || current_proof != &expected.proof
        || current_requirement != &expected.requirement
    {
        return Err(refused(
            "The creation journal response is stale or belongs to another host request",
        ));
    }
    Ok(())
}
fn refused(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn challenge() -> UiJournalChallenge {
        UiJournalChallenge {
            request_id: 7,
            nonce: OperationId::new(),
            proof: HostSessionProof {
                host_session_id: "host".into(),
                boot_id: "boot".into(),
                host_incarnation: "incarnation".into(),
            },
            requirement: DirectoryStartRequirement {card:crate::model::CardId::new(),session:crate::model::TerminalId::new(),
                operation:OperationId::new(),generation:3,attempt:uuid::Uuid::new_v4(),kind:crate::model::DirectoryStartKind::Terminal},
            window: "primary".into(),
            database: [4, 5],
            roles: [[6, 7]; 6],
        }
    }
    #[test]
    fn response_cannot_cross_nonce_request_host_or_role_identity() {
        let expected = challenge();
        validate_echo(&expected, &expected, 7, &expected.proof, &expected.requirement).unwrap();
        let mut changed = expected.clone();
        changed.nonce = OperationId::new();
        assert!(validate_echo(&expected, &changed, 7, &expected.proof, &expected.requirement).is_err());
        assert!(validate_echo(&expected, &expected, 8, &expected.proof, &expected.requirement).is_err());
        let mut host = expected.proof.clone();
        host.host_incarnation = "restarted".into();
        assert!(validate_echo(&expected, &expected, 7, &host, &expected.requirement).is_err());
        let mut recipient=expected.requirement.clone(); recipient.card=crate::model::CardId::new();
        assert!(validate_echo(&expected,&expected,7,&expected.proof,&recipient).is_err());
        let mut recipient=expected.requirement.clone(); recipient.attempt=uuid::Uuid::new_v4();
        assert!(validate_echo(&expected,&expected,7,&expected.proof,&recipient).is_err());
        let mut changed = expected.clone();
        changed.roles[5][1] += 1;
        assert!(validate_echo(&expected, &changed, 7, &expected.proof, &expected.requirement).is_err());
    }
    #[test]
    fn response_and_host_proof_have_strict_bounds() {
        assert!(UiJournalResponse::from_wire(challenge(), vec![]).is_err());
        assert!(
            UiJournalResponse::from_wire(challenge(), vec![0; MAX_CREATION_ROW_BYTES + 1]).is_err()
        );
        let mut changed = challenge();
        changed.proof.host_incarnation = "x".repeat(257).into();
        assert!(changed.validate_shape().is_err());
        let mut changed = challenge();
        changed.window = "../other".into();
        assert!(changed.validate_shape().is_err());
    }
}
