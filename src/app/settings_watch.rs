//! One bounded directory watcher per window. Watching the parent survives an
//! editor's atomic rename; a quiet interval combines partial writes.
use super::*;
use notify::{RecursiveMode, Watcher};

pub(super) fn stream(
    path: &PathBuf,
) -> impl cosmic::iced::futures::Stream<Item = settings::LiveSettings> + Send + 'static {
    let path = path.clone();
    cosmic::iced::stream::channel(1, move |mut output| async move {
        use cosmic::iced::futures::SinkExt;
        let (sender, receiver) = async_channel::bounded(1);
        let watched_path = path.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event
                .as_ref()
                .is_ok_and(|event| event.paths.iter().any(|changed| changed == &watched_path))
            {
                let _ = sender.try_send(());
            }
        });
        let mut watcher = match watcher {
            Ok(watcher) => watcher,
            Err(error) => {
                let _ = output
                    .send(settings::LiveSettings::now(Err(format!(
                        "Could not watch settings.toml: {error}"
                    ))))
                    .await;
                return;
            }
        };
        if let Some(parent) = path.parent() {
            if let Err(error) = watcher.watch(parent, RecursiveMode::NonRecursive) {
                let _ = output
                    .send(settings::LiveSettings::now(Err(format!(
                        "Could not watch settings.toml: {error}"
                    ))))
                    .await;
                return;
            }
        }
        // Also cover a write between bootstrap and subscription registration.
        let initial_path = path.clone();
        let initial =
            tokio::task::spawn_blocking(move || settings::load_live_observed(initial_path))
                .await
                .unwrap_or_else(|error| settings::LiveSettings::now(Err(error.to_string())));
        if output.send(initial).await.is_err() {
            return;
        }
        while receiver.recv().await.is_ok() {
            // Restart the quiet interval for every new write, without an unbounded queue.
            loop {
                match tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await {
                    Ok(Ok(())) => continue,
                    _ => break,
                }
            }
            let path = path.clone();
            let result = tokio::task::spawn_blocking(move || settings::load_live_observed(path))
                .await
                .unwrap_or_else(|error| settings::LiveSettings::now(Err(error.to_string())));
            if output.send(result).await.is_err() {
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;

    #[tokio::test]
    async fn directory_watch_observes_atomic_replacement_and_invalid_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        settings::save(&path, &AppSettings::default()).unwrap();
        let stream = stream(&path);
        cosmic::iced::futures::pin_mut!(stream);
        assert!(stream.next().await.unwrap().result.is_ok());
        let updated = AppSettings {
            terminal_font_size: 19.0,
            ..AppSettings::default()
        };
        settings::save(&path, &updated).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .unwrap()
            .unwrap()
            .result
            .unwrap();
        assert_eq!(result.terminal_font_size, 19.0);
        std::fs::write(&path, "schema_version = 3\nterminal_font_size = -1\n").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(3), stream.next())
                .await
                .unwrap()
                .unwrap()
                .result
                .is_err()
        );
    }
}
