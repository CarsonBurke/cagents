use super::*;
fn item(body: &str) -> Arc<chat::Item> {
    Arc::new(chat::Item {
        id: "message".into(),
        role: chat::Role::Assistant,
        started_at: None,
        at: None,
        content: ItemContent::Text {
            text: body.into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
    })
}
fn target() -> Target {
    Target {
        card: CardId::new(),
        generation: 1,
    }
}
fn configured(target: Target, item: &Arc<chat::Item>) -> State {
    let mut state = State::default();
    state.context(target, Some("session"), None, std::slice::from_ref(item));
    state
}
fn job(state: &State) -> (u64, Arc<Shared>) {
    let entry = &state.entries["message"];
    (entry.run, entry.shared.clone())
}
/// The ranks that obtain `pool`'s permits, in the order they do, when
/// waiters with `ranks` queue behind permits that are all held.
fn grants(pool: Pool, held: usize, ranks: &[u64], late: Option<u64>) -> Vec<u64> {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    executor.block_on(async {
        let pool = Arc::new(pool);
        let blockers: Vec<_> = (0..held)
            .map(|_| pool.permits.clone().try_acquire_owned().unwrap())
            .collect();
        let (granted, mut order) = tokio::sync::mpsc::unbounded_channel();
        let spawn = |rank: u64| {
            let (pool, granted) = (pool.clone(), granted.clone());
            tokio::spawn(async move {
                let permit = pool.acquire(&AtomicU64::new(rank), None).await.unwrap();
                granted.send(rank).unwrap();
                // Hold the parser briefly, as a worker does.
                tokio::task::yield_now().await;
                drop(permit);
            })
        };
        let mut tasks: Vec<_> = ranks.iter().map(|&rank| spawn(rank)).collect();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        tasks.extend(late.map(spawn));
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        drop(blockers);
        for task in tasks {
            task.await.unwrap();
        }
        drop(granted);
        let mut grants = Vec::new();
        while let Some(rank) = order.recv().await {
            grants.push(rank);
        }
        grants
    })
}

#[test]
fn parsers_go_to_the_best_ranked_waiters_first() {
    // Arrival order is the order subscriptions happened to start in.
    assert_eq!(
        grants(Pool::new(1), 1, &[40, 2, 30, 1, 20], None),
        [1, 2, 20, 30, 40]
    );
    // A row the reader returned to, ranked after others already queued,
    // still goes before them.
    assert_eq!(
        grants(Pool::new(1), 1, &[40, 30, 20], Some(1)),
        [1, 20, 30, 40]
    );
    // Equal ranks keep arrival order.
    assert_eq!(grants(Pool::new(1), 1, &[5, 5, 5], None), [5, 5, 5]);
}

#[test]
fn a_waiter_that_stops_waiting_gives_up_its_place() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    executor.block_on(async {
        let pool = Arc::new(Pool::new(1));
        let held = pool.permits.clone().try_acquire_owned().unwrap();
        let first = tokio::spawn({
            let pool = pool.clone();
            async move { pool.acquire(&AtomicU64::new(1), None).await.map(drop) }
        });
        tokio::task::yield_now().await;
        let second = tokio::spawn({
            let pool = pool.clone();
            async move { pool.acquire(&AtomicU64::new(2), None).await.map(drop) }
        });
        tokio::task::yield_now().await;
        // The first in line is cancelled, as when its message is replaced.
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(pool.first(), Some((2, 1)));
        drop(held);
        tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .expect("the next waiter is granted the released parser")
            .unwrap()
            .unwrap();
        assert!(pool.lock().waiting.is_empty());
    });
}

/// Spawns a waiter at `rank` that reports its rank when granted and
/// holds the parser until `release` says so.
fn waiter(
    pool: &Arc<Pool>,
    rank: u64,
    place: Option<Arc<Place>>,
    granted: &tokio::sync::mpsc::UnboundedSender<u64>,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let (pool, granted) = (pool.clone(), granted.clone());
    let task = tokio::spawn(async move {
        let permit = pool
            .acquire(&AtomicU64::new(rank), place.as_deref())
            .await
            .unwrap();
        granted.send(rank).unwrap();
        let _ = released.await;
        drop(permit);
    });
    (task, release)
}
async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}
fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn ranking_a_waiting_input_again_moves_it() {
    current_thread().block_on(async {
        let pool = Arc::new(Pool::new(1));
        let held = pool.permits.clone().try_acquire_owned().unwrap();
        let (granted, mut order) = tokio::sync::mpsc::unbounded_channel();
        let place = Arc::new(Place::default());
        let mut releases: HashMap<u64, _> = [(30, Some(place.clone())), (20, None), (10, None)]
            .into_iter()
            .map(|(rank, place)| (rank, waiter(&pool, rank, place, &granted).1))
            .collect();
        settle().await;
        // The reader scrolled to the oldest one while it waited.
        pool.rerank(&place, 5);
        assert_eq!(pool.first(), Some((5, 0)));
        drop(held);
        let mut grants = Vec::new();
        for _ in 0..3 {
            let rank = order.recv().await.unwrap();
            grants.push(rank);
            let _ = releases.remove(&rank).unwrap().send(());
        }
        // Waiters report the rank they arrived with.
        assert_eq!(grants, [30, 10, 20]);
    });
}

#[test]
fn ranking_every_waiter_alike_leaves_the_head_contending() {
    current_thread().block_on(async {
        let pool = Arc::new(Pool::new(1));
        let held = pool.permits.clone().try_acquire_owned().unwrap();
        let (granted, mut order) = tokio::sync::mpsc::unbounded_channel();
        let places: Vec<_> = (0..3).map(|_| Arc::new(Place::default())).collect();
        let mut releases: Vec<_> = places
            .iter()
            .zip([10, 20, 30])
            .map(|(place, rank)| waiter(&pool, rank, Some(place.clone()), &granted).1)
            .collect();
        settle().await;
        let wakes = pool.wakes.load(Ordering::Relaxed);
        // A message arrives: everything already waiting is one further
        // from the newest.
        for (place, rank) in places.iter().zip([11, 21, 31]) {
            pool.rerank(place, rank);
        }
        settle().await;
        assert_eq!(pool.lock().head, Some((11, 0)));
        assert_eq!(
            pool.wakes.load(Ordering::Relaxed),
            wakes,
            "nobody is woken when the order stays the same"
        );
        drop(held);
        for expected in [10, 20, 30] {
            assert_eq!(order.recv().await, Some(expected));
            let _ = releases.remove(0).send(());
        }
    });
}

#[test]
fn a_rank_given_as_an_input_joins_the_queue_is_kept() {
    current_thread().block_on(async {
        let pool = Arc::new(Pool::new(1));
        let _held = pool.permits.clone().try_acquire_owned().unwrap();
        let priority = Arc::new(AtomicU64::new(30));
        let place = Arc::new(Place::default());
        // The UI ranks the input again after its subscription started
        // waiting, but before it has a place in the queue to move.
        *pool.arriving.lock().unwrap() = Some(Box::new({
            let (pool, priority, place) = (pool.clone(), priority.clone(), place.clone());
            move || pool.rank(&priority, &place, 5)
        }));
        let waiting = tokio::spawn({
            let (pool, priority, place) = (pool.clone(), priority.clone(), place.clone());
            async move { pool.acquire(&priority, Some(&place)).await.map(drop) }
        });
        settle().await;
        assert_eq!(pool.first(), Some((5, 0)));
        waiting.abort();
    });
}

#[test]
fn the_growing_tail_leaves_a_parser_for_the_rest() {
    current_thread().block_on(async {
        let pool = Arc::new(Pool::new(2));
        let (granted, mut order) = tokio::sync::mpsc::unbounded_channel();
        // One streaming chat parses its tail.
        let (_first, first_release) = waiter(&pool, TAIL, None, &granted);
        settle().await;
        assert_eq!(order.recv().await, Some(TAIL));
        let held = pool.permits.clone().try_acquire_owned().unwrap();
        // Another chat's tail and some history wait for the other parser.
        let (_second, second_release) = waiter(&pool, TAIL, None, &granted);
        let (_history, history_release) = waiter(&pool, 9, None, &granted);
        settle().await;
        drop(held);
        assert_eq!(
            order.recv().await,
            Some(9),
            "the last parser goes to history while a tail holds the other"
        );
        let _ = first_release.send(());
        assert_eq!(order.recv().await, Some(TAIL));
        let _ = second_release.send(());
        let _ = history_release.send(());
    });
}

#[test]
fn the_tail_takes_a_free_parser_at_once_only_when_nobody_waits() {
    current_thread().block_on(async {
        let pool = Arc::new(Pool::new(2));
        let first = pool.try_acquire_tail().expect("a free parser and no queue");
        assert_eq!(pool.lock().tail_held, 1);
        let (granted, mut order) = tokio::sync::mpsc::unbounded_channel();
        let (_history, history_release) = waiter(&pool, 9, None, &granted);
        // History joins the queue, then yields before it contends.
        tokio::task::yield_now().await;
        assert_eq!(pool.lock().waiting.len(), 1);
        assert_eq!(pool.permits.available_permits(), 1);
        assert!(
            pool.try_acquire_tail().is_none(),
            "history queued for the free parser is not overtaken"
        );
        assert_eq!(order.recv().await, Some(9));
        drop(first);
        assert_eq!(pool.lock().tail_held, 0);
        let _ = history_release.send(());
    });
}

#[test]
fn the_update_that_stages_a_streamed_publication_can_install_it() {
    use iced::futures::StreamExt;
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let _entered = executor.enter();
    let target = target();
    let theme = crate::settings::EditorSyntaxTheme::Auto;
    let first = item("The answer");
    let mut state = configured(target, &first);
    state.set_provider(Provider::Claude);
    // Without a rank the input is history and parses in its turn.
    state.stage(&first, true, theme, |_, _| false);
    assert!(matches!(
        state.settle_tail(target, Duration::from_secs(10)),
        Settled::Idle
    ));
    state.rank("message", TAIL);
    let Settled::Ready(id, run, ready) = state.settle_tail(target, Duration::from_secs(10)) else {
        panic!("a free parser prepares the tail within the budget");
    };
    assert_eq!(id, "message");
    assert!(
        state.subscriptions(target).is_empty(),
        "a handed-over run is not parsed again by a subscription"
    );
    let prepared = state.accept(target, &id, run, ready).expect("current run");
    assert!(Arc::ptr_eq(prepared.item(), &first));
    prepared.discard();

    // A parse that outlasts the budget arrives through the task.
    let second = item("The answer grows");
    state.stage(&second, true, theme, |_, _| true);
    let held = hold_retirement();
    let Settled::Pending(task) = state.settle_tail(target, Duration::from_millis(1)) else {
        panic!("the held reclaimer keeps the parse past the budget");
    };
    assert!(state.subscriptions(target).is_empty());
    assert!(matches!(
        state.settle_tail(target, Duration::from_millis(1)),
        Settled::Idle
    ));
    drop(held);
    let message = executor.block_on(async {
        iced::runtime::task::into_stream(task)
            .expect("a pending parse")
            .next()
            .await
    });
    let Some(iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
        Action::MarkdownPrepared(delivered, id, delivered_run, ready),
    )))) = message
    else {
        panic!("the task delivers the prepared publication");
    };
    assert_eq!((delivered, delivered_run), (target, job(&state).0));
    let prepared = state
        .accept(target, &id, delivered_run, ready)
        .expect("current run");
    assert!(Arc::ptr_eq(prepared.item(), &second));
    prepared.discard();

    // A run its subscription has seen may already hold a parser, and
    // is never handed over a second time.
    let third = item("The answer grows again");
    state.stage(&third, true, theme, |_, _| true);
    assert_eq!(state.subscriptions(target).len(), 1);
    let fourth = item("The answer grows again, and again");
    state.stage(&fourth, true, theme, |_, _| true);
    assert!(matches!(
        state.settle_tail(target, Duration::from_secs(10)),
        Settled::Idle
    ));
    assert_eq!(state.subscriptions(target).len(), 1);
    drop(state);
    retirement_barrier();
}

#[test]
fn a_handover_the_chat_declines_parses_again_through_its_subscription() {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let _entered = executor.enter();
    let target = target();
    let source = item("A chat that stops presenting declines this parse.");
    let mut state = configured(target, &source);
    state.stage(
        &source,
        true,
        crate::settings::EditorSyntaxTheme::Auto,
        |_, _| false,
    );
    state.rank("message", TAIL);
    let Settled::Ready(id, run, ready) = state.settle_tail(target, Duration::from_secs(10)) else {
        panic!("a free parser prepares the tail within the budget");
    };
    assert!(state.subscriptions(target).is_empty());
    // Only the handed-over run is released: a stale run or another
    // entry leaves it handed over.
    state.unclaim(&id, run + 1);
    state.unclaim("another", run);
    assert!(state.subscriptions(target).is_empty());
    drop(ready);
    state.unclaim(&id, run);
    assert_eq!(
        state.subscriptions(target).len(),
        1,
        "the declined run is parsed again rather than left unprepared"
    );
    assert!(
        matches!(
            state.settle_tail(target, Duration::from_secs(10)),
            Settled::Idle
        ),
        "a run its subscription has seen is not handed over again"
    );
    drop(state);
    retirement_barrier();
}

#[test]
fn pending_handoffs_reject_replaced_sources_and_contexts() {
    use iced::futures::StreamExt;
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let _entered = executor.enter();
    let target = target();
    let theme = crate::settings::EditorSyntaxTheme::Auto;
    for change_context in [false, true] {
        let original = item("The old answer is still being prepared.");
        let raw = Arc::downgrade(&original);
        let mut state = configured(target, &original);
        state.stage(&original, true, theme, |_, _| false);
        state.rank("message", TAIL);
        let (old_run, shared) = job(&state);
        let lease = Arc::downgrade(&shared.lease);
        let pool = shared.workers.clone();
        let capacity = pool.permits.available_permits();
        let held = hold_retirement();
        let Settled::Pending(task) = state.settle_tail(target, Duration::ZERO) else {
            panic!("the held reclaimer keeps the handoff pending");
        };
        assert_eq!(pool.permits.available_permits(), capacity - 1);

        let replacement = item("A rewritten answer belongs to the current context.");
        if change_context {
            state.context(
                target,
                Some("other-session"),
                Some("child"),
                std::slice::from_ref(&replacement),
            );
        }
        state.stage(&replacement, true, theme, |_, _| false);
        let current_run = job(&state).0;
        assert_ne!(current_run, old_run);
        assert!(shared.cancelled.load(Ordering::Relaxed));
        drop(original);
        drop(shared);
        // Release before awaiting: execute must cross this same barrier.
        drop(held);
        let message = executor.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(10),
                iced::runtime::task::into_stream(task).unwrap().next(),
            )
            .await
            .expect("the cancelled handoff still delivers its outcome")
        });
        let Some(iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
            Action::MarkdownPrepared(delivered, id, delivered_run, ready),
        )))) = message
        else {
            panic!("the task delivers the old handoff outcome");
        };
        assert_eq!((delivered, delivered_run), (target, old_run));
        assert!(state.accept(target, &id, delivered_run, ready).is_none());
        assert_eq!(job(&state).0, current_run);
        assert!(
            state.error().is_none(),
            "old cancellation cannot fail the replacement"
        );
        retirement_barrier();
        assert!(raw.upgrade().is_none());
        assert!(lease.upgrade().is_none());
        assert_eq!(pool.permits.available_permits(), capacity);

        state.rank("message", TAIL);
        let Settled::Ready(id, run, ready) = state.settle_tail(target, Duration::from_secs(10))
        else {
            panic!("the replacement can use the released parser");
        };
        let prepared = state.accept(target, &id, run, ready).unwrap();
        assert!(Arc::ptr_eq(prepared.item(), &replacement));
        prepared.discard();
        drop(state);
        retirement_barrier();
        assert_eq!(pool.permits.available_permits(), capacity);
    }
}

