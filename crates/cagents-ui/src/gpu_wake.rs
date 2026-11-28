use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use cosmic::iced::wgpu::Backends;

/// RADV opens each AMD render node during Vulkan adapter discovery, even if
/// iced will use a different adapter. Opening a suspended amdgpu render node
/// can wait seconds for runtime resume. Give that resume a head start while
/// the daemon and workspace are starting.
pub fn start() {
    let iced_backend = std::env::var("ICED_BACKEND").ok();
    if !prefers_vulkan(iced_backend.as_deref(), Backends::from_env())
        || [
            "VK_DRIVER_FILES",
            "VK_ICD_FILENAMES",
            "VK_ADD_DRIVER_FILES",
            "VK_LOADER_DRIVERS_SELECT",
            "VK_LOADER_DRIVERS_DISABLE",
        ]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return;
    }

    let worker = std::thread::Builder::new()
        .name("gpu-wake".into())
        .spawn(|| {
            for node in suspended_amdgpu_nodes(Path::new("/sys/class/drm"), Path::new("/dev/dri")) {
                let started = Instant::now();
                // Closing gives the device back to its normal runtime power policy.
                let result = fs::File::open(&node).map(drop);
                tracing::debug!(
                    node = %node.display(),
                    elapsed_ms = started.elapsed().as_millis(),
                    error = result.err().map(tracing::field::display),
                    "Finished opening a suspended GPU render node"
                );
            }
        });
    if let Err(error) = worker {
        tracing::debug!(%error, "Could not start GPU resume");
    }
}

fn prefers_vulkan(iced_backend: Option<&str>, wgpu_backends: Option<Backends>) -> bool {
    let first = iced_backend.and_then(|candidates| {
        candidates
            .split(',')
            .find(|candidate| !candidate.is_empty())
    });
    first.is_none_or(|backend| backend == "wgpu")
        && wgpu_backends.is_none_or(|backends| backends.contains(Backends::VULKAN))
}

fn suspended_amdgpu_nodes(sysfs: &Path, devices: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(sysfs) else {
        return Vec::new();
    };

    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let index = name.to_str()?.strip_prefix("renderD")?;
            if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }

            let device = entry.path().join("device");
            let driver = fs::read_link(device.join("driver")).ok()?;
            if driver.file_name()? != "amdgpu" {
                return None;
            }

            let status = fs::read_to_string(device.join("power/runtime_status")).ok()?;
            (status.trim() == "suspended").then(|| devices.join(name))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn resume_follows_the_preferred_renderer_and_wgpu_backend() {
        assert!(prefers_vulkan(None, None));
        assert!(prefers_vulkan(Some(",wgpu,tiny-skia"), None));
        assert!(prefers_vulkan(
            Some(""),
            Some(Backends::VULKAN | Backends::GL)
        ));
        assert!(!prefers_vulkan(Some("tiny-skia,wgpu"), None));
        assert!(!prefers_vulkan(None, Some(Backends::GL)));
        assert!(!prefers_vulkan(Some("wgpu"), Some(Backends::empty())));
    }

    #[test]
    fn resume_targets_suspended_amdgpu_render_nodes() {
        let root = tempfile::tempdir().unwrap();
        let sysfs = root.path().join("drm");
        let devices = root.path().join("dri");
        for (name, driver, status) in [
            ("card0", "amdgpu", "suspended"),
            ("renderD128", "amdgpu", "suspended"),
            ("renderD129", "amdgpu", "active"),
            ("renderD130", "nvidia", "suspended"),
            ("renderD131", "amdgpu", "unsupported"),
            ("renderD-other", "amdgpu", "suspended"),
        ] {
            let device = sysfs.join(name).join("device");
            fs::create_dir_all(device.join("power")).unwrap();
            symlink(
                Path::new("/sys/bus/pci/drivers").join(driver),
                device.join("driver"),
            )
            .unwrap();
            fs::write(device.join("power/runtime_status"), format!("{status}\n")).unwrap();
        }
        fs::create_dir_all(sysfs.join("renderD132")).unwrap();

        assert_eq!(
            suspended_amdgpu_nodes(&sysfs, &devices),
            [devices.join("renderD128")]
        );
        assert!(suspended_amdgpu_nodes(&root.path().join("missing"), &devices).is_empty());
    }
}
