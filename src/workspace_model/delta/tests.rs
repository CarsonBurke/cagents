use super::*;
use crate::model::{BrowserId, BrowserSession, CardKind, ChannelId};
use crate::workspace_model::views::Pane;

fn pane() -> Pane {
    Pane {
        id: CardId::new(),
        title: "browser".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: "https://example.com".parse().unwrap(),
            history: vec![],
            history_index: 0,
            scroll_x: 0.,
            scroll_y: 0.,
            fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
        }),
    }
}

fn view(scope: &Scope, revision: u64, panes: Vec<Pane>) -> View {
    View {
        scope: scope.clone(),
        revision,
        panes,
        retired: Vec::new(),
    }
}

fn change(scope: &Scope, change: Change) -> ModelEdit {
    ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: 0,
        operation: uuid::Uuid::new_v4(),
        change,
    }
}

fn revised(scope: &Scope, before: Option<u64>, after: Option<u64>) -> ViewRevision {
    ViewRevision {
        scope: scope.clone(),
        before,
        after,
    }
}

fn effects(views: Vec<ViewRevision>, closed: Vec<CardId>) -> EditEffects {
    EditEffects {
        views,
        closed,
        ..Default::default()
    }
}

#[test]
fn an_edge_applies_only_to_the_revision_it_was_made_against() {
    let scope = Scope::Channel(ChannelId::new());
    let (first, second) = (pane(), pane());
    let insert = change(
        &scope,
        Change::Insert {
            pane: second.clone(),
            before: None,
        },
    );
    // Revisions come from one clock, so 5 -> 9 is one edit, not four.
    let mut mirror = Mirror::new(&[view(&scope, 5, vec![first.clone()])]);
    let applied = mirror.apply(&insert, &effects(vec![revised(&scope, Some(5), Some(9))], vec![]));
    assert_eq!(
        applied,
        Applied {
            changes: ViewChanges {
                views: vec![view(&scope, 9, vec![first.clone(), second.clone()])],
                removed: vec![],
            },
            unresolved: vec![],
        }
    );
    assert_eq!(mirror.revision(&scope), Some(9));

    // An edge made against a revision this copy never held is read instead.
    let mut behind = Mirror::new(&[view(&scope, 4, vec![first.clone()])]);
    let applied = behind.apply(&insert, &effects(vec![revised(&scope, Some(5), Some(9))], vec![]));
    assert_eq!(applied.unresolved, vec![scope.clone()]);
    assert!(applied.changes.is_empty());
    assert_eq!(behind.revision(&scope), Some(4), "nothing is guessed");
}

#[test]
fn a_scope_the_edit_created_or_a_seed_is_read_rather_than_derived() {
    let scope = Scope::Channel(ChannelId::new());
    let mut mirror = Mirror::default();
    // The daemon may have refused the insert itself (another view holds the
    // resource) and still created the scope.
    let insert = change(
        &scope,
        Change::Insert {
            pane: pane(),
            before: None,
        },
    );
    let applied = mirror.apply(&insert, &effects(vec![revised(&scope, None, Some(3))], vec![]));
    assert_eq!(applied.unresolved, vec![scope.clone()]);

    let mut mirror = Mirror::new(&[view(&scope, 3, vec![])]);
    let seed = ModelEdit::SeedView {
        scope: scope.clone(),
        panes: vec![pane()],
        closed: vec![],
    };
    let applied = mirror.apply(&seed, &effects(vec![revised(&scope, Some(3), Some(4))], vec![]));
    assert_eq!(applied.unresolved, vec![scope]);
}

