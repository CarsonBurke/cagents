//! Registry-only peer context tests. No context here authenticates a live host
//! process or authorizes reading a provider cache; those remain host duties.
use super::{PeerReadContext, peer_chat_read_context_in, placement};
use crate::{
    agent::Provider,
    agent_execution::{
        Authority, Destination, HostedSession, Origin, Spec, Submit, Surface, Target,
    },
    channel_work::Home,
    host::model::Registry,
    model::{
        AgentId, BootId, CardId, CardKind, ChannelId, HostIncarnation, HostSessionId,
        HostSessionProof, LaunchRecipe, TerminalId,
    },
    workspace_model::{
        ModelEdit,
        views::{Change, Pane, Scope, View},
    },
};
use rusqlite::{Connection, params};
use std::{fs, path::PathBuf};
use uuid::Uuid;

struct Fixture {
    registry: Registry,
    _directory: tempfile::TempDir,
    repository: PathBuf,
    home: Home,
    caller: HostedSession,
    target: crate::chat::Session,
    authority: Authority,
    caller_card: CardId,
    target_card: CardId,
    target_agent: Option<AgentId>,
}

fn source(repository: &std::path::Path) -> crate::chat::Session {
    crate::chat::Session {
        id: TerminalId::new(),
        launch: crate::chat::Launch {
            provider: Provider::Codex,
            recipe: LaunchRecipe {
                program: "fixture-codex".into(),
                arguments: Vec::new(),
                working_directory: repository.to_path_buf(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: Some(Uuid::new_v4().to_string()),
        },
        proof: Some(HostSessionProof {
            host_session_id: HostSessionId::new("fixture-host"),
            boot_id: BootId::new("fixture-boot"),
            host_incarnation: HostIncarnation::new("fixture-incarnation"),
        }),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    }
}

fn pane(card: CardId, session: crate::chat::Session) -> Pane {
    Pane {
        id: card,
        title: "Peer context fixture".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        kind: CardKind::Chat(session),
        scope: None,
    }
}

fn channel(registry: &mut Registry, repository: &std::path::Path) -> ChannelId {
    let id = ChannelId::new();
    registry
        .apply(&ModelEdit::CreateChannel {
            id,
            repository: repository.to_path_buf(),
            title: "Peer context fixture".into(),
            title_source: Default::default(),
        })
        .unwrap();
    id
}

fn managed_agent(
    registry: &mut Registry,
    repository: &std::path::Path,
    channel: ChannelId,
    source: HostedSession,
) -> AgentId {
    let accepted = registry
        .submit_agent(
            &Origin::User,
            &Submit {
                name: None,
                destination: Destination::New(Spec {
                    provider: Provider::Codex,
                    account: None,
                    target: Target {
                        repository: repository.to_path_buf(),
                        directory: repository.to_path_buf(),
                        placement: Some(Scope::Channel(channel)),
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                }),
                prompt: "Fixture acceptance only; no provider is started".into(),
                idempotency_key: Uuid::new_v4().to_string(),
            },
            1,
        )
        .unwrap();
    registry
        .bind_agent(accepted.agent.id, None, source)
        .unwrap();
    accepted.agent.id
}

fn fixture(managed_target: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    let repository = directory.path().join("project");
    fs::create_dir(&repository).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            kind: Default::default(),
            repository: repository.clone(),
            name: "Peer context fixture".into(),
        })
        .unwrap();
    let id = channel(&mut registry, &repository);
    let model = registry.model().unwrap();
    let home = Home {
        registry: model.identity,
        channel: id,
        creation_ordinal: model.channel(id).unwrap().ordinal,
    };
    let caller_chat = source(&repository);
    let caller = HostedSession::Chat(caller_chat.clone());
    let target = source(&repository);
    let authority = registry.register_agent_authority(&caller).unwrap();
    registry
        .register_agent_authority(&HostedSession::Chat(target.clone()))
        .unwrap();
    let target_agent = managed_target.then(|| {
        managed_agent(
            &mut registry,
            &repository,
            id,
            HostedSession::Chat(target.clone()),
        )
    });
    let caller_card = CardId::new();
    let target_card = target_agent.map_or_else(CardId::new, |id| CardId::from_uuid(id.as_uuid()));
    registry
        .apply(&ModelEdit::SeedView {
            scope: Scope::Channel(id),
            panes: vec![
                pane(caller_card, caller_chat),
                pane(target_card, target.clone()),
            ],
            closed: Vec::new(),
        })
        .unwrap();
    Fixture {
        registry,
        _directory: directory,
        repository,
        home,
        caller,
        target,
        authority,
        caller_card,
        target_card,
        target_agent,
    }
}

// Compare durable state on both successful and refused reads. Do not print
// issued capability contents even on an assertion failure.
fn durable_state(db: &Connection) -> Vec<Vec<String>> {
    [
        "SELECT json_array(sequence) FROM metadata ORDER BY key",
        "SELECT json_array(id,last_epoch) FROM channel_work_placement_clock ORDER BY id",
        "SELECT json_array(pane,epoch) FROM channel_work_placement_epochs ORDER BY pane",
        "SELECT json_array(token,session_id,source) FROM agent_authorities ORDER BY session_id",
        "SELECT json_array(session_id,lineage) FROM session_lineages ORDER BY session_id",
        "SELECT json_array(scope,revision,panes,retired) FROM shared_views ORDER BY scope",
        "SELECT json_array(id,record,session_id,closed) FROM agents ORDER BY id",
    ]
    .into_iter()
    .map(|sql| {
        db.prepare(sql)
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    })
    .collect()
}

fn read_preserving(
    registry: &Registry,
    authority: &Authority,
    caller: &HostedSession,
    target: &crate::chat::Session,
) -> Result<PeerReadContext, String> {
    assert!(registry.db.is_autocommit());
    let before = durable_state(&registry.db);
    let result = registry.peer_chat_read_context(authority, caller, target);
    assert!(
        registry.db.is_autocommit(),
        "the owned read must be released"
    );
    assert!(
        before == durable_state(&registry.db),
        "peer reads must not write"
    );
    result
}

fn context(fixture: &Fixture) -> PeerReadContext {
    read_preserving(
        &fixture.registry,
        &fixture.authority,
        &fixture.caller,
        &fixture.target,
    )
    .unwrap()
}

fn view(registry: &Registry, scope: &Scope) -> View {
    registry
        .model()
        .unwrap()
        .views
        .into_iter()
        .find(|view| &view.scope == scope)
        .unwrap()
}

fn edit(registry: &mut Registry, scope: Scope, change: Change) {
    registry
        .apply(&ModelEdit::ChangeView {
            expected_revision: registry.view_revision(&scope).unwrap(),
            scope,
            operation: Uuid::new_v4(),
            change,
        })
        .unwrap();
}

fn transfer(registry: &mut Registry, from: Scope, card: CardId, destination: Scope) {
    edit(
        registry,
        from,
        Change::Transfer {
            id: card,
            destination,
            before: None,
        },
    );
}

#[test]
fn proved_child_reads_managed_user_and_unmanaged_peer_in_one_home() {
    for managed in [false, true] {
        let f = fixture(managed);
        assert!(matches!(
            f.registry.execution_origin(&f.authority).unwrap(),
            Origin::Child { .. }
        ));
        if let Some(id) = f.target_agent {
            assert_eq!(f.registry.agent(id).unwrap().origin, Origin::User);
        }
        let observed = context(&f);
        assert_eq!(observed.home, f.home);
        assert_eq!(observed.caller_card, f.caller_card);
        assert_eq!(observed.target_card, f.target_card);
        assert_eq!(
            observed.caller_placement_epoch,
            placement::token(&f.registry.db, f.caller_card).unwrap()
        );
        assert_eq!(
            observed.target_placement_epoch,
            placement::token(&f.registry.db, f.target_card).unwrap()
        );
    }
}

#[test]
fn user_invalid_unissued_wrong_source_and_retired_capabilities_grant_no_peer_context() {
    for case in 0..6 {
        let mut f = fixture(false);
        context(&f);
        let authority = match case {
            0 => Authority::default(),
            1 => Authority::session("invalid".into()),
            2 => Authority::session(Uuid::new_v4().to_string()),
            3 => f
                .registry
                .register_agent_authority(&HostedSession::Chat(f.target.clone()))
                .unwrap(),
            4 => {
                f.registry.retire_agent_authority(&f.caller).unwrap();
                f.authority.clone()
            }
            5 => {
                f.registry
                    .retire_agent_authority(&HostedSession::Chat(f.target.clone()))
                    .unwrap();
                f.authority.clone()
            }
            _ => unreachable!(),
        };
        assert!(read_preserving(&f.registry, &authority, &f.caller, &f.target).is_err());
    }
}

#[test]
fn peer_context_requires_exact_complete_caller_and_target_sources() {
    for change_caller in [false, true] {
        for damage in 0..3 {
            let f = fixture(false);
            context(&f);
            let mut caller = f.caller.clone();
            let mut target = f.target.clone();
            let changed = if change_caller {
                let HostedSession::Chat(session) = &mut caller else {
                    unreachable!()
                };
                session
            } else {
                &mut target
            };
            match damage {
                0 => changed.proof = None,
                1 => {
                    changed.proof.as_mut().unwrap().host_incarnation =
                        HostIncarnation::new("different-incarnation")
                }
                2 => changed
                    .launch
                    .recipe
                    .arguments
                    .push("different-source".into()),
                _ => unreachable!(),
            }
            assert!(read_preserving(&f.registry, &f.authority, &caller, &target).is_err());
            context(&f);
        }
    }
}

#[test]
fn same_project_does_not_waive_channel_or_worktree_boundaries() {
    for move_caller in [false, true] {
        for worktree in [false, true] {
            let mut f = fixture(false);
            context(&f);
            let destination = if worktree {
                Scope::Worktree {
                    repository: f.repository.clone(),
                    path: f.repository.clone(),
                }
            } else {
                Scope::Channel(channel(&mut f.registry, &f.repository))
            };
            transfer(
                &mut f.registry,
                Scope::Channel(f.home.channel),
                if move_caller {
                    f.caller_card
                } else {
                    f.target_card
                },
                destination,
            );
            assert!(read_preserving(&f.registry, &f.authority, &f.caller, &f.target).is_err());
        }
    }
}

#[test]
fn deleting_and_recreating_channel_does_not_reuse_old_home() {
    let mut f = fixture(false);
    let before = context(&f);
    let panes = view(&f.registry, &Scope::Channel(f.home.channel)).panes;
    f.registry
        .apply(&ModelEdit::DeleteChannel { id: f.home.channel })
        .unwrap();
    assert!(read_preserving(&f.registry, &f.authority, &f.caller, &f.target).is_err());
    f.registry
        .apply(&ModelEdit::CreateChannel {
            id: f.home.channel,
            repository: f.repository.clone(),
            title: "A new channel with the old UUID".into(),
            title_source: Default::default(),
        })
        .unwrap();
    assert!(read_preserving(&f.registry, &f.authority, &f.caller, &f.target).is_err());
    // Explicit publication can establish a new Home, never revive the captured
    // Home or either captured placement token merely by reusing the UUID.
    f.registry
        .apply(&ModelEdit::SeedView {
            scope: Scope::Channel(f.home.channel),
            panes,
            closed: Vec::new(),
        })
        .unwrap();
    let after = context(&f);
    assert_eq!(after.home.channel, before.home.channel);
    assert_ne!(after.home.creation_ordinal, before.home.creation_ordinal);
    assert_ne!(after.home, before.home);
    assert_ne!(after.caller_placement_epoch, before.caller_placement_epoch);
    assert_ne!(after.target_placement_epoch, before.target_placement_epoch);
}

#[test]
fn ambiguous_retired_pending_missing_and_unproved_canonical_panes_refuse() {
    for damage_caller in [false, true] {
        for damage in 0..6 {
            let f = fixture(false);
            context(&f);
            let scope = Scope::Channel(f.home.channel);
            let mut current = view(&f.registry, &scope);
            let card = if damage_caller {
                f.caller_card
            } else {
                f.target_card
            };
            let index = current
                .panes
                .iter()
                .position(|pane| pane.id == card)
                .unwrap();
            // Construct malformed retained state only AFTER a genuine public
            // SeedView and issued capabilities have proved the healthy case.
            match damage {
                0 => {
                    let mut duplicate = current.panes[index].clone();
                    duplicate.id = CardId::new();
                    current.panes.push(duplicate);
                }
                1 => current.retired.push(card),
                2 => current.panes[index].pending_terminal_start = true,
                3 => {
                    current.panes.remove(index);
                }
                4 => {
                    let CardKind::Chat(session) = &mut current.panes[index].kind else {
                        unreachable!()
                    };
                    session.proof = None;
                }
                5 => {
                    let mut duplicate = current.panes[index].clone();
                    duplicate.kind = CardKind::Chat(source(&f.repository));
                    current.panes.push(duplicate);
                }
                _ => unreachable!(),
            }
            f.registry
                .db
                .execute(
                    "UPDATE shared_views SET panes=?2,retired=?3 WHERE scope=?1",
                    params![
                        serde_json::to_string(&scope).unwrap(),
                        serde_json::to_string(&current.panes).unwrap(),
                        serde_json::to_string(&current.retired).unwrap(),
                    ],
                )
                .unwrap();
            assert!(read_preserving(&f.registry, &f.authority, &f.caller, &f.target).is_err());
        }
    }
}

#[test]
fn conflicting_managed_record_cannot_authorize_an_unrelated_process() {
    for closed in [false, true] {
        let f = fixture(true);
        context(&f);
        let id = f.target_agent.unwrap();
        let mut agent = f.registry.agent(id).unwrap();
        if closed {
            agent.closed = true;
        } else {
            let HostedSession::Chat(session) = agent.session.as_mut().unwrap() else {
                unreachable!()
            };
            session.launch.recipe.arguments.push("other-process".into());
        }
        // Leave the actual indexed session and canonical pane untouched; the
        // managed record disagreement must still refuse the peer observation.
        f.registry
            .db
            .execute(
                "UPDATE agents SET record=?2 WHERE id=?1",
                params![id.to_string(), serde_json::to_string(&agent).unwrap()],
            )
            .unwrap();
        assert!(read_preserving(&f.registry, &f.authority, &f.caller, &f.target).is_err());
    }
    let mut f = fixture(false);
    context(&f);
    let managed = managed_agent(
        &mut f.registry,
        &f.repository,
        f.home.channel,
        f.caller.clone(),
    );
    // A source that was legitimately displayed before management must not get
    // peer authority from an aliased noncanonical card after management.
    assert!(read_preserving(&f.registry, &f.authority, &f.caller, &f.target).is_err());
    let scope = Scope::Channel(f.home.channel);
    let mut canonical = view(&f.registry, &scope)
        .panes
        .into_iter()
        .find(|pane| pane.id == f.caller_card)
        .unwrap();
    edit(
        &mut f.registry,
        scope.clone(),
        Change::Remove { id: f.caller_card },
    );
    canonical.id = CardId::from_uuid(managed.as_uuid());
    f.caller_card = canonical.id;
    edit(
        &mut f.registry,
        scope,
        Change::Insert {
            pane: canonical,
            before: None,
        },
    );
    assert_eq!(context(&f).caller_card, f.caller_card);
}

#[test]
fn presentation_is_stable_but_both_actor_round_trips_change_context() {
    for move_caller in [false, true] {
        let mut f = fixture(false);
        let original = context(&f);
        let scope = Scope::Channel(f.home.channel);
        let before = view(&f.registry, &scope).panes[0].clone();
        let mut after = before.clone();
        after.title = "A presentation edit".into();
        after.user_title = Some("An explicit title".into());
        after.width_fraction = 0.75;
        edit(
            &mut f.registry,
            scope.clone(),
            Change::Update {
                before,
                pane: after,
            },
        );
        edit(
            &mut f.registry,
            scope.clone(),
            Change::Move {
                id: f.target_card,
                before: Some(f.caller_card),
            },
        );
        assert_eq!(context(&f), original);
        let other = Scope::Channel(channel(&mut f.registry, &f.repository));
        let card = if move_caller {
            f.caller_card
        } else {
            f.target_card
        };
        transfer(&mut f.registry, scope.clone(), card, other.clone());
        assert!(read_preserving(&f.registry, &f.authority, &f.caller, &f.target).is_err());
        transfer(&mut f.registry, other, card, scope);
        let returned = context(&f);
        assert_eq!(returned.home, original.home);
        assert_eq!(returned.caller_card, original.caller_card);
        assert_eq!(returned.target_card, original.target_card);
        if move_caller {
            assert_ne!(
                returned.caller_placement_epoch,
                original.caller_placement_epoch
            );
            assert_eq!(
                returned.target_placement_epoch,
                original.target_placement_epoch
            );
        } else {
            assert_ne!(
                returned.target_placement_epoch,
                original.target_placement_epoch
            );
            assert_eq!(
                returned.caller_placement_epoch,
                original.caller_placement_epoch
            );
        }
        assert_ne!(returned, original);
    }
}

#[test]
fn wal_snapshot_does_not_mix_peer_home_and_placement_generations() {
    let mut f = fixture(false);
    let original = context(&f);
    let home_scope = Scope::Channel(f.home.channel);
    let other_scope = Scope::Channel(channel(&mut f.registry, &f.repository));
    let mut writer = f.registry.execution_connection().unwrap();
    assert_eq!(
        f.registry
            .db
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
    f.registry
        .db
        .pragma_update(None, "query_only", true)
        .unwrap();
    let tx = f.registry.db.unchecked_transaction().unwrap();
    let captured = peer_chat_read_context_in(&tx, &f.authority, &f.caller, &f.target).unwrap();
    assert_eq!(captured, original);
    transfer(
        &mut writer,
        home_scope.clone(),
        f.target_card,
        other_scope.clone(),
    );
    transfer(&mut writer, other_scope, f.target_card, home_scope);
    // The real writer committed twice while this actual read transaction stayed
    // open. Its original snapshot must not mix in the new placement clock.
    assert_eq!(
        peer_chat_read_context_in(&tx, &f.authority, &f.caller, &f.target).unwrap(),
        captured
    );
    assert!(!tx.is_autocommit());
    assert_eq!(
        tx.query_row("PRAGMA query_only", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    tx.commit().unwrap();
    assert!(f.registry.db.is_autocommit());
    f.registry
        .db
        .pragma_update(None, "query_only", false)
        .unwrap();
    let fresh = context(&f);
    assert_eq!(fresh.home, captured.home);
    assert_eq!(fresh.caller_card, captured.caller_card);
    assert_eq!(fresh.target_card, captured.target_card);
    assert_eq!(
        fresh.caller_placement_epoch,
        captured.caller_placement_epoch
    );
    assert_ne!(
        fresh.target_placement_epoch,
        captured.target_placement_epoch
    );
    assert_ne!(fresh, captured);
}

#[test]
fn public_context_refuses_existing_transaction_without_ending_its_owner() {
    let f = fixture(false);
    let original = context(&f);
    let before = durable_state(&f.registry.db);
    let tx = f.registry.db.unchecked_transaction().unwrap();
    assert_eq!(
        peer_chat_read_context_in(&tx, &f.authority, &f.caller, &f.target).unwrap(),
        original
    );
    assert!(
        f.registry
            .peer_chat_read_context(&f.authority, &f.caller, &f.target)
            .is_err()
    );
    assert!(!tx.is_autocommit(), "the caller still owns its transaction");
    assert_eq!(
        peer_chat_read_context_in(&tx, &f.authority, &f.caller, &f.target).unwrap(),
        original
    );
    tx.rollback().unwrap();
    assert!(f.registry.db.is_autocommit());
    assert!(before == durable_state(&f.registry.db));
    assert_eq!(context(&f), original);
}
