//! The replacement daemon's custody of a staged installation.
use super::*;

/// Owned by the exact standby before it accepts the source's terminal lease.
pub struct LocalPublication {
    switch: crate::install_guard::CurrentExchange,
    candidate: InstallLayout,
    #[cfg(target_os = "macos")]
    stamp: installed::Stamp,
}

impl LocalPublication {
    pub(crate) fn prepare(expected: &Path) -> Result<Self, UpdateError> {
        let candidate = locate(&crate::process::current_executable()?)?;
        if candidate.version != crate::version() {
            return Err(invalid("The standby's staged source no longer matches"));
        }
        #[cfg(target_os = "linux")]
        let switch = {
            let selection = installed::Selection::capture(&candidate)?;
            if selection.previous_link() != Some(expected) {
                return Err(invalid("The standby's staged source no longer matches"));
            }
            selection
                .prepare(&candidate)?
                .ok_or_else(|| invalid("The standby's local build is already selected"))?
        };
        #[cfg(target_os = "macos")]
        let switch = {
            if served_version_root(&candidate)? == candidate.version_root {
                return Err(invalid("The standby's build is already selected"));
            }
            let link = Path::new("versions").join(&candidate.version);
            crate::install_guard::CurrentExchange::prepare(&candidate.prefix, &link, expected)?
        };
        #[cfg(target_os = "macos")]
        let stamp = installed::stamp(&candidate.version_root)?;
        Ok(Self {
            switch,
            candidate,
            #[cfg(target_os = "macos")]
            stamp,
        })
    }

    pub fn publish(self) -> Result<(), UpdateError> {
        #[cfg(target_os = "macos")]
        if installed::stamp(&self.candidate.version_root)? != self.stamp {
            return Err(invalid("The replacement build changed after it was parked"));
        }
        installed::complete_selection(Some(self.switch), &self.candidate)
    }
}
