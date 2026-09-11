#![cfg(feature = "host")]

use std::{
    collections::BTreeMap,
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver as WakeReceiver, SyncSender, TryRecvError, sync_channel},
    },
    time::{Duration, Instant},
};

use cagents::{
    model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal::{TerminalConfig, TerminalEvent, TerminalHandle, TerminalWake},
};
use crossbeam_channel::Receiver as EventReceiver;

const FAILURE_GUARD: Duration = Duration::from_secs(5);
const CONTROLLED_SHELL: &str = r#"
printf 'READY\n\033]0;ready\007'
while IFS= read -r step; do
    printf '%s\n\033]0;%s\007' "$step" "$step"
done
"#;

struct RunningTerminal(TerminalHandle);

impl Deref for RunningTerminal {
    type Target = TerminalHandle;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for RunningTerminal {
    fn drop(&mut self) {
        let _ = self.0.shutdown();
    }
}

fn terminal_session(script: &str) -> TerminalSession {
    let launch = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), script.into()],
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };

    TerminalSession {
        id: TerminalId::new(),
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: None,
        },
    }
}

fn spawn_with_probe(
    initially_visible: bool,
) -> (
    RunningTerminal,
    EventReceiver<TerminalEvent>,
    Arc<AtomicUsize>,
    WakeReceiver<TerminalWake>,
) {
    let callbacks = Arc::new(AtomicUsize::new(0));
    let callback_count = Arc::clone(&callbacks);
    let (wake_sender, wakes) = sync_channel(16);
    let handle = TerminalHandle::spawn(
        terminal_session(CONTROLLED_SHELL),
        TerminalConfig {
            initially_visible,
            ..TerminalConfig::default()
        },
        move |wake| {
            callback_count.fetch_add(1, Ordering::SeqCst);
            let _ = wake_sender.try_send(wake);
        },
    )
    .expect("controlled terminal should start");
    let events = handle.events();

    (RunningTerminal(handle), events, callbacks, wakes)
}

fn send_step(handle: &TerminalHandle, step: &str) {
    handle
        .input(format!("{step}\n").into_bytes())
        .expect("terminal should accept the synchronization step");
}

fn wait_for_title(events: &EventReceiver<TerminalEvent>, expected: &str) {
    let deadline = Instant::now() + FAILURE_GUARD;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for title {expected:?}"
        );
        match events.recv_timeout(remaining) {
            Ok(TerminalEvent::TitleChanged(title)) if title == expected => return,
            Ok(TerminalEvent::TitleChanged(_))
            | Ok(TerminalEvent::ResumeHintChanged(_))
            | Ok(TerminalEvent::AgentActivityChanged(_))
            | Ok(TerminalEvent::Bell) => {}
            Ok(TerminalEvent::Error { operation, message }) => {
                panic!("terminal error during {operation}: {message}")
            }
            Ok(TerminalEvent::Exited {
                exit_code, signal, ..
            }) => panic!(
                "terminal exited before title {expected:?}: code={exit_code}, signal={signal:?}"
            ),
            Err(error) => panic!("failed waiting for title {expected:?}: {error}"),
        }
    }
}

fn receive_wake(wakes: &WakeReceiver<TerminalWake>, context: &str) -> TerminalWake {
    wakes
        .recv_timeout(FAILURE_GUARD)
        .unwrap_or_else(|error| panic!("{context}: {error}"))
}

fn assert_no_queued_wake(wakes: &WakeReceiver<TerminalWake>, context: &str) {
    match wakes.try_recv() {
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => panic!("{context}: wake callback disconnected"),
        Ok(wake) => panic!(
            "{context}: unexpected redraw wake for generation {}",
            wake.generation
        ),
    }
}

