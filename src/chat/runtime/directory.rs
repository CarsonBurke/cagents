//! A relocated spelling is operational authority only after the exact owned
//! provider process proves that it still occupies the approved directory.
use super::*;
use crate::{agent_execution::DirectoryIdentity, daemon::holder_registry::Process};
use std::{fs::File, os::unix::fs::OpenOptionsExt, path::Path};

#[derive(Clone, Debug)]
pub(crate) struct DirectoryProof {
    pub(crate) process: Process,
    pub(crate) identity: DirectoryIdentity,
    pub(crate) directory: Arc<File>,
    pub(crate) path: std::path::PathBuf,
}

impl DirectoryProof {
    /// The caller must have obtained this exact process from an authenticated
    /// original holder or retained child, never by rediscovering a numeric ID.
    pub(crate) fn capture_owned_process(process: &Process, candidate: &Path, approved: Option<DirectoryIdentity>) -> Result<Self, String> {
        prove(process, candidate, approved)
    }
}

impl LocalHandle {
    /// Call outside HostState: this is serialized with the owning actor and
    /// may wait behind provider work. An unavailable proof never authorizes a
    /// guessed relocation or a replacement provider.
    pub(crate) fn capture_execution_directory(&self, candidate: &Path) -> Result<DirectoryProof, String> {
        if self.is_completed() || self.0.draining.load(Ordering::Acquire) || self.0.stop.load(Ordering::Acquire) {
            return Err("The original provider actor has stopped; its directory cannot be relocated".into());
        }
        let (proof, response) = crossbeam_channel::bounded(1);
        let (reply, _) = crossbeam_channel::bounded(1);
        self.0.commands.try_send((ActorCommand::CaptureExecutionDirectory {
            directory: candidate.to_owned(),
            proof,
        }, reply)).map_err(|error| format!("Provider directory verification queue unavailable: {error}"))?;
        self.wake();
        response.recv_timeout(Duration::from_secs(2))
            .map_err(|_| "The provider did not return a directory ownership proof; retry Locate after it is ready".to_owned())?
    }
}

fn owned_process(actor: &Actor) -> Result<Process, String> {
    if let Some(child) = &actor.child {
        // The retained pidfd is tied to this Child, including after numeric
        // PID reuse. An exited process cannot establish a current cwd proof.
        let mut descriptor = libc::pollfd { fd: child.identity.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if ready != 0 {
            return Err("The original provider process has exited or cannot be verified; its directory was not relocated".into());
        }
        return Process::read(child.child.id() as i32).map_err(|error| error.to_string());
    }
    actor.native_peer.clone()
        .filter(Process::is_current)
        .ok_or_else(|| "The native provider has no authenticated live process identity yet; retry Locate after it is ready".into())
}

fn cwd(process: &Process) -> Result<File, String> {
    if !process.is_current() {
        return Err("The provider process incarnation changed; its directory was not relocated".into());
    }
    std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(format!("/proc/{}/cwd", process.pid))
        .map_err(|error| format!("Cannot verify the original provider working directory: {error}"))
}

fn prove(process: &Process, candidate: &Path, approved: Option<DirectoryIdentity>) -> Result<DirectoryProof, String> {
    let original = cwd(process)?;
    let identity = DirectoryIdentity::from_metadata(&original.metadata().map_err(|error| error.to_string())?);
    if approved.is_some_and(|approved| approved != identity) {
        return Err("The provider no longer occupies its approved directory; close it before locating a replacement folder".into());
    }
    let candidate_context = super::super::protocol::ExecutionContext::capture(candidate)?;
    if candidate_context.directory_identity()? != Some(identity) {
        return Err(format!("{} is a different directory from the original provider process; close this session and start a new one for the replacement folder", candidate.display()));
    }
    let after = cwd(process)?;
    if DirectoryIdentity::from_metadata(&after.metadata().map_err(|error| error.to_string())?) != identity
        || !process.is_current()
    {
        return Err("The provider working directory changed during Locate; retry after it is stable".into());
    }
    Ok(DirectoryProof {
        process: process.clone(), identity,
        directory: candidate_context.directory.expect("capture always retains the opened directory"),
        path: candidate.to_owned(),
    })
}

pub(super) fn capture(actor: &mut Actor, candidate: &Path) -> Result<DirectoryProof, String> {
    let process = owned_process(actor)?;
    let approved = actor.launch_context.as_ref().map(|context| context.directory_identity()).transpose()?.flatten();
    let proof = prove(&process, candidate, approved)?;
    if let Some(context) = &actor.launch_context {
        context.pin_owned_directory(proof.directory.clone())?;
    } else {
        // A legacy actor may acquire a pin only from its positively identified
        // live cwd. This changes no issued recipe or operational path.
        actor.launch_context = Some(Arc::new(super::super::protocol::ExecutionContext::new(Some(proof.directory.clone()))));
    }
    Ok(proof)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Running(std::process::Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn owned_cwd_proof_accepts_same_inode_move_and_refuses_reused_old_path() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        let moved = root.path().join("moved");
        std::fs::create_dir(&original).unwrap();
        let expected = DirectoryIdentity::from_metadata(&std::fs::metadata(&original).unwrap());
        let child = Running(std::process::Command::new("/bin/sleep").arg("30").current_dir(&original).spawn().unwrap());
        let process = Process::read(child.0.id() as i32).unwrap();
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        let proof = prove(&process, &moved, Some(expected)).unwrap();
        assert_eq!(proof.identity, expected);
        assert_eq!(DirectoryIdentity::from_metadata(&proof.directory.metadata().unwrap()), expected);
        assert!(prove(&process, &original, Some(expected)).unwrap_err().contains("different directory"));
        assert!(original.is_dir(), "refusal preserves the replacement folder");
        let mut reused_process = process;
        reused_process.start = reused_process.start.saturating_add(1);
        assert!(prove(&reused_process, &moved, Some(expected)).unwrap_err().contains("incarnation changed"));
    }

    #[test]
    fn an_unproved_legacy_process_can_only_pin_its_actual_owned_cwd() {
        let root = tempfile::tempdir().unwrap();
        let actual = root.path().join("actual");
        let replacement = root.path().join("replacement");
        std::fs::create_dir(&actual).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        let child = Running(std::process::Command::new("/bin/sleep").arg("30").current_dir(&actual).spawn().unwrap());
        let process = Process::read(child.0.id() as i32).unwrap();
        assert!(prove(&process, &actual, None).is_ok());
        assert!(prove(&process, &replacement, None).is_err());
        let wrong = DirectoryIdentity::from_metadata(&std::fs::metadata(&replacement).unwrap());
        assert!(prove(&process, &actual, Some(wrong)).unwrap_err().contains("approved directory"));
    }
}
