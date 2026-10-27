//! Saved GUI operations retain a logical destination, never execution authority.
//! Profile capture and comparison perform no transport or filesystem I/O.
use std::{io, path::PathBuf};

use crate::channel_work_ui::journal::{Destination, Profile};

const PROFILE_CHANGED: &str =
    "Open the original device profile before checking or retrying this saved operation.";
const SOCKET_UNAVAILABLE: &str = "The original device profile could not be reconnected.";

/// `device::selected` is the validated, process-owned profile snapshot. It does
/// not reread device settings, resolve a socket or initialize an SSH bridge.
/// None denotes Local; an SSH alias is not a physical device identity.
pub(super) fn capture_profile() -> Option<Profile> {
    crate::device::selected().map(|device| Profile {
        name: device.name.clone(),
        host: device.host.clone(),
        executable: device.executable.clone(),
    })
}

/// Check and Retry must retain their saved logical destination. The caller
/// separately validates the record and the recovered route's Home/host proof.
pub(super) fn require_current_profile(destination: &Destination) -> Result<(), &'static str> {
    require_profile(destination, capture_profile().as_ref())
}

fn require_profile(
    destination: &Destination,
    current: Option<&Profile>,
) -> Result<(), &'static str> {
    if destination.profile.as_ref() == current {
        Ok(())
    } else {
        Err(PROFILE_CHANGED)
    }
}

/// Only an explicit Check may resolve a fresh transport: remote proxy socket
/// names belong to their original process and can be gone after app restart.
/// This returns only a candidate socket, not a verified recovered route. The
/// caller must always handshake and read the original Registry and exact Home
/// before installing its separate recovery proof, even if the host proof is
/// unchanged. The immutable original destination is never updated here.
/// Retry uses the already verified route and must not call this resolver.
pub(super) fn explicit_check_socket(destination: &Destination) -> Result<PathBuf, &'static str> {
    explicit_check_socket_with(
        destination,
        capture_profile().as_ref(),
        crate::terminal::ipc::daemon_socket_path,
    )
}

fn explicit_check_socket_with(
    destination: &Destination,
    current: Option<&Profile>,
    resolve: impl FnOnce() -> io::Result<PathBuf>,
) -> Result<PathBuf, &'static str> {
    require_profile(destination, current)?;
    resolve().map_err(|_| SOCKET_UNAVAILABLE)
}

#[cfg(test)]
mod tests;
