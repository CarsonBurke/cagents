use std::{future::Future, path::Path, process::Stdio, time::Duration};

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

async fn show_item(uri: Url) -> Result<(), String> {
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

pub(crate) async fn open_system_url(url: Url) -> Result<(), String> {
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

pub(crate) async fn open_system_url_with_opener(
    url: &Url,
    mut opener: tokio::process::Command,
) -> Result<(), String> {
    let presented = SanitizedLocation::from(url);
    let result = opener
        .arg(url.as_str())
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
        Ok(status) => Err(format!(
            "desktop portal could not open {presented}; system opener exited with {status}"
        )),
        Err(error) => Err(format!(
            "desktop portal could not open {presented}; system opener could not be started: {error}"
        )),
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
