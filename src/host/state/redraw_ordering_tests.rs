//! Orderings a redraw broadcast must keep now that its frames are encoded
//! outside the host lock. Each test plans a broadcast, lets something else
//! take the lock, then commits, and replays the subscriber's queue the way
//! `HostedTerminal` applies it.
use super::*;
use crate::{
    model::{BootId, HostIncarnation, HostSessionId},
    terminal::{CellSnapshot, CellText, RenderStreamFixture, RowSnapshot, ScrollSnapshot},
    terminal_ipc::{ServerResponse, SnapshotContent},
};

const ROWS: u16 = 3;

fn host() -> SharedHost {
    Arc::new(HostLock::new(HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("redraw-order"),
        boot_id: BootId::new("redraw-order-boot"),
        host_incarnation: HostIncarnation::new("redraw-order-incarnation"),
    })))
}

fn row(text: &str) -> Arc<RowSnapshot> {
    Arc::new(RowSnapshot {
        cells: text
            .split("")
            .filter(|cell| !cell.is_empty())
            .map(|cell| CellSnapshot {
                text: CellText::new(cell),
                ..CellSnapshot::default()
            })
            .collect(),
        ..RowSnapshot::default()
    })
}

fn grid(generation: u64, lines: &[Arc<RowSnapshot>]) -> RenderSnapshot {
    RenderSnapshot {
        generation,
        columns: 4,
        rows: lines.len() as u16,
        scroll: ScrollSnapshot {
            total: lines.len() as u64,
            len: lines.len() as u64,
            ..ScrollSnapshot::default()
        },
        lines: lines.to_vec().into(),
        ..RenderSnapshot::empty()
    }
}

fn text(snapshot: &RenderSnapshot) -> Vec<String> {
    snapshot
        .lines
        .iter()
        .map(|row| row.cells.iter().map(|cell| cell.text.as_str()).collect())
        .collect()
}

/// A terminal with one visible subscriber, which already holds the frame of
/// generation 1, delivered by a committed broadcast.
struct Fixture {
    shared: SharedHost,
    id: TerminalId,
    stream: RenderStreamFixture,
    delivery: Arc<DeliveryQueue>,
    lines: Vec<Arc<RowSnapshot>>,
    generation: u64,
}

impl Fixture {
    fn new() -> Self {
        let shared = host();
        let id = TerminalId::new();
        let stream = RenderStreamFixture::new(|_| {});
        let lines = vec![row("aaaa"), row("bbbb"), row("cccc")];
        stream.publish_snapshot(grid(1, &lines));
        let delivery = {
            let mut state = shared.lock().unwrap();
            let delivery = state.register_client(1);
            state.reserve_spawn(id).unwrap();
            state
                .finish_spawn(
                    1,
                    super::redraw_identity_tests::session(id),
                    stream.handle.clone(),
                    true,
                )
                .unwrap();
            delivery
        };
        let fixture = Self {
            shared,
            id,
            stream,
            delivery,
            lines,
            generation: 1,
        };
        broadcast_redraw(
            &Arc::downgrade(&fixture.shared),
            id,
            1,
            &fixture.stream.handle,
        );
        fixture
    }

    /// Publishes the next grid, replacing one row so the delta is not empty.
    fn change(&mut self, index: usize, contents: &str) -> u64 {
        self.generation += 1;
        self.lines[index] = row(contents);
        self.stream
            .publish_snapshot(grid(self.generation, &self.lines));
        self.generation
    }

    fn plan(&self, generation: u64) -> Vec<PlannedFrame> {
        plan_redraw(
            &mut self.shared.lock().unwrap(),
            self.id,
            generation,
            &self.stream.handle,
        )
    }

    fn commit(&self, planned: Vec<PlannedFrame>) {
        let encoded = planned.into_iter().map(PlannedFrame::encode).collect();
        drop(commit_redraw(
            &mut self.shared.lock().unwrap(),
            self.id,
            &self.stream.handle,
            encoded,
        ));
    }

    fn acknowledged_source(&self) -> u64 {
        self.shared.lock().unwrap().subscriptions.viewports[&(1, self.id)].acknowledged_source
    }

