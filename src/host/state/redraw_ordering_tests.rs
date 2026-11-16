//! Orderings a redraw broadcast must keep now that its frames are encoded
//! outside the host lock. Each test plans a broadcast, lets something else
//! take the lock, then commits, and replays the subscriber's queue the way a
//! hosted client applies it.
use super::*;
use crate::{
    model::{BootId, HostIncarnation, HostSessionId},
    terminal::{CellSnapshot, CellText, RenderStreamFixture, RowSnapshot, ScrollSnapshot},
    terminal_ipc::SnapshotContent,
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
}

/// What the subscriber ends up with, replayed as `HostedTerminal` applies its
/// queue. Panics on any frame a client could not apply against exactly the
/// frame it received before it.
struct Replay {
    screen: Option<RenderSnapshot>,
    frames: Vec<u64>,
    /// The newest `Redraw` received after the last frame, if any.
    pending_redraw: Option<u64>,
}

fn replay(delivery: &DeliveryQueue, id: TerminalId) -> Replay {
    delivery.close_after_drain();
    let mut replay = Replay {
        screen: None,
        frames: Vec::new(),
        pending_redraw: None,
    };
    while let Some(message) = delivery.pop() {
        match message.message {
            ServerMessage::Snapshot {
                terminal_id,
                snapshot,
            } if terminal_id == id => {
                let current = replay.screen.as_ref();
                if let SnapshotContent::Delta {
                    base_generation, ..
                } = &snapshot.content
                {
                    assert_eq!(
                        Some(*base_generation),
                        current.map(|screen| screen.generation),
                        "a delta was queued against a frame other than the one queued before it"
                    );
                }
                let resolved = snapshot.apply(current).expect("frame applies");
                if let Some(current) = current {
                    assert!(
                        resolved.generation >= current.generation,
                        "frame {} queued after frame {}",
                        resolved.generation,
                        current.generation
                    );
                    if resolved.generation == current.generation {
                        // A hosted client drops a frame numbered like the one
                        // it holds, so that frame must show the same screen.
                        assert_eq!(text(&resolved), text(current));
                        continue;
                    }
                }
                replay.frames.push(resolved.generation);
                replay.pending_redraw = replay
                    .pending_redraw
                    .filter(|redraw| *redraw > resolved.generation);
                replay.screen = Some(resolved);
            }
            ServerMessage::Redraw {
                terminal_id,
                generation,
            } if terminal_id == id => {
                replay.pending_redraw = replay.pending_redraw.max(Some(generation));
            }
            _ => {}
        }
    }
    replay
}

impl Replay {
    /// The subscriber either shows `expected` or holds a newer `Redraw` that
    /// makes it fetch the current screen.
    fn converges_to(&self, expected: &[Arc<RowSnapshot>]) {
        let screen = self
            .screen
            .as_ref()
            .expect("the subscriber received a frame");
        let expected = text(&grid(0, expected));
        if text(screen) == expected {
            return;
        }
        assert!(
            self.pending_redraw
                .is_some_and(|redraw| redraw > screen.generation),
            "the subscriber shows {:?} instead of {expected:?} and was not told to fetch",
            text(screen)
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
        let replay = replay(&fixture.delivery, fixture.id);
        replay.converges_to(&fixture.lines);
        let last = *replay.frames.last().unwrap();
        let state = fixture.shared.lock().unwrap();
        let view = &state.subscriptions.viewports[&(1, fixture.id)];
        assert_eq!(
            state
                .subscriptions
                .last_sent(1, fixture.id)
                .unwrap()
                .generation,
            last,
            "{order:?}: the recorded base is not the last frame queued"
        );
        // Acknowledging the last frame releases the credit of the capture it
        // carried, never one the subscriber did not receive.
        assert_eq!(
            state.redraw_source(1, fixture.id, last),
            view.acknowledged_source
        );
        assert!(view.acknowledged_source <= fixture.generation);
    }
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
    let replay = replay(&fixture.delivery, fixture.id);
    assert_eq!(
        replay.frames,
        [1],
        "no frame at the old size after a resize"
    );
    assert_eq!(replay.pending_redraw, Some(resized));
}

#[test]
fn a_frame_planned_before_a_snapshot_reply_is_not_sent_against_the_replaced_base() {
    let mut fixture = Fixture::new();
    let generation = fixture.change(0, "late");
    let planned = fixture.plan(generation);
    let reply = fixture.change(1, "reply");
    {
        // What `client_snapshot` does in its final hold, with its reply
        // queued in that same hold.
        let mut state = fixture.shared.lock().unwrap();
        let revision = state.presentation_observed(1, fixture.id, reply);
        let mut capture = (*fixture.stream.handle.snapshot()).clone();
        capture.generation = revision;
        let capture = Arc::new(capture);
        state
            .subscriptions
            .viewports
            .get_mut(&(1, fixture.id))
            .unwrap()
            .acknowledged_source = reply;
        state
            .subscriptions
            .record_sent(1, fixture.id, Arc::clone(&capture));
        let proof = state.proof_handle();
        fixture
            .delivery
            .push(Outbound::broadcast(
                &proof,
                ServerMessage::Snapshot {
                    terminal_id: fixture.id,
                    snapshot: TerminalSnapshot::full(&capture),
                },
            ))
            .unwrap();
    }
    fixture.commit(planned);
    assert_eq!(fixture.acknowledged_source(), reply);
    let replay = replay(&fixture.delivery, fixture.id);
    assert_eq!(
        replay.frames.len(),
        2,
        "only the initial frame and the reply"
    );
    replay.converges_to(&fixture.lines);
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
fn a_hidden_subscriber_receives_nothing_planned_while_it_was_visible() {
    let mut fixture = Fixture::new();
    let generation = fixture.change(0, "late");
    let planned = fixture.plan(generation);
    fixture
        .shared
        .lock()
        .unwrap()
        .set_visibility(1, fixture.id, TerminalVisibility::Hidden)
        .unwrap();
    fixture.commit(planned);
    let replay = replay(&fixture.delivery, fixture.id);
    assert_eq!(replay.frames, [1]);
}

/// The same property with real threads: two broadcasts of each published
/// generation race, as the terminal's worker and the shared event notifier can.
#[test]
fn racing_broadcast_threads_keep_every_delta_chained() {
    const STEPS: usize = 150;
    let mut fixture = Fixture::new();
    let host = Arc::downgrade(&fixture.shared);
    let id = fixture.id;
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let racers = (0..2)
        .map(|_| {
            let host = host.clone();
            let worker = fixture.stream.handle.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                for _ in 0..STEPS {
                    barrier.wait();
                    broadcast_redraw(&host, id, worker.generation(), &worker);
                    barrier.wait();
                }
            })
        })
        .collect::<Vec<_>>();
    for step in 0..STEPS {
        fixture.change(step % usize::from(ROWS), &format!("{step:04}"));
        barrier.wait();
        barrier.wait();
    }
    for racer in racers {
        racer.join().unwrap();
    }
    let replay = replay(&fixture.delivery, fixture.id);
    replay.converges_to(&fixture.lines);
    assert!(
        replay.frames.len() > STEPS / 2,
        "the racing broadcasts delivered {} of {STEPS} frames",
        replay.frames.len()
    );
}
