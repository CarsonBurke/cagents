//! Hands files, folders and URLs to the desktop: through the freedesktop
//! file manager interface and portals on Linux, and through `open(1)` on
//! macOS, which reveals an item selected in Finder and opens anything else
//! with its default application.
use std::{future::Future, path::Path, process::Stdio};

#[cfg(target_os = "linux")]
use ashpd::zbus;
use url::Url;

use crate::location::SanitizedLocation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevealOutcome {
    Selected,
    OpenedContainingFolder,
}

pub async fn reveal(path: &Path) -> Result<RevealOutcome, String> {
    reveal_with(path, show_item, open_system_url).await
}

#[cfg(target_os = "linux")]
async fn show_item(uri: Url) -> Result<(), String> {
    use std::time::Duration;
    // A desktop without this interface can still open the containing folder.
    // Bound activation time so an unresponsive file manager cannot prevent
    // that fallback indefinitely.
    tokio::time::timeout(Duration::from_secs(5), async {
        let connection = zbus::Connection::session().await?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.freedesktop.FileManager1",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1",
        )
        .await?;
        proxy
            .call::<_, _, ()>("ShowItems", &(vec![uri.as_str()], ""))
            .await
    })
    .await
    .map_err(|_| "File manager did not respond".to_owned())?
    .map_err(|error| error.to_string())
}

/// Finder selects the item in its folder, the way the freedesktop
/// `ShowItems` does.
#[cfg(target_os = "macos")]
async fn show_item(uri: Url) -> Result<(), String> {
    let path = uri
        .to_file_path()
        .map_err(|_| "Could not reveal a non-local path".to_owned())?;
    let mut open = tokio::process::Command::new(MACOS_OPEN);
    // `--` keeps a name starting with a dash from reading as an option.
    open.args(["-R", "--"]).arg(path);
    run_opener(open)
        .await
        .map_err(|status| format!("Finder could not reveal the item: {status}"))
}

/// macOS's opener, by absolute path so a `PATH` entry cannot stand in for it.
#[cfg(target_os = "macos")]
const MACOS_OPEN: &str = "/usr/bin/open";

/// The program that opens a folder in the desktop's file manager when given
/// its path.
#[cfg(target_os = "linux")]
pub const FOLDER_OPENER: &str = "xdg-open";
#[cfg(target_os = "macos")]
pub const FOLDER_OPENER: &str = MACOS_OPEN;

async fn reveal_with<S, O, SF, OF>(
    path: &Path,
    select: S,
    open_folder: O,
) -> Result<RevealOutcome, String>
where
    S: FnOnce(Url) -> SF,
    O: FnOnce(Url) -> OF,
    SF: Future<Output = Result<(), String>>,
    OF: Future<Output = Result<(), String>>,
{
    // Preserve the item itself, including symlinks. Canonicalizing here would
    // reveal a symlink's target instead of the row the user clicked.
    let uri = Url::from_file_path(path)
        .map_err(|_| "Could not reveal a relative file path".to_owned())?;
    if select(uri).await.is_ok() {
        return Ok(RevealOutcome::Selected);
    }
    let parent = path.parent().unwrap_or(path);
    let folder = Url::from_directory_path(parent)
        .map_err(|_| "Could not locate the containing folder".to_owned())?;
    open_folder(folder)
        .await
        .map_err(|error| format!("Could not open the containing folder: {error}"))?;
    Ok(RevealOutcome::OpenedContainingFolder)
}

/// What hands a URL to the system browser. The app opens every such URL
/// through one of these, so a test can stand in for the desktop and never
/// start a real browser.
#[derive(Clone, Default)]
pub enum SystemUrlOpener {
    #[default]
    Desktop,
    /// Records each URL it is given and reports it opened.
    #[cfg(all(any(test, feature = "test-support"), feature = "integrated-browser"))]
    Recording(std::sync::Arc<std::sync::Mutex<Vec<Url>>>),
}

impl SystemUrlOpener {
    pub fn open(&self, url: Url) -> impl Future<Output = Result<(), String>> + Send + 'static {
        let opener = self.clone();
        async move {
            match opener {
                Self::Desktop => open_system_url(url).await,
                #[cfg(all(any(test, feature = "test-support"), feature = "integrated-browser"))]
                Self::Recording(opened) => {
                    opened.lock().unwrap().push(url);
                    Ok(())
                }
            }
        }
    }
}