    /// Answers a `ClientRequest::Snapshot` as the daemon does: the reply is
    /// recorded under the lock, and its connection queues it afterwards.
    fn reply(&self, after: u64) -> Outbound {
        let snapshot = client_snapshot(&self.shared, 1, self.id, after).unwrap();
        let proof = self.shared.lock().unwrap().proof().clone();
        Outbound::response(
            &proof,
            7,
            crate::terminal_ipc::PreparedResponse::Decoded(ServerResponse::Snapshot {
                terminal_id: self.id,
                snapshot,
            }),
        )
    }

    /// Whether acknowledging what the subscriber shows releases the worker's
    /// capture credit, rather than leaving capture withheld.
    fn acknowledgement_releases_credit(&self, replay: &Replay) {
        if replay.pending_redraw.is_some() {
            return;
        }
        let shown = replay.screen.as_ref().unwrap().generation;
        let state = self.shared.lock().unwrap();
        let source = state.redraw_source(1, self.id, shown);
        assert_ne!(source, 0, "acknowledging frame {shown} releases nothing");
        assert_eq!(
            source,
            state.subscriptions.viewports[&(1, self.id)].acknowledged_source
        );
    }
}

/// What the subscriber ends up with, replayed as `HostedTerminal` applies its
/// queue. Panics on a pushed frame that is not relative to exactly the frame
/// queued before it.
struct Replay {
    screen: Option<RenderSnapshot>,
    frames: Vec<u64>,
    /// A `Redraw` newer than the screen, which makes the client fetch.
    pending_redraw: Option<u64>,
    /// The client re-requested a full grid after a reply it could not apply.
    refetching: bool,
}

/// `snapshot_pending` is whether the client has a snapshot request in flight
/// when the queue starts; it then ignores `Redraw` and failed pushed frames.
fn replay(delivery: &DeliveryQueue, id: TerminalId, mut snapshot_pending: bool) -> Replay {
    delivery.close_after_drain();
    let mut replay = Replay {
        screen: None,
        frames: Vec::new(),
        pending_redraw: None,
        refetching: false,
    };
    while let Some(message) = delivery.pop() {
        let reply = message.wire().request_id.is_some();
        let snapshot = match message.message {
            ServerMessage::Snapshot {
                terminal_id,
                snapshot,
            } if terminal_id == id => Some(snapshot),
            ServerMessage::Response {
                response:
                    ServerResponse::Snapshot {
                        terminal_id,
                        snapshot,
                    },
            } if terminal_id == id => {
                snapshot_pending = false;
                snapshot
            }
            ServerMessage::Redraw {
                terminal_id,
                generation,
            } if terminal_id == id => {
                let cursor = replay.screen.as_ref().map_or(0, |screen| screen.generation);
                if generation > cursor && !snapshot_pending {
                    replay.pending_redraw = replay.pending_redraw.max(Some(generation));
                }
                None
            }
            _ => None,
        };
        let Some(snapshot) = snapshot else { continue };
        let current = replay.screen.as_ref();
        if let SnapshotContent::Delta {
            base_generation, ..
        } = &snapshot.content
            && !reply
        {
            assert_eq!(
                Some(*base_generation),
                current.map(|screen| screen.generation),
                "a delta was pushed against a frame other than the one queued before it"
            );
        }
        let Ok(resolved) = snapshot.apply(current) else {
            // Only a reply can get here: its base was replaced by a frame
            // pushed before it. The client asks for a full grid.
            assert!(reply);
            replay.refetching = true;
            continue;
        };
        if let Some(current) = current
            && resolved.generation <= current.generation
        {
            // A hosted client drops a frame no newer than the one it holds.
            if !reply {
                assert_eq!(resolved.generation, current.generation);
                assert_eq!(
                    text(&resolved),
                    text(current),
                    "a pushed frame reused a number for a different screen"
                );
            }
            continue;
        }
        replay.frames.push(resolved.generation);
        replay.pending_redraw = replay
            .pending_redraw
            .filter(|redraw| *redraw > resolved.generation);
        replay.screen = Some(resolved);
    }
    replay
}

