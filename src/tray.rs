#[cfg(any(test, not(any(debug_assertions, feature = "development"))))]
mod production {
    use std::{
        os::unix::process::CommandExt,
        process::{Command, Stdio},
        sync::{Arc, LazyLock},
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use ksni::{TrayMethods, menu::StandardItem};
    use tokio::sync::{mpsc, oneshot};

    use crate::{
        control::{ControlClient, shutdown_outcome_message},
        instance,
        terminal_ipc::{ShutdownOutcome, daemon_socket_path},
    };

    static ICON: LazyLock<ksni::Icon> = LazyLock::new(|| {
        let image = image::load_from_memory_with_format(
            include_bytes!("../packaging/dev.cagents.Cagents.png"),
            image::ImageFormat::Png,
        )
        .expect("embedded tray icon must be a valid PNG")
        .into_rgba8();
        let (width, height) = image.dimensions();
        let mut data = image.into_raw();
        for pixel in data.chunks_exact_mut(4) {
            pixel.rotate_right(1);
        }
        ksni::Icon {
            width: width as i32,
            height: height as i32,
            data,
        }
    });

    struct DaemonTray {
        pending: bool,
        status: String,
        updates: mpsc::UnboundedSender<String>,
    }

    impl DaemonTray {
        fn run_action(&mut self, stop: bool) {
            if self.pending {
                return;
            }
            self.pending = true;
            self.status = if stop {
                "Pending: checking hosted terminals…"
            } else {
                "Pending: opening cagents…"
            }
            .into();
            let updates = self.updates.clone();
            if let Err(error) = thread::Builder::new()
                .name("cagents-tray-action".into())
                .spawn(move || {
                    let result = if stop {
                        stop_from_tray()
                    } else {
                        run_child("cagents", &["ui".into()])
                    };
                    let status = result.unwrap_or_else(|error| format!("Failed: {error}"));
                    let _ = updates.send(status);
                })
            {
                self.pending = false;
                self.status = format!("Failed to start tray action: {error}");
            }
        }
    }

    fn run_child(name: &str, arguments: &[String]) -> Result<String, String> {
        let mut executable = std::env::current_exe().map_err(|error| error.to_string())?;
        executable.set_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        let output = Command::new(&executable)
            .args(arguments)
            .stdin(Stdio::null())
            // The tray lives in the daemon, which inherits the launcher
            // handshake flag from whichever `cagents ui` started it. Left set,
            // the child appends a startup-protocol line to the stdout this
            // function shows the user as the shutdown result.
            .env_remove(instance::UI_LAUNCH_HANDSHAKE_ENV)
            .process_group(0)
            .output()
            .map_err(|error| {
                format!("could not launch {name}: {error}; no confirmation was performed")
            })?;
        child_outcome(output.status.success(), &output.stdout, &output.stderr)
    }

    fn child_outcome(success: bool, stdout: &[u8], stderr: &[u8]) -> Result<String, String> {
        let stdout = String::from_utf8_lossy(stdout);
        let stderr = String::from_utf8_lossy(stderr);
        if !success {
            // Both streams can carry a distinct failure; welded together they
            // read as one nonsensical sentence.
            let detail = [stdout.trim(), stderr.trim()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            let detail = if detail.is_empty() {
                "no output".to_owned()
            } else {
                detail
            };
            return Err(format!("confirmation/action failed: {detail}"));
        }
        if stdout.trim().is_empty() {
            // A crashed or mismatched UI must not be presented as accepted.
            return Ok(
                "Action finished without a shutdown result; daemon termination is not confirmed"
                    .into(),
            );
        }
        Ok(stdout.trim().into())
    }

    fn stop_from_tray() -> Result<String, String> {
        let socket = daemon_socket_path().map_err(|error| error.to_string())?;
        let mut client = ControlClient::connect(&socket).map_err(|error| error.to_string())?;
        match client
            .shutdown_with_policy(true, None)
            .map_err(|error| error.to_string())?
        {
            outcome @ ShutdownOutcome::Accepted { .. } => {
                return Ok(shutdown_outcome_message(&outcome));
            }
            ShutdownOutcome::EmptyRequired { .. } => {}
            outcome => return Ok(shutdown_outcome_message(&outcome)),
        }
        let preflight = client
            .shutdown_preflight()
            .map_err(|error| error.to_string())?;
        let capability =
            serde_json::to_string(&preflight.capability).map_err(|error| error.to_string())?;
        run_child("cagents-ui", &["--confirm-daemon-stop".into(), capability])
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn asynchronous_command_failure_is_visible_and_not_acceptance() {
            assert_eq!(
                child_outcome(false, b"", b"daemon restarted").unwrap_err(),
                "confirmation/action failed: daemon restarted"
            );
            assert_eq!(
                child_outcome(false, b"refused: 2 live\n", b"daemon restarted\n").unwrap_err(),
                "confirmation/action failed: refused: 2 live; daemon restarted"
            );
            assert_eq!(
                child_outcome(false, b" \n", b"").unwrap_err(),
                "confirmation/action failed: no output"
            );
            assert_eq!(
                child_outcome(true, b"Shutdown cancelled; no termination requested\n", b"")
                    .unwrap(),
                "Shutdown cancelled; no termination requested"
            );
            assert!(
                child_outcome(true, b"", b"")
                    .unwrap()
                    .contains("not confirmed")
            );
        }

        #[test]
        fn pending_activation_is_not_a_second_click_confirmation() {
            let (updates, mut results) = mpsc::unbounded_channel();
            let mut tray = DaemonTray {
                pending: true,
                status: "Pending confirmation".into(),
                updates,
            };
            tray.run_action(true);
            assert!(tray.pending);
            assert_eq!(tray.status, "Pending confirmation");
            assert!(results.try_recv().is_err());
        }
    }

    impl ksni::Tray for DaemonTray {
        fn id(&self) -> String {
            instance::APPLICATION_ID.into()
        }

        fn title(&self) -> String {
            "cagents daemon".into()
        }

        fn icon_name(&self) -> String {
            instance::APPLICATION_ID.into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            vec![ICON.clone()]
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            self.run_action(false);
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            vec![
                StandardItem {
                    label: "Open cagents".into(),
                    icon_name: instance::APPLICATION_ID.into(),
                    enabled: !self.pending,
                    activate: Box::new(|tray: &mut DaemonTray| tray.run_action(false)),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: "Stop daemon".into(),
                    icon_name: "application-exit".into(),
                    enabled: !self.pending,
                    activate: Box::new(|tray: &mut DaemonTray| tray.run_action(true)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: self.status.clone(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    pub struct TrayGuard {
        shutdown: Option<oneshot::Sender<()>>,
        worker: Option<JoinHandle<()>>,
    }

    impl Drop for TrayGuard {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(worker) = self.worker.take() {
                let deadline = Instant::now() + Duration::from_millis(250);
                while !worker.is_finished() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                if worker.is_finished() {
                    if worker.join().is_err() {
                        tracing::warn!("cagents tray thread panicked during shutdown");
                    }
                } else {
                    tracing::warn!("cagents tray shutdown exceeded 250 ms; detaching it");
                }
            }
        }
    }

    pub fn start() -> TrayGuard {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let worker = thread::Builder::new()
            .name("cagents-tray".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::warn!(%error, "could not create cagents tray runtime");
                        return;
                    }
                };
                runtime.block_on(async move {
                    let (updates, mut results) = mpsc::unbounded_channel();
                    let tray = DaemonTray {
                        pending: false,
                        status: "Daemon running".into(),
                        updates,
                    };
                    let handle = match tray.assume_sni_available(true).spawn().await {
                        Ok(handle) => handle,
                        Err(error) => {
                            tracing::warn!(%error, "could not register cagents tray icon");
                            return;
                        }
                    };
                    let handle = Arc::new(handle);
                    let update_handle = Arc::clone(&handle);
                    let updater = tokio::spawn(async move {
                        while let Some(status) = results.recv().await {
                            update_handle
                                .update(move |tray| {
                                    tray.pending = false;
                                    tray.status = status;
                                })
                                .await;
                        }
                    });
                    let _ = shutdown_rx.await;
                    updater.abort();
                    handle.shutdown().await;
                });
            })
            .map_err(|error| tracing::warn!(%error, "could not start cagents tray thread"))
            .ok();
        TrayGuard {
            shutdown: Some(shutdown_tx),
            worker,
        }
    }
}

#[cfg(not(any(debug_assertions, feature = "development")))]
pub use production::{TrayGuard, start};

#[cfg(any(debug_assertions, feature = "development"))]
pub struct TrayGuard;

#[cfg(any(debug_assertions, feature = "development"))]
pub fn start() -> TrayGuard {
    TrayGuard
}
