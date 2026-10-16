//! Bounded PTY reads on the existing shared terminal reactor.
//! The writer establishes O_NONBLOCK on the original shared master before
//! registration. This reader never changes descriptor flags or runs a parser.
use super::*;
use tokio::io::unix::AsyncFd;

pub(super) struct Task(Option<tokio::task::JoinHandle<()>>);

impl Drop for Task {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            // Only failure/destructive teardown uses this fallback. Carry
            // first observes done after draining every consumed byte.
            task.abort();
        }
    }
}

impl Task {
    pub(super) fn prepare(
        reader: File,
        wake: OwnedFd,
        output: Sender<PtyMessage>,
        discard: Arc<AtomicBool>,
    ) -> Result<(Self, tokio::sync::oneshot::Sender<()>, Receiver<()>), String> {
        Self::prepare_observed(reader, wake, output, discard, |_| {})
    }

    fn prepare_observed(
        reader: File,
        wake: OwnedFd,
        output: Sender<PtyMessage>,
        discard: Arc<AtomicBool>,
        observed: impl FnMut(usize) + Send + 'static,
    ) -> Result<(Self, tokio::sync::oneshot::Sender<()>, Receiver<()>), String> {
        let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 || flags & libc::O_NONBLOCK == 0 {
            return Err(
                "Shared PTY reader requires the original nonblocking writer admission".into(),
            );
        }
        let runtime = crate::terminal::reactor::runtime()?;
        let _entered = runtime.enter();
        let reader = AsyncFd::new(reader).map_err(|error| format!("register PTY read: {error}"))?;
        let wake =
            AsyncFd::new(wake).map_err(|error| format!("register PTY cancellation: {error}"))?;
        let (start, started) = tokio::sync::oneshot::channel();
        let (finished, done) = crossbeam_channel::bounded::<()>(0);
        let task = runtime.spawn(async move {
            let _finished = finished;
            if started.await.is_ok() {
                read(reader, wake, output, discard, observed).await;
            }
        });
        Ok((Self(Some(task)), start, done))
    }

    /// Called only by the original parser thread, after positive done. It
    /// observes task termination; it does not prove process-family cleanup.
    pub(super) fn join(mut self) -> Result<(), String> {
        crate::terminal::reactor::runtime()?
            .block_on(self.0.take().expect("original reader task joined once"))
            .map_err(|error| format!("PTY reader task failed: {error}"))
    }
}

async fn read(
    reader: AsyncFd<File>,
    wake: AsyncFd<OwnedFd>,
    output: Sender<PtyMessage>,
    discard: Arc<AtomicBool>,
    mut observed: impl FnMut(usize) + Send,
) {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let result = tokio::select! {
            biased;
            _ = wake.readable() => return,
            ready = reader.readable() => match ready {
                Ok(mut ready) => match ready.try_io(|fd| {
                    let count = unsafe {
                        libc::read(fd.get_ref().as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len())
                    };
                    if count < 0 { Err(std::io::Error::last_os_error()) }
                    else { Ok(count as usize) }
                }) {
                    Ok(result) => result,
                    Err(_) => continue, // stale readiness was cleared by try_io
                },
                Err(error) => Err(error),
            },
        };
        let (message, ended) = match result {
            Ok(0) => (PtyMessage::Eof, true),
            Ok(count) => {
                observed(count);
                (PtyMessage::Bytes(buffer[..count].to_vec()), false)
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                tokio::task::yield_now().await;
                continue;
            }
            Err(error) if error.raw_os_error() == Some(libc::EIO) => (PtyMessage::Eof, true),
            Err(error) => (PtyMessage::Error(error.to_string()), true),
        };
        if !deliver(message, &output, &wake, &discard).await || ended {
            return;
        }
        // A continuously readable stream cannot monopolize the reactor.
        tokio::task::yield_now().await;
    }
}

async fn deliver(
    mut message: PtyMessage,
    output: &Sender<PtyMessage>,
    wake: &AsyncFd<OwnedFd>,
    discard: &AtomicBool,
) -> bool {
    let mut draining = false;
    loop {
        if discard.load(Ordering::Acquire) {
            return false;
        }
        match output.try_send(message) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(pending)) => message = pending,
        }
        if draining {
            // Carry closes wake but must deliver this exact consumed message.
            // Do not repeatedly poll its permanently-ready cancellation FD.
            tokio::time::sleep(Duration::from_millis(10)).await;
        } else {
            tokio::select! {
                biased;
                _ = wake.readable() => draining = true,
                _ = tokio::time::sleep(Duration::from_millis(10)) => {},
            }
        }
    }
}

#[cfg(test)]
mod tests;