/// Opens `url` through `open(1)` without running anything the person did
/// not choose to run.
///
/// Given a path, `open` launches an application bundle, runs a `.command`
/// script in Terminal or starts an installer, and a file an agent wrote
/// carries no quarantine, so Gatekeeper never asks first. So web and mail
/// links open in their default applications; a local page, image, PDF or
/// plain-text document opens in the application its kind names, which
/// macOS picks by extension, so a script named `.png` still only displays;
/// a plain folder opens in Finder; and any other local item, scripts and
/// bundles among them, is only selected in Finder, to be opened deliberately.
/// Other schemes are refused: an `ssh:`, `x-apple.` or application link can
/// act without asking. Linux leaves this to the portal, which asks.
///
/// The item is classified, then opened: an agent that replaced a plain
/// folder with an application bundle in between would have Finder open
/// the bundle.
#[cfg(target_os = "macos")]
pub async fn open_system_url(url: Url) -> Result<(), String> {
    let presented = SanitizedLocation::from(&url);
    let mut open = tokio::process::Command::new(MACOS_OPEN);
    match macos_open(&url).await? {
        // A URL begins with its scheme, so it cannot read as an option.
        MacosOpen::Url => open.arg("-u").arg(url.as_str()),
        MacosOpen::Document(path) => open.arg("--").arg(path),
        MacosOpen::Folder(path) => open.args(["-a", "Finder", "--"]).arg(path),
        MacosOpen::Reveal(path) => open.args(["-R", "--"]).arg(path),
    };
    run_opener(open)
        .await
        .map_err(|outcome| format!("could not open {presented}; system opener {outcome}"))
}

/// How [`open_system_url`] has `open(1)` handle a URL.
#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
enum MacosOpen {
    Url,
    Document(std::path::PathBuf),
    Folder(std::path::PathBuf),
    Reveal(std::path::PathBuf),
}

/// Kinds whose default applications only display or edit them: pages open
/// in the browser, as browserless mode needs for a saved local page, images
/// and PDFs in Preview, text in a text editor. Source code is left out:
/// macOS opens a `.py` or `.sh` in a launcher or Terminal, which runs it.
#[cfg(target_os = "macos")]
const DOCUMENT_EXTENSIONS: &[&str] = &[
    "avif", "bmp", "csv", "gif", "heic", "heif", "htm", "html", "jpeg", "jpg", "json", "log",
    "markdown", "md", "pdf", "png", "svg", "tif", "tiff", "txt", "webp", "xhtml",
];

#[cfg(target_os = "macos")]
async fn macos_open(url: &Url) -> Result<MacosOpen, String> {
    match url.scheme() {
        "http" | "https" | "mailto" => return Ok(MacosOpen::Url),
        "file" => {}
        _ => return Err("cagents opens only web, mail and local links on macOS".to_owned()),
    }
    let path = url
        .to_file_path()
        .map_err(|_| "Could not open a link to another computer's file".to_owned())?;
    // Launch Services opens a symbolic link as what it leads to, so that is
    // what is classified and opened.
    let path = tokio::fs::canonicalize(&path)
        .await
        .map_err(|error| format!("Could not open the item: {error}"))?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| format!("Could not open the item: {error}"))?;
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    Ok(if metadata.is_dir() {
        // Bundles are folders that Finder launches or installs, and every
        // kind macOS knows is named by its extension.
        if extension.is_some()
            || tokio::fs::try_exists(path.join("Contents/Info.plist"))
                .await
                .unwrap_or(true)
        {
            MacosOpen::Reveal(path)
        } else {
            MacosOpen::Folder(path)
        }
    } else if extension.is_some_and(|extension| DOCUMENT_EXTENSIONS.contains(&extension.as_str())) {
        MacosOpen::Document(path)
    } else {
        MacosOpen::Reveal(path)
    })
}

#[cfg(target_os = "linux")]
pub async fn open_system_url(url: Url) -> Result<(), String> {
    let portal_result = async {
        let request = ashpd::desktop::open_uri::OpenFileRequest::default()
            .send_uri(&url)
            .await
            .map_err(|error| error.to_string())?;
        request.response().map_err(|error| error.to_string())
    }
    .await;
    match portal_result {
        Ok(()) => Ok(()),
        // Portal diagnostics are also intentionally omitted: they are an
        // external error boundary and may include the URI supplied above.
        Err(_) => open_system_url_with_opener(&url, tokio::process::Command::new("xdg-open")).await,
    }
}

pub async fn open_system_url_with_opener(
    url: &Url,
    mut opener: tokio::process::Command,
) -> Result<(), String> {
    let presented = SanitizedLocation::from(url);
    // Linux reaches the opener only after the portal failed.
    let failed = if cfg!(target_os = "linux") {
        "desktop portal could not open"
    } else {
        "could not open"
    };
    opener.arg(url.as_str());
    run_opener(opener)
        .await
        .map_err(|outcome| format!("{failed} {presented}; system opener {outcome}"))
}

