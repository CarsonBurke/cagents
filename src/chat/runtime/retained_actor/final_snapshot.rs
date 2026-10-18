//! Exact bounded final publication retained until host cleanup is acknowledged.
#[cfg(test)]
use super::transport::RemoteActor;
use super::transport::{ActorIdentity, PROTOCOL, RetirementReceipt};
use super::{MAX_FRAME, execution_exit};
use crate::{
    chat::{self, Snapshot},
    daemon::holder_registry::{self as registry, Kind, Record},
    model::HostSessionProof,
};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::File,
    io::{Read, Write},
    path::PathBuf,
};
/// Observation is not retirement authority. Only the validated capsule grants
/// a caller access to the authenticated, fully joined retirement descriptor.
#[derive(Debug)]
pub(crate) enum RetirementInspection {
    Running,
    ExitedUnproved,
    Verified(VerifiedRetirement),
}
#[derive(Debug)]
pub(crate) struct VerifiedRetirement(RetirementReceipt);
impl VerifiedRetirement {
    pub(crate) fn receipt(&self) -> &RetirementReceipt {
        &self.0
    }
    pub(crate) fn into_receipt(self) -> RetirementReceipt {
        self.0
    }
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    #[serde(
        default = "super::transport::legacy_installed",
        skip_serializing_if = "super::transport::is_installed"
    )]
    installed: bool,
    identity: ActorIdentity,
    proof: HostSessionProof,
    source: chat::Session,
    previous_sources: Vec<chat::Session>,
    paired_holder: Option<crate::terminal::holder::Record>,
    launch_account_scope: Result<crate::account::scope::LaunchAccountScope, String>,
    confirmed_session: Option<String>,
    snapshot: Snapshot,
}
fn path(record: &Record) -> Result<PathBuf, String> {
    Ok(registry::directory(Kind::Native)?.join(format!(
        "{}-{}-{}.final",
        record.terminal, record.holder.pid, record.holder.start
    )))
}
pub(super) fn save(
    installed: bool,
    identity: &ActorIdentity,
    proof: &HostSessionProof,
    source: &chat::Session,
    previous_sources: &[chat::Session],
    paired_holder: Option<&crate::terminal::holder::Record>,
    scope: Result<&crate::account::scope::LaunchAccountScope, &str>,
    confirmed_session: Option<&str>,
    snapshot: &Snapshot,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(&Receipt {
        installed,
        identity: identity.clone(),
        proof: proof.clone(),
        source: source.clone(),
        previous_sources: previous_sources.to_vec(),
        paired_holder: paired_holder.cloned(),
        launch_account_scope: scope.cloned().map_err(str::to_owned),
        confirmed_session: confirmed_session.map(str::to_owned),
        snapshot: snapshot.clone(),
    })
    .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_FRAME - 64 * 1024 {
        return Err(
            "The final retained publication exceeds the private bounded snapshot capacity".into(),
        );
    }
    save_at(&path(&identity.record)?, &bytes)
}
fn save_at(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("Missing final snapshot directory")?;
    let temporary = parent.join(format!(".actor-final-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| error.to_string())?;
        std::fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
        File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
pub(super) fn read(
    identity: &ActorIdentity,
    proof: &HostSessionProof,
    source: &chat::Session,
) -> Result<Snapshot, String> {
    let receipt = read_receipt(&identity.record)?;
    validate_publication(&receipt, identity, proof, source)?;
    Ok(receipt.snapshot)
}
fn read_receipt(record: &Record) -> Result<Receipt, String> {
    let file = crate::secure_file::open_existing_private_file_readonly(&path(record)?)
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take((MAX_FRAME + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_FRAME {
        return Err("Final retained snapshot is oversized".into());
    }
    let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    Ok(receipt)
}
fn validate_publication(
    receipt: &Receipt,
    identity: &ActorIdentity,
    proof: &HostSessionProof,
    source: &chat::Session,
) -> Result<(), String> {
    if identity.instance.is_nil()
        || &receipt.identity != identity
        || &receipt.proof != proof
        || receipt.source.id != identity.record.terminal
        || receipt.source.launch.provider != crate::agent::Provider::Claude
        || receipt.source.owned_codex_listener.is_some()
        || !matches!(
            (&receipt.source.native_continuity, receipt.installed),
            (chat::NativeContinuity::ReconnectOnly, _)
                | (
                    chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL },
                    true
                )
        )
        || receipt
            .source
            .launch
            .session_id
            .as_ref()
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
        || receipt
            .confirmed_session
            .as_ref()
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
        || receipt
            .source
            .proof
            .as_ref()
            .is_none_or(|issued| issued.boot_id != proof.boot_id)
        || (source != &receipt.source && !receipt.previous_sources.contains(source))
        || !receipt.snapshot.exited
        || !execution_exit::identity_absent(&identity.record.root)?
    {
        return Err("Final retained snapshot belongs to another actor, source, attachment, or still-live provider".into());
    }
    Ok(())
}
/// Provider closure proves neither semantic retirement nor permission to start a replacement.
pub(super) fn close_proved(
    identity: &ActorIdentity,
    proof: &HostSessionProof,
    source: &chat::Session,
) -> Result<bool, String> {
    if !execution_exit::identity_absent(&identity.record.root)? {
        return Ok(false);
    }
    read(identity, proof, source).map(|_| true)
}
const MAX_RETIREMENT: usize = 256 * 1024;
#[derive(Serialize, Deserialize)]
struct Retirement {
    protocol: u32,
    semantic_acknowledged: bool,
    #[serde(flatten)]
    receipt: RetirementReceipt,
}
fn retirement_path(record: &Record) -> Result<PathBuf, String> {
    Ok(path(record)?.with_extension("retired"))
}
pub(super) fn publish_retirement(receipt: RetirementReceipt) -> Result<(), String> {
    let path = retirement_path(&receipt.identity.record)?;
    let bytes = serde_json::to_vec(&Retirement {
        protocol: PROTOCOL,
        semantic_acknowledged: true,
        receipt,
    })
    .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_RETIREMENT {
        return Err("Retained semantic retirement proof is oversized".into());
    }
    save_at(&path, &bytes)
}
pub(super) fn retirement_proved(
    identity: &ActorIdentity,
    proof: &HostSessionProof,
) -> Result<bool, String> {
    retirement_proved_inner(&identity.record, proof, Some(identity))
}

pub(super) fn retirement_proved_for_record(
    record: &Record,
    proof: &HostSessionProof,
) -> Result<bool, String> {
    retirement_proved_inner(record, proof, None)
}

fn retirement_proved_inner(
    record: &Record,
    proof: &HostSessionProof,
    expected_identity: Option<&ActorIdentity>,
) -> Result<bool, String> {
    Ok(matches!(
        inspect_retirement_with_proof(
            record,
            proof,
            expected_identity,
            AttachmentProof::ExactCurrent
        )?,
        RetirementInspection::Verified(_)
    ))
}

enum AttachmentProof {
    ExactCurrent,
    SavedSameBoot,
}

pub(super) fn inspect_retirement(
    record: &Record,
    current: &HostSessionProof,
) -> Result<RetirementInspection, String> {
    inspect_retirement_with_proof(record, current, None, AttachmentProof::SavedSameBoot)
}

fn inspect_retirement_with_proof(
    record: &Record,
    current: &HostSessionProof,
    expected_identity: Option<&ActorIdentity>,
    authority: AttachmentProof,
) -> Result<RetirementInspection, String> {
    if !current.is_complete() || record.boot != current.boot_id {
        return Err(
            "Retained retirement inspection names another boot or incomplete current lease".into(),
        );
    }
    let _exit = match execution_exit::observe(&record.holder)? {
        execution_exit::Observation::Running => return Ok(RetirementInspection::Running),
        execution_exit::Observation::Exited(exit) => exit,
    };
    if !execution_exit::identity_absent(&record.root)? {
        return Ok(RetirementInspection::ExitedUnproved);
    }
    let file =
        match crate::secure_file::open_existing_private_file_readonly(&retirement_path(record)?) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RetirementInspection::ExitedUnproved);
            }
            Err(error) => return Err(error.to_string()),
        };
    let mut bytes = Vec::new();
    file.take((MAX_RETIREMENT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_RETIREMENT {
        return Err("Retained semantic retirement proof is oversized".into());
    }
    let saved: Retirement = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if saved.receipt.proof.boot_id != current.boot_id || !saved.receipt.proof.is_complete() {
        return Err(
            "Retained semantic retirement belongs to another boot or incomplete attachment".into(),
        );
    }
    // A live attachment requires its exact lease; a cold reader separately
    // authenticates the current lease and the receipt's historical lease.
    let proof = match authority {
        AttachmentProof::ExactCurrent => current,
        AttachmentProof::SavedSameBoot => &saved.receipt.proof,
    };
    validate_retirement(&saved, record, proof, expected_identity)?;
    validate_retired_final(&saved.receipt)?;
    if registry::retirement_pending(record).map_err(|error| error.to_string())? {
        return Ok(RetirementInspection::ExitedUnproved);
    }
    Ok(RetirementInspection::Verified(VerifiedRetirement(
        saved.receipt,
    )))
}

fn validate_retirement(
    receipt: &Retirement,
    record: &Record,
    proof: &HostSessionProof,
    expected_identity: Option<&ActorIdentity>,
) -> Result<(), String> {
    if receipt.protocol != PROTOCOL
        || !receipt.semantic_acknowledged
        || receipt.receipt.identity.instance.is_nil()
        || &receipt.receipt.identity.record != record
        || expected_identity.is_some_and(|expected| expected != &receipt.receipt.identity)
        || &receipt.receipt.proof != proof
        || receipt.receipt.source.id != record.terminal
        || receipt.receipt.source.launch.provider != crate::agent::Provider::Claude
        || !receipt.receipt.source.has_valid_native_continuity()
        || receipt.receipt.source.owned_codex_listener.is_some()
        || receipt
            .receipt
            .confirmed_session
            .as_ref()
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
        || receipt
            .receipt
            .source
            .launch
            .session_id
            .as_ref()
            .zip(receipt.receipt.confirmed_session.as_ref())
            .is_some_and(|(issued, confirmed)| issued != confirmed)
        || receipt
            .receipt
            .source
            .launch
            .session_id
            .as_ref()
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
        || !matches!(
            (
                receipt.receipt.installed,
                &receipt.receipt.source.native_continuity
            ),
            (
                true,
                chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
            ) | (false, chat::NativeContinuity::ReconnectOnly)
        )
        || receipt
            .receipt
            .source
            .proof
            .as_ref()
            .is_none_or(|source| !source.is_complete() || source.boot_id != proof.boot_id)
        || receipt
            .receipt
            .launch_account_scope
            .as_ref()
            .is_ok_and(|scope| {
                scope.provider != crate::agent::Provider::Claude
                    || !scope.canonical_home.is_absolute()
            })
    {
        return Err(
            "Retained semantic retirement receipt belongs to another actor or attachment".into(),
        );
    }
    Ok(())
}
fn validate_retired_final(receipt: &RetirementReceipt) -> Result<(), String> {
    let final_receipt = read_receipt(&receipt.identity.record)?;
    validate_retired_publication(&final_receipt, receipt)
}
fn validate_retired_publication(
    final_receipt: &Receipt,
    receipt: &RetirementReceipt,
) -> Result<(), String> {
    validate_publication(
        final_receipt,
        &receipt.identity,
        &receipt.proof,
        &receipt.source,
    )?;
    if final_receipt.installed != receipt.installed
        || final_receipt.launch_account_scope != receipt.launch_account_scope
        || final_receipt.confirmed_session != receipt.confirmed_session
        || final_receipt.paired_holder != receipt.paired_holder
        || receipt.previous_sources.iter().any(|source| {
            source != &final_receipt.source && !final_receipt.previous_sources.contains(source)
        })
    {
        return Err(
            "Retired helper changed its final captured account or confirmed session".into(),
        );
    }
    Ok(())
}
pub(super) fn retired_actor_receipt(
    record: &Record,
    current: &HostSessionProof,
) -> Result<Option<RetirementReceipt>, String> {
    match inspect_retirement(record, current)? {
        RetirementInspection::Verified(receipt) => Ok(Some(receipt.into_receipt())),
        RetirementInspection::Running | RetirementInspection::ExitedUnproved => Ok(None),
    }
}

pub(crate) fn remove(record: &Record) -> Result<(), String> {
    let path = path(record)?;
    match std::fs::remove_file(retirement_path(record)?) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    match std::fs::remove_file(&path) {
        Ok(()) => File::open(path.parent().ok_or("Missing final snapshot directory")?)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reaped_process() -> registry::Process {
        struct Owner(std::process::Child);
        impl Drop for Owner {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Owner(
            std::process::Command::new("/bin/cat")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let identity = registry::Process::read(child.0.id() as i32).unwrap();
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        identity
    }

    #[test]
    fn final_close_proof_requires_exact_source_attachment_and_dead_provider() {
        let actor = RemoteActor::fixture();
        let mut identity = actor.identity();
        let proof = actor.proof();
        let mut source = super::super::transport::fixture_source();
        source.id = identity.record.terminal;
        source.proof = Some(proof.clone());
        let live = Receipt {
            installed: true,
            identity: identity.clone(),
            proof: proof.clone(),
            source: source.clone(),
            previous_sources: Vec::new(),
            paired_holder: None,
            launch_account_scope: Err("fixture".into()),
            confirmed_session: None,
            snapshot: Snapshot {
                exited: true,
                ..Default::default()
            },
        };
        assert!(validate_publication(&live, &identity, &proof, &source).is_err());
        identity.record.root = reaped_process();
        let mut receipt = Receipt {
            installed: true,
            identity: identity.clone(),
            ..live
        };
        assert!(validate_publication(&receipt, &identity, &proof, &source).is_ok());
        let mut changed_source = source.clone();
        changed_source
            .launch
            .recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), "/another-account".into());
        assert!(validate_publication(&receipt, &identity, &proof, &changed_source).is_err());
        let mut changed_proof = proof.clone();
        changed_proof.host_incarnation = crate::model::HostIncarnation::new("another-daemon");
        assert!(validate_publication(&receipt, &identity, &changed_proof, &source).is_err());
        let mut malformed = serde_json::to_value(&receipt).unwrap();
        malformed["source"]["launch"]["session_id"] = serde_json::json!("not-a-uuid");
        let malformed: Receipt = serde_json::from_value(malformed).unwrap();
        assert!(validate_publication(&malformed, &identity, &proof, &malformed.source).is_err());
        let mut contradictory = serde_json::to_value(&receipt).unwrap();
        contradictory["installed"] = serde_json::json!(false);
        contradictory["source"]["native_continuity"] =
            serde_json::to_value(chat::NativeContinuity::RetainedClaudeActor {
                protocol: PROTOCOL,
            })
            .unwrap();
        let contradictory: Receipt = serde_json::from_value(contradictory).unwrap();
        assert!(
            validate_publication(&contradictory, &identity, &proof, &contradictory.source).is_err()
        );
        receipt.snapshot.exited = false;
        assert!(validate_publication(&receipt, &identity, &proof, &source).is_err());
    }
    #[test]
    fn retirement_cross_checks_final_tuple_without_reopening_a_removed_account_home() {
        use std::os::unix::fs::MetadataExt;
        let home = tempfile::tempdir_in("/var/tmp").unwrap();
        let metadata = home.path().metadata().unwrap();
        let scope = crate::account::scope::LaunchAccountScope {
            provider: crate::agent::Provider::Claude,
            canonical_home: home.path().to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
            registered_id: None,
            purge_ids: Vec::new(),
        };
        let actor = RemoteActor::fixture();
        let mut identity = actor.identity();
        identity.record.root = reaped_process();
        identity.record.holder = reaped_process();
        let proof = actor.proof();
        let mut source = super::super::transport::fixture_source();
        source.id = identity.record.terminal;
        source.proof = Some(proof.clone());
        source.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        source.native_continuity =
            chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL };
        let final_receipt = Receipt {
            installed: true,
            identity: identity.clone(),
            proof: proof.clone(),
            source: source.clone(),
            previous_sources: Vec::new(),
            paired_holder: None,
            launch_account_scope: Ok(scope.clone()),
            confirmed_session: None,
            snapshot: Snapshot {
                session_id: source.launch.session_id.clone(),
                exited: true,
                ..Default::default()
            },
        };
        let make = || RetirementReceipt {
            installed: true,
            identity: identity.clone(),
            proof: proof.clone(),
            source: source.clone(),
            previous_sources: Vec::new(),
            launch_account_scope: Ok(scope.clone()),
            confirmed_session: None,
            paired_holder: None,
        };
        drop(home);
        assert!(!scope.canonical_home.exists());
        assert!(validate_retired_publication(&final_receipt, &make()).is_ok());
        let mut contradictory = make();
        contradictory.installed = false;
        assert!(
            validate_retired_publication(&final_receipt, &contradictory).is_err(),
            "the final and retired receipts must seal the same installation state"
        );
        assert!(final_receipt.snapshot.session_id.is_some());
        assert!(
            make().confirmed_session.is_none(),
            "issued identity is not provider confirmation"
        );
        let mut changed = make();
        changed.launch_account_scope.as_mut().unwrap().inode += 1;
        assert!(validate_retired_publication(&final_receipt, &changed).is_err());
        let mut changed = make();
        changed.identity.instance = uuid::Uuid::new_v4();
        assert!(validate_retired_publication(&final_receipt, &changed).is_err());
        let mut changed = make();
        changed.proof.host_incarnation = crate::model::HostIncarnation::new("another-attachment");
        assert!(validate_retired_publication(&final_receipt, &changed).is_err());
        let mut changed = make();
        changed.paired_holder = Some(identity.record.clone());
        assert!(validate_retired_publication(&final_receipt, &changed).is_err());
        let mut changed = make();
        changed.confirmed_session = Some(uuid::Uuid::new_v4().to_string());
        assert!(validate_retired_publication(&final_receipt, &changed).is_err());
        let mut changed = make();
        let mut foreign = source;
        foreign
            .launch
            .recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), "/another-home".into());
        changed.previous_sources.push(foreign);
        assert!(validate_retired_publication(&final_receipt, &changed).is_err());
    }
    #[test]
    fn cold_retirement_validation_uses_exact_saved_record_and_proof_without_inventing_actor_id() {
        let actor = RemoteActor::fixture();
        let identity = actor.identity();
        let proof = actor.proof();
        let mut source = super::super::transport::fixture_source();
        source.id = identity.record.terminal;
        source.proof = Some(proof.clone());
        source.native_continuity =
            chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL };
        let make = || Retirement {
            protocol: PROTOCOL,
            semantic_acknowledged: true,
            receipt: RetirementReceipt {
                installed: true,
                identity: identity.clone(),
                proof: proof.clone(),
                source: source.clone(),
                previous_sources: Vec::new(),
                launch_account_scope: Err("fixture".into()),
                confirmed_session: None,
                paired_holder: None,
            },
        };
        assert!(validate_retirement(&make(), &identity.record, &proof, None).is_ok());
        let mut uninstalled = make();
        uninstalled.receipt.installed = false;
        assert!(
            validate_retirement(&uninstalled, &identity.record, &proof, None).is_err(),
            "uninstalled evidence cannot claim installed continuity"
        );
        uninstalled.receipt.source.native_continuity = chat::NativeContinuity::ReconnectOnly;
        assert!(validate_retirement(&uninstalled, &identity.record, &proof, None).is_ok());
        let mut malformed = serde_json::to_value(&uninstalled).unwrap();
        malformed["source"]["launch"]["session_id"] = serde_json::json!("not-a-uuid");
        let malformed: Retirement = serde_json::from_value(malformed).unwrap();
        assert!(validate_retirement(&malformed, &identity.record, &proof, None).is_err());
        let mut wrong_surface = serde_json::to_value(&uninstalled).unwrap();
        wrong_surface["source"]["owned_codex_listener"] = serde_json::json!({
            "socket": "/fixture/foreign-codex.sock", "allocator": proof,
        });
        let wrong_surface: Retirement = serde_json::from_value(wrong_surface).unwrap();
        assert!(validate_retirement(&wrong_surface, &identity.record, &proof, None).is_err());
        uninstalled.receipt.installed = true;
        assert!(
            validate_retirement(&uninstalled, &identity.record, &proof, None).is_err(),
            "installed evidence cannot accept raw cleanup continuity"
        );
        let mut legacy = serde_json::to_value(make()).unwrap();
        assert!(
            legacy.get("installed").is_none(),
            "installed receipt keeps the legacy wire shape"
        );
        legacy.as_object_mut().unwrap().remove("installed");
        let legacy: Retirement = serde_json::from_value(legacy).unwrap();
        assert!(legacy.receipt.installed);
        validate_retirement(&legacy, &identity.record, &proof, None).unwrap();
        let mut changed = make();
        changed.receipt.identity.record.holder.start += 1;
        assert!(validate_retirement(&changed, &identity.record, &proof, None).is_err());
        let mut changed = make();
        changed.receipt.proof.host_incarnation =
            crate::model::HostIncarnation::new("another-attachment");
        assert!(validate_retirement(&changed, &identity.record, &proof, None).is_err());
        let mut changed = make();
        changed.semantic_acknowledged = false;
        assert!(validate_retirement(&changed, &identity.record, &proof, None).is_err());
        let mut changed = make();
        changed.protocol += 1;
        assert!(validate_retirement(&changed, &identity.record, &proof, None).is_err());
        let mut changed = make();
        changed.receipt.identity.instance = uuid::Uuid::nil();
        assert!(validate_retirement(&changed, &identity.record, &proof, None).is_err());
        let mut expected = identity.clone();
        expected.instance = uuid::Uuid::new_v4();
        assert!(validate_retirement(&make(), &identity.record, &proof, Some(&expected)).is_err());
    }

    #[test]
    fn external_unreaped_helper_requires_joined_retirement_without_consuming_parent_custody() {
        use std::{
            io::{BufRead, BufReader},
            process::{Child, Command, Stdio},
            time::{Duration, Instant},
        };
        const EXACT: &str = "chat::runtime::retained_actor::final_snapshot::tests::external_unreaped_helper_requires_joined_retirement_without_consuming_parent_custody";
        const ROOT: &str = "CAGENTS_EXECUTION_EXIT_READER_FIXTURE";
        if std::env::var_os(ROOT).is_none() {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", EXACT, "--nocapture"])
                .env_clear()
                .env(ROOT, directory.path())
                .env("HOME", directory.path())
                .env("XDG_RUNTIME_DIR", directory.path())
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated retirement reader failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
        let gate = root.join("exit-helper");
        struct Parent {
            child: Child,
            gate: PathBuf,
        }
        impl Drop for Parent {
            fn drop(&mut self) {
                let _ = std::fs::write(&self.gate, b"exit");
                drop(self.child.stdin.take());
                let _ = self.child.wait();
            }
        }
        // The simulated helper is an unreaped grandchild: this reader cannot
        // wait for it. Its actual parent keeps custody until stdin closes.
        let script = r#"import os,sys,time
pid=os.fork()
if pid==0:
 for _ in range(2000):
  if os.path.exists(sys.argv[1]): break
  time.sleep(.005)
 os._exit(73)
print(pid,flush=True)
sys.stdin.readline()
os.waitpid(pid,0)
"#;
        let mut parent = Parent {
            child: Command::new("/usr/bin/python3")
                .args(["-c", script])
                .arg(&gate)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
            gate: gate.clone(),
        };
        let mut line = String::new();
        BufReader::new(parent.child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let helper = registry::Process::read(line.trim().parse().unwrap()).unwrap();
        let actor = RemoteActor::fixture();
        let mut identity = actor.identity();
        identity.record.holder = helper.clone();
        identity.record.root = reaped_process();
        let proof = actor.proof();
        let mut source = super::super::transport::fixture_source();
        source.id = identity.record.terminal;
        source.proof = Some(proof.clone());
        source.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        source.native_continuity =
            chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL };
        std::fs::create_dir_all(registry::directory(Kind::Native).unwrap()).unwrap();
        let snapshot = Snapshot {
            exited: true,
            session_id: source.launch.session_id.clone(),
            ..Default::default()
        };
        save(
            true,
            &identity,
            &proof,
            &source,
            &[],
            None,
            Err("fixture unknown account"),
            source.launch.session_id.as_deref(),
            &snapshot,
        )
        .unwrap();
        let receipt = RetirementReceipt {
            installed: true,
            identity: identity.clone(),
            proof: proof.clone(),
            source: source.clone(),
            previous_sources: Vec::new(),
            launch_account_scope: Err("fixture unknown account".into()),
            confirmed_session: source.launch.session_id.clone(),
            paired_holder: None,
        };
        publish_retirement(receipt.clone()).unwrap();
        assert!(matches!(
            inspect_retirement(&identity.record, &proof).unwrap(),
            RetirementInspection::Running
        ));
        std::fs::write(&gate, b"exit").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(
            execution_exit::observe(&helper).unwrap(),
            execution_exit::Observation::Running
        ) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            helper.is_current(),
            "the external parent must still own its unreaped child's status"
        );
        assert!(matches!(
            inspect_retirement(&identity.record, &proof).unwrap(),
            RetirementInspection::Verified(_)
        ));
        assert!(retirement_proved(&identity, &proof).unwrap());
        assert!(
            retired_actor_receipt(&identity.record, &proof)
                .unwrap()
                .is_some()
        );
        let mut successor = proof.clone();
        successor.host_incarnation = crate::model::HostIncarnation::new("successor");
        assert!(
            retirement_proved_for_record(&identity.record, &successor).is_err(),
            "strict attachment reader must not adopt historical proof"
        );
        assert!(
            matches!(
                inspect_retirement(&identity.record, &successor).unwrap(),
                RetirementInspection::Verified(_)
            ),
            "cold current-lease reader may validate same-boot historical proof"
        );
        let mut other_boot = proof.clone();
        other_boot.boot_id = crate::model::BootId::new("another-boot");
        assert!(inspect_retirement(&identity.record, &other_boot).is_err());
        let mut foreign_boot_record = identity.record.clone();
        foreign_boot_record.boot = other_boot.boot_id.clone();
        assert!(inspect_retirement(&foreign_boot_record, &proof).is_err());
        let mut foreign = identity.clone();
        foreign.instance = uuid::Uuid::new_v4();
        assert!(retirement_proved(&foreign, &proof).is_err());
        identity
            .record
            .create(&registry::directory(Kind::Native).unwrap())
            .unwrap();
        assert!(matches!(
            inspect_retirement(&identity.record, &proof).unwrap(),
            RetirementInspection::ExitedUnproved
        ));
        identity
            .record
            .remove_record(&registry::directory(Kind::Native).unwrap())
            .unwrap();
        let mut unacked = Retirement {
            protocol: PROTOCOL,
            semantic_acknowledged: false,
            receipt: receipt.clone(),
        };
        save_at(
            &retirement_path(&identity.record).unwrap(),
            &serde_json::to_vec(&unacked).unwrap(),
        )
        .unwrap();
        assert!(inspect_retirement(&identity.record, &proof).is_err());
        unacked.semantic_acknowledged = true;
        unacked.receipt.identity.record.holder.start += 1;
        save_at(
            &retirement_path(&identity.record).unwrap(),
            &serde_json::to_vec(&unacked).unwrap(),
        )
        .unwrap();
        assert!(inspect_retirement(&identity.record, &proof).is_err());
        publish_retirement(receipt).unwrap();
        let mut invalid_provider = identity.record.clone();
        invalid_provider.root.pid = -1;
        assert!(inspect_retirement(&invalid_provider, &proof).is_err());
        invalid_provider.root = identity.record.root.clone();
        invalid_provider.root.start = 0;
        assert!(inspect_retirement(&invalid_provider, &proof).is_err());
        let mut live_provider = identity.record.clone();
        live_provider.root = registry::Process::read(std::process::id() as i32).unwrap();
        assert!(matches!(
            inspect_retirement(&live_provider, &proof).unwrap(),
            RetirementInspection::ExitedUnproved
        ));
        std::fs::remove_file(retirement_path(&identity.record).unwrap()).unwrap();
        assert!(matches!(
            inspect_retirement(&identity.record, &proof).unwrap(),
            RetirementInspection::ExitedUnproved
        ));
        assert!(
            helper.is_current(),
            "no receipt reader may reap another parent's child"
        );
        drop(parent);
        assert!(!helper.is_current());
    }

    #[test]
    fn still_running_helper_cannot_become_retired_from_a_read_only_request() {
        let actor = RemoteActor::fixture();
        let identity = actor.identity();
        assert!(identity.record.holder.is_current());
        // This returns before any global registry path is opened. A live
        // helper's PID/start identity is never interpreted as cleanup proof.
        assert!(!retirement_proved_for_record(&identity.record, &actor.proof()).unwrap());
    }

    #[test]
    fn final_publication_is_private_durable_and_bound_to_the_exact_actor() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let path = directory.path().join("final");
        let actor = RemoteActor::fixture();
        let identity = actor.identity();
        let proof = actor.proof();
        let receipt = Receipt {
            installed: true,
            identity: identity.clone(),
            proof: proof.clone(),
            source: super::super::transport::fixture_source(),
            previous_sources: Vec::new(),
            paired_holder: None,
            launch_account_scope: Err("fixture".into()),
            confirmed_session: None,
            snapshot: Snapshot {
                generation: 77,
                title: Some("final".into()),
                ..Default::default()
            },
        };
        save_at(&path, &serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let read: Receipt = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(read.identity, identity);
        assert_eq!(read.proof, proof);
        assert_eq!(read.snapshot, receipt.snapshot);
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            1,
            "atomic publication leaves no startup temporary file"
        );
    }
}