impl Replay {
    fn fetching(&self) -> bool {
        self.refetching || self.pending_redraw.is_some()
    }

    /// The subscriber either shows `expected` or is fetching the current
    /// screen.
    fn converges_to(&self, expected: &[Arc<RowSnapshot>]) {
        let screen = self
            .screen
            .as_ref()
            .expect("the subscriber received a frame");
        assert!(
            text(screen) == text(&grid(0, expected)) || self.fetching(),
            "the subscriber shows {:?} instead of {:?} and is not fetching",
            text(screen),
            text(&grid(0, expected))
        );
    }
}

#[test]
fn concurrent_broadcasts_never_queue_a_delta_against_a_frame_the_client_lacks() {
    // Two broadcasts for one terminal, from its worker thread and the shared
    // event notifier, in every order their two holds can interleave.
    let orders: [[(usize, bool); 4]; 6] = [
        [(0, false), (1, false), (0, true), (1, true)],
        [(0, false), (1, false), (1, true), (0, true)],
        [(1, false), (0, false), (0, true), (1, true)],
        [(1, false), (0, false), (1, true), (0, true)],
        [(0, false), (0, true), (1, false), (1, true)],
        [(1, false), (1, true), (0, false), (0, true)],
    ];
    for order in orders {
        let mut fixture = Fixture::new();
        let mut planned: [Option<Vec<PlannedFrame>>; 2] = [None, None];
        let mut changed = 0;
        for (broadcast, commit) in order {
            if commit {
                fixture.commit(planned[broadcast].take().unwrap());
            } else {
                changed += 1;
                let generation = fixture.change(changed, &format!("new{changed}"));
                planned[broadcast] = Some(fixture.plan(generation));
            }
        }
        let replay = replay(&fixture.delivery, fixture.id, false);
        replay.converges_to(&fixture.lines);
        let last = *replay.frames.last().unwrap();
        assert_eq!(
            fixture
                .shared
                .lock()
                .unwrap()
                .subscriptions
                .last_sent(1, fixture.id)
                .unwrap()
                .generation,
            last,
            "{order:?}: the recorded base is not the last frame queued"
        );
        fixture.acknowledgement_releases_credit(&replay);
    }
}

/// A wake can name an older generation than the capture it finds, when the
/// worker and the event notifier broadcast at once. The newer screen must get
/// a number of its own: under the old one the subscriber would drop it while
/// the daemon recorded it as the next delta's base.
#[test]
fn a_stale_wake_with_a_newer_capture_numbers_it_afresh() {
    let mut fixture = Fixture::new();
    let current = fixture.change(0, "two");
    broadcast_redraw(
        &Arc::downgrade(&fixture.shared),
        fixture.id,
        current,
        &fixture.stream.handle,
    );
    let newest = fixture.change(1, "tres");
    broadcast_redraw(
        &Arc::downgrade(&fixture.shared),
        fixture.id,
        current,
        &fixture.stream.handle,
    );
    // Its own wake then finds nothing it has not already sent.
    broadcast_redraw(
        &Arc::downgrade(&fixture.shared),
        fixture.id,
        newest,
        &fixture.stream.handle,
    );
    let last = fixture.change(2, "four");
    broadcast_redraw(
        &Arc::downgrade(&fixture.shared),
        fixture.id,
        last,
        &fixture.stream.handle,
    );
    let replay = replay(&fixture.delivery, fixture.id, false);
    assert_eq!(replay.frames.len(), 4, "{:?}", replay.frames);
    replay.converges_to(&fixture.lines);
    fixture.acknowledgement_releases_credit(&replay);
}

