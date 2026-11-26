use std::{io, os::unix::net::UnixStream};

use super::*;
use crate::{
    agent::ActivityState,
    mobile::{Lines, Outgoing, write_lines},
    model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe},
    terminal::{
        CellSnapshot, CellStyle, CellText, RowSnapshot,
        ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
            ServerResponse, TerminalDescriptor, TerminalSnapshot, TerminalVisibility,
            WireTerminalEvent, read_frame, write_frame,
        },
    },
};

const WAIT: Duration = Duration::from_secs(5);

fn row(text: &str) -> Arc<RowSnapshot> {
    Arc::new(RowSnapshot {
        cells: text
            .chars()
            .map(|ch| CellSnapshot {
                text: CellText::new(ch.encode_utf8(&mut [0; 4])),
                ..CellSnapshot::default()
            })
            .collect(),
        wrapped: false,
        hyperlinks: Vec::new(),
    })
}

fn frame(generation: u64, rows: &[&str]) -> RenderSnapshot {
    RenderSnapshot {
        generation,
        columns: 80,
        rows: rows.len() as u16,
        lines: rows.iter().map(|text| row(text)).collect::<Vec<_>>().into(),
        ..RenderSnapshot::empty()
    }
}

#[test]
fn screen_clips_older_rows_and_long_unicode_lines_without_rendering_controls() {
    let long = "界".repeat(MAX_ROW_BYTES / 3 + 1);
    let mut rows = vec!["older"; MAX_ROWS + 1];
    for row in rows
        .iter_mut()
        .rev()
        .take(MAX_SCREEN_BYTES / MAX_ROW_BYTES + 2)
    {
        *row = &long;
    }
    *rows.last_mut().unwrap() = "  tail\t   ";
    let frame = frame(1, &rows);
    let screen = screen(TerminalId::new(), &frame, api::Activity::Working, Some(42));
    assert!(screen.clipped);
    assert_eq!(usize::from(screen.rows), rows.len());
    assert_eq!(screen.last_output_at, Some(42));
    assert_eq!(screen.lines.last().unwrap(), "  tail");
    assert!(screen.lines.len() < MAX_ROWS);
    assert!(screen.lines.iter().map(String::len).sum::<usize>() <= MAX_SCREEN_BYTES);
    assert!(screen.lines.iter().all(|line| line.len() <= MAX_ROW_BYTES));
}

#[test]
fn screen_keeps_grid_spacing_and_masks_invisible_cells() {
    let cell = |text: &str| CellSnapshot {
        text: CellText::new(text),
        ..Default::default()
    };
    let mut hidden = cell("s");
    hidden.style = CellStyle {
        invisible: true,
        ..Default::default()
    };
    let row = Arc::new(RowSnapshot {
        cells: vec![
            cell("界"),
            CellSnapshot::default(),
            hidden,
            CellSnapshot::default(),
            cell("x"),
        ],
        ..Default::default()
    });
    let frame = RenderSnapshot {
        columns: 5,
        rows: 1,
        lines: vec![row].into(),
        ..RenderSnapshot::empty()
    };
    assert_eq!(
        screen(TerminalId::new(), &frame, api::Activity::Idle, None).lines,
        ["界  x"]
    );
}

fn next_line(lines: &Lines) -> api::ServerLine {
    match lines
        .outgoing
        .recv_timeout(WAIT)
        .expect("a line for the phone")
    {
        Outgoing::Line(line) => line,
        Outgoing::TerminalScreen(pending) => api::ServerLine::Event(api::Event::TerminalScreen(
            pending.lock().unwrap().take().unwrap(),
        )),
        Outgoing::Exit => panic!("the bridge exited"),
    }
}

fn next_screen(lines: &Lines) -> api::TerminalScreen {
    let api::ServerLine::Event(api::Event::TerminalScreen(screen)) = next_line(lines) else {
        panic!("expected a terminal screen");
    };
    screen
}

struct FakeDaemon {
    stream: UnixStream,
    proof: HostSessionProof,
}

impl FakeDaemon {
    fn receive(&mut self) -> ClientEnvelope {
        read_frame(&mut self.stream).unwrap().expect("a request")
    }

    fn send(&mut self, request_id: Option<u64>, message: ServerMessage) {
        write_frame(
            &mut self.stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id,
                proof: self.proof.clone(),
                message,
            },
        )
        .unwrap();
    }

    fn reply(&mut self, request_id: u64, response: ServerResponse) {
        self.send(Some(request_id), ServerMessage::Response { response });
    }
}