#[test]
fn dropping_a_pending_handoff_task_retires_its_output_and_releases_its_parser() {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let entered = executor.enter();
    let target = target();
    let source = item("The abandoned task still owns an actual parser result.");
    let raw = Arc::downgrade(&source);
    let mut state = configured(target, &source);
    state.stage(
        &source,
        true,
        crate::settings::EditorSyntaxTheme::Auto,
        |_, _| false,
    );
    state.rank("message", TAIL);
    let (_, shared) = job(&state);
    let lease = Arc::downgrade(&shared.lease);
    let pool = shared.workers.clone();
    let capacity = pool.permits.available_permits();
    drop(shared);
    let held = hold_retirement();
    let Settled::Pending(task) = state.settle_tail(target, Duration::ZERO) else {
        panic!("the held reclaimer keeps the handoff pending");
    };
    drop(task);
    assert_eq!(pool.permits.available_permits(), capacity - 1);
    // Keep State alive until the worker has finished, so it produces a
    // successful result rather than merely observing cancellation.
    drop(held);
    drop(entered);
    executor.shutdown_timeout(Duration::from_secs(10));
    retirement_barrier();
    assert!(
        state.growing_canonical_for_test(&source).is_none(),
        "an abandoned result was never accepted by the UI"
    );
    assert!(
        state.entries["message"]
            .shared
            .canonical
            .lock()
            .unwrap()
            .is_some()
    );
    assert_eq!(pool.permits.available_permits(), capacity);
    drop(state);
    drop(source);
    retirement_barrier();
    assert!(raw.upgrade().is_none());
    assert!(lease.upgrade().is_none());
}

#[test]
fn a_contending_tail_yields_the_head_to_history_that_arrives() {
    current_thread().block_on(async {
        let pool = Arc::new(Pool::new(2));
        let (granted, mut order) = tokio::sync::mpsc::unbounded_channel();
        let (_first, first_release) = waiter(&pool, TAIL, None, &granted);
        settle().await;
        assert_eq!(order.recv().await, Some(TAIL));
        let held = pool.permits.clone().try_acquire_owned().unwrap();
        // Alone, another tail is the head and contends for the last
        // parser.
        let (_second, second_release) = waiter(&pool, TAIL, None, &granted);
        settle().await;
        assert_eq!(pool.lock().head.map(|key| key.0), Some(TAIL));
        // History arriving behind it takes the head from it.
        let (_history, history_release) = waiter(&pool, 9, None, &granted);
        settle().await;
        assert_eq!(pool.lock().head.map(|key| key.0), Some(9));
        drop(held);
        assert_eq!(
            order.recv().await,
            Some(9),
            "the tail that was contending leaves the last parser to history"
        );
        settle().await;
        assert!(order.try_recv().is_err(), "the tail still waits");
        let _ = history_release.send(());
        assert_eq!(order.recv().await, Some(TAIL));
        let _ = first_release.send(());
        let _ = second_release.send(());
    });
}

#[test]
fn waiters_behind_the_head_are_not_woken_by_others_coming_and_going() {
    current_thread().block_on(async {
        let pool = Arc::new(Pool::new(1));
        let held = pool.permits.clone().try_acquire_owned().unwrap();
        let (granted, mut order) = tokio::sync::mpsc::unbounded_channel();
        let waiters: Vec<_> = (1..=64)
            .map(|rank| waiter(&pool, rank, None, &granted))
            .collect();
        settle().await;
        drop(held);
        for (rank, (_task, release)) in (1..=64).zip(waiters) {
            assert_eq!(order.recv().await, Some(rank));
            let _ = release.send(());
        }
        let wakes = pool.wakes.load(Ordering::Relaxed);
        assert!(
            wakes <= 2 * 64,
            "each waiter wakes about once, on reaching the head: {wakes}"
        );
    });
}

#[test]
fn a_transcript_ranks_its_newest_messages_first() {
    let session = fresh_chat_session(
        Provider::Claude,
        TerminalId::new(),
        "/project".into(),
        &Account::system(Provider::Claude),
        false,
    )
    .unwrap();
    let items: Vec<_> = (0..6)
        .map(|index| {
            Arc::new(chat::Item {
                id: format!("answer-{index}"),
                role: chat::Role::Assistant,
                content: ItemContent::Text {
                    text: format!("Answer **{index}**"),
                    thinking: false,
                    phase: chat::Phase::Final,
                },
                started_at: None,
                at: None,
            })
        })
        .collect();
    let handle = Arc::new(ClientHandle::fixture(
        session.clone(),
        Snapshot {
            ready: true,
            generation: 1,
            items,
            ..Snapshot::default()
        },
    ));
    let runtime = Runtime::new(
        target(),
        session,
        Connection::Live(handle),
        true,
        crate::settings::EditorSyntaxTheme::Auto,
    );
    let ranks: Vec<u64> = (0..6)
        .map(|index| {
            runtime.preparation.entries[&format!("answer-{index}")]
                .shared
                .priority
                .load(Ordering::Relaxed)
        })
        .collect();
    assert!(
        ranks.windows(2).all(|pair| pair[0] > pair[1]),
        "older messages wait behind newer ones: {ranks:?}"
    );
}

#[test]
fn a_saved_window_at_the_old_end_is_in_the_first_staged_wave() {
    let session = fresh_chat_session(
        Provider::Claude,
        TerminalId::new(),
        "/project".into(),
        &Account::system(Provider::Claude),
        false,
    )
    .unwrap();
    let handle = Arc::new(ClientHandle::fixture(
        session.clone(),
        Snapshot {
            ready: true,
            ..Snapshot::default()
        },
    ));
    let mut runtime = Runtime::new(
        target(),
        session,
        Connection::Live(handle.clone()),
        true,
        crate::settings::EditorSyntaxTheme::Auto,
    );
    let total = 3 * QUEUED_ITEMS;
    let items: Vec<_> = (0..total)
        .map(|index| {
            Arc::new(chat::Item {
                id: format!("answer-{index}"),
                role: chat::Role::Assistant,
                content: ItemContent::Text {
                    text: format!("Answer **{index}**"),
                    thinking: false,
                    phase: chat::Phase::Final,
                },
                started_at: None,
                at: None,
            })
        })
        .collect();
    // The reader left the main transcript near its oldest messages,
    // far beyond the newest-back reach of one wave.
    let window = ["answer-2", "answer-3", "answer-4"];
    runtime.scroll_memory.save_window(None, &window);
    handle.replace_snapshot_for_test(Snapshot {
        generation: 1,
        ready: true,
        items,
        ..Snapshot::default()
    });
    runtime.refresh();
    assert!(
        runtime.preparation.waiting(),
        "the wave is bounded and the rest waits"
    );
    assert!(runtime.preparation.entries.len() <= QUEUED_ITEMS);
    for id in window {
        let rank = runtime
            .preparation
            .entries
            .get(id)
            .unwrap_or_else(|| panic!("{id} from the saved window is staged in the first wave"))
            .shared
            .priority
            .load(Ordering::Relaxed);
        assert_eq!(rank, 1, "{id} ranks as a reading row");
    }
    assert!(
        runtime
            .preparation
            .contains(&format!("answer-{}", total - 1)),
        "the newest message still follows the saved window"
    );
    assert!(
        !runtime.preparation.contains("answer-1"),
        "history next to the saved window waits its newest-back turn"
    );
}

#[test]
fn another_applications_waiters_leave_this_queue_alone() {
    let ours = workers();
    let theirs = std::thread::spawn(workers).join().unwrap();
    assert!(!Arc::ptr_eq(&ours, &theirs));
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    executor.block_on(async {
        let _full = theirs
            .permits
            .clone()
            .try_acquire_many_owned(theirs.permits.available_permits() as u32)
            .unwrap();
        // Their best-ranked waiter is stuck behind their own full pool.
        let stuck = tokio::spawn({
            let theirs = theirs.clone();
            async move { theirs.acquire(&AtomicU64::new(0), None).await.map(drop) }
        });
        tokio::task::yield_now().await;
        assert_eq!(theirs.first(), Some((0, 0)));
        assert_eq!(ours.first(), None);
        tokio::time::timeout(
            Duration::from_secs(5),
            ours.acquire(&AtomicU64::new(UNRANKED), None),
        )
        .await
        .expect("our worst-ranked waiter still gets one of our parsers")
        .unwrap();
        stuck.abort();
    });
}

fn permit() -> tokio::sync::OwnedSemaphorePermit {
    Arc::new(tokio::sync::Semaphore::new(1))
        .try_acquire_owned()
        .unwrap()
}
fn ready(prepared: Result<Prepared, String>) -> Ready {
    Ready::new(prepared)
}

#[test]
fn parser_jobs_keep_the_originating_application_admission_across_executor_threads() {
    use iced::futures::FutureExt;
    let source = item("A real parser job from the originating application.");
    let mut state = configured(target(), &source);
    state.set_provider(Provider::Claude);
    state.stage(
        &source,
        false,
        crate::settings::EditorSyntaxTheme::Auto,
        |_, _| false,
    );
    let (_, shared) = job(&state);
    let originating_workers = workers().permits.clone();
    let capacity = originating_workers.available_permits();
    let observed_workers = originating_workers.clone();
    let held = hold_retirement();
    let (report, observed) = std::sync::mpsc::channel();
    let (proceed, resumed) = tokio::sync::oneshot::channel();
    let executor_thread = std::thread::spawn(move || {
        // A fresh thread is a different test application with its own
        // pool. Exhausting that pool must not touch the constructing UI's.
        let foreign = workers().permits.clone();
        let all = foreign.available_permits() as u32;
        let _foreign_application = foreign.try_acquire_many_owned(all).unwrap();
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        executor.block_on(async {
            let mut output = Box::pin(run(shared));
            // Admission yields once so a wave of subscriptions queues
            // before any contends; then the real worker holds its slot
            // and cannot pass its reclamation barrier yet.
            for _ in 0..2 {
                assert!(output.as_mut().now_or_never().is_none());
            }
            report.send(observed_workers.available_permits()).unwrap();
            if resumed.await.unwrap() {
                Some(output.await)
            } else {
                // A broken ownership implementation must fail without
                // leaving a job waiting on its own foreign permit guard.
                None
            }
        })
    });
    let available = observed.recv().unwrap();
    drop(held);
    proceed.send(available == capacity - 1).unwrap();
    let output = executor_thread.join().unwrap();
    assert_eq!(
        available,
        capacity - 1,
        "the dispatched job must own a slot from its constructing application"
    );
    let output = output.expect("admitted real parser job").unwrap();
    assert!(Arc::ptr_eq(output.item(), &source));
    assert_eq!(originating_workers.available_permits(), capacity - 1);
    // Discarded outputs retain their originating slot through queued
    // storage retirement, even while the reclaimer is held.
    let held = hold_retirement();
    output.discard();
    assert_eq!(originating_workers.available_permits(), capacity - 1);
    drop(held);
    retirement_barrier();
    assert_eq!(originating_workers.available_permits(), capacity);
    drop(state);
    retirement_barrier();
}

fn literal_runtime(content: ItemContent) -> (Runtime, Arc<ClientHandle>) {
    let (mut runtime, handle) = view::prepared_runtime_for_test(&[]);
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    let mut source = (*item("")).clone();
    source.content = content;
    snapshot.items.push(Arc::new(source));
    handle.publish_delta_snapshot_for_test(snapshot);
    runtime.refresh();
    (runtime, handle)
}

#[test]
fn literal_search_actual_worker_keeps_fields_and_refuses_same_id_rewrite_ready() {
    let url = format!("https://example.test/{}#L22", "u".repeat(44 * 1024));
    let content = ItemContent::WebSearch {
        query: "**literal query**".into(),
        results: vec![chat::SearchResult {
            title: String::new(),
            url: url.clone(),
            snippet: "[literal snippet](not-a-link)".into(),
        }],
    };
    let (mut runtime, handle) = literal_runtime(content);
    let source = runtime.snapshot.items[0].clone();
    assert!(runtime.prepared_literal(&source).is_none());
    let (run, shared) = job(&runtime.preparation);
    let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
        .join()
        .unwrap();
    assert!(output.parsed.markdown.items().is_empty());
    let literal_content::Prepared::Search(document) = output.literal.as_ref().unwrap() else {
        panic!("literal Search payload")
    };
    assert_ne!(document.created_on, std::thread::current().id());
    runtime.markdown_prepared(runtime.target, "message", run, ready(Ok(output)));
    let accepted = runtime.prepared_literal(&source).unwrap();
    let literal_content::Content::Search(document) = accepted else {
        panic!("installed Search")
    };
    let markdown::Item::Quote(fields) = &document.items()[1] else {
        panic!("result fields")
    };
    let markdown::Item::Paragraph(title) = &fields[0] else {
        panic!("fallback title")
    };
    assert_eq!(title.plain().as_ref(), url);
    assert!(title.plain_ready());
    let mut builder = selection::Catalog::builder();
    accepted.catalog(&mut builder, &source, true);
    let catalog = builder.finish();
    let (a, b) = catalog.endpoints().unwrap();
    assert_eq!(
        catalog.copy(a, b).unwrap(),
        format!("**literal query**\n\n{url}\n\n{url}\n\n[literal snippet](not-a-link)")
    );
    assert!(
        runtime.prepared_literal(&(*source).clone()).is_none(),
        "an equal item clone cannot authorize a prepared source"
    );
    let mut replacement = (*handle.snapshot()).clone();
    replacement.generation += 1;
    Arc::make_mut(&mut replacement.items[0]).content = ItemContent::WebSearch {
        query: "replacement".into(),
        results: Vec::new(),
    };
    handle.publish_snapshot_for_test(replacement);
    runtime.refresh();
    assert!(
        runtime
            .prepared_literal(&runtime.snapshot.items[0])
            .is_none()
    );
    let (old_run, old_shared) = job(&runtime.preparation);
    let old = std::thread::spawn(move || execute(old_shared, permit()).unwrap())
        .join()
        .unwrap();
    let mut rewritten = (*handle.snapshot()).clone();
    rewritten.generation += 1;
    Arc::make_mut(&mut rewritten.items[0]).content = ItemContent::WebSearch {
        query: "current".into(),
        results: Vec::new(),
    };
    handle.publish_snapshot_for_test(rewritten);
    runtime.refresh();
    runtime.markdown_prepared(runtime.target, "message", old_run, ready(Ok(old)));
    assert!(
        runtime
            .prepared_literal(&runtime.snapshot.items[0])
            .is_none(),
        "late ready cannot publish a previous same-ID result"
    );
    drain_fixture(&mut runtime);
    let literal_content::Content::Search(current) = runtime
        .prepared_literal(&runtime.snapshot.items[0])
        .unwrap()
    else {
        panic!("current Search")
    };
    let markdown::Item::Paragraph(query) = &current.items()[0] else {
        panic!("query")
    };
    assert_eq!(query.plain().as_ref(), "current");
    runtime.show(false);
    retirement_barrier();
}

