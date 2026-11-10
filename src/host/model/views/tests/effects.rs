//! Every edit a registry applies, mirrored from its effects alone, leaves the
//! subscriber's copy equal to the registry's own views.

use super::*;
use crate::workspace_model::delta::{EditEffects, Mirror};

/// A subscriber that knows only the starting model, the edges and the scopes
/// it reads when an edge does not determine them.
struct Subscriber {
    mirror: Mirror,
    /// Scopes read so far, by `ReadWorkspaceView`.
    reads: Vec<Scope>,
}

impl Subscriber {
    fn new(registry: &Registry) -> Self {
        Self {
            mirror: Mirror::new(&registry.model().unwrap().views),
            reads: Vec::new(),
        }
    }

    /// Applies `edit` to the registry and its edge to this copy; returns the
    /// edge's effects and the scopes this edge had to read.
    fn follow(&mut self, registry: &mut Registry, edit: &ModelEdit) -> (EditEffects, Vec<Scope>) {
        let applied = registry.apply(edit).unwrap();
        assert!(applied.changed, "{edit:?} must change the model");
        let unresolved = self.mirror.apply(edit, &applied.effects).unresolved;
        for scope in &unresolved {
            let (view, _) = registry.view_snapshot(scope).unwrap();
            self.mirror.install(scope.clone(), view);
        }
        self.reads.extend(unresolved.iter().cloned());
        self.assert_matches(registry);
        (applied.effects, unresolved)
    }

    fn assert_matches(&self, registry: &Registry) {
        let mut held = self.mirror.views().cloned().collect::<Vec<_>>();
        let mut canonical = registry.model().unwrap().views;
        let key = |view: &View| format!("{:?}", view.scope);
        held.sort_by_key(key);
        canonical.sort_by_key(key);
        assert_eq!(held, canonical);
    }
}

fn changed(effects: &EditEffects) -> Vec<(Scope, Option<u64>, Option<u64>)> {
    effects
        .views
        .iter()
        .map(|revision| (revision.scope.clone(), revision.before, revision.after))
        .collect()
}

#[test]
fn view_edits_are_derived_from_the_edge_without_reading() {
    let (mut r, _directory, a, b) = setup();
    let mut subscriber = Subscriber::new(&r);
    let (first, second, third) = (pane(), pane(), pane());

    // A seed is a whole view the edge does not carry: it is read.
    let seed = ModelEdit::SeedView {
        scope: Scope::Channel(a),
        panes: vec![first.clone(), second.clone()],
        closed: vec![],
    };
    let (effects, reads) = subscriber.follow(&mut r, &seed);
    assert_eq!(reads, vec![Scope::Channel(a)]);
    let seeded = r.view_revision(&Scope::Channel(a)).unwrap();
    assert_eq!(
        changed(&effects),
        vec![(Scope::Channel(a), None, Some(seeded))]
    );

    let mut retitled = first.clone();
    retitled.user_title = Some("renamed".into());
    for change in [
        Change::Insert {
            pane: third.clone(),
            before: Some(second.id),
        },
        Change::Update {
            before: first.clone(),
            pane: retitled,
        },
        Change::Move {
            id: first.id,
            before: None,
        },
        Change::Move {
            id: third.id,
            before: Some(first.id),
        },
    ] {
        let before = r.view_revision(&Scope::Channel(a)).unwrap();
        let change = edit(&mut r, a, change);
        let (effects, reads) = subscriber.follow(&mut r, &change);
        assert_eq!(reads, vec![], "{change:?} is derived from the edge");
        assert_eq!(
            changed(&effects),
            vec![(
                Scope::Channel(a),
                Some(before),
                Some(r.view_revision(&Scope::Channel(a)).unwrap())
            )]
        );
        assert!(effects.closed.is_empty());
    }

    // A removal names the pane it closed.
    let remove = edit(&mut r, a, Change::Remove { id: second.id });
    let (effects, reads) = subscriber.follow(&mut r, &remove);
    assert_eq!(reads, vec![]);
    assert_eq!(effects.closed, vec![second.id]);

    // A transfer changes both views, into a scope the edit creates, and
    // closes nothing.
    let transfer = edit(
        &mut r,
        a,
        Change::Transfer {
            id: third.id,
            destination: Scope::Channel(b),
            before: None,
        },
    );
    let (effects, reads) = subscriber.follow(&mut r, &transfer);
    assert_eq!(reads, vec![]);
    assert_eq!(
        changed(&effects)
            .into_iter()
            .map(|(scope, before, _)| (scope, before.is_some()))
            .collect::<Vec<_>>(),
        vec![(Scope::Channel(a), true), (Scope::Channel(b), false)]
    );
    assert!(effects.closed.is_empty(), "a transferred pane is not closed");

    // And back, into a destination that now exists.
    let back = edit(
        &mut r,
        b,
        Change::Transfer {
            id: third.id,
            destination: Scope::Channel(a),
            before: Some(first.id),
        },
    );
    let (effects, reads) = subscriber.follow(&mut r, &back);
    assert_eq!(reads, vec![]);
    assert_eq!(effects.views.len(), 2);
    assert_eq!(subscriber.reads, vec![Scope::Channel(a)], "only the seed was read");
}

