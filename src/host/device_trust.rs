//! This daemon's side of device links; see [`crate::device_trust`].
//!
//! The links this computer made, and the computers linked to this one, live
//! in `device-trust.json` beside the workspace registry, readable only by the
//! user. A peer's challenges and the capabilities its sessions hold exist only
//! in memory and end with their connection.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};

use crate::{
    agent_execution::{Authority, Lineage, Origin},
    device_trust::{LinkKey, LinkStatus, Reply, Request, SECRET_BYTES},
    host::state::{ClientId, SharedHost},
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
};

/// Each peer capability begins with this, so it can never be mistaken for a
/// session's own, which is a bare UUID.
const CAPABILITY_PREFIX: &str = "peer-";
const MAX_LINKS: usize = 64;
const MAX_PEERS: usize = 64;
const MAX_LABEL_CHARS: usize = 64;
const MAX_STORE_BYTES: u64 = 256 * 1024;

/// Linked computers' challenges and capabilities, by connection.
#[derive(Default)]
pub(crate) struct Peers {
    challenges: BTreeMap<ClientId, [u8; SECRET_BYTES]>,
    capabilities: HashMap<String, Capability>,
}

struct Capability {
    client: ClientId,
    lineage: Lineage,
}

impl Peers {
    fn challenge(&mut self, client: ClientId) -> io::Result<[u8; SECRET_BYTES]> {
        let mut nonce = [0; SECRET_BYTES];
        crate::fs_safety::fill_random(&mut nonce)?;
        self.challenges.insert(client, nonce);
        Ok(nonce)
    }

    /// The challenge `client` was sent, which a proof may answer only once.
    fn take_challenge(&mut self, client: ClientId) -> Option<[u8; SECRET_BYTES]> {
        self.challenges.remove(&client)
    }

    pub(in crate::host) fn issue(&mut self, client: ClientId, lineage: Lineage) -> String {
        let token = format!("{CAPABILITY_PREFIX}{}", uuid::Uuid::new_v4().simple());
        self.capabilities
            .insert(token.clone(), Capability { client, lineage });
        token
    }

    /// The peer lineage a capability acts for, while its connection is open.
    pub(crate) fn lineage(&self, token: &str) -> Option<Lineage> {
        self.capabilities
            .get(token)
            .map(|capability| capability.lineage)
    }

    /// Whether `token` is the capability `client`'s proof was issued.
    pub(in crate::host) fn issued_to(&self, token: &str, client: ClientId) -> bool {
        self.capabilities
            .get(token)
            .is_some_and(|capability| capability.client == client)
    }

    pub(crate) fn disconnect(&mut self, client: ClientId) {
        self.challenges.remove(&client);
        self.capabilities
            .retain(|_, capability| capability.client != client);
    }
}

/// Whether `authority` is a peer capability, which only this daemon's
/// connections can hold.
pub(crate) fn is_peer_capability(authority: &Authority) -> bool {
    authority
        .token()
        .is_some_and(|token| token.starts_with(CAPABILITY_PREFIX))
}

/// The origin a peer capability acts as, or `None` when `authority` is not
/// one. A capability whose connection has ended is refused rather than
/// treated as some other kind of caller.
pub(crate) fn peer_origin(
    host: &SharedHost,
    authority: &Authority,
) -> Option<Result<Origin, String>> {
    let token = authority.token()?;
    if !token.starts_with(CAPABILITY_PREFIX) {
        return None;
    }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    Some(
        state
            .peers
            .lineage(token)
            .map(|parent| Origin::Child { parent })
            .ok_or_else(|| {
                "This device capability has ended with its connection; run the command again"
                    .to_owned()
            }),
    )
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    links: Vec<StoredLink>,
    #[serde(default)]
    peers: Vec<StoredPeer>,
}

/// A device this computer linked to.
#[derive(Serialize, Deserialize)]
struct StoredLink {
    device: String,
    /// The peer this computer is on that device.
    peer: uuid::Uuid,
    key: StoredKey,
    agents_allowed: bool,
    linked_at: u64,
}

/// A computer linked to this one.
#[derive(Serialize, Deserialize)]
struct StoredPeer {
    peer: uuid::Uuid,
    label: String,
    key: StoredKey,
    linked_at: u64,
}

/// A key written as hexadecimal.
#[derive(Clone)]
struct StoredKey(LinkKey);

impl Serialize for StoredKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let hex: String = self
            .0
            .bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        serializer.serialize_str(&hex)
    }
}