#[test]
fn literal_notice_preview_and_actual_ready_retire_spans_before_same_permit() {
    for body in [
        "one\r\ntwo\r\nthree\r\n",
        "one\r\ntwo\r\nthree\r\nfour",
        "single huge line",
        &*"a single line too long to read at a glance ".repeat(20),
    ] {
        let cancelled = AtomicBool::new(false);
        let prepared =
            prompt::Prepared::with_policy(body, prompt::PreviewPolicy::Notice, &cancelled).unwrap();
        let long =
            body.lines().count() > view::NOTICE_LINES || body.chars().count() > view::NOTICE_CHARS;
        let expected = if long {
            body.lines()
                .take(view::NOTICE_LINES)
                .collect::<Vec<_>>()
                .join("\n")
                .chars()
                .take(view::NOTICE_CHARS)
                .collect()
        } else {
            body.to_owned()
        };
        assert_eq!(prepared.long, long);
        assert_eq!(prepared.shown(false).plain().as_ref(), expected);
        assert_eq!(prepared.full.plain().as_ref(), body);
    }
    let body = format!(
        "{}\r\nsecond\r\nthird\r\nlast",
        "literal **raw** λ ".repeat(65_536)
    );
    let (mut runtime, _handle) = literal_runtime(ItemContent::Delivery {
        kind: chat::DeliveryKind::Notice,
        from: String::new(),
        sender: String::new(),
        summary: "NONSELECTABLE SUMMARY".into(),
        body: body.clone(),
    });
    let (run, shared) = job(&runtime.preparation);
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let admitted = slots.clone().try_acquire_owned().unwrap();
    let mut output = std::thread::spawn(move || execute(shared, admitted).unwrap())
        .join()
        .unwrap();
    assert!(output.parsed.markdown.items().is_empty());
    let literal_content::Prepared::Notice(notice) = output.literal.as_mut().unwrap() else {
        panic!("notice payload")
    };
    assert_eq!(notice.body.full.plain().as_ref(), body);
    let (sent, disposed) = std::sync::mpsc::channel();
    notice.body.full.watch_raw_spans(sent);
    let hold = hold_retirement();
    runtime.show(false);
    runtime.cool();
    runtime.markdown_prepared(runtime.target, "message", run, ready(Ok(output)));
    assert!(disposed.try_recv().is_err());
    assert!(
        slots.clone().try_acquire_owned().is_err(),
        "stale literal output keeps the same output permit until actual destruction"
    );
    drop(hold);
    let worker = retirement_barrier();
    assert_eq!(
        disposed.recv_timeout(Duration::from_secs(5)).unwrap().0,
        worker
    );
    assert!(slots.try_acquire_owned().is_ok());
}

/// Claude Code names a command run without a description by the command
/// itself, and its task notification carries that name as the summary. A
/// subagent's 11 KB heredoc reached the parent conversation this way and
/// drew every line of it under a collapsed group.
#[test]
fn notice_heading_preserves_first_line_for_fade_and_multiline_detail() {
    let command = format!(
        "mkdir -p /var/tmp/fw16-audit && cat > /var/tmp/fw16-audit/sampler.py <<'EOF'\n{}EOF\npython3 /var/tmp/fw16-audit/sampler.py 2 10",
        "def sample(path):\n    with open(path) as f:\n        return f.read()\n\n".repeat(160)
    );
    let one_line = format!("cargo test {}", "--features host ".repeat(200));
    for (summary, body, heading, lead) in [
        (
            command.as_str(),
            "",
            "mkdir -p /var/tmp/fw16-audit && cat > /var/tmp/fw16-audit/sampler.py <<'EOF'",
            command.split_once('\n').unwrap().1,
        ),
        (one_line.as_str(), "exit code 0", one_line.trim(), ""),
    ] {
        let (mut runtime, _handle) = literal_runtime(ItemContent::Delivery {
            kind: chat::DeliveryKind::Notice,
            from: String::new(),
            sender: "b1".into(),
            summary: summary.into(),
            body: body.into(),
        });
        let (_run, shared) = job(&runtime.preparation);
        let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        let Some(literal_content::Prepared::Notice(notice)) = output.literal.as_ref() else {
            panic!("notice payload")
        };
        assert_eq!(notice.summary.plain().as_ref(), heading);
        let full = notice.body.full.plain();
        assert!(full.starts_with(lead), "the whole name stays on demand");
        assert!(full.ends_with(body));
        assert_eq!(
            notice.body.long,
            !lead.is_empty(),
            "only multiline details need a preview"
        );
        let shown = notice.body.shown(false).plain();
        assert!(shown.lines().count() <= view::NOTICE_LINES, "{shown}");
        assert!(shown.chars().count() <= view::NOTICE_CHARS, "{shown}");
        assert_eq!(notice.body.shown(true).plain(), full);
        runtime.show(false);
    }
    for (summary, body) in [
        (
            "Background command \"Run tests\" completed (exit code 0)",
            "",
        ),
        ("Monitor event: \"Build\"", "warning: unused import"),
    ] {
        let (heading, shown) = literal_content::notice_parts(summary, body);
        let expected = if summary.starts_with("Monitor event: ") {
            "Build"
        } else {
            summary
        };
        assert_eq!((&*heading, &*shown), (expected, body));
    }
    let (heading, shown) = literal_content::notice_parts("Done\r\n\r\nexit 0\r\n", "log");
    assert_eq!((&*heading, &*shown), ("Done", "exit 0\n\nlog"));
    retirement_barrier();
}

#[test]
fn managed_image_actual_worker_uses_receipt_label_and_preserves_diagnostics() {
    let reference = chat::ImageReference {
        id: uuid::Uuid::new_v4(),
        number: 59,
        device: uuid::Uuid::new_v4(),
        name: "Original screenshot.png".into(),
        sha256: "a".repeat(64),
        extension: "png".into(),
    };
    for path in [
        "/private/image-storage/private-internal-digest.png",
        "https://example.test/private-internal-digest.png",
    ] {
        let description = "The user's original screenshot description";
        let content = ItemContent::Image {
            path: path.into(),
            description: description.into(),
            reference: Some(reference.clone()),
        };
        let (mut runtime, _handle) = literal_runtime(content.clone());
        let (run, shared) = job(&runtime.preparation);
        let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        assert!(output.parsed.markdown.items().is_empty());
        let literal_content::Prepared::Image { document, .. } =
            output.literal.as_ref().expect("actual prepared image DTO")
        else {
            panic!("actual worker produced an image payload")
        };
        assert_ne!(document.created_on, std::thread::current().id());
        runtime.markdown_prepared(runtime.target, "message", run, ready(Ok(output)));
        let source = runtime.snapshot.items[0].clone();
        let prepared = runtime
            .prepared_literal(&source)
            .expect("installed exact image");
        let label = prepared.image_prompt().unwrap().plain();
        assert_eq!(label.as_ref(), reference.label());
        assert!(!label.contains("private-internal-digest"));
        let literal_content::Content::Image { document, url } = prepared else {
            panic!("installed image document")
        };
        let markdown::Item::Paragraph(actual_description) = &document.items()[0] else {
            panic!("original image description")
        };
        assert_eq!(actual_description.plain().as_ref(), description);
        if path.starts_with("https://") {
            assert_eq!(url.as_deref(), Some(path));
            assert!(
                matches!(&document.items()[1], markdown::Item::Quote(items) if items.is_empty())
            );
            let markdown::Item::Paragraph(open) = &document.items()[3] else {
                panic!("shared original image URI")
            };
            let settings = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE);
            assert!(Arc::ptr_eq(
                open.spans(settings.style)[0].link.as_ref().unwrap(),
                url.as_ref().unwrap(),
            ));
        } else {
            assert!(url.is_none());
            let markdown::Item::Paragraph(actual_path) = &document.items()[1] else {
                panic!("original diagnostic image path")
            };
            assert_eq!(actual_path.plain().as_ref(), path);
        }
        runtime.show(false);
        retirement_barrier();

        // The same managed-image branch must still reject canceled work;
        // a stable label must not make stale preparation installable.
        let (runtime, _handle) = literal_runtime(content);
        let (_, shared) = job(&runtime.preparation);
        shared.cancelled.store(true, Ordering::Release);
        let result = std::thread::spawn(move || execute(shared, permit()))
            .join()
            .unwrap();
        assert!(matches!(result, Err(ref error) if error == "Message loading was cancelled."));
        drop(runtime);
        retirement_barrier();
    }
}

#[test]
fn literal_image_worker_native_fields_keep_catalog_and_shared_remote_uri() {
    use iced::advanced::{
        layout,
        widget::{Operation, Tree},
    };
    let description = format!("literal **description** {}", "λ ".repeat(8_192));
    let (mut runtime, _handle) = literal_runtime(ItemContent::Image {
        path: "/tmp/very-long-path/image.png".into(),
        description: description.clone(),
        reference: None,
    });
    let (run, shared) = job(&runtime.preparation);
    let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
        .join()
        .unwrap();
    runtime.markdown_prepared(runtime.target, "message", run, ready(Ok(output)));
    let source = runtime.snapshot.items[0].clone();
    let cached = &runtime.cache["message"];
    let settings = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE);
    let mut element: Element<'static, Message> = widget::column::with_children(
        (0..2)
            .filter_map(|part| {
                literal_content::image_field(&runtime, &source, cached, part, settings)
            })
            .collect::<Vec<_>>(),
    )
    .into();
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(&element);
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 100_000.0));
    let before = markdown::materialized_bytes();
    element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert_eq!(
        markdown::materialized_bytes(),
        before,
        "cold native literal Image layout cannot materialize its long field"
    );
    // complete_fixture intentionally materializes on its caller. Drive the
    // actual admitted Flow task here so this oracle measures UI work only.
    let literal_content::Content::Image { document, .. } = cached.literal.as_ref().unwrap() else {
        panic!("prepared Image document")
    };
    let Some(prose_document::Node::Item(markdown::Item::Paragraph(text))) = document.node(&[0])
    else {
        panic!("prepared Image description")
    };
    let mut source_style = settings.style;
    source_style.font = iced::Font::default();
    let entry = cached.flow_cache.markdown_entry(
        selection::logical_identity(&source.id, &[8, 0]),
        text.clone(),
        source_style,
    );
    let request = entry
        .borrow_mut()
        .request()
        .expect("mounted Image Flow request");
    let task = flow_layout::run(runtime.target, request).expect("admitted Image Flow worker");
    use iced::futures::StreamExt;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (request, ready) = executor.block_on(async {
        let mut stream = iced::runtime::task::into_stream(task).expect("actual Image Flow task");
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(effect) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                    Action::FlowReady(target, request, ready),
                ))) = effect
                {
                    assert_eq!(target, runtime.target);
                    return (request, ready);
                }
            }
            panic!("actual Image Flow completion")
        })
        .await
        .unwrap()
    });
    assert!(ready.is_some(), "actual Image Flow worker returns geometry");
    assert!(entry.borrow_mut().complete(&request, ready));
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert_eq!(
        markdown::materialized_bytes(),
        before,
        "native literal Image fields use primed stable sources"
    );
    #[derive(Default)]
    struct Fields(Vec<Arc<str>>);
    impl Operation for Fields {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn custom(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(text) = state.downcast_ref::<selectable_prose::State>() {
                self.0.push(text.plain.clone());
            }
        }
    }
    let mut fields = Fields::default();
    element.as_widget_mut().operate(
        &mut tree,
        iced::advanced::Layout::new(&node),
        &renderer,
        &mut fields,
    );
    assert_eq!(
        fields.0.iter().map(AsRef::as_ref).collect::<Vec<&str>>(),
        [description.as_str(), "/tmp/very-long-path/image.png"]
    );
    let mut builder = selection::Catalog::builder();
    runtime
        .prepared_literal(&source)
        .unwrap()
        .catalog(&mut builder, &source, true);
    let catalog = builder.finish();
    let (a, b) = catalog.endpoints().unwrap();
    assert_eq!(
        catalog.copy(a, b).unwrap(),
        format!("{description}\n\n/tmp/very-long-path/image.png")
    );
    assert_eq!(
        catalog
            .text(selection::logical_identity("message", &[8, 0]))
            .unwrap()
            .as_ref(),
        description
    );
    let remote = format!("https://example.test/{}", "u".repeat(48 * 1024));
    let content = ItemContent::Image {
        path: remote.clone(),
        description: String::new(),
        reference: None,
    };
    let prepared = std::thread::spawn(move || {
        literal_content::Prepared::new(&content, &AtomicBool::new(false))
            .unwrap()
            .unwrap()
    })
    .join()
    .unwrap()
    .install();
    let literal_content::Content::Image {
        document,
        url: Some(url),
    } = &prepared
    else {
        panic!("remote image URI")
    };
    let markdown::Item::Paragraph(open) = &document.items()[3] else {
        panic!("shared URI backing")
    };
    assert!(Arc::ptr_eq(
        open.spans(settings.style)[0].link.as_ref().unwrap(),
        url
    ));
    let action = Action::OpenSharedUrl(runtime.target, url.clone());
    assert!(matches!(action,Action::OpenSharedUrl(_,ref shared) if Arc::ptr_eq(shared,url)));
    assert_eq!(url.as_ref(), remote);
    drop(element);
    runtime.show(false);
    drop(prepared);
    retirement_barrier();
}

#[test]
fn literal_user_source_uses_actual_worker_payload_and_refuses_rewrite_until_current_ready() {
    let body = format!(
        "**literal** [not a link](https://invalid.test)\r\n{}\r\nEND_LITERAL_λ",
        "raw `code` & <tag> ".repeat(32_768)
    );
    let (mut runtime, handle) = view::prepared_runtime_for_test(&[]);
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    let mut user = (*item(&body)).clone();
    user.role = chat::Role::User;
    snapshot.items.push(Arc::new(user));
    handle.publish_delta_snapshot_for_test(snapshot);
    let materialized = markdown::materialized_bytes();
    runtime.refresh();
    let source = runtime.snapshot.items[0].clone();
    assert!(
        runtime.prepared_prompt(&source).is_none(),
        "cold literal source must wait for exact preparation"
    );
    assert_eq!(
        markdown::materialized_bytes(),
        materialized,
        "cold literal staging cannot materialize the full UI body"
    );
    let (run, shared) = job(&runtime.preparation);
    let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
        .join()
        .unwrap();
    assert!(
        output.parsed.markdown.items().is_empty(),
        "literal User is not Markdown parsed"
    );
    assert_eq!(output.parsed.markdown.parsed_work_bytes(), 0);
    assert_ne!(
        output.prompt.as_ref().unwrap().created_on,
        std::thread::current().id()
    );
    runtime.markdown_prepared(runtime.target, "message", run, ready(Ok(output)));
    let accepted = runtime
        .prepared_prompt(&source)
        .expect("actual worker accepted literal source");
    assert_eq!(&*accepted.full.plain(), body);
    assert_eq!(&*accepted.shown(false).plain(), prompt::preview(&body).0);
    assert!(accepted.full.plain_ready());
    let plain = accepted.full.plain();
    let revision = runtime.cache["message"].revision;
    let mut metadata = (*handle.snapshot()).clone();
    metadata.generation += 1;
    Arc::make_mut(&mut metadata.items[0]).at = Some(1_700_000_000_000);
    handle.publish_delta_snapshot_for_test(metadata);
    runtime.refresh();
    let current = &runtime.snapshot.items[0];
    assert!(Arc::ptr_eq(
        &plain,
        &runtime.prepared_prompt(current).unwrap().full.plain()
    ));
    assert_eq!(runtime.cache["message"].revision, revision);
    let mut replacement = (*handle.snapshot()).clone();
    replacement.generation += 1;
    if let ItemContent::Text { text, .. } = &mut Arc::make_mut(&mut replacement.items[0]).content {
        text.replace_range(..2, "!!");
    }
    handle.publish_snapshot_for_test(replacement);
    runtime.refresh();
    assert!(
        runtime
            .prepared_prompt(&runtime.snapshot.items[0])
            .is_none(),
        "same-ID rewrite must not relabel the old literal DTO"
    );
    drain_fixture(&mut runtime);
    let ItemContent::Text { text, .. } = &runtime.snapshot.items[0].content else {
        unreachable!()
    };
    assert_eq!(
        &*runtime
            .prepared_prompt(&runtime.snapshot.items[0])
            .unwrap()
            .full
            .plain(),
        text
    );
    runtime.show(false);
    retirement_barrier();
}

