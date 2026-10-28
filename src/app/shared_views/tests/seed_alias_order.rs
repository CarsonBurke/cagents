use super::*;
use crate::app::workspace_model_sync::ModelUpdate;
use crate::workspace_model::WorkspaceModel;

#[derive(Clone, Debug, PartialEq)]
struct Publication {
    operation: uuid::Uuid,
    scope: Scope,
    expected_revision: u64,
    change: Change,
}

impl Publication {
    fn pending(&self) -> (uuid::Uuid, Scope, Change) {
        (self.operation, self.scope.clone(), self.change.clone())
    }
}

struct SeedFixture {
    app: Cagents,
    source: ChannelId,
    destination: ChannelId,
    panes: [Pane; 3],
    canonical: Pane,
    model: WorkspaceModel,
    rx: async_channel::Receiver<ModelEdit>,
}

fn seed_fixture(alias_before_c: bool) -> SeedFixture {
    let (mut app, source, destination, mut a) = channel_move_fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    a.title = "A".into();
    let mut b = a.clone();
    b.id = CardId::new();
    b.title = "B".into();
    let mut c = a.clone();
    c.id = CardId::new();
    c.title = "C".into();
    for pane in [&mut b, &mut c] {
        let CardKind::Browser(browser) = &mut pane.kind else {
            panic!("the existing fixture supplies inert browser descriptors");
        };
        browser.id = BrowserId::new();
    }
    let local = if alias_before_c {
        vec![b.clone(), a.clone(), c.clone()]
    } else {
        vec![b.clone(), c.clone(), a.clone()]
    };
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(source))
        .unwrap()
        .panes = local.clone();
    drop(app.reconcile_shared_views());
    assert!(app.shared_views.pending.is_empty());

    // The host already owns the same browser under another pane identity.
    // Only its returned inventory is a fixture: seeding and every queued
    // intention below still go through the real App producer/reducer paths.
    let mut canonical = a.clone();
    canonical.id = CardId::new();
    let mut model = app.workspace_model.clone();
    for pane in &mut model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(source))
        .unwrap()
        .panes
    {
        if pane.id == a.id {
            *pane = canonical.clone();
        }
    }
    model.sequence += 1;

    let scope = Scope::Channel(source);
    app.shared_views.initialized.remove(&scope);
    app.shared_views.seeding.remove(&scope);
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    app.persist();
    let ModelEdit::SeedView {
        scope: seeded,
        panes,
        closed,
    } = rx
        .try_recv()
        .expect("persist must publish the initial seed")
    else {
        panic!("expected the actual initial SeedView");
    };
    assert_eq!(seeded, scope);
    assert_eq!(panes, local);
    assert!(closed.is_empty());
    assert!(rx.try_recv().is_err());
    assert!(app.shared_views.seeding.contains(&scope));
    assert!(!app.shared_views.initialized.contains(&scope));
    assert!(app.shared_views.pending.is_empty());
    assert_eq!(app.shared_views.baseline[&scope], local);

    SeedFixture {
        app,
        source,
        destination,
        panes: [a, b, c],
        canonical,
        model,
        rx,
    }
}

fn reorder(app: &mut Cagents, card: CardId, before: CardId) {
    let generation = app.workspace_view_generation;
    drop(app.update(Message::Viewport {
        generation,
        event: WorkspaceViewportEvent::Reordered {
            card,
            before: Some(before),
        },
    }));
}

fn rename(app: &mut Cagents, card: CardId) {
    use crate::app::chrome_ui::Action;

    drop(app.update_chrome(Action::Rename(card)));
    drop(app.update_chrome(Action::RenameChanged("Renamed during seed".into())));
    drop(app.update_chrome(Action::RenameSubmit));
}

fn publications(rx: &async_channel::Receiver<ModelEdit>) -> Vec<Publication> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|edit| match edit {
            ModelEdit::ChangeView {
                operation,
                scope,
                expected_revision,
                change,
            } => Publication {
                operation,
                scope,
                expected_revision,
                change,
            },
            other => panic!("unexpected publication: {other:?}"),
        })
        .collect()
}

fn observe(app: &mut Cagents, update: ModelUpdate) {
    drop(app.update(Message::WorkspaceModelObserved(update)));
}

