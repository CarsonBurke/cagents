//! This daemon's side of device links; see [`crate::device_trust`].
//!
//! The links this computer made, and the computers linked to this one, live
//! in `device-trust.json` beside the workspace registry, readable only by the
//! user. A peer's challenges and the capabilities its sessions hold exist only
//! in memory. A capability ends with its connection, when it lapses unproved
//! after [`CAPABILITY_LIFETIME`], or when its peer is forgotten or re-keyed;
//! the daemon then closes that connection.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Instant,
};

use serde::{Deserialize, Serialize};

use crate::{
    agent_execution::{Authority, Lineage, Origin, permission::Ceiling},
    device_trust::{CAPABILITY_LIFETIME, Claim, LinkKey, LinkStatus, Reply, Request, SECRET_BYTES},
    host::{
        delivery::DeliveryQueue,
        state::{ClientId, HostLock, HostState, SharedHost},
    },
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
    /// Each connection a linked computer proved, with the capability its
    /// latest proof issued. An entry stays after its capability ends, so the
    /// connection is refused everything, never treated as a person's, until
    /// it closes.
    connections: BTreeMap<ClientId, Capability>,
    /// The thread that closes connections whose capability lapsed, while any
    /// capability is live.
    reaper: Option<std::thread::Thread>,
}

struct Capability {
    token: String,
    peer: uuid::Uuid,
    lineage: Lineage,
    /// The ceiling the linked computer's daemon vouched for: the session's
    /// own there, which the agents it starts here never exceed.
    ceiling: Ceiling,
    /// When it lapses unless the connection proves itself again.
    expires: Instant,
    /// Ended, because it lapsed or its peer was forgotten or re-keyed. Its
    /// connection is being closed.
    revoked: bool,
}

impl Capability {
    fn live(&self, now: Instant) -> bool {
        !self.revoked && now < self.expires
    }
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

    pub(in crate::host) fn issue(
        &mut self,
        client: ClientId,
        peer: uuid::Uuid,
        lineage: Lineage,
        ceiling: Ceiling,
    ) -> String {
        // A connection holds one capability: proving it again replaces the
        // last, with whatever ceiling the new proof vouched for.
        let token = format!("{CAPABILITY_PREFIX}{}", uuid::Uuid::new_v4().simple());
        self.connections.insert(
            client,
            Capability {
                token: token.clone(),
                peer,
                lineage,
                ceiling,
                expires: Instant::now() + CAPABILITY_LIFETIME,
                revoked: false,
            },
        );
        token
    }

    /// The live capability `token` names.
    fn live(&self, token: &str) -> Option<&Capability> {
        let now = Instant::now();
        self.connections
            .values()
            .find(|capability| capability.token == token && capability.live(now))
    }

    /// The peer lineage a capability acts for, while it is live.
    pub(crate) fn lineage(&self, token: &str) -> Option<Lineage> {
        self.live(token).map(|capability| capability.lineage)
    }

    /// Whether `token` is the live capability `client`'s proof was issued.
    pub(in crate::host) fn issued_to(&self, token: &str, client: ClientId) -> bool {
        self.connections
            .get(&client)
            .is_some_and(|capability| capability.token == token && capability.live(Instant::now()))
    }

    /// Whether `client` was proved for a linked computer's session whose
    /// capability has since ended. Such a connection may do nothing more.
    pub(in crate::host) fn ended(&self, client: ClientId) -> bool {
        self.connections
            .get(&client)
            .is_some_and(|capability| !capability.live(Instant::now()))
    }

    /// Ends every capability `peer`'s connections hold, returning those
    /// connections to close.
    fn revoke(&mut self, peer: uuid::Uuid) -> Vec<ClientId> {
        self.connections
            .iter_mut()
            .filter(|(_, capability)| capability.peer == peer && !capability.revoked)
            .map(|(client, capability)| {
                capability.revoked = true;
                *client
            })
            .collect()
    }

    /// Ends the capabilities that lapsed by `now`, returning their
    /// connections to close and when the next live one lapses.
    fn lapse(&mut self, now: Instant) -> (Vec<ClientId>, Option<Instant>) {
        let mut lapsed = Vec::new();
        let mut next = None::<Instant>;
        for (client, capability) in &mut self.connections {
            if capability.revoked {
                continue;
            }
            if now < capability.expires {
                next = Some(next.map_or(capability.expires, |at| at.min(capability.expires)));
            } else {
                capability.revoked = true;
                lapsed.push(*client);
            }
        }
        (lapsed, next)
    }

