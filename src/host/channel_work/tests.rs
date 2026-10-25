use super::*;
use crate::{
    channel_work::{Change, Mutation, Role},
    github_identity::{GitHubRepoKey, PullRequestKey},
    host::{
        model::Registry,
        state::{HostLock, HostState},
    },
    model::{ChannelId, HostSessionProof},
    terminal_ipc::{ClientRequest, ServerResponse},
    workspace_model::ModelEdit,
};
use std::sync::Arc;

pub(super) fn fixture() -> (SharedHost, tempfile::TempDir, Home) {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    let repository = directory.path().join("project");
    std::fs::create_dir(&repository).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: repository.clone(),
            name: "Project".into(),
            kind: Default::default(),
        })
        .unwrap();
    let channel = ChannelId::new();
    registry
        .apply(&ModelEdit::CreateChannel {
            id: channel,
            repository,
            title: "Channel".into(),
            title_source: Default::default(),
        })
        .unwrap();
    let model = registry.model().unwrap();
    let home = Home {
        registry: model.identity,
        channel,
        creation_ordinal: model.channel(channel).unwrap().ordinal,
    };
    let mut state = HostState::new(HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("channel-work-fixture"),
        boot_id: crate::model::BootId::new("fixture"),
        host_incarnation: crate::model::HostIncarnation::new("fixture"),
    });
    state.install_model(registry);
    (Arc::new(HostLock::new(state)), directory, home)
}

fn mutation(home: Home) -> Mutation {
    Mutation {
        home,
        expected_revision: 0,
        operation: uuid::Uuid::new_v4(),
        change: Change::Claim {
            target: Target::PullRequest(
                PullRequestKey::new(
                    GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
                    1,
                )
                .unwrap(),
            ),
            role: Role::Review,
        },
    }
}

pub(super) fn wire(host: &SharedHost, authority: Authority, request: Request) -> Response {
    let (response, _, _) = crate::host::client::process_request(
        host,
        1,
        ClientRequest::ChannelWork { authority, request },
    )
    .unwrap();
    let ServerResponse::ChannelWork(response) = response else {
        panic!("channel work response")
    };
    response
}

#[test]
fn public_handler_refuses_unknown_authority_instead_of_falling_back_to_user_and_requires_explicit_user_channel()
 {
    let (host, _directory, home) = fixture();
    assert!(matches!(
        wire(
            &host,
            Authority::default(),
            Request::Context { channel: None }
        ),
        Response::Failed(Failure::Refused(_))
    ));
    let invalid = Authority::session(uuid::Uuid::new_v4().to_string());
    for request in [
        Request::Context {
            channel: Some(home.channel),
        },
        Request::Read {
            home,
            page: Page::default(),
        },
        Request::ReadHome { home },
        Request::ReadTarget {
            home,
            target: mutation(home).change.target().clone(),
        },
        Request::Mutate {
            mutation: mutation(home),
            caller: None,
        },
        Request::PrepareLocal {
            home,
            directory: "/must-not-be-opened".into(),
            caller: None,
        },
        Request::ApplySelection {
            selection: uuid::Uuid::new_v4(),
            mutation: Mutation {
                change: Change::Claim {
                    target: Target::Local(uuid::Uuid::new_v4()),
                    role: Role::Contribute,
                },
                ..mutation(home)
            },
            caller: None,
        },
    ] {
        assert!(matches!(
            wire(&host, invalid.clone(), request),
            Response::Failed(Failure::Refused(_))
        ));
    }
    let Response::Snapshot(before) = wire(
        &host,
        Authority::default(),
        Request::Read {
            home,
            page: Page::default(),
        },
    ) else {
        panic!("read")
    };
    assert!(before.entries.is_empty() && before.history.is_empty());
    let Response::Context(context) = wire(
        &host,
        Authority::default(),
        Request::Context {
            channel: Some(home.channel),
        },
    ) else {
        panic!("explicit channel")
    };
    assert_eq!(context.home, home);
    assert!(context.caller.is_none());
    let Response::Applied(applied) = wire(
        &host,
        Authority::default(),
        Request::Mutate {
            mutation: mutation(home),
            caller: None,
        },
    ) else {
        panic!("explicit user mutation")
    };
    assert_eq!(applied.snapshot.entries.len(), 1);
    assert!(
        !applied.snapshot.entries[0].is_ongoing_work(),
        "review-only is not implementation ownership"
    );
}

#[test]
fn public_point_and_home_reads_keep_exact_scope_and_recover_retired_home_without_projection() {
    let (host, _directory, home) = fixture();
    let original = mutation(home);
    let target = original.change.target().clone();
    let Response::Applied(applied) = wire(
        &host,
        Authority::default(),
        Request::Mutate {
            mutation: original.clone(),
            caller: None,
        },
    ) else {
        panic!("track work")
    };
    let Response::Target(point) = wire(
        &host,
        Authority::default(),
        Request::ReadTarget {
            home,
            target: target.clone(),
        },
    ) else {
        panic!("exact target read")
    };
    point.validate().unwrap();
    assert_eq!(point.home, home);
    assert_eq!(point.target, target);
    assert_eq!(point.entry, applied.snapshot.entries.first().cloned());
    let foreign = Home {
        registry: uuid::Uuid::new_v4(),
        ..home
    };
    for request in [
        Request::ReadHome { home: foreign },
        Request::ReadTarget {
            home: foreign,
            target,
        },
    ] {
        assert!(matches!(
            wire(&host, Authority::default(), request),
            Response::Failed(Failure::Refused(_))
        ));
    }
    let registry = agent_execution::registry(&host).unwrap();
    registry
        .lock()
        .unwrap()
        .apply(&ModelEdit::DeleteChannel { id: home.channel })
        .unwrap();
    assert!(matches!(
        wire(&host, Authority::default(), Request::ReadHome { home }),
        Response::Home {
            home: exact,
            lifecycle: work::HomeLifecycle::Deleted,
            revision
        } if exact == home && revision > applied.snapshot.revision
    ));
    assert_eq!(
        wire(
            &host,
            Authority::default(),
            Request::Receipt {
                home,
                operation: original.operation,
            },
        ),
        Response::Receipt(Some(applied.receipt))
    );
}