#[test]
fn local_prompt_retires_actual_raw_source_and_literal_spans_before_output_readmission() {
    let source = prompt::LocalSource::new(format!(
        "literal local **source**\r\n{}",
        "λ raw queued bytes ".repeat(32_768)
    ));
    let body_len = source.text().len();
    let target = target();
    let mut state = State::default();
    state.stage_local(target, &source, crate::settings::EditorSyntaxTheme::Auto);
    let (_, entry) = state.local.as_ref().unwrap();
    let shared = entry.shared.clone();
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let admitted = slots.clone().try_acquire_owned().unwrap();
    let mut output = std::thread::spawn(move || execute(shared, admitted).unwrap())
        .join()
        .unwrap();
    let (raw_sent, raw_disposed) = std::sync::mpsc::channel();
    *source.disposed.lock().unwrap() = Some(raw_sent);
    let (spans_sent, spans_disposed) = std::sync::mpsc::channel();
    output
        .prompt
        .as_mut()
        .unwrap()
        .full
        .watch_raw_spans(spans_sent);
    assert_eq!(
        &*output.prompt.as_ref().unwrap().full.plain(),
        source.text().trim_end()
    );
    let hold = hold_retirement();
    state.clear_local();
    drop(source);
    Ready::new(Ok(output)); // discarded queued output, without UI installation
    assert!(
        slots.clone().try_acquire_owned().is_err(),
        "discarded literal reply retains output admission until actual raw retirement"
    );
    assert!(raw_disposed.try_recv().is_err());
    assert!(spans_disposed.try_recv().is_err());
    drop(hold);
    let worker = retirement_barrier();
    let (raw_thread, raw_bytes) = raw_disposed.recv_timeout(Duration::from_secs(5)).unwrap();
    let (spans_thread, _) = spans_disposed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(raw_bytes, body_len);
    assert_eq!(raw_thread, worker);
    assert_eq!(spans_thread, worker);
    assert_ne!(worker, std::thread::current().id());
    assert!(
        slots.try_acquire_owned().is_ok(),
        "actual source and span payloads drop before this private admission returns"
    );
}

#[test]
fn local_prompt_acceptance_is_exact_and_real_failures_retry_without_poisoning_replacements() {
    let target = target();
    let source = prompt::LocalSource::new("original raw local\r\n".repeat(4096));
    let replacement = prompt::LocalSource::new("new source with same local role".into());
    let mut state = State::default();
    state.stage_local(target, &source, crate::settings::EditorSyntaxTheme::Auto);
    let (_, entry) = state.local.as_ref().unwrap();
    let old_run = entry.run;
    let old = Ready::new(execute(entry.shared.clone(), permit()));
    state.stage_local(
        target,
        &replacement,
        crate::settings::EditorSyntaxTheme::Auto,
    );
    assert!(
        state
            .accept_local(target, old_run, &replacement, old)
            .is_none(),
        "old queued reply cannot install on replacement source"
    );
    let (_, entry) = state.local.as_ref().unwrap();
    let run = entry.run;
    let wrong = Target {
        generation: target.generation + 1,
        ..target
    };
    assert!(
        state
            .accept_local(
                wrong,
                run,
                &replacement,
                Ready::new(Err("stale error".into()))
            )
            .is_none()
    );
    assert!(
        state.error().is_none(),
        "stale target error cannot quarantine the current queue"
    );
    assert!(
        state
            .accept_local(
                target,
                run,
                &replacement,
                Ready::new(Err("real local failure".into()))
            )
            .is_none()
    );
    assert!(state.local.is_none());
    assert_eq!(state.error(), Some("real local failure"));
    state.stage_local(
        target,
        &replacement,
        crate::settings::EditorSyntaxTheme::Auto,
    );
    assert!(
        state.local.is_none(),
        "real failure is visible, not a silently stuck once-stream"
    );
    state.retry();
    state.stage_local(
        target,
        &replacement,
        crate::settings::EditorSyntaxTheme::Auto,
    );
    let (_, entry) = state.local.as_ref().unwrap();
    let run = entry.run;
    let output = Ready::new(execute(entry.shared.clone(), permit()));
    let mut presentation = prompt::LocalPresentation::default();
    state
        .accept_local(target, run, &replacement, output)
        .unwrap()
        .install_local(&mut presentation);
    assert_eq!(
        &*presentation.prepared.as_ref().unwrap().full.plain(),
        replacement.text()
    );
    drop(presentation);
    drop(state);
    retirement_barrier();
}

#[test]
fn local_input_reusing_a_retained_lease_still_obeys_shared_entry_admission() {
    let target = target();
    let source = prompt::LocalSource::new("local canonical bytes".into());
    let mut state = State::default();
    state.stage_local(target, &source, crate::settings::EditorSyntaxTheme::Auto);
    assert!(source.lease.lock().unwrap().is_some());
    state.clear_local();
    for index in 0..QUEUED_ITEMS {
        let mut next = (*item("small actual prose source")).clone();
        next.id = format!("history-{index}");
        state.stage(
            &Arc::new(next),
            false,
            crate::settings::EditorSyntaxTheme::Auto,
            |_, _| false,
        );
    }
    assert_eq!(state.entries.len(), QUEUED_ITEMS);
    state.stage_local(target, &source, crate::settings::EditorSyntaxTheme::Auto);
    assert!(
        state.local.is_none(),
        "retained local lease cannot bypass the shared entry bound"
    );
    state.remove("history-0");
    state.stage_local(target, &source, crate::settings::EditorSyntaxTheme::Auto);
    assert!(state.local.is_some());
    let mut overflow = (*item("another valid source")).clone();
    overflow.id = "overflow".into();
    state.stage(
        &Arc::new(overflow),
        false,
        crate::settings::EditorSyntaxTheme::Auto,
        |_, _| false,
    );
    assert!(
        state.was_deferred(),
        "transcript admission counts the local entry too"
    );
    assert_eq!(
        state.entries.len() + usize::from(state.local.is_some()),
        QUEUED_ITEMS
    );
    drop(state);
    drop(source);
    retirement_barrier();
}

#[test]
fn unpolled_transcript_subscription_retires_final_request_and_canonical_before_lease() {
    let source = item(&"# Actual retained transcript\n\nlarge literal body λ\n\n".repeat(16_384));
    let raw = Arc::downgrade(&source);
    let mut state = State::default();
    state.stage(
        &source,
        true,
        crate::settings::EditorSyntaxTheme::Auto,
        |_, _| false,
    );
    let (run, shared) = job(&state);
    let lease = Arc::downgrade(&shared.lease);
    let worker = shared.clone();
    let output = std::thread::spawn(move || execute(worker, permit()).unwrap())
        .join()
        .unwrap();
    let canonical = shared
        .canonical
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .parsed
        .markdown
        .items()
        .iter()
        .find_map(|item| match item {
            markdown::Item::Heading(_, text) | markdown::Item::Paragraph(text) => {
                Some(text.plain())
            }
            _ => None,
        })
        .expect("actual growing parser retains canonical text");
    let plain = Arc::downgrade(&canonical);
    drop(canonical);
    output.discard();
    let subscription = stream(&Job {
        target: target(),
        id: Some("message".into()),
        run,
        shared: shared.clone(),
    });
    state.clear();
    drop(source);
    drop(shared);
    retirement_barrier();
    assert!(raw.upgrade().is_some());
    assert!(plain.upgrade().is_some());
    let held = hold_retirement();
    drop(subscription); // actual never-polled stream is now the final owner
    assert!(
        raw.upgrade().is_some(),
        "UI stream cancellation cannot destroy the actual transcript request"
    );
    assert!(
        plain.upgrade().is_some(),
        "UI stream cancellation cannot destroy the canonical plain allocation"
    );
    assert!(
        lease.upgrade().is_some(),
        "same source admission survives until raw retirement"
    );
    drop(held);
    retirement_barrier();
    assert!(raw.upgrade().is_none());
    assert!(plain.upgrade().is_none());
    assert!(lease.upgrade().is_none());
}

#[test]
fn unpolled_local_subscription_retires_actual_source_before_its_surviving_lease() {
    let target = target();
    let source = prompt::LocalSource::new("actual unpolled local source λ\r\n".repeat(16_384));
    let bytes = source.text().len();
    let (sent, disposed) = std::sync::mpsc::channel();
    *source.disposed.lock().unwrap() = Some(sent);
    let mut state = State::default();
    state.stage_local(target, &source, crate::settings::EditorSyntaxTheme::Auto);
    let (_, entry) = state.local.as_ref().unwrap();
    let lease = Arc::downgrade(&entry.shared.lease);
    let subscription = stream(&Job {
        target,
        id: None,
        run: entry.run,
        shared: entry.shared.clone(),
    });
    // Never poll: the real captured async input must still carry source
    // admission when its State/queued canonical owners disappear first.
    state.clear_local();
    drop(source);
    retirement_barrier();
    assert!(state.local.is_none());
    assert!(disposed.try_recv().is_err());
    assert!(
        lease.upgrade().is_some(),
        "unpolled local input still holds its actual source lease without an Entry"
    );
    let held = hold_retirement();
    drop(subscription);
    assert!(
        lease.upgrade().is_some(),
        "UI cancellation cannot release source capacity before raw retirement"
    );
    assert!(disposed.try_recv().is_err());
    drop(held);
    let worker = retirement_barrier();
    let (thread, actual) = disposed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(actual, bytes);
    assert_eq!(thread, worker);
    assert_ne!(thread, std::thread::current().id());
    assert!(
        lease.upgrade().is_none(),
        "actual local raw source drops before its final source lease"
    );
}

#[test]
fn authenticated_metadata_retains_exact_graph_and_retires_old_snapshot_items_after_commit() {
    for growing in [false, true] {
        let source = item(&format!(
            "# Stable rich source\n\n{}",
            "large unchanged body ".repeat(50_000)
        ));
        let (mut runtime, handle) = view::prepared_runtime_for_test(&[(*source).clone()]);
        // This setup publication can share the exact Item with the later
        // metadata predecessor. Observe its independent client owner too.
        let initial_client_retired = handle.observe_snapshot_retirement_for_test();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.busy = growing;
        if growing {
            let ItemContent::Text { text, .. } = &mut Arc::make_mut(&mut snapshot.items[0]).content
            else {
                unreachable!()
            };
            text.push_str("\n\nActual growing tail");
        }
        handle.publish_delta_snapshot_for_test(snapshot);
        initial_client_retired
            .recv_timeout(Duration::from_secs(5))
            .expect("the initial client's exact publication must retire");
        runtime.refresh();
        drain_fixture(&mut runtime);
        retirement_barrier();
        assert_eq!(
            runtime.preparation.entries.contains_key("message"),
            growing,
            "the growing case must own an actually accepted managed canonical"
        );
        let cached = &runtime.cache["message"];
        let revision = cached.revision;
        let document = cached.document.as_ref().unwrap().clone();
        let graph = document.items().as_ptr();
        let old = cached.item.as_ref().unwrap().clone();
        let old_snapshot = Arc::downgrade(&runtime.snapshot);
        let old_item = Arc::downgrade(&old);
        let disposed = watch_metadata_retirement(&old);
        let version = runtime.preparation.entries.get("message").map(|entry| {
            assert!(!entry.running);
            entry.shared.latest.lock().unwrap().version
        });
        let client_retired = handle.observe_snapshot_retirement_for_test();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        Arc::make_mut(&mut snapshot.items[0]).at = Some(1_700_000_000_000);
        handle.publish_delta_snapshot_for_test(snapshot);
        assert!(handle.known_append(&old, &handle.snapshot().items[0]));
        drop(old);
        // Client disposal is independent of presentation retirement. Wait
        // before refresh, where the metadata guard samples actual owners.
        client_retired
            .recv_timeout(Duration::from_secs(5))
            .expect("the metadata predecessor's exact client publication must retire");
        let parser_before = markdown::parser_work_bytes_on_thread();
        runtime.refresh();
        assert_eq!(
            markdown::parser_work_bytes_on_thread(),
            parser_before,
            "metadata-only refresh performs no UI body parse"
        );
        let cached = &runtime.cache["message"];
        assert_eq!(
            cached.revision, revision,
            "authenticated equal-body metadata preserves the prepared revision"
        );
        assert_eq!(
            cached.document.as_ref().unwrap().items().as_ptr(),
            graph,
            "authenticated equal-body metadata preserves the actual rich graph"
        );
        assert!(
            Arc::ptr_eq(cached.item.as_ref().unwrap(), &runtime.snapshot.items[0]),
            "authenticated equal-body metadata advances only exact current item authority"
        );
        assert_eq!(cached.stamp.as_ref().unwrap().0, 1_700_000_000_000);
        assert!(runtime.preparation.subscriptions(runtime.target).is_empty());
        assert!(old_snapshot.upgrade().is_none());
        let (thread, raw_gone) = disposed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            raw_gone && old_item.upgrade().is_none(),
            "actual old Snapshot and raw item owners are gone before metadata capacity returns"
        );
        assert_eq!(
            thread,
            retirement_barrier(),
            "actual old raw item retirement belongs to the reclaimer"
        );
        if let Some(version) = version {
            let entry = &runtime.preparation.entries["message"];
            assert!(!entry.running);
            let latest = entry.shared.latest.lock().unwrap();
            assert_eq!(latest.version, version);
            assert!(
                latest
                    .input
                    .transcript()
                    .is_some_and(|item| Arc::ptr_eq(item, &runtime.snapshot.items[0]))
            );
        }
        // The managed canonical cursor must accept a later real append.
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        let ItemContent::Text { text, .. } = &mut Arc::make_mut(&mut snapshot.items[0]).content
        else {
            unreachable!()
        };
        text.push_str("\n\nFollowing actual provider append.");
        handle.publish_delta_snapshot_for_test(snapshot);
        runtime.refresh();
        drain_fixture(&mut runtime);
        let cached = &runtime.cache["message"];
        assert!(!cached.placeholder);
        assert!(Arc::ptr_eq(
            cached.item.as_ref().unwrap(),
            &runtime.snapshot.items[0]
        ));
        assert!(cached.document.as_ref().unwrap().items().len() >= 3);
        runtime.show(false);
        retirement_barrier();
    }
}

