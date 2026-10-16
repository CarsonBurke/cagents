use super::*;
use std::{io::Write, os::fd::FromRawFd};

/// Real original sockets and a real pidfd for this still-live test process;
/// no child, provider, daemon or synthetic exit-authority is created here.
struct Stream {
    watch: Option<Watch>,
    release: Option<std::io::PipeWriter>,
    peer: Option<UnixStream>,
    owner: UnixStream,
    exits: Receiver<ChildExit>,
    acknowledgements: Receiver<Result<(), String>>,
    cleanup: Receiver<()>,
}
impl Stream {
    fn start() -> Self {
        let (owner, peer) = UnixStream::pair().unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        // SAFETY: pidfd_open captures this current process, never signals it.
        let root = unsafe { libc::syscall(libc::SYS_pidfd_open, std::process::id(), 0) };
        assert!(root >= 0, "{}", io::Error::last_os_error());
        let root = unsafe { File::from_raw_fd(root as i32) };
        let (released, release) = std::io::pipe().unwrap();
        let (exits, received) = crossbeam_channel::bounded(2);
        let (acknowledge, acknowledgements) = crossbeam_channel::bounded(1);
        let (cleanup, cleanup_received) = crossbeam_channel::bounded(1);
        let watch = Watch::start(
            owner.try_clone().unwrap(),
            root,
            released,
            exits,
            acknowledge,
            cleanup,
            false,
        )
        .unwrap();
        Self {
            watch: Some(watch),
            release: Some(release),
            peer: Some(peer),
            owner,
            exits: received,
            acknowledgements,
            cleanup: cleanup_received,
        }
    }
    fn send(&self, report: Report) {
        super::super::send(self.peer.as_ref().unwrap(), &report, &[]).unwrap();
    }
    fn stop(&mut self) -> bool {
        drop(self.release.take());
        if let Some(watch) = self.watch.take() {
            let stopped = matches!(
                watch.done.recv_timeout(RELEASE_GRACE),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected)
            );
            watch.stop();
            stopped
        } else {
            true
        }
    }
    fn blocking_flags(&self) -> i32 {
        // SAFETY: owner remains a live descriptor for the call.
        let flags = unsafe { libc::fcntl(self.owner.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        flags & libc::O_NONBLOCK
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[test]
fn thirty_original_holder_streams_share_one_io_thread_and_keep_control_writes_blocking() {
    let mut streams = Vec::new();
    for index in 0..30 {
        let stream = Stream::start();
        assert_eq!(stream.blocking_flags(), 0);
        stream.send(Report::Failed(format!("stream-{index}")));
        streams.push(stream);
    }
    for (index, stream) in streams.iter().enumerate() {
        assert_eq!(
            stream
                .acknowledgements
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            Err(format!("stream-{index}"))
        );
        assert_eq!(
            stream.blocking_flags(),
            0,
            "observer must not change shared OFD flags"
        );
        assert!(stream.exits.try_recv().is_err());
    }
    let threads = std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|task| {
            std::fs::read_to_string(task.path().join("comm"))
                .is_ok_and(|name| name.trim() == "terminal-io")
        })
        .count();
    assert_eq!(
        threads, 1,
        "30 live control observers must use one I/O thread"
    );
    let began = Instant::now();
    for stream in &mut streams {
        assert!(stream.stop(), "observer must stop without abort fallback");
    }
    assert!(began.elapsed() < Duration::from_secs(2));
}

#[test]
fn partial_holder_report_yields_to_other_streams_and_is_cancelled_by_original_owner() {
    let mut stalled = Stream::start();
    let mut active = Stream::start();
    let body = serde_json::to_vec(&Report::ScreenCached).unwrap();
    let length = (body.len() as u32).to_le_bytes();
    stalled
        .peer
        .as_ref()
        .unwrap()
        .write_all(&length[..2])
        .unwrap();
    active.send(Report::AdoptionCommitted);
    assert_eq!(
        active
            .acknowledgements
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        Ok(())
    );
    let start = Instant::now();
    assert!(
        stalled.stop(),
        "partial frame cancellation must complete before fallback"
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    active.send(Report::ScreenCached);
    assert_eq!(
        active
            .acknowledgements
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        Ok(())
    );
    assert_eq!(active.blocking_flags(), 0);
    assert!(active.stop());
}

#[test]
fn original_control_eof_never_reports_a_still_live_root_as_exited() {
    let mut stream = Stream::start();
    drop(stream.peer.take());
    assert!(matches!(
        stream.exits.recv_timeout(Duration::from_millis(100)),
        Err(crossbeam_channel::RecvTimeoutError::Timeout)
    ));
    assert!(stream.stop());
    assert!(matches!(
        stream.cleanup.try_recv(),
        Err(crossbeam_channel::TryRecvError::Disconnected)
    ));
}

#[test]
fn actual_framed_exit_is_published_once_before_detach_ack() {
    let mut stream = Stream::start();
    stream.send(Report::Exited {
        wait_status: 7 << 8,
    });
    assert_eq!(
        stream
            .exits
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .exit_code(),
        7
    );
    stream.send(Report::Exited {
        wait_status: 7 << 8,
    });
    stream.send(Report::Detached);
    assert_eq!(
        stream
            .acknowledgements
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        Ok(())
    );
    assert!(
        matches!(
            stream
                .watch
                .as_ref()
                .unwrap()
                .done
                .recv_timeout(Duration::from_secs(2)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        ),
        "detach ACK must be followed by observer completion, not timeout"
    );
    assert!(
        stream.exits.try_recv().is_err(),
        "duplicate Exited must not publish twice"
    );
    assert!(stream.stop());
    assert!(matches!(
        stream.cleanup.try_recv(),
        Err(crossbeam_channel::TryRecvError::Disconnected)
    ));
}

#[test]
fn only_cleanup_proved_reaches_the_dedicated_family_receipt_channel() {
    let mut stream = Stream::start();
    for report in [Report::ScreenCached, Report::AdoptionCommitted] {
        stream.send(report);
        assert_eq!(
            stream
                .acknowledgements
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            Ok(())
        );
        assert!(stream.cleanup.try_recv().is_err());
    }
    stream.send(Report::Exited { wait_status: 0 });
    assert_eq!(
        stream
            .exits
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .exit_code(),
        0
    );
    stream.send(Report::Failed("pre-proof barrier".into()));
    assert_eq!(
        stream
            .acknowledgements
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        Err("pre-proof barrier".into())
    );
    assert!(
        stream.cleanup.try_recv().is_err(),
        "ordinary ACK and root exit are not family proof"
    );

    stream.send(Report::CleanupProved);
    stream.cleanup.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        stream
            .watch
            .as_ref()
            .unwrap()
            .done
            .recv_timeout(Duration::from_secs(2)),
        Err(crossbeam_channel::RecvTimeoutError::Disconnected)
    ));
    assert!(
        stream.acknowledgements.try_recv().is_err(),
        "proof cannot masquerade as an ordinary ACK"
    );
    assert!(
        stream.exits.try_recv().is_err(),
        "proof must not fabricate another root exit"
    );
    assert!(
        matches!(
            stream.cleanup.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected)
        ),
        "proof is published once"
    );
    assert!(stream.stop());
}

#[test]
fn root_exit_followed_by_control_eof_never_proves_family_cleanup() {
    let mut stream = Stream::start();
    stream.send(Report::Exited { wait_status: 0 });
    stream
        .exits
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    drop(stream.peer.take());
    assert!(matches!(
        stream
            .watch
            .as_ref()
            .unwrap()
            .done
            .recv_timeout(Duration::from_secs(2)),
        Err(crossbeam_channel::RecvTimeoutError::Disconnected)
    ));
    assert!(
        matches!(
            stream.cleanup.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected)
        ),
        "EOF after a genuine framed root exit is still not the dedicated family report"
    );
    assert!(stream.stop());
}

