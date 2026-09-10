//! Installed editor choices for native context menus. Discovery never runs in view.
use crate::sidebar::SidebarEditor;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug, Clone)]
pub struct Editor {
    pub choice: SidebarEditor,
    pub executable: PathBuf,
}

pub fn installed() -> Vec<Editor> {
    use std::os::unix::fs::PermissionsExt;
    let paths: Vec<_> = env::var_os("PATH")
        .map(|value| {
            env::split_paths(&value)
                .filter(|path| path.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    let data = desktop_directories();
    let mut editors: Vec<Editor> = Vec::new();
    for (id, name, binaries, desktops, fallback_icon) in [
        (
            "ceditor",
            "ceditor",
            &["ceditor"][..],
            &["io.github.cagents.ceditor.desktop"][..],
            "io.github.cagents.ceditor",
        ),
        (
            "code",
            "VS Code",
            &["code"][..],
            &["code.desktop", "visual-studio-code.desktop"][..],
            "visual-studio-code",
        ),
        (
            "codium",
            "VSCodium",
            &["codium"][..],
            &["codium.desktop", "vscodium.desktop"][..],
            "vscodium",
        ),
        (
            "cursor",
            "Cursor",
            &["cursor"][..],
            &["cursor.desktop"][..],
            "cursor",
        ),
        (
            "zed",
            "Zed",
            &["zed", "zeditor"][..],
            &["dev.zed.Zed.desktop", "zed.desktop"][..],
            "dev.zed.Zed",
        ),
        (
            "sublime",
            "Sublime Text",
            &["subl"][..],
            &["sublime_text.desktop", "sublime-text.desktop"][..],
            "sublime-text",
        ),
    ] {
        let executable = binaries
            .iter()
            .flat_map(|binary| paths.iter().map(move |path| path.join(binary)))
            .find(|path| {
                path.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            });
        let executable = executable.or_else(|| {
            if id != "ceditor" {
                return None;
            }
            let sibling = env::current_exe().ok()?.parent()?.join("ceditor");
            sibling
                .metadata()
                .is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
                .then_some(sibling)
        });
        let Some(executable) = executable else {
            continue;
        };
        let icon = data
            .iter()
            .flat_map(|directory| desktops.iter().map(move |name| directory.join(name)))
            .find_map(|path| desktop_icon(&path))
            .unwrap_or_else(|| fallback_icon.into());
        editors.push(Editor {
            choice: SidebarEditor {
                id: id.into(),
                name: name.into(),
                icon,
            },
            executable,
        });
    }
    editors
}

fn desktop_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(home) = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
    {
        directories.push(home.join("applications"));
    }
    let data = env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    directories.extend(
        env::split_paths(&data)
            .filter(|path| path.is_absolute())
            .map(|path| path.join("applications")),
    );
    directories
}

fn desktop_icon(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut text = String::new();
    fs::File::open(path)
        .ok()?
        .take(64 * 1024)
        .read_to_string(&mut text)
        .ok()?;
    let mut entry = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            entry = line == "[Desktop Entry]";
        }
        if entry && let Some(icon) = line.strip_prefix("Icon=") {
            let icon = icon.trim();
            if !icon.is_empty() && !icon.contains('\0') {
                return Some(icon.into());
            }
        }
    }
    None
}

/// Launch a previously discovered editor, passing paths as a single argv value.
pub fn launch(editor: &Editor, path: &Path) -> Result<(), String> {
    if !path.is_absolute() || !path.exists() {
        return Err("The selected file or directory is unavailable.".into());
    }
    let mut child = Command::new(&editor.executable)
        .arg("--")
        .arg(path)
        .current_dir(if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!(
                "Could not open {} in {}: {error}",
                path.display(),
                editor.choice.name
            )
        })?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