fn seed_ack(fixture: &mut SeedFixture) {
    observe(
        &mut fixture.app,
        ModelUpdate::ViewApplied {
            scope: Scope::Channel(fixture.source),
            operation: None,
            model: fixture.model.clone(),
        },
    );
    assert_eq!(fixture.app.workspace_model.views, fixture.model.views);
    assert!(
        !fixture
            .app
            .shared_views
            .seeding
            .contains(&Scope::Channel(fixture.source))
    );
}

fn channel_order(app: &Cagents, channel: ChannelId) -> Vec<CardId> {
    app.durable
        .channels
        .iter()
        .find(|view| view.id == channel)
        .unwrap()
        .cards
        .clone()
}

fn assert_projection(app: &Cagents, source: ChannelId, expected: &[CardId]) {
    assert_eq!(channel_order(app, source), expected);
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), expected);
    assert_eq!(
        app.shared_views.baseline[&Scope::Channel(source)]
            .iter()
            .map(|pane| pane.id)
            .collect::<Vec<_>>(),
        expected
    );
}

fn assert_quiet_read(fixture: &mut SeedFixture, expected: &[CardId]) {
    for repeat in 0..2 {
        if repeat == 0 {
            // Advance the complete-snapshot watermark once, then repeat the
            // equal inventory. Quietness must not pass by skipping stale data.
            fixture.model.sequence += 1;
        }
        observe(&mut fixture.app, ModelUpdate::Read(fixture.model.clone()));
        fixture.app.persist();
        assert_eq!(
            fixture.app.shared_views.snapshot_sequence, fixture.model.sequence,
            "quietness must be checked after an accepted complete read"
        );
        assert_eq!(fixture.app.workspace_model.views, fixture.model.views);
        assert_projection(&fixture.app, fixture.source, expected);
        assert!(
            publications(&fixture.rx).is_empty(),
            "repeated canonical reads and persistence must not issue another retry"
        );
    }
}

fn settle_retry(
    fixture: &mut SeedFixture,
    original: &Publication,
    retry: &Publication,
    expected: &[CardId],
) {
    let retry_basis = fixture
        .app
        .shared_views
        .source_bases
        .get(&retry.operation)
        .cloned();
    // The original alias request was queued first. Its missing-ID/anchor
    // application can be a recorded no-op, so its receipt must not acknowledge
    // the distinct canonical retry that is still waiting behind it.
    let view = fixture
        .model
        .views
        .iter_mut()
        .find(|view| view.scope == original.scope)
        .unwrap();
    assert_eq!(original.expected_revision, view.revision);
    assert!(!view.apply(&original.change).unwrap());
    fixture.model.sequence += 1;
    observe(
        &mut fixture.app,
        ModelUpdate::ViewApplied {
            scope: original.scope.clone(),
            operation: Some(original.operation),
            model: fixture.model.clone(),
        },
    );
    assert_eq!(fixture.app.shared_views.pending, [retry.pending()]);
    assert!(
        !fixture
            .app
            .shared_views
            .source_bases
            .contains_key(&original.operation),
        "the original receipt retires its own source basis"
    );
    assert_eq!(
        fixture
            .app
            .shared_views
            .source_bases
            .get(&retry.operation)
            .cloned(),
        retry_basis,
        "the old receipt must not retire the canonical retry's source basis"
    );
    assert_projection(&fixture.app, fixture.source, expected);
    assert!(publications(&fixture.rx).is_empty());
    assert_quiet_read(fixture, expected);
    assert_eq!(fixture.app.shared_views.pending, [retry.pending()]);

    let view = fixture
        .model
        .views
        .iter_mut()
        .find(|view| view.scope == retry.scope)
        .unwrap();
    assert_eq!(retry.expected_revision, view.revision);
    assert!(view.apply(&retry.change).unwrap());
    view.revision += 1;
    fixture.model.sequence += 1;
    observe(
        &mut fixture.app,
        ModelUpdate::ViewApplied {
            scope: retry.scope.clone(),
            operation: Some(retry.operation),
            model: fixture.model.clone(),
        },
    );
    assert!(fixture.app.shared_views.pending.is_empty());
    assert!(
        !fixture
            .app
            .shared_views
            .source_bases
            .contains_key(&retry.operation)
    );
    assert_projection(&fixture.app, fixture.source, expected);
    assert!(publications(&fixture.rx).is_empty());
    assert_quiet_read(fixture, expected);
}