#[test]
fn watches_a_terminal_with_daemon_push_and_releases_it_on_exit() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let listener = crate::descriptor::bind(&socket).unwrap();
    let session = TerminalId::new();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("session"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let (outbox, lines) = Outbox::new();
    let _watch = Watch::start(1, session, Provider::Claude, socket, proof.clone(), outbox).unwrap();
    let (stream, _) = crate::descriptor::accept(&listener).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let mut daemon = FakeDaemon { stream, proof };
    let connect = daemon.receive();
    assert!(matches!(connect.request, ClientRequest::Connect { .. }));
    let mut terminal = TerminalDescriptor::relaunchable(
        session,
        LaunchRecipe {
            program: "claude".into(),
            arguments: Vec::new(),
            working_directory: "/fixture/project".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        },
        None,
    );
    terminal.attached = true;
    terminal.event_sequence = 2;
    terminal.agent_activity = Some(crate::agent::Activity {
        provider: Provider::Claude,
        state: ActivityState::Working,
    });
    daemon.reply(
        connect.request_id,
        ServerResponse::Connected {
            proof: daemon.proof.clone(),
            resumed: vec![terminal],
        },
    );
    let visible = daemon.receive();
    assert!(matches!(
        visible.request,
        ClientRequest::SetVisibility { terminal_id, visibility: TerminalVisibility::Visible }
            if terminal_id == session
    ));
    daemon.reply(visible.request_id, ServerResponse::Accepted);

    // The descriptor describes the current agent. Older activity events
    // replayed ahead of the grid must not replace it or end this watch.
    for (sequence, activity) in [
        (1, None),
        (
            2,
            Some(crate::agent::Activity {
                provider: Provider::Codex,
                state: ActivityState::Idle,
            }),
        ),
    ] {
        daemon.send(
            None,
            ServerMessage::Event {
                terminal_id: session,
                sequence,
                event: WireTerminalEvent::AgentActivityChanged { activity },
            },
        );
    }
    daemon.send(
        None,
        ServerMessage::Redraw {
            terminal_id: session,
            generation: 1,
        },
    );
    let read = daemon.receive();
    assert!(matches!(
        read.request,
        ClientRequest::Snapshot { terminal_id, after_generation: 0 } if terminal_id == session
    ));

    let first = frame(1, &["running"]);
    let second = frame(2, &["finished"]);
    // The ordinary desktop transport pushes full and incremental snapshots.
    for (frame, snapshot, expected) in [
        (&first, TerminalSnapshot::full(&first), "running"),
        (&second, TerminalSnapshot::diff(&second, &first), "finished"),
    ] {
        if frame.generation == 1 {
            daemon.reply(
                read.request_id,
                ServerResponse::Snapshot {
                    terminal_id: session,
                    snapshot: Some(snapshot),
                },
            );
            assert_eq!(next_line(&lines), api::ServerLine::Ok { id: 1, page: None });
        } else {
            daemon.send(
                None,
                ServerMessage::Snapshot {
                    terminal_id: session,
                    snapshot,
                },
            );
        }
        let screen = next_screen(&lines);
        assert_eq!(screen.session, session.to_string());
        assert_eq!(screen.activity, api::Activity::Working);
        assert_eq!(screen.lines, [expected]);
        let ack = daemon.receive();
        assert!(matches!(
            ack.request,
            ClientRequest::AcknowledgeRedraw { terminal_id, generation }
                if terminal_id == session && generation == frame.generation
        ));
        daemon.reply(ack.request_id, ServerResponse::Accepted);
    }

    daemon.send(
        None,
        ServerMessage::Event {
            terminal_id: session,
            sequence: 3,
            event: WireTerminalEvent::AgentActivityChanged {
                activity: Some(crate::agent::Activity {
                    provider: Provider::Claude,
                    state: ActivityState::Approval,
                }),
            },
        },
    );
    let screen = next_screen(&lines);
    assert_eq!(screen.activity, api::Activity::Waiting);
    assert_eq!(screen.lines, ["finished"]);

    daemon.send(
        None,
        ServerMessage::Event {
            terminal_id: session,
            sequence: 4,
            event: WireTerminalEvent::Exited {
                exit_code: 0,
                signal: None,
                intentional: false,
            },
        },
    );
    assert!(matches!(
        next_line(&lines),
        api::ServerLine::Event(api::Event::TerminalEnded(ending)) if ending.session == session.to_string()
    ));
    // Even while the phone keeps its final text, the daemon attachment is gone.
    assert!(
        read_frame::<ClientEnvelope>(&mut daemon.stream)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_slow_phone_gets_only_the_latest_queued_screen_before_the_end() {
    let (outbox, lines) = Outbox::new();
    let publisher = Publisher {
        session: TerminalId::new(),
        state: Arc::new(Mutex::new(Some(State::Opening(1)))),
        pending: PendingScreen::default(),
        outbox: outbox.clone(),
    };
    for text in ["a", "b", "c"] {
        publisher.screen(screen(
            publisher.session,
            &frame(1, &[text]),
            api::Activity::Working,
            None,
        ));
    }
    publisher.finish(exited(0, None));
    assert_eq!(next_line(&lines), api::ServerLine::Ok { id: 1, page: None });
    assert_eq!(next_screen(&lines).lines, ["c"]);
    assert!(matches!(
        next_line(&lines),
        api::ServerLine::Event(api::Event::TerminalEnded(_))
    ));
    assert!(lines.outgoing.try_recv().is_err());
    // Also run the real writer's slot branch.
    let pending = PendingScreen::default();
    outbox.terminal_screen(
        &pending,
        screen(publisher.session, &frame(2, &[]), api::Activity::Idle, None),
    );
    outbox.exit();
    let mut output = io::Cursor::new(Vec::new());
    write_lines(lines, &mut output);
    assert!(
        String::from_utf8(output.into_inner())
            .unwrap()
            .contains("\"event\":\"terminal_screen\"")
    );
}

#[test]
fn stopping_during_open_answers_once_and_fences_a_late_screen() {
    let (outbox, lines) = Outbox::new();
    let (stop, _stopped) = bounded(0);
    let session = TerminalId::new();
    let publisher = Publisher {
        session,
        state: Arc::new(Mutex::new(Some(State::Opening(3)))),
        pending: PendingScreen::default(),
        outbox,
    };
    let watch = Watch {
        session,
        _stop: stop,
        publisher: publisher.clone(),
    };
    drop(watch);
    publisher.screen(screen(
        session,
        &frame(1, &["too late"]),
        api::Activity::Working,
        None,
    ));
    publisher.finish(exited(0, None));
    assert!(matches!(
        next_line(&lines),
        api::ServerLine::Error { id: Some(3), error } if error.code == api::ErrorCode::Unavailable
    ));
    assert!(lines.outgoing.try_recv().is_err());
}
