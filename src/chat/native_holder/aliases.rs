//! Hash-only attestations for descriptors published while attached to an old
//! helper. Every alias remains bound to its exact helper and confirmed server.
use super::{Recovery, cleanup::source_digest};
use crate::{chat::{Session, NativeContinuity}, daemon::holder_registry::{self as registry, Kind, Record}};
use serde::{Deserialize, Serialize};
use std::{fs, io::{Read, Write}, os::unix::fs::OpenOptionsExt, path::PathBuf};

const MAX_BINDINGS: usize = 1024;
const MAX_BYTES: usize = 256 * 1024;
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Confirmation {
    thread: String,
    version: String,
    identity: [u8; 32],
    process: registry::Process,
}
impl Confirmation {
    fn from_recovery(recovery: &Recovery) -> Result<Self, String> {
        use sha2::{Digest, Sha256};
        Ok(Self {
            thread: recovery.confirmed_thread.clone().ok_or("Alias requires confirmed thread")?,
            version: recovery.server_version.clone().ok_or("Alias requires verified server version")?,
            identity: Sha256::digest(serde_json::to_vec(recovery.server_identity.as_ref().ok_or("Alias requires exact server identity")?).map_err(|e| e.to_string())?).into(),
            process: recovery.server_process.clone().ok_or("Alias requires exact server process")?,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Binding { published: [u8; 32], wire: [u8; 32] }
#[derive(Serialize, Deserialize)]
struct Attestation { version: u32, record: Record, confirmation: Confirmation, bindings: Vec<Binding> }
fn path(record: &Record) -> Result<PathBuf, String> {
    Ok(registry::directory(Kind::Native)?.join(format!("{}-{}-{}.aliases", record.terminal, record.holder.pid, record.holder.start)))
}
fn load(record: &Record) -> Result<Option<Attestation>, String> {
    let file = match crate::secure_file::open_existing_private_file(&path(record)?) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BYTES { return Err("Native alias attestation is oversized".into()); }
    let value: Attestation = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if value.version != 1 || value.record != *record || value.bindings.len() > MAX_BINDINGS {
        return Err("Native alias attestation belongs to different ownership evidence".into());
    }
    Ok(Some(value))
}
fn save(value: &Attestation) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BYTES { return Err("Native alias attestation is full".into()); }
    let target = path(&value.record)?;
    let parent = target.parent().ok_or("Native alias attestation has no parent")?;
    let temp = parent.join(format!(".alias-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&temp).map_err(|e| e.to_string())?;
        file.write_all(&bytes).and_then(|_| file.sync_all()).map_err(|e| e.to_string())?;
        fs::rename(&temp, &target).map_err(|e| e.to_string())?;
        fs::File::open(parent).and_then(|f| f.sync_all()).map_err(|e| e.to_string())
    })();
    if result.is_err() { let _ = fs::remove_file(&temp); }
    result
}

/// Called only with recovery received over the authenticated holder endpoint.
/// The None-ID presentation was issued verbatim; only its confirmed ID and
/// capability may have become visible before an old helper rewrote its recipe.
pub(crate) fn recognized_source(record: &Record, recovery: &Recovery, requested: &Session) -> Result<Session, String> {
    let sources = || recovery.previous_sources.iter().chain(std::iter::once(&recovery.source));
    if let Some(source) = sources().find(|source| *source == requested) { return Ok(source.clone()); }
    let confirmation = Confirmation::from_recovery(recovery)?;
    for source in sources().filter(|source| source.launch.session_id.is_none()) {
        for continuity in [NativeContinuity::ReconnectOnly, NativeContinuity::Durable { provider_version: confirmation.version.clone() }] {
            let mut presentation = source.clone();
            presentation.launch.session_id = Some(confirmation.thread.clone());
            presentation.native_continuity = continuity;
            if presentation == *requested { return Ok(source.clone()); }
        }
    }
    if let Some(attestation) = load(record)? {
        if attestation.confirmation != confirmation { return Err("Native alias server confirmation changed".into()); }
        let digest = source_digest(requested)?;
        for binding in attestation.bindings.iter().filter(|binding| binding.published == digest) {
            for source in sources() {
                if source_digest(source)? == binding.wire { return Ok(source.clone()); }
            }
        }
    }
    Err("Native holder does not recognize this exact issued descriptor".into())
}

/// Additional presentations genuinely witnessed by the authenticated holder.
/// Automatic orphan adoption must retain these before an unopened saved pane
/// arrives; hash-only cleanup attestations never invent a full descriptor.
pub(crate) fn presentation_sources(record: &Record, recovery: &Recovery) -> Result<Vec<Session>, String> {
    if record.kind != Kind::Native || record.application != crate::instance::APPLICATION_ID
        || record.terminal != recovery.source.id
        || recovery.source.proof.as_ref().is_none_or(|proof| proof.boot_id != record.boot) {
        return Err("Native presentations lack exact holder identity".into());
    }
    recovery.validate(&record.boot)?;
    let confirmation = Confirmation::from_recovery(recovery)?;
    let mut presentations = Vec::new();
    let mut bytes = 0usize;
    for source in recovery.previous_sources.iter().chain(std::iter::once(&recovery.source)).filter(|source| source.launch.session_id.is_none()) {
        for continuity in [NativeContinuity::ReconnectOnly, NativeContinuity::Durable { provider_version: confirmation.version.clone() }] {
            let mut presentation = source.clone();
            presentation.launch.session_id = Some(confirmation.thread.clone());
            presentation.native_continuity = continuity;
            if presentation == recovery.source || recovery.previous_sources.contains(&presentation) || presentations.contains(&presentation) { continue; }
            bytes = bytes.saturating_add(serde_json::to_vec(&presentation).map_err(|e| e.to_string())?.len());
            if presentations.len() >= 1024 || bytes > 4 * 1024 * 1024 {
                return Err("Native presentation lineage is full; existing provider ownership was preserved".into());
            }
            presentations.push(presentation);
        }
    }
    Ok(presentations)
}

/// The caller has authenticated the helper and its canonical wire descriptor.
/// Persist before publishing the presentation; never evict an issued alias.
pub(super) fn retain(record: &Record, recovery: &Recovery, published: &Session, wire: &Session) -> Result<(), String> {
    if !recovery.previous_sources.iter().chain(std::iter::once(&recovery.source)).any(|source| source == wire) {
        return Err("Native alias lacks exact retained wire ownership".into());
    }
    Publisher::new(recovery)?.retain(record, published, wire)
}

/// Keep only bounded server identity in the actor, never a duplicate of the
/// helper's up-to-4MiB launch lineage for each recovered conversation.
pub(super) struct Publisher { confirmation: Confirmation }
impl Publisher {
    pub(super) fn new(recovery: &Recovery) -> Result<Self, String> {
        Ok(Self { confirmation: Confirmation::from_recovery(recovery)? })
    }
    /// `wire` is the exact canonical descriptor acknowledged by this helper.
    pub(super) fn retain(&self, record: &Record, published: &Session, wire: &Session) -> Result<(), String> {
        if published == wire { return Ok(()); }
        let mut value = load(record)?.unwrap_or_else(|| Attestation { version: 1, record: record.clone(), confirmation: self.confirmation.clone(), bindings: Vec::new() });
        if value.confirmation != self.confirmation { return Err("Native alias server confirmation changed".into()); }
        let binding = Binding { published: source_digest(published)?, wire: source_digest(wire)? };
        if !value.bindings.contains(&binding) {
            if value.bindings.len() >= MAX_BINDINGS { return Err("Native alias ownership lineage is full; existing aliases were preserved".into()); }
            value.bindings.push(binding);
            save(&value)?;
        }
        Ok(())
    }
}

/// A missing socket is never teardown proof. The exact helper's receipt must
/// explicitly include the canonical descriptor bound by this attestation.
pub(super) fn receipt_covers(record: &Record, published: &[u8;32], sources: &[[u8;32]]) -> Result<bool, String> {
    Ok(load(record)?.is_some_and(|value| value.bindings.iter().any(|binding| &binding.published == published && sources.contains(&binding.wire))))
}

/// Called only after the exact teardown receipt has been consumed. Losing an
/// alias before that point could strand a retry after a lost acknowledgment.
pub(super) fn forget(record: &Record) {
    match path(record).and_then(|path| fs::remove_file(path).map_err(|e| e.to_string())) {
        Ok(()) => {}
        Err(error) => tracing::debug!(%error, "native alias attestation already absent or retained after cleanup"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn legacy() -> (Record, Recovery, Session) {
        let mut recovery = super::super::tests::recovery();
        let initial = recovery.source.clone();
        recovery.confirm("thread-retained".into(), super::super::tests::server_identity(), registry::Process { pid: 42, start: 1 }).unwrap();
        // Released confirmation retains the exact initial None-ID alias.
        // Fresh --remote recipes do not acquire a resume prefix here; only
        // an existing resume ID would be rewritten by the released helper.
        crate::chat::protocol::align_resume_identity(&mut recovery.source.launch).unwrap();
        let record = Record { version: 3, build: None, kind: Kind::Native, application: crate::instance::APPLICATION_ID.into(), boot: initial.proof.as_ref().unwrap().boot_id.clone(), terminal: initial.id, holder: registry::Process { pid: 43, start: 2 }, root: registry::Process { pid: 42, start: 1 } };
        (record, recovery, initial)
    }
    #[test]
    fn legacy_named_presentation_requires_exact_initial_alias_and_confirmation() {
        let (record, mut recovery, initial) = legacy();
        for capability in [NativeContinuity::ReconnectOnly, recovery.source.native_continuity.clone()] {
            let mut presentation = initial.clone();
            presentation.launch.session_id = recovery.confirmed_thread.clone();
            presentation.native_continuity = capability;
            assert_eq!(recognized_source(&record, &recovery, &presentation).unwrap(), initial);
        }
        let projections = presentation_sources(&record, &recovery).unwrap();
        assert_eq!(projections.len(), 1);
        assert_eq!(projections[0].native_continuity, NativeContinuity::ReconnectOnly);
        assert_eq!(recognized_source(&record, &recovery, &projections[0]).unwrap(), initial);
        recovery.confirmed_thread = None;
        let mut named = initial; named.launch.session_id = Some("thread-retained".into());
        assert!(recognized_source(&record, &recovery, &named).is_err());
    }
    #[test]
    fn alias_attestations_survive_proof_rotations_and_bind_cleanup_exactly() {
        const STAGE: &str = "CAGENTS_ALIAS_ATTESTATION_TEST";
        if std::env::var_os(STAGE).is_none() {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "chat::native_holder::aliases::tests::alias_attestations_survive_proof_rotations_and_bind_cleanup_exactly", "--nocapture"])
                .env(STAGE, "1").env("XDG_RUNTIME_DIR", root.path())
                .env("XDG_DATA_HOME", root.path().join("data")).output().unwrap();
            assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            return;
        }
        let (record, mut recovery, initial) = legacy();
        fs::create_dir_all(path(&record).unwrap().parent().unwrap()).unwrap();
        let mut published = initial.clone();
        published.launch.session_id = recovery.confirmed_thread.clone();
        published.native_continuity = recovery.source.native_continuity.clone();
        assert_eq!(published, recovery.source, "released fresh confirmation preserves the recipe");
        let mut original = published.clone();
        // Confirmed ID and durable capability occupy separate runtime locks;
        // this named ReconnectOnly presentation is the bounded early alias.
        original.native_continuity = NativeContinuity::ReconnectOnly;
        let wire = recognized_source(&record, &recovery, &original).unwrap();
        let mut proof = published.proof.clone().unwrap(); proof.host_incarnation = crate::model::HostIncarnation::new("successor-one");
        recovery.adopt(&wire, proof.clone()).unwrap();
        retain(&record, &recovery, &original, &wire).unwrap();
        published.proof = Some(proof);
        retain(&record, &recovery, &published, &recovery.source).unwrap();
        let serialized = serde_json::to_vec(&recovery).unwrap();
        let mut recovered: Recovery = serde_json::from_slice(&serialized).unwrap();
        assert_eq!(recognized_source(&record, &recovered, &published).unwrap(), recovered.source);
        let canonical = recovered.source.clone();
        let mut proof = published.proof.clone().unwrap(); proof.host_incarnation = crate::model::HostIncarnation::new("successor-two");
        recovered.adopt(&canonical, proof.clone()).unwrap();
        published.proof = Some(proof);
        retain(&record, &recovered, &published, &recovered.source).unwrap();
        assert_eq!(recognized_source(&record, &recovered, &published).unwrap(), recovered.source);
        assert_eq!(recognized_source(&record, &recovered, &original).unwrap(), initial);
        let hash = source_digest(&original).unwrap();
        assert!(!receipt_covers(&record, &hash, &[]).unwrap());
        assert!(receipt_covers(&record, &hash, &[source_digest(&initial).unwrap()]).unwrap());
        assert!(!receipt_covers(&record, &hash, &[source_digest(&recovered.source).unwrap()]).unwrap());
        for mutation in 0..7 {
            let mut forged = published.clone();
            match mutation {
                0 => forged.launch.session_id = Some("foreign-thread".into()),
                1 => forged.proof.as_mut().unwrap().host_incarnation = crate::model::HostIncarnation::new("unissued-proof"),
                2 => { forged.launch.recipe.environment.insert("CODEX_HOME".into(), "/foreign-account".into()); },
                3 => forged.owned_codex_listener.as_mut().unwrap().socket = "/foreign-listener".into(),
                4 => forged.launch.recipe.arguments.push("--foreign-option".into()),
                5 => forged.launch.recipe.working_directory = "/foreign-cwd".into(),
                _ => forged.terminal = Some(crate::model::TerminalSession { id: forged.id, launch: forged.launch.recipe.clone(), continuity: crate::model::TerminalContinuity::Live { host_session_id: forged.proof.as_ref().unwrap().host_session_id.clone(), boot_id: forged.proof.as_ref().unwrap().boot_id.clone(), host_incarnation: forged.proof.as_ref().unwrap().host_incarnation.clone() }, transcript_path: None, resume_hint: None, owned_codex_listener: forged.owned_codex_listener.clone(), recovery_origin: None, predecessor: None }),
            }
            assert!(recognized_source(&record, &recovered, &forged).is_err(), "accepted mutation {mutation}");
            assert!(!receipt_covers(&record, &source_digest(&forged).unwrap(), &[source_digest(&recovered.source).unwrap()]).unwrap());
        }
        let mut wrong = recovered.clone(); wrong.server_process.as_mut().unwrap().start += 1;
        assert!(Publisher::new(&wrong).unwrap().retain(&record, &original, &recovered.source).is_err());
        let mut replacement = record.clone(); replacement.root.start += 1;
        assert!(receipt_covers(&replacement, &hash, &[source_digest(&initial).unwrap()]).is_err());
        let mut full = load(&record).unwrap().unwrap();
        full.bindings = (0..MAX_BINDINGS).map(|index| { let mut hash = [0;32]; hash[..8].copy_from_slice(&(index as u64).to_le_bytes()); Binding { published: hash, wire: hash } }).collect();
        save(&full).unwrap();
        assert!(retain(&record, &recovered, &original, &recovered.source).unwrap_err().contains("full"));
        assert_eq!(load(&record).unwrap().unwrap().bindings, full.bindings);
        let bytes = fs::read(path(&record).unwrap()).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("--remote"));
        assert!(!String::from_utf8_lossy(&bytes).contains("confirmed-codex-home"));
    }
}