/// Runs an opener to completion, or says how it failed.
async fn run_opener(mut opener: tokio::process::Command) -> Result<(), String> {
    let result = opener
        .stdin(Stdio::null())
        // An opener can echo its argument in diagnostics. The authoritative
        // URL may contain credentials, query data, or a fragment, so neither
        // child stream may inherit cagents' presentation or logging streams.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    match result {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("exited with {status}")),
        Err(error) => Err(format!("could not be started: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn selecting_an_item_preserves_its_path_and_does_not_open_a_second_window() {
        let path = Path::new("/workspace/project/linked folder/file #1.txt");
        let outcome = reveal_with(
            path,
            |uri| async move {
                assert_eq!(uri.to_file_path().unwrap(), path);
                assert_eq!(
                    uri.as_str(),
                    "file:///workspace/project/linked%20folder/file%20%231.txt"
                );
                Ok(())
            },
            |_| async { panic!("Successful selection must not open a second window") },
        )
        .await
        .unwrap();
        assert_eq!(outcome, RevealOutcome::Selected);
    }

    #[tokio::test]
    async fn unavailable_selection_opens_parent_for_files_and_directories() {
        for path in ["/workspace/project/file.rs", "/workspace/project/folder"] {
            let outcome = reveal_with(
                Path::new(path),
                |_| async { Err("Interface unavailable".into()) },
                |uri| async move {
                    assert_eq!(uri.as_str(), "file:///workspace/project/");
                    Ok(())
                },
            )
            .await
            .unwrap();
            assert_eq!(outcome, RevealOutcome::OpenedContainingFolder);
        }
    }

    #[tokio::test]
    async fn folder_open_failure_is_reported() {
        let error = reveal_with(
            Path::new("/workspace/file.rs"),
            |_| async { Err("No file manager interface".into()) },
            |_| async { Err("No system opener".into()) },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            "Could not open the containing folder: No system opener"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_opens_nothing_that_could_run() {
        let directory = tempfile::tempdir().unwrap();
        // The temporary directory is itself reached through a link.
        let root = directory.path().canonicalize().unwrap();
        let item = |name: &str| root.join(name);
        std::fs::write(item("photo.PNG"), b"").unwrap();
        std::fs::write(item("page.html"), b"").unwrap();
        std::fs::write(item("tool.py"), b"").unwrap();
        std::fs::write(item("run.command"), b"#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(item("run.command"), item("alias.png")).unwrap();
        std::fs::create_dir(item("plain")).unwrap();
        std::fs::create_dir_all(item("Tool.app/Contents")).unwrap();
        std::fs::create_dir_all(item("bare/Contents")).unwrap();
        std::fs::write(item("bare/Contents/Info.plist"), b"").unwrap();
        let open = |path: std::path::PathBuf| async move {
            macos_open(&Url::from_file_path(path).unwrap())
                .await
                .unwrap()
        };
        assert_eq!(
            open(item("photo.PNG")).await,
            MacosOpen::Document(item("photo.PNG"))
        );
        assert_eq!(
            open(item("page.html")).await,
            MacosOpen::Document(item("page.html"))
        );
        assert_eq!(open(item("plain")).await, MacosOpen::Folder(item("plain")));
        // An alias is opened as what it leads to.
        assert_eq!(
            open(item("alias.png")).await,
            MacosOpen::Reveal(item("run.command"))
        );
        for name in ["run.command", "tool.py", "Tool.app", "bare"] {
            assert_eq!(
                open(item(name)).await,
                MacosOpen::Reveal(item(name)),
                "{name}"
            );
        }
        for url in [
            "https://example.com/",
            "http://example.com/",
            "mailto:a@example.com",
        ] {
            assert_eq!(
                macos_open(&Url::parse(url).unwrap()).await,
                Ok(MacosOpen::Url)
            );
        }
        for url in [
            "ssh://host",
            "x-apple.systempreferences:",
            "vscode://file/a",
        ] {
            assert!(
                macos_open(&Url::parse(url).unwrap()).await.is_err(),
                "{url}"
            );
        }
    }

    #[tokio::test]
    async fn relative_paths_never_reach_desktop_services() {
        let result = reveal_with(
            Path::new("file.rs"),
            |_| async { panic!("Relative path must not reach selection service") },
            |_| async { panic!("Relative path must not reach system opener") },
        )
        .await;
        assert!(result.is_err());
    }
}