#[test]
fn a_frame_planned_before_a_resize_is_replaced_by_a_redraw() {
    let mut fixture = Fixture::new();
    let generation = fixture.change(0, "late");
    let planned = fixture.plan(generation);
    assert_eq!(planned.len(), 1);
    let resized = {
        let mut state = fixture.shared.lock().unwrap();
        state
            .resize_client(
                1,
                fixture.id,
                crate::terminal::TerminalSize {
                    columns: 4,
                    rows: ROWS + 2,
                    cell_width_px: 8,
                    cell_height_px: 16,
                },
            )
            .unwrap();
        state.subscriptions.viewports[&(1, fixture.id)].revision
    };
    let before = fixture.acknowledged_source();
    fixture.commit(planned);
    assert_eq!(
        fixture.acknowledged_source(),
        before,
        "an unsent frame must not move the acknowledged capture"
    );
    let replay = replay(&fixture.delivery, fixture.id, false);
    assert_eq!(
        replay.frames,
        [1],
        "no frame at the old size after a resize"
    );
    assert_eq!(replay.pending_redraw, Some(resized));
}

/// A `ClientRequest::Snapshot` reply is recorded under the lock but queued by
/// its connection after the lock is released, so a broadcast can be queued
/// first. The client still holds its old screen then. A delta against the
/// reply would fail; with the reply pending the client would not re-request,
/// would settle on the reply while the daemon recorded the broadcast's frame,
/// and its acknowledgement would release no capture credit.
#[test]
fn a_broadcast_racing_a_snapshot_reply_still_applies_and_releases_credit() {
    for reply_queued_first in [false, true] {
        let mut fixture = Fixture::new();
        fixture.change(0, "reply");
        let reply = fixture.reply(1);
        if reply_queued_first {
            fixture.delivery.push(reply.clone()).unwrap();
        }
        let generation = fixture.change(1, "push");
        broadcast_redraw(
            &Arc::downgrade(&fixture.shared),
            fixture.id,
            generation,
            &fixture.stream.handle,
        );
        if !reply_queued_first {
            fixture.delivery.push(reply).unwrap();
        }
        let replay = replay(&fixture.delivery, fixture.id, true);
        // Queued after the broadcast, the reply's delta no longer applies and
        // the client asks once for a full grid; it already shows the newest.
        assert_eq!(replay.refetching, !reply_queued_first);
        assert_eq!(
            text(replay.screen.as_ref().unwrap()),
            text(&grid(0, &fixture.lines))
        );
        fixture.acknowledgement_releases_credit(&replay);
        // The broadcast's frame is a base the client now holds in order, so
        // the next frame is planned as a delta against it. (The replay closed
        // the connection, so read the recorded base rather than plan.)
        assert!(
            fixture.shared.lock().unwrap().subscriptions.sent[&(1, fixture.id)].queued,
            "the next frame would not be a delta"
        );
    }
}

#[test]
fn a_frame_encoded_for_a_retired_worker_never_reaches_a_same_id_successor() {
    let mut fixture = Fixture::new();
    let generation = fixture.change(0, "old");
    let planned = fixture.plan(generation);
    assert_eq!(planned.len(), 1);
    let successor = RenderStreamFixture::new(|_| {});
    successor.publish_snapshot(grid(1, &[row("next"), row("next"), row("next")]));
    {
        let mut state = fixture.shared.lock().unwrap();
        drop(state.retire_terminal(fixture.id).unwrap());
        state.reserve_spawn(fixture.id).unwrap();
        state
            .finish_spawn(
                1,
                super::redraw_identity_tests::session(fixture.id),
                successor.handle.clone(),
                true,
            )
            .unwrap();
    }
    // The successor presents its own first frame before the old commit runs.
    broadcast_redraw(
        &Arc::downgrade(&fixture.shared),
        fixture.id,
        1,
        &successor.handle,
    );
    let successor_frame = Arc::clone(
        fixture
            .shared
            .lock()
            .unwrap()
            .subscriptions
            .last_sent(1, fixture.id)
            .unwrap(),
    );
    let commands = successor.queued_commands();
    fixture.commit(planned);
    assert_eq!(successor.queued_commands(), commands);
    let proof = {
        let state = fixture.shared.lock().unwrap();
        assert!(Arc::ptr_eq(
            state.subscriptions.last_sent(1, fixture.id).unwrap(),
            &successor_frame
        ));
        state.proof_handle()
    };
    // Mark the end of the queue: nothing may follow the successor's frame.
    let marker = TerminalId::new();
    fixture
        .delivery
        .push(Outbound::broadcast(
            &proof,
            ServerMessage::Redraw {
                terminal_id: marker,
                generation: 0,
            },
        ))
        .unwrap();
    fixture.delivery.close_after_drain();
    let mut after_successor = None;
    while let Some(message) = fixture.delivery.pop() {
        match message.message {
            ServerMessage::Snapshot { terminal_id, .. } if terminal_id == fixture.id => {
                after_successor = Some(Vec::new());
            }
            other => {
                if let Some(after) = after_successor.as_mut() {
                    after.push(other);
                }
            }
        }
    }
    assert!(
        matches!(
            after_successor.expect("the successor's frame was queued").as_slice(),
            [ServerMessage::Redraw { terminal_id, .. }] if *terminal_id == marker
        ),
        "the retired worker's commit queued a message after the successor's frame"
    );
}