impl<'de> Deserialize<'de> for StoredKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let hex = String::deserialize(deserializer)?;
        let invalid = || serde::de::Error::custom("a link key is 64 hexadecimal digits");
        if hex.len() != SECRET_BYTES * 2 || !hex.is_ascii() {
            return Err(invalid());
        }
        let mut bytes = [0; SECRET_BYTES];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte =
                u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).map_err(|_| invalid())?;
        }
        Ok(Self(LinkKey::from_bytes(bytes)))
    }
}

/// Serializes this daemon's reads and writes of its store.
static STORE: Mutex<()> = Mutex::new(());

fn store_path() -> Result<PathBuf, String> {
    crate::instance::project_dirs()
        .map(|dirs| dirs.data_dir().join("device-trust.json"))
        .ok_or_else(|| "the data directory is unavailable".to_owned())
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

impl Store {
    fn load(path: &Path) -> Result<Self, String> {
        use std::io::Read;
        let file = match crate::secure_file::open_existing_private_file_readonly(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        };
        let mut bytes = Vec::new();
        file.take(MAX_STORE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        if bytes.len() as u64 > MAX_STORE_BYTES {
            return Err(format!("{} is larger than it can be", path.display()));
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("{} is not readable: {error}", path.display()))
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let directory = path
            .parent()
            .ok_or_else(|| "the device trust store has no directory".to_owned())?;
        let failed = |error: io::Error| format!("could not save {}: {error}", path.display());
        crate::secure_file::prepare_private_directory(directory).map_err(failed)?;
        let mut bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        let temporary = directory.join(format!(
            ".device-trust-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)?;
            std::fs::File::open(directory)?.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.map_err(failed)
    }

    fn link(&self, device: &str) -> Option<&StoredLink> {
        self.links.iter().find(|link| link.device == device)
    }

    fn peer(&self, peer: uuid::Uuid) -> Option<&StoredPeer> {
        self.peers.iter().find(|stored| stored.peer == peer)
    }

    fn apply(&mut self, request: Request) -> Result<(Reply, bool), String> {
        Ok(match request {
            Request::Links => (
                Reply::Links(
                    self.links
                        .iter()
                        .map(|link| LinkStatus {
                            device: link.device.clone(),
                            peer: link.peer,
                            agents_allowed: link.agents_allowed,
                            linked_at: link.linked_at,
                        })
                        .collect(),
                ),
                false,
            ),
            Request::SaveLink {
                device,
                peer,
                key,
                agents_allowed,
            } => {
                crate::device::validate_name(&device).map_err(|error| error.to_string())?;
                let replaced = self
                    .links
                    .iter()
                    .position(|link| link.device == device)
                    .map(|index| self.links.remove(index).peer);
                if self.links.len() >= MAX_LINKS {
                    return Err(format!(
                        "cagents keeps at most {MAX_LINKS} device links; unlink one first"
                    ));
                }
                self.links.push(StoredLink {
                    device,
                    peer,
                    key: StoredKey(key),
                    agents_allowed,
                    linked_at: now(),
                });
                self.links.sort_by(|a, b| a.device.cmp(&b.device));
                (Reply::Saved { replaced }, true)
            }
            Request::SetAgentsAllowed { device, allowed } => {
                let link = self
                    .links
                    .iter_mut()
                    .find(|link| link.device == device)
                    .ok_or_else(|| crate::device_trust::unlinked(&device))?;
                let changed = link.agents_allowed != allowed;
                link.agents_allowed = allowed;
                (Reply::Updated, changed)
            }
            Request::RemoveLink { device } => {
                let peer = self
                    .links
                    .iter()
                    .position(|link| link.device == device)
                    .map(|index| self.links.remove(index).peer);
                (Reply::Removed { peer }, peer.is_some())
            }
            Request::RegisterPeer { peer, label, key } => {
                let label: String = label
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(MAX_LABEL_CHARS)
                    .collect();
                self.peers.retain(|stored| stored.peer != peer);
                if self.peers.len() >= MAX_PEERS {
                    return Err(format!(
                        "This device keeps at most {MAX_PEERS} linked computers; unlink one first"
                    ));
                }
                self.peers.push(StoredPeer {
                    peer,
                    label,
                    key: StoredKey(key),
                    linked_at: now(),
                });
                (Reply::Registered, true)
            }
            Request::ForgetPeer { peer } => {
                let before = self.peers.len();
                self.peers.retain(|stored| stored.peer != peer);
                let existed = self.peers.len() != before;
                (Reply::Forgotten { existed }, existed)
            }
        })
    }
}

fn unavailable(message: String) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}

fn denied(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::PermissionDenied, message, false)
}

/// Answers a person's request about links and peers. The caller has already
/// refused connections that act for an agent session.
pub(crate) fn process(request: Request) -> Result<Reply, ProtocolError> {
    process_at(&store_path().map_err(unavailable)?, request)
}

pub(in crate::host) fn process_at(path: &Path, request: Request) -> Result<Reply, ProtocolError> {
    let _store = STORE.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = Store::load(path).map_err(unavailable)?;
    let (reply, changed) = store.apply(request).map_err(unavailable)?;
    if changed {
        store.save(path).map_err(unavailable)?;
    }
    Ok(reply)
}

/// A fresh nonce for `client` to have signed.
pub(crate) fn challenge(
    host: &SharedHost,
    client: ClientId,
) -> Result<[u8; SECRET_BYTES], ProtocolError> {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .peers
        .challenge(client)
        .map_err(|error| {
            ProtocolError::new(
                ProtocolErrorCode::Internal,
                format!("could not make a challenge: {error}"),
                false,
            )
        })
}

/// Checks a linked computer's proof for the challenge `client` was sent and
/// issues the capability its session uses on this connection.
pub(crate) fn authenticate(
    host: &SharedHost,
    client: ClientId,
    peer: uuid::Uuid,
    lineage: Lineage,
    proof: &[u8; SECRET_BYTES],
) -> Result<Authority, ProtocolError> {
    authenticate_at(
        &store_path().map_err(unavailable)?,
        host,
        client,
        peer,
        lineage,
        proof,
    )
}

pub(in crate::host) fn authenticate_at(
    path: &Path,
    host: &SharedHost,
    client: ClientId,
    peer: uuid::Uuid,
    lineage: Lineage,
    proof: &[u8; SECRET_BYTES],
) -> Result<Authority, ProtocolError> {
    let nonce = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .peers
        .take_challenge(client)
        .ok_or_else(|| denied("Ask for a challenge before presenting a device proof"))?;
    let key = {
        let _store = STORE.lock().unwrap_or_else(|p| p.into_inner());
        Store::load(path)
            .map_err(unavailable)?
            .peer(peer)
            .map(|stored| stored.key.0.clone())
    }
    .ok_or_else(|| {
        denied("This computer is not linked to this device; a person can link it again with `cagents device link`")
    })?;
    if !crate::device_trust::verify(&key, &nonce, peer, lineage, proof) {
        return Err(denied("The device proof is not valid"));
    }
    let lineage = crate::device_trust::peer_lineage(peer, lineage);
    // The connection acts for the peer session from here on, whether or not
    // it also authenticates with the capability, so it is admitted only to
    // what a session may do.
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    match state.agent_clients.get(&client) {
        Some(bound) if *bound != lineage => {
            return Err(denied(
                "This connection already acts for another agent session",
            ));
        }
        _ => {
            state.agent_clients.insert(client, lineage);
        }
    }
    let token = state.peers.issue(client, lineage);
    Ok(Authority::session(token))
}

/// Signs `nonce` from `device` for the live session `authority` was issued
/// to, when a person allowed sessions on this computer to use the device.
pub(crate) fn sign(
    host: &SharedHost,
    authority: &Authority,
    device: &str,
    nonce: &[u8; SECRET_BYTES],
) -> Result<(uuid::Uuid, Lineage, [u8; SECRET_BYTES]), ProtocolError> {
    sign_at(
        &store_path().map_err(unavailable)?,
        host,
        authority,
        device,
        nonce,
    )
}

pub(in crate::host) fn sign_at(
    path: &Path,
    host: &SharedHost,
    authority: &Authority,
    device: &str,
    nonce: &[u8; SECRET_BYTES],
) -> Result<(uuid::Uuid, Lineage, [u8; SECRET_BYTES]), ProtocolError> {
    if authority.token().is_none() {
        return Err(denied(
            "Only an agent session signs a device challenge; a person's own commands use the device directly",
        ));
    }
    if is_peer_capability(authority) {
        return Err(denied(
            "A session from another computer cannot use this computer's devices",
        ));
    }
    let (origin, session) = super::agent_execution::caller(host, authority).map_err(denied)?;
    let (Origin::Child { parent }, Some(_)) = (origin, session) else {
        return Err(denied(
            "Only a live agent session on this computer can use a device",
        ));
    };
    let _store = STORE.lock().unwrap_or_else(|p| p.into_inner());
    let store = Store::load(path).map_err(unavailable)?;
    let link = store
        .link(device)
        .ok_or_else(|| denied(crate::device_trust::unlinked(device)))?;
    if !link.agents_allowed {
        return Err(denied(crate::device_trust::not_allowed(device)));
    }
    Ok((
        link.peer,
        parent,
        crate::device_trust::proof(&link.key.0, nonce, link.peer, parent),
    ))
}

#[cfg(test)]
mod tests;
