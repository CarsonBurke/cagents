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
//! it hides, and [`Budget`] keeps at most [`CHATS`] warm chats within
//! [`BYTES`], releasing the least recently hidden first. The estimate is the
//! runtime's own count of what it retains, not process memory; see
//! `docs/performance.md` for how it compares with measured RSS.
use std::cell::Cell;

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

/// The order a chat was hidden in; later hides are larger.
pub(super) fn stamp() -> u64 {
    DIRTY.with(|dirty| dirty.set(true));
    NEXT.with(|next| {
        let stamp = next.get();
        next.set(stamp + 1);
        stamp
    })
}

/// Whether a chat went warm since the budget was last applied.
pub(super) fn take_dirty() -> bool {
    DIRTY.with(|dirty| dirty.replace(false))
}

/// Which warm chats to release, given each one's hide stamp and retained
/// bytes: the positions of those outside the budget. Chats are kept from the
/// most recently hidden back, each while it still fits.
pub(super) fn over_budget(warm: &[(u64, usize)]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..warm.len()).collect();
    order.sort_unstable_by_key(|&index| std::cmp::Reverse(warm[index].0));
    let (mut kept, mut bytes) = (0, 0usize);
    order
        .into_iter()
        .filter(|&index| {
            let size = warm[index].1;
            let fits = kept < CHATS && bytes.saturating_add(size) <= BYTES;
            if fits {
                kept += 1;
                bytes += size;
            }
            !fits
        })
        .collect()
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
    fn the_budget_keeps_the_most_recently_hidden_chats_warm() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let mut cards = Vec::new();
        for _ in 0..CHATS + 2 {
            let (mut runtime, _handle) = shown(items());
            runtime.set_visible(false).unwrap();
            let card = runtime.target.card;
            app.cards.insert(card, CardRuntime::Chat(Box::new(runtime)));
            cards.push(card);
        }
        app.trim_warm_chats();
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

    #[test]
    fn the_most_recently_hidden_chats_stay_warm_within_the_budget() {
        // Ten small chats: the eight hidden last stay warm.
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
}