#[test]
fn new_pull_request_labels_reparse_only_the_messages_whose_links_read_otherwise() {
    let message = |id: &str, body: &str| chat::Item {
        id: id.into(),
        ..(*item(body)).clone()
    };
    let agent = |id: &str, prompt: &str| chat::Item {
        id: id.into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "Agent".into(),
            input: serde_json::json!({"prompt": prompt, "run_in_background": true}).to_string(),
            output: "Background task launched".into(),
            status: chat::ToolStatus::Succeeded,
        },
        started_at: None,
        at: None,
    };
    let (mut runtime, _handle) = view::prepared_runtime_for_test(&[
        message("named", "Fixed in https://github.com/acme/widgets/pull/20."),
        message("other", "Nothing to link here."),
        agent(
            "card",
            "Review https://github.com/acme/widgets/pull/20 again.",
        ),
        agent("plain card", "Review the parser."),
    ]);
    drain_fixture(&mut runtime);
    retirement_barrier();
    let shown = |runtime: &Runtime, id: &str| {
        let markdown::Item::Paragraph(text) = &runtime.cache[id].markdown.items()[0] else {
            panic!("a paragraph");
        };
        text.plain()
    };
    // Before the daemon says anything, the link reads as a reference.
    assert!(shown(&runtime, "named").contains("acme/\u{2060}widgets#20"));
    let other = shown(&runtime, "other");
    let other_revision = runtime.cache["other"].revision;
    runtime.set_pull_request_labels(Arc::new(pull_request_links::fixture::fixture()));
    assert!(runtime.cache["named"].preparing);
    assert!(!runtime.cache["other"].preparing);
    // A coordination card is prepared again only for a link it holds.
    assert!(runtime.cache["card"].coordination.is_some());
    assert!(runtime.cache["card"].preparing);
    assert!(!runtime.cache["plain card"].preparing);
    drain_fixture(&mut runtime);
    retirement_barrier();
    assert!(
        shown(&runtime, "named").contains("#20\u{a0}Retry stalled"),
        "{:?}",
        shown(&runtime, "named")
    );
    assert!(Arc::ptr_eq(&other, &shown(&runtime, "other")));
    assert_eq!(runtime.cache["other"].revision, other_revision);
    // The same labels again change nothing.
    let named_revision = runtime.cache["named"].revision;
    runtime.set_pull_request_labels(Arc::new(pull_request_links::fixture::fixture()));
    assert_eq!(runtime.cache["named"].revision, named_revision);
    // Opening or closing a card rebuilds its row around the coordination
    // it keeps, which still shows these links' labels, so a later label
    // change still finds it.
    let card = runtime.active_item("card").cloned().expect("the card");
    let previous = runtime.cache.remove("card");
    let toggled = CachedItem::updated(
        &card.content,
        runtime.syntax_theme,
        previous,
        true,
        false,
        None,
    );
    assert!(toggled.coordination.is_some());
    assert_eq!(toggled.pull_request_links.len(), 1);
    runtime.cache.insert("card".into(), toggled);
    runtime.set_pull_request_labels(Arc::new(pull_request_links::Labels::default()));
    assert!(runtime.cache["card"].preparing);
    runtime.show(false);
    retirement_barrier();
}

#[test]
fn a_number_the_window_comes_to_know_relabels_only_the_messages_naming_it() {
    let message = |id: &str, body: &str| chat::Item {
        id: id.into(),
        ..(*item(body)).clone()
    };
    let (mut runtime, _handle) = view::prepared_runtime_for_test(&[
        message("known", "Fixed by #20."),
        message("later", "Tracked in #99."),
        message("other", "Nothing to name here."),
    ]);
    drain_fixture(&mut runtime);
    retirement_barrier();
    let shown = |runtime: &Runtime, id: &str| {
        let markdown::Item::Paragraph(text) = &runtime.cache[id].markdown.items()[0] else {
            panic!("a paragraph");
        };
        text.plain()
    };
    // Before the chat's repository is known, no number names anything.
    assert_eq!(&*shown(&runtime, "known"), "Fixed by #20.");
    let other_revision = runtime.cache["other"].revision;
    let mut list = pull_request_links::fixture::fixture_list();
    runtime.set_pull_request_labels(Arc::new(pull_request_links::fixture::of(&list)));
    assert!(runtime.cache["known"].preparing);
    // #99 is still unknown, so it reads as written either way.
    assert!(!runtime.cache["later"].preparing);
    assert!(!runtime.cache["other"].preparing);
    drain_fixture(&mut runtime);
    retirement_barrier();
    assert!(
        shown(&runtime, "known").contains("#20\u{a0}Retry stalled"),
        "{:?}",
        shown(&runtime, "known")
    );
    assert_eq!(&*shown(&runtime, "later"), "Tracked in #99.");
    // A poll that comes to know #99 relabels the message that mentioned
    // it while it was unknown, and no other.
    list.pull_requests.push(pull_request_links::fixture::row(
        "acme",
        "widgets",
        99,
        Some(("Count brokers", crate::work::PullRequestState::Open, false)),
    ));
    runtime.set_pull_request_labels(Arc::new(pull_request_links::fixture::of(&list)));
    assert!(runtime.cache["later"].preparing);
    assert!(!runtime.cache["known"].preparing);
    assert!(!runtime.cache["other"].preparing);
    drain_fixture(&mut runtime);
    retirement_barrier();
    assert!(
        shown(&runtime, "later").contains("#99\u{a0}Count brokers"),
        "{:?}",
        shown(&runtime, "later")
    );
    assert_eq!(runtime.cache["other"].revision, other_revision);
    runtime.show(false);
    retirement_barrier();
}

#[test]
fn a_message_prepared_under_older_labels_is_relabelled_on_arrival() {
    let (mut runtime, handle) = linked_edit_staged();
    // The worker took the edit before the labels arrived, so it parses
    // with none, and the message held no link to relabel it by.
    let (run, shared) = {
        let entry = &runtime.preparation.entries["named"];
        (entry.run, entry.shared.clone())
    };
    let permits = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = iced::futures::executor::block_on(permits.acquire_owned()).unwrap();
    let ready = Ready::new(execute(shared, permit));
    runtime.set_pull_request_labels(Arc::new(pull_request_links::fixture::fixture()));
    let target = runtime.target;
    runtime.markdown_prepared(target, "named", run, ready);
    drain_fixture(&mut runtime);
    retirement_barrier();
    assert_labelled_edit(&runtime);
    drop(handle);
    runtime.show(false);
    retirement_barrier();
}

#[test]
fn a_message_staged_before_the_labels_arrive_parses_with_them() {
    let (mut runtime, handle) = linked_edit_staged();
    let labels = Arc::new(pull_request_links::fixture::fixture());
    runtime.set_pull_request_labels(labels.clone());
    let staged = runtime.preparation.entries["named"]
        .shared
        .latest
        .lock()
        .unwrap()
        .labels
        .clone();
    assert!(Arc::ptr_eq(&staged, &labels), "the queued parse reads them");
    drain_fixture(&mut runtime);
    retirement_barrier();
    assert_labelled_edit(&runtime);
    drop(handle);
    runtime.show(false);
    retirement_barrier();
}

/// A chat whose message has just been edited to add a pull request link,
/// staged for preparation while no labels are known.
fn linked_edit_staged() -> (Runtime, Arc<ClientHandle>) {
    let (mut runtime, handle) = view::prepared_runtime_for_test(&[chat::Item {
        id: "named".into(),
        ..(*item("Fixed it.")).clone()
    }]);
    retirement_barrier();
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    let ItemContent::Text { text, .. } = &mut Arc::make_mut(&mut snapshot.items[0]).content else {
        unreachable!()
    };
    text.push_str(" See https://github.com/acme/widgets/pull/20 again.");
    handle.publish_delta_snapshot_for_test(snapshot);
    runtime.refresh();
    assert!(runtime.preparation.entries.contains_key("named"));
    assert!(runtime.cache["named"].pull_request_links.is_empty());
    (runtime, handle)
}

fn assert_labelled_edit(runtime: &Runtime) {
    let markdown::Item::Paragraph(text) = &runtime.cache["named"].markdown.items()[0] else {
        panic!("a paragraph");
    };
    assert!(
        text.plain().contains("#20\u{a0}Retry stalled") && text.plain().ends_with("again."),
        "{:?}",
        text.plain()
    );
    assert_eq!(runtime.cache["named"].pull_request_links.len(), 1);
}

#[test]
fn metadata_reuse_refuses_full_resets_rewrites_shape_changes_and_inflight_work() {
    for case in [
        "full reset",
        "same-length rewrite",
        "thinking",
        "phase",
        "role",
        "inflight palette",
        "finalization",
    ] {
        let source = item(&format!(
            "{}{}",
            "a".repeat(16_384),
            "same tail ".repeat(32)
        ));
        let (mut runtime, handle) = view::prepared_runtime_for_test(&[(*source).clone()]);
        if case == "finalization" {
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            snapshot.busy = true;
            let ItemContent::Text { text, .. } = &mut Arc::make_mut(&mut snapshot.items[0]).content
            else {
                unreachable!()
            };
            text.push_str(" actual unfinished tail");
            handle.publish_delta_snapshot_for_test(snapshot);
            runtime.refresh();
            drain_fixture(&mut runtime);
            assert!(runtime.preparation.entries.contains_key("message"));
        }
        retirement_barrier();
        let cached = &runtime.cache["message"];
        let revision = cached.revision;
        let document = cached.document.as_ref().unwrap().clone();
        let previous = cached.item.as_ref().unwrap().clone();
        if case == "inflight palette" {
            runtime.set_syntax_theme(crate::settings::EditorSyntaxTheme::SolarizedDark);
            assert!(runtime.preparation.entries["message"].running);
        }
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        if case == "finalization" {
            snapshot.busy = false;
        }
        let next = Arc::make_mut(&mut snapshot.items[0]);
        next.at = Some(1_700_000_000_000);
        match (&mut next.content, case) {
            (ItemContent::Text { text, .. }, "same-length rewrite") => {
                text.replace_range(0..1, "b");
            }
            (ItemContent::Text { thinking, .. }, "thinking") => *thinking = true,
            (ItemContent::Text { phase, .. }, "phase") => *phase = chat::Phase::Commentary,
            (_, "role") => next.role = chat::Role::User,
            _ => {}
        }
        if case == "same-length rewrite" {
            assert_eq!(
                ChangeToken::of(&previous.content),
                ChangeToken::of(&next.content)
            );
        }
        if case == "full reset" {
            handle.publish_snapshot_for_test(snapshot);
        } else {
            handle.publish_delta_snapshot_for_test(snapshot);
        }
        let parser_before = markdown::parser_work_bytes_on_thread();
        runtime.refresh();
        assert_eq!(
            markdown::parser_work_bytes_on_thread(),
            parser_before,
            "metadata and finalization never parse a large body on UI"
        );
        let cached = &runtime.cache["message"];
        assert!(
            cached.preparing,
            "{case}: a new current preparation is required"
        );
        if matches!(case, "inflight palette" | "finalization") {
            assert!(
                Arc::ptr_eq(cached.item.as_ref().unwrap(), &previous),
                "a metadata publication cannot relabel an in-flight old graph as current"
            );
        } else {
            assert!(
                cached.placeholder,
                "{case}: unproved source cannot expose the old graph"
            );
        }
        drain_fixture(&mut runtime);
        let cached = &runtime.cache["message"];
        assert!(!cached.placeholder);
        assert!(Arc::ptr_eq(
            cached.item.as_ref().unwrap(),
            &runtime.snapshot.items[0]
        ));
        assert_ne!(cached.revision, revision);
        assert_ne!(
            cached.document.as_ref().unwrap().items().as_ptr(),
            document.items().as_ptr()
        );
        runtime.show(false);
        retirement_barrier();
    }
}

#[test]
fn metadata_retirement_pressure_uses_normal_admission_and_idle_release_without_retry() {
    use iced::futures::StreamExt;
    for (growing, large_id) in [(false, false), (true, false), (false, true)] {
        let mut source = item(&if large_id {
            "x".into()
        } else {
            "source ".repeat(4096)
        });
        if large_id {
            Arc::make_mut(&mut source).id = "opaque".repeat(16_384);
        }
        let id = source.id.clone();
        let (mut runtime, handle) = view::prepared_runtime_for_test(&[(*source).clone()]);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.busy = growing;
        if growing {
            let ItemContent::Text { text, .. } = &mut Arc::make_mut(&mut snapshot.items[0]).content
            else {
                unreachable!()
            };
            text.push_str(" actual growing tail");
        }
        handle.publish_delta_snapshot_for_test(snapshot);
        runtime.refresh();
        drain_fixture(&mut runtime);
        retirement_barrier();
        let previous = runtime.cache[&id].item.as_ref().unwrap().clone();
        let pressure = Lease::default();
        let spare = if large_id { 4096 } else { 0 };
        assert!(pressure.resize(QUEUED_BYTES - reserved().load(Ordering::Relaxed) - spare));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        Arc::make_mut(&mut snapshot.items[0]).at = Some(1_700_000_000_000);
        handle.publish_delta_snapshot_for_test(snapshot);
        runtime.refresh();
        assert!(
            Arc::ptr_eq(runtime.cache[&id].item.as_ref().unwrap(), &previous),
            "retirement pressure retains the proven old owner rather than promoting it"
        );
        assert!(runtime.cache[&id].preparing);
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut events = Box::pin(availability_stream());
        executor.block_on(events.next()).unwrap();
        drop(pressure);
        let message = executor
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), events.next()).await })
            .unwrap()
            .unwrap();
        let target = runtime.target;
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
        drop(app.update(message));
        let runtime = app.chat_runtime_mut(target).unwrap();
        drain_fixture(runtime);
        assert!(!runtime.cache[&id].placeholder && !runtime.preparation.waiting());
        assert!(Arc::ptr_eq(
            runtime.cache[&id].item.as_ref().unwrap(),
            &runtime.snapshot.items[0]
        ));
        runtime.show(false);
        retirement_barrier();
        executor.shutdown_background();
    }
}

#[test]
fn rehydrated_readers_verify_only_exact_logical_paths_and_actual_prefixes_on_worker() {
    let previous = reader_source::Source::new("wide line\n").proof();
    let code = "wide line\nnew hidden tail\n";
    let source = item(&format!("```text\n{code}```\n\n```text\n{code}```"));
    let target = target();
    let mut state = configured(target, &source);
    let identity = selection::logical_identity(&source.id, &[0, 0]);
    let unmounted = selection::logical_identity(&source.id, &[9]);
    state.set_reader_proofs(Arc::new(
        [(identity, previous), (unmounted, previous)].into(),
    ));
    assert!(state.stage(
        &source,
        false,
        crate::settings::EditorSyntaxTheme::Auto,
        |_, _| false
    ));
    let (_, shared) = job(&state);
    let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
        .join()
        .unwrap();
    assert_eq!(
        output.reader_verifications.len(),
        1,
        "equal text cannot bless another logical path"
    );
    assert_eq!(
        output.reader_verifications[&identity],
        (reader_source::Source::new(code).proof(), previous)
    );
    output.discard();
    drop(state);
    retirement_barrier();

    let input = "rewritten input";
    let output = "wide line\nnew hidden tail\n";
    let source = Arc::new(chat::Item {
        content: ItemContent::Tool {
            name: "fixture".into(),
            input: input.into(),
            output: output.into(),
            status: chat::ToolStatus::Succeeded,
        },
        ..(*source).clone()
    });
    let mut state = configured(target, &source);
    let mut proofs = HashMap::new();
    let old_input = reader_source::Source::new("original input").proof();
    for (part, proof) in [(4, old_input), (5, previous)] {
        let logical = selection::logical_identity(&source.id, &[part]);
        for axis in [scroll::ReaderAxis::Horizontal, scroll::ReaderAxis::Vertical] {
            proofs.insert(scroll::reader_key(logical, axis), proof);
        }
    }
    state.set_reader_proofs(Arc::new(proofs));
    assert!(state.stage(
        &source,
        false,
        crate::settings::EditorSyntaxTheme::Auto,
        |_, _| false
    ));
    let (_, shared) = job(&state);
    let prepared = std::thread::spawn(move || execute(shared, permit()).unwrap())
        .join()
        .unwrap();
    assert!(
        prepared.parsed.markdown.items().is_empty(),
        "tool verification does not parse its output as Markdown"
    );
    assert_eq!(
        prepared.reader_verifications.len(),
        2,
        "both output axes survive; rewritten input axes fail closed"
    );
    let output_logical = selection::logical_identity(&source.id, &[5]);
    for axis in [scroll::ReaderAxis::Horizontal, scroll::ReaderAxis::Vertical] {
        assert_eq!(
            prepared.reader_verifications[&scroll::reader_key(output_logical, axis)],
            (reader_source::Source::new(output).proof(), previous)
        );
    }
    let worker_output = prepared
        .prepared_tools
        .as_ref()
        .unwrap()
        .output
        .as_ref()
        .unwrap()
        .text
        .clone();
    let mut cached = CachedItem::preparing(&source, crate::settings::EditorSyntaxTheme::Auto);
    prepared.install(&mut cached, false, true);
    assert!(
        Arc::ptr_eq(&worker_output, &cached.tool_output.as_ref().unwrap().text),
        "UI installation shares the worker allocation"
    );
    assert!(!cached.placeholder && !cached.preparing);
    assert_eq!(cached.tool_input.as_ref().unwrap().text.as_ref(), input);
    assert_eq!(cached.tool_output.as_ref().unwrap().text.as_ref(), output);
    drop(cached);
    drop(state);
    retirement_barrier();
}

