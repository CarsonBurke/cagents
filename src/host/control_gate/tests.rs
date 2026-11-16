use super::ControlGate;
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
    thread::sleep(Duration::from_millis(60));
    assert!(gate.try_exclusive().is_none(), "the readers overlap");

    let started = Instant::now();
    let admitted = gate.exclusive_until(started + Duration::from_secs(5));
    assert!(
        admitted.is_some(),
        "steady reader traffic starved the writer"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(admitted);

    running.store(false, Ordering::Relaxed);
    for reader in readers {
        reader.join().unwrap();
    }
    typist.join().unwrap();
}
