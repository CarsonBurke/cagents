#[cfg(not(feature = "development"))]
mod production {
    use std::{
        os::unix::process::CommandExt,
        process::{Command, Stdio},
        sync::LazyLock,
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use ksni::{TrayMethods, menu::StandardItem};
    use tokio::sync::oneshot;

    use crate::instance;

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

    struct DaemonTray;

    impl DaemonTray {
        fn run_control(arguments: &[&str]) {
            let executable = match std::env::current_exe() {
                Ok(mut executable) => {
                    executable.set_file_name(format!("cagents{}", std::env::consts::EXE_SUFFIX));
                    executable
                }
                Err(error) => {
                    tracing::warn!(%error, "could not locate cagents for tray action");
                    return;
                }
            };
            let mut child = match Command::new(&executable)
                .args(arguments)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
            {
                Ok(child) => child,
                Err(error) => {
                    tracing::warn!(%error, path = %executable.display(), "could not run cagents tray action");
                    return;
                }
            };
            if let Err(error) = thread::Builder::new()
                .name("cagents-tray-action-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                })
            {
                tracing::warn!(%error, "could not reap cagents tray action");
            }
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
            Self::run_control(&["ui"]);
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            vec![
                StandardItem {
                    label: "Open cagents".into(),
                    icon_name: instance::APPLICATION_ID.into(),
                    activate: Box::new(|_| Self::run_control(&["ui"])),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: "Stop daemon".into(),
                    icon_name: "application-exit".into(),
                    activate: Box::new(|_| Self::run_control(&["daemon", "stop"])),
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
                    let handle = match DaemonTray.assume_sni_available(true).spawn().await {
                        Ok(handle) => handle,
                        Err(error) => {
                            tracing::warn!(%error, "could not register cagents tray icon");
                            return;
                        }
                    };
                    let _ = shutdown_rx.await;
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

#[cfg(not(feature = "development"))]
pub use production::{TrayGuard, start};

#[cfg(feature = "development")]
pub struct TrayGuard;

#[cfg(feature = "development")]
pub fn start() -> TrayGuard {
    TrayGuard
}
