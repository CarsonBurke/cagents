//! Wakes the GPUs the renderer will open, while the rest of startup runs.
//!
//! Creating the window's renderer enumerates every Vulkan device, and Mesa's
//! drivers open each GPU's render node to do so, even though the renderer only
//! uses the compositor's GPU. Opening a runtime-suspended GPU resumes it first.
//! On a Framework 16, the Radeon RX 7700S beside the integrated GPU takes about
//! 2.1 s to resume. That resume used to begin only after the daemon handshake,
//! session restore and application init, and it held the first frame on the UI
//! thread. Opening the render nodes from a background thread at launch overlaps
//! the resume with that work. A GPU that is already awake opens at once.

use std::path::{Path, PathBuf};

/// NVIDIA's PCI vendor ID. The renderer keeps NVIDIA's Vulkan driver unloaded
/// unless the compositor runs on an NVIDIA GPU, which is then awake already,
/// so waking one here would power up a GPU this window never opens.
const NVIDIA: &str = "0x10de";

/// Starts waking the GPUs in the background when the renderer will enumerate
/// Vulkan devices. Failures only lose the head start, so they are not reported
/// beyond a debug log.
pub fn spawn() {
    if !renderer_enumerates_vulkan(
        std::env::var("ICED_BACKEND").ok().as_deref(),
        std::env::var("WGPU_BACKEND").ok().as_deref(),
    ) {
        return;
    }
    let nodes = render_nodes(Path::new("/sys/class/drm"), Path::new("/dev/dri"));
    if nodes.is_empty() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("gpu-wake".into())
        .spawn(move || {
            for node in nodes {
                let started = std::time::Instant::now();
                // Opening resumes the device; closing lets it suspend again
                // after its own autosuspend delay if nothing else opens it.
                let opened = std::fs::File::open(&node).map(drop);
                tracing::debug!(
                    node = %node.display(),
                    elapsed_ms = started.elapsed().as_millis(),
                    error = opened.err().map(tracing::field::display),
                    "Woke a GPU for the renderer"
                );
            }
        });
    if let Err(error) = spawned {
        tracing::debug!(%error, "Could not start waking the GPUs");
    }
}

/// Whether the renderer's first choice enumerates Vulkan devices: cosmic tries
/// wgpu for any `ICED_BACKEND` candidate that is empty or `wgpu`, and wgpu
/// enumerates Vulkan unless `WGPU_BACKEND` leaves it out.
fn renderer_enumerates_vulkan(iced_backend: Option<&str>, wgpu_backend: Option<&str>) -> bool {
    let wgpu = iced_backend.is_none_or(|candidates| {
        let mut candidates = candidates.split(',').filter(|candidate| !candidate.is_empty()).peekable();
        candidates.peek().is_none() || candidates.any(|candidate| candidate == "wgpu")
    });
    let vulkan = wgpu_backend.is_none_or(|backends| {
        backends
            .to_lowercase()
            .split(',')
            .any(|backend| matches!(backend.trim(), "vulkan" | "vk"))
    });
    wgpu && vulkan
}

/// The render nodes under `devices` whose GPU, as `sysfs` describes it, is not
/// NVIDIA's. A node whose vendor cannot be read is skipped.
fn render_nodes(sysfs: &Path, devices: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(sysfs) else {
        return Vec::new();
    };
    let mut nodes = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .filter(|name| name.to_string_lossy().starts_with("renderD"))
        .filter(|name| {
            std::fs::read_to_string(sysfs.join(name).join("device/vendor"))
                .is_ok_and(|vendor| vendor.trim() != NVIDIA)
        })
        .map(|name| devices.join(name))
        .collect::<Vec<_>>();
    nodes.sort();
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_renderer_enumerates_vulkan() {
        assert!(renderer_enumerates_vulkan(None, None));
        assert!(renderer_enumerates_vulkan(Some("wgpu"), None));
        assert!(renderer_enumerates_vulkan(Some(""), None));
        assert!(renderer_enumerates_vulkan(Some("tiny-skia,wgpu"), Some("VK,gl")));
    }

    #[test]
    fn a_renderer_without_wgpu_or_vulkan_wakes_nothing() {
        assert!(!renderer_enumerates_vulkan(Some("tiny-skia"), None));
        assert!(!renderer_enumerates_vulkan(None, Some("gl")));
        assert!(!renderer_enumerates_vulkan(Some("wgpu"), Some("")));
    }

    #[test]
    fn only_render_nodes_of_gpus_other_than_nvidia_are_woken() {
        let root = tempfile::tempdir().unwrap();
        let sysfs = root.path().join("drm");
        for (name, vendor) in [
            ("card1", Some("0x1002")),
            ("renderD128", Some("0x1002\n")),
            ("renderD129", Some("0x10de\n")),
            ("renderD130", Some("0x8086\n")),
            ("renderD131", None),
        ] {
            let device = sysfs.join(name).join("device");
            std::fs::create_dir_all(&device).unwrap();
            if let Some(vendor) = vendor {
                std::fs::write(device.join("vendor"), vendor).unwrap();
            }
        }
        let devices = Path::new("/dev/dri");

        assert_eq!(
            render_nodes(&sysfs, devices),
            [devices.join("renderD128"), devices.join("renderD130")]
        );
        assert!(render_nodes(&root.path().join("missing"), devices).is_empty());
    }
}
