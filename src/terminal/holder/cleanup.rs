//! Durable proof that an exact terminal root/PTY owner was explicitly released.
//! Active close first stops its inspected process family; natural completion
//! requires root exit and PTY drain. Neither case claims ownership of previously
//! disowned processes outside that terminal family. Native provider holders
//! independently require ECHILD for their provider/tool descendants.
//! Recipes remain in holder memory; only authenticated lineage digests persist.
use super::{Record, Recovery};
use crate::{
    daemon::holder_registry::Kind,
    model::{HostSessionProof, TerminalSession},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
};

#[derive(Serialize, Deserialize)]
struct Receipt {
    record: Record,
    sources: Vec<[u8; 32]>,
}

fn digest(source: &TerminalSession) -> Result<[u8; 32], String> {
    use sha2::{Digest, Sha256};
    let mut source = source.clone();
    let proof = source
        .continuity
        .live_proof()
        .ok_or("Terminal close source has no ownership proof")?;
    if !proof.is_complete() {
        return Err("Terminal close source has incomplete ownership proof".into());
    }
    // These are projection/observation fields, not the immutable worker owner.
    source.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
        host_session_id: proof.host_session_id,
        boot_id: proof.boot_id,
        host_incarnation: proof.host_incarnation,
    };
    source.resume_hint = None;
    Ok(Sha256::digest(serde_json::to_vec(&source).map_err(|e| e.to_string())?).into())
}