#[test]
fn a_subscriber_hidden_after_planning_gets_nothing_and_its_reveal_returns_credit() {
    let mut fixture = Fixture::new();
    let generation = fixture.change(0, "late");
    let planned = fixture.plan(generation);
    let before = fixture.acknowledged_source();
    fixture
        .shared
        .lock()
        .unwrap()
        .set_visibility(1, fixture.id, TerminalVisibility::Hidden)
        .unwrap();
    fixture.commit(planned);
    assert_eq!(fixture.acknowledged_source(), before);
    // The reveal notifies; the client answers by fetching what it missed.
    fixture
        .shared
        .lock()
        .unwrap()
        .set_visibility(1, fixture.id, TerminalVisibility::Visible)
        .unwrap();
    let reply = fixture.reply(1);
    fixture.delivery.push(reply).unwrap();
    let replay = replay(&fixture.delivery, fixture.id, false);
    assert_eq!(replay.frames.len(), 2, "the hidden frame was pushed");
    replay.converges_to(&fixture.lines);
    assert_eq!(fixture.acknowledged_source(), generation);
    assert!(!replay.fetching());
    fixture.acknowledgement_releases_credit(&replay);
}

/// How the worker of a terminal still being spawned comes to be hidden.
#[derive(Clone, Copy, Debug)]
enum HiddenWhileStarting {
    /// A wake finds only the spawning client, attached hidden for now.
    ByWake,
    /// Another client attaches, shows the terminal, then hides it.
    ByAnotherClient,
}

/// Fills the worker's bounded command queue, as a worker busy with a flood of
/// input would leave it.
fn fill_commands(stream: &RenderStreamFixture) {
    let refused = std::iter::repeat_with(|| stream.handle.scroll_to_bottom()).find_map(Result::err);
    assert_eq!(refused, Some(TerminalSendError::Full));
}

/// A spawning client is attached hidden until its spawn is published. Its
/// worker may be hidden meanwhile, so publishing it visible must reveal the
/// worker, or it captures nothing more until the terminal exits. The
/// publication has committed by then, so the reveal must not be refusable by
/// a full command queue either.
#[test]
fn publishing_a_spawn_reveals_a_worker_hidden_while_it_started() {
    for (hidden_by, initially_visible, queue_full) in [
        (HiddenWhileStarting::ByWake, true, false),
        (HiddenWhileStarting::ByAnotherClient, true, false),
        (HiddenWhileStarting::ByWake, false, false),
        (HiddenWhileStarting::ByWake, true, true),
        (HiddenWhileStarting::ByAnotherClient, true, true),
    ] {
        let shared = host();
        let id = TerminalId::new();
        let stream = RenderStreamFixture::new(|_| {});
        stream.publish(1);
        {
            let mut state = shared.lock().unwrap();
            state.register_client(1);
            state.register_client(2);
            state
                .install_startup_parent(
                    1,
                    super::redraw_identity_tests::session(id),
                    stream.handle.clone(),
                    false,
                    None,
                    None,
                )
                .unwrap();
            state.publication_pending.insert(
                id,
                PendingPublication {
                    handle: stream.handle.clone(),
                    source: super::redraw_identity_tests::session(id),
                    pane: None,
                    original: None,
                    reservation: None,
                },
            );
        }
        match hidden_by {
            HiddenWhileStarting::ByWake => {
                broadcast_redraw(&Arc::downgrade(&shared), id, 1, &stream.handle);
            }
            HiddenWhileStarting::ByAnotherClient => {
                let mut state = shared.lock().unwrap();
                state.attach(2, id).unwrap();
                state
                    .set_visibility(2, id, TerminalVisibility::Visible)
                    .unwrap();
                state
                    .set_visibility(2, id, TerminalVisibility::Hidden)
                    .unwrap();
            }
        }
        assert_eq!(stream.drain_visibility(), Some(false), "{hidden_by:?}");
        if queue_full {
            fill_commands(&stream);
        }

        let options = TerminalSpawnOptions {
            initially_visible,
            ..TerminalSpawnOptions::default()
        };
        publish_installed_terminal(&shared, Some(1), id, &stream.handle, &options).unwrap();
        assert_eq!(
            stream.drain_visibility(),
            initially_visible.then_some(true),
            "{hidden_by:?}, initially visible: {initially_visible}, queue full: {queue_full}"
        );
    }
}

