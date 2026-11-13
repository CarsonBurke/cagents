use super::*;
use crate::model::{BrowserId, BrowserSession, CardId, CardKind};
use crate::workspace_model::{
    NavigationItem,
    views::{Change, Pane, Scope},
};

fn setup() -> (Registry, tempfile::TempDir, ChannelId) {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = Registry::at(&dir.path().join("model.sqlite3")).unwrap();
    registry
        .apply_delta(&ModelEdit::OpenProject {
            repository: "/repo".into(),
            name: "repo".into(),
            kind: Default::default(),
        })
        .unwrap();
    let channel = ChannelId::new();
    registry
        .apply_delta(&ModelEdit::CreateChannel {
            id: channel,
            repository: "/repo".into(),
            title: "channel".into(),
            title_source: Default::default(),
        })
        .unwrap();
    (registry, dir, channel)
}

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

#[test]
fn no_op_receipts_supply_canonical_metadata_without_advancing_the_clock() {
    let (mut registry, _dir, id) = setup();
    let before = registry.model().unwrap();
    let receipt = registry
        .apply_delta(&ModelEdit::CreateChannel {
            id,
            repository: "/repo".into(),
            title: "stale optimistic name".into(),
            title_source: Default::default(),
        })
        .unwrap();
    assert!(!receipt.changed);
    assert_eq!(receipt.receipt.sequence, before.sequence);
    assert_eq!(receipt.receipt.effects.channels, before.channels);
    assert_eq!(receipt.receipt.identity, before.identity);
    let receipt = registry
        .apply_delta(&ModelEdit::OpenProject {
            repository: "/repo".into(),
            name: "repo".into(),
            kind: Default::default(),
        })
        .unwrap();
    assert!(!receipt.changed);
    assert_eq!(receipt.receipt.effects.projects, before.projects);
    registry
        .apply_delta(&ModelEdit::DeleteChannel { id })
        .unwrap();
    let receipt = registry
        .apply_delta(&ModelEdit::RetitleChannel {
            id,
            title: "cannot revive".into(),
        })
        .unwrap();
    assert!(!receipt.changed);
    assert_eq!(receipt.receipt.effects.removed_channels, vec![id]);
}

#[test]
fn a_delta_commit_and_scope_read_do_not_parse_an_unrelated_scope() {
    let (mut registry, _dir, id) = setup();
    let other = ChannelId::new();
    registry
        .apply_delta(&ModelEdit::CreateChannel {
            id: other,
            repository: "/repo".into(),
            title: "other".into(),
            title_source: Default::default(),
        })
        .unwrap();
    let broken = pane();
    registry
        .apply_delta(&ModelEdit::SeedView {
            scope: Scope::Channel(other),
            panes: vec![broken.clone()],
            closed: vec![],
        })
        .unwrap();
    registry
        .db
        .execute(
            "UPDATE panes SET descriptor = 'unreadable' WHERE id = ?1",
            [broken.id.to_string()],
        )
        .unwrap();
    assert!(
        registry.model().is_err(),
        "control proves a whole read reaches the bad descriptor"
    );
    use crate::workspace_model::metadata::{Query, Reply};
    let expected_identity = registry.metadata(Query::Identity).unwrap();
    let Reply::Identity(identity) = expected_identity else {
        panic!("identity reply")
    };
    assert!(
        matches!(registry.metadata(Query::Projects).unwrap(), Reply::Projects { identity: held, projects } if held == identity && projects.len() == 1)
    );
    assert!(
        matches!(registry.metadata(Query::Agents).unwrap(), Reply::Agents { identity: held, agents } if held == identity && agents.is_empty())
    );

    let kept = pane();
    let committed = registry
        .apply_delta(&ModelEdit::ChangeView {
            scope: Scope::Channel(id),
            expected_revision: 0,
            operation: Uuid::new_v4(),
            change: Change::Insert {
                pane: kept.clone(),
                before: None,
            },
        })
        .unwrap();
    assert!(committed.changed);
    assert_eq!(committed.receipt.effects.views.len(), 1);
    let (view, sequence) = registry.view_snapshot(&Scope::Channel(id)).unwrap();
    assert_eq!(view.unwrap().panes, vec![kept.clone()]);
    assert_eq!(
        registry.pane_view(kept.id).unwrap().unwrap().panes,
        vec![kept]
    );
    assert_eq!(sequence, committed.receipt.sequence);
}

#[test]
fn navigation_receipts_carry_the_merged_order_and_reset() {
    let (mut registry, _dir, id) = setup();
    let first = NavigationItem::Channel(id);
    let second = NavigationItem::Worktree("/repo/second".into());
    let third = NavigationItem::Worktree("/repo/third".into());
    registry
        .apply_delta(&ModelEdit::ReorderNavigation {
            repository: "/repo".into(),
            source: second.clone(),
            before: Some(first.clone()),
            fallback: vec![first.clone(), second.clone(), third.clone()],
        })
        .unwrap();
    // A stale fallback from another window must preserve the third item.
    let receipt = registry
        .apply_delta(&ModelEdit::ReorderNavigation {
            repository: "/repo".into(),
            source: first.clone(),
            before: Some(second.clone()),
            fallback: vec![first.clone(), second.clone()],
        })
        .unwrap();
    assert_eq!(
        receipt.receipt.effects.navigation_orders[0].items,
        vec![first, second, third]
    );
    let receipt = registry
        .apply_delta(&ModelEdit::ResetNavigation {
            repository: "/repo".into(),
        })
        .unwrap();
    assert!(
        receipt.receipt.effects.navigation_orders[0]
            .items
            .is_empty()
    );
}

#[test]
fn subscriber_snapshot_excludes_transfer_tombstones_from_closed_identities() {
    let (mut registry, _dir, id) = setup();
    let other = ChannelId::new();
    registry
        .apply_delta(&ModelEdit::CreateChannel {
            id: other,
            repository: "/repo".into(),
            title: "other".into(),
            title_source: Default::default(),
        })
        .unwrap();
    let moved = pane();
    let closed = pane();
    registry
        .apply_delta(&ModelEdit::SeedView {
            scope: Scope::Channel(id),
            panes: vec![moved.clone(), closed.clone()],
            closed: vec![],
        })
        .unwrap();
    for change in [
        Change::Transfer {
            id: moved.id,
            destination: Scope::Channel(other),
            before: None,
        },
        Change::Remove { id: closed.id },
    ] {
        let revision = registry.view_revision(&Scope::Channel(id)).unwrap();
        registry
            .apply_delta(&ModelEdit::ChangeView {
                scope: Scope::Channel(id),
                expected_revision: revision,
                operation: Uuid::new_v4(),
                change,
            })
            .unwrap();
    }
    let (snapshot, ids) = registry.subscriber_snapshot().unwrap();
    assert_eq!(ids, vec![closed.id]);
    assert_eq!(snapshot, registry.model().unwrap());
}