    pub(crate) fn disconnect(&mut self, client: ClientId) {
        self.challenges.remove(&client);
        self.connections.remove(&client);
    }
}

#[cfg(test)]
impl Peers {
    /// Lapses `client`'s capability now and wakes the watcher, as if its
    /// lifetime had passed unproved.
    pub(in crate::host) fn lapse_now(&mut self, client: ClientId) {
        if let Some(capability) = self.connections.get_mut(&client) {
            capability.expires = Instant::now();
        }
        if let Some(reaper) = &self.reaper {
            reaper.unpark();
        }
    }
}

/// The delivery queues of connections whose device capability ended.
fn queues(state: &HostState, clients: &[ClientId]) -> Vec<Arc<DeliveryQueue>> {
    clients
        .iter()
        .filter_map(|client| state.clients.get(client).cloned())
        .collect()
}

/// Closes those connections, discarding what was queued for them. Called
/// without the host lock, since a backlog can hold whole terminal frames. A
/// closed connection's state is cleared as it leaves; until then it is
/// refused everything.
fn close(queues: Vec<Arc<DeliveryQueue>>) {
    for delivery in queues {
        delivery.abort("its device capability ended");
    }
}

/// Starts the thread that closes connections as their capabilities lapse,
/// unless it is running. It runs only while a capability is live.
fn watch_lapses(host: &SharedHost, state: &mut HostState) {
    if state.peers.reaper.is_some() {
        return;
    }
    let weak = Arc::downgrade(host);
    match std::thread::Builder::new()
        .name("cagentsd-device-capabilities".into())
        .spawn(move || reap(&weak))
    {
        Ok(handle) => state.peers.reaper = Some(handle.thread().clone()),
        // Every use still checks a capability's lifetime; only the prompt
        // close of an idle connection is lost.
        Err(error) => tracing::warn!(%error, "cannot watch device capabilities lapse"),
    }
}

fn reap(host: &Weak<HostLock>) {
    loop {
        let Some(shared) = host.upgrade() else {
            return;
        };
        let next = {
            let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
            let (lapsed, next) = state.peers.lapse(Instant::now());
            let lapsed = queues(&state, &lapsed);
            if next.is_none() {
                // Decided under the lock, so a capability issued after this
                // starts a new watcher.
                state.peers.reaper = None;
            }
            drop(state);
            close(lapsed);
            next
        };
        drop(shared);
        let Some(next) = next else {
            return;
        };
        std::thread::park_timeout(next.saturating_duration_since(Instant::now()));
    }
}

/// Whether `authority` is a peer capability, which only this daemon's
/// connections can hold.
pub(crate) fn is_peer_capability(authority: &Authority) -> bool {
    authority
        .token()
        .is_some_and(|token| token.starts_with(CAPABILITY_PREFIX))
}

/// The ceiling a peer capability's session has, or `None` when `authority`
/// is not one. A capability whose connection has ended is refused.
pub(crate) fn peer_ceiling(
    host: &SharedHost,
    authority: &Authority,
) -> Option<Result<Ceiling, String>> {
    let token = authority.token()?;
    if !token.starts_with(CAPABILITY_PREFIX) {
        return None;
    }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    Some(
        state
            .peers
            .live(token)
            .map(|capability| capability.ceiling.clone())
            .ok_or_else(ended),
    )
}

pub(in crate::host) fn ended() -> String {
    "This device capability has ended; run the command again".to_owned()
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
            .ok_or_else(ended),
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

/// This daemon's links could not be read or saved: a fault here, not a
/// refusal of the request or a connection failure.
fn store_failed(message: String) -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::Internal,
        format!("its device links are unusable: {message}"),
        false,
    )
}

fn denied(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::PermissionDenied, message, false)
}

/// Answers a person's request about links and peers. The caller has already
/// refused connections that act for an agent session.
pub(crate) fn process(host: &SharedHost, request: Request) -> Result<Reply, ProtocolError> {
    process_with(host, &store_path().map_err(store_failed)?, request)
}

pub(in crate::host) fn process_with(
    host: &SharedHost,
    path: &Path,
    request: Request,
) -> Result<Reply, ProtocolError> {
    // A forgotten or re-keyed peer's open connections were proved with a key
    // it no longer holds, so they end. Under the store lock, so no proof
    // checked against the old key issues a capability after this.
    let ended = match &request {
        Request::RegisterPeer { peer, .. } | Request::ForgetPeer { peer } => Some(*peer),
        _ => None,
    };
    let (reply, revoked) = {
        let _store = STORE.lock().unwrap_or_else(|p| p.into_inner());
        let reply = apply_at(path, request)?;
        let revoked = ended.map(|peer| {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let clients = state.peers.revoke(peer);
            queues(&state, &clients)
        });
        (reply, revoked)
    };
    close(revoked.unwrap_or_default());
    Ok(reply)
}