#[test]
fn an_insert_into_a_new_scope_is_read_because_the_daemon_may_refuse_the_pane() {
    let (mut r, _directory, a, b) = setup();
    let shared = pane();
    seed(&mut r, a, vec![shared.clone()]);
    let mut subscriber = Subscriber::new(&r);
    // The daemon keeps one pane per hosted resource: the insert into `b` is
    // refused, yet the edge still creates `b`'s scope.
    let mut alias = shared.clone();
    alias.id = CardId::new();
    let insert = edit(
        &mut r,
        b,
        Change::Insert {
            pane: alias,
            before: None,
        },
    );
    let (effects, reads) = subscriber.follow(&mut r, &insert);
    assert_eq!(reads, vec![Scope::Channel(b)]);
    assert_eq!(effects.views[0].before, None);

    // A fresh pane inserted into a scope that exists is derived.
    let insert = edit(
        &mut r,
        b,
        Change::Insert {
            pane: pane(),
            before: None,
        },
    );
    assert_eq!(subscriber.follow(&mut r, &insert).1, vec![]);
}

#[test]
fn a_subscriber_behind_the_edge_reads_the_scope_instead_of_guessing() {
    let (mut r, _directory, a, _) = setup();
    let (first, second) = (pane(), pane());
    seed(&mut r, a, vec![first.clone()]);
    let mut subscriber = Subscriber::new(&r);
    // An edge this subscriber missed.
    let missed = edit(
        &mut r,
        a,
        Change::Insert {
            pane: second.clone(),
            before: None,
        },
    );
    r.apply(&missed).unwrap();
    let remove = edit(&mut r, a, Change::Remove { id: first.id });
    let (_, reads) = subscriber.follow(&mut r, &remove);
    assert_eq!(reads, vec![Scope::Channel(a)]);
}

#[test]
fn deleting_a_channel_or_closing_a_project_removes_its_views_and_closes_their_panes() {
    let (mut r, _directory, a, b) = setup();
    let (in_a, in_b) = (pane(), pane());
    seed(&mut r, a, vec![in_a.clone()]);
    seed(&mut r, b, vec![in_b.clone()]);
    let mut subscriber = Subscriber::new(&r);
    let revision = r.view_revision(&Scope::Channel(a)).unwrap();
    let (effects, reads) = subscriber.follow(&mut r, &ModelEdit::DeleteChannel { id: a });
    assert_eq!(reads, vec![]);
    assert_eq!(
        changed(&effects),
        vec![(Scope::Channel(a), Some(revision), None)]
    );
    assert_eq!(effects.closed, vec![in_a.id]);

    let revision = r.view_revision(&Scope::Channel(b)).unwrap();
    let (effects, reads) = subscriber.follow(
        &mut r,
        &ModelEdit::CloseProject {
            repository: "/repo".into(),
        },
    );
    assert_eq!(reads, vec![]);
    assert_eq!(
        changed(&effects),
        vec![(Scope::Channel(b), Some(revision), None)]
    );
    assert_eq!(effects.closed, vec![in_b.id]);
}

#[test]
fn a_new_primary_conversation_carries_the_channel_title_it_reset() {
    let (mut r, _directory, _, _) = setup();
    let channel = ChannelId::new();
    r.apply(&ModelEdit::CreateChannel {
        id: channel,
        repository: "/repo".into(),
        title: "Initial inferred title".into(),
        title_source: crate::model::ChannelTitleSource::Prompt,
    })
    .unwrap();
    let mut original = live_source_pane();
    let CardKind::Terminal(terminal) = original.kind else {
        unreachable!()
    };
    original.kind = CardKind::Chat(crate::chat::Session {
        id: terminal.id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            recipe: terminal.launch.clone(),
            session_id: Some("first-thread".into()),
        },
        proof: terminal.continuity.live_proof(),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    });
    seed(&mut r, channel, vec![original.clone()]);
    let mut subscriber = Subscriber::new(&r);

    // A suggestion lands only if the stored title accepts it, so the edge
    // carries the row it left.
    let suggestion = ModelEdit::SuggestConversationTitle {
        id: channel,
        card: original.id,
        expected_revision: r.view_revision(&Scope::Channel(channel)).unwrap(),
        title: "Provider chosen".into(),
        source: crate::model::ChannelTitleSource::Provider,
    };
    let (effects, _) = subscriber.follow(&mut r, &suggestion);
    let row = r.model().unwrap().channel(channel).unwrap().clone();
    assert_eq!(row.title, "Provider chosen");
    assert_eq!(effects.channels, vec![row]);
    assert!(effects.views.is_empty());

    let mut successor = original.clone();
    let CardKind::Chat(chat) = &mut successor.kind else {
        unreachable!()
    };
    chat.launch.session_id = Some("replacement-thread".into());
    let update = edit(
        &mut r,
        channel,
        Change::Update {
            before: original,
            pane: successor,
        },
    );
    let (effects, reads) = subscriber.follow(&mut r, &update);
    assert_eq!(reads, vec![]);
    let row = r.model().unwrap().channel(channel).unwrap().clone();
    assert_eq!(row.title, "", "the automatic title went with the thread");
    assert_eq!(effects.channels, vec![row]);

    // An edit that leaves the title alone carries no row.
    let insert = edit(
        &mut r,
        channel,
        Change::Insert {
            pane: pane(),
            before: None,
        },
    );
    assert!(subscriber.follow(&mut r, &insert).0.channels.is_empty());
}

#[test]
fn an_unchanged_edit_carries_no_effects() {
    let (mut r, _directory, a, _) = setup();
    let first = pane();
    seed(&mut r, a, vec![first.clone()]);
    let remove = edit(&mut r, a, Change::Remove { id: first.id });
    assert!(!r.apply(&remove).unwrap().effects.is_empty());
    let replay = r.apply(&remove).unwrap();
    assert!(!replay.changed);
    assert!(replay.effects.is_empty());
}