#[test]
fn owned_parser_moves_across_thread_and_matches_whole_document() {
    let body = format!(
        "# Owned\n\n[ref]: https://example.com\n\n```rust\n{}\n```\n\n| a | b |\n| - | - |\n| [reference][ref] | 🦀 |\n",
        "let value = 42;\n".repeat(24_000)
    );
    let expected = {
        let mut source = markdown_source::Source::default();
        match source.advance(&body, false) {
            markdown_source::Step::Append(text) | markdown_source::Step::Restart(text) => {
                markdown::Content::parse(&text)
            }
        }
    };
    let parsed = std::thread::spawn(move || {
        let mut parsed = Parsed::default();
        parse_publication(
            &mut parsed,
            &body,
            false,
            &Arc::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        parsed
    })
    .join()
    .unwrap();
    assert_eq!(
        format!("{:?}", parsed.markdown.items()),
        format!("{:?}", expected.items())
    );
}
#[test]
fn cold_rich_paragraph_is_parsed_once_and_syntax_is_ready_before_install() {
    let body = format!(
        "A *rich* [link](https://example.com) paragraph {} tail.",
        "emphasis *word* and `code` ".repeat(40_000)
    );
    let mut parsed = Parsed::default();
    parse_publication(
        &mut parsed,
        &body,
        false,
        &Arc::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        parsed.markdown.parsed_work_bytes(),
        body.len(),
        "cold input must not repeatedly reparse artificial prefixes"
    );
    let body = (0..4_000)
        .map(|index| format!("```rust\nlet value_{index} = {index};\n```\n\n"))
        .collect::<String>();
    let source = item(&body);
    let target = target();
    let mut state = configured(target, &source);
    state.stage(
        &source,
        false,
        crate::settings::EditorSyntaxTheme::Mocha,
        |_, _| false,
    );
    let (_, shared) = job(&state);
    let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
        .join()
        .unwrap();
    assert_eq!(output.blocks.len(), 4_000);
    let key = fence_key("rust", "let value_123 = 123;\n");
    let expected = prose::CodeBlock::new(
        Some("rust"),
        "let value_123 = 123;\n",
        crate::settings::EditorSyntaxTheme::Mocha,
    );
    let dark = output.blocks[&key].highlight(true).clone();
    let light = output.blocks[&key].highlight(false).clone();
    assert_eq!(&dark, expected.highlight(true));
    assert_eq!(&light, expected.highlight(false));
    let mut cached = CachedItem::preparing(&source, crate::settings::EditorSyntaxTheme::Mocha);
    output.install(&mut cached, false, false);
    assert!(
        dark.ptr_eq(cached.blocks[&key].highlight(true)),
        "installation moves the worker's spans without re-highlighting"
    );
    assert!(light.ptr_eq(cached.blocks[&key].highlight(false)));
    assert!(!cached.placeholder);
}
#[test]
fn streaming_tail_reuses_finished_fence_highlights_without_new_syntax_work() {
    let mut body = (0..200)
        .map(|index| format!("```rust\nlet value_{index} = {index};\n```\n\n"))
        .collect::<String>();
    body.push_str("A growing tail");
    let first = item(&body);
    let target = target();
    let mut state = configured(target, &first);
    let theme = crate::settings::EditorSyntaxTheme::Mocha;
    state.stage(&first, true, theme, |_, _| false);
    let (run, shared) = job(&state);
    let output = execute(shared, permit()).unwrap();
    assert_eq!(output.syntax_work.0, 200);
    let key = fence_key("rust", "let value_123 = 123;\n");
    let highlight = output.blocks[&key].highlight(true).clone();
    drop(
        state
            .accept(target, "message", run, ready(Ok(output)))
            .unwrap(),
    );
    for _ in 0..5 {
        body.push_str(" more words");
        let next = item(&body);
        state.stage(&next, true, theme, |_, _| true);
        let (run, shared) = job(&state);
        let output = execute(shared, permit()).unwrap();
        assert_eq!(
            output.syntax_work,
            (0, 0),
            "a tiny tail append does not highlight completed history again"
        );
        assert!(highlight.ptr_eq(output.blocks[&key].highlight(true)));
        drop(
            state
                .accept(target, "message", run, ready(Ok(output)))
                .unwrap(),
        );
    }
}
#[test]
fn worker_primes_plain_for_all_nested_logical_leaves() {
    fn verify(items: &[markdown::Item]) -> usize {
        items
            .iter()
            .map(|item| match item {
                markdown::Item::Paragraph(text) | markdown::Item::Heading(_, text) => {
                    assert!(text.plain_ready());
                    1
                }
                markdown::Item::Image { alt, .. } => {
                    assert!(alt.plain_ready());
                    1
                }
                markdown::Item::Quote(items) => verify(items),
                markdown::Item::List { bullets, .. } => bullets
                    .iter()
                    .map(|bullet| {
                        let (markdown::Bullet::Point { items }
                        | markdown::Bullet::Task { items, .. }) = bullet;
                        verify(items)
                    })
                    .sum(),
                markdown::Item::Table { columns, rows } => {
                    columns
                        .iter()
                        .map(|column| verify(&column.header))
                        .sum::<usize>()
                        + rows
                            .iter()
                            .flat_map(|row| &row.cells)
                            .map(|cell| verify(cell))
                            .sum::<usize>()
                }
                _ => 0,
            })
            .sum()
    }
    let source = item(
        "# Heading\n\nA *rich* paragraph.\n\n> A quote.\n\n- A list item.\n\n| heading | second |\n| --- | --- |\n| cell | other |\n\n![image alt](https://example.com/image.png)\n",
    );
    let target = target();
    let mut state = configured(target, &source);
    state.stage(
        &source,
        true,
        crate::settings::EditorSyntaxTheme::default(),
        |_, _| false,
    );
    let (_, shared) = job(&state);
    let output = execute(shared.clone(), permit()).unwrap();
    assert!(verify(output.parsed.markdown.items()) >= 8);
    assert!(
        verify(
            shared
                .canonical
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .parsed
                .markdown
                .items()
        ) >= 8
    );
}
#[test]
fn installation_retires_old_owned_document_before_worker_readmission() {
    let old = item(&"```rust\nlet old = 1;\n```\n\n".repeat(2_000));
    let weak = Arc::downgrade(&old);
    let mut cached = CachedItem::new(&old.content, crate::settings::EditorSyntaxTheme::default());
    cached.item = Some(old.clone());
    drop(old);
    let next = item("Replacement document.");
    let target = target();
    let mut state = configured(target, &next);
    state.stage(
        &next,
        false,
        crate::settings::EditorSyntaxTheme::default(),
        |_, _| false,
    );
    let (_, shared) = job(&state);
    let permits = Arc::new(tokio::sync::Semaphore::new(1));
    let output = execute(shared, permits.clone().try_acquire_owned().unwrap()).unwrap();
    output.install(&mut cached, false, false);
    let admitted = iced::futures::executor::block_on(permits.clone().acquire_owned()).unwrap();
    assert!(
        weak.upgrade().is_none(),
        "old input is released before replacement parsing can resume"
    );
    assert_ne!(
        retirement_barrier(),
        std::thread::current().id(),
        "destructors run on the retirement thread"
    );
    assert_eq!(cached.markdown.items().len(), 1);
    drop(admitted);
}
#[test]
fn zero_rendered_reference_document_retires_its_actual_parser_off_thread() {
    let body: String = (0..4_000)
        .map(|index| format!("[reference-{index}]: https://example.test/{index}\n"))
        .collect();
    let source = item(&body);
    let mut state = configured(target(), &source);
    state.stage(
        &source,
        true,
        crate::settings::EditorSyntaxTheme::Auto,
        |_, _| false,
    );
    let (_, shared) = job(&state);
    let output = execute(shared, permit()).unwrap();
    let mut cached = CachedItem::preparing(&source, crate::settings::EditorSyntaxTheme::Auto);
    output.install(&mut cached, true, false);
    assert!(cached.markdown.items().is_empty());
    assert!(cached.blocks.is_empty());
    assert!(cached.markdown.reference_count() >= 3_999);
    assert!(cached.markdown.has_retained_allocations());
    // Remove the canonical copy first: the cached document is the final
    // owner, including its non-rendered reference definitions.
    drop(state);
    retirement_barrier();
    let (sender, receiver) = std::sync::mpsc::channel();
    cached.markdown.watch_disposal(sender);
    drop(cached);
    let disposed = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("actual Content disposal");
    assert_eq!(disposed, retirement_barrier());
    assert_ne!(disposed, std::thread::current().id());
}

#[test]
fn retained_composite_factories_keep_one_real_worker_graph_until_last_ui_owner_drops() {
    use iced::advanced::{layout, widget::Tree};
    let body = (0..120).map(|index| format!(
        "> Quote {index} λ\n>\n> - nested **list**\n> - second item\n\n- outer {index}\n  - inner cell\n\n| header | second |\n| --- | --- |\n| row {index} | Unicode é |\n\n"
    )).collect::<String>();
    for cache_first in [true, false] {
        let source = item(&body);
        let mut state = configured(target(), &source);
        state.stage(
            &source,
            false,
            crate::settings::EditorSyntaxTheme::Auto,
            |_, _| false,
        );
        let (_, shared) = job(&state);
        let mut output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        assert_ne!(output.document.created_on, std::thread::current().id());
        let roots = output.parsed.markdown.items().len();
        assert!(roots > 4);
        assert!(
            output
                .parsed
                .markdown
                .items()
                .iter()
                .any(|item| matches!(item, markdown::Item::Quote(_)))
        );
        assert!(
            output
                .parsed
                .markdown
                .items()
                .iter()
                .any(|item| matches!(item, markdown::Item::List { .. }))
        );
        assert!(
            output
                .parsed
                .markdown
                .items()
                .iter()
                .any(|item| matches!(item, markdown::Item::Table { .. }))
        );
        let (sent, disposed) = std::sync::mpsc::channel();
        output.document.watch_disposal(sent);
        let mut cached = CachedItem::preparing(&source, crate::settings::EditorSyntaxTheme::Auto);
        output.install(&mut cached, false, false);
        let document = cached.document.as_ref().unwrap();
        let mut leaves = Vec::new();
        fn plain(items: &[markdown::Item], leaves: &mut Vec<String>) {
            for item in items {
                match item {
                    markdown::Item::Paragraph(text) | markdown::Item::Heading(_, text) => {
                        leaves.push(text.plain().to_string())
                    }
                    markdown::Item::Quote(items) => plain(items, leaves),
                    markdown::Item::List { bullets, .. } => {
                        for bullet in bullets {
                            let (markdown::Bullet::Point { items }
                            | markdown::Bullet::Task { items, .. }) = bullet;
                            plain(items, leaves);
                        }
                    }
                    markdown::Item::Table { columns, rows } => {
                        for column in columns {
                            plain(&column.header, leaves);
                        }
                        for row in rows {
                            for cell in &row.cells {
                                plain(cell, leaves);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        plain(cached.markdown.items(), &mut leaves);
        let mut shared_leaves = Vec::new();
        plain(document.items(), &mut shared_leaves);
        assert_eq!(
            shared_leaves, leaves,
            "the immutable graph preserves every rendered source in parser order"
        );
        let clones = markdown::cloned_items();
        let mut element = prose::view(
            &cached,
            design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE),
            target(),
            None,
        );
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(&element);
        element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(720.0, 900.0)),
        );
        assert_eq!(
            markdown::cloned_items(),
            clones,
            "real AST Item::clone never runs while rebuilding/mounting composite UI factories"
        );
        drop(state);
        retirement_barrier();
        if cache_first {
            drop(cached);
            retirement_barrier();
            assert!(
                disposed.try_recv().is_err(),
                "actual retained row factories still own the original graph"
            );
            let held = hold_retirement();
            drop(element);
            drop(tree);
            assert!(
                disposed.try_recv().is_err(),
                "UI eviction transfers raw graph; it does not destruct it"
            );
            drop(held);
        } else {
            drop(element);
            drop(tree);
            retirement_barrier();
            assert!(
                disposed.try_recv().is_err(),
                "the current cache still owns the graph after widget eviction"
            );
            drop(cached);
        }
        let (thread, actual_roots) = disposed
            .recv_timeout(Duration::from_secs(5))
            .expect("actual composite graph disposal");
        assert_eq!(actual_roots, roots);
        assert_ne!(thread, std::thread::current().id());
        assert_eq!(thread, retirement_barrier());
    }
}

#[test]
fn rejected_composite_output_destroys_raw_graph_before_reclaimer_barrier_and_admission_release() {
    let source = item(&"> quote\n>\n> - nested list\n\n".repeat(200));
    let mut state = configured(target(), &source);
    state.stage(
        &source,
        false,
        crate::settings::EditorSyntaxTheme::Auto,
        |_, _| false,
    );
    let (_, shared) = job(&state);
    let mut output = std::thread::spawn(move || execute(shared, permit()).unwrap())
        .join()
        .unwrap();
    let (sent, disposed) = std::sync::mpsc::channel();
    output.document.watch_disposal(sent);
    let held = hold_retirement();
    output.discard();
    assert!(disposed.try_recv().is_err());
    drop(held);
    let barrier = retirement_barrier();
    let (thread, roots) = disposed
        .try_recv()
        .expect("outer retirement disposes its actual raw AST before acknowledging the barrier");
    assert!(roots > 0);
    assert_eq!(thread, barrier);
    assert_ne!(thread, std::thread::current().id());
    drop(state);
}

#[test]
fn large_image_title_is_a_flowed_caption_with_its_exact_selection_source() {
    use iced::advanced::{
        layout,
        widget::{Tree, tree},
    };
    let alt = format!(
        "{}**strong**{}",
        "ALT λ é office ".repeat(1800),
        " suffix".repeat(200)
    );
    let expected_alt = alt.replace("**", "");
    let title = "TITLE *literal* λ é ".repeat(1800);
    let source = item(&format!(
        "![{alt}](https://example.test/private.png \"{title}\")"
    ));
    let mut state = configured(target(), &source);
    state.stage(
        &source,
        false,
        crate::settings::EditorSyntaxTheme::Auto,
        |_, _| false,
    );
    let (_, shared) = job(&state);
    let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
        .join()
        .unwrap();
    let [
        markdown::Item::Image {
            alt, title_text, ..
        },
    ] = output.parsed.markdown.items()
    else {
        panic!("actual parsed image");
    };
    assert!(alt.plain_ready() && title_text.plain_ready());
    assert_eq!(alt.plain().as_ref(), expected_alt);
    assert_eq!(title_text.plain().as_ref(), title);
    let mut cached = CachedItem::preparing(&source, crate::settings::EditorSyntaxTheme::Auto);
    output.install(&mut cached, false, false);
    let before = markdown::materialized_bytes();
    let mut element = prose::view(
        &cached,
        design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE),
        target(),
        None,
    );
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(&element);
    element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(720.0, 900.0)),
    );
    assert_eq!(
        markdown::materialized_bytes(),
        before,
        "image views never style/materialize their full long alt or title on the UI thread"
    );
    fn collect(tree: &Tree, result: &mut Vec<(u64, Option<bool>)>) {
        if tree.tag == tree::Tag::of::<selectable_prose::State>() {
            let state = tree.state.downcast_ref::<selectable_prose::State>();
            result.push((
                state.logical.as_ref().unwrap().identity,
                state.flow_readiness(),
            ));
        }
        for child in &tree.children {
            collect(child, result);
        }
    }
    let mut leaves = Vec::new();
    collect(&tree, &mut leaves);
    // The alternative text names the image's link and is not drawn as
    // text of its own; the title is the caption beneath it.
    let alt_id = selection::logical_identity(&source.id, &[0, 0, 0]);
    let title_id = selection::logical_identity(&source.id, &[0, 0, 1]);
    assert_eq!(
        leaves,
        vec![(title_id, Some(false))],
        "the caption awaits the worker, with its original child 1 identity"
    );
    let mut catalog = selection::Catalog::builder();
    catalog.markdown(&source.id, &[0], cached.markdown.items());
    let catalog = catalog.finish();
    assert!(
        catalog.text(alt_id).is_none(),
        "alt text is not selectable text"
    );
    let plain = catalog.text(title_id).unwrap();
    assert_eq!(plain.as_ref(), title.as_str());
    let mut leaf = selection::Catalog::builder();
    leaf.logical(title_id, plain);
    let leaf = leaf.finish();
    let (first, last) = leaf.endpoints().unwrap();
    assert_eq!(
        catalog.copy(first, last).as_deref(),
        Some(title.as_str()),
        "the full original title survives virtual worker preparation"
    );
    drop(element);
    drop(tree);
    drop(cached);
    drop(state);
    retirement_barrier();
}

#[test]
fn large_coordination_runtime_admits_worker_and_retires_real_graphs() {
    let prompt = "# Task λ\n\n> quoted **work**\n>\n> - nested item\n\n| first | second |\n| --- | --- |\n| exact | é |\n\n```text\nsource line\n```\n\n".repeat(700);
    let result = "# Report\n\nExact returned rich result λ.\n\n".repeat(1000);
    for (provider, name, input, output) in [
        (Provider::Claude, "Agent", serde_json::json!({"prompt":prompt.clone(),"subagent_type":"Explore","run_in_background":false}).to_string(), result.clone()),
        (Provider::Codex, "spawn_agent", prompt.clone(), String::new()),
        (Provider::Omp, "task", serde_json::json!({"task":prompt.clone(),"context":result.clone(),"agent":"scout","name":"private-child"}).to_string(), String::new()),
    ] {
        let session = fresh_chat_session(provider, TerminalId::new(), "/project".into(), &Account::system(provider), false).unwrap();
        let handle = Arc::new(ClientHandle::fixture(session.clone(), Snapshot { ready:true, ..Snapshot::default() }));
        let mut runtime = Runtime::new(target(), session, Connection::Live(handle.clone()), true, crate::settings::EditorSyntaxTheme::Auto);
        let source = Arc::new(chat::Item { id:"message".into(), role:chat::Role::Assistant,
            content:ItemContent::Tool { name:name.into(), input, output, status:chat::ToolStatus::Succeeded }, started_at:None, at:None });
        let ui_work = coordination::preparations();
        handle.replace_snapshot_for_test(Snapshot { generation:1,ready:true,items:vec![source.clone()], ..Snapshot::default() });
        runtime.refresh();
        assert_eq!(coordination::preparations(), ui_work, "actual Runtime admission does not parse coordination on UI");
        assert!(runtime.cache["message"].preparing && runtime.cache["message"].coordination.is_none());
        assert!(runtime.preparation.queued_bytes() > 64*1024);
        let (run, shared) = job(&runtime.preparation);
        let mut output = std::thread::spawn(move || execute(shared, permit()).unwrap()).join().unwrap();
        let prepared = output.coordination.as_mut().expect("provider-specific worker recognition");
        assert_ne!(prepared.created_on, std::thread::current().id());
        assert!(prepared.contents().next().unwrap().items().len() > 4);
        let (sent, disposed) = std::sync::mpsc::channel();
        prepared.watch_disposal(sent);
        runtime.markdown_prepared(runtime.target, "message", run, ready(Ok(output)));
        let cached = &runtime.cache["message"];
        let coordinated = cached.coordination.as_ref().unwrap();
        assert!(coordinated.is_card());
        assert!(matches!(&coordinated.reading.kind, crate::chat::coordination::Kind::Spawn(spawn) if spawn.agents[0].prompt.inline()==Some(prompt.as_str())));
        assert!(runtime.turns.iter().flat_map(|turn| &turn.prose).any(|placed| placed.index==0 && placed.slot==turn::Slot::Card), "async completion regroups actual parent turns into the rich card slot");
        let address = coordinated.address();
        let contents = coordinated.contents().enumerate().map(|(index, content)| {
            let mut builder = selection::Catalog::builder();
            builder.markdown(&source.id, &[1,index as u32], content.items());
            let catalog = builder.finish();
            let (first,last) = catalog.endpoints().unwrap();
            (catalog.search_blocks(),catalog.copy(first,last).unwrap())
        }).collect::<Vec<_>>();
        // What a one-row step said is drawn, and selectable, only in its open detail.
        let closed = runtime.selection_document().0;
        assert!(contents.iter().flat_map(|(blocks, _)| blocks).all(|(identity, _)| closed.text(*identity).is_none()), "a closed row's detail is not selectable");
        drop(closed);
        runtime.expanded.insert("message".into());
        let catalog = runtime.selection_document().0;
        for (blocks, text) in contents {
            for (identity, plain) in blocks {
                assert_eq!(catalog.text(identity).as_deref(), Some(plain.as_ref()), "actual open detail catalog preserves every prepared logical source block");
            }
            assert!(text.contains("λ") || text.contains("Report"));
        }
        drop(catalog);
        runtime.expanded.remove("message");
        let previous = runtime.cache.remove("message").unwrap();
        let rebuilt = CachedItem::updated(&source.content, runtime.syntax_theme, Some(previous), true, false, None);
        assert_eq!(rebuilt.coordination.as_ref().unwrap().address(), address, "disclosure reuses the same prepared canonical graphs/palettes");
        assert_eq!(coordination::preparations(), ui_work);
        runtime.cache.insert("message".into(), rebuilt);
        runtime.show(false);
        runtime.cool();
        let thread = disposed.recv_timeout(Duration::from_secs(5)).expect("actual coordinated canonical graph destructor");
        assert_eq!(thread, retirement_barrier());
        assert_ne!(thread, std::thread::current().id());
    }
}

#[test]
fn more_than_128_mixed_sources_settle_from_the_same_snapshot_without_retry() {
    let provider = Provider::Claude;
    let session = fresh_chat_session(
        provider,
        TerminalId::new(),
        "/project".into(),
        &Account::system(provider),
        false,
    )
    .unwrap();
    let handle = Arc::new(ClientHandle::fixture(
        session.clone(),
        Snapshot {
            ready: true,
            ..Snapshot::default()
        },
    ));
    let mut runtime = Runtime::new(
        target(),
        session,
        Connection::Live(handle.clone()),
        true,
        crate::settings::EditorSyntaxTheme::Auto,
    );
    let sources = (0..300).map(|index| Arc::new(chat::Item {
        id:format!("source-{index}"), role:chat::Role::Assistant, started_at:None, at:None,
        content:match index % 3 {
            0 => ItemContent::Command { command:format!("cargo test package_{index}"), output:format!("exact ordinary output {index}"), status:chat::ToolStatus::Succeeded, exit_code:Some(0), label:String::new(),working_directory:"/project".into(),action:chat::CommandAction::Run,duration_ms:None },
            1 => ItemContent::Tool { name:"Agent".into(), input:serde_json::json!({"prompt":format!("# Exact prompt {index} λ\n\n> source body\n\n- nested item\n"),"subagent_type":"Explore","run_in_background":true}).to_string(), output:"Background task launched".into(), status:chat::ToolStatus::Succeeded },
            _ => ItemContent::Command { command:format!("printf '%s' '{}-{index}'", "x".repeat(3000)), output:format!("exact long-command output {index}"), status:chat::ToolStatus::Succeeded, exit_code:Some(0), label:String::new(),working_directory:"/project".into(),action:chat::CommandAction::Run,duration_ms:None },
        },
    })).collect::<Vec<_>>();
    let ui_work = coordination::preparations();
    handle.replace_snapshot_for_test(Snapshot {
        generation: 1,
        ready: true,
        items: sources.clone(),
        ..Snapshot::default()
    });
    runtime.refresh();
    assert!(runtime.preparation.waiting());
    assert!(runtime.preparation.entries.len() <= QUEUED_ITEMS);
    assert!(
        runtime.preparation.contains("source-299"),
        "the newest body has first admission ahead of historical work"
    );
    assert!(
        runtime.preparation.error().is_none(),
        "transient pressure is not a sticky source failure"
    );
    // Real worker completion and the production refill path use this same
    // immutable source generation. No new provider update, input or Retry.
    use iced::futures::StreamExt;
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    let events = availability_stream();
    let mut events = Box::pin(events);
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let Message::ChatUi(wake) = executor.block_on(events.next()).unwrap() else {
        panic!("admission wake")
    };
    drop(app.handle_chat_ui(wake));
    for _ in 0..400 {
        let runtime = app.chat_runtime_mut(target).unwrap();
        let next = runtime
            .preparation
            .entries
            .iter()
            .find(|(_, entry)| entry.running)
            .map(|(id, entry)| (id.clone(), entry.run, entry.shared.clone()));
        let Some((id, run, shared)) = next else {
            if !runtime.preparation.waiting() {
                break;
            }
            retirement_barrier();
            let Message::ChatUi(wake) = executor.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), events.next())
                    .await
                    .expect("actual idle capacity wake")
                    .unwrap()
            }) else {
                panic!("admission wake")
            };
            drop(app.handle_chat_ui(wake));
            continue;
        };
        let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        drop(app.handle_chat_ui(Action::MarkdownPrepared(target, id, run, ready(Ok(output)))));
    }
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert!(!runtime.preparation.waiting());
    assert!(runtime.preparation.entries.is_empty());
    assert!(runtime.preparation.error().is_none());
    for (index, source) in sources.iter().enumerate() {
        let cached = &runtime.cache[&source.id];
        assert!(!cached.preparing && !cached.placeholder);
        assert!(Arc::ptr_eq(cached.item.as_ref().unwrap(), source));
        assert_eq!(cached.coordination.is_some(), index % 3 == 1);
        if let Some(coordinated) = &cached.coordination {
            assert!(
                matches!(&coordinated.reading.kind, crate::chat::coordination::Kind::Spawn(spawn) if spawn.agents[0].prompt.inline().unwrap().contains(&format!("Exact prompt {index} λ")))
            );
        }
    }
    runtime.refresh();
    assert!(
        runtime.preparation.entries.is_empty(),
        "unchanged accepted sources are not restaged"
    );
    assert_eq!(
        coordination::preparations(),
        ui_work,
        "neither saturation nor refill falls back to UI recognition/parsing"
    );
    runtime.show(false);
    retirement_barrier();
}