pub(crate) fn publish(record: &Record, recovery: &Recovery) -> Result<(), String> {
    if record.root.is_current() {
        return Err("Terminal root has not been reaped".into());
    }
    recovery.validate(&record.boot, record.terminal)?;
    let receipt = Receipt {
        record: record.clone(),
        sources: recovery
            .previous_sources
            .iter()
            .chain(std::iter::once(&recovery.source))
            .map(digest)
            .collect::<Result<_, _>>()?,
    };
    let directory = super::directory()?;
    let destination = directory.join(format!(
        "{}-{}-{}.closed",
        record.terminal, record.holder.pid, record.holder.start
    ));
    let temporary = directory.join(format!(".cleanup-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut file, &receipt).map_err(|e| e.to_string())?;
        file.flush()
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string())?;
        fs::rename(&temporary, &destination).map_err(|e| e.to_string())?;
        fs::File::open(&directory)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub(crate) fn authorize(proof: &HostSessionProof) -> Result<(), String> {
    let paths = crate::daemon::RuntimePaths::discover().map_err(|e| e.to_string())?;
    if crate::daemon::lease_owner(&paths.lock, proof).map_err(|e| e.to_string())?
        != std::process::id()
    {
        return Err("Terminal cleanup caller does not own the daemon lease".into());
    }
    Ok(())
}

/// Absence is not a receipt. A new daemon must present an exact authenticated
/// source alias and own this installation's current exclusive lease.
pub(crate) fn confirmed(
    source: &TerminalSession,
    expected: &Record,
    proof: &HostSessionProof,
) -> Result<bool, String> {
    if source.id != expected.terminal {
        return Ok(false);
    }
    confirmed_with_role(source, expected, proof, None)
}

/// The worker retains this exact-record capability from live authenticated
/// adoption; public source descriptors alone cannot authorize another role.
pub(crate) fn confirmed_with_role(
    source: &TerminalSession,
    expected: &Record,
    proof: &HostSessionProof,
    role: Option<&crate::daemon::holder_registry::OriginalRole>,
) -> Result<bool, String> {
    if source.id != expected.terminal {
        return Ok(false);
    }
    confirmed_receipt(expected, proof, Some(digest(source)?), role)
}

/// Used only when another authenticated owner (the native holder) retained this
/// exact private Record. Public terminal IDs or launch recipes are insufficient.
pub(crate) fn confirmed_owner(expected: &Record, proof: &HostSessionProof) -> Result<bool, String> {
    confirmed_receipt(expected, proof, None, None)
}

/// The parent native holder authenticated this exact paired Record. Its
/// retained role capability remains necessary even after the terminal exits.
pub(crate) fn confirmed_owner_with_role(
    expected: &Record,
    proof: &HostSessionProof,
    role: Option<&crate::daemon::holder_registry::OriginalRole>,
) -> Result<bool, String> {
    confirmed_receipt(expected, proof, None, role)
}

fn confirmed_receipt(
    expected: &Record,
    proof: &HostSessionProof,
    source: Option<[u8; 32]>,
    role: Option<&crate::daemon::holder_registry::OriginalRole>,
) -> Result<bool, String> {
    authorize(proof)?;
    let directory = super::directory()?;
    if crate::daemon::holder_registry::recovery_candidate(
        &super::socket(&directory, expected.terminal),
        Kind::Terminal,
    )?
    .is_some()
    {
        return Ok(false);
    }
    let path = directory.join(format!(
        "{}-{}-{}.closed",
        expected.terminal, expected.holder.pid, expected.holder.start
    ));
    let file = match crate::secure_file::open_existing_private_file(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(256 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 256 * 1024 {
        return Err("Terminal close receipt is oversized".into());
    }
    let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok(receipt.record == *expected
        && receipt.record.kind == Kind::Terminal
        && matches!(receipt.record.version, 4 | 5)
        && (receipt.record.application == crate::instance::APPLICATION_ID
            || role.is_some_and(|role| role.matches(&receipt.record, proof)))
        && receipt.record.boot == proof.boot_id
        && holder_exited(&receipt.record.holder)?
        && !receipt.record.root.is_current()
        && source.is_none_or(|digest| receipt.sources.contains(&digest)))
}

/// An orphan holder may remain a zombie until its new parent reaps it. Its
/// exact identity still exists in /proc, but it can no longer own the socket.
/// Keep this distinct from the root's stronger requirement to have been reaped.
fn holder_exited(holder: &crate::daemon::holder_registry::Process) -> Result<bool, String> {
    let stat = match fs::read(format!("/proc/{}/stat", holder.pid)) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.to_string()),
    };
    let (_, start) = crate::model::agent_recovery::process_identity(&stat)
        .ok_or("Invalid terminal holder process identity")?;
    let state = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .and_then(|end| {
            stat[end + 1..]
                .iter()
                .find(|byte| !byte.is_ascii_whitespace())
        })
        .copied()
        .ok_or("Invalid terminal holder process state")?;
    Ok(start != holder.start || matches!(state, b'Z' | b'X'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proved_receipts_accept_supported_versions_and_preserve_exact_ownership_checks() {
        use crate::daemon::{HostLease, RuntimePaths, holder_registry::Process};
        use std::{
            fs::File,
            os::unix::fs::PermissionsExt,
            process::{Child, Command, Stdio},
            time::{Duration, Instant},
        };
        const CHILD: &str = "CAGENTS_CLEANUP_RECEIPT_VERSION_CHILD";
        const NAME: &str = "terminal::holder::cleanup::tests::proved_receipts_accept_supported_versions_and_preserve_exact_ownership_checks";
        if std::env::var_os(CHILD).is_none() {
            let private = tempfile::Builder::new().prefix("cr-").tempdir().unwrap();
            fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let log = File::create(private.path().join("child.log")).unwrap();
            // Inherit caller authority and HOME unchanged. Only the private
            // fixture runtime and TMPDIR differ in this exact self-child.
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .env("XDG_RUNTIME_DIR", private.path())
                .env("TMPDIR", private.path())
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    // Failure-only cleanup uses this actual owned Child.
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "receipt child timed out; evidence: {}",
                        private.keep().display()
                    );
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            let output = fs::read_to_string(private.path().join("child.log")).unwrap();
            print!("{output}");
            assert!(
                status.success(),
                "receipt child failed ({status}); evidence: {}",
                private.keep().display()
            );
            assert!(
                output.contains("1 passed; 0 failed"),
                "exact child did not execute one test"
            );
            return;
        }

        // Hold two real processes alive until their identities have been
        // captured. Normal release and wait establish exited/reaped controls;
        // this does not pretend these fixture children are actual PTY holders.
        struct Held(Child, Process);
        impl Held {
            fn start() -> Self {
                let child = Command::new("/bin/sh")
                    .args(["-c", "read release"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                let identity = Process::read(child.id() as i32).unwrap();
                Self(child, identity)
            }
            fn finish(&mut self) {
                self.0
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(b"release\n")
                    .unwrap();
                assert!(self.0.wait().unwrap().success());
                assert!(
                    !self.1.is_current(),
                    "original child must actually be reaped"
                );
            }
        }
        impl Drop for Held {
            fn drop(&mut self) {
                if !matches!(self.0.try_wait(), Ok(Some(_))) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }
        let proof = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("receipt-version-owner"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: crate::model::HostIncarnation::new(uuid::Uuid::new_v4().to_string()),
        };
        let paths = RuntimePaths::discover().unwrap();
        let lease = HostLease::acquire(&paths, &proof).unwrap();
        let directory = super::super::directory().unwrap();
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let source = TerminalSession {
            id: crate::model::TerminalId::new(),
            launch: crate::model::LaunchRecipe::shell("/".into()).unwrap(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            },
        };
        let recovery = Recovery {
            source: source.clone(),
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            options: crate::terminal::TerminalSpawnOptions::default(),
            execution_environment: None,
            execution_recipe: None,
        };
        let mut root = Held::start();
        let mut holder = Held::start();
        let mut record = Record {
            version: 5,
            build: None,
            kind: Kind::Terminal,
            application: crate::instance::APPLICATION_ID.into(),
            boot: proof.boot_id.clone(),
            terminal: source.id,
            holder: holder.1.clone(),
            root: root.1.clone(),
        };
        assert!(
            publish(&record, &recovery).is_err(),
            "a live root cannot produce a receipt"
        );
        root.finish();
        publish(&record, &recovery).unwrap();
        assert!(
            !confirmed(&source, &record, &proof).unwrap(),
            "a receipt cannot certify a live holder"
        );
        holder.finish();
        for (version, accepted) in [(4, true), (5, true), (3, false), (6, false)] {
            record.version = version;
            publish(&record, &recovery).unwrap();
            assert_eq!(
                confirmed(&source, &record, &proof).unwrap(),
                accepted,
                "actual written receipt version {version}"
            );
        }
        record.version = 5;
        publish(&record, &recovery).unwrap();
        let mut wrong_source = source.clone();
        wrong_source
            .launch
            .arguments
            .push("different-lineage".into());
        assert!(!confirmed(&wrong_source, &record, &proof).unwrap());
        let mut different = record.clone();
        different.version = 4;
        assert!(
            !confirmed(&source, &different, &proof).unwrap(),
            "supported versions are not interchangeable exact records"
        );
        different = record.clone();
        different.holder.start += 1;
        assert!(!confirmed(&source, &different, &proof).unwrap());
        different = record.clone();
        different.application.push_str(".other");
        assert!(!confirmed(&source, &different, &proof).unwrap());
        let mut wrong_proof = proof.clone();
        wrong_proof.host_incarnation = crate::model::HostIncarnation::new("not-the-lease-owner");
        assert!(confirmed(&source, &record, &wrong_proof).is_err());
        drop(lease);
        assert!(
            confirmed(&source, &record, &proof).is_err(),
            "an unlocked receipt alone is not authority"
        );
    }

    #[test]
    fn exited_holder_is_proven_before_its_parent_reaps_it() {
        use crate::daemon::holder_registry::Process;
        let current = Process::read(std::process::id() as i32).unwrap();
        assert!(!holder_exited(&current).unwrap());
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 0.05"])
            .spawn()
            .unwrap();
        let holder = Process::read(child.id() as i32).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !holder_exited(&holder).unwrap() {
            assert!(std::time::Instant::now() < deadline, "holder did not exit");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            holder.is_current(),
            "fixture must remain an unreaped zombie"
        );
        child.wait().unwrap();
        assert!(holder_exited(&holder).unwrap());
    }
}
