use super::{ControlGate, Counts};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// Starts a queued exclusive request on its own thread and returns once it
/// is queued, or once it has already answered.
fn waiting_exclusive(gate: &Arc<ControlGate>, patience: Duration) -> mpsc::Receiver<bool> {
    let (sender, receiver) = mpsc::channel();
    let waiter = Arc::clone(gate);
    let before = gate.counts().queued;
    thread::spawn(move || {
        let admitted = waiter.exclusive_until(Instant::now() + patience).is_some();
        let _ = sender.send(admitted);
    });
    wait_for(
        || gate.counts().queued > before,
        "the exclusive request never queued",
    );
    receiver
}

/// Polls observable gate state; never samples elapsed time.
fn wait_for(condition: impl Fn() -> bool, failure: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "{failure}");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_waiting_exclusive_request_holds_back_ordinary_requests_but_not_terminal_io() {
    let gate = Arc::new(ControlGate::default());
    let slow = gate.shared();
    let exclusive = waiting_exclusive(&gate, Duration::from_secs(30));

    assert!(gate.try_shared().is_none(), "ordinary requests yield");
    let (sender, typed) = mpsc::channel();
    let typist = Arc::clone(&gate);
    thread::spawn(move || {
        let _io = typist.terminal_io();
        let _ = sender.send(());
    });
    typed
        .recv_timeout(Duration::from_secs(2))
        .expect("terminal I/O waited for an exclusive request that was only waiting");

    drop(slow);
    assert!(exclusive.recv_timeout(Duration::from_secs(5)).unwrap());
}

#[test]
fn an_exclusive_holder_excludes_terminal_io_until_it_leaves() {
    // Exclusion itself is the predicate on the counts; the waiter below only
    // shows that releasing the holder wakes a typist that had to wait.
    let gate = Arc::new(ControlGate::default());
    let exclusive = gate.exclusive();
    assert!(!gate.counts().admits_terminal_io());
    assert!(gate.try_shared().is_none());
    let (sender, typed) = mpsc::channel();
    let typist = Arc::clone(&gate);
    thread::spawn(move || {
        let _io = typist.terminal_io();
        let _ = sender.send(());
    });
    drop(exclusive);
    typed
        .recv_timeout(Duration::from_secs(5))
        .expect("terminal I/O stayed out after the exclusive holder left");
}

#[test]
fn an_expired_exclusive_request_refuses_and_lets_ordinary_requests_continue() {
    let gate = Arc::new(ControlGate::default());
    let slow = gate.shared();
    let (sender, exclusive) = mpsc::channel();
    let waiter = Arc::clone(&gate);
    thread::spawn(move || {
        let admitted = waiter
            .exclusive_until(Instant::now() + Duration::from_millis(200))
            .is_some();
        let _ = sender.send(admitted);
    });
    // Whether this request starts before or after the writer queues, it must
    // be admitted once the writer has given up.
    let (entered_sender, entered) = mpsc::channel();
    let ordinary = Arc::clone(&gate);
    thread::spawn(move || {
        let _shared = ordinary.shared();
        let _ = entered_sender.send(());
    });

    assert!(!exclusive.recv_timeout(Duration::from_secs(5)).unwrap());
    entered
        .recv_timeout(Duration::from_secs(5))
        .expect("an ordinary request stayed held back after the exclusive one gave up");
    assert_eq!(gate.counts().queued, 0);
    assert!(gate.try_shared().is_some());
    drop(slow);
    assert!(gate.try_exclusive().is_some());
}

#[test]
fn an_exclusive_request_is_admitted_under_overlapping_reader_traffic() {
    let gate = Arc::new(ControlGate::default());
    let running = Arc::new(AtomicBool::new(true));
    // Staggered ordinary readers keep the gate shared at every instant, so a
    // writer that only waited for a free gate would never be admitted.
    let readers = (0..3)
        .map(|offset| {
            let (gate, running) = (Arc::clone(&gate), Arc::clone(&running));
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(offset * 10));
                while running.load(Ordering::Relaxed) {
                    let _shared = gate.shared();
                    thread::sleep(Duration::from_millis(30));
                }
            })
        })
        .collect::<Vec<_>>();
    let typist = {
        let (gate, running) = (Arc::clone(&gate), Arc::clone(&running));
        thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                drop(gate.terminal_io());
                thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let admitted = gate.exclusive_until(Instant::now() + Duration::from_secs(10));
    assert!(
        admitted.is_some(),
        "steady reader traffic starved the writer"
    );
    drop(admitted);

    running.store(false, Ordering::Relaxed);
    for reader in readers {
        reader.join().unwrap();
    }
    typist.join().unwrap();
}

