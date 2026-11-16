//! Hidden chats that keep their rendered transcript.
//!
//! Switching channel hides every pane of the one left behind. A hidden chat
//! used to give up its presentation and its client's copy of the transcript at
//! once, so coming back fetched the whole window from the daemon again, parsed
//! and highlighted every message, and showed "Loading message…" in the
//! meantime. The chats a person moved away from most recently now stay warm:
//! the runtime keeps its prepared items and the client keeps the transcript,
//! so a return shows the last presentation at once and the daemon sends only
//! what changed since, or nothing.
//!
//! Warmth is bounded. A chat whose transcript is too large to keep goes cold as
//! it hides, and [`over_budget`] keeps at most [`CHATS`] warm chats within
//! [`BYTES`], releasing the least recently hidden first. The estimate is of
//! the heap a warm chat holds: its transcript, what its prepared items count
//! of themselves, and its parsed documents by their text and node count,
//! calibrated against the heap cooling actually frees. Process RSS can stay
//! higher once heap is freed; see `docs/performance.md`.
//!
//! A Resume that retires its source can keep one last projected window even
//! after cooling: that source cannot reload it. The runtime owns that window
//! independently of disposable warmth, with its old client's body interest
//! released. This budget bounds warm presentations, not the sum of those
//! unreloadable source windows, each still bounded by its publication limits.
use std::cell::Cell;

/// Conservative per-node allowance beyond the document's text. The parse
/// and its document share raw runs but keep their own items, text cells,
/// metadata and outline. Keep headroom for complex nodes; the cooling census
/// in `frame_cost::what_a_warm_hidden_chat_holds` measures the retained graph.
pub(super) const NODE_BYTES: usize = 2 << 10;
/// What a prepared item holds whatever its content: its cached state, flow
/// cache and reader memory. Measured the same way, about 10 KiB an item in a
/// window of ordinary exchanges.
pub(super) const ITEM_BYTES: usize = 12 << 10;

/// How many hidden chats keep their presentation at once.
pub(super) const CHATS: usize = 8;
/// What all warm chats together may retain, by [`super::Runtime::warm_bytes`].
pub(super) const BYTES: usize = 96 << 20;
/// A chat retaining more than this goes cold as soon as it hides.
pub(super) const CHAT_BYTES: usize = 32 << 20;

thread_local! {
    static NEXT: Cell<u64> = const { Cell::new(1) };
    static DIRTY: Cell<bool> = const { Cell::new(false) };
}

/// The order a chat was hidden in; later hides are larger. Chats hidden
/// together, such as every pane of the channel a person left, share one
/// stamp until the budget is next applied, so they stay warm or go cold
/// together and a return never finds part of a channel still loading.
pub(super) fn stamp() -> u64 {
    DIRTY.with(|dirty| dirty.set(true));
    NEXT.with(Cell::get)
}

/// Whether a chat went warm since the budget was last applied. Taking it
/// closes the current hide, so chats hidden after it are newer.
pub(super) fn take_dirty() -> bool {
    let dirty = DIRTY.with(|dirty| dirty.replace(false));
    if dirty {
        NEXT.with(|next| next.set(next.get() + 1));
    }
    dirty
}

