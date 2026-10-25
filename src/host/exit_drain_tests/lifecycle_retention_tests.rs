//! The actual lifecycle idle edge must enter the ordinary original-child drain.
use super::*;
use std::sync::Mutex;

struct IdleLatch {
    arrived: mpsc::SyncSender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

static IDLE_LATCH: OnceLock<IdleLatch> = OnceLock::new();

/// Only the isolated regression arms this bounded scheduling point. Other
/// tests and ordinary HOST execution do not wait or change shutdown state.
pub(in crate::host) fn observe_lifecycle_idle_shutdown() {
    if let Some(latch) = IDLE_LATCH.get() {
        latch.arrived.try_send(()).unwrap();
        latch
            .release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .expect("isolated lifecycle idle scheduling point was not released");
    }
}

struct UnpublishedOriginal(Option<Child>);

impl Drop for UnpublishedOriginal {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0
            && !matches!(child.try_wait(), Ok(Some(_)))
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn lifecycle_idle_exit_drains_a_late_original_before_releasing_listener_and_lease() {
    const NAME: &str = "host::exit_drain_tests::lifecycle_retention_tests::lifecycle_idle_exit_drains_a_late_original_before_releasing_listener_and_lease";
    let Some(root) = private_runtime(NAME) else {
        return;
    };
    assert!(crate::instance::IS_DEVELOPMENT);
    // SAFETY: this exact self-exec runs one test. Configure its private HOST
    // before starting any HOST/worker thread; inherited authority is untouched.
    unsafe {
        std::env::set_var("CAGENTS_DEVELOPMENT_IDLE_TIMEOUT_SECS", "0");
    }
    let (arrived, observed) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    assert!(
        IDLE_LATCH
            .set(IdleLatch {
                arrived,
                release: Mutex::new(released),
            })
            .is_ok()
    );
    let paths = RuntimePaths::discover().unwrap();
    let (host, completed, running) = run_host();
    observed
        .recv_timeout(Duration::from_secs(5))
        .expect("the actual Lifecycle must request idle shutdown");
    {
        let state = host.lock().unwrap();
        assert!(state.is_shutting_down());
        assert!(!state.ordinary_exit_cleanup_pending());
        assert!(state.terminals.is_empty() && state.clients.is_empty());
    }
    assert!(!child_reaper::has_outstanding_ownership());
    // A preexisting ticket would correctly prevent idle. Admit this original
    // AFTER Lifecycle's real idle latch, BEFORE ordinary exit seals admission.
    let ticket = child_reaper::reserve().unwrap();
    let mut child = UnpublishedOriginal(Some(
        Command::new("/bin/cat")
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let stdin = child.0.as_mut().unwrap().stdin.take().unwrap();
    let mut original = OriginalPin::new(child.0.as_ref().unwrap().id());
    assert!(!child_reaper::has_retained_ownership());
    release.send(()).unwrap();
    wait_for(|| host.lock().unwrap().ordinary_exit_cleanup_pending());
    assert!(child_reaper::reserve().is_err());
    assert!(completed.try_recv().is_err());
    assert!(original.process.is_current() && !original.exited());
    assert!(paths.socket.exists());
    assert_lease_held(&paths);

    // Fresh public framing must still reach the pending listener. There was no
    // listed terminal at idle, so do not invent a successful terminal Close.
    let mut fresh = Rpc::connect(&paths.socket);
    assert!(
        matches!(fresh.request(ClientRequest::List).unwrap(), ServerResponse::Listed { terminals } if terminals.is_empty())
    );
    let unknown = TerminalId::new();
    let refusal = fresh
        .request(ClientRequest::CloseTerminalChecked {
            terminal_id: unknown,
            allow_active: true,
        })
        .unwrap_err();
    assert_eq!(refusal.code, ProtocolErrorCode::PermissionDenied);
    assert!(refusal.message.contains(&unknown.to_string()));
    assert!(refusal.message.contains("not attached"));
    assert!(host.lock().unwrap().terminals.is_empty());
    assert!(original.process.is_current() && !original.exited());
    assert!(completed.try_recv().is_err());

    // Publish the SAME reserved original after the actual idle-triggered drain
    // began. Neither an empty terminal map nor a refused Close proves absence.
    ticket.quarantine();
    ticket
        .retain(child.0.take().unwrap(), original.pidfd.try_clone().unwrap())
        .unwrap();
    assert!(child_reaper::has_retained_ownership());
    assert_lease_held(&paths);
    assert!(
        matches!(fresh.request(ClientRequest::List).unwrap(), ServerResponse::Listed { terminals } if terminals.is_empty())
    );
    assert!(completed.try_recv().is_err());
    drop(stdin); // actual cat EOF; no signal, fake receipt, or state-helper exit
    assert!(
        completed
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .is_ok()
    );
    // Keep the RPC open through return: client disconnect cannot supply the
    // wake or conceal a missing sealed-delivery fence.
    running.join().unwrap();
    original.prove_reaped();
    assert!(!child_reaper::has_outstanding_ownership());
    assert!(!paths.socket.exists());
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&paths.lock)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "actual host::run must release its HostLease after original reap"
    );
    drop(fresh);
}