#[test]
fn deferred_history_replacement_and_selected_child_reject_old_queued_output() {
    for child in [false, true] {
        let provider = Provider::Claude;
        let session = fresh_chat_session(
            provider,
            TerminalId::new(),
            "/project".into(),
            &Account::system(provider),
            false,
        )
        .unwrap();
        let handle = Arc::new(ClientHandle::fixture(
            session.clone(),
            Snapshot {
                ready: true,
                ..Snapshot::default()
            },
        ));
        let mut runtime = Runtime::new(
            target(),
            session,
            Connection::Live(handle.clone()),
            true,
            crate::settings::EditorSyntaxTheme::Auto,
        );
        let make = |id: String, prompt: &str| {
            Arc::new(chat::Item {
                id,
                role: chat::Role::Assistant,
                content: ItemContent::Tool {
                    name: "Agent".into(),
                    input: serde_json::json!({"prompt":prompt,"run_in_background":true})
                        .to_string(),
                    output: "Background task launched".into(),
                    status: chat::ToolStatus::Succeeded,
                },
                started_at: None,
                at: None,
            })
        };
        let old = (0..160)
            .map(|index| {
                make(
                    format!("old-{index}"),
                    "Old graph must never enter the next context.",
                )
            })
            .collect::<Vec<_>>();
        handle.replace_snapshot_for_test(Snapshot {
            generation: 1,
            ready: true,
            items: old,
            ..Snapshot::default()
        });
        runtime.refresh();
        assert!(runtime.preparation.waiting());
        let (run, shared) = {
            let entry = &runtime.preparation.entries["old-159"];
            (entry.run, entry.shared.clone())
        };
        let old_output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        let source = make("old-159".into(), "# Exact replacement λ\n\nNew graph only.");
        let mut snapshot = Snapshot {
            generation: 2,
            ready: true,
            items: vec![source.clone()],
            ..Snapshot::default()
        };
        if child {
            snapshot.subagents.push(chat::Subagent {
                id: "replacement-child".into(),
                kind: "fixture".into(),
                description: "Child".into(),
                status: chat::ToolStatus::Succeeded,
                model: None,
                waiting: false,
                detail: String::new(),
                background: true,
                items: vec![source.clone()],
            });
            runtime.selected_subagent = Some("replacement-child".into());
        }
        handle.replace_snapshot_for_test(snapshot);
        runtime.refresh();
        runtime.markdown_prepared(runtime.target, "old-159", run, ready(Ok(old_output)));
        assert!(
            runtime.presentation_cache()["old-159"]
                .coordination
                .is_none(),
            "old exact run cannot install into replaced source/selected context"
        );
        let (run, shared) = {
            let entry = &runtime.preparation.entries["old-159"];
            (entry.run, entry.shared.clone())
        };
        let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        runtime.markdown_prepared(runtime.target, "old-159", run, ready(Ok(output)));
        assert!(
            !runtime.preparation.waiting(),
            "old deferred cursor is not carried into a new source context"
        );
        assert_eq!(runtime.presentation_cache().len(), 1);
        let cached = &runtime.presentation_cache()["old-159"];
        assert!(Arc::ptr_eq(cached.item.as_ref().unwrap(), &source));
        assert!(
            matches!(&cached.coordination.as_ref().unwrap().reading.kind,crate::chat::coordination::Kind::Spawn(spawn) if spawn.agents[0].prompt.inline()==Some("# Exact replacement λ\n\nNew graph only."))
        );
        runtime.show(false);
        retirement_barrier();
    }
}