/// Which warm chats to release, given each one's hide stamp and retained
/// bytes: the positions of those outside the budget. Hides are kept from the
/// most recent back, each whole while it still fits. Only the most recent
/// hide, when it alone exceeds the budget, is kept in part.
pub(super) fn over_budget(warm: &[(u64, usize)]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..warm.len()).collect();
    order.sort_unstable_by_key(|&index| (std::cmp::Reverse(warm[index].0), index));
    let (mut kept, mut bytes) = (0, 0usize);
    let mut released = Vec::new();
    for hide in order.chunk_by(|&a, &b| warm[a].0 == warm[b].0) {
        let size = hide
            .iter()
            .map(|&index| warm[index].1)
            .fold(0usize, usize::saturating_add);
        if kept + hide.len() <= CHATS && bytes.saturating_add(size) <= BYTES {
            kept += hide.len();
            bytes += size;
        } else if kept == 0 {
            for &index in hide {
                let size = warm[index].1;
                if kept < CHATS && bytes.saturating_add(size) <= BYTES {
                    kept += 1;
                    bytes += size;
                } else {
                    released.push(index);
                }
            }
        } else {
            released.extend_from_slice(hide);
        }
    }
    released
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::{CHAT_BYTES, CHATS, over_budget};

    fn answer(id: &str, text: String) -> Arc<chat::Item> {
        Arc::new(chat::Item {
            id: id.into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text,
                thinking: false,
                phase: chat::Phase::Final,
            },
            started_at: None,
            at: None,
        })
    }

    fn items() -> Vec<Arc<chat::Item>> {
        (0..6)
            .map(|index| {
                answer(
                    &format!("answer-{index}"),
                    format!("## Answer {index}\n\nSome **bold** prose and `code`.\n"),
                )
            })
            .collect()
    }

    /// A visible runtime whose transcript is prepared, and its client.
    fn shown(items: Vec<Arc<chat::Item>>) -> (Runtime, Arc<ClientHandle>) {
        let (mut runtime, handle) = attached(items);
        preparation::drain_fixture(&mut runtime);
        assert!(runtime.cache.values().all(|cached| !cached.placeholder));
        (runtime, handle)
    }

    /// A visible runtime and its client, before its workers prepare anything.
    fn attached(items: Vec<Arc<chat::Item>>) -> (Runtime, Arc<ClientHandle>) {
        let account = Account::system(Provider::Claude);
        let session = fresh_chat_session(
            Provider::Claude,
            TerminalId::new(),
            "/project".into(),
            &account,
            false,
        )
        .expect("a session");
        let snapshot = Snapshot {
            ready: true,
            generation: 1,
            items,
            ..Snapshot::default()
        };
        let handle = Arc::new(ClientHandle::fixture(session.clone(), snapshot));
        let target = Target {
            card: CardId::new(),
            generation: 1,
        };
        let runtime = Runtime::new(
            target,
            session,
            Connection::Live(handle.clone()),
            true,
            crate::settings::EditorSyntaxTheme::default(),
        );
        (runtime, handle)
    }

    fn revisions(runtime: &Runtime) -> Vec<(String, u64)> {
        let mut revisions: Vec<_> = runtime
            .cache
            .iter()
            .map(|(id, cached)| (id.clone(), cached.revision))
            .collect();
        revisions.sort();
        revisions
    }

    #[test]
    fn a_hidden_chat_keeps_its_presentation_and_shows_it_again_at_once() {
        let (mut runtime, handle) = shown(items());
        let prepared = revisions(&runtime);
        let items = runtime.snapshot.items.clone();
        runtime.set_visible(false).unwrap();
        assert!(runtime.warm_stamp().is_some());
        assert_eq!(revisions(&runtime), prepared);
        assert!(handle.resident_snapshot_with_clock().is_some());
        // Hidden passes reapply visibility; they keep it warm.
        runtime.set_visible(false).unwrap();
        runtime.refresh();
        assert_eq!(revisions(&runtime), prepared);

        runtime.set_visible(true).unwrap();
        assert!(runtime.warm_stamp().is_none());
        assert!(
            runtime.snapshot.items.len() == items.len()
                && runtime
                    .snapshot
                    .items
                    .iter()
                    .zip(&items)
                    .all(|(shown, kept)| Arc::ptr_eq(shown, kept)),
            "the same transcript is shown again"
        );
        assert_eq!(revisions(&runtime), prepared);
        assert!(
            !runtime.preparation.waiting() && runtime.preparation.queued_bytes() == 0,
            "an unchanged return prepares nothing"
        );
    }

    #[test]
    fn a_warm_return_keeps_the_readers_place_disclosure_and_draft() {
        let (mut runtime, _handle) = shown(items());
        runtime.replace_draft(text_editor::Content::with_text("Unsent draft"));
        runtime.scroll = view::Scroll::new(200.0, 1000.0, 400.0);
        runtime.expanded.insert("answer-2".into());
        let turn = runtime.turns[0].key.clone();
        runtime.opened.insert(turn.clone(), false);
        runtime.unseen_activity = false;
        let scroll = runtime.scroll;
        let turns: Vec<_> = runtime.turns.iter().map(|turn| turn.key.clone()).collect();
        let prepared = revisions(&runtime);

        runtime.set_visible(false).unwrap();
        assert!(runtime.warm_stamp().is_some());
        runtime.refresh();
        runtime.set_visible(true).unwrap();
        runtime.refresh();

        assert_eq!(runtime.composer.text().trim_end(), "Unsent draft");
        assert_eq!(runtime.scroll, scroll, "the reader's place is kept");
        assert!(runtime.expanded.contains("answer-2"));
        assert_eq!(
            runtime.opened.get(&turn),
            Some(&false),
            "a closed turn stays closed"
        );
        assert_eq!(
            runtime
                .turns
                .iter()
                .map(|turn| turn.key.clone())
                .collect::<Vec<_>>(),
            turns
        );
        assert_eq!(revisions(&runtime), prepared);
        assert!(
            !runtime.unseen_activity,
            "an unchanged return is not new activity"
        );
    }

    #[test]
    fn a_full_snapshot_after_a_warm_return_keeps_unchanged_items_prepared() {
        let (mut runtime, handle) = shown(items());
        let prepared = revisions(&runtime);
        runtime.set_visible(false).unwrap();
        // The answer grew and another arrived while hidden; the daemon sends
        // the whole window because the client's generation is not its last.
        let mut next = handle.snapshot().as_ref().clone();
        next.generation = 3;
        next.items = next
            .items
            .iter()
            .map(|item| Arc::new(item.as_ref().clone()))
            .collect();
        let last = next.items.len() - 1;
        let ItemContent::Text { text, .. } = &next.items[last].content else {
            unreachable!()
        };
        next.items[last] = answer(&next.items[last].id, format!("{text}More words.\n"));
        next.items.push(answer("new", "A new answer.".into()));
        runtime.set_visible(true).unwrap();
        handle.deliver_snapshot_for_test(next);
        runtime.refresh();
        preparation::drain_fixture(&mut runtime);
        let after = revisions(&runtime);
        let unchanged = |id: &str| {
            prepared.iter().find(|(old, _)| old == id).map(|(_, r)| *r)
                == after.iter().find(|(new, _)| new == id).map(|(_, r)| *r)
        };
        for index in 0..last {
            assert!(
                unchanged(&format!("answer-{index}")),
                "answer-{index} was prepared again"
            );
        }
        assert!(runtime.cache.values().all(|cached| !cached.placeholder));
        assert!(runtime.cache.contains_key("new"));
        let ItemContent::Text { text, .. } = &runtime.snapshot.items[last].content else {
            unreachable!()
        };
        assert!(text.ends_with("More words.\n"));
    }

    #[test]
    fn cooling_releases_the_presentation_and_the_clients_body() {
        let (mut runtime, handle) = shown(items());
        runtime.set_visible(false).unwrap();
        runtime.cool();
        assert!(runtime.warm_stamp().is_none());
        assert!(runtime.cache.is_empty());
        assert!(runtime.snapshot.items.is_empty());
        assert!(handle.resident_snapshot_with_clock().is_none());
        // Showing it again waits for the daemon's hydration, as before.
        runtime.set_visible(true).unwrap();
        assert!(runtime.cache.is_empty());
    }

    #[test]
    fn a_chat_too_large_to_keep_goes_cold_as_it_hides() {
        let body = "word ".repeat(CHAT_BYTES / 5 + 1);
        let (mut runtime, handle) = attached(vec![answer("huge", body)]);
        assert!(runtime.warm_bytes() > CHAT_BYTES);
        runtime.set_visible(false).unwrap();
        assert!(runtime.warm_stamp().is_none());
        assert!(runtime.cache.is_empty());
        assert!(handle.resident_snapshot_with_clock().is_none());
    }

    #[test]
    fn a_warm_chat_whose_client_lost_its_body_goes_cold() {
        let (mut runtime, handle) = shown(items());
        runtime.set_visible(false).unwrap();
        // A resync or policy change replaced the kept body with metadata.
        handle.set_interest(false, false).unwrap();
        runtime.refresh();
        assert!(runtime.warm_stamp().is_none());
        assert!(runtime.cache.is_empty());
    }

    #[test]
    fn a_warm_chat_that_reads_while_hidden_goes_cold_instead_of_preparing() {
        let (mut runtime, handle) = shown(items());
        // A local follow-up keeps the client reading while the chat is hidden.
        runtime.followups.push(Followup {
            id: 0,
            text: "Then this".into(),
            preview: "Then this".into(),
            attachments: Vec::new(),
        });
        runtime.set_visible(false).unwrap();
        assert!(runtime.warm_stamp().is_some());
        runtime.refresh();
        assert!(runtime.warm_stamp().is_some(), "nothing new, so still warm");

        let mut next = handle.snapshot().as_ref().clone();
        next.generation = 2;
        next.items.push(answer("new", "A new answer.".into()));
        handle.deliver_snapshot_for_test(next);
        runtime.refresh();
        assert!(runtime.warm_stamp().is_none());
        assert!(runtime.cache.is_empty(), "nothing is prepared while hidden");
        assert!(!runtime.preparation.waiting());
        assert_eq!(runtime.snapshot.generation, 2, "its metadata still follows");
    }

    #[test]
    fn a_parked_warm_chat_follows_a_syntax_theme_change() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (mut runtime, _handle) = shown(items());
        runtime.set_visible(false).unwrap();
        assert!(runtime.warm_stamp().is_some());
        let card = runtime.target.card;
        app.chat_ui.parked.insert(card, Box::new(runtime));
        let previous = app.settings.clone();
        let next = crate::settings::EditorSyntaxTheme::Ocean;
        assert_ne!(previous.editor_syntax_theme, next);
        app.settings.editor_syntax_theme = next;
        drop(app.synchronize_settings(&previous));
        assert_eq!(app.chat_ui.parked[&card].syntax_theme, next);
    }

    #[test]
    fn the_budget_keeps_the_most_recently_hidden_chats_warm() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let mut cards = Vec::new();
        // One chat hidden per channel switch, as each switch applies the
        // budget once.
        for _ in 0..CHATS + 2 {
            let (mut runtime, _handle) = shown(items());
            runtime.set_visible(false).unwrap();
            let card = runtime.target.card;
            app.cards.insert(card, CardRuntime::Chat(Box::new(runtime)));
            cards.push(card);
            app.trim_warm_chats();
        }
        let warm = |app: &Cagents, card: &CardId| match app.cards.get(card) {
            Some(CardRuntime::Chat(runtime)) => runtime.warm_stamp().is_some(),
            _ => unreachable!(),
        };
        assert!(!warm(&app, &cards[0]) && !warm(&app, &cards[1]));
        assert!(cards[2..].iter().all(|card| warm(&app, card)));
        // Nothing went warm since, so another pass changes nothing.
        app.trim_warm_chats();
        assert!(cards[2..].iter().all(|card| warm(&app, card)));
    }

    /// Switching scope hides every pane of the one left before the returning
    /// panes are laid out and shown. The budget must wait for them: with eight
    /// others hidden since, the chat being returned to is otherwise the one
    /// hidden longest, and would be cooled just as it is shown.
    #[test]
    fn returning_to_a_channel_after_eight_others_keeps_its_chat_ready() {
        use crate::model::{ChannelId, ProjectChannel, ProjectId};
        use crate::repository::RepositoryId;
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let project = ProjectId::new();
        app.sidebar_model
            .projects
            .push(crate::sidebar::SidebarProject::new(
                project,
                RepositoryId::new(),
                "project".into(),
                "/repo".into(),
                vec![crate::sidebar::SidebarWorktree::from_durable(
                    crate::model::WorktreeId::new(),
                    "/repo".into(),
                    Some("main".into()),
                )],
                Vec::new(),
            ));
        let mut channels = Vec::new();
        let mut cards = Vec::new();
        for ordinal in 0..=CHATS {
            let (runtime, _handle) = shown(items());
            let card = runtime.target.card;
            let channel = ChannelId::new();
            let durable = Arc::make_mut(&mut app.durable);
            let mut state = durable.cards[0].clone();
            state.id = card;
            state.kind = crate::model::CardKind::Chat(runtime.session.clone());
            durable.cards.push(state);
            durable.channels.push(ProjectChannel {
                title_source: Default::default(),
                id: channel,
                project_id: project,
                title: format!("Channel {ordinal}"),
                ordinal: ordinal as u32,
                shared: false,
                cards: vec![card],
                focused: Some(card),
            });
            app.cards.insert(card, CardRuntime::Chat(Box::new(runtime)));
            channels.push(channel);
            cards.push(card);
        }
        app.refresh_sidebar_channels();
        app.workspace.resize_viewport(1200.0);
        let runtime = |app: &Cagents, card: &CardId| match app.cards.get(card) {
            Some(CardRuntime::Chat(runtime)) => {
                let ready = !runtime.cache.is_empty()
                    && runtime.cache.values().all(|cached| !cached.placeholder);
                (runtime.visible.load(Ordering::Relaxed), ready)
            }
            _ => unreachable!(),
        };
        // Read the first channel, then visit the other eight in turn.
        for channel in &channels {
            drop(app.select_channel(*channel));
        }
        assert_eq!(runtime(&app, &cards[0]), (false, true), "kept warm");
        drop(app.select_channel(channels[0]));
        assert_eq!(
            runtime(&app, &cards[0]),
            (true, true),
            "the chat returned to is shown as it was left"
        );
        assert!(
            cards[1..]
                .iter()
                .all(|card| runtime(&app, card) == (false, true)),
            "the eight hidden since stay within the budget"
        );
    }

    #[test]
    fn the_most_recently_hidden_chats_stay_warm_within_the_budget() {
        // Ten small chats hidden one at a time: the eight hidden last stay.
        let small: Vec<_> = (1..=10).map(|stamp| (stamp, 1 << 20)).collect();
        assert_eq!(over_budget(&small), [1, 0]);
        // Bytes bind before the count does: the newest chat is large, so
        // only it and the next one fit.
        let large = [(1, 40 << 20), (2, 40 << 20), (3, 40 << 20), (4, 50 << 20)];
        let mut released = over_budget(&large);
        released.sort_unstable();
        assert_eq!(released, [0, 1]);
        // A smaller older chat still fits beside newer ones that leave room.
        let mixed = [(1, 1 << 20), (2, 60 << 20), (3, 40 << 20)];
        assert_eq!(over_budget(&mixed), [1]);
        assert!(over_budget(&[]).is_empty());
    }

    #[test]
    fn chats_hidden_together_stay_warm_or_go_cold_together() {
        // Three channels of three panes, hidden in turn: the oldest channel
        // goes whole, rather than one pane of the middle one.
        let channels: Vec<_> = (1..=3)
            .flat_map(|stamp| std::iter::repeat_n((stamp, 1 << 20), 3))
            .collect();
        let mut released = over_budget(&channels);
        released.sort_unstable();
        assert_eq!(released, [0, 1, 2]);
        // A channel over the budget by itself keeps what fits of it.
        let crowded: Vec<_> = std::iter::repeat_n((1, 1 << 20), CHATS + 1).collect();
        assert_eq!(over_budget(&crowded), [CHATS]);
        // Hides stamped between applications of the budget are distinct.
        let first = super::stamp();
        assert_eq!(super::stamp(), first);
        assert!(super::take_dirty());
        assert!(super::stamp() > first);
        assert!(super::take_dirty());
    }

    /// A long working conversation: each exchange reasons before answering,
    /// every third one runs a command, and each answer is mostly short code
    /// lines, which its row's height estimate counts as far fewer lines than
    /// it lays out.
    fn long_conversation() -> Vec<Arc<chat::Item>> {
        let code: String = (0..34)
            .map(|line| format!("    let value_{line} = state.next({line});\n"))
            .collect();
        let mut items = Vec::new();
        for step in 1..=30 {
            items.push(Arc::new(chat::Item {
                id: format!("{step}-prompt"),
                role: chat::Role::User,
                content: ItemContent::Text {
                    text: format!("Next, step {step}: keep going and show the code."),
                    thinking: false,
                    phase: chat::Phase::Final,
                },
                started_at: None,
                at: None,
            }));
            items.push(Arc::new(chat::Item {
                id: format!("{step}-thinking"),
                role: chat::Role::Assistant,
                content: ItemContent::Text {
                    text: "I should read the stream first, then its caller. ".repeat(6),
                    thinking: true,
                    phase: chat::Phase::Final,
                },
                started_at: None,
                at: None,
            }));
            if step % 3 == 0 {
                items.push(Arc::new(chat::Item {
                    id: format!("{step}-command"),
                    role: chat::Role::Assistant,
                    content: ItemContent::Command {
                        label: String::new(),
                        command: "cargo test -p ledger".into(),
                        working_directory: "/project".into(),
                        action: chat::CommandAction::Run,
                        status: chat::ToolStatus::Succeeded,
                        exit_code: Some(0),
                        duration_ms: Some(1_250),
                        output: "test ledger::case ... ok\n".repeat(14),
                    },
                    started_at: None,
                    at: None,
                }));
            }
            items.push(answer(
                &format!("{step}-answer"),
                format!(
                    "## {step}. Stream the ledger\n\nThe failure comes from \
                     **LedgerStream** holding state across calls.\n\n- One.\n- Two.\n\n\
                     ```rust\n{code}```\n\nTo check it locally:\n\n```sh\ncargo test\n```\n\n\
                     | Check | Before | After |\n| --- | --- | --- |\n| Tests | 37 | 39 |\n"
                ),
            ));
        }
        items
    }

    /// Switching channel drops a pane's widget tree, so the returning
    /// transcript is laid out afresh with only the rows near its end mounted:
    /// the rows the reader left are placeholders at first. Whether the chat
    /// stayed warm or went cold, the reader sees the same row at the top, at
    /// the same offset within it, as when they left.
    #[test]
    fn returning_to_a_channel_shows_the_reader_the_same_place() {
        use crate::model::{ChannelId, ProjectChannel, ProjectId};
        use crate::repository::RepositoryId;
        use cosmic::iced::advanced::widget::{Operation, operation};
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{Event, Point, Rectangle, Size, Vector, mouse};

        /// The transcript's top, and the bounds of each message laid out.
        struct Reader {
            scroll: widget::Id,
            ids: Vec<(widget::Id, String)>,
            content: Option<Rectangle>,
            top: f32,
            rows: Vec<(String, Rectangle)>,
        }
        impl Operation for Reader {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn custom(
                &mut self,
                _: Option<&widget::Id>,
                _: Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                // Only messages laid out, not their placeholders' bounds.
                if let Some(policy) = state.downcast_mut::<virtualized::PlaceholderPolicy>() {
                    policy.visit = false;
                }
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                _: Rectangle,
                content: Rectangle,
                translation: Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                if id == Some(&self.scroll) {
                    self.content = Some(content);
                    self.top = translation.y;
                }
            }
            fn container(&mut self, id: Option<&widget::Id>, bounds: Rectangle) {
                if let Some((_, item)) = self.ids.iter().find(|(known, _)| Some(known) == id) {
                    self.rows.push((item.clone(), bounds));
                }
            }
        }
        impl Reader {
            /// The innermost message at the top of the transcript, or the next
            /// one below it, and how far into it the top is.
            fn place(&self) -> Option<(String, f32)> {
                let content = self.content?;
                let rows = || {
                    self.rows
                        .iter()
                        .map(|(item, bounds)| (item, bounds.y - content.y, bounds.height))
                };
                rows()
                    .filter(|(_, y, height)| *y <= self.top && y + height > self.top)
                    .max_by(|a, b| a.1.total_cmp(&b.1))
                    .or_else(|| {
                        rows()
                            .filter(|(_, y, _)| *y > self.top)
                            .min_by(|a, b| a.1.total_cmp(&b.1))
                    })
                    .map(|(item, y, _)| (item.clone(), self.top - y))
            }
        }

        let items = long_conversation();
        for cold in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(true);
            let project = ProjectId::new();
            app.sidebar_model
                .projects
                .push(crate::sidebar::SidebarProject::new(
                    project,
                    RepositoryId::new(),
                    "project".into(),
                    "/repo".into(),
                    vec![crate::sidebar::SidebarWorktree::from_durable(
                        crate::model::WorktreeId::new(),
                        "/repo".into(),
                        Some("main".into()),
                    )],
                    Vec::new(),
                ));
            let mut channels = Vec::new();
            let mut handles = Vec::new();
            for (ordinal, items) in [items.clone(), self::items()].into_iter().enumerate() {
                let (runtime, handle) = shown(items);
                let card = runtime.target.card;
                let durable = Arc::make_mut(&mut app.durable);
                let mut state = durable.cards[0].clone();
                state.id = card;
                state.kind = crate::model::CardKind::Chat(runtime.session.clone());
                durable.cards.push(state);
                let channel = ChannelId::new();
                durable.channels.push(ProjectChannel {
                    title_source: Default::default(),
                    id: channel,
                    project_id: project,
                    title: format!("Channel {ordinal}"),
                    ordinal: ordinal as u32,
                    shared: false,
                    cards: vec![card],
                    focused: Some(card),
                });
                app.cards.insert(card, CardRuntime::Chat(Box::new(runtime)));
                channels.push((channel, card));
                handles.push(handle);
            }
            app.refresh_sidebar_channels();
            app.workspace.resize_viewport(1200.0);
            let (reading, card) = channels[0];
            let mut renderer = crate::app::tests::headless_renderer();
            let mut cache = Cache::default();
            // One frame of the whole window, as the shell runs it after an
            // update, with the pointer over the transcript.
            let mut frame = |app: &mut Cagents, cache: &mut Cache, events: &[Event]| {
                let pointer = Point::new(700.0, 500.0);
                let mut ui = UserInterface::build(
                    app.view(),
                    Size::new(1500.0, 940.0),
                    std::mem::take(cache),
                    &mut renderer,
                );
                let mut messages = Vec::new();
                let mut redraw = vec![Event::Window(cosmic::iced::window::Event::RedrawRequested(
                    cosmic::iced::time::Instant::now(),
                ))];
                redraw.extend_from_slice(events);
                ui.update(
                    &redraw,
                    mouse::Cursor::Available(pointer),
                    &mut renderer,
                    &mut cosmic::iced::advanced::clipboard::Null,
                    &mut messages,
                );
                ui.draw(
                    &mut renderer,
                    &cosmic::Theme::dark(),
                    &cosmic::iced::advanced::renderer::Style::default(),
                    mouse::Cursor::Available(pointer),
                );
                let mut reader = Reader {
                    scroll: card_control_id(card, "chat-transcript"),
                    ids: items
                        .iter()
                        .map(|item| (scroll::item_id(&item.id), item.id.clone()))
                        .collect(),
                    content: None,
                    top: 0.0,
                    rows: Vec::new(),
                };
                ui.operate(&renderer, &mut reader);
                *cache = ui.into_cache();
                for message in messages {
                    drop(app.update(message));
                }
                reader.place()
            };

            drop(app.select_channel(reading));
            for _ in 0..3 {
                frame(&mut app, &mut cache, &[]);
            }
            let wheel = Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            });
            for _ in 0..240 {
                frame(&mut app, &mut cache, std::slice::from_ref(&wheel));
            }
            // Read on up to a place well into an answer, not at its start.
            let mut left = frame(&mut app, &mut cache, &[]).expect("the reader's place");
            for _ in 0..40 {
                if left.0.ends_with("-answer") && left.1 > 200.0 {
                    break;
                }
                frame(&mut app, &mut cache, std::slice::from_ref(&wheel));
                left = frame(&mut app, &mut cache, &[]).expect("the reader's place");
            }
            assert!(
                left.0.ends_with("-answer") && left.1 > 200.0,
                "the reader is well into an answer: {left:?}"
            );

            drop(app.select_channel(channels[1].0));
            frame(&mut app, &mut cache, &[]);
            let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
                unreachable!()
            };
            assert!(runtime.warm_stamp().is_some(), "the chat left stays warm");
            if cold {
                runtime.cool();
            }
            drop(app.select_channel(reading));
            if cold {
                // The daemon hydrates the cold chat again, and its messages are
                // prepared while the reader waits.
                frame(&mut app, &mut cache, &[]);
                let mut hydrated = handles[0].snapshot().as_ref().clone();
                hydrated.items = items.clone();
                handles[0].deliver_snapshot_for_test(hydrated);
                let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
                    unreachable!()
                };
                runtime.refresh();
                frame(&mut app, &mut cache, &[]);
                let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
                    unreachable!()
                };
                preparation::drain_fixture(runtime);
            }
            let returned = frame(&mut app, &mut cache, &[]).expect("the reader's place");
            assert_eq!(returned.0, left.0, "cold: {cold}");
            assert!(
                (returned.1 - left.1).abs() <= 1.0,
                "cold: {cold}: the reader left {left:?} and returned to {returned:?}"
            );
            for _ in 0..3 {
                assert_eq!(frame(&mut app, &mut cache, &[]), Some(returned.clone()));
            }
        }
    }
}