#[test]
fn authenticated_terminal_connection_cannot_supply_user_authority_to_channel_work() {
    let (host, _directory, home) = fixture();
    host.lock().unwrap().agent_clients.insert(
        1,
        crate::agent_execution::Lineage(crate::model::TerminalId::new()),
    );
    assert!(
        crate::host::client::process_request(
            &host,
            1,
            ClientRequest::ChannelWork {
                authority: Authority::default(),
                request: Request::Mutate {
                    mutation: mutation(home),
                    caller: None
                },
            }
        )
        .is_err()
    );
    let registry = agent_execution::registry(&host).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .read_channel_work(&home, Page::default())
            .unwrap()
            .entries
            .is_empty()
    );
}

#[test]
fn a_registered_but_not_live_native_source_cannot_declare_work() {
    let (host, _directory, home) = fixture();
    let source = crate::agent_execution::HostedSession::Chat(crate::chat::Session {
        id: crate::model::TerminalId::new(),
        proof: Some(host.lock().unwrap().proof().clone()),
        terminal: None,
        native_continuity: Default::default(),
        owned_codex_listener: None,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            session_id: None,
            recipe: crate::model::LaunchRecipe {
                program: "fixture-never-executed".into(),
                arguments: vec![],
                working_directory: "/fixture".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
        },
    });
    let registry = agent_execution::registry(&host).unwrap();
    let card = crate::model::CardId::new();
    let resolved = {
        let mut registry = registry.lock().unwrap();
        let authority = registry.register_agent_authority(&source).unwrap();
        let crate::agent_execution::HostedSession::Chat(session) = &source else {
            unreachable!()
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope: crate::workspace_model::views::Scope::Channel(home.channel),
                panes: vec![crate::workspace_model::views::Pane {
                    id: card,
                    title: "Native fixture".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 1.0,
                    kind: crate::model::CardKind::Chat(session.clone()),
                    scope: None,
                }],
                closed: vec![],
            })
            .unwrap();
        let resolved = registry
            .resolve_channel_work_source(&authority, &source)
            .unwrap();
        (authority, resolved)
    };
    let (authority, resolved) = resolved;
    let captured = caller(&resolved);
    assert!(
        captured.is_some(),
        "the canonical source proof is complete; only live Host custody is absent"
    );
    assert!(matches!(
        wire(
            &host,
            authority,
            Request::Mutate {
                mutation: mutation(home),
                caller: captured
            }
        ),
        Response::Failed(Failure::Refused(_))
    ));
    assert!(
        registry
            .lock()
            .unwrap()
            .read_channel_work(&home, Page::default())
            .unwrap()
            .entries
            .is_empty()
    );
}

#[test]
fn valid_unicode_pr_status_survives_unusable_optional_presentation_metadata() {
    let repository = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
    let raw = serde_json::to_vec(&serde_json::json!({
        "number":1,"title":"界".repeat(200),"state":"MERGED",
        "headRefName":"followup","headRefOid":"not-an-object-id",
        "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"ForkOwner"},
        "baseRefName":"main","updatedAt":"2026-10-23T00:00:00Z",
        "url":"https://github.com/Owner/Repo/pull/1","id":"PR_known",
    }))
    .unwrap();
    let parsed = crate::github::parse_pull_request(&raw, &repository).unwrap();
    let normalized = observation(1, work::PrState::Merged, parsed);
    assert_eq!(normalized.state, work::PrState::Merged);
    assert_eq!(normalized.freshness, work::Freshness::Current);
    assert!(normalized.title.is_none());
    assert!(normalized.head.is_none());
    assert_eq!(normalized.node_id.as_deref(), Some("PR_known"));
    // Prove that the normalized observation reaches the real canonical write,
    // rather than silently stalling at pure metadata validation.
    let (host, _directory, home) = fixture();
    let registry = agent_execution::registry(&host).unwrap();
    let mut registry = registry.lock().unwrap();
    let request = mutation(home);
    let origin = TrustedResolvedOrigin {
        origin: Origin::User,
        proof: None,
    };
    registry.apply_channel_work(&origin, &request).unwrap();
    let observed = registry
        .observe_channel_work_authenticated(
            &origin,
            &home,
            request.change.target(),
            1,
            1,
            work::PollOutcome::Success(normalized),
        )
        .unwrap();
    assert!(observed.changed);
    assert!(observed.snapshot.entries.is_empty());
    assert_eq!(
        observed.snapshot.history[0]
            .observation
            .as_ref()
            .unwrap()
            .state,
        work::PrState::Merged
    );
}