fn settle_fifo(fixture: &mut SeedFixture, issued: &[Publication], expected: &[CardId]) {
    assert_eq!(
        fixture.app.shared_views.pending,
        issued.iter().map(Publication::pending).collect::<Vec<_>>(),
        "append-only correction must retain every original queued receipt"
    );
    let mut acknowledged = HashSet::new();
    // Typed host inventories apply each emitted descriptor after the existing
    // publisher's revision refresh, retaining its UUID and FIFO position. This
    // is App receipt coverage, not transport/registry integration coverage.
    for (index, publication) in issued.iter().enumerate() {
        assert!(acknowledged.insert(publication.operation));
        let view = fixture
            .model
            .views
            .iter_mut()
            .find(|view| view.scope == publication.scope)
            .unwrap();
        assert!(publication.expected_revision <= view.revision);
        if view.apply(&publication.change).unwrap() {
            view.revision += 1;
        }
        fixture.model.sequence += 1;
        observe(
            &mut fixture.app,
            ModelUpdate::ViewApplied {
                scope: publication.scope.clone(),
                operation: Some(publication.operation),
                model: fixture.model.clone(),
            },
        );
        assert_eq!(fixture.app.workspace_model.views, fixture.model.views);
        assert_projection(&fixture.app, fixture.source, expected);
        assert_eq!(
            fixture.app.shared_views.pending,
            issued[index + 1..]
                .iter()
                .map(Publication::pending)
                .collect::<Vec<_>>(),
            "only the current wire receipt may retire an intention"
        );
        fixture.app.persist();
        assert!(publications(&fixture.rx).is_empty());
    }
    assert_eq!(
        fixture
            .model
            .views
            .iter()
            .find(|view| view.scope == Scope::Channel(fixture.source))
            .unwrap()
            .panes
            .iter()
            .map(|pane| pane.id)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(fixture.app.shared_views.pending.is_empty());
    assert_quiet_read(fixture, expected);
}

#[test]
fn public_reorder_during_seed_follows_the_adopted_pane_with_a_fresh_receipt() {
    let mut fixture = seed_fixture(false);
    let [a, b, c] = fixture.panes.clone();
    reorder(&mut fixture.app, a.id, c.id);
    assert_eq!(
        channel_order(&fixture.app, fixture.source),
        [b.id, a.id, c.id]
    );
    assert!(fixture.app.shared_views.pending.is_empty());
    assert!(publications(&fixture.rx).is_empty());

    seed_ack(&mut fixture);
    let issued = publications(&fixture.rx);
    assert_eq!(issued.len(), 2);
    let [original, retry] = issued.as_slice() else {
        unreachable!();
    };
    assert_eq!(original.scope, Scope::Channel(fixture.source));
    assert_eq!(retry.scope, original.scope);
    assert_eq!(
        original.change,
        Change::Move {
            id: a.id,
            before: Some(c.id),
        }
    );
    assert_eq!(
        retry.change,
        Change::Move {
            id: fixture.canonical.id,
            before: Some(c.id),
        }
    );
    assert_ne!(original.operation, retry.operation);
    assert_eq!(
        fixture.app.shared_views.pending,
        [original.pending(), retry.pending()]
    );
    let expected = [b.id, fixture.canonical.id, c.id];
    assert_projection(&fixture.app, fixture.source, &expected);
    assert!(
        fixture
            .app
            .card_state(fixture.canonical.id)
            .unwrap()
            .existence
            .is_open()
    );
    assert!(
        !fixture
            .app
            .card_state(a.id)
            .is_some_and(|card| card.existence.is_open())
    );
    settle_retry(&mut fixture, original, retry, &expected);
}

#[test]
fn two_public_reorders_during_seed_settle_in_wire_order() {
    let mut fixture = seed_fixture(false);
    let [a, b, c] = fixture.panes.clone();
    reorder(&mut fixture.app, a.id, b.id);
    assert_eq!(
        channel_order(&fixture.app, fixture.source),
        [a.id, b.id, c.id]
    );
    assert_eq!(
        fixture.app.workspace.order().collect::<Vec<_>>(),
        [a.id, b.id, c.id]
    );
    reorder(&mut fixture.app, c.id, b.id);
    assert_eq!(
        channel_order(&fixture.app, fixture.source),
        [a.id, c.id, b.id]
    );
    assert_eq!(
        fixture.app.workspace.order().collect::<Vec<_>>(),
        [a.id, c.id, b.id]
    );
    fixture.app.persist();
    assert!(fixture.app.shared_views.pending.is_empty());
    assert!(publications(&fixture.rx).is_empty());

    seed_ack(&mut fixture);
    let issued = publications(&fixture.rx);
    assert!(issued.len() >= 2);
    let scope = Scope::Channel(fixture.source);
    assert_eq!(issued[0].scope, scope);
    assert_eq!(issued[1].scope, scope);
    assert_eq!(
        issued[0].change,
        Change::Move {
            id: a.id,
            before: Some(b.id),
        }
    );
    assert_eq!(
        issued[1].change,
        Change::Move {
            id: c.id,
            before: Some(b.id),
        }
    );
    let expected = [fixture.canonical.id, c.id, b.id];
    assert_projection(&fixture.app, fixture.source, &expected);

    // Model the emitted queue in its actual FIFO order, including obsolete
    // requests already ahead of the canonical correction. Never apply all edits
    // before their receipts: the real publisher sends ACK1 before request2.
    // These Move-only inventory fixtures use the publisher's existing revision
    // refresh on conflict, which preserves the UUID and change (send()). This
    // exercises the App receipts, not a transport or registry integration.
    let original_raw = [
        [b.id, c.id, fixture.canonical.id],
        [c.id, b.id, fixture.canonical.id],
    ];
    let mut acknowledged = HashSet::new();
    for (index, publication) in issued.iter().enumerate() {
        assert_eq!(publication.scope, scope);
        assert!(matches!(publication.change, Change::Move { .. }));
        assert!(acknowledged.insert(publication.operation));
        let view = fixture
            .model
            .views
            .iter_mut()
            .find(|view| view.scope == scope)
            .unwrap();
        assert!(publication.expected_revision <= view.revision);
        let changed = view.apply(&publication.change).unwrap();
        assert_eq!(changed, index != 0);
        if changed {
            view.revision += 1;
        }
        if index < original_raw.len() {
            assert_eq!(
                view.panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
                original_raw[index],
                "raw host inventory after original wire request {index}"
            );
        }
        let mut pending_after_receipt = fixture.app.shared_views.pending.clone();
        pending_after_receipt.retain(|(operation, _, _)| *operation != publication.operation);
        fixture.model.sequence += 1;
        observe(
            &mut fixture.app,
            ModelUpdate::ViewApplied {
                scope: scope.clone(),
                operation: Some(publication.operation),
                model: fixture.model.clone(),
            },
        );
        assert_eq!(fixture.app.workspace_model.views, fixture.model.views);
        assert_projection(&fixture.app, fixture.source, &expected);
        assert_eq!(fixture.app.shared_views.pending, pending_after_receipt);
        fixture.app.persist();
        assert!(
            publications(&fixture.rx).is_empty(),
            "wire receipt {index} must not publish another repair"
        );
    }
    assert_eq!(
        fixture
            .model
            .views
            .iter()
            .find(|view| view.scope == scope)
            .unwrap()
            .panes
            .iter()
            .map(|pane| pane.id)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(fixture.app.shared_views.pending.is_empty());
    assert_eq!(
        issued.len(),
        3,
        "the original queue needs one final canonical-order correction"
    );
    assert_eq!(
        issued[2].change,
        Change::Move {
            id: fixture.canonical.id,
            before: Some(c.id),
        }
    );
    assert_eq!(
        acknowledged.len(),
        3,
        "the correction needs a fresh receipt"
    );
    assert_quiet_read(&mut fixture, &expected);
}

#[test]
fn a_later_insert_keeps_its_position_when_the_seed_alias_is_adopted() {
    let mut fixture = seed_fixture(false);
    let [a, b, c] = fixture.panes.clone();
    reorder(&mut fixture.app, a.id, b.id);
    reorder(&mut fixture.app, c.id, b.id);

    // Typed inert browser inventory at the normal persist boundary. No browser
    // opening/integration is claimed, and no pending operation is fabricated.
    let mut d = fixture.app.card_state(b.id).unwrap().clone();
    d.id = CardId::new();
    d.title = "D".into();
    let CardKind::Browser(browser) = &mut d.kind else {
        panic!("browser fixture");
    };
    browser.id = BrowserId::new();
    let durable = Arc::make_mut(&mut fixture.app.durable);
    durable.cards.push(d.clone());
    durable
        .channels
        .iter_mut()
        .find(|channel| channel.id == fixture.source)
        .unwrap()
        .cards
        .insert(1, d.id);
    drop(
        fixture
            .app
            .workspace
            .add_card(
                WorkspaceCard::new(d.id, "D", CardWidth::Fraction(d.width_fraction)).unwrap(),
                1,
                false,
            )
            .unwrap(),
    );
    fixture.app.persist();
    assert_eq!(
        channel_order(&fixture.app, fixture.source),
        [a.id, d.id, c.id, b.id]
    );
    assert!(fixture.app.shared_views.pending.is_empty());
    assert!(publications(&fixture.rx).is_empty());

    seed_ack(&mut fixture);
    let issued = publications(&fixture.rx);
    assert!(issued.len() >= 3);
    assert_eq!(
        issued[0].change,
        Change::Move {
            id: a.id,
            before: Some(b.id),
        }
    );
    let Change::Insert { pane, before } = &issued[1].change else {
        panic!("persist must publish the actual new descriptor");
    };
    assert_eq!(pane.id, d.id);
    assert_eq!(*before, Some(b.id));
    assert_eq!(
        issued[2].change,
        Change::Move {
            id: c.id,
            before: Some(b.id),
        }
    );
    let expected = [fixture.canonical.id, d.id, c.id, b.id];
    assert_projection(&fixture.app, fixture.source, &expected);
    settle_fifo(&mut fixture, &issued, &expected);
    assert_eq!(
        issued
            .iter()
            .filter(|publication| matches!(publication.change, Change::Insert { .. }))
            .count(),
        1,
        "the original insertion must not be republished after adoption"
    );
}

#[test]
fn removing_the_original_move_anchor_is_not_replayed_after_alias_adoption() {
    let mut fixture = seed_fixture(false);
    let [a, b, c] = fixture.panes.clone();
    // Settle the initial seed under its existing IDs. Two subsequent public
    // actions can now queue a Move followed by Remove; a single during-seed
    // diff would put all removals first and would not exercise this barrier.
    let mut alias_model = fixture.app.workspace_model.clone();
    alias_model.sequence += 1;
    observe(
        &mut fixture.app,
        ModelUpdate::ViewApplied {
            scope: Scope::Channel(fixture.source),
            operation: None,
            model: alias_model,
        },
    );
    assert!(fixture.app.shared_views.pending.is_empty());
    assert!(publications(&fixture.rx).is_empty());

    reorder(&mut fixture.app, a.id, b.id);
    drop(fixture.app.update(Message::CloseCard(b.id)));
    drop(fixture.app.update(Message::CommitClose(b.id)));
    assert_eq!(channel_order(&fixture.app, fixture.source), [a.id, c.id]);
    assert!(
        !fixture
            .app
            .card_state(b.id)
            .is_some_and(|card| card.existence.is_open())
    );
    let mut issued = publications(&fixture.rx);
    assert_eq!(issued.len(), 2);
    assert_eq!(
        issued[0].change,
        Change::Move {
            id: a.id,
            before: Some(b.id),
        }
    );
    assert_eq!(issued[1].change, Change::Remove { id: b.id });
    assert_eq!(
        fixture.app.shared_views.pending,
        issued.iter().map(Publication::pending).collect::<Vec<_>>()
    );
    fixture.model.sequence += 1;
    observe(&mut fixture.app, ModelUpdate::Read(fixture.model.clone()));
    issued.extend(publications(&fixture.rx));
    let expected = [fixture.canonical.id, c.id];
    assert_projection(&fixture.app, fixture.source, &expected);
    settle_fifo(&mut fixture, &issued, &expected);
    assert_eq!(
        issued
            .iter()
            .filter(|publication| matches!(publication.change, Change::Remove { .. }))
            .count(),
        1,
        "the original anchor removal must not be republished"
    );
    assert!(
        !fixture
            .app
            .card_state(b.id)
            .is_some_and(|card| card.existence.is_open())
    );
}

#[test]
fn public_reorder_during_seed_rebases_an_adopted_before_anchor() {
    // This starting order makes the public diff move C before A. Starting at
    // [B, C, A] would instead move A's ID and would not exercise the anchor.
    let mut fixture = seed_fixture(true);
    let [a, b, c] = fixture.panes.clone();
    reorder(&mut fixture.app, c.id, a.id);
    assert!(fixture.app.shared_views.pending.is_empty());
    assert!(publications(&fixture.rx).is_empty());

    seed_ack(&mut fixture);
    let issued = publications(&fixture.rx);
    assert_eq!(issued.len(), 2);
    let [original, retry] = issued.as_slice() else {
        unreachable!();
    };
    assert_eq!(original.scope, Scope::Channel(fixture.source));
    assert_eq!(retry.scope, original.scope);
    assert_eq!(
        original.change,
        Change::Move {
            id: c.id,
            before: Some(a.id),
        }
    );
    assert_eq!(
        retry.change,
        Change::Move {
            id: c.id,
            before: Some(fixture.canonical.id),
        }
    );
    assert_ne!(original.operation, retry.operation);
    assert_eq!(
        fixture.app.shared_views.pending,
        [original.pending(), retry.pending()]
    );
    let expected = [b.id, c.id, fixture.canonical.id];
    assert_projection(&fixture.app, fixture.source, &expected);
    settle_retry(&mut fixture, original, retry, &expected);
}

#[test]
fn rename_during_seed_rebases_both_update_ids_and_keeps_its_source_basis() {
    let mut fixture = seed_fixture(false);
    let [a, b, c] = fixture.panes.clone();
    rename(&mut fixture.app, a.id);
    assert!(fixture.app.shared_views.pending.is_empty());
    assert!(publications(&fixture.rx).is_empty());

    seed_ack(&mut fixture);
    let issued = publications(&fixture.rx);
    assert_eq!(issued.len(), 2);
    let [original, retry] = issued.as_slice() else {
        unreachable!();
    };
    assert_ne!(original.operation, retry.operation);
    assert_eq!(original.scope, Scope::Channel(fixture.source));
    assert_eq!(retry.scope, original.scope);
    let Change::Update { before, pane } = &original.change else {
        panic!("the public rename must produce an Update");
    };
    assert_eq!(before.id, a.id);
    assert_eq!(pane.id, a.id);
    assert_eq!(pane.user_title.as_deref(), Some("Renamed during seed"));
    assert_eq!(before.kind, pane.kind);
    assert_eq!(before.scope, pane.scope);
    assert_eq!(before.pending_terminal_start, pane.pending_terminal_start);
    let mut expected_change = original.change.clone();
    let Change::Update { before, pane } = &mut expected_change else {
        unreachable!();
    };
    before.id = fixture.canonical.id;
    pane.id = fixture.canonical.id;
    assert_eq!(retry.change, expected_change);
    assert_eq!(
        fixture.app.shared_views.pending,
        [original.pending(), retry.pending()]
    );
    assert_eq!(
        fixture.app.shared_views.source_bases[&original.operation],
        (fixture.model.identity, PathBuf::from("/repo/.git"))
    );
    assert_eq!(
        fixture.app.shared_views.source_bases[&retry.operation],
        (fixture.model.identity, PathBuf::from("/repo/.git"))
    );
    assert_eq!(
        fixture
            .app
            .local_view_panes(&Scope::Channel(fixture.source))
            .into_iter()
            .find(|pane| pane.id == fixture.canonical.id)
            .unwrap()
            .user_title
            .as_deref(),
        Some("Renamed during seed")
    );
    let expected = [b.id, c.id, fixture.canonical.id];
    settle_retry(&mut fixture, original, retry, &expected);
}

#[test]
fn a_local_draft_blocks_alias_adoption_and_does_not_translate_its_move() {
    let mut fixture = seed_fixture(false);
    let [a, b, c] = fixture.panes.clone();
    reorder(&mut fixture.app, a.id, c.id);
    // Use the actual draft guard rather than inserting a synthetic blocked
    // scope. A saved local draft makes this adoption ineligible as a whole.
    fixture.app.chat_ui =
        chat_ui::State::with_drafts(HashMap::from([(a.id, "keep local draft".into())]));
    seed_ack(&mut fixture);
    let issued = publications(&fixture.rx);
    assert_eq!(issued.len(), 1);
    assert_eq!(issued[0].scope, Scope::Channel(fixture.source));
    assert_eq!(
        issued[0].change,
        Change::Move {
            id: a.id,
            before: Some(c.id),
        }
    );
    assert_eq!(fixture.app.shared_views.pending, [issued[0].pending()]);
    assert!(
        fixture
            .app
            .shared_views
            .blocked
            .contains(&Scope::Channel(fixture.source))
    );
    assert_projection(&fixture.app, fixture.source, &[b.id, a.id, c.id]);
    assert!(fixture.app.card_state(a.id).unwrap().existence.is_open());
    assert!(fixture.app.card_state(fixture.canonical.id).is_none());
    assert_quiet_read(&mut fixture, &[b.id, a.id, c.id]);
}

#[test]
fn adoption_in_another_scope_does_not_redirect_the_source_reorder() {
    let mut fixture = seed_fixture(false);
    let [a, b, c] = fixture.panes.clone();
    for view in &mut fixture.model.views {
        if view.scope == Scope::Channel(fixture.source) {
            view.panes.retain(|pane| pane.id != fixture.canonical.id);
        } else if view.scope == Scope::Channel(fixture.destination) {
            view.panes.push(fixture.canonical.clone());
        }
    }
    reorder(&mut fixture.app, a.id, c.id);
    seed_ack(&mut fixture);
    let issued = publications(&fixture.rx);
    assert_eq!(issued.len(), 1);
    assert_eq!(issued[0].scope, Scope::Channel(fixture.source));
    assert_eq!(
        issued[0].change,
        Change::Move {
            id: a.id,
            before: Some(c.id),
        }
    );
    assert_eq!(fixture.app.shared_views.pending, [issued[0].pending()]);
    assert_projection(&fixture.app, fixture.source, &[b.id, c.id]);
    assert_eq!(
        channel_order(&fixture.app, fixture.destination),
        [fixture.canonical.id]
    );
    assert_quiet_read(&mut fixture, &[b.id, c.id]);
}

#[test]
fn source_changing_updates_are_not_translated_to_an_adopted_alias() {
    for field in ["kind", "scope"] {
        let mut fixture = seed_fixture(false);
        let [a, b, c] = fixture.panes.clone();
        // Typed descriptor data at the persistence boundary, not a simulated
        // browser integration or provider observation. The real persist/seed
        // ACK producer must queue the Update; pending is never inserted here.
        match field {
            "kind" => {
                let CardKind::Browser(browser) =
                    &mut fixture.app.card_state_mut(a.id).unwrap().kind
                else {
                    panic!("browser fixture");
                };
                browser.current_url = "https://example.com/local-source-change".parse().unwrap();
            }
            "scope" => {
                Arc::make_mut(&mut fixture.app.durable)
                    .card_scopes
                    .insert(a.id, PathBuf::from("/repo/local-source-change"));
            }
            _ => unreachable!(),
        }
        fixture.app.persist();
        assert!(fixture.app.shared_views.pending.is_empty());
        assert!(publications(&fixture.rx).is_empty());
        seed_ack(&mut fixture);
        let issued = publications(&fixture.rx);
        assert_eq!(issued.len(), 1, "source field: {field}");
        let original = &issued[0];
        let Change::Update { before, pane } = &original.change else {
            panic!("persist must publish the descriptor change: {field}");
        };
        assert_eq!(before.id, a.id);
        assert_eq!(pane.id, a.id);
        match field {
            "kind" => assert_ne!(before.kind, pane.kind),
            "scope" => assert_ne!(before.scope, pane.scope),
            _ => unreachable!(),
        }
        assert_eq!(fixture.app.shared_views.pending, [original.pending()]);
        let current = fixture
            .app
            .local_view_panes(&Scope::Channel(fixture.source))
            .into_iter()
            .find(|pane| pane.id == fixture.canonical.id)
            .unwrap();
        assert_eq!(current.kind, fixture.canonical.kind);
        assert_eq!(current.scope, fixture.canonical.scope);
        assert_eq!(
            current.pending_terminal_start,
            fixture.canonical.pending_terminal_start
        );
        assert_quiet_read(&mut fixture, &[b.id, c.id, current.id]);
    }
}

#[test]
fn presentation_retries_cannot_gain_unknown_or_foreign_source_authority() {
    for origin in [
        "unknown_repository",
        "changed_repository",
        "changed_registry",
    ] {
        let mut fixture = seed_fixture(false);
        let [a, b, c] = fixture.panes.clone();
        // First settle a seed that retains the alias. A later public rename
        // can then genuinely publish before the canonical-identity Read.
        let mut alias_model = fixture.app.workspace_model.clone();
        alias_model.sequence += 1;
        observe(
            &mut fixture.app,
            ModelUpdate::ViewApplied {
                scope: Scope::Channel(fixture.source),
                operation: None,
                model: alias_model,
            },
        );
        assert!(fixture.app.shared_views.pending.is_empty());
        assert!(publications(&fixture.rx).is_empty());
        let original_identity = fixture.app.workspace_model.identity;
        if origin == "unknown_repository" {
            // Typed incomplete metadata fixture: without a source channel in
            // this inventory, the real publisher cannot capture its repository.
            // Do not remove source_bases or inject a pending operation by hand.
            fixture
                .app
                .workspace_model
                .channels
                .retain(|channel| channel.id != fixture.source);
        }
        rename(&mut fixture.app, a.id);
        let issued = publications(&fixture.rx);
        assert_eq!(issued.len(), 1, "origin: {origin}");
        let original = &issued[0];
        assert_eq!(original.scope, Scope::Channel(fixture.source));
        assert!(matches!(
            &original.change,
            Change::Update { before, pane } if before.id == a.id && pane.id == a.id
        ));
        let basis = fixture
            .app
            .shared_views
            .source_bases
            .get(&original.operation)
            .cloned();
        if origin == "unknown_repository" {
            assert_eq!(basis, None);
        } else {
            assert_eq!(
                basis,
                Some((original_identity, PathBuf::from("/repo/.git")))
            );
        }
        // These are explicit inventory/authority fixtures, not claims about a
        // live registry reconnect or host source-verification integration.
        match origin {
            "changed_repository" => {
                fixture
                    .model
                    .channels
                    .iter_mut()
                    .find(|channel| channel.id == fixture.source)
                    .unwrap()
                    .repository = PathBuf::from("/other-repository/.git");
            }
            "changed_registry" => fixture.model.identity = uuid::Uuid::new_v4(),
            "unknown_repository" => {}
            _ => unreachable!(),
        }
        fixture.model.sequence += 1;
        observe(&mut fixture.app, ModelUpdate::Read(fixture.model.clone()));
        assert!(
            publications(&fixture.rx).is_empty(),
            "alias adoption must not mint source authority: {origin}"
        );
        assert_eq!(fixture.app.shared_views.pending, [original.pending()]);
        assert_eq!(
            fixture
                .app
                .shared_views
                .source_bases
                .get(&original.operation)
                .cloned(),
            basis
        );
        let current = fixture
            .app
            .local_view_panes(&Scope::Channel(fixture.source))
            .into_iter()
            .find(|pane| pane.id == fixture.canonical.id)
            .unwrap();
        assert_eq!(current.title, fixture.canonical.title);
        assert_eq!(current.user_title, fixture.canonical.user_title);
        assert_eq!(current.kind, fixture.canonical.kind);
        assert_quiet_read(&mut fixture, &[b.id, c.id, current.id]);
    }
}
