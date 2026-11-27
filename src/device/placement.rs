//! Which device each of this computer's windows last showed.
//!
//! Switching a window to a device replaces its process with one connected to
//! the device. Without a record of that, `cagents ui` would reopen the window
//! on this computer after a restart, though its projects and terminals are on
//! the device. A window is named by the slot it has on this computer, such as
//! `primary`; on device `NAME` it is that slot scoped to the device, so
//! switching back and forth returns to the same saved window on each side.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

/// The reader refuses a larger file, so the writer never produces one.
const MAX_WINDOWS: usize = 1024;
const MAX_BYTES: usize = 64 * 1024;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Placements {
    /// This computer's window slot, and the device it shows.
    #[serde(default)]
    windows: BTreeMap<String, String>,
}

fn path() -> io::Result<PathBuf> {
    crate::instance::project_dirs()
        .map(|dirs| dirs.data_dir().join("window-devices.json"))
        .ok_or_else(|| io::Error::other("data directory unavailable"))
}

/// The device window `slot` showed when it last switched, if it is on one.
pub fn device(slot: &str) -> io::Result<Option<String>> {
    Ok(read(&path()?)?.windows.remove(slot))
}

/// Records that window `slot` now shows `device`, or this computer.
pub fn record(slot: &str, device: Option<&str>) -> io::Result<()> {
    record_at(&path()?, slot, device)
}

/// The slot on this computer of the window that shows `device` as
/// `scoped_slot`, if a switch put it there.
pub fn origin(device: &str, scoped_slot: &str) -> io::Result<Option<String>> {
    Ok(origin_in(&read(&path()?)?, device, scoped_slot))
}

fn origin_in(placements: &Placements, device: &str, scoped_slot: &str) -> Option<String> {
    placements
        .windows
        .iter()
        .find(|(slot, placed)| {
            *placed == device && super::scoped_window_slot(device, slot) == scoped_slot
        })
        .map(|(slot, _)| slot.clone())
}

fn validate_slot(slot: &str) -> io::Result<()> {
    crate::instance::validate_window_slot(slot)?;
    if slot.starts_with("device-") {
        return Err(io::Error::other(
            "a device window has no slot on this computer",
        ));
    }
    Ok(())
}

fn read(path: &Path) -> io::Result<Placements> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let bytes = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .and_then(|file| {
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "window-devices.json must be a regular file",
                ));
            }
            let mut bytes = Vec::new();
            file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
            Ok(bytes)
        });
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Placements::default()),
        Err(error) => return Err(error),
    };
    if bytes.len() > MAX_BYTES {
        return Err(io::Error::other("window-devices.json exceeds 64 KiB"));
    }
    let placements: Placements = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if placements.windows.len() > MAX_WINDOWS {
        return Err(io::Error::other(
            "window-devices.json lists too many windows",
        ));
    }
    for (slot, device) in &placements.windows {
        validate_slot(slot)?;
        super::validate_name(device)?;
    }
    Ok(placements)
}

fn record_at(path: &Path, slot: &str, device: Option<&str>) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    validate_slot(slot)?;
    let device = device.filter(|device| *device != "local");
    if let Some(device) = device {
        super::validate_name(device)?;
    }
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("window-devices.json has no parent directory"))?;
    crate::secure_file::prepare_private_directory(directory)?;
    // Windows switching at the same time each keep their own record.
    let _lock =
        crate::instance::owned_lock(&directory.join("window-devices.json.lock"), libc::LOCK_EX)?;
    // A damaged record only costs where windows reopen; start it again
    // rather than refuse every switch.
    let mut placements = read(path).unwrap_or_else(|error| {
        tracing::warn!(%error, "replacing an unreadable window device record");
        Placements::default()
    });
    let changed = match device {
        Some(device) => {
            placements
                .windows
                .insert(slot.to_owned(), device.to_owned())
                != Some(device.into())
        }
        None => placements.windows.remove(slot).is_some(),
    };
    if !changed {
        return Ok(());
    }
    if placements.windows.len() > MAX_WINDOWS {
        return Err(io::Error::other(
            "cagents remembers the device of at most 1024 windows",
        ));
    }
    let mut bytes = serde_json::to_vec_pretty(&placements).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let temporary = directory.join(format!(
        ".window-devices-{}.json",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(directory)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_keeps_its_device_until_it_switches_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data/window-devices.json");
        assert!(read(&path).unwrap().windows.is_empty());

        record_at(&path, "primary", Some("build-host")).unwrap();
        record_at(&path, "window-2", Some("laptop")).unwrap();
        let placements = read(&path).unwrap();
        assert_eq!(placements.windows["primary"], "build-host");
        // The device window finds which window of this computer it is.
        let scoped = super::super::scoped_window_slot("build-host", "primary");
        assert_eq!(
            origin_in(&placements, "build-host", &scoped).as_deref(),
            Some("primary")
        );
        assert_eq!(origin_in(&placements, "laptop", &scoped), None);

        record_at(&path, "primary", Some("local")).unwrap();
        let placements = read(&path).unwrap();
        assert!(!placements.windows.contains_key("primary"));
        assert_eq!(placements.windows["window-2"], "laptop");
    }

    #[test]
    fn device_window_slots_and_damaged_records_are_refused_or_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("window-devices.json");
        let scoped = super::super::scoped_window_slot("build-host", "primary");
        assert!(record_at(&path, &scoped, Some("build-host")).is_err());
        assert!(record_at(&path, "primary", Some("bad name")).is_err());

        fs::write(&path, b"not json").unwrap();
        assert!(read(&path).is_err());
        record_at(&path, "primary", Some("build-host")).unwrap();
        assert_eq!(read(&path).unwrap().windows["primary"], "build-host");
    }
}