fn snapshot_text(handle: &TerminalHandle) -> String {
    handle
        .snapshot()
        .lines
        .iter()
        .map(|row| {
            row.cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn interactive_color_survives_launcher_ci_but_respects_explicit_policy() {
    const CHILD: &str = "CAGENTS_COLOR_CONTRACT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Isolate daemon inheritance without mutating this test process's
        // environment while other terminals/tests may be running.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "interactive_color_survives_launcher_ci_but_respects_explicit_policy",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("CI", "true")
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    // Common color detectors check CI/NO_COLOR before TERM/COLORTERM.
    // Exercise that decision inside the real PTY, through parser and snapshot.
    let script = r#"
if [ -t 1 ] && [ "${CI+x}" != x ] && [ "${NO_COLOR+x}" != x ] &&
   [ "$TERM" != dumb ] && [ "$COLORTERM" = truecolor ]; then
    printf '\033[38;2;18;52;86mX\033[0m'
else
    printf X
fi
printf '\033]0;ready\007'
IFS= read -r line
"#;
    for (policy, colored) in [
        (None, true),
        (Some(("CI", "true")), false),
        (Some(("NO_COLOR", "1")), false),
        (Some(("TERM", "dumb")), false),
    ] {
        let mut session = terminal_session(script);
        if let Some((name, value)) = policy {
            session.launch.environment.insert(name.into(), value.into());
        }
        let (wake_sender, wakes) = sync_channel(16);
        let terminal = RunningTerminal(
            TerminalHandle::spawn(session, TerminalConfig::default(), move |wake| {
                let _ = wake_sender.try_send(wake);
            })
            .unwrap(),
        );
        wait_for_title(&terminal.events(), "ready");
        let deadline = Instant::now() + FAILURE_GUARD;
        let snapshot = loop {
            let snapshot = terminal.snapshot();
            if snapshot.lines[0].cells[0].text == "X" {
                break snapshot;
            }
            terminal.acknowledge_redraw(snapshot.generation);
            wakes
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("color output was not captured");
        };
        let foreground = snapshot.lines[0].cells[0].foreground;
        if colored {
            assert_eq!(
                (foreground.red, foreground.green, foreground.blue),
                (18, 52, 86)
            );
        } else {
            assert_eq!(foreground, snapshot.foreground, "{policy:?}");
        }
    }
}

#[test]
fn visible_wakes_are_bounded_until_acknowledged_and_then_rearm() {
    let (terminal, events, callbacks, wakes) = spawn_with_probe(true);

    let initial = receive_wake(&wakes, "initial visible snapshot did not wake");
    wait_for_title(&events, "ready");
    send_step(&terminal, "one");
    wait_for_title(&events, "one");

    assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    assert_no_queued_wake(
        &wakes,
        "visible publications must coalesce behind an unacknowledged edge",
    );

    let first_observed_generation = terminal.snapshot().generation;
    assert!(first_observed_generation >= initial.generation);
    terminal.acknowledge_redraw(first_observed_generation);
    send_step(&terminal, "two");
    wait_for_title(&events, "two");

    let rearmed = receive_wake(&wakes, "acknowledgement did not rearm redraw delivery");
    assert!(rearmed.generation > initial.generation);
    assert_eq!(callbacks.load(Ordering::SeqCst), 2);

    for step in ["three", "four", "barrier"] {
        send_step(&terminal, step);
        wait_for_title(&events, step);
    }
    assert_eq!(callbacks.load(Ordering::SeqCst), 2);
    assert_no_queued_wake(
        &wakes,
        "additional visible output must remain coalesced while a wake is pending",
    );

    let latest_observed_generation = terminal.snapshot().generation;
    // Capture itself, not just wake delivery, remains bounded by credit.
    assert_eq!(latest_observed_generation, rearmed.generation);
    send_step(&terminal, "final");
    wait_for_title(&events, "final");
    // The title follows the output in the fixture, so all final bytes are
    // parsed before we request the next useful capture.
    terminal.acknowledge_redraw(latest_observed_generation);

    let final_wake = receive_wake(&wakes, "a second acknowledgement did not rearm delivery");
    assert!(final_wake.generation > rearmed.generation);
    assert_eq!(callbacks.load(Ordering::SeqCst), 3);
    assert_no_queued_wake(&wakes, "one output burst must produce one redraw edge");
    let final_text = snapshot_text(&terminal);
    for expected in ["READY", "one", "two", "three", "four", "barrier", "final"] {
        assert!(
            final_text.contains(expected),
            "missing {expected:?}: {final_text:?}"
        );
    }

    terminal
        .shutdown()
        .expect("terminal should shut down cleanly");
}

#[test]
fn hidden_output_updates_terminal_state_without_redraw_until_reveal() {
    let (terminal, events, callbacks, wakes) = spawn_with_probe(true);

    let initial = receive_wake(&wakes, "initial visible snapshot did not wake");
    wait_for_title(&events, "ready");
    terminal.acknowledge_redraw(initial.generation);
    let ready = receive_wake(&wakes, "returning credit did not capture ready output");
    assert!(snapshot_text(&terminal).contains("READY"));
    let visible_callbacks = callbacks.load(Ordering::SeqCst);
    terminal.hide().expect("terminal should hide");
    send_step(&terminal, "HIDDEN_ONE");
    wait_for_title(&events, "HIDDEN_ONE");

    let hidden_snapshot = terminal.snapshot();
    assert_eq!(hidden_snapshot.generation, ready.generation);
    assert!(snapshot_text(&terminal).contains("READY"));
    terminal.acknowledge_redraw(hidden_snapshot.generation);
    assert_eq!(callbacks.load(Ordering::SeqCst), visible_callbacks);
    assert_no_queued_wake(&wakes, "hidden PTY output must not request a redraw");

    send_step(&terminal, "HIDDEN_TWO");
    wait_for_title(&events, "HIDDEN_TWO");
    let still_hidden_snapshot = terminal.snapshot();
    assert!(Arc::ptr_eq(&hidden_snapshot, &still_hidden_snapshot));
    assert_eq!(callbacks.load(Ordering::SeqCst), visible_callbacks);
    assert_no_queued_wake(
        &wakes,
        "continued hidden PTY consumption must not publish rendering edges",
    );

    terminal.reveal().expect("terminal should reveal");
    let reveal = receive_wake(&wakes, "reveal did not publish the current terminal state");
    let revealed_snapshot = terminal.snapshot();
    assert_eq!(revealed_snapshot.generation, reveal.generation);
    assert!(revealed_snapshot.generation > hidden_snapshot.generation);
    let revealed_text = snapshot_text(&terminal);
    assert!(revealed_text.contains("HIDDEN_ONE"), "{revealed_text:?}");
    assert!(revealed_text.contains("HIDDEN_TWO"), "{revealed_text:?}");
    assert_eq!(callbacks.load(Ordering::SeqCst), visible_callbacks + 1);
    assert_no_queued_wake(&wakes, "one reveal must emit one current-snapshot edge");

    terminal
        .shutdown()
        .expect("terminal should shut down cleanly");
}

#[test]
fn hibernated_terminal_preserves_process_output_through_hidden_restore() {
    let (terminal, events, callbacks, wakes) = spawn_with_probe(true);
    let initial = receive_wake(&wakes, "initial terminal snapshot did not wake");
    wait_for_title(&events, "ready");
    terminal.acknowledge_redraw(initial.generation);
    receive_wake(&wakes, "ready output was not captured");
    let visible_callbacks = callbacks.load(Ordering::SeqCst);

    terminal.hibernate().unwrap();
    send_step(&terminal, "WHILE_HIBERNATED");
    wait_for_title(&events, "WHILE_HIBERNATED");
    let detached = terminal.snapshot();
    assert!(detached.detached);
    assert_eq!(callbacks.load(Ordering::SeqCst), visible_callbacks);

    terminal.restore(false).unwrap();
    send_step(&terminal, "AFTER_HIDDEN_RESTORE");
    wait_for_title(&events, "AFTER_HIDDEN_RESTORE");
    assert!(Arc::ptr_eq(&detached, &terminal.snapshot()));
    assert_no_queued_wake(&wakes, "hidden restoration must not publish a frame");

    terminal.restore(true).unwrap();
    receive_wake(
        &wakes,
        "visible restoration did not recover terminal output",
    );
    let text = snapshot_text(&terminal);
    for expected in ["READY", "WHILE_HIBERNATED", "AFTER_HIDDEN_RESTORE"] {
        assert!(text.contains(expected), "missing {expected:?}: {text:?}");
    }
    assert_eq!(callbacks.load(Ordering::SeqCst), visible_callbacks + 1);
    assert!(!terminal.snapshot().detached);
}

#[test]
fn many_hidden_terminals_consume_pty_output_without_a_wake_storm() {
    const TERMINAL_COUNT: usize = 12;

    let callbacks = Arc::new(AtomicUsize::new(0));
    let (unexpected_wake_sender, unexpected_wakes) = sync_channel(1);
    let mut terminals = Vec::with_capacity(TERMINAL_COUNT);

    for index in 0..TERMINAL_COUNT {
        let callback_count = Arc::clone(&callbacks);
        let wake_sender: SyncSender<(usize, TerminalWake)> = unexpected_wake_sender.clone();
        let handle = TerminalHandle::spawn(
            terminal_session(CONTROLLED_SHELL),
            TerminalConfig {
                initially_visible: false,
                ..TerminalConfig::default()
            },
            move |wake| {
                callback_count.fetch_add(1, Ordering::SeqCst);
                let _ = wake_sender.try_send((index, wake));
            },
        )
        .expect("hidden controlled terminal should start");
        let events = handle.events();
        terminals.push((RunningTerminal(handle), events));
    }
    drop(unexpected_wake_sender);

    for (index, (terminal, events)) in terminals.iter().enumerate() {
        wait_for_title(events, "ready");
        let payload = format!("hidden-load-{index}");
        send_step(terminal, &payload);
        wait_for_title(events, &payload);
        let barrier = format!("hidden-barrier-{index}");
        send_step(terminal, &barrier);
        wait_for_title(events, &barrier);
    }

    assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    match unexpected_wakes.try_recv() {
        Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        Ok((index, wake)) => panic!(
            "hidden terminal {index} emitted a redraw wake for generation {}",
            wake.generation
        ),
    }

    for (terminal, _) in &terminals {
        terminal
            .shutdown()
            .expect("hidden terminal should shut down cleanly");
    }
}