/// Visibility requests are levels, not queued commands: a full command queue
/// refuses none of them, and the newest replaces a redraw's Hide.
#[test]
fn a_full_command_queue_refuses_no_visibility_change() {
    use TerminalVisibility::{Hibernated, Hidden, Visible};
    let fixture = Fixture::new();
    let set = |visibility| {
        fixture
            .shared
            .lock()
            .unwrap()
            .set_visibility(1, fixture.id, visibility)
    };
    set(Hidden).unwrap();
    // The worker published before it applied the hide; the redraw re-asks.
    broadcast_redraw(
        &Arc::downgrade(&fixture.shared),
        fixture.id,
        1,
        &fixture.stream.handle,
    );
    assert_eq!(fixture.stream.take_host_visibility(), Some(Hidden));
    // Every request below is made while the queue stays full.
    fill_commands(&fixture.stream);
    set(Visible).unwrap();
    assert_eq!(fixture.stream.take_host_visibility(), Some(Visible));
    set(Hibernated).unwrap();
    assert_eq!(fixture.stream.take_host_visibility(), Some(Hibernated));
    let mut state = fixture.shared.lock().unwrap();
    state.register_client(2);
    state.attach(2, fixture.id).unwrap();
    assert_eq!(
        fixture.stream.take_host_visibility(),
        Some(Hidden),
        "attach"
    );
    state.detach(2, fixture.id).unwrap();
    assert_eq!(
        fixture.stream.take_host_visibility(),
        Some(Hibernated),
        "detach"
    );
    drop(state);
    fill_commands(&fixture.stream);
}

/// A worker reveals once per transition to Visible: each reveal forces a
/// full capture and resets the subscriber's capture credit. A client leaving
/// while another still shows the terminal changes nothing.
#[test]
fn a_client_leaving_a_terminal_another_shows_does_not_reveal_it_again() {
    use TerminalVisibility::{Hidden, Visible};
    let fixture = Fixture::new();
    // Whatever installing the terminal asked of its worker.
    fixture.stream.take_host_visibility();
    let mut state = fixture.shared.lock().unwrap();
    state.register_client(2);
    state.attach(2, fixture.id).unwrap();
    state.set_visibility(2, fixture.id, Visible).unwrap();
    assert_eq!(fixture.stream.take_host_visibility(), None);
    drop(state.unregister_client(2));
    assert_eq!(
        fixture.stream.take_host_visibility(),
        None,
        "the remaining viewer's terminal was revealed again"
    );
    // Control: the last viewer leaving does change it.
    drop(state.unregister_client(1));
    assert_eq!(fixture.stream.take_host_visibility(), Some(Hidden));
}