#[test]
fn retired_source_lease_release_emits_the_idle_admission_wake() {
    use iced::futures::StreamExt;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let source = item("Owned source whose final lease is held by rejected output.");
    let mut state = configured(target(), &source);
    state.stage(
        &source,
        false,
        crate::settings::EditorSyntaxTheme::Auto,
        |_, _| false,
    );
    let (_, shared) = job(&state);
    let output = execute(shared, permit()).unwrap();
    let retained = Arc::downgrade(&output._owner);
    state.clear();
    retirement_barrier();
    let events = availability_stream();
    let mut events = Box::pin(events);
    executor
        .block_on(events.next())
        .expect("current-state subscription closes defer/install race");
    let release = hold_retirement();
    drop(ready(Ok(output)));
    // The source Arc and lease remain in the actual queued raw output.
    release.send(()).unwrap();
    executor.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                assert!(matches!(
                    events.next().await,
                    Some(Message::ChatUi(Action::PreparationAvailable))
                ));
                if retained.upgrade().is_none() {
                    break;
                }
            }
        })
        .await
        .expect("final lease release wakes an idle UI without provider/input events");
    });
    retirement_barrier();
}

#[test]
fn rejected_large_coordination_output_retires_all_raw_graphs_before_barrier() {
    let source = Arc::new(chat::Item { id:"message".into(), role:chat::Role::Assistant,
        content:ItemContent::Tool { name:"Agent".into(), input:serde_json::json!({"prompt":"> actual graph λ\n>\n> - nested item\n\n".repeat(4000),"subagent_type":"Explore"}).to_string(), output:String::new(),status:chat::ToolStatus::Succeeded },started_at:None,at:None });
    let mut state = configured(target(), &source);
    state.set_provider(Provider::Claude);
    state.stage(
        &source,
        false,
        crate::settings::EditorSyntaxTheme::Auto,
        |_, _| false,
    );
    let (_, shared) = job(&state);
    let mut output = std::thread::spawn(move || execute(shared, permit()).unwrap())
        .join()
        .unwrap();
    let (sent, disposed) = std::sync::mpsc::channel();
    output.coordination.as_mut().unwrap().watch_disposal(sent);
    let held = hold_retirement();
    drop(ready(Ok(output)));
    assert!(disposed.try_recv().is_err());
    drop(held);
    let barrier = retirement_barrier();
    assert_eq!(
        disposed
            .try_recv()
            .expect("raw coordinated output disposed within its retirement batch"),
        barrier
    );
    drop(state);
}

#[test]
fn syntax_theme_change_rejects_old_worker_palette_without_losing_input() {
    let source = item("```rust\nlet value = 42;\n```\n");
    let target = target();
    let mut state = configured(target, &source);
    state.stage(
        &source,
        false,
        crate::settings::EditorSyntaxTheme::Mocha,
        |_, _| false,
    );
    let (old, shared) = job(&state);
    let output = ready(execute(shared, permit()));
    state.stage(
        &source,
        false,
        crate::settings::EditorSyntaxTheme::Ocean,
        |_, _| false,
    );
    assert!(state.accept(target, "message", old, output).is_none());
    let (run, shared) = job(&state);
    let output = execute(shared, permit()).unwrap();
    assert_eq!(
        output.request.theme,
        crate::settings::EditorSyntaxTheme::Ocean
    );
    assert!(
        state
            .accept(target, "message", run, ready(Ok(output)))
            .is_some()
    );
}
#[test]
fn queued_inputs_coalesce_and_progressive_append_results_do_not_starve() {
    let target = target();
    let first = item("First prefix");
    let latest = item("First prefix and newer suffix");
    let mut state = configured(target, &first);
    state.stage(
        &first,
        true,
        crate::settings::EditorSyntaxTheme::default(),
        |_, _| false,
    );
    let (run, shared) = job(&state);
    state.stage(
        &latest,
        true,
        crate::settings::EditorSyntaxTheme::default(),
        |old, new| Arc::ptr_eq(old, &first) && Arc::ptr_eq(new, &latest),
    );
    let output = execute(shared, permit()).unwrap();
    assert!(Arc::ptr_eq(output.item(), &latest));
    let newest = item("First prefix and newer suffix plus another append");
    state.stage(
        &newest,
        true,
        crate::settings::EditorSyntaxTheme::default(),
        |old, new| Arc::ptr_eq(old, &latest) && Arc::ptr_eq(new, &newest),
    );
    let accepted = state
        .accept(target, "message", run, ready(Ok(output)))
        .unwrap();
    assert!(
        Arc::ptr_eq(accepted.item(), &latest),
        "a useful older proven prefix can be displayed"
    );
    drop(accepted);
    let (next, shared) = job(&state);
    assert_ne!(run, next);
    let output = execute(shared, permit()).unwrap();
    assert!(Arc::ptr_eq(output.item(), &newest));
    assert!(
        state
            .accept(target, "message", next, ready(Ok(output)))
            .is_some()
    );
    assert!(!state.entries["message"].running);
}
#[test]
fn replacements_and_context_changes_reject_completed_old_outputs() {
    let target = target();
    let original = item("old text");
    let replacement = item("new text");
    for change_context in [false, true] {
        let mut state = configured(target, &original);
        state.stage(
            &original,
            false,
            crate::settings::EditorSyntaxTheme::default(),
            |_, _| false,
        );
        let (run, shared) = job(&state);
        let output = ready(execute(shared.clone(), permit()));
        if change_context {
            state.context(
                target,
                Some("other-session"),
                Some("child"),
                std::slice::from_ref(&replacement),
            );
        }
        state.stage(
            &replacement,
            false,
            crate::settings::EditorSyntaxTheme::default(),
            |_, _| false,
        );
        assert!(shared.cancelled.load(Ordering::Relaxed));
        assert!(state.accept(target, "message", run, output).is_none());
        let (run, shared) = job(&state);
        let output = execute(shared, permit()).unwrap();
        assert!(Arc::ptr_eq(output.item(), &replacement));
        assert!(
            state
                .accept(target, "message", run, ready(Ok(output)))
                .is_some()
        );
    }
}
#[test]
fn output_backpressure_and_hidden_eviction_release_actual_owners() {
    let target = target();
    let original = item("a retained body");
    let weak = Arc::downgrade(&original);
    let mut state = configured(target, &original);
    state.stage(
        &original,
        true,
        crate::settings::EditorSyntaxTheme::default(),
        |_, _| false,
    );
    let (run, shared) = job(&state);
    let permits = Arc::new(tokio::sync::Semaphore::new(1));
    let output = ready(execute(
        shared.clone(),
        permits.clone().try_acquire_owned().unwrap(),
    ));
    assert_eq!(
        permits.available_permits(),
        0,
        "queued output retains worker admission"
    );
    let output_owner = Arc::downgrade(&shared);
    drop(shared);
    state.clear();
    let owner = output_owner
        .upgrade()
        .expect("queued output retains source lease after eviction");
    assert!(owner.cancelled.load(Ordering::Relaxed));
    assert_eq!(owner.lease.bytes(), "a retained body".len());
    drop(owner);
    assert!(state.accept(target, "message", run, output).is_none());
    let retired = iced::futures::executor::block_on(permits.clone().acquire_owned()).unwrap();
    assert!(
        output_owner.upgrade().is_none(),
        "retired output releases its source lease before readmission"
    );
    drop(retired);
    assert_eq!(
        permits.available_permits(),
        1,
        "discarded stale output releases admission"
    );
    drop(original);
    assert!(weak.upgrade().is_none());
    assert_eq!(state.queued_bytes(), 0);
    let mut parsed = Parsed::default();
    assert!(
        parse_publication(
            &mut parsed,
            "must not parse",
            false,
            &Arc::default(),
            &AtomicBool::new(true)
        )
        .is_err()
    );
    assert!(parsed.markdown.items().is_empty());
}
#[test]
fn cold_result_reaches_the_mounted_transcript_without_a_new_snapshot() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    struct Plain {
        selectable: Vec<String>,
        captions: Vec<String>,
    }
    impl Operation for Plain {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&iced::advanced::widget::Id>, _: iced::Rectangle, text: &str) {
            self.captions.push(text.into());
        }
        fn custom(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(state) = state.downcast_ref::<selectable_prose::State>() {
                self.selectable.push(state.plain.to_string());
            }
        }
    }
    let session = fresh_chat_session(
        Provider::Claude,
        TerminalId::new(),
        "/project".into(),
        &Account::system(Provider::Claude),
        false,
    )
    .unwrap();
    let handle = Arc::new(ClientHandle::fixture(
        session.clone(),
        Snapshot {
            ready: true,
            ..Snapshot::default()
        },
    ));
    let mut runtime = Runtime::new(
        target(),
        session,
        Connection::Live(handle.clone()),
        true,
        crate::settings::EditorSyntaxTheme::default(),
    );
    let source = item("Worker result reaches the mounted transcript.");
    handle.replace_snapshot_for_test(Snapshot {
        generation: 7,
        ready: true,
        items: vec![source],
        ..Snapshot::default()
    });
    runtime.refresh();
    assert!(runtime.cache["message"].preparing);
    let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut tree = Tree::empty();
    let mut rendered = |runtime: &Runtime| {
        let mut element = runtime.find_turn_view_for_test();
        tree.diff(element.as_widget_mut());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(640.0, 1000.0)),
        );
        let mut plain = Plain {
            selectable: vec![],
            captions: vec![],
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut plain);
        plain
    };
    assert!(
        rendered(&runtime)
            .captions
            .iter()
            .any(|text| text.contains("Loading message"))
    );
    assert!(rendered(&runtime).selectable.is_empty());
    assert!(runtime.selection_document().0.is_empty());
    let generation = runtime.snapshot.generation;
    drain_fixture(&mut runtime);
    assert_eq!(runtime.snapshot.generation, generation);
    assert!(
        rendered(&runtime)
            .selectable
            .iter()
            .any(|text| text == "Worker result reaches the mounted transcript.")
    );
    assert!(!runtime.cache["message"].preparing);
    handle.replace_snapshot_for_test(Snapshot {
        generation: 8,
        ready: true,
        items: vec![item("A replacement has different Markdown context.")],
        ..Snapshot::default()
    });
    runtime.refresh();
    assert!(
        rendered(&runtime)
            .captions
            .iter()
            .any(|text| text.contains("Loading message")),
        "unproven replacement must not display old context"
    );
    drain_fixture(&mut runtime);
    assert!(
        rendered(&runtime)
            .selectable
            .iter()
            .any(|text| text == "A replacement has different Markdown context.")
    );
}
#[test]
fn pending_status_is_not_copied_and_unrelated_selection_survives_ready() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    #[derive(Default)]
    struct Clipboard(String);
    impl iced::advanced::Clipboard for Clipboard {
        fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
            Some(self.0.clone())
        }
        fn write(&mut self, _: iced::advanced::clipboard::Kind, text: String) {
            self.0 = text;
        }
    }
    #[derive(Default)]
    struct Bounds(Vec<(String, iced::Rectangle)>);
    impl Operation for Bounds {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn custom(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            bounds: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(state) = state.downcast_ref::<selectable_prose::State>() {
                self.0.push((state.plain.to_string(), bounds));
            }
        }
    }
    fn interact(
        runtime: &Runtime,
        tree: &mut Tree,
        clipboard: &mut Clipboard,
        select: bool,
        keys: &[&str],
    ) {
        let (catalog, bookmark) = runtime.selection_document();
        let mut element = selection::transcript_virtualized(
            runtime.find_turn_view_for_test(),
            runtime.target,
            None,
            catalog,
            bookmark,
        );
        tree.diff(element.as_widget_mut());
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(640.0, 1000.0));
        let mut node = element.as_widget_mut().layout(tree, &renderer, &limits);
        let mut bounds = Bounds::default();
        element
            .as_widget_mut()
            .operate(tree, Layout::new(&node), &renderer, &mut bounds);
        let first = bounds.0.first().expect("existing ready prose").1;
        let start = iced::Point::new(first.x, first.y + 3.0);
        let end = iced::Point::new(first.x + first.width + 5.0, first.y + first.height - 1.0);
        let mut events = vec![];
        if select {
            events.extend([
                (
                    iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                        iced::mouse::Button::Left,
                    )),
                    start,
                ),
                (
                    iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: end }),
                    end,
                ),
                (
                    iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                        iced::mouse::Button::Left,
                    )),
                    end,
                ),
            ]);
        }
        events.extend(keys.iter().map(|key| {
            (
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character((*key).into()),
                    modified_key: keyboard::Key::Character((*key).into()),
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::CTRL,
                    text: None,
                    repeat: false,
                }),
                end,
            )
        }));
        for (event, pointer) in events {
            let mut messages = vec![];
            let mut shell = iced::advanced::Shell::new(&mut messages);
            element.as_widget_mut().update(
                tree,
                &event,
                Layout::new(&node),
                iced::mouse::Cursor::Available(pointer),
                &renderer,
                clipboard,
                &mut shell,
                &iced::Rectangle::with_size(iced::Size::new(640.0, 1000.0)),
            );
            if shell.is_layout_invalid() {
                node = element.as_widget_mut().layout(tree, &renderer, &limits);
            }
        }
    }
    let session = fresh_chat_session(
        Provider::Claude,
        TerminalId::new(),
        "/project".into(),
        &Account::system(Provider::Claude),
        false,
    )
    .unwrap();
    let handle = Arc::new(ClientHandle::fixture(
        session.clone(),
        Snapshot {
            ready: true,
            ..Snapshot::default()
        },
    ));
    let mut runtime = Runtime::new(
        target(),
        session,
        Connection::Live(handle.clone()),
        true,
        crate::settings::EditorSyntaxTheme::default(),
    );
    let stable = item("Stable selected text.");
    handle.replace_snapshot_for_test(Snapshot {
        generation: 1,
        ready: true,
        items: vec![stable.clone()],
        ..Snapshot::default()
    });
    runtime.refresh();
    drain_fixture(&mut runtime);
    let mut pending = (*item("Newly prepared answer.")).clone();
    pending.id = "pending".into();
    handle.replace_snapshot_for_test(Snapshot {
        generation: 2,
        ready: true,
        items: vec![stable, Arc::new(pending)],
        ..Snapshot::default()
    });
    runtime.refresh();
    assert!(runtime.cache["pending"].placeholder);
    let mut tree = Tree::empty();
    let mut clipboard = Clipboard::default();
    interact(&runtime, &mut tree, &mut clipboard, true, &["a", "c"]);
    assert_eq!(clipboard.0, "Stable selected text.");
    interact(&runtime, &mut tree, &mut clipboard, true, &["c"]);
    assert_eq!(clipboard.0, "Stable selected text.");
    drain_fixture(&mut runtime);
    interact(&runtime, &mut tree, &mut clipboard, false, &["c"]);
    assert_eq!(
        clipboard.0, "Stable selected text.",
        "ready must preserve an unrelated pointer selection"
    );
    interact(&runtime, &mut tree, &mut clipboard, false, &["a", "c"]);
    assert_eq!(
        clipboard.0,
        "Stable selected text.\n\nNewly prepared answer."
    );
    assert!(!clipboard.0.contains("Loading message"));
}
