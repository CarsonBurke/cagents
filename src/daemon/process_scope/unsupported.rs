//! The backend for a system with no OOM selection unit to give, macOS.
//!
//! Every enrollment keeps the existing launch, as the Linux backend does
//! without an OS user manager, so no caller distinguishes the two. What is
//! lost is only what a scope adds: a memory kill ends a provider together
//! with its commands, and no command is reported as stopped for memory.
//! Ending the commands a force-stopped provider left behind belongs to
//! process ownership, not to this backend.
use std::{io, os::unix::net::UnixStream};

use super::{CommandStop, Killer};

/// A provider's OOM selection unit, which this system never gives: no value
/// exists, so each method is unreachable.
pub(crate) enum ProviderScope {}

impl ProviderScope {
    pub(crate) fn keep_commands_apart(
        &mut self,
        _root: u32,
        _kept: Vec<u32>,
        _wake: Option<UnixStream>,
    ) -> io::Result<()> {
        match *self {}
    }

    pub(crate) fn provider_killer(&self) -> Option<Killer> {
        match *self {}
    }

    pub(crate) fn reports_activity(&self) {
        match *self {}
    }

    pub(crate) fn poke(&self) {
        match *self {}
    }

    pub(crate) fn take_command_stops(&self) -> Vec<CommandStop> {
        match *self {}
    }

    pub(crate) fn keeps_commands_apart(&self) -> bool {
        match *self {}
    }

    pub(crate) fn end(&mut self) -> io::Result<()> {
        match *self {}
    }
}

pub(crate) fn enroll_current_process() -> io::Result<()> {
    Ok(())
}

pub(crate) fn enroll_dividing_holder() -> io::Result<Option<ProviderScope>> {
    Ok(None)
}

/// `wake` is dropped: no command is ever stopped for memory here.
pub(crate) fn enroll_provider(
    _pid: u32,
    _wake: Option<UnixStream>,
) -> io::Result<Option<ProviderScope>> {
    Ok(None)
}

/// No scopes exist to collect.
pub fn collect_empty_scopes() -> io::Result<usize> {
    Ok(0)
}

/// A provider always shares its commands' memory fate here, which is no
/// degradation to report.
pub(crate) fn warn_if_shared(_holder: u32, _root: u32) {}

pub(crate) fn warn_if_bootstrap_shared(_root: u32) {}

#[cfg(test)]
pub(crate) mod tests {
    use std::{io, os::unix::process::ExitStatusExt, process::ExitStatus};

    /// Isolation is never available, so it is reported unqualified, as the
    /// Linux backend reports it without an OS user manager.
    pub(crate) fn assert_separate(_holder: u32, _root: u32) {
        eprintln!("scope isolation unqualified: no OOM selection unit on this system");
    }

    pub(crate) fn assert_commands_apart(_root: u32) {
        eprintln!("command cgroups unqualified: no OOM selection unit on this system");
    }

    /// An enrollment failure as the Linux backend reports a refused request,
    /// for owners that test the launch path an enrollment error takes.
    pub(crate) fn rejected_scope_request() -> io::Result<()> {
        Err(io::Error::other(format!(
            "user scope request failed: {}",
            ExitStatus::from_raw(1 << 8)
        )))
    }

    #[test]
    fn every_enrollment_keeps_the_existing_launch() {
        super::enroll_current_process().unwrap();
        assert!(super::enroll_dividing_holder().unwrap().is_none());
        assert!(
            super::enroll_provider(std::process::id(), None)
                .unwrap()
                .is_none()
        );
        assert_eq!(super::collect_empty_scopes().unwrap(), 0);
    }

    #[test]
    fn a_rejected_request_reads_as_the_linux_refusal() {
        let error = rejected_scope_request().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "user scope request failed: exit status: 1"
        );
    }
}
