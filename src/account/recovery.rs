//! Metadata discovery for managed homes whose registry rows were lost.
//!
//! A candidate is a hint for the daemon's reserved recovery operation, never
//! permission to read provider files, launch a provider or register a login.
//! Discovery neither creates directories nor changes their permissions.

use std::{fs, io, os::unix::fs::MetadataExt, path::Path};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Account, home::Homes, registry::Registry};
use crate::agent::Provider;

#[cfg(feature = "host")]
mod restore;
#[cfg(feature = "host")]
pub(crate) use restore::restore_reserved;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Candidate {
    pub provider: Provider,
    pub key: Uuid,
}

/// A metadata-only snapshot supplied by the selected device. An error keeps
/// the last successful hints visible; it never confirms a login or absence.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Discovery {
    pub candidates: Vec<Candidate>,
    pub problem: Option<String>,
}

impl Discovery {
    pub(crate) fn read(homes: Option<&Homes>, registry: &Registry) -> Self {
        match homes.map(|homes| discover(homes, registry)).transpose() {
            Ok(candidates) => Self {
                candidates: candidates.unwrap_or_default(),
                problem: None,
            },
            Err(error) => Self {
                candidates: Vec::new(),
                problem: Some(error.to_string()),
            },
        }
    }
}

/// Only `Missing` proves absence. An empty discovery result, a represented
/// home, or a failed inspection must not become an "account removed" notice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HomeStatus {
    Missing,
    Registered,
    Pending,
    Represented,
    /// Two managed provider directories claim the same registry-wide key.
    Ambiguous,
    Available(Candidate),
}

fn supported_registry(registry: &Registry) -> io::Result<()> {
    if registry.is_newer() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "The accounts were saved by a newer cagents; recovery was not attempted",
        ));
    }
    Ok(())
}

fn owned_directory(path: &Path) -> io::Result<fs::Metadata> {
    crate::secure_file::reject_symlink_components(path)?;
    crate::secure_file::validate_owned_directory(path)?;
    let metadata = fs::symlink_metadata(path)?;
    // Inspect the final observation too: a pathname can change between the
    // common ownership check and this metadata-only hint being assembled.
    // SAFETY: geteuid has no preconditions and does not modify process state.
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The managed account path is not an owned real directory",
        ));
    }
    Ok(metadata)
}

fn same_directory(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// Inspect one daemon-derived path. Call again at admission and checked
/// commit; the host reservation must additionally retain its physical pin.
pub(crate) fn inspect(
    homes: &Homes,
    registry: &Registry,
    provider: Provider,
    key: Uuid,
) -> io::Result<HomeStatus> {
    supported_registry(registry)?;
    if !Account::PROVIDERS.contains(&provider) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "This provider manages its own account homes",
        ));
    }
    // UUIDs are registry-wide identities, even across provider directories.
    if registry.find(key).is_some() {
        return Ok(HomeStatus::Registered);
    }
    if registry.pending().iter().any(|pending| pending.key == key) {
        return Ok(HomeStatus::Pending);
    }
    if registry.contains_managed_home(homes, provider, key) {
        return Ok(HomeStatus::Represented);
    }
    let path = homes.path(provider, key);
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Account provider directory is unavailable",
        )
    })?;
    let root = directory.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Managed account directory is unavailable",
        )
    })?;
    let metadata = match (|| {
        owned_directory(root)?;
        owned_directory(directory)?;
        owned_directory(&path)
    })() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HomeStatus::Missing),
        Err(error) => return Err(error),
    };
    for other in Account::PROVIDERS {
        if other == provider {
            continue;
        }
        let other_path = homes.path(other, key);
        let other_directory = other_path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Account provider directory is unavailable",
            )
        })?;
        match owned_directory(other_directory).and_then(|_| owned_directory(&other_path)) {
            Ok(_) => return Ok(HomeStatus::Ambiguous),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    for linked in registry
        .registered()
        .iter()
        .filter_map(|row| row.linked_home.as_ref())
    {
        // A linked registration may use another spelling of this directory.
        // Follow that explicitly registered alias for metadata only, never
        // provider files, and never adopt the same physical home twice.
        match fs::metadata(linked) {
            Ok(linked_metadata) if same_directory(&metadata, &linked_metadata) => {
                return Ok(HomeStatus::Represented);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(HomeStatus::Available(Candidate { provider, key }))
}

/// Enumerate canonical UUID directories in this installation's managed
/// namespace. Unknown, parked and unsafe entries remain untouched.
pub(crate) fn discover(homes: &Homes, registry: &Registry) -> io::Result<Vec<Candidate>> {
    supported_registry(registry)?;
    let mut candidates = Vec::new();
    for provider in Account::PROVIDERS {
        let example = homes.path(provider, Uuid::nil());
        let directory = example.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Account provider directory is unavailable",
            )
        })?;
        let root = directory.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Managed account directory is unavailable",
            )
        })?;
        match owned_directory(root).and_then(|_| owned_directory(directory)) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(key) = name.to_str().and_then(|name| Uuid::parse_str(name).ok()) else {
                continue;
            };
            if entry.path() != homes.path(provider, key) || !entry.file_type()?.is_dir() {
                continue;
            }
            if let HomeStatus::Available(candidate) = inspect(homes, registry, provider, key)? {
                candidates.push(candidate);
            }
        }
    }
    candidates.sort_by_key(|candidate| (candidate.provider.executable(), candidate.key));
    Ok(candidates)
}

#[cfg(test)]
mod tests;