#[test]
fn a_removal_closes_the_pane_the_daemon_named_or_is_read() {
    let scope = Scope::Channel(ChannelId::new());
    let (kept, closed) = (pane(), pane());
    let held = view(&scope, 7, vec![kept.clone(), closed.clone()]);
    let remove = change(&scope, Change::Remove { id: closed.id });
    let mut mirror = Mirror::new(std::slice::from_ref(&held));
    let applied = mirror.apply(
        &remove,
        &effects(vec![revised(&scope, Some(7), Some(8))], vec![closed.id]),
    );
    let mut expected = view(&scope, 8, vec![kept.clone()]);
    expected.retired.push(closed.id);
    assert_eq!(applied.changes.views, vec![expected]);

    // The daemon closed a pane this edit does not close here: the copies
    // disagree, so the scope is read.
    let mut mirror = Mirror::new(&[held]);
    let applied = mirror.apply(
        &remove,
        &effects(vec![revised(&scope, Some(7), Some(8))], vec![kept.id]),
    );
    assert_eq!(applied.unresolved, vec![scope]);
}

#[test]
fn a_transfer_moves_the_pane_between_both_views_including_a_new_one() {
    let source = Scope::Channel(ChannelId::new());
    let destination = Scope::Channel(ChannelId::new());
    let moved = pane();
    let transfer = change(
        &source,
        Change::Transfer {
            id: moved.id,
            destination: destination.clone(),
            before: None,
        },
    );
    let mut mirror = Mirror::new(&[view(&source, 2, vec![moved.clone()])]);
    let applied = mirror.apply(
        &transfer,
        &effects(
            vec![
                revised(&source, Some(2), Some(10)),
                revised(&destination, None, Some(11)),
            ],
            vec![],
        ),
    );
    let mut emptied = view(&source, 10, vec![]);
    emptied.retired.push(moved.id);
    assert_eq!(
        applied.changes.views,
        vec![emptied, view(&destination, 11, vec![moved])]
    );
    assert!(applied.unresolved.is_empty());
    assert_eq!(mirror.revision(&destination), Some(11));
}

#[test]
fn a_scope_already_read_past_the_edge_is_resent_rather_than_rewound() {
    let scope = Scope::Channel(ChannelId::new());
    let newer = view(&scope, 12, vec![pane()]);
    let mut mirror = Mirror::new(std::slice::from_ref(&newer));
    let applied = mirror.apply(
        &change(&scope, Change::Remove { id: CardId::new() }),
        &effects(vec![revised(&scope, Some(5), Some(9))], vec![]),
    );
    // The subscriber may have installed an older snapshot since the read; this
    // edge is its prompt to take the newer view again.
    assert_eq!(applied.changes.views, vec![newer]);
    assert!(applied.unresolved.is_empty());
    assert_eq!(mirror.revision(&scope), Some(12));
}

#[test]
fn a_deleted_scope_goes_unless_a_later_incarnation_replaced_it() {
    let channel = Scope::Channel(ChannelId::new());
    let worktree = Scope::Worktree {
        repository: "/repo/.git".into(),
        path: "/repo/feature".into(),
    };
    let reopened = view(&worktree, 30, vec![]);
    let mut mirror = Mirror::new(&[view(&channel, 4, vec![pane()]), reopened.clone()]);
    let applied = mirror.apply(
        &ModelEdit::CloseProject {
            repository: "/repo/.git".into(),
        },
        &effects(
            vec![
                revised(&channel, Some(4), None),
                revised(&worktree, Some(20), None),
            ],
            vec![],
        ),
    );
    assert_eq!(
        applied.changes.removed,
        vec![(channel.clone(), 4), (worktree.clone(), 20)]
    );
    assert_eq!(applied.changes.views, vec![reopened]);
    assert_eq!(mirror.revision(&channel), None);
    assert_eq!(mirror.revision(&worktree), Some(30));
}

#[test]
fn a_read_installs_the_newest_view_or_the_removal() {
    let scope = Scope::Channel(ChannelId::new());
    let mut mirror = Mirror::new(&[view(&scope, 6, vec![])]);
    let read = view(&scope, 8, vec![pane()]);
    assert_eq!(mirror.install(scope.clone(), Some(read.clone())).views, vec![read.clone()]);
    // A read cannot rewind what an edge already brought further.
    let older = view(&scope, 7, vec![]);
    assert_eq!(mirror.install(scope.clone(), Some(older)).views, vec![read]);
    assert_eq!(
        mirror.install(scope.clone(), None).removed,
        vec![(scope.clone(), 8)]
    );
    assert_eq!(mirror.revision(&scope), None);
}
