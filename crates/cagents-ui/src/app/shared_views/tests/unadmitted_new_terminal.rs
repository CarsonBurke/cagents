//! A terminal opened in a channel whose shared view is not admitted yet, as
//! on a device whose daemon has just started, waits for that view instead of
//! closing when the view first arrives without it (#4380).
use super::*;
use crate::app::workspace_model_sync::ModelUpdate;

struct Window {
    app: Cagents,
    scope: Scope,
    rx: TestModelRequests,
}

fn window() -> Window {
    let (app, channel, _card, _target, rx) = unpublished_default_chat();
    Window {
        app,
        scope: Scope::Channel(channel),
        rx,
    }
}

fn drain(rx: &TestModelRequests) {
    while rx.try_recv().is_ok() {}
}

fn inserts(rx: &TestModelRequests, card: CardId) -> usize {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|edit| {
            matches!(edit, ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card)
        })
        .count()
}

fn new_terminal(window: &mut Window) -> CardId {
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(window.app.insert_terminal_card(session, None));
    let card = window.app.workspace.focused().unwrap();
    assert!(
        window
            .app
            .durable
            .pending_terminal_creations
            .contains(&card)
    );
    card
}

fn seed_applied(window: &mut Window, panes: Vec<Pane>) {
    let scope = window.scope.clone();
    let mut model = window.app.workspace_model.clone();
    model.sequence += 1;
    let mut view = View::empty(scope.clone());
    view.revision = 1;
    view.panes = panes;
    model.views.retain(|view| view.scope != scope);
    model.views.push(view);
    let attempt = window.app.shared_views.seed_attempts[&scope];
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::SeedApplied {
                scope,
                attempt,
                model,
            }),
    );
}

fn assert_open(window: &Window, card: CardId) {
    assert!(
        window
            .app
            .card_state(card)
            .is_some_and(|card| card.existence.is_open()),
        "the new terminal closed before its view could accept it"
    );
    assert!(
        window
            .app
            .workspace
            .cards()
            .iter()
            .any(|shown| shown.id == card)
    );
}

#[test]
fn a_terminal_opened_while_its_channel_seeds_survives_the_seed() {
    let mut window = window();
    let scope = window.scope.clone();
    window.app.workspace_model.views.clear();
    window.app.shared_views.initialized.remove(&scope);
    window.app.shared_views.baseline.remove(&scope);
    drop(window.app.reconcile_shared_views());
    assert!(window.app.shared_views.seeding.contains(&scope));
    drain(&window.rx);

    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    assert_open(&window, card);
    // Nothing is inserted into a view still being seeded.
    assert_eq!(inserts(&window.rx, card), 0);

    seed_applied(&mut window, Vec::new());
    assert_open(&window, card);
    assert_eq!(inserts(&window.rx, card), 1);
}

#[test]
fn a_terminal_opened_while_connecting_survives_joining_the_registry() {
    let mut window = window();
    window.app.workspace_model.identity = crate::workspace_model::NO_REGISTRY;
    window.app.workspace_model.views.clear();
    drop(window.app.reconcile_shared_views());
    drain(&window.rx);

    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    assert_open(&window, card);

    let mut model = window.app.workspace_model.clone();
    model.identity = uuid::Uuid::new_v4();
    model.sequence += 1;
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::Connected(model)),
    );
    assert_open(&window, card);
    let mut early_inserts = 0;
    let seeded = std::iter::from_fn(|| window.rx.try_recv().ok())
        .find_map(|edit| match edit {
            ModelEdit::SeedView { scope, panes, .. } if scope == window.scope => Some(panes),
            ModelEdit::ChangeView {
                change: Change::Insert { pane, .. },
                ..
            } if pane.id == card => {
                early_inserts += 1;
                None
            }
            _ => None,
        })
        .expect("joining seeds the channel");
    assert_eq!(early_inserts, 0);
    // A creation still awaiting acceptance never migrates with the seed; it
    // is shared by its own Insert once the seed is in.
    assert!(seeded.iter().all(|pane| pane.id != card));
    seed_applied(&mut window, seeded);
    assert_open(&window, card);
    assert_eq!(inserts(&window.rx, card), 1);
}

#[test]
fn a_terminal_opened_in_a_blocked_channel_survives_the_block() {
    let mut window = window();
    // A terminal here whose canonical pane is listed by a view this window
    // cannot project holds the channel back until that view goes. The
    // previous pass already found it held back.
    window.app.shared_views.blocked.insert(window.scope.clone());
    let session = fresh_terminal_session(LaunchRecipe::shell(PathBuf::from("/repo")).unwrap());
    drop(window.app.insert_terminal_card(session, None));
    let alias = window.app.workspace.focused().unwrap();
    let mut canonical = Pane::from_card(window.app.card_state(alias).unwrap(), None).unwrap();
    canonical.id = CardId::new();
    let mut elsewhere = View::empty(ChannelId::new());
    elsewhere.panes.push(canonical);
    window.app.workspace_model.views.push(elsewhere);
    drop(window.app.reconcile_shared_views());
    assert!(window.app.shared_views.blocked.contains(&window.scope));
    drain(&window.rx);

    let card = new_terminal(&mut window);
    assert_ne!(card, alias);
    drop(window.app.reconcile_shared_views());
    assert!(window.app.shared_views.blocked.contains(&window.scope));
    assert_open(&window, card);
    assert_eq!(inserts(&window.rx, card), 0);

    window
        .app
        .workspace_model
        .views
        .retain(|view| view.scope == window.scope);
    drop(window.app.reconcile_shared_views());
    assert!(!window.app.shared_views.blocked.contains(&window.scope));
    assert_open(&window, card);
    assert_eq!(inserts(&window.rx, card), 1);
}

#[test]
fn a_new_terminal_its_view_refuses_still_closes() {
    let mut window = window();
    drain(&window.rx);
    let card = new_terminal(&mut window);
    drop(window.app.reconcile_shared_views());
    assert_eq!(inserts(&window.rx, card), 1);

    // Sent and refused, it has nothing to wait for: the view that does not
    // list it closes it rather than inserting it again.
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::ViewRefused {
                scope: window.scope.clone(),
                operation: card.as_uuid(),
                error: "refused".into(),
            }),
    );
    assert_eq!(inserts(&window.rx, card), 0);
    assert!(
        !window
            .app
            .card_state(card)
            .is_some_and(|card| card.existence.is_open())
    );
    assert!(
        !window
            .app
            .durable
            .pending_terminal_creations
            .contains(&card)
    );
    assert!(!window.app.shared_views.sent_creations.contains(&card));
}
