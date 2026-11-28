//! The terminal I/O reactor reads PTYs. Each renderer still owns a bounded
//! parser inbox, and a full inbox suspends only its own reader.

use std::{
    cell::RefCell,
    fs::File,
    io::{self, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crossbeam_channel::{Receiver, Sender};
use tokio::{
    io::{Interest, unix::AsyncFd},
    sync::{Notify, oneshot},
};

use super::{PTY_READ_SIZE, PtyMessage, holder::PtyMaster};

thread_local! {
    static BUFFER: RefCell<[u8; PTY_READ_SIZE]> = const { RefCell::new([0; PTY_READ_SIZE]) };
}

pub(super) struct Reader {
    pub(super) task: Task,
    pub(super) output: Receiver<PtyMessage>,
    #[cfg(any(test, feature = "test-support"))]
    pub(super) test_queue: Sender<PtyMessage>,
}

pub(super) struct Task {
    #[cfg(test)]
    pub(super) id: tokio::task::Id,
    pub(super) start: Option<oneshot::Sender<()>>,
    stop: Option<oneshot::Sender<()>>,
    pub(super) done: Receiver<()>,
    flow: Arc<Flow>,
}

impl Task {
    pub(super) fn stop(&mut self) {
        // Closing the sender also stops a task suspended at a full inbox.
        drop(self.start.take());
        drop(self.stop.take());
    }

    pub(super) fn stopping(&self) -> bool {
        self.stop.is_none()
    }

    pub(super) fn consumed(&self) {
        self.flow.capacity.notify_one();
    }

    pub(super) fn discard_pending(&self) {
        self.flow.discard.store(true, Ordering::Release);
        self.flow.capacity.notify_one();
    }
}

#[derive(Default)]
struct Flow {
    capacity: Notify,
    discard: AtomicBool,
}

impl Reader {
    pub(super) fn start(
        master: &PtyMaster,
        capacity: usize,
        pending: Vec<u8>,
    ) -> Result<Self, String> {
        let mut reader = Self::prepare(master, capacity, pending)?;
        if let Some(start) = reader.task.start.take() {
            let _ = start.send(());
        }
        Ok(reader)
    }

    /// Register without reading until the renderer commits custody and makes
    /// the shared PTY file description nonblocking.
    pub(super) fn prepare(
        master: &PtyMaster,
        capacity: usize,
        pending: Vec<u8>,
    ) -> Result<Self, String> {
        let file = master
            .duplicate()
            .map(File::from)
            .map_err(|error| format!("duplicate PTY descriptor for reading: {error}"))?;
        Self::prepare_file(file, capacity, pending)
    }

    pub(super) fn prepare_file(
        file: File,
        capacity: usize,
        pending: Vec<u8>,
    ) -> Result<Self, String> {
        let runtime = super::super::reactor::runtime()?;
        let master = {
            let _entered = runtime.enter();
            AsyncFd::with_interest(file, Interest::READABLE)
                .map_err(|error| format!("watch PTY readability: {error}"))?
        };
        let (sender, output) = crossbeam_channel::bounded(capacity.max(1));
        if !pending.is_empty() {
            let _ = sender.try_send(PtyMessage::Bytes(pending));
        }
        #[cfg(any(test, feature = "test-support"))]
        let test_queue = sender.clone();
        let (done_sender, done) = crossbeam_channel::bounded(0);
        let (start, started) = oneshot::channel();
        let (stop, stopped) = oneshot::channel();
        let flow = Arc::new(Flow::default());
        let reader_flow = Arc::clone(&flow);
        let reader = runtime.spawn(async move {
            // Drop the descriptor before releasing a renderer that needs
            // proof this reader can consume nothing else during a carry.
            let _finished = done_sender;
            let master = master;
            if started.await.is_ok() {
                read_pty(&master, sender, stopped, &reader_flow).await;
            }
        });
        #[cfg(test)]
        let id = reader.id();
        drop(reader);
        Ok(Self {
            task: Task {
                #[cfg(test)]
                id,
                start: Some(start),
                stop: Some(stop),
                done,
                flow,
            },
            output,
            #[cfg(any(test, feature = "test-support"))]
            test_queue,
        })
    }
}

async fn read_pty(
    master: &AsyncFd<File>,
    sender: Sender<PtyMessage>,
    mut stopped: oneshot::Receiver<()>,
    flow: &Flow,
) {
    loop {
        // Only this task enqueues while active. Reserving space by checking
        // the bounded inbox before read leaves backpressured bytes in the
        // kernel, so a later owner can read them after an exact cut.
        if sender.is_full() && !flow.discard.load(Ordering::Acquire) {
            tokio::select! {
                biased;
                _ = &mut stopped => return,
                _ = flow.capacity.notified() => continue,
            }
        }
        let ready = tokio::select! {
            biased;
            _ = &mut stopped => return,
            ready = master.readable() => ready,
        };
        let message = match ready {
            Ok(mut ready) => match ready.try_io(|fd| read_message(fd.get_ref())) {
                Ok(Ok(message)) => message,
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                // The original readiness may have gone before this read.
                Err(_would_block) => continue,
                Ok(Err(error)) => PtyMessage::Error(error.to_string()),
            },
            Err(error) => PtyMessage::Error(error.to_string()),
        };
        let finished = !matches!(message, PtyMessage::Bytes(_));
        if !flow.discard.load(Ordering::Acquire) && sender.try_send(message).is_err() {
            return;
        }
        if finished {
            return;
        }
        // A chatty PTY shares its executor with holder-control observers.
        tokio::task::consume_budget().await;
    }
}

pub(super) fn read_message(mut file: &File) -> io::Result<PtyMessage> {
    BUFFER.with_borrow_mut(|buffer| match file.read(buffer) {
        Ok(0) => Ok(PtyMessage::Eof),
        Ok(read) => Ok(PtyMessage::Bytes(buffer[..read].to_vec())),
        Err(error) if error.raw_os_error() == Some(libc::EIO) => Ok(PtyMessage::Eof),
        Err(error) => Err(error),
    })
}
