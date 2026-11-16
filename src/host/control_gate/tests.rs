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

/// Starts an exclusive request on its own thread and returns once it waits.
fn waiting_exclusive(gate: &Arc<ControlGate>, patience: Duration) -> mpsc::Receiver<bool> {
    let (sender, receiver) = mpsc::channel();
    let waiter = Arc::clone(gate);
    thread::spawn(move || {
        let admitted = waiter.exclusive_until(Instant::now() + patience).is_some();
        let _ = sender.send(admitted);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while gate.try_shared().is_some() {
        assert!(
            Instant::now() < deadline,
            "the exclusive request never waited"
        );
        thread::sleep(Duration::from_millis(1));
    }
    receiver
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
    let gate = Arc::new(ControlGate::default());
    let exclusive = gate.exclusive();
    let (sender, typed) = mpsc::channel();
    let typist = Arc::clone(&gate);
    thread::spawn(move || {
        let _io = typist.terminal_io();
        let _ = sender.send(());
    });
    assert!(typed.recv_timeout(Duration::from_millis(100)).is_err());
    drop(exclusive);
    typed.recv_timeout(Duration::from_secs(5)).unwrap();
}

#[test]
fn an_expired_exclusive_request_refuses_and_lets_ordinary_requests_continue() {
    let gate = Arc::new(ControlGate::default());
    let slow = gate.shared();
    let exclusive = waiting_exclusive(&gate, Duration::from_millis(100));
    let (sender, entered) = mpsc::channel();
    let ordinary = Arc::clone(&gate);
    thread::spawn(move || {
        let _shared = ordinary.shared();
        let _ = sender.send(());
    });

    assert!(!exclusive.recv_timeout(Duration::from_secs(5)).unwrap());
    entered
        .recv_timeout(Duration::from_secs(5))
        .expect("an ordinary request stayed held back after the exclusive one gave up");
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
