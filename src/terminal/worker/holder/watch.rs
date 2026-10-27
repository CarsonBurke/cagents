//! Observe only the already-authenticated original control socket/root pidfd.
//! Descriptor handover remains synchronous; this parser handles report frames.
use super::*;
use tokio::io::unix::AsyncFd;

pub(super) struct Watch {
    task: tokio::task::JoinHandle<()>,
    done: Receiver<()>,
}

impl Watch {
    pub(super) fn start(
        control: UnixStream,
        root: File,
        released: std::io::PipeReader,
        exits: Sender<ChildExit>,
        acknowledge: Sender<Result<Acknowledgement, String>>,
        reported: bool,
    ) -> Result<Self, String> {
        let runtime = crate::terminal::reactor::runtime()?;
        // Register every owned descriptor before admitting the observer.
        let _entered = runtime.enter();
        let control = AsyncFd::new(control).map_err(|error| error.to_string())?;
        let root = AsyncFd::new(root).map_err(|error| error.to_string())?;
        let released = AsyncFd::new(released).map_err(|error| error.to_string())?;
        let (finished, done) = crossbeam_channel::bounded::<()>(0);
        let task = runtime.spawn(async move {
            let _finished = finished;
            observe(control, root, released, exits, acknowledge, reported).await;
        });
        Ok(Self { task, done })
    }

    /// The owner closes its release pipe before calling this. Metadata I/O
    /// cannot make Holder::drop wait indefinitely, even on a partial frame.
    pub(super) fn stop(self) {
        if matches!(
            self.done.recv_timeout(RELEASE_GRACE),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        ) {
            self.task.abort();
            tracing::warn!("terminal holder observer exceeded its cancellation deadline");
        }
    }
}

async fn observe(
    control: AsyncFd<UnixStream>,
    root: AsyncFd<File>,
    released: AsyncFd<std::io::PipeReader>,
    exits: Sender<ChildExit>,
    acknowledge: Sender<Result<Acknowledgement, String>>,
    mut reported: bool,
) {
    loop {
        let response = tokio::select! {
            biased;
            _ = released.readable() => return,
            response = receive_report(&control) => response,
        };
        match response {
            Ok(Some(Report::Exited { wait_status })) if !reported => {
                reported = true;
                let _ = exits.try_send(Ok(exit_status(wait_status)));
            }
            Ok(Some(Report::Detached)) => {
                let _ = acknowledge.try_send(Ok(Acknowledgement::Detached));
                return;
            }
            Ok(Some(Report::ScreenCached)) => {
                let _ = acknowledge.try_send(Ok(Acknowledgement::ScreenCached));
            }
            Ok(Some(Report::AdoptionCommitted)) => {
                let _ = acknowledge.try_send(Ok(Acknowledgement::AdoptionCommitted));
            }
            Ok(Some(Report::Failed(message))) => {
                let _ = acknowledge.try_send(Err(message));
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
        // A continuously ready holder must not monopolize other streams.
        tokio::task::yield_now().await;
    }
    if reported {
        return;
    }
    tracing::warn!("a terminal holder ended before its root process; the terminal stays attached");
    // Control EOF alone cannot certify the root's exit. Wait for that original
    // pidfd, or for the owner's release, without synthesizing a known status.
    tokio::select! {
        biased;
        _ = released.readable() => {},
        result = root.readable() => {
            if result.is_ok() {
                let _ = exits.try_send(Err("the terminal's holder ended before it, so its exit status is unknown".into()));
            }
        },
    }
}

async fn receive_report(control: &AsyncFd<UnixStream>) -> io::Result<Option<Report>> {
    let mut header = [0u8; 4];
    if !receive_exact(control, &mut header, true).await? {
        return Ok(None);
    }
    let length = u32::from_le_bytes(header);
    if length > MAX_FRAME {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut body = vec![0u8; length as usize];
    receive_exact(control, &mut body, false).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

async fn receive_exact(
    control: &AsyncFd<UnixStream>,
    bytes: &mut [u8],
    allow_eof: bool,
) -> io::Result<bool> {
    let mut filled = 0;
    while filled < bytes.len() {
        let mut ready = control.readable().await?;
        let result = ready.try_io(|fd| {
            // MSG_DONTWAIT is per call: changing O_NONBLOCK here would also
            // change the owner's cloned socket and its bounded control writes.
            // SAFETY: owned socket and remaining writable slice span the call.
            let count = unsafe {
                libc::recv(
                    fd.get_ref().as_raw_fd(),
                    bytes[filled..].as_mut_ptr().cast(),
                    (bytes.len() - filled).min(64 * 1024),
                    libc::MSG_DONTWAIT,
                )
            };
            if count < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        });
        match result {
            Err(_) => continue,
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Ok(Err(error)) => return Err(error),
            Ok(Ok(0)) if filled == 0 && allow_eof => return Ok(false),
            Ok(Ok(0)) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(Ok(count)) => filled += count,
        }
        if filled < bytes.len() {
            tokio::task::yield_now().await;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