/// A redraw pushes its frames under the host lock. When that overflows a slow
/// client's queue, what the queue discards, frames included, must be freed by
/// the client's writer, not inside the broadcast's hold.
#[test]
fn a_redraw_overflowing_a_slow_client_frees_nothing_under_the_host_lock() {
    let mut fixture = Fixture::new();
    let proof = fixture.shared.lock().unwrap().proof_handle();
    while fixture.delivery.queued() < DEFAULT_CLIENT_QUEUE_CAPACITY {
        fixture
            .delivery
            .push(Outbound::broadcast(
                &proof,
                ServerMessage::TerminalObserved {
                    terminal_id: TerminalId::new(),
                    epoch: 0,
                },
            ))
            .unwrap();
    }
    let queued = Arc::strong_count(&proof);
    let generation = fixture.change(0, "over");
    broadcast_redraw(
        &Arc::downgrade(&fixture.shared),
        fixture.id,
        generation,
        &fixture.stream.handle,
    );
    // The refused frame and the QueueFull notice joined the discarded queue.
    assert_eq!(Arc::strong_count(&proof), queued + 2);
    let next = fixture.change(1, "gone");
    assert!(
        fixture.plan(next).is_empty(),
        "a frame was built for a closed connection"
    );
    let notice = fixture.delivery.pop().unwrap();
    assert!(matches!(
        notice.message,
        ServerMessage::Error(ref error) if error.code == ProtocolErrorCode::QueueFull
    ));
    drop(notice);
    assert!(fixture.delivery.pop().is_none());
    assert_eq!(
        Arc::strong_count(&proof),
        queued - DEFAULT_CLIENT_QUEUE_CAPACITY
    );
}

/// A connection that closes between the holds refuses the frame. It is
/// neither recorded as sent nor freed under the commit's hold.
#[test]
fn a_frame_for_a_connection_closed_after_planning_is_refused() {
    let mut fixture = Fixture::new();
    let generation = fixture.change(0, "late");
    let planned = fixture.plan(generation);
    assert_eq!(planned.len(), 1);
    let recorded = |fixture: &Fixture| {
        Arc::clone(
            fixture
                .shared
                .lock()
                .unwrap()
                .subscriptions
                .last_sent(1, fixture.id)
                .unwrap(),
        )
    };
    let sent = recorded(&fixture);
    let before = fixture.acknowledged_source();
    fixture.delivery.close_after_drain();
    fixture.commit(planned);
    assert!(
        Arc::ptr_eq(&recorded(&fixture), &sent),
        "a refused frame was recorded as sent"
    );
    assert_eq!(fixture.acknowledged_source(), before);
}

/// The same property with real threads: two broadcasts race for every
/// pair of generations the worker publishes, one with the newer generation
/// and one with the older, as the worker and the shared event notifier can.
/// The deterministic interleavings above are what cover each order; this
/// checks nothing else is missed when the holds actually contend.
#[test]
fn racing_broadcast_threads_keep_every_delta_chained() {
    const STEPS: usize = 100;
    let mut fixture = Fixture::new();
    let host = Arc::downgrade(&fixture.shared);
    let id = fixture.id;
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let wakes = Arc::new(std::sync::Mutex::new([0_u64; 2]));
    let racers = (0..2)
        .map(|racer| {
            let host = host.clone();
            let worker = fixture.stream.handle.clone();
            let barrier = Arc::clone(&barrier);
            let wakes = Arc::clone(&wakes);
            thread::spawn(move || {
                for _ in 0..STEPS {
                    barrier.wait();
                    let generation = wakes.lock().unwrap()[racer];
                    broadcast_redraw(&host, id, generation, &worker);
                    barrier.wait();
                }
            })
        })
        .collect::<Vec<_>>();
    for step in 0..STEPS {
        let older = fixture.change(step % usize::from(ROWS), &format!("{step:03}a"));
        let newer = fixture.change((step + 1) % usize::from(ROWS), &format!("{step:03}b"));
        *wakes.lock().unwrap() = [older, newer];
        barrier.wait();
        barrier.wait();
    }
    for racer in racers {
        racer.join().unwrap();
    }
    let replay = replay(&fixture.delivery, fixture.id, false);
    replay.converges_to(&fixture.lines);
    fixture.acknowledgement_releases_credit(&replay);
    assert!(
        replay.frames.len() > STEPS / 2,
        "the racing broadcasts delivered {} of {STEPS} frames",
        replay.frames.len()
    );
}