#[cfg(test)]
pub(in crate::host) fn process_at(path: &Path, request: Request) -> Result<Reply, ProtocolError> {
    let _store = STORE.lock().unwrap_or_else(|p| p.into_inner());
    apply_at(path, request)
}

/// Applies `request` to the store at `path`; the caller holds [`STORE`].
fn apply_at(path: &Path, request: Request) -> Result<Reply, ProtocolError> {
    let mut store = Store::load(path).map_err(store_failed)?;
    let (reply, changed) = store.apply(request).map_err(unavailable)?;
    if changed {
        store.save(path).map_err(store_failed)?;
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
    claim: &Claim,
    proof: &[u8; SECRET_BYTES],
) -> Result<Authority, ProtocolError> {
    authenticate_at(
        &store_path().map_err(store_failed)?,
        host,
        client,
        peer,
        claim,
        proof,
    )
}

pub(in crate::host) fn authenticate_at(
    path: &Path,
    host: &SharedHost,
    client: ClientId,
    peer: uuid::Uuid,
    claim: &Claim,
    proof: &[u8; SECRET_BYTES],
) -> Result<Authority, ProtocolError> {
    let nonce = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .peers
        .take_challenge(client)
        .ok_or_else(|| denied("Ask for a challenge before presenting a device proof"))?;
    // Held until the capability is issued, so forgetting or re-keying the
    // peer meanwhile ends it rather than missing it.
    let _store = STORE.lock().unwrap_or_else(|p| p.into_inner());
    let key = Store::load(path)
        .map_err(store_failed)?
        .peer(peer)
        .map(|stored| stored.key.0.clone())
        .ok_or_else(|| {
            denied("This computer is not linked to this device; a person can link it again with `cagents device link`")
        })?;
    if !crate::device_trust::verify(&key, &nonce, peer, claim, proof) {
        return Err(denied("The device proof is not valid"));
    }
    let ceiling = Ceiling::from_stated(&claim.ceiling).map_err(denied)?;
    let lineage = crate::device_trust::peer_lineage(peer, claim.lineage);
    // The connection acts for the peer session from here on, whether or not
    // it also authenticates with the capability, so it is admitted only to
    // what a session may do.
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    // A connection whose capability ended while this proof was on its way is
    // closing; a proof must not revive it.
    if state.peers.ended(client) {
        return Err(denied(ended()));
    }
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
    let token = state.peers.issue(client, peer, lineage, ceiling);
    watch_lapses(host, &mut state);
    let authority = Authority::session(token);
    // What the connection types into a terminal is held to the session's
    // ceiling, as on a local session's own connection.
    state
        .agent_client_authorities
        .insert(client, authority.clone());
    Ok(authority)
}

/// Signs `nonce` from `device` for the live session `authority` was issued
/// to, when a person allowed sessions on this computer to use the device.
pub(crate) fn sign(
    host: &SharedHost,
    authority: &Authority,
    device: &str,
    nonce: &[u8; SECRET_BYTES],
) -> Result<(uuid::Uuid, Claim, [u8; SECRET_BYTES]), ProtocolError> {
    sign_at(
        &store_path().map_err(store_failed)?,
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
) -> Result<(uuid::Uuid, Claim, [u8; SECRET_BYTES]), ProtocolError> {
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
    // A session always has a ceiling; one this daemon cannot read counts as
    // asking, as it does here.
    let ceiling = super::agent_execution::caller_ceiling(host, authority)
        .map_err(denied)?
        .unwrap_or_else(Ceiling::unknown);
    let _store = STORE.lock().unwrap_or_else(|p| p.into_inner());
    let store = Store::load(path).map_err(store_failed)?;
    let link = store
        .link(device)
        .ok_or_else(|| denied(crate::device_trust::unlinked(device)))?;
    if !link.agents_allowed {
        return Err(denied(crate::device_trust::not_allowed(device)));
    }
    let claim = Claim {
        lineage: parent,
        ceiling: ceiling.stated(),
    };
    let proof = crate::device_trust::proof(&link.key.0, nonce, link.peer, &claim);
    Ok((link.peer, claim, proof))
}

#[cfg(test)]
mod tests;
