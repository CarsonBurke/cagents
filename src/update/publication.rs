//! The replacement daemon's custody of a staged installation.
use super::*;

/// Owned by the exact standby before it accepts the source's terminal lease.
pub struct LocalPublication {
    switch: crate::install_guard::CurrentExchange,
    candidate: InstallLayout,
}

impl LocalPublication {
    pub(crate) fn prepare(expected: &Path) -> Result<Self, UpdateError> {
        let candidate = locate(&crate::process::current_executable()?)?;
        let selection = installed::Selection::capture(&candidate)?;
        if selection.previous_link() != Some(expected) || candidate.version != crate::version() {
            return Err(invalid("The standby's staged source no longer matches"));
        }
        let switch = selection
            .prepare(&candidate)?
            .ok_or_else(|| invalid("The standby's local build is already selected"))?;
        Ok(Self { switch, candidate })
    }

    pub fn publish(self) -> Result<(), UpdateError> {
        installed::complete_selection(Some(self.switch), &self.candidate)
    }
}