#[test]
fn truncated_cleanup_report_is_cancelled_without_publishing_proof() {
    let mut stream = Stream::start();
    let body = serde_json::to_vec(&Report::CleanupProved).unwrap();
    let mut peer = stream.peer.take().unwrap();
    peer.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
    peer.write_all(&body[..body.len() - 1]).unwrap();
    drop(peer);
    assert!(
        stream.stop(),
        "original release must finish partial-frame observation"
    );
    assert!(matches!(
        stream.cleanup.try_recv(),
        Err(crossbeam_channel::TryRecvError::Disconnected)
    ));
    assert!(stream.acknowledgements.try_recv().is_err());
    assert!(stream.exits.try_recv().is_err());
}

#[test]
fn oversized_private_report_is_not_root_exit_authority() {
    let mut stream = Stream::start();
    stream
        .peer
        .as_ref()
        .unwrap()
        .write_all(&(MAX_FRAME + 1).to_le_bytes())
        .unwrap();
    assert!(matches!(
        stream.exits.recv_timeout(Duration::from_millis(100)),
        Err(crossbeam_channel::RecvTimeoutError::Timeout)
    ));
    assert!(stream.stop());
    assert!(matches!(
        stream.cleanup.try_recv(),
        Err(crossbeam_channel::TryRecvError::Disconnected)
    ));
}
