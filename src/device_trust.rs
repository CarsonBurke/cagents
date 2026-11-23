//! How a session on this computer may drive agents on a linked device.
//!
//! A person links a device once: this computer's daemon and the device's
//! daemon then share a key that names this computer as a peer. Whether sessions
//! here may use the device is a separate grant, which only a person sets.
//!
//! A session never carries its own capability to the device. For each
//! connection, the device sends a nonce, this computer's daemon signs it for
//! the caller's live session lineage when the grant allows, and the device then
//! issues a capability for that peer lineage on that connection. The
//! capability lapses after [`CAPABILITY_LIFETIME`] unless the connection proves
//! itself again, which the CLI does every [`CAPABILITY_RENEWAL`], so denying
//! agents, unlinking, ending the session or lowering its mode reaches a
//! connection that is already open: the device ends it, or holds it to the
//! lower ceiling this computer vouches for next. Forgetting or re-keying a
//! peer on the device ends that peer's connections at once. On the device that
//! capability has a session's scope: through
//! cagents requests it reaches its own children and terminals, listings and
//! messaging, never accounts, lifecycle, installs or device links. Like a
//! session there, a terminal it starts runs commands as the device's user.

use serde::{Deserialize, Serialize};

use crate::agent_execution::Lineage;

/// The bytes of a link key, a nonce and a proof.
pub const SECRET_BYTES: usize = 32;

/// How long a device capability lasts after its connection's latest proof.
/// It bounds how long a connection keeps access that this computer has since
/// stopped vouching for.
pub const CAPABILITY_LIFETIME: std::time::Duration = std::time::Duration::from_secs(30);

/// How soon after a proof the CLI proves its connection again: half the
/// lifetime, leaving the other half for the round trips.
pub const CAPABILITY_RENEWAL: std::time::Duration = std::time::Duration::from_secs(15);

/// The secret two daemons share for one link. It never appears in
/// `devices.json`, diagnostics or a public reply.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LinkKey([u8; SECRET_BYTES]);

impl std::fmt::Debug for LinkKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LinkKey(<redacted>)")
    }
}

impl LinkKey {
    /// A fresh key from the kernel's random pool.
    pub fn generate() -> std::io::Result<Self> {
        let mut key = [0; SECRET_BYTES];
        crate::fs_safety::fill_random(&mut key)?;
        Ok(Self(key))
    }

    #[cfg(any(test, feature = "host"))]
    pub(crate) fn from_bytes(bytes: [u8; SECRET_BYTES]) -> Self {
        Self(bytes)
    }

    #[cfg(feature = "host")]
    pub(crate) fn bytes(&self) -> &[u8; SECRET_BYTES] {
        &self.0
    }
}

/// What a person, or the CLI on their behalf, asks of a daemon's links and
/// peers. A daemon answers only connections that act for the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// This computer's links, without their keys.
    Links,
    /// Records the link to `device`, replacing any earlier one.
    SaveLink {
        device: String,
        peer: uuid::Uuid,
        key: LinkKey,
        agents_allowed: bool,
    },
    /// Allows or stops sessions on this computer using `device`.
    SetAgentsAllowed { device: String, allowed: bool },
    /// Forgets the link to `device`. Sessions here can no longer use it.
    RemoveLink { device: String },
    /// On the device: accepts `peer` as a linked computer, replacing its
    /// key if it was linked before.
    RegisterPeer {
        peer: uuid::Uuid,
        label: String,
        key: LinkKey,
    },
    /// On the device: forgets a linked computer.
    ForgetPeer { peer: uuid::Uuid },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Links(Vec<LinkStatus>),
    /// The link saved, and the peer it replaced, which the device should
    /// forget.
    Saved {
        replaced: Option<uuid::Uuid>,
    },
    Updated,
    /// The removed link's peer, if there was one.
    Removed {
        peer: Option<uuid::Uuid>,
    },
    Registered,
    Forgotten {
        existed: bool,
    },
}

/// One link as `cagents device list` shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkStatus {
    pub device: String,
    pub peer: uuid::Uuid,
    pub agents_allowed: bool,
    /// Unix seconds.
    pub linked_at: u64,
}

/// What this computer's daemon vouches for about the session it signs for.
/// The device acts for it only within both.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// The session's lineage on this computer.
    pub lineage: Lineage,
    /// The most the session may give the agents it starts, or approve for
    /// them, as on this computer.
    pub ceiling: crate::agent_execution::permission::StatedCeiling,
}

/// What this computer's daemon signs for a session: the device's nonce, the
/// peer the link names this computer as, and the session's claim.
#[cfg(feature = "host")]
pub(crate) fn proof(
    key: &LinkKey,
    nonce: &[u8; SECRET_BYTES],
    peer: uuid::Uuid,
    claim: &Claim,
) -> [u8; SECRET_BYTES] {
    use hmac::Mac;
    mac(key, nonce, peer, claim).finalize().into_bytes().into()
}