#[test]
fn a_queued_exclusive_request_takes_the_first_empty_gate() {
    let queued = |shared| Counts {
        shared,
        queued: 1,
        ..Counts::default()
    };
    // Terminal I/O passes the requests a queued writer waits for...
    assert!(queued(1).admits_terminal_io());
    assert!(!queued(1).admits_ordinary());
    // ...but not the writer itself once the gate empties, so a keystroke
    // arriving between the last release and the writer's wakeup cannot take
    // the gate first.
    assert!(!queued(0).admits_terminal_io());
    assert!(queued(0).admits_exclusive());
    let held = Counts {
        exclusive: true,
        ..Counts::default()
    };
    assert!(!held.admits_terminal_io() && !held.admits_ordinary());
}

#[test]
fn an_unqueued_exclusive_request_holds_back_nobody_and_takes_a_free_gate() {
    let gate = Arc::new(ControlGate::default());
    let slow = gate.shared();
    let (sender, admitted) = mpsc::channel();
    let closer = Arc::clone(&gate);
    thread::spawn(move || {
        let admission = closer.exclusive_when_free_until(Instant::now() + Duration::from_secs(30));
        let _ = sender.send(admission.is_some());
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while gate.counts().unqueued == 0 {
        assert!(Instant::now() < deadline, "the close never waited");
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        gate.try_shared().is_some(),
        "an ordinary request waited for a close"
    );
    drop(gate.terminal_io());
    assert_eq!(
        gate.counts().unqueued,
        1,
        "the close entered beside a shared holder"
    );

    drop(slow);
    assert!(admitted.recv_timeout(Duration::from_secs(5)).unwrap());
}

#[test]
fn an_unqueued_exclusive_request_refuses_at_its_deadline() {
    let gate = ControlGate::default();
    let _slow = gate.shared();
    assert!(
        gate.exclusive_when_free_until(Instant::now() + Duration::from_millis(20))
            .is_none()
    );
    assert_eq!(gate.counts().unqueued, 0);
    assert!(gate.try_shared().is_some());
}

/// A writer that records how many writers are inside at once.
fn writer(
    gate: &Arc<ControlGate>,
    inside: &Arc<std::sync::atomic::AtomicUsize>,
    patience: Duration,
    queue: bool,
) -> mpsc::Receiver<bool> {
    let (sender, receiver) = mpsc::channel();
    let (gate, inside) = (Arc::clone(gate), Arc::clone(inside));
    thread::spawn(move || {
        let deadline = Instant::now() + patience;
        let admission = if queue {
            gate.exclusive_until(deadline)
        } else {
            gate.exclusive_when_free_until(deadline)
        };
        let Some(_admission) = admission else {
            let _ = sender.send(false);
            return;
        };
        let alone = inside.fetch_add(1, Ordering::SeqCst) == 0 && gate.counts().shared == 0;
        thread::sleep(Duration::from_millis(10));
        inside.fetch_sub(1, Ordering::SeqCst);
        let _ = sender.send(alone);
    });
    receiver
}

#[test]
fn queued_exclusive_requests_are_each_admitted_alone() {
    let gate = Arc::new(ControlGate::default());
    let inside = Arc::default();
    let slow = gate.shared();
    let writers = (0..3)
        .map(|_| writer(&gate, &inside, Duration::from_secs(30), true))
        .collect::<Vec<_>>();
    wait_for(|| gate.counts().queued == 3, "the writers never queued");
    assert!(gate.try_shared().is_none());
    drop(slow);
    for admitted in writers {
        assert!(admitted.recv_timeout(Duration::from_secs(10)).unwrap());
    }
    assert!(gate.try_shared().is_some());
}

#[test]
fn a_writer_that_gives_up_leaves_the_other_queued_writer_in_place() {
    let gate = Arc::new(ControlGate::default());
    let slow = gate.shared();
    let patient = waiting_exclusive(&gate, Duration::from_secs(30));
    let (sender, impatient) = mpsc::channel();
    let waiter = Arc::clone(&gate);
    thread::spawn(move || {
        let admitted = waiter
            .exclusive_until(Instant::now() + Duration::from_millis(100))
            .is_some();
        let _ = sender.send(admitted);
    });
    assert!(!impatient.recv_timeout(Duration::from_secs(5)).unwrap());
    assert_eq!(gate.counts().queued, 1);
    assert!(
        gate.try_shared().is_none(),
        "ordinary requests stopped yielding to the writer still queued"
    );
    drop(slow);
    assert!(patient.recv_timeout(Duration::from_secs(5)).unwrap());
}

#[test]
fn a_panic_inside_any_admission_releases_it() {
    let gate = Arc::new(ControlGate::default());
    let holders: [fn(&ControlGate); 3] = [
        |gate| {
            let _shared = gate.shared();
            panic!("ordinary holder failed");
        },
        |gate| {
            let _io = gate.terminal_io();
            panic!("terminal I/O failed");
        },
        |gate| {
            let _exclusive = gate.exclusive();
            panic!("exclusive holder failed");
        },
    ];
    for holder in holders {
        let failing = Arc::clone(&gate);
        assert!(thread::spawn(move || holder(&failing)).join().is_err());
        let counts = gate.counts();
        assert_eq!((counts.shared, counts.exclusive), (0, false));
        drop(counts);
        assert!(gate.try_exclusive().is_some());
    }
}

#[test]
fn a_queued_writer_is_admitted_alone_beside_steady_terminal_io() {
    let gate = Arc::new(ControlGate::default());
    let inside = Arc::default();
    let running = Arc::new(AtomicBool::new(true));
    let entered = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let slow = gate.shared();
    let admitted = writer(&gate, &inside, Duration::from_secs(30), true);
    wait_for(|| gate.counts().queued == 1, "the writer never queued");
    // Typists keep entering on real threads while the writer waits and after
    // the gate empties; `a_queued_exclusive_request_takes_the_first_empty_gate`
    // pins the handoff rule itself without depending on scheduling.
    let typists = (0..4)
        .map(|_| {
            let (gate, running, entered) = (
                Arc::clone(&gate),
                Arc::clone(&running),
                Arc::clone(&entered),
            );
            thread::spawn(move || {
                while running.load(Ordering::Relaxed) {
                    drop(gate.terminal_io());
                    entered.fetch_add(1, Ordering::Relaxed);
                    thread::sleep(Duration::from_millis(1));
                }
            })
        })
        .collect::<Vec<_>>();
    wait_for(
        || entered.load(Ordering::Relaxed) > 0,
        "terminal I/O waited for a writer that was only queued",
    );
    drop(slow);
    let alone = admitted.recv_timeout(Duration::from_secs(10));
    running.store(false, Ordering::Relaxed);
    for typist in typists {
        typist.join().unwrap();
    }
    assert!(
        matches!(alone, Ok(true)),
        "the writer was kept out or not alone: {alone:?}"
    );
}

#[test]
fn queued_and_unqueued_writers_competing_are_each_admitted_alone() {
    let gate = Arc::new(ControlGate::default());
    let inside = Arc::default();
    let slow = gate.shared();
    let closes = (0..2)
        .map(|_| writer(&gate, &inside, Duration::from_secs(30), false))
        .collect::<Vec<_>>();
    let upgrade = writer(&gate, &inside, Duration::from_secs(30), true);
    wait_for(
        || {
            let counts = gate.counts();
            counts.unqueued == 2 && counts.queued == 1
        },
        "the writers never waited",
    );
    // The queued writer holds back ordinary requests; terminal I/O still runs.
    assert!(gate.try_shared().is_none());
    drop(gate.terminal_io());
    drop(slow);
    assert!(upgrade.recv_timeout(Duration::from_secs(10)).unwrap());
    for close in closes {
        assert!(close.recv_timeout(Duration::from_secs(10)).unwrap());
    }
    let counts = gate.counts();
    assert_eq!(
        (
            counts.shared,
            counts.exclusive,
            counts.queued,
            counts.unqueued
        ),
        (0, false, 0, 0)
    );
}
