use ashpd::desktop::Color;
use ashpd::desktop::settings::{ColorScheme, Contrast};
use iced::futures::{self, FutureExt, SinkExt, StreamExt, select};
use iced_futures::stream;
use tracing::error;

#[derive(Debug, Clone)]
pub enum Desktop {
    Accent(Color),
    ColorScheme(ColorScheme),
    Contrast(Contrast),
}

#[cold]
pub fn desktop_settings() -> iced_futures::Subscription<Desktop> {
    iced_futures::Subscription::run(|| {
        stream::channel(10, |mut tx: futures::channel::mpsc::Sender<Desktop>| {
            async move {
                let mut attempts = 0;
                loop {
                    let Ok(settings) = ashpd::desktop::settings::Settings::new().await else {
                        error!("Failed to create the settings proxy");
                        retry_after(&mut attempts).await;
                        continue;
                    };

                    match settings.color_scheme().await {
                        Ok(color_scheme) => {
                            let _ = tx.send(Desktop::ColorScheme(color_scheme)).await;
                        }
                        Err(err) => error!("Failed to get the color scheme {err:?}"),
                    };
                    match settings.contrast().await {
                        Ok(contrast) => {
                            let _ = tx.send(Desktop::Contrast(contrast)).await;
                        }
                        Err(err) => error!("Failed to get the contrast {err:?}"),
                    };

                    let mut color_scheme_stream =
                        settings.receive_color_scheme_changed().await.ok();
                    if color_scheme_stream.is_none() {
                        error!("Failed to receive color scheme changes");
                    }

                    let mut contrast_stream = settings.receive_contrast_changed().await.ok();
                    if contrast_stream.is_none() {
                        error!("Failed to receive contrast changes");
                    }

                    loop {
                        if color_scheme_stream.is_none() && contrast_stream.is_none() {
                            break;
                        }
                        let next_color_scheme = async {
                            if let Some(s) = color_scheme_stream.as_mut() {
                                return s.next().await;
                            }
                            futures::future::pending().await
                        };

                        let next_contrast = async {
                            if let Some(s) = contrast_stream.as_mut() {
                                return s.next().await;
                            }
                            futures::future::pending().await
                        };

                        select! {
                            s = next_color_scheme.fuse() => {
                                if let Some(s) = s {
                                    _ = tx.send(Desktop::ColorScheme(s)).await;
                                    attempts = 0;
                                } else {
                                    color_scheme_stream = None;
                                }
                            },

                            c = next_contrast.fuse() => {
                                if let Some(c) = c {
                                    _ = tx.send(Desktop::Contrast(c)).await;
                                    attempts = 0;
                                } else {
                                    contrast_stream = None;
                                }
                            }
                        };
                    }

                    // Neither stream is left. When the session bus is gone,
                    // every call above fails at once, so reconnecting without
                    // a pause would spin and log five errors per pass.
                    retry_after(&mut attempts).await;
                }
            }
        })
    })
}

/// Waits before the next attempt to reach the settings portal, doubling the
/// pause after each consecutive failure up to a minute.
async fn retry_after(attempts: &mut u32) {
    #[cfg(feature = "tokio")]
    {
        let seconds = 2_u64.saturating_pow(*attempts).min(60);
        ::tokio::time::sleep(::tokio::time::Duration::from_secs(seconds)).await;
        *attempts = attempts.saturating_add(1);
    }
    #[cfg(not(feature = "tokio"))]
    {
        let _ = attempts;
        futures::future::pending::<()>().await;
    }
}