#[cfg(feature = "host")]
fn mac(
    key: &LinkKey,
    nonce: &[u8; SECRET_BYTES],
    peer: uuid::Uuid,
    claim: &Claim,
) -> hmac::Hmac<sha2::Sha256> {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key.bytes())
        .expect("HMAC accepts a key of any length");
    let ceiling = claim.ceiling.signed_bytes();
    mac.update(b"cagents device proof v1\0");
    mac.update(nonce);
    mac.update(peer.as_bytes());
    mac.update(claim.lineage.0.as_uuid().as_bytes());
    mac.update(&(ceiling.len() as u64).to_be_bytes());
    mac.update(&ceiling);
    mac
}

/// Whether `proof` is what [`proof`] signs, compared in constant time.
#[cfg(feature = "host")]
pub(crate) fn verify(
    key: &LinkKey,
    nonce: &[u8; SECRET_BYTES],
    peer: uuid::Uuid,
    claim: &Claim,
    proof: &[u8; SECRET_BYTES],
) -> bool {
    use hmac::Mac;
    mac(key, nonce, peer, claim).verify_slice(proof).is_ok()
}

/// The lineage a peer's session has on the device. It is derived from both,
/// so no peer can name a lineage of the device's own or of another peer.
pub fn peer_lineage(peer: uuid::Uuid, lineage: Lineage) -> Lineage {
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    digest.update(b"cagents peer lineage v1\0");
    digest.update(peer.as_bytes());
    digest.update(lineage.0.as_uuid().as_bytes());
    let hash = digest.finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hash[..16]);
    Lineage(crate::model::TerminalId::from_uuid(
        uuid::Builder::from_custom_bytes(bytes).into_uuid(),
    ))
}

/// The refusal a session gets when this computer has no link to `device`.
pub fn unlinked(device: &str) -> String {
    format!(
        "{device} is not linked to this computer, so sessions here cannot use it. A person can link it by running `cagents device link {device} --allow-agents` in a system terminal outside cagents."
    )
}

/// The refusal a session gets when a person has not allowed agents on `device`.
pub fn not_allowed(device: &str) -> String {
    format!(
        "Agents on this computer are not allowed to use {device}. A person can allow it by running `cagents device allow-agents {device}` in a system terminal outside cagents."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lineage() -> Lineage {
        Lineage(crate::model::TerminalId::new())
    }

    #[test]
    fn a_peer_lineage_is_distinct_per_peer_and_stable() {
        let session = lineage();
        let (first, second) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        assert_eq!(peer_lineage(first, session), peer_lineage(first, session));
        assert_ne!(peer_lineage(first, session), peer_lineage(second, session));
        assert_ne!(peer_lineage(first, session), session);
    }

    #[test]
    fn a_key_never_prints() {
        let key = LinkKey::from_bytes([7; SECRET_BYTES]);
        assert_eq!(format!("{key:?}"), "LinkKey(<redacted>)");
    }

    #[cfg(feature = "host")]
    #[test]
    fn a_proof_binds_its_key_nonce_peer_lineage_and_ceiling() {
        use crate::agent::Provider;
        use crate::agent_execution::permission::StatedCeiling;
        let claim = |lineage, mode: &str| Claim {
            lineage,
            ceiling: StatedCeiling {
                provider: Some(Provider::Claude),
                mode: Some(mode.into()),
            },
        };
        let key = LinkKey::from_bytes([1; SECRET_BYTES]);
        let nonce = [2; SECRET_BYTES];
        let (peer, session) = (uuid::Uuid::new_v4(), lineage());
        let signed_claim = claim(session, "default");
        let signed = proof(&key, &nonce, peer, &signed_claim);
        assert!(verify(&key, &nonce, peer, &signed_claim, &signed));
        assert!(!verify(
            &LinkKey::from_bytes([9; SECRET_BYTES]),
            &nonce,
            peer,
            &signed_claim,
            &signed
        ));
        assert!(!verify(
            &key,
            &[3; SECRET_BYTES],
            peer,
            &signed_claim,
            &signed
        ));
        assert!(!verify(
            &key,
            &nonce,
            uuid::Uuid::new_v4(),
            &signed_claim,
            &signed
        ));
        assert!(!verify(
            &key,
            &nonce,
            peer,
            &claim(lineage(), "default"),
            &signed
        ));
        // Whoever relays the proof cannot raise the session's ceiling.
        assert!(!verify(
            &key,
            &nonce,
            peer,
            &claim(session, "bypassPermissions"),
            &signed
        ));
        let mut unknown = signed_claim.clone();
        unknown.ceiling.mode = None;
        assert!(!verify(&key, &nonce, peer, &unknown, &signed));
    }
}
