use super::*;

use crate::terminal::ipc::{
    ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
    ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
};

fn receipt_response(model: WorkspaceModel) -> ServerResponse {
    ServerResponse::WorkspaceEdit(EditReceipt {
        identity: model.identity,
        sequence: model.sequence,
        effects: EditEffects {
            channels: model.channels,
            projects: model.projects,
            agents: model.agents,
            navigation_orders: model.navigation_orders,
            ..Default::default()
        },
    })
}

fn publisher_reply(
    peer: &mut std::os::unix::net::UnixStream,
    proof: &crate::model::HostSessionProof,
    request_id: u64,
    message: ServerMessage,
) {
    write_frame(
        peer,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request_id),
            proof: proof.clone(),
            message,
        },
    )
    .unwrap();
}

fn publisher_connection(
    serve: impl FnOnce(&mut std::os::unix::net::UnixStream, &crate::model::HostSessionProof)
    + Send
    + 'static,
) -> (ControlClient, std::thread::JoinHandle<()>) {
    let (stream, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let server = std::thread::spawn(move || {
        let proof = crate::model::HostSessionProof {
            host_session_id: "publisher-refusal".into(),
            boot_id: "boot".into(),
            host_incarnation: "owner".into(),
        };
        let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        publisher_reply(
            &mut peer,
            &proof,
            hello.request_id,
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        serve(&mut peer, &proof);
    });
    (ControlClient::from_stream(stream).unwrap(), server)
}

#[test]
fn refused_seed_is_reported_to_the_window() {
    let seed = ModelRequest::from(ModelEdit::SeedView {
        scope: crate::workspace_model::views::Scope::Channel(ChannelId::new()),
        panes: vec![],
        closed: vec![],
    });
    let attempt = seed.seed_attempt.unwrap();
    let expected = seed.edit.clone();
    let (mut client, server) = publisher_connection(move |peer, proof| {
        let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
        assert_eq!(
            request.request,
            ClientRequest::ApplyWorkspaceEdit { edit: expected }
        );
        publisher_reply(
            peer,
            proof,
            request.request_id,
            ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "Fixture registry refused the saved panes",
                false,
            )),
        );
    });
    let (requests, receiver) = async_channel::unbounded();
    requests.try_send(seed).unwrap();
    let mut pending = None;
    drop(requests);
    let (updates, received) = async_channel::unbounded();
    let _ = publish_with_client(&updates, &receiver, &mut pending, &mut client);
    drop(client);
    server.join().unwrap();
    assert!(
        matches!(received.try_recv().unwrap(), ModelUpdate::SeedRefused { attempt: actual, error, .. }
        if actual == attempt && error == "Fixture registry refused the saved panes")
    );
    assert!(
        pending.is_none(),
        "a definitive refusal must not be replayed"
    );
}

#[test]
fn successful_seed_publishes_its_attempt_with_the_scoped_delta_receipt() {
    use crate::workspace_model::{
        delta::ViewRevision,
        views::{Scope, View},
    };
    let scope = Scope::Channel(ChannelId::new());
    let seed = ModelRequest::from(ModelEdit::SeedView {
        scope: scope.clone(),
        panes: vec![],
        closed: vec![],
    });
    let attempt = seed.seed_attempt.unwrap();
    let edit = seed.edit.clone();
    let expected_edit = edit.clone();
    let mut view = View::empty(scope.clone());
    view.revision = 1;
    let expected_view = view.clone();
    let identity = uuid::Uuid::new_v4();
    let receipt = EditReceipt {
        identity,
        sequence: 2,
        effects: EditEffects {
            views: vec![ViewRevision {
                scope: scope.clone(),
                before: None,
                after: Some(1),
            }],
            ..Default::default()
        },
    };
    let expected_receipt = receipt.clone();
    let (mut client, server) = publisher_connection(move |peer, proof| {
        let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
        assert_eq!(
            request.request,
            ClientRequest::ApplyWorkspaceEdit {
                edit: expected_edit
            }
        );
        publisher_reply(
            peer,
            proof,
            request.request_id,
            ServerMessage::Response {
                response: ServerResponse::WorkspaceEdit(receipt),
            },
        );
        let read: ClientEnvelope = read_frame(peer).unwrap().unwrap();
        assert_eq!(read.request, ClientRequest::ReadWorkspaceView { scope });
        publisher_reply(
            peer,
            proof,
            read.request_id,
            ServerMessage::Response {
                response: ServerResponse::WorkspaceView {
                    view: Some(view),
                    sequence: 2,
                },
            },
        );
    });
    let (queued, requests) = async_channel::unbounded();
    queued.try_send(seed).unwrap();
    drop(queued);
    let (updates, received) = async_channel::unbounded();
    let mut pending = None;
    assert_eq!(
        publish_with_client(&updates, &requests, &mut pending, &mut client).unwrap_err(),
        "the window stopped publishing"
    );
    let ModelUpdate::SeedCommitted {
        attempt: actual,
        receipt,
        edit: actual_edit,
        views,
    } = received.try_recv().unwrap()
    else {
        panic!("a seed must retain its local attempt on the delta receipt")
    };
    assert_eq!(actual, attempt);
    assert_eq!(receipt, expected_receipt);
    assert_eq!(actual_edit, edit);
    assert_eq!(views.views, vec![expected_view]);
    assert!(received.try_recv().is_err());
    assert!(pending.is_none());
    drop(client);
    server.join().unwrap();
}

#[test]
fn seed_delta_receipts_fence_stale_attempts_and_only_current_receipts_admit() {
    use crate::workspace_model::{
        delta::ViewRevision,
        views::{Scope, View},
    };
    let (mut app, project, repository) = app_with_project();
    let channel = ChannelId::new();
    let scope = Scope::Channel(channel);
    let card = app.durable.cards[0].id;
    let mut saved_channel = local(channel, project, "Release planning", 1);
    saved_channel.cards.push(card);
    saved_channel.focused = Some(card);
    Arc::make_mut(&mut app.durable).channels.push(saved_channel);
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    app.workspace_model = registry(vec![shared(channel, &repository, "Release planning", 1)]);
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(app.workspace_model.identity);
    let (queued, requests) = async_channel::unbounded();
    app.workspace_model_tx = queued;
    app.publish_view_changes();
    let first = std::iter::from_fn(|| requests.try_recv().ok())
        .find(|request| matches!(&request.edit, ModelEdit::SeedView { scope: owner, .. } if owner == &scope))
        .unwrap();
    let stale = first.seed_attempt.unwrap();
    drop(app.apply_workspace_model_update(ModelUpdate::SeedRefused {
        scope: scope.clone(),
        attempt: stale,
        error: "The panes were refused".into(),
    }));
    app.retry_view_seeds();
    let current = app.shared_views.seed_attempts[&scope];
    assert_ne!(current, stale);
    assert!(app.has_refused_view_seeds());
    assert!(!app.can_retry_view_seeds());
    let mut view = View::empty(scope.clone());
    view.revision = 2;
    let ModelEdit::SeedView { panes, .. } = &first.edit else {
        unreachable!()
    };
    view.panes = panes.clone();
    let receipt = EditReceipt {
        identity: app.workspace_model.identity,
        sequence: 2,
        effects: EditEffects {
            views: vec![ViewRevision {
                scope: scope.clone(),
                before: None,
                after: Some(2),
            }],
            ..Default::default()
        },
    };
    let edit = first.edit;
    let views = ViewChanges {
        sequence: 2,
        views: vec![view.clone()],
        ..Default::default()
    };
    let before_model = app.workspace_model.clone();
    let before_attempts = app.shared_views.seed_attempts.clone();
    let before_failures = app.shared_views.seed_failures.clone();
    let (_, projection) = app.apply_workspace_model_update_projection(ModelUpdate::SeedCommitted {
        attempt: stale,
        receipt: receipt.clone(),
        edit: edit.clone(),
        views: views.clone(),
    });
    assert!(matches!(projection, Projection::Unchanged));
    assert_eq!(
        app.workspace_model, before_model,
        "a stale seed cannot install canonical rows"
    );
    assert_eq!(app.shared_views.seed_attempts, before_attempts);
    assert_eq!(app.shared_views.seed_failures, before_failures);
    // The observer can announce the same committed seed first, but only
    // the matching publisher receipt settles this window's held offer.
    drop(app.apply_workspace_model_update(ModelUpdate::Changed {
        sequence: receipt.sequence,
        edit: edit.clone(),
        changes: EdgeChanges {
            identity: Some(receipt.identity),
            effects: receipt.effects.clone(),
            views: views.clone(),
        },
    }));
    assert_eq!(app.shared_views.seed_attempts[&scope], current);
    assert_eq!(app.shared_views.seed_failures, before_failures);
    assert!(!app.can_retry_view_seeds());
    drop(
        app.apply_workspace_model_update(ModelUpdate::SeedCommitted {
            attempt: current,
            receipt,
            edit,
            views,
        }),
    );
    assert!(!app.shared_views.seed_attempts.contains_key(&scope));
    assert!(!app.has_refused_view_seeds());
    assert!(app.workspace_model.views.contains(&view));
}

#[test]
fn invalid_receipt_reconnects_before_sending_later_edits() {
    let id = ChannelId::new();
    let title = ModelEdit::SuggestConversationTitle {
        id,
        card: CardId::new(),
        expected_revision: 1,
        title: "First prompt".into(),
        source: crate::model::ChannelTitleSource::Prompt,
    };
    let next = ModelEdit::RetitleChannel {
        id,
        title: "Manual name".into(),
    };
    let (mut client, server) = publisher_connection(move |peer, proof| {
        let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
        assert!(matches!(
            request.request,
            ClientRequest::ApplyWorkspaceEdit {
                edit: ModelEdit::SuggestConversationTitle { .. }
            }
        ));
        publisher_reply(
            peer,
            proof,
            request.request_id,
            ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "Title basis changed",
                false,
            )),
        );
        let read: ClientEnvelope = read_frame(peer).unwrap().unwrap();
        assert!(matches!(
            read.request,
            ClientRequest::ApplyWorkspaceEdit {
                edit: ModelEdit::RetitleChannel { .. }
            }
        ));
        publisher_reply(
            peer,
            proof,
            read.request_id + 1,
            ServerMessage::Response {
                response: receipt_response(WorkspaceModel::default()),
            },
        );
        let later: Option<ClientEnvelope> = read_frame(peer).unwrap();
        assert!(
            later.is_none(),
            "a desynchronized connection must not carry another edit"
        );
    });
    let (updates, _received) = async_channel::unbounded();
    let (queued, requests) = async_channel::unbounded();
    queued.send_blocking(title.into()).unwrap();
    queued.send_blocking(next.clone().into()).unwrap();
    drop(queued);
    let mut pending = None;
    let error = publish_with_client(&updates, &requests, &mut pending, &mut client).unwrap_err();
    drop(client);
    server.join().unwrap();
    assert!(error.contains("does not match"), "{error}");
    assert!(
        pending
            .as_ref()
            .is_some_and(|(request, _)| request.edit == next),
        "only the edit with an unknown receipt may be retried"
    );
    assert!(requests.try_recv().is_err());
}

#[test]
fn transport_error_after_refusal_retries_only_the_pending_edit_on_reconnect() {
    let id = ChannelId::new();
    let title = ModelEdit::SuggestChannelTitle {
        id,
        title: "First prompt".into(),
        source: crate::model::ChannelTitleSource::Prompt,
    };
    let next = ModelEdit::RetitleChannel {
        id,
        title: "Manual name".into(),
    };
    let expected = next.clone();
    let (mut client, server) = publisher_connection(move |peer, proof| {
        let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
        assert_eq!(
            request.request,
            ClientRequest::ApplyWorkspaceEdit { edit: title }
        );
        publisher_reply(
            peer,
            proof,
            request.request_id,
            ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "Title basis changed",
                false,
            )),
        );
        let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
        assert_eq!(
            request.request,
            ClientRequest::ApplyWorkspaceEdit { edit: expected }
        );
        peer.shutdown(std::net::Shutdown::Both).unwrap();
    });
    let (updates, received) = async_channel::unbounded();
    let (queued, requests) = async_channel::unbounded();
    queued
        .send_blocking(
            ModelEdit::SuggestChannelTitle {
                id,
                title: "First prompt".into(),
                source: crate::model::ChannelTitleSource::Prompt,
            }
            .into(),
        )
        .unwrap();
    queued.send_blocking(next.clone().into()).unwrap();
    drop(queued);
    let mut pending = None;
    let error = publish_with_client(&updates, &requests, &mut pending, &mut client).unwrap_err();
    drop(client);
    server.join().unwrap();
    assert_eq!(error, ControlError::ConnectionClosed.to_string());
    assert_eq!(
        pending.as_ref().map(|(request, _)| &request.edit),
        Some(&next)
    );
    assert!(received.is_empty());

    let expected = next.clone();
    let (mut client, server) = publisher_connection(move |peer, proof| {
        let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
        assert_eq!(
            request.request,
            ClientRequest::ApplyWorkspaceEdit { edit: expected }
        );
        publisher_reply(
            peer,
            proof,
            request.request_id,
            ServerMessage::Response {
                response: receipt_response(WorkspaceModel::default()),
            },
        );
    });
    assert_eq!(
        publish_with_client(&updates, &requests, &mut pending, &mut client).unwrap_err(),
        "the window stopped publishing",
    );
    drop(client);
    server.join().unwrap();
    assert!(pending.is_none());
    assert!(matches!(
        received.try_recv().unwrap(),
        ModelUpdate::Committed { .. }
    ));
    assert!(received.is_empty());
}

#[test]
fn refused_titles_do_not_disconnect_or_block_later_model_edits() {
    use crate::terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    };
    let id = ChannelId::new();
    let titles = [
        ModelEdit::SuggestConversationTitle {
            id,
            card: CardId::new(),
            expected_revision: 1,
            title: "First prompt".into(),
            source: crate::model::ChannelTitleSource::Prompt,
        },
        ModelEdit::SuggestChannelTitle {
            id,
            title: "First prompt".into(),
            source: crate::model::ChannelTitleSource::Prompt,
        },
    ];
    let next = ModelEdit::RetitleChannel {
        id,
        title: "Manual name".into(),
    };
    let expected_next = next.clone();
    let expected_titles = titles.clone();
    let (stream, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let server = std::thread::spawn(move || {
        let proof = crate::model::HostSessionProof {
            host_session_id: "refused-titles".into(),
            boot_id: "boot".into(),
            host_incarnation: "owner".into(),
        };
        let respond = |peer: &mut std::os::unix::net::UnixStream, request_id, message| {
            write_frame(
                peer,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request_id),
                    proof: proof.clone(),
                    message,
                },
            )
            .unwrap();
        };
        let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        respond(
            &mut peer,
            hello.request_id,
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        for title in expected_titles {
            let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(
                request.request,
                ClientRequest::ApplyWorkspaceEdit {
                    edit: title.clone()
                }
            );
            respond(
                &mut peer,
                request.request_id,
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "Title basis changed",
                    false,
                )),
            );
        }
        let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert_eq!(
            request.request,
            ClientRequest::ApplyWorkspaceEdit {
                edit: expected_next
            }
        );
        respond(
            &mut peer,
            request.request_id,
            ServerMessage::Response {
                response: receipt_response(WorkspaceModel::default()),
            },
        );
    });
    let mut client = ControlClient::from_stream(stream).unwrap();
    let (updates, received) = async_channel::unbounded();
    let (queued, requests) = async_channel::unbounded();
    for title in titles {
        queued.send_blocking(title.into()).unwrap();
    }
    queued.send_blocking(next.into()).unwrap();
    drop(queued);
    let mut pending = None;
    let error = publish_with_client(&updates, &requests, &mut pending, &mut client).unwrap_err();
    assert_eq!(
        error, "the window stopped publishing",
        "only draining the queue ends publication"
    );
    assert!(pending.is_none());
    assert!(matches!(
        received.try_recv().unwrap(),
        ModelUpdate::RestoreCanonicalTitle
    ));
    assert!(matches!(
        received.try_recv().unwrap(),
        ModelUpdate::Committed { .. }
    ));
    drop(client);
    server.join().unwrap();
}

#[test]
fn a_pane_change_is_resent_only_while_the_daemon_still_remembers_it() {
    use crate::workspace_model::views::{Change, Scope};
    let first = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let change = ModelEdit::ChangeView {
        scope: Scope::Channel(ChannelId::new()),
        expected_revision: 4,
        operation: uuid::Uuid::new_v4(),
        change: Change::Remove { id: CardId::new() },
    };
    assert!(may_resend(&change, first, first));
    assert!(may_resend(&change, first, first + REPLAY_LIMIT));
    assert!(!may_resend(
        &change,
        first,
        first + REPLAY_LIMIT + Duration::from_secs(1)
    ));
    // A clock that went back cannot show the change is still young.
    assert!(!may_resend(&change, first, first - Duration::from_secs(1)));
    // The limit must leave the daemon days of margin for clock skew.
    assert!(
        crate::workspace_model::views::OPERATION_RETENTION >= REPLAY_LIMIT * 6,
        "the daemon must remember an operation well past its last resend"
    );
    // Other edits carry their own idempotency and are never dropped.
    let delete = ModelEdit::DeleteChannel {
        id: ChannelId::new(),
    };
    assert!(may_resend(&delete, first, first + REPLAY_LIMIT * 30));
}

#[test]
fn absent_channel_candidates_preserve_local_order_duplicates_and_shared_flags() {
    let project = ProjectId::new();
    let repository = PathBuf::from("/repo/.git");
    let mut locals = (0..2_046)
        .map(|index| {
            let mut channel = local(ChannelId::new(), project, "local", index);
            channel.shared = index % 2 == 0;
            let card = CardId::new();
            channel.cards.push(card);
            channel.focused = Some(card);
            channel
        })
        .collect::<Vec<_>>();
    let missing = locals[0].id;
    let present = locals[1].id;
    let skipped_adoption = locals[3].id;
    let mut incoming = locals
        .iter()
        .enumerate()
        .filter(|(index, _)| index % 3 != 0)
        .rev()
        .map(|(_, channel)| shared(channel.id, &repository, "shared", channel.ordinal))
        .collect::<Vec<_>>();
    // Even a model row the caller skips while adopting is still present for
    // absence authority. The selector must consume the complete model list.
    incoming.push(shared(skipped_adoption, &repository, "skipped adoption", 0));
    incoming.push(incoming[0].clone());
    incoming.push(shared(ChannelId::new(), &repository, "not local", 1));
    let mut duplicate_missing = locals[0].clone();
    duplicate_missing.shared = false;
    duplicate_missing.title = "unshared duplicate".into();
    locals.insert(9, duplicate_missing);
    let mut duplicate_present = locals[2].clone();
    // After the insertion, original row1 is still at index1.
    duplicate_present.id = present;
    duplicate_present.shared = true;
    locals.push(duplicate_present);
    assert_eq!(locals.len(), 2_048);
    let before = locals.clone();

    // Independent old nested membership is the semantic oracle, not an
    // assertion about HashSet iteration or measured runtime.
    let expected = locals
        .iter()
        .filter(|local| !incoming.iter().any(|shared| shared.id == local.id))
        .map(|local| (local.id, local.shared))
        .collect::<Vec<_>>();
    let absent = channels_absent_from_model(&locals, &incoming);
    assert_eq!(absent, expected);
    assert_eq!(absent.len(), 682);
    assert_eq!(
        absent
            .iter()
            .copied()
            .filter(|(id, _)| *id == missing)
            .collect::<Vec<_>>(),
        [(missing, true), (missing, false)],
        "local duplicates retain order and their distinct retirement authority"
    );
    assert!(
        !absent
            .iter()
            .any(|(id, _)| *id == present || *id == skipped_adoption)
    );
    assert_eq!(
        locals, before,
        "titles, conversations and focus are untouched"
    );

    assert_eq!(
        channels_absent_from_model(&locals, &[]),
        locals
            .iter()
            .map(|local| (local.id, local.shared))
            .collect::<Vec<_>>()
    );
    let all_present = locals
        .iter()
        .rev()
        .map(|channel| shared(channel.id, &repository, "shared", channel.ordinal))
        .collect::<Vec<_>>();
    assert!(channels_absent_from_model(&locals, &all_present).is_empty());
}

#[test]
fn agent_and_rank_edges_carry_canonical_rows_without_full_reads() {
    for removed in [false, true] {
        let (_, mut agent, _, _) = managed_display_fixture();
        agent.name = Some("Refreshed managed agent".into());
        let agent_id = agent.id;
        let initial = if removed {
            vec![agent.clone()]
        } else {
            Vec::new()
        };
        let expected_agents = if removed {
            Vec::new()
        } else {
            vec![agent.clone()]
        };
        let channel = shared(ChannelId::new(), Path::new("/repo/.git"), "Original", 1);
        let mut titled = channel.clone();
        titled.title = "Manual title".into();
        let created = shared(ChannelId::new(), Path::new("/repo/.git"), "Other", 2);
        let expected_created = created.clone();
        let expected_initial = initial.clone();
        let (mut client, server) = publisher_connection(move |peer, proof| {
            let identity = uuid::Uuid::new_v4();
            let labels = |peer: &mut std::os::unix::net::UnixStream| {
                let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
                assert_eq!(
                    request.request,
                    ClientRequest::ReadWorkspaceMetadata {
                        query: crate::workspace_model::metadata::Query::AgentLabels,
                    },
                    "only the new agent's accepted labels need a metadata read"
                );
                publisher_reply(
                    peer,
                    proof,
                    request.request_id,
                    ServerMessage::Response {
                        response: ServerResponse::WorkspaceMetadata(
                            crate::workspace_model::metadata::Reply::AgentLabels {
                                identity,
                                labels: vec![(agent_id, "reviewer · Review".into())],
                            },
                        ),
                    },
                );
            };
            let watch: ClientEnvelope = read_frame(peer).unwrap().unwrap();
            assert_eq!(watch.request, ClientRequest::WatchWorkspaceModel);
            publisher_reply(
                peer,
                proof,
                watch.request_id,
                ServerMessage::Response {
                    response: ServerResponse::WorkspaceSnapshot {
                        model: WorkspaceModel {
                            identity,
                            channels: vec![channel.clone()],
                            agents: initial,
                            ..Default::default()
                        },
                        closed: vec![],
                    },
                },
            );
            if removed {
                labels(peer);
            }
            let edits = [
                (
                    ModelEdit::RetitleChannel {
                        id: channel.id,
                        title: titled.title.clone(),
                    },
                    EditEffects {
                        channels: vec![titled],
                        // Re-observing an existing agent does not read its
                        // immutable labels a second time.
                        agents: if removed {
                            vec![agent.clone()]
                        } else {
                            Vec::new()
                        },
                        ..Default::default()
                    },
                ),
                (
                    ModelEdit::AgentsChanged,
                    EditEffects {
                        agents: if removed { Vec::new() } else { vec![agent] },
                        removed_agents: if removed { vec![agent_id] } else { Vec::new() },
                        ..Default::default()
                    },
                ),
                (
                    ModelEdit::CreateChannel {
                        id: created.id,
                        repository: created.repository.clone(),
                        title: created.title.clone(),
                        title_source: created.title_source,
                        checkout: None,
                    },
                    EditEffects {
                        channels: vec![created],
                        ..Default::default()
                    },
                ),
            ];
            for (sequence, (edit, effects)) in (1..).zip(edits) {
                write_frame(
                    peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: None,
                        proof: proof.clone(),
                        message: ServerMessage::WorkspaceModelEdited {
                            sequence,
                            edit,
                            effects,
                        },
                    },
                )
                .unwrap();
                if sequence == 2 && !removed {
                    labels(peer);
                }
            }
            write_frame(
                peer,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: None,
                    proof: proof.clone(),
                    message: ServerMessage::HostShuttingDown,
                },
            )
            .unwrap();
            assert!(
                read_frame::<ClientEnvelope>(peer).unwrap().is_none(),
                "metadata deltas must not request another model or repeat known labels"
            );
        });
        let (tx, rx) = async_channel::unbounded();
        assert!(observe_with_client(&tx, &mut client).is_err());
        drop(client);
        server.join().unwrap();
        let updates: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(updates.len(), 5);
        let label_index = if removed { 0 } else { 2 };
        assert!(
            matches!(&updates[label_index], ModelUpdate::AgentLabels(labels)
            if labels == &vec![(agent_id, "reviewer · Review".into())])
        );
        assert!(
            if removed {
                matches!(&updates[label_index + 1], ModelUpdate::ConnectedClosed { model, .. }
                    if model.agents.iter().any(|agent| agent.id == agent_id))
            } else {
                matches!(&updates[label_index + 1], ModelUpdate::Changed { changes, .. }
                    if changes.effects.agents.iter().any(|agent| agent.id == agent_id))
            },
            "the label arrives before the initial inventory or delta that exposes its agent"
        );
        let updates: Vec<_> = updates
            .into_iter()
            .filter(|update| !matches!(update, ModelUpdate::AgentLabels(_)))
            .collect();
        assert_eq!(updates.len(), 4);
        assert!(
            matches!(&updates[0], ModelUpdate::ConnectedClosed { model, .. } if model.agents == expected_initial)
        );
        assert!(matches!(&updates[2], ModelUpdate::Changed { changes, .. }
            if changes.effects.agents == expected_agents
                && changes.effects.removed_agents == if removed { vec![agent_id] } else { Vec::new() }));
        assert!(matches!(&updates[3], ModelUpdate::Changed { changes, .. }
            if changes.effects.channels == [expected_created]));
    }
}

#[test]
fn a_view_edge_applies_to_the_held_view_and_reads_only_the_scope_it_cannot() {
    use crate::terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
        ServerResponse, read_frame, write_frame,
    };
    use crate::workspace_model::{
        delta::{EditEffects, ViewRevision},
        views::{Change, Pane, Scope, View},
    };
    let pane = || Pane {
        id: crate::model::CardId::new(),
        title: "browser".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: crate::model::CardKind::Browser(crate::model::BrowserSession {
            id: crate::model::BrowserId::new(),
            current_url: "https://example.com".parse().unwrap(),
            history: vec![],
            history_index: 0,
            scroll_x: 0.,
            scroll_y: 0.,
            fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
        }),
    };
    let view = |scope: &Scope, revision, panes| View {
        scope: scope.clone(),
        revision,
        panes,
        retired: vec![],
    };
    let edge = |scope: &Scope, change, before, after| {
        (
            ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: 0,
                operation: uuid::Uuid::new_v4(),
                change,
            },
            EditEffects {
                views: vec![ViewRevision {
                    scope: scope.clone(),
                    before,
                    after,
                }],
                ..Default::default()
            },
        )
    };
    let held = Scope::Channel(ChannelId::new());
    let seeded = Scope::Channel(ChannelId::new());
    let (first, second, third) = (pane(), pane(), pane());
    let initial = view(&held, 5, vec![first.clone()]);
    let inserted = view(&held, 9, vec![first.clone(), second.clone()]);
    // The daemon's view after an edit this window never saw an edge for.
    let missed = view(&held, 14, vec![second.clone(), third.clone()]);
    let created = view(&seeded, 15, vec![third.clone()]);
    let edges = [
        edge(
            &held,
            Change::Insert {
                pane: second.clone(),
                before: None,
            },
            Some(5),
            Some(9),
        ),
        edge(&held, Change::Remove { id: first.id }, Some(12), Some(14)),
        (
            ModelEdit::SeedView {
                scope: seeded.clone(),
                panes: vec![third.clone()],
                closed: vec![],
            },
            EditEffects {
                views: vec![ViewRevision {
                    scope: seeded.clone(),
                    before: None,
                    after: Some(15),
                }],
                ..Default::default()
            },
        ),
    ];
    let expected_edits = edges
        .iter()
        .map(|(edit, _)| edit.clone())
        .collect::<Vec<_>>();
    let reads = [None, Some(missed.clone()), Some(created.clone())];
    let watched = WorkspaceModel {
        identity: uuid::Uuid::new_v4(),
        views: vec![initial],
        ..Default::default()
    };
    let (stream, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let server = std::thread::spawn(move || {
        let proof = crate::model::HostSessionProof {
            host_session_id: "observer".into(),
            boot_id: "boot".into(),
            host_incarnation: "owner".into(),
        };
        let send = |peer: &mut std::os::unix::net::UnixStream, request_id, message| {
            write_frame(
                peer,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id,
                    proof: proof.clone(),
                    message,
                },
            )
            .unwrap()
        };
        let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        let connected = ServerResponse::Connected {
            proof: proof.clone(),
            resumed: vec![],
        };
        send(
            &mut peer,
            Some(hello.request_id),
            ServerMessage::Response {
                response: connected,
            },
        );
        let watch: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert_eq!(watch.request, ClientRequest::WatchWorkspaceModel);
        send(
            &mut peer,
            Some(watch.request_id),
            ServerMessage::Response {
                response: ServerResponse::WorkspaceSnapshot {
                    model: watched,
                    closed: vec![],
                },
            },
        );
        for (sequence, ((edit, effects), read)) in (1..).zip(edges.into_iter().zip(reads)) {
            send(
                &mut peer,
                None,
                ServerMessage::WorkspaceModelEdited {
                    sequence,
                    edit,
                    effects: effects.clone(),
                },
            );
            let Some(view) = read else {
                continue;
            };
            let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(
                request.request,
                ClientRequest::ReadWorkspaceView {
                    scope: effects.views[0].scope.clone(),
                },
                "only the scope the edge does not determine is read"
            );
            send(
                &mut peer,
                Some(request.request_id),
                ServerMessage::Response {
                    response: ServerResponse::WorkspaceView {
                        view: Some(view),
                        sequence,
                    },
                },
            );
        }
        send(&mut peer, None, ServerMessage::HostShuttingDown);
    });
    let mut client = ControlClient::from_stream(stream).unwrap();
    let (tx, rx) = async_channel::unbounded();
    assert!(observe_with_client(&tx, &mut client).is_err());
    server.join().unwrap();
    let updates = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(matches!(&updates[0], ModelUpdate::ConnectedClosed { .. }));
    let changed = updates[1..]
        .iter()
        .map(|update| match update {
            ModelUpdate::Changed {
                sequence,
                edit,
                changes,
            } => (*sequence, edit.clone(), changes.views.clone()),
            other => panic!("every view edge installs its views, not {other:?}"),
        })
        .collect::<Vec<_>>();
    let installed = |sequence, views: Vec<crate::workspace_model::views::View>| ViewChanges {
        sequence,
        observed: if sequence > 1 {
            views
                .iter()
                .map(|view| (view.scope.clone(), sequence))
                .collect()
        } else {
            HashMap::new()
        },
        views,
        removed: vec![],
        absent: vec![],
    };
    assert_eq!(
        changed,
        vec![
            (1, expected_edits[0].clone(), installed(1, vec![inserted])),
            (2, expected_edits[1].clone(), installed(2, vec![missed])),
            (3, expected_edits[2].clone(), installed(3, vec![created])),
        ],
        "no edge reads the whole model"
    );
}

#[test]
fn create_publisher_keeps_the_canonical_rank_without_unrelated_views() {
    use crate::terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
        ServerResponse, read_frame, write_frame,
    };
    let id = ChannelId::new();
    let repository = PathBuf::from("/repo/.git");
    let mut model = registry(vec![shared(id, &repository, "Canonical title", 17)]);
    model.sequence = 42;
    model.projects.push(ModelProject {
        repository: repository.clone(),
        name: "repo".into(),
        kind: ProjectKind::Git,
        ordinal: 3,
    });
    let app = super::super::tests::context_inspector_test_app(true);
    model.views.push(crate::workspace_model::views::View {
        scope: crate::workspace_model::views::Scope::Channel(id),
        revision: 9,
        panes: vec![
            crate::workspace_model::views::Pane::from_card(&app.durable.cards[0], None).unwrap(),
        ],
        retired: Vec::new(),
    });
    let expected = model.clone();
    let edit = ModelEdit::CreateChannel {
        id,
        repository,
        title: "Requested title".into(),
        title_source: Default::default(),
        checkout: None,
    };
    let expected_edit = edit.clone();
    let (stream, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let server = std::thread::spawn(move || {
        let proof = crate::model::HostSessionProof {
            host_session_id: "rank-response".into(),
            boot_id: "rank-boot".into(),
            host_incarnation: "rank-host".into(),
        };
        let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        write_frame(
            &mut peer,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(hello.request_id),
                proof: proof.clone(),
                message: ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: Vec::new(),
                    },
                },
            },
        )
        .unwrap();
        let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert_eq!(
            request.request,
            ClientRequest::ApplyWorkspaceEdit {
                edit: expected_edit
            }
        );
        write_frame(
            &mut peer,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(request.request_id),
                proof,
                message: ServerMessage::Response {
                    response: receipt_response(model),
                },
            },
        )
        .unwrap();
    });
    let mut client = ControlClient::from_stream(stream).unwrap();
    let (sender, updates) = async_channel::unbounded();
    send(&mut client, edit, None, &sender).unwrap();
    let ModelUpdate::Committed { receipt, views, .. } = updates.try_recv().unwrap() else {
        panic!("a successful create forwards its canonical receipt")
    };
    assert_eq!(receipt.identity, expected.identity);
    assert_eq!(receipt.sequence, expected.sequence);
    assert_eq!(receipt.effects.channels, expected.channels);
    assert_eq!(receipt.effects.projects, expected.projects);
    assert!(
        views.is_empty(),
        "unrelated pane descriptors never reach the UI"
    );
    assert!(updates.try_recv().is_err());
    drop(client);
    server.join().unwrap();
}

#[test]
fn busy_title_publication_yields_after_three_original_attempts_to_the_next_edit() {
    use crate::terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    };
    let id = ChannelId::new();
    let title = ModelEdit::SuggestConversationTitle {
        id,
        card: crate::model::CardId::new(),
        expected_revision: 17,
        title: "Encryption review".into(),
        source: crate::model::ChannelTitleSource::Provider,
    };
    let manual = ModelEdit::RetitleChannel {
        id,
        title: "My project".into(),
    };
    let expected_title = title.clone();
    let expected_manual = manual.clone();
    let (client, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let server = std::thread::spawn(move || {
        let proof = crate::model::HostSessionProof {
            host_session_id: "title-publisher".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "fixture-incarnation".into(),
        };
        let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        write_frame(
            &mut peer,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(hello.request_id),
                proof: proof.clone(),
                message: ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                },
            },
        )
        .unwrap();
        for _ in 0..3 {
            let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(
                request.request,
                ClientRequest::ApplyWorkspaceEdit {
                    edit: expected_title.clone()
                }
            );
            write_frame(
                &mut peer,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::QueueFull,
                        "Resident metadata is busy",
                        true,
                    )),
                },
            )
            .unwrap();
        }
        let next: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert_eq!(
            next.request,
            ClientRequest::ApplyWorkspaceEdit {
                edit: expected_manual
            }
        );
        write_frame(
            &mut peer,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(next.request_id),
                proof: proof.clone(),
                message: ServerMessage::Response {
                    response: receipt_response(WorkspaceModel::default()),
                },
            },
        )
        .unwrap();
        assert!(
            read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
            "No title replay after yielding"
        );
    });
    let mut client = ControlClient::from_stream(client).unwrap();
    let (sender, updates) = async_channel::unbounded();
    send(&mut client, title.clone(), None, &sender).unwrap();
    assert!(matches!(
        updates.try_recv().unwrap(),
        ModelUpdate::RestoreCanonicalTitle
    ));
    assert!(
        matches!(updates.try_recv().unwrap(), ModelUpdate::TitleDeferred(edit) if edit == title)
    );
    send(&mut client, manual, None, &sender).unwrap();
    drop(client);
    server.join().unwrap();
}

#[test]
fn a_source_conflict_drains_the_stale_request_without_an_applied_receipt() {
    use crate::terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    };
    use crate::workspace_model::views::{Change, Pane, Scope, View};
    use std::os::unix::net::UnixStream;
    let app = super::super::tests::context_inspector_test_app(true);
    let before = Pane::from_card(&app.durable.cards[0], None).unwrap();
    let scope = Scope::Channel(ChannelId::new());
    let mut desired = before.clone();
    desired.pending_terminal_start = true;
    desired.title = "Captured title".into();
    let stale = uuid::Uuid::new_v4();
    let next = uuid::Uuid::new_v4();
    let change = Change::Update {
        before: before.clone(),
        pane: desired,
    };
    let mut current = before;
    current.scope = Some("/changed-source".into());
    let model = WorkspaceModel {
        views: vec![View {
            scope: scope.clone(),
            revision: 2,
            panes: vec![current.clone()],
            retired: vec![],
        }],
        ..WorkspaceModel::default()
    };
    let mut presentation = current.clone();
    presentation.title = "Independent next request".into();
    let next_edit = ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: 2,
        operation: next,
        change: Change::Update {
            before: current,
            pane: presentation,
        },
    };
    let first_edit = ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: 1,
        operation: stale,
        change: change.clone(),
    };
    let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
    server_stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let expected_model = model.clone();
    let server = std::thread::spawn(move || {
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("conflict-fixture"),
            boot_id: crate::model::BootId::new("conflict-boot"),
            host_incarnation: crate::model::HostIncarnation::new("conflict-host"),
        };
        let envelope = |request_id, message| ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request_id),
            proof: proof.clone(),
            message,
        };
        let connect: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
        assert!(matches!(connect.request, ClientRequest::Connect { .. }));
        write_frame(
            &mut server_stream,
            &envelope(
                connect.request_id,
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                },
            ),
        )
        .unwrap();
        let edit: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
        assert!(matches!(edit.request, ClientRequest::ApplyWorkspaceEdit {
            edit: ModelEdit::ChangeView { operation, .. } } if operation == stale));
        write_frame(
            &mut server_stream,
            &envelope(
                edit.request_id,
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::ViewConflict,
                    "The pane source changed",
                    true,
                )),
            ),
        )
        .unwrap();
        let read: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
        assert_eq!(
            read.request,
            ClientRequest::ReadWorkspaceView {
                scope: model.views[0].scope.clone()
            }
        );
        write_frame(
            &mut server_stream,
            &envelope(
                read.request_id,
                ServerMessage::Response {
                    response: ServerResponse::WorkspaceView {
                        view: Some(model.views[0].clone()),
                        sequence: 2,
                    },
                },
            ),
        )
        .unwrap();
        // A revision-only retry would repeat stale here and fail. The
        // publisher must instead be free to admit the next independent edit.
        let edit: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
        assert!(matches!(edit.request, ClientRequest::ApplyWorkspaceEdit {
            edit: ModelEdit::ChangeView { operation, .. } } if operation == next));
        write_frame(
            &mut server_stream,
            &envelope(
                edit.request_id,
                ServerMessage::Response {
                    response: ServerResponse::WorkspaceEdit(EditReceipt {
                        identity: model.identity,
                        sequence: 3,
                        effects: EditEffects {
                            views: vec![crate::workspace_model::delta::ViewRevision {
                                scope: model.views[0].scope.clone(),
                                before: Some(2),
                                after: Some(3),
                            }],
                            ..Default::default()
                        },
                    }),
                },
            ),
        )
        .unwrap();
        assert!(
            read_frame::<ClientEnvelope>(&mut server_stream)
                .unwrap()
                .is_none()
        );
    });
    let mut client = ControlClient::from_stream(client_stream).unwrap();
    let (tx, rx) = async_channel::unbounded();
    let mut mirror = Mirror::default();
    send_with_mirror(&mut client, first_edit, None, &tx, &mut mirror).unwrap();
    assert!(
        matches!(rx.try_recv().unwrap(), ModelUpdate::ScopeConflicted {
        operation, change: actual, views, ..
    } if operation == stale && actual == change && views.views == expected_model.views)
    );
    assert!(
        rx.try_recv().is_err(),
        "a refused source edit has no ViewApplied receipt"
    );
    send_with_mirror(&mut client, next_edit, None, &tx, &mut mirror).unwrap();
    assert!(matches!(rx.try_recv().unwrap(), ModelUpdate::Committed {
        edit: ModelEdit::ChangeView { operation, .. }, ..
    } if operation == next));
    drop(client);
    server.join().unwrap();
}

#[test]
fn a_definitive_worker_refusal_does_not_replay_or_block_the_next_edit() {
    use crate::terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    };
    use crate::workspace_model::views::{Change, Pane, Scope, TerminalRemovalBasis, View};
    use std::os::unix::net::UnixStream;
    let app = super::super::tests::context_inspector_test_app(true);
    let mut before = Pane::from_card(&app.durable.cards[0], None).unwrap();
    before.kind = CardKind::Terminal(fresh_terminal_session(
        LaunchRecipe::shell("/fixture".into()).unwrap(),
    ));
    let proof = HostSessionProof {
        host_session_id: "fixture".into(),
        boot_id: "boot".into(),
        host_incarnation: "host".into(),
    };
    let scope = Scope::Channel(ChannelId::new());
    let change = Change::UpdateTerminalSource {
        before: before.clone(),
        pane: before.clone(),
        worker: TerminalRemovalBasis::Worker {
            owner: proof.clone(),
            instance: uuid::Uuid::new_v4(),
        },
    };
    let first = ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: 1,
        operation: uuid::Uuid::new_v4(),
        change: change.clone(),
    };
    let next = ModelEdit::OpenProject {
        repository: "/fixture/next".into(),
        name: "Next".into(),
        kind: Default::default(),
    };
    let identity = uuid::Uuid::new_v4();
    let view = View {
        scope: scope.clone(),
        revision: 2,
        panes: vec![before],
        retired: vec![],
    };
    let (stream, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let expected_first = first.clone();
    let expected_next = next.clone();
    let server = std::thread::spawn(move || {
        for step in 0..4 {
            let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            let message = match step {
                0 => {
                    assert!(matches!(request.request, ClientRequest::Connect { .. }));
                    ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: vec![],
                        },
                    }
                }
                1 => {
                    assert!(
                        matches!(request.request, ClientRequest::ApplyWorkspaceEdit { edit } if edit == expected_first)
                    );
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::InvalidRequest,
                        crate::workspace_model::views::REMOVAL_SOURCE_CONFLICT,
                        false,
                    ))
                }
                2 => {
                    assert!(matches!(
                        request.request,
                        ClientRequest::ReadWorkspaceView { .. }
                    ));
                    ServerMessage::Response {
                        response: ServerResponse::WorkspaceView {
                            view: Some(view.clone()),
                            sequence: 2,
                        },
                    }
                }
                _ => {
                    assert!(
                        matches!(request.request, ClientRequest::ApplyWorkspaceEdit { edit } if edit == expected_next)
                    );
                    ServerMessage::Response {
                        response: ServerResponse::WorkspaceEdit(EditReceipt {
                            identity,
                            sequence: 3,
                            effects: Default::default(),
                        }),
                    }
                }
            };
            write_frame(
                &mut peer,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: proof.clone(),
                    message,
                },
            )
            .unwrap();
        }
        assert!(read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none());
    });
    let mut client = ControlClient::from_stream(stream).unwrap();
    let (out, updates) = async_channel::unbounded();
    let (input, requests) = async_channel::unbounded::<ModelRequest>();
    input.send_blocking(first.into()).unwrap();
    input.send_blocking(next.clone().into()).unwrap();
    drop(input);
    let mut pending = None;
    assert!(publish_with_client(&out, &requests, &mut pending, &mut client).is_err());
    assert!(pending.is_none(), "neither request remains to replay");
    assert!(
        matches!(updates.try_recv().unwrap(), ModelUpdate::ScopeConflicted { change: denied, .. } if denied == change)
    );
    assert!(
        matches!(updates.try_recv().unwrap(), ModelUpdate::Committed { edit, .. } if edit == next)
    );
    assert!(updates.try_recv().is_err());
    drop(client);
    server.join().unwrap();
}

#[test]
fn device_project_metadata_creates_durable_sidebar_without_local_filesystem() {
    let mut app = super::super::tests::context_inspector_test_app(true);
    let storage = tempfile::tempdir().unwrap();
    let database = storage.path().join("state.sqlite3");
    let boot = crate::model::BootId::new("device-adoption");
    let (session, _) = crate::session::SessionRuntime::open(&database, boot.clone()).unwrap();
    app.session = Some(session);
    let focus = app.workspace.focused();
    let active = app.durable.active_channel;
    let cards = app.durable.cards.clone();
    let repository = PathBuf::from(format!(
        "/device-only/{}/repository/.git",
        uuid::Uuid::new_v4()
    ));
    assert!(
        !repository.exists(),
        "fixture must be absent on the displaying host"
    );
    let metadata = ModelProject {
        repository: repository.clone(),
        name: "Device repository".into(),
        kind: ProjectKind::Git,
        ordinal: 1,
    };
    assert!(matches!(
        app.adopt_device_project(&metadata),
        Adoption::Opening(_)
    ));
    let row = app
        .sidebar_model
        .projects
        .iter()
        .find(|row| row.repository == repository)
        .unwrap();
    let id = row.id;
    assert_eq!(row.name, metadata.name);
    let channel = shared(ChannelId::new(), &repository, "Remote work", 0);
    assert!(matches!(
        app.adopt_shared_channel(&channel, Some(ProjectKind::Git)),
        Adoption::Applied
    ));
    assert!(
        app.durable
            .channels
            .iter()
            .any(|local| local.id == channel.id && local.project_id == id)
    );
    assert!(matches!(
        app.adopt_device_project(&metadata),
        Adoption::Retained
    ));
    assert_eq!(
        app.sidebar_model
            .projects
            .iter()
            .filter(|project| project.id == id)
            .count(),
        1
    );
    assert_eq!(app.workspace.focused(), focus);
    assert_eq!(app.durable.active_channel, active);
    assert_eq!(app.durable.cards, cards);
    let mut session = app.session.take().unwrap();
    session.shutdown();
    assert_eq!(session.last_error(), None);
    let (mut restored, _) = crate::session::SessionRuntime::open(&database, boot).unwrap();
    let (projects, _) = restored.take_catalog();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].id, id);
    assert_eq!(projects[0].repository, repository);
    assert_eq!(projects[0].name, metadata.name);
    assert_eq!(projects[0].kind, ProjectKind::Git);
    assert!(projects[0].existence.is_open());
    restored.shutdown();
    app.project_ui.closed.insert(repository.clone());
    app.sidebar_model.projects.retain(|row| row.id != id);
    assert!(matches!(
        app.adopt_device_project(&metadata),
        Adoption::Retained
    ));
    assert!(
        app.sidebar_model
            .projects
            .iter()
            .all(|row| row.repository != repository)
    );
}

#[test]
fn worktree_execution_adopts_into_exact_strip_without_a_channel_or_focus_change() {
    use crate::agent_execution::{Agent, Origin, Placement, Spec, Surface, Target};
    let (mut app, _, repository) = app_with_project();
    let path = PathBuf::from("/repo");
    app.active_workspace = Some(path.clone());
    Arc::make_mut(&mut app.durable).active_channel = None;
    let focus = app.workspace.focused();
    let console = TerminalSession {
        id: TerminalId::new(),
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        launch: LaunchRecipe {
            program: "claude".into(),
            arguments: vec![],
            working_directory: "/repo/nested".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        },
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("placement-test"),
            boot_id: crate::model::BootId::new("placement-test"),
            host_incarnation: crate::model::HostIncarnation::new("placement-test"),
        },
    };
    let placement = Placement::Worktree {
        repository: repository.clone(),
        path: path.clone(),
    };
    let agent = Agent {
        name: None,
        id: crate::model::AgentId::new(),
        origin: Origin::User,
        spec: Spec {
            provider: crate::agent::Provider::Claude,
            account: None,
            target: Target {
                repository,
                directory: "/repo/nested".into(),
                placement: Some(placement.clone()),
            },
            surface: Surface::Terminal,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        },
        placement,
        session: None,
        console: Some(console),
        created_ms: 0,
        closed: false,
    };
    let card = CardId::from_uuid(agent.id.as_uuid());
    let channels = app.durable.channels.clone();
    let deletion = app.project_ui.begin_deleting(path.clone());
    app.chrome_ui
        .agent_labels
        .insert(agent.id, "Agent task".into());
    assert!(app.adopt_execution_agent(&agent).is_none());
    drop(app.update(Message::ProjectUi(
        crate::app::project_ui::Action::Deleted {
            capture: app.capture_worktree_removal(&deletion),
            target: deletion,
            result: Err("fixture deletion refused".into()),
        },
    )));
    app.chrome_ui
        .agent_labels
        .insert(agent.id, "Agent task".into());
    assert!(app.adopt_execution_agent(&agent).is_some());
    assert_eq!(app.durable.channels, channels);
    assert_eq!(app.workspace.focused(), focus);
    assert!(app.workspace.cards().iter().any(|pane| pane.id == card));
    assert_eq!(app.durable.card_scopes.get(&card), Some(&path));
    assert_eq!(app.worktree_owners.get(&card), Some(&path));
    assert!(
        app.adopt_execution_agent(&agent).is_none(),
        "replayed model does not mount twice"
    );
}

#[test]
fn first_model_read_uses_moved_native_pane_instead_of_stale_managed_placement() {
    use crate::agent::Provider;
    use crate::agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target};
    let (mut app, project, repository) = app_with_project();
    let active = ChannelId::new();
    let target = ChannelId::new();
    let focus = app.workspace.focused();
    let mailbox_project = ProjectId::new();
    app.sidebar_model.projects.push(SidebarProject::new(
        mailbox_project,
        RepositoryId::new(),
        "Mailbox".into(),
        "/other-project".into(),
        Vec::new(),
        Vec::new(),
    ));
    let durable = Arc::make_mut(&mut app.durable);
    durable
        .channels
        .push(local(active, mailbox_project, "Active", 0));
    durable
        .channels
        .push(local(target, project, "Execution", 1));
    durable.active_channel = Some(active);
    let session = crate::chat::Session {
        native_continuity: Default::default(),
        id: TerminalId::new(),
        proof: Some(crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("execution-host"),
            boot_id: crate::model::BootId::new("execution-boot"),
            host_incarnation: crate::model::HostIncarnation::new("execution-incarnation"),
        }),
        terminal: None,
        owned_codex_listener: None,
        launch: crate::chat::Launch {
            provider: Provider::Claude,
            session_id: None,
            recipe: LaunchRecipe {
                program: "claude".into(),
                arguments: Vec::new(),
                working_directory: PathBuf::from("/repo/worktree"),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            draft_model: None,
        },
    };
    let agent = Agent {
        name: None,
        id: crate::model::AgentId::new(),
        origin: Origin::User,
        spec: Spec {
            provider: Provider::Claude,
            account: None,
            target: Target {
                repository,
                directory: PathBuf::from("/repo/worktree"),
                placement: Some(crate::agent_execution::Placement::Channel(target)),
            },
            surface: Surface::Chat,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        },
        placement: crate::agent_execution::Placement::Channel(target),
        session: Some(HostedSession::Chat(session.clone())),
        console: None,
        created_ms: 0,
        closed: false,
    };
    let card = CardId::from_uuid(agent.id.as_uuid());
    let mut model = registry(vec![
        shared(active, &agent.spec.target.repository, "Mailbox", 0),
        shared(target, &agent.spec.target.repository, "Original", 1),
    ]);
    // Display membership is independent of execution ownership: legacy
    // protocol-77 transfers could place a cagents actor in another project.
    model.channels[0].repository = "/other-project".into();
    model.projects = vec![
        ModelProject {
            repository: agent.spec.target.repository.clone(),
            name: "Execution".into(),
            kind: ProjectKind::Git,
            ordinal: 0,
        },
        ModelProject {
            repository: "/other-project".into(),
            name: "Mailbox".into(),
            kind: ProjectKind::Git,
            ordinal: 1,
        },
    ];
    model.agents.push(agent.clone());
    model.views.push(crate::workspace_model::views::View {
        scope: crate::workspace_model::views::Scope::Channel(active),
        revision: 2,
        panes: vec![crate::workspace_model::views::Pane {
            id: card,
            title: "Mailbox lead".into(),
            user_title: Some("Mailbox lead".into()),
            pending_terminal_start: false,
            width_fraction: 0.5,
            kind: CardKind::Chat(session.clone()),
            scope: Some(agent.spec.target.directory.clone()),
        }],
        retired: vec![],
    });
    model.views.push(crate::workspace_model::views::View {
        scope: agent.placement.clone(),
        revision: 2,
        panes: vec![],
        retired: vec![card],
    });
    assert!(app.workspace_model.views.is_empty());
    app.chrome_ui
        .agent_labels
        .insert(agent.id, "Mailbox lead".into());
    app.workspace_model = model;
    let settings = tempfile::tempdir().unwrap();
    app.settings_path = Some(settings.path().join("settings.toml"));
    app.settings_write_blocked = false;
    let saved = app.settings_save_generation;
    drop(app.reconcile_workspace_model());
    assert!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == active)
            .unwrap()
            .cards
            .contains(&card)
    );
    // The adopted pane's name is saved with the window's settings, or the
    // next start would read it back missing and migrate the view (#3032).
    assert_eq!(
        app.chrome_ui.user_titles.get(&card).map(String::as_str),
        Some("Mailbox lead")
    );
    assert!(app.settings_save_generation > saved);
    assert!(
        !app.durable
            .channels
            .iter()
            .find(|channel| channel.id == target)
            .unwrap()
            .cards
            .contains(&card)
    );
    assert_eq!(
        app.durable.card_scopes.get(&card),
        Some(&agent.spec.target.directory)
    );
    assert_eq!(
        app.workspace_model.agents[0], agent,
        "display adoption must not rewrite execution authority"
    );
    assert!(app.chat_start_pending(card));
    assert_eq!(app.workspace.focused(), focus);
    assert!(
        matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(current) if current == &session)
    );
    // Open Session in automation history must select this same display,
    // even when its retained agent record still names the original target.
    use crate::automation_ui::{Action, Effect};
    use crate::automations::{
        Definition, Firing, FiringState, RunDetail, Schedule, SchedulePattern, SessionMode, Trigger,
    };
    let automation = uuid::Uuid::new_v4();
    let mut automated = agent.clone();
    automated.origin = Origin::Automation { id: automation };
    let detail = RunDetail {
        definition: Definition {
            name: "Mailbox".into(),
            prompt: "Review".into(),
            agent: automated.spec.clone(),
            schedule: Schedule {
                pattern: SchedulePattern::Weekly {
                    weekday: 1,
                    hour: 9,
                    minute: 0,
                },
                timezone: "UTC".into(),
            },
            session_mode: SessionMode::Reuse,
            precheck: None,
        },
        firing: Firing {
            id: uuid::Uuid::new_v4(),
            automation,
            name: "Mailbox".into(),
            revision: 1,
            trigger: Trigger::Manual {
                request: uuid::Uuid::new_v4(),
            },
            state: FiringState::Running,
            release_pending: false,
            cancel_requested: false,
            created_ms: 0,
            started_ms: Some(0),
            finished_ms: None,
            agent: Some(automated.id),
            run: None,
            skip: None,
            skipped_occurrences: None,
            diagnostic: None,
        },
        agent: Some(automated),
        run: None,
    };
    let Some(Effect::Request { sequence, .. }) = app
        .automation_ui
        .update(Action::InspectRun(crate::model::RunId::new()))
    else {
        panic!("run request expected")
    };
    app.automation_ui.update(Action::Received {
        sequence,
        result: Arc::new(Ok(crate::automations::Response::Detail(Box::new(
            detail.clone(),
        )))),
    });
    let Some(Effect::Request { sequence, .. }) = app.automation_ui.update(Action::OpenSession)
    else {
        panic!("open request expected")
    };
    Arc::make_mut(&mut app.durable).active_channel = Some(target);
    drop(app.update_automation_ui(Action::Received {
        sequence,
        result: Arc::new(Ok(crate::automations::Response::Detail(Box::new(detail)))),
    }));
    assert_eq!(
        app.durable.active_channel,
        Some(active),
        "history opens the actual moved pane, not the stale launch channel"
    );
}

#[test]
fn model_read_publishes_new_managed_chat_before_initialized_view_projection() {
    use crate::agent::Provider;
    use crate::agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target};
    use crate::workspace_model::views::{Change, Scope, View};
    for (active_target, retired, blocked_before, metadata_edge) in [
        (true, false, false, 0),
        (false, false, false, 0),
        (true, true, false, 0),
        (true, false, true, 0),
        (true, false, false, 1),
        (true, false, false, 2),
        (true, false, false, 3),
        (true, false, false, 4),
        (true, false, false, 5),
        (true, false, false, 6),
        (true, false, false, 7),
    ] {
        let existing_primary = metadata_edge != 0;
        let (mut app, project, repository) = app_with_project();
        let active = ChannelId::new();
        let target = if active_target {
            active
        } else {
            ChannelId::new()
        };
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(local(active, project, "Active", 0));
        if target != active {
            durable
                .channels
                .push(local(target, project, "Execution", 1));
        }
        durable.active_channel = Some(active);
        let session = crate::chat::Session {
            native_continuity: Default::default(),
            id: TerminalId::new(),
            proof: Some(crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("execution-host"),
                boot_id: crate::model::BootId::new("execution-boot"),
                host_incarnation: crate::model::HostIncarnation::new("execution-incarnation"),
            }),
            terminal: None,
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: Provider::Claude,
                session_id: None,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: PathBuf::from("/repo/worktree"),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                draft_model: None,
            },
        };
        let agent = Agent {
            name: None,
            id: crate::model::AgentId::new(),
            origin: Origin::User,
            spec: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository,
                    directory: PathBuf::from("/repo/worktree"),
                    placement: Some(crate::agent_execution::Placement::Channel(target)),
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: crate::agent_execution::Placement::Channel(target),
            session: Some(HostedSession::Chat(session.clone())),
            console: None,
            created_ms: 0,
            closed: false,
        };
        app.chrome_ui
            .agent_labels
            .insert(agent.id, "Agent task".into());
        let card = CardId::from_uuid(agent.id.as_uuid());
        let scope = Scope::Channel(target);
        let mut channels = vec![shared(active, &agent.spec.target.repository, "Active", 0)];
        if target != active {
            channels.push(shared(
                target,
                &agent.spec.target.repository,
                "Execution",
                1,
            ));
        }
        let mut model = registry(channels);
        if existing_primary {
            model
                .channels
                .iter_mut()
                .find(|channel| channel.id == target)
                .unwrap()
                .title_source = crate::model::ChannelTitleSource::Prompt;
        }
        let mut view = View::empty(scope.clone());
        view.revision = 17;
        if retired {
            view.retired.push(card);
        }
        model.views.push(view);
        if existing_primary {
            app.active_workspace = Some(PathBuf::from("/repo"));
        }
        app.workspace.resize_viewport(1000.0);
        let primary = existing_primary.then(|| {
            let mut primary = agent.clone();
            primary.id = crate::model::AgentId::new();
            let mut primary_session = session.clone();
            primary_session.id = TerminalId::new();
            primary.session = Some(HostedSession::Chat(primary_session.clone()));
            let id = CardId::from_uuid(primary.id.as_uuid());
            model.views[0]
                .panes
                .push(crate::workspace_model::views::Pane {
                    id,
                    title: "Existing primary".into(),
                    user_title: Some("Existing primary".into()),
                    pending_terminal_start: false,
                    width_fraction: 0.5,
                    kind: CardKind::Chat(primary_session),
                    scope: Some(primary.spec.target.directory.clone()),
                });
            app.chrome_ui
                .agent_labels
                .insert(primary.id, "Existing primary".into());
            model.agents.push(primary);
            id
        });
        app.workspace_model = model.clone();
        let (tx, rx) = test_model_channel();
        app.workspace_model_tx = tx;
        // This view already exists and its real seed acknowledgement has
        // completed before the daemon announces the new live Agent.
        app.acknowledge_view_edit(scope.clone(), None);
        if let Some(primary) = primary {
            drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
            assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![primary]);
            assert_eq!(app.workspace.focused(), Some(primary));
        }
        while rx.try_recv().is_ok() {}
        let mut conflict = None;
        if blocked_before {
            let mut conflict_agent = agent.clone();
            conflict_agent.id = crate::model::AgentId::new();
            conflict_agent.origin = Origin::Automation {
                id: uuid::Uuid::new_v4(),
            };
            let mut foreign_session = session.clone();
            foreign_session.id = TerminalId::new();
            let id = CardId::from_uuid(conflict_agent.id.as_uuid());
            model.views[0]
                .panes
                .push(crate::workspace_model::views::Pane {
                    id,
                    title: "Conflicting saved descriptor".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 0.72,
                    kind: CardKind::Chat(foreign_session),
                    scope: Some(agent.spec.target.directory.clone()),
                });
            model.agents.push(conflict_agent);
            model.sequence += 1;
            drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
            assert!(app.shared_views.quarantined.contains(&id));
            assert!(
                !app.shared_views.blocked.contains(&scope),
                "a stale sibling is fenced alone, not with its whole scope"
            );
            assert!(app.card_state(id).is_none());
            while rx.try_recv().is_ok() {}
            conflict = Some(id);
        }
        // A fresh actor is admitted beside any fenced sibling.
        model.agents.push(agent.clone());
        model.sequence += 1;
        let focus = app.workspace.focused();
        drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
        let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        if let Some(conflict) = conflict {
            assert!(app.card_state(conflict).is_none());
            assert!(
                !edits.iter().any(|edit| matches!(edit,
                    ModelEdit::ChangeView { change, .. } if change_mentions(change, conflict))),
                "the fenced pane is neither published, moved nor removed"
            );
        }
        let inserts = edits
            .iter()
            .filter_map(|edit| match edit {
                ModelEdit::ChangeView {
                    scope: actual,
                    operation,
                    expected_revision,
                    change: Change::Insert { pane, before },
                } if pane.id == card => Some((actual, operation, expected_revision, pane, before)),
                _ => None,
            })
            .collect::<Vec<_>>();
        if retired {
            assert!(
                inserts.is_empty(),
                "authoritative retirement grants no new publication"
            );
            assert!(app.card_state(card).is_none());
            continue;
        }
        assert_eq!(
            inserts.len(),
            1,
            "exact first adoption must publish once before passive removal"
        );
        let (actual, operation, revision, pane, before) = inserts[0];
        assert_eq!(actual, &scope);
        assert_eq!(*revision, 17);
        assert_eq!(*before, None);
        assert!(matches!(&pane.kind, CardKind::Chat(actual) if actual == &session));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == target)
                .unwrap()
                .cards
                .contains(&card)
        );
        assert!(
            app.chat_start_pending(card),
            "unpolled real attachment Task remains Starting, not detached"
        );
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert_eq!(app.workspace_model.agents.last(), Some(&agent));
        if !active_target || existing_primary {
            assert_eq!(app.workspace.focused(), focus);
        }
        if let Some(primary) = primary {
            assert_eq!(
                app.workspace.order().collect::<Vec<_>>(),
                vec![primary, card]
            );
        }
        // Repeated daemon snapshots preserve the exact pending Insert,
        // rather than re-issuing it or treating the old empty view as close.
        drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(!std::iter::from_fn(|| rx.try_recv().ok()).any(|edit|
            matches!(edit, ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card)));
        let change = Change::Insert {
            pane: pane.clone(),
            before: None,
        };
        model
            .views
            .iter_mut()
            .find(|view| view.scope == scope)
            .unwrap()
            .apply(&change)
            .unwrap();
        model.sequence += 1;
        model
            .views
            .iter_mut()
            .find(|view| view.scope == scope)
            .unwrap()
            .revision += 1;
        let edge = existing_primary.then(|| match metadata_edge {
            1 => ModelEdit::AgentsChanged,
            2 => ModelEdit::RetitleChannel {
                id: target,
                title: "New manual title".into(),
            },
            3 => ModelEdit::SuggestChannelTitle {
                id: target,
                title: "New provider title".into(),
                source: crate::model::ChannelTitleSource::Provider,
            },
            4 => ModelEdit::SuggestConversationTitle {
                id: target,
                card: primary.unwrap(),
                expected_revision: model.views[0].revision,
                title: "New conversation title".into(),
                source: crate::model::ChannelTitleSource::Provider,
            },
            5 => ModelEdit::CreateChannel {
                id: ChannelId::new(),
                repository: agent.spec.target.repository.clone(),
                title: "Independent channel".into(),
                title_source: Default::default(),
                checkout: None,
            },
            6 => ModelEdit::OpenProject {
                repository: "/other/.git".into(),
                name: "Other".into(),
                kind: ProjectKind::Git,
            },
            7 => ModelEdit::ReopenProject {
                repository: "/other/.git".into(),
                name: "Other".into(),
                kind: ProjectKind::Git,
            },
            _ => unreachable!(),
        });
        if let Some(edit) = &edge {
            // The observer and publisher use distinct connections. A
            // metadata edge may arrive before this Insert's own reply.
            if matches!(edit, ModelEdit::SuggestConversationTitle { .. }) {
                // The real observer reads this proof-dependent edge as a
                // complete snapshot, not a fold against its stale view17.
                let mut observed = model.clone();
                observed.fold(model.sequence + 1, edit);
                drop(app.apply_workspace_model_update(ModelUpdate::Read(observed)));
            } else {
                drop(app.apply_workspace_model_update(ModelUpdate::Edited {
                    sequence: model.sequence + 1,
                    edit: edit.clone(),
                }));
            }
        }
        let newer_metadata = (
            app.workspace_model.channels.clone(),
            app.workspace_model.projects.clone(),
        );
        let accepted_snapshot = model.clone();
        drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: scope.clone(),
            operation: Some(*operation),
            model: model.clone(),
        }));
        if let Some(primary) = primary {
            assert_eq!(
                app.workspace.order().collect::<Vec<_>>(),
                vec![primary, card]
            );
            assert_eq!(app.workspace.focused(), Some(primary));
            assert_eq!(app.workspace_model.channels, newer_metadata.0);
            assert_eq!(app.workspace_model.projects, newer_metadata.1);
            let expected_title = match metadata_edge {
                2 => Some("New manual title"),
                3 => Some("New provider title"),
                4 => Some("New conversation title"),
                _ => None,
            };
            if let Some(title) = expected_title {
                assert_eq!(app.workspace_model.channel(target).unwrap().title, title);
                assert_eq!(
                    app.durable
                        .channels
                        .iter()
                        .find(|channel| channel.id == target)
                        .unwrap()
                        .title,
                    title
                );
            }
            let next_sequence = model.sequence + 1;
            model.fold(next_sequence, edge.as_ref().unwrap());
            drop(app.apply_workspace_model_update(ModelUpdate::Read(model)));
            assert_eq!(
                app.workspace.order().collect::<Vec<_>>(),
                vec![primary, card]
            );
            assert_eq!(app.workspace.focused(), Some(primary));
        }
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.chat_start_pending(card));
        assert!(
            app.workspace_model
                .views
                .iter()
                .find(|view| view.scope == scope)
                .unwrap()
                .panes
                .iter()
                .any(|pane| pane.id == card)
        );
        if let Some(primary) = primary {
            // A genuinely newer complete view remains authoritative even
            // when the original Insert receipt is delivered again.
            let mut removed = app.workspace_model.clone();
            removed.sequence += 1;
            let view = removed
                .views
                .iter_mut()
                .find(|view| view.scope == scope)
                .unwrap();
            view.apply(&Change::Remove { id: card }).unwrap();
            view.revision += 1;
            drop(app.apply_workspace_model_update(ModelUpdate::Read(removed.clone())));
            drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: scope.clone(),
                operation: Some(*operation),
                model: accepted_snapshot,
            }));
            assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![primary]);
            assert!(!app.card_state(card).unwrap().existence.is_open());
            assert_eq!(app.workspace_model.views, removed.views);
        }
    }
}

#[test]
fn late_view_snapshot_preserves_observed_channel_and_project_removal() {
    use crate::workspace_model::views::{Scope, View};
    for close_project in [false, true] {
        let (mut app, _, repository) = app_with_project();
        let channel = ChannelId::new();
        let mut original = registry(vec![shared(channel, &repository, "Original", 0)]);
        original.views.push(View::empty(Scope::Channel(channel)));
        assert!(app.install_workspace_model_snapshot(original.clone()));
        let edit = if close_project {
            ModelEdit::CloseProject {
                repository: repository.clone(),
            }
        } else {
            ModelEdit::DeleteChannel { id: channel }
        };
        let next_sequence = original.sequence + 2;
        drop(app.apply_workspace_model_update(ModelUpdate::Edited {
            sequence: next_sequence,
            edit,
        }));
        let metadata = app.workspace_model.clone();
        original.sequence += 1;
        original.views[0].revision += 1;
        assert!(app.install_workspace_model_snapshot(original.clone()));
        assert_eq!(app.workspace_model.sequence, next_sequence);
        assert_eq!(app.workspace_model.channels, metadata.channels);
        assert_eq!(app.workspace_model.projects, metadata.projects);
        assert_eq!(
            app.workspace_model.closed_projects,
            metadata.closed_projects
        );
        assert_eq!(app.workspace_model.views, original.views);
    }
}

#[test]
fn edge_views_install_by_revision_and_fence_older_snapshots() {
    use crate::workspace_model::{
        delta::ViewChanges,
        views::{Scope, View},
    };
    let (mut app, _, repository) = app_with_project();
    let channel = ChannelId::new();
    let scope = Scope::Channel(channel);
    let mut original = registry(vec![shared(channel, &repository, "Original", 0)]);
    original.sequence = 4;
    original.views.push(View {
        revision: 3,
        ..View::empty(scope.clone())
    });
    assert!(app.install_workspace_model_snapshot(original.clone()));
    let at = |revision| View {
        revision,
        ..View::empty(scope.clone())
    };
    let mut retitled = original.channels[0].clone();
    retitled.title = String::new();
    let changes = |views, channels| EdgeChanges {
        identity: None,
        views: ViewChanges {
            sequence: 0,
            observed: HashMap::new(),
            views,
            removed: vec![],
            absent: vec![],
        },
        effects: EditEffects {
            channels,
            ..Default::default()
        },
    };
    let change = || ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: 0,
        operation: uuid::Uuid::new_v4(),
        change: crate::workspace_model::views::Change::Remove {
            id: crate::model::CardId::new(),
        },
    };
    drop(app.apply_workspace_model_update(ModelUpdate::Changed {
        sequence: 6,
        edit: change(),
        changes: changes(vec![at(7)], vec![retitled.clone()]),
    }));
    assert_eq!(app.workspace_model.views, vec![at(7)]);
    assert_eq!(app.workspace_model.channels, vec![retitled]);
    assert_eq!(app.shared_views.snapshot_sequence, 4);
    // A reconnect snapshot recovers unrelated rows while preserving newer edge rows.
    let mut older = original.clone();
    older.sequence = 5;
    assert!(app.install_workspace_model_snapshot(older));
    assert_eq!(app.workspace_model.views, vec![at(7)]);

    // A stale edge still brings a view a read took past the snapshot,
    // but neither rewinds one nor revives a scope the snapshot lacks.
    let other = Scope::Channel(ChannelId::new());
    drop(app.apply_workspace_model_update(ModelUpdate::Changed {
        sequence: 5,
        edit: change(),
        changes: changes(
            vec![at(9), View::empty(other.clone())],
            vec![original.channels[0].clone()],
        ),
    }));
    assert_eq!(app.workspace_model.views, vec![at(9)]);
    assert_eq!(app.workspace_model.channels[0].title, "");
    drop(app.apply_workspace_model_update(ModelUpdate::Changed {
        sequence: 5,
        edit: change(),
        changes: changes(vec![at(8)], vec![]),
    }));
    assert_eq!(app.workspace_model.views, vec![at(9)]);
}

#[test]
fn scoped_absence_removes_newer_window_data_and_preserves_later_recreation() {
    use crate::workspace_model::views::{Scope, View};
    let (mut app, _, repository) = app_with_project();
    let channel = ChannelId::new();
    let scope = Scope::Channel(channel);
    let at = |revision| View {
        revision,
        ..View::empty(scope.clone())
    };
    let mut initial = registry(vec![shared(channel, &repository, "Work", 0)]);
    initial.sequence = 4;
    initial.views = vec![at(10)];
    assert!(app.install_workspace_model_snapshot(initial));
    assert!(app.install_edge_views(
        ViewChanges {
            sequence: 13,
            views: vec![at(13)],
            ..Default::default()
        },
        true
    ));
    let mut older_mirror = Mirror::new(&[at(10)]);
    let absent = older_mirror.install_at(scope.clone(), None, 20);
    assert!(app.install_edge_views(absent, true));
    assert!(
        app.workspace_model.views.is_empty(),
        "query absence is authoritative beyond the mirror's old revision"
    );
    assert!(!app.install_edge_views(
        ViewChanges {
            sequence: 15,
            views: vec![at(15)],
            ..Default::default()
        },
        true
    ));
    assert!(
        app.workspace_model.views.is_empty(),
        "an older presence cannot undo a later absence"
    );
    assert!(app.install_edge_views(
        ViewChanges {
            sequence: 21,
            views: vec![at(30)],
            ..Default::default()
        },
        true
    ));
    assert!(!app.install_edge_views(older_mirror.install_at(scope.clone(), None, 20), true));
    assert_eq!(
        app.workspace_model.views,
        vec![at(30)],
        "a delayed absence cannot erase a recreated scope"
    );
}

#[test]
fn delayed_reconnect_baseline_never_resurrects_a_deleted_scope() {
    use crate::workspace_model::views::{Scope, View};
    let (mut app, _, repository) = app_with_project();
    let channel = ChannelId::new();
    let scope = Scope::Channel(channel);
    let mut baseline = registry(vec![shared(channel, &repository, "Work", 0)]);
    baseline.sequence = 100;
    baseline.views = vec![View {
        revision: 80,
        ..View::empty(scope.clone())
    }];
    assert!(app.install_workspace_model_snapshot(baseline.clone()));
    drop(app.apply_changed_edge(
        120,
        &ModelEdit::AgentsChanged,
        EdgeChanges {
            views: ViewChanges {
                sequence: 120,
                removed: vec![(scope.clone(), 80)],
                ..Default::default()
            },
            ..Default::default()
        },
    ));
    assert!(app.workspace_model.views.is_empty());
    assert!(app.install_workspace_model_snapshot(baseline.clone()));
    assert!(
        app.workspace_model.views.is_empty(),
        "a delayed reconnect baseline cannot resurrect a deleted scope"
    );
    let recreated = View {
        revision: 130,
        ..View::empty(scope)
    };
    drop(app.apply_changed_edge(
        130,
        &ModelEdit::AgentsChanged,
        EdgeChanges {
            views: ViewChanges {
                sequence: 130,
                views: vec![recreated.clone()],
                ..Default::default()
            },
            ..Default::default()
        },
    ));
    baseline.views.clear();
    baseline.sequence = 110;
    assert!(app.install_workspace_model_snapshot(baseline));
    assert_eq!(
        app.workspace_model.views,
        vec![recreated],
        "a baseline's older absence cannot delete a later scope incarnation"
    );
}

#[test]
fn compacted_worktree_view_stays_initialized_through_recreation() {
    use crate::workspace_model::{
        delta::ViewRevision,
        views::{Pane, Scope, View},
    };
    let (mut app, project, repository) = app_with_project();
    let path = PathBuf::from("/repo/removed-worktree");
    let scope = Scope::Worktree {
        repository: repository.clone(),
        path: path.clone(),
    };
    // The view removal can arrive before the window refreshes the worktree list.
    app.sidebar_model.projects[0]
        .worktrees
        .push(crate::sidebar::SidebarWorktree::from_durable(
            crate::model::WorktreeId::new(),
            path.clone(),
            None,
        ));
    app.known_worktree_roots.insert(path);
    let channel = ChannelId::new();
    let card = app.workspace.focused().unwrap();
    let pane = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
    let mut local_channel = local(channel, project, "Current work", 0);
    local_channel.shared = true;
    local_channel.cards = vec![card];
    local_channel.focused = Some(card);
    let durable = Arc::make_mut(&mut app.durable);
    durable.channels.push(local_channel);
    durable.active_channel = Some(channel);

    let current_view = View {
        revision: 3,
        panes: vec![pane.clone()],
        ..View::empty(channel)
    };
    let mut initial = registry(vec![shared(channel, &repository, "Current work", 0)]);
    initial.sequence = 4;
    initial.views = vec![
        current_view.clone(),
        View {
            revision: 4,
            ..View::empty(scope.clone())
        },
    ];
    let identity = initial.identity;
    let mut mirror = Mirror::for_model(&initial);
    assert!(app.install_workspace_model_snapshot(initial));
    app.shared_views
        .initialize(current_view.scope.clone(), vec![pane]);
    app.shared_views.initialize(scope.clone(), vec![]);
    let published = intercept_publishes(&mut app);
    let assert_pane_unchanged = |app: &Cagents| {
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.shared_views.closed.is_empty());
        assert_eq!(app.durable.active_channel, Some(channel));
        assert_eq!(app.durable.channels[0].cards, vec![card]);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![card]);
        assert_eq!(app.workspace.focused(), Some(card));
        assert!(app.shared_views.scope_settled(&scope));
        for edit in drain(&published) {
            assert!(
                !matches!(
                    edit,
                    ModelEdit::SeedView { .. } | ModelEdit::ChangeView { .. }
                ),
                "unexpected view publication: {edit:?}"
            );
        }
    };

    let edit = ModelEdit::AgentsChanged;
    let effects = EditEffects {
        views: vec![ViewRevision {
            scope: scope.clone(),
            before: Some(4),
            after: None,
        }],
        ..Default::default()
    };
    let mut removal = mirror.apply(&edit, &effects).changes;
    removal.sequence = 5;
    drop(app.apply_workspace_model_update(ModelUpdate::Changed {
        sequence: 5,
        edit,
        changes: EdgeChanges {
            identity: Some(identity),
            views: removal,
            effects,
        },
    }));
    assert_eq!(app.workspace_model.views, vec![current_view.clone()]);
    assert_eq!(app.shared_views.scope_absences.get(&scope), Some(&5));
    assert_pane_unchanged(&app);

    let edit = ModelEdit::SeedView {
        scope: scope.clone(),
        panes: vec![],
        closed: vec![],
    };
    let recreated = View {
        revision: 6,
        ..View::empty(scope.clone())
    };
    let effects = EditEffects {
        views: vec![ViewRevision {
            scope: scope.clone(),
            before: None,
            after: Some(recreated.revision),
        }],
        ..Default::default()
    };
    assert_eq!(
        mirror.apply(&edit, &effects).unresolved,
        vec![scope.clone()]
    );
    let views = mirror.install_at(scope.clone(), Some(recreated.clone()), 6);
    drop(app.apply_workspace_model_update(ModelUpdate::Changed {
        sequence: 6,
        edit,
        changes: EdgeChanges {
            identity: Some(identity),
            views,
            effects,
        },
    }));
    assert_eq!(app.workspace_model.views, vec![current_view, recreated]);
    assert_pane_unchanged(&app);
}

#[test]
fn scoped_reads_ahead_of_the_edge_fence_later_but_older_full_snapshots() {
    use crate::workspace_model::views::{Scope, View};
    let (mut app, _, repository) = app_with_project();
    let channel = ChannelId::new();
    let scope = Scope::Channel(channel);
    let at = |revision| View {
        revision,
        ..View::empty(scope.clone())
    };
    let mut baseline = registry(vec![shared(channel, &repository, "Work", 0)]);
    baseline.sequence = 4;
    baseline.views = vec![at(4)];
    assert!(app.install_workspace_model_snapshot(baseline.clone()));
    let mut mirror = Mirror::new(&baseline.views);
    let ahead = mirror.install_at(scope.clone(), Some(at(10)), 10);
    drop(app.apply_changed_edge(
        5,
        &ModelEdit::AgentsChanged,
        EdgeChanges {
            views: ahead,
            ..Default::default()
        },
    ));
    assert_eq!(app.workspace_model.sequence, 5);
    baseline.sequence = 8;
    baseline.views = vec![at(8)];
    assert!(app.install_workspace_model_snapshot(baseline.clone()));
    assert_eq!(
        app.workspace_model.views,
        vec![at(10)],
        "the scope read is newer even though the full snapshot overtook the edge sequence"
    );
    let absent = mirror.install_at(scope.clone(), None, 20);
    drop(app.apply_changed_edge(
        9,
        &ModelEdit::AgentsChanged,
        EdgeChanges {
            views: absent,
            ..Default::default()
        },
    ));
    baseline.sequence = 18;
    baseline.views = vec![at(18)];
    assert!(app.install_workspace_model_snapshot(baseline));
    assert!(
        app.workspace_model.views.is_empty(),
        "a later full snapshot can still predate an already observed absence"
    );
}

#[test]
fn newer_scoped_presence_preserves_ancestors_across_an_older_reconnect_absence() {
    use crate::workspace_model::views::{Scope, View};
    let (mut app, _, repository) = app_with_project();
    let channel = ChannelId::new();
    let scope = Scope::Channel(channel);
    let mut baseline = registry(vec![shared(channel, &repository, "Work", 0)]);
    baseline.sequence = 4;
    baseline.projects = vec![ModelProject {
        repository: repository.clone(),
        name: "repo".into(),
        kind: ProjectKind::Git,
        ordinal: 0,
    }];
    baseline.views = vec![View {
        revision: 4,
        ..View::empty(scope.clone())
    }];
    assert!(app.install_workspace_model_snapshot(baseline.clone()));
    let mut mirror = Mirror::new(&baseline.views);
    drop(app.apply_changed_edge(
        5,
        &ModelEdit::AgentsChanged,
        EdgeChanges {
            views: mirror.install_at(
                scope.clone(),
                Some(View {
                    revision: 10,
                    ..View::empty(scope.clone())
                }),
                10,
            ),
            ..Default::default()
        },
    ));
    baseline.sequence = 8;
    baseline.projects.clear();
    baseline.channels.clear();
    baseline.views.clear();
    baseline.closed_projects.push(repository.clone());
    assert!(app.install_workspace_model_snapshot(baseline.clone()));
    assert!(app.workspace_model.project(&repository).is_some());
    assert!(app.workspace_model.channel(channel).is_some());
    assert_eq!(app.workspace_model.views.len(), 1);
    assert!(
        !app.workspace_model.closed_projects.contains(&repository),
        "positive scope evidence newer than the reconnect baseline also fences its old ancestor close"
    );
    drop(app.apply_changed_edge(
        9,
        &ModelEdit::AgentsChanged,
        EdgeChanges {
            views: mirror.install_at(scope, None, 20),
            ..Default::default()
        },
    ));
    baseline.sequence = 18;
    assert!(app.install_workspace_model_snapshot(baseline));
    assert!(app.workspace_model.project(&repository).is_none());
    assert!(app.workspace_model.channel(channel).is_none());
    assert!(
        app.workspace_model.views.is_empty(),
        "a newer absence alone never protects old parent metadata"
    );
}

#[test]
fn review2789_ahead_scope_must_not_discard_reconnect_metadata() {
    use crate::model::{BrowserId, BrowserRestoreFidelity, BrowserSession};
    use crate::workspace_model::views::{Pane, Scope, View};
    let (mut app, _, repository) = app_with_project();
    let channel = ChannelId::new();
    let scope = Scope::Channel(channel);
    let pane = Pane {
        id: CardId::new(),
        title: "Browser".into(),
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
            fidelity: BrowserRestoreFidelity::UrlOnly,
        }),
    };
    let mut baseline = registry(vec![shared(channel, &repository, "Old channel", 0)]);
    baseline.sequence = 4;
    baseline.projects = vec![ModelProject {
        repository: repository.clone(),
        name: "Old project".into(),
        kind: ProjectKind::Git,
        ordinal: 0,
    }];
    baseline.views = vec![View {
        revision: 4,
        panes: vec![pane.clone()],
        ..View::empty(scope.clone())
    }];
    assert!(app.install_workspace_model_snapshot(baseline.clone()));

    // A publisher's older receipt resolves its scope at sequence 10.
    // The observer's reconnect baseline was captured at 8, but arrives later.
    let mut mirror = Mirror::for_model(&baseline);
    let mut resized = pane;
    resized.width_fraction = 0.7;
    let ahead = mirror.install_at(
        scope.clone(),
        Some(View {
            revision: 9,
            panes: vec![resized],
            ..View::empty(scope.clone())
        }),
        10,
    );
    drop(app.install_changed_edge(
        5,
        &ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![],
            closed: vec![],
        },
        EdgeChanges {
            views: ahead,
            ..Default::default()
        },
    ));
    // Peer metadata edits at 6 and 7 are recovered by this baseline.
    baseline.sequence = 8;
    baseline.projects[0].name = "New project".into();
    baseline.projects[0].ordinal = 2;
    baseline.channels[0].title = "New channel".into();
    baseline.channels[0].ordinal = 3;
    baseline.views[0].revision = 8;
    baseline.views[0].panes[0].width_fraction = 0.6;
    assert!(app.install_workspace_model_snapshot(baseline));
    assert_eq!(app.workspace_model.views[0].revision, 9);
    assert_eq!(app.workspace_model.views[0].panes[0].width_fraction, 0.7);
    assert_eq!(app.shared_views.snapshot_sequence, 8);
    assert_eq!(
        (
            app.workspace_model.channels[0].title.as_str(),
            app.workspace_model.projects[0].name.as_str(),
        ),
        ("New channel", "New project"),
        "a newer view proves ancestor presence, not the freshness of cached ancestor fields"
    );
    assert_eq!(app.workspace_model.projects[0].ordinal, 2);
    assert_eq!(app.workspace_model.channels[0].ordinal, 3);
}

#[test]
fn reconnect_project_metadata_uses_its_own_clock_despite_a_newer_child() {
    for project_sequence in [None, Some(7), Some(8), Some(9)] {
        for baseline_has_project in [false, true] {
            let (mut app, _, repository) = app_with_project();
            let channel = ChannelId::new();
            let mut baseline = registry(vec![shared(channel, &repository, "Old channel", 0)]);
            baseline.sequence = 4;
            baseline.projects.push(ModelProject {
                repository: repository.clone(),
                name: "Old project".into(),
                kind: ProjectKind::Git,
                ordinal: 0,
            });
            assert!(app.install_workspace_model_snapshot(baseline.clone()));
            if let Some(sequence) = project_sequence {
                let mut project = baseline.projects[0].clone();
                project.name = "Observed project".into();
                project.ordinal = 2;
                drop(app.install_changed_edge(
                    sequence,
                    &ModelEdit::OpenProject {
                        repository: repository.clone(),
                        name: project.name.clone(),
                        kind: project.kind,
                    },
                    EdgeChanges {
                        effects: EditEffects {
                            projects: vec![project],
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                ));
            }
            let child = shared(channel, &repository, "New child title", 3);
            drop(app.install_changed_edge(
                10,
                &ModelEdit::RetitleChannel {
                    id: channel,
                    title: child.title.clone(),
                },
                EdgeChanges {
                    effects: EditEffects {
                        channels: vec![child.clone()],
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ));
            let cached = app.workspace_model.projects[0].clone();
            baseline.sequence = 8;
            baseline.projects[0].name = "Recovered project".into();
            baseline.projects[0].ordinal = 1;
            let recovered = baseline.projects[0].clone();
            if !baseline_has_project {
                baseline.projects.clear();
                baseline.closed_projects.push(repository.clone());
            }
            assert!(app.install_workspace_model_snapshot(baseline));
            let expected =
                if !baseline_has_project || project_sequence.is_some_and(|sequence| sequence > 8) {
                    cached
                } else {
                    recovered
                };
            assert_eq!(app.workspace_model.projects, vec![expected]);
            assert_eq!(app.workspace_model.channels, vec![child]);
            assert!(app.workspace_model.closed_projects.is_empty());
            assert_eq!(app.shared_views.snapshot_sequence, 8);
        }
    }
}

#[test]
fn newer_receipt_does_not_discard_an_unrelated_earlier_observer_edge() {
    let (mut app, _, repository) = app_with_project();
    let first = ChannelId::new();
    let second = ChannelId::new();
    let mut baseline = registry(vec![
        shared(first, &repository, "First", 0),
        shared(second, &repository, "Second", 1),
    ]);
    baseline.sequence = 100;
    assert!(app.install_workspace_model_snapshot(baseline.clone()));
    let mut newer = baseline.channels[0].clone();
    newer.title = "First at 120".into();
    drop(app.apply_workspace_model_update(ModelUpdate::Committed {
        receipt: EditReceipt {
            identity: baseline.identity,
            sequence: 120,
            effects: EditEffects {
                channels: vec![newer.clone()],
                ..Default::default()
            },
        },
        edit: ModelEdit::RetitleChannel {
            id: first,
            title: newer.title.clone(),
        },
        views: ViewChanges::default(),
    }));
    let mut earlier = baseline.channels[1].clone();
    earlier.title = "Second at 110".into();
    drop(app.apply_changed_edge(
        110,
        &ModelEdit::RetitleChannel {
            id: second,
            title: earlier.title.clone(),
        },
        EdgeChanges {
            effects: EditEffects {
                channels: vec![earlier.clone()],
                ..Default::default()
            },
            ..Default::default()
        },
    ));
    assert_eq!(app.workspace_model.channel(first), Some(&newer));
    assert_eq!(app.workspace_model.channel(second), Some(&earlier));
    assert_eq!(
        app.shared_views.snapshot_sequence, 100,
        "a partial receipt is never a full snapshot watermark"
    );
    assert!(app.install_workspace_model_snapshot(baseline));
    assert_eq!(app.workspace_model.channel(first), Some(&newer));
    assert_eq!(app.workspace_model.channel(second), Some(&earlier));
}

#[test]
fn closed_identity_survives_deleted_scope_reconnect_until_registry_changes() {
    use crate::workspace_model::views::{Scope, View};
    let (mut app, _, repository) = app_with_project();
    let channel = ChannelId::new();
    let card = CardId::new();
    let scope = Scope::Channel(channel);
    let mut baseline = registry(vec![shared(channel, &repository, "Work", 0)]);
    baseline.sequence = 4;
    baseline.views = vec![View {
        revision: 3,
        ..View::empty(scope.clone())
    }];
    drop(
        app.apply_workspace_model_update(ModelUpdate::ConnectedClosed {
            model: baseline.clone(),
            closed: vec![],
        }),
    );
    drop(app.apply_changed_edge(
        5,
        &ModelEdit::AgentsChanged,
        EdgeChanges {
            views: ViewChanges {
                sequence: 5,
                removed: vec![(scope, 3)],
                ..Default::default()
            },
            effects: EditEffects {
                closed: vec![card],
                ..Default::default()
            },
            ..Default::default()
        },
    ));
    baseline.sequence = 6;
    baseline.views.clear();
    // Cascading the scope deletes its retired rows; the live window keeps
    // the already observed close across a connection loss.
    drop(
        app.apply_workspace_model_update(ModelUpdate::ConnectedClosed {
            model: baseline.clone(),
            closed: vec![],
        }),
    );
    assert!(app.shared_views.closed.contains(&card));
    assert!(app.shared_chat_creation_revoked(card, None));
    baseline.identity = uuid::Uuid::new_v4();
    drop(
        app.apply_workspace_model_update(ModelUpdate::ConnectedClosed {
            model: baseline,
            closed: vec![],
        }),
    );
    assert!(
        !app.shared_views.closed.contains(&card),
        "closed identities belong to one registry"
    );
}

#[test]
fn absent_close_survives_scope_deletion_and_blocks_a_delayed_agent_arrival() {
    use crate::workspace_model::{
        delta::ViewRevision,
        views::{Change, Scope, View},
    };
    for seed_closed in [false, true] {
        let (mut app, mut agent, pane, destination) = managed_display_fixture();
        let source = agent.placement.clone();
        let Scope::Channel(source_channel) = source.clone() else {
            unreachable!()
        };
        agent.placement = Scope::Channel(destination);
        agent.spec.target.placement = Some(agent.placement.clone());
        let identity = app.workspace_model.identity;
        let mut baseline = app.workspace_model.clone();
        baseline.sequence = 10;
        baseline.agents.clear();
        for view in &mut baseline.views {
            view.revision = 10;
            view.panes.clear();
            view.retired.clear();
        }
        let (sent, requests) = test_model_channel();
        app.workspace_model_tx = sent;
        drop(
            app.apply_workspace_model_update(ModelUpdate::ConnectedClosed {
                model: baseline,
                closed: vec![],
            }),
        );
        assert!(!app.durable.cards.iter().any(|card| card.id == pane.id));
        let edit = if seed_closed {
            ModelEdit::SeedView {
                scope: source.clone(),
                panes: vec![],
                closed: vec![pane.id],
            }
        } else {
            ModelEdit::ChangeView {
                scope: source.clone(),
                expected_revision: 10,
                operation: uuid::Uuid::new_v4(),
                change: Change::Remove { id: pane.id },
            }
        };
        drop(app.apply_workspace_model_update(ModelUpdate::Changed {
            sequence: 11,
            edit,
            changes: EdgeChanges {
                identity: Some(identity),
                views: ViewChanges {
                    sequence: 11,
                    views: vec![View {
                        scope: source.clone(),
                        revision: 11,
                        panes: vec![],
                        retired: vec![pane.id],
                    }],
                    ..Default::default()
                },
                effects: EditEffects {
                    views: vec![ViewRevision {
                        scope: source.clone(),
                        before: Some(10),
                        after: Some(11),
                    }],
                    closed: vec![pane.id],
                    ..Default::default()
                },
            },
        }));
        drop(app.apply_workspace_model_update(ModelUpdate::Changed {
            sequence: 12,
            edit: ModelEdit::DeleteChannel { id: source_channel },
            changes: EdgeChanges {
                identity: Some(identity),
                views: ViewChanges {
                    sequence: 12,
                    removed: vec![(source.clone(), 11)],
                    ..Default::default()
                },
                effects: EditEffects {
                    views: vec![ViewRevision {
                        scope: source.clone(),
                        before: Some(11),
                        after: None,
                    }],
                    removed_channels: vec![source_channel],
                    ..Default::default()
                },
            },
        }));
        assert!(
            !app.workspace_model
                .views
                .iter()
                .any(|view| view.retired.contains(&pane.id)),
            "scope deletion erased the only view tombstone"
        );
        drop(app.apply_workspace_model_update(ModelUpdate::Changed {
            sequence: 13,
            edit: ModelEdit::AgentsChanged,
            changes: EdgeChanges {
                identity: Some(identity),
                effects: EditEffects {
                    agents: vec![agent.clone()],
                    ..Default::default()
                },
                ..Default::default()
            },
        }));
        assert!(
            crate::workspace_model::views::managed_display(&app.workspace_model, &agent).is_some(),
            "without the retained closed-identity fence, this otherwise valid delayed agent would be adopted"
        );
        assert_eq!(app.workspace_model.agents, vec![agent]);
        assert!(
            app.shared_views.closed.contains(&pane.id),
            "the close is retained without a reconnect after its scope disappears"
        );
        assert!(
            !app.durable.cards.iter().any(|card| card.id == pane.id),
            "a delayed agent row must not adopt the explicitly closed pane in another channel"
        );
        assert!(
            !app.cards.contains_key(&pane.id),
            "no runtime or attachment may be admitted for that pane"
        );
        assert!(
            !std::iter::from_fn(|| requests.try_recv().ok()).any(|edit| match edit {
                ModelEdit::SeedView { panes, .. } =>
                    panes.iter().any(|candidate| candidate.id == pane.id),
                ModelEdit::ChangeView {
                    change:
                        Change::Insert {
                            pane: candidate, ..
                        }
                        | Change::InsertIfEmpty { pane: candidate },
                    ..
                } => candidate.id == pane.id,
                _ => false,
            }),
            "the delayed agent must not publish a replacement membership"
        );
    }
}

#[test]
fn baseline_closed_identities_fence_adoption_before_reconciliation() {
    use crate::workspace_model::views::Scope;
    let (mut app, agent, pane, destination) = managed_display_fixture();
    let mut replacement = app.workspace_model.clone();
    replacement.identity = uuid::Uuid::new_v4();
    replacement.sequence = 100;
    assert!(!app.durable.cards.iter().any(|card| card.id == pane.id));
    drop(
        app.apply_workspace_model_update(ModelUpdate::ConnectedClosed {
            model: replacement,
            closed: vec![pane.id],
        }),
    );
    assert!(app.shared_views.closed.contains(&pane.id));
    assert!(
        !app.durable.cards.iter().any(|card| card.id == pane.id),
        "closed identity must fence both managed and ordinary projection before adoption"
    );
    assert!(!app.cards.contains_key(&pane.id));
    assert!(app.shared_chat_creation_revoked(pane.id, Some(&Scope::Channel(destination))));
    assert!(app.adopt_execution_agent(&agent).is_none());
}

#[test]
fn newer_live_children_fence_old_ancestor_closes_but_absence_does_not() {
    use crate::workspace_model::views::{Scope, View};
    let (mut app, _, repository) = app_with_project();
    let channel = ChannelId::new();
    let scope = Scope::Channel(channel);
    let mut baseline = registry(vec![shared(channel, &repository, "Work", 0)]);
    baseline.sequence = 100;
    baseline.projects.push(ModelProject {
        repository: repository.clone(),
        name: "repo".into(),
        kind: ProjectKind::Git,
        ordinal: 0,
    });
    baseline.views.push(View {
        revision: 100,
        ..View::empty(scope.clone())
    });
    assert!(app.install_workspace_model_snapshot(baseline));
    drop(app.apply_changed_edge(
        120,
        &ModelEdit::AgentsChanged,
        EdgeChanges {
            views: ViewChanges {
                sequence: 120,
                views: vec![View {
                    revision: 120,
                    ..View::empty(scope.clone())
                }],
                ..Default::default()
            },
            ..Default::default()
        },
    ));
    drop(app.apply_changed_edge(
        110,
        &ModelEdit::CloseProject {
            repository: repository.clone(),
        },
        EdgeChanges {
            effects: EditEffects {
                removed_projects: vec![repository.clone()],
                removed_channels: vec![channel],
                ..Default::default()
            },
            ..Default::default()
        },
    ));
    assert!(
        app.workspace_model.project(&repository).is_some(),
        "a newer live scope proves an older close was followed by reopening"
    );
    assert!(app.workspace_model.channel(channel).is_some());
    // A later empty-scope read, unlike live scope evidence, does not imply
    // reopening and cannot keep a correctly closed project alive.
    app.install_edge_views(
        ViewChanges {
            absent: vec![(scope, 130)],
            ..Default::default()
        },
        true,
    );
    drop(app.apply_changed_edge(
        125,
        &ModelEdit::CloseProject {
            repository: repository.clone(),
        },
        EdgeChanges {
            effects: EditEffects {
                removed_projects: vec![repository.clone()],
                removed_channels: vec![channel],
                ..Default::default()
            },
            ..Default::default()
        },
    ));
    assert!(app.workspace_model.project(&repository).is_none());
    assert!(app.workspace_model.channel(channel).is_none());
}

#[test]
fn project_close_fences_delayed_navigation_even_after_reopen() {
    use crate::workspace_model::{NavigationItem, NavigationOrder};
    for close_before_reopen in [false, true] {
        let (mut app, _, repository) = app_with_project();
        let project = ModelProject {
            repository: repository.clone(),
            name: "repo".into(),
            kind: ProjectKind::Git,
            ordinal: 0,
        };
        let mut baseline = registry(Vec::new());
        baseline.sequence = 10;
        baseline.projects.push(project.clone());
        assert!(app.install_workspace_model_snapshot(baseline));
        let reopened = EdgeChanges {
            effects: EditEffects {
                projects: vec![project],
                ..Default::default()
            },
            ..Default::default()
        };
        let closed = EdgeChanges {
            effects: EditEffects {
                removed_projects: vec![repository.clone()],
                ..Default::default()
            },
            ..Default::default()
        };
        let reopen_edit = ModelEdit::ReopenProject {
            repository: repository.clone(),
            name: "repo".into(),
            kind: ProjectKind::Git,
        };
        let close_edit = ModelEdit::CloseProject {
            repository: repository.clone(),
        };
        if close_before_reopen {
            drop(app.apply_changed_edge(12, &close_edit, closed));
            drop(app.apply_changed_edge(13, &reopen_edit, reopened));
        } else {
            drop(app.apply_changed_edge(13, &reopen_edit, reopened));
            drop(app.apply_changed_edge(12, &close_edit, closed));
        }
        let order = NavigationOrder {
            repository: repository.clone(),
            items: vec![NavigationItem::Worktree("/repo/worktree".into())],
        };
        drop(app.apply_changed_edge(
            11,
            &ModelEdit::ResetNavigation {
                repository: repository.clone(),
            },
            EdgeChanges {
                effects: EditEffects {
                    navigation_orders: vec![order],
                    ..Default::default()
                },
                ..Default::default()
            },
        ));
        assert!(app.workspace_model.project(&repository).is_some());
        assert!(
            app.workspace_model.navigation_orders.is_empty(),
            "a close's implicit order deletion fences a delayed write even when parent reopening overtook it"
        );
    }
}

#[test]
fn publisher_waits_for_observer_identity_and_acknowledges_every_baseline() {
    let (mut app, _, repository) = app_with_project();
    let channel = ChannelId::new();
    let old = registry(vec![shared(channel, &repository, "Old", 0)]);
    drop(
        app.apply_workspace_model_update(ModelUpdate::ConnectedClosed {
            model: old.clone(),
            closed: vec![],
        }),
    );
    let mut replacement = old.clone();
    replacement.identity = uuid::Uuid::new_v4();
    replacement.channels[0].title = "Replacement".into();
    let (ready, waiting) = std::sync::mpsc::sync_channel(1);
    drop(
        app.apply_workspace_model_update(ModelUpdate::PublisherConnected {
            identity: replacement.identity,
            ready,
        }),
    );
    assert!(
        !waiting.recv().unwrap(),
        "no edit can be drained before the observer establishes the registry"
    );
    assert_eq!(app.workspace_model.identity, old.identity);
    drop(
        app.apply_workspace_model_update(ModelUpdate::ConnectedClosed {
            model: replacement.clone(),
            closed: vec![],
        }),
    );
    let (ready, waiting) = std::sync::mpsc::sync_channel(1);
    let mut older = replacement.clone();
    older.sequence = 0;
    drop(
        app.apply_workspace_model_update(ModelUpdate::PublisherConnected {
            identity: older.identity,
            ready,
        }),
    );
    assert!(
        waiting.recv().unwrap(),
        "same-registry readiness is acknowledged even when its stale snapshot is ignored"
    );
    let expected = app.workspace_model.clone();
    let (ready, waiting) = std::sync::mpsc::sync_channel(1);
    drop(
        app.apply_workspace_model_update(ModelUpdate::PublisherConnected {
            identity: old.identity,
            ready,
        }),
    );
    assert!(!waiting.recv().unwrap());
    drop(app.apply_workspace_model_update(ModelUpdate::Committed {
        receipt: EditReceipt {
            identity: old.identity,
            sequence: 900,
            effects: EditEffects::default(),
        },
        edit: ModelEdit::AgentsChanged,
        views: ViewChanges::default(),
    }));
    assert_eq!(
        app.workspace_model, expected,
        "a receipt already in flight from the superseded registry cannot mutate the replacement"
    );
}

#[test]
fn delayed_observer_edge_cannot_mutate_a_replacement_registry() {
    use crate::workspace_model::views::{Scope, View};
    let (mut app, _, repository) = app_with_project();
    let channel = ChannelId::new();
    let card = CardId::new();
    let original = registry(vec![shared(channel, &repository, "Old registry", 0)]);
    drop(
        app.apply_workspace_model_update(ModelUpdate::ConnectedClosed {
            model: original.clone(),
            closed: vec![],
        }),
    );
    let mut replacement = original.clone();
    replacement.identity = uuid::Uuid::new_v4();
    replacement.channels[0].title = "Replacement registry".into();
    drop(
        app.apply_workspace_model_update(ModelUpdate::ConnectedClosed {
            model: replacement.clone(),
            closed: vec![],
        }),
    );
    let expected = app.workspace_model.clone();
    drop(app.apply_changed_edge(
        900,
        &ModelEdit::AgentsChanged,
        EdgeChanges {
            identity: Some(original.identity),
            views: ViewChanges {
                sequence: 900,
                views: vec![View {
                    revision: 900,
                    ..View::empty(Scope::Channel(channel))
                }],
                ..Default::default()
            },
            effects: EditEffects {
                channels: original.channels,
                closed: vec![card],
                ..Default::default()
            },
        },
    ));
    assert_eq!(app.workspace_model, expected);
    assert!(!app.shared_views.closed.contains(&card));
}

#[test]
fn headless_execution_adopts_exact_target_without_stealing_focus_or_resurrecting_tombstones() {
    use crate::agent::Provider;
    use crate::agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target};
    let (mut app, project, repository) = app_with_project();
    let active = ChannelId::new();
    let target = ChannelId::new();
    let focus = app.workspace.focused();
    let durable = Arc::make_mut(&mut app.durable);
    durable.channels.push(local(active, project, "Active", 0));
    durable
        .channels
        .push(local(target, project, "Execution", 1));
    durable.active_channel = Some(active);
    let session = crate::chat::Session {
        native_continuity: Default::default(),
        id: TerminalId::new(),
        proof: Some(crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("execution-host"),
            boot_id: crate::model::BootId::new("execution-boot"),
            host_incarnation: crate::model::HostIncarnation::new("execution-incarnation"),
        }),
        terminal: None,
        owned_codex_listener: None,
        launch: crate::chat::Launch {
            provider: Provider::Claude,
            session_id: None,
            recipe: LaunchRecipe {
                program: "claude".into(),
                arguments: Vec::new(),
                working_directory: PathBuf::from("/repo/worktree"),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            draft_model: None,
        },
    };
    let mut agent = Agent {
        name: None,
        id: crate::model::AgentId::new(),
        origin: Origin::User,
        spec: Spec {
            provider: Provider::Claude,
            account: None,
            target: Target {
                repository,
                directory: PathBuf::from("/repo/worktree"),
                placement: Some(crate::agent_execution::Placement::Channel(target)),
            },
            surface: Surface::Chat,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        },
        placement: crate::agent_execution::Placement::Channel(target),
        session: Some(HostedSession::Chat(session.clone())),
        console: None,
        created_ms: 0,
        closed: false,
    };
    let card = CardId::from_uuid(agent.id.as_uuid());
    let mut scheduled = agent.clone();
    scheduled.origin = Origin::Automation {
        id: uuid::Uuid::new_v4(),
    };
    app.workspace_model = registry(vec![
        shared(active, &agent.spec.target.repository, "Active", 0),
        shared(target, &agent.spec.target.repository, "Execution", 1),
    ]);
    app.workspace_model.agents.push(scheduled);
    drop(app.reconcile_workspace_model());
    assert!(
        !app.durable
            .cards
            .iter()
            .any(|candidate| candidate.id == card),
        "model refresh must not mount scheduled history or its live actor"
    );
    app.chrome_ui
        .agent_labels
        .insert(agent.id, "Agent task".into());
    assert!(app.adopt_execution_agent(&agent).is_some());
    assert!(
        app.chat_start_pending(card),
        "even an unnamed accepted conversation in a hidden channel attaches to its proven actor"
    );
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
    assert_eq!(app.durable.active_channel, Some(active));
    assert_eq!(app.workspace.focused(), focus);
    assert_eq!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == target)
            .unwrap()
            .cards,
        vec![card]
    );
    assert_eq!(
        app.durable.card_scopes.get(&card),
        Some(&PathBuf::from("/repo/worktree"))
    );
    assert!(
        matches!(&app.durable.cards.iter().find(|entry| entry.id == card).unwrap().kind, CardKind::Chat(actual) if actual == &session)
    );
    app.chrome_ui
        .agent_labels
        .insert(agent.id, "Agent task".into());
    assert!(app.adopt_execution_agent(&agent).is_none());
    Arc::make_mut(&mut app.durable)
        .cards
        .iter_mut()
        .find(|entry| entry.id == card)
        .unwrap()
        .existence = Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: 1,
            reason: Some("user closed".into()),
        },
    };
    app.chrome_ui
        .agent_labels
        .insert(agent.id, "Agent task".into());
    assert!(app.adopt_execution_agent(&agent).is_none());
    assert_eq!(
        app.durable
            .cards
            .iter()
            .filter(|entry| entry.id == card)
            .count(),
        1
    );
    agent.id = crate::model::AgentId::new();
    agent.spec.surface = Surface::Terminal;
    assert!(
        app.adopt_execution_agent(&agent).is_none(),
        "wait for the exact paired console descriptor"
    );
}

fn managed_display_fixture() -> (
    Cagents,
    crate::agent_execution::Agent,
    crate::workspace_model::views::Pane,
    ChannelId,
) {
    use crate::agent_execution::{Agent, HostedSession, Origin, Placement, Spec, Surface, Target};
    let (mut app, project, repository) = app_with_project();
    let original = ChannelId::new();
    let destination = ChannelId::new();
    let durable = Arc::make_mut(&mut app.durable);
    durable
        .channels
        .push(local(original, project, "Original", 0));
    durable
        .channels
        .push(local(destination, project, "Current", 1));
    let CardKind::Terminal(mut console) = super::super::tests::terminal_card_state().kind else {
        unreachable!()
    };
    console.launch.working_directory = "/repo".into();
    console.continuity = TerminalContinuity::Live {
        host_session_id: crate::model::HostSessionId::new("managed-display"),
        boot_id: crate::model::BootId::new("managed-display"),
        host_incarnation: crate::model::HostIncarnation::new("managed-display"),
    };
    let placement = Placement::Channel(original);
    let agent = Agent {
        id: crate::model::AgentId::new(),
        name: None,
        origin: Origin::User,
        spec: Spec {
            provider: crate::agent::Provider::Claude,
            account: None,
            target: Target {
                repository: repository.clone(),
                directory: "/repo".into(),
                placement: Some(placement.clone()),
            },
            surface: Surface::Terminal,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        },
        placement,
        session: Some(HostedSession::Terminal(console.clone())),
        console: Some(console.clone()),
        created_ms: 0,
        closed: false,
    };
    let pane = crate::workspace_model::views::Pane {
        id: CardId::from_uuid(agent.id.as_uuid()),
        title: "Current managed title".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.6,
        kind: CardKind::Terminal(console),
        scope: Some("/repo".into()),
    };
    app.workspace_model = registry(vec![
        shared(original, &repository, "Original", 0),
        shared(destination, &repository, "Current", 1),
    ]);
    let mut source = crate::workspace_model::views::View::empty(original);
    source.retired.push(pane.id);
    app.workspace_model.views = vec![
        source,
        crate::workspace_model::views::View {
            scope: Placement::Channel(destination),
            revision: 9,
            panes: vec![pane.clone()],
            retired: vec![],
        },
    ];
    app.workspace_model.agents.push(agent.clone());
    app.chrome_ui
        .agent_labels
        .insert(agent.id, pane.title.clone());
    (app, agent, pane, destination)
}

fn unloaded_chat_label_fixture() -> (
    Cagents,
    crate::agent_execution::Agent,
    crate::workspace_model::views::Pane,
    ChannelId,
) {
    use crate::agent_execution::{HostedSession, Surface};
    let (mut app, mut agent, mut pane, destination) = managed_display_fixture();
    let CardKind::Terminal(console) = pane.kind else {
        unreachable!()
    };
    let session = crate::chat::Session {
        id: console.id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: console.launch,
            session_id: None,
            draft_model: None,
        },
        proof: Some(crate::model::HostSessionProof {
            host_session_id: "labels-host".into(),
            boot_id: "labels-boot".into(),
            host_incarnation: "labels-owner".into(),
        }),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    agent.name = Some("reviewer".into());
    agent.spec.surface = Surface::Chat;
    agent.session = Some(HostedSession::Chat(session.clone()));
    agent.console = None;
    agent.placement = crate::agent_execution::Placement::Channel(destination);
    for channel in &mut Arc::make_mut(&mut app.durable).channels {
        if channel.id == destination {
            channel.title_source = crate::model::ChannelTitleSource::Prompt;
        }
    }
    for channel in &mut app.workspace_model.channels {
        if channel.id == destination {
            channel.title_source = crate::model::ChannelTitleSource::Prompt;
        }
    }
    app.workspace_model.agents = vec![agent.clone()];
    app.workspace_model.views.clear();
    app.chrome_ui.agent_labels.clear();
    pane.kind = CardKind::Chat(session);
    (app, agent, pane, destination)
}

/// A managed pane whose saved proof no longer matches its registry actor
/// is fenced by itself. Its scope still seeds and projects its other
/// panes, while the fenced pane keeps this window's card, presentation and
/// place and is never seeded, rewritten, moved or closed from here. With
/// `elsewhere`, the view placing it belongs to another channel while this
/// window still shows it here.
#[test]
fn stale_managed_pane_is_fenced_alone_while_its_scope_seeds_and_projects() {
    use crate::agent_execution::{Agent, HostedSession, Origin, Placement, Spec, Surface, Target};
    use crate::workspace_model::views::{Change, Pane, Scope, View};
    for ((closed, elsewhere), close_it) in [(false, false), (true, false), (false, true)]
        .into_iter()
        .flat_map(|variant| [(variant, false), (variant, true)])
    {
        let (mut app, project, repository) = app_with_project();
        let channel = ChannelId::new();
        let other = ChannelId::new();
        let scope = Scope::Channel(channel);
        let owner = Scope::Channel(if elsewhere { other } else { channel });
        let sibling = app.durable.cards[0].id;
        let CardKind::Terminal(mut registered) = super::super::tests::terminal_card_state().kind
        else {
            unreachable!()
        };
        registered.launch.working_directory = "/repo".into();
        let live = |boot: &str, incarnation: &str| TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("managed-host"),
            boot_id: crate::model::BootId::new(boot),
            host_incarnation: crate::model::HostIncarnation::new(incarnation),
        };
        registered.continuity = live("previous-boot", "managed-incarnation");
        let mut current = registered.clone();
        current.continuity = live("current-boot", "managed-incarnation");
        let mut partial = registered.clone();
        partial.continuity = live("current-boot", "another-incarnation");
        let mut agent = Agent {
            id: crate::model::AgentId::new(),
            name: None,
            origin: Origin::User,
            spec: Spec {
                provider: crate::agent::Provider::Claude,
                account: None,
                target: Target {
                    repository: repository.clone(),
                    directory: "/repo".into(),
                    placement: Some(Placement::Channel(channel)),
                },
                surface: Surface::Terminal,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: Placement::Channel(channel),
            session: Some(HostedSession::Terminal(registered.clone())),
            console: Some(registered.clone()),
            created_ms: 0,
            closed,
        };
        let managed = CardId::from_uuid(agent.id.as_uuid());
        let mut kept = super::super::tests::terminal_card_state();
        kept.id = managed;
        kept.title = "Kept local title".into();
        kept.width_fraction = 0.4;
        kept.kind = CardKind::Terminal(registered.clone());
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.push(kept.clone());
        let mut local_channel = local(channel, project, "Channel", 0);
        local_channel.cards = vec![sibling, managed];
        local_channel.focused = Some(sibling);
        durable.channels.push(local_channel);
        durable.channels.push(local(other, project, "Other", 1));
        durable.active_channel = Some(channel);
        app.chrome_ui
            .user_titles
            .insert(managed, "Kept user title".into());
        let canonical = Pane {
            id: managed,
            title: "Canonical title".into(),
            user_title: Some("Canonical user title".into()),
            pending_terminal_start: false,
            width_fraction: 0.8,
            kind: CardKind::Terminal(current.clone()),
            scope: Some("/repo".into()),
        };
        let mut model = registry(vec![
            shared(channel, &repository, "Channel", 0),
            shared(other, &repository, "Other", 1),
        ]);
        model.views.push(View {
            scope: owner.clone(),
            revision: 3,
            panes: vec![canonical.clone()],
            retired: vec![],
        });
        model.agents.push(agent.clone());
        let published = intercept_publishes(&mut app);
        drop(app.apply_workspace_model_update(ModelUpdate::Connected(model.clone())));
        assert!(app.shared_views.quarantined.contains(&managed));
        assert!(app.shared_views.blocked.is_empty());
        let edits = drain(&published);
        let seeds = edits
            .iter()
            .filter_map(|edit| match edit {
                ModelEdit::SeedView {
                    scope: seeded,
                    panes,
                    closed,
                } if *seeded == scope => Some((panes, closed)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(seeds.len(), 1, "the scope seeds despite its stale pane");
        assert!(seeds[0].0.iter().any(|pane| pane.id == sibling));
        assert!(!seeds[0].0.iter().any(|pane| pane.id == managed));
        assert!(!seeds[0].1.contains(&managed));
        assert!(!edits.iter().any(|edit| matches!(edit,
            ModelEdit::ChangeView { change, .. } if change_mentions(change, managed))));

        // The host appends the seeded sibling after the saved pane.
        let sibling_pane = Pane::from_card(app.card_state(sibling).unwrap(), None).unwrap();
        if elsewhere {
            model.views.push(View {
                scope: scope.clone(),
                revision: 1,
                panes: vec![sibling_pane],
                retired: vec![],
            });
        } else {
            model.views[0].panes.push(sibling_pane);
            model.views[0].revision += 1;
        }
        for seeded in [&scope, &Scope::Channel(other)] {
            model.sequence += 1;
            drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: seeded.clone(),
                operation: None,
                model: model.clone(),
            }));
        }
        let members = &app
            .durable
            .channels
            .iter()
            .find(|entry| entry.id == channel)
            .unwrap()
            .cards;
        // Canonical order where its view places it; its local place
        // otherwise, after the sibling it followed here.
        let order = if elsewhere {
            vec![sibling, managed]
        } else {
            vec![managed, sibling]
        };
        assert_eq!(members, &order);
        assert!(
            !app.durable
                .channels
                .iter()
                .any(|entry| entry.id == other && entry.cards.contains(&managed)),
            "a fenced pane is not also shown where its view places it"
        );
        assert!(app.shared_views.attached.contains(&sibling));
        assert!(!app.shared_views.attached.contains(&managed));
        let card = app.card_state(managed).unwrap();
        assert!(card.existence.is_open());
        assert_eq!(card.kind, kept.kind, "the stale descriptor is not adopted");
        assert_eq!(card.title, kept.title);
        assert_eq!(card.width_fraction, kept.width_fraction);
        assert_eq!(
            app.chrome_ui.user_titles.get(&managed).map(String::as_str),
            Some("Kept user title")
        );
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), order);
        assert!(
            app.persistent_workspace()
                .cards
                .iter()
                .any(|card| card.id == managed && card.kind == kept.kind),
            "the saved workspace keeps the fenced pane"
        );

        // Repeated snapshots, evidence matching only in part, and local
        // edits publish nothing for it.
        model.agents[0].session = Some(HostedSession::Terminal(partial.clone()));
        model.agents[0].console = Some(partial.clone());
        for _ in 0..2 {
            model.sequence += 1;
            drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
            assert!(app.shared_views.quarantined.contains(&managed));
        }
        app.card_state_mut(managed).unwrap().width_fraction = 0.9;
        app.publish_view_changes();
        // Deleting its channel would ask the daemon to close only the
        // sibling's session, never the fenced pane's unconfirmed one.
        assert_eq!(
            app.channel_sessions_to_close(&[sibling, managed]),
            vec![app.card_state(sibling).unwrap().kind.clone()]
        );
        let mentions = |edits: Vec<ModelEdit>| {
            assert!(!edits.iter().any(|edit| matches!(edit,
                ModelEdit::SeedView { panes, closed: removed, .. }
                    if removed.contains(&managed) || panes.iter().any(|pane| pane.id == managed))));
            edits
                .into_iter()
                .filter_map(|edit| match edit {
                    ModelEdit::ChangeView { scope, change, .. }
                        if change_mentions(&change, managed) =>
                    {
                        Some((scope, change))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        if close_it {
            // Closing it asks nothing of its agent. It leaves the view that
            // holds it, wherever this window showed it, and recently closed
            // can reopen it.
            drop(app.update(Message::CloseCard(managed)));
            assert!(app.terminal_close.is_none(), "no terminal is stopped");
            drop(app.update(Message::CommitClose(managed)));
            assert!(!app.card_state(managed).unwrap().existence.is_open());
            assert!(!app.workspace.order().any(|id| id == managed));
            assert!(app.card_state(sibling).unwrap().existence.is_open());
            assert!(
                app.recently_closed
                    .iter()
                    .any(|(card, _)| card.id == managed)
            );
            assert_eq!(
                mentions(drain(&published)),
                vec![(owner.clone(), Change::Remove { id: managed })]
            );
            // Snapshots that still hold the pane before the removal is
            // acknowledged neither reopen it nor remove it again.
            model.sequence += 1;
            drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
            app.publish_view_changes();
            assert!(!app.card_state(managed).unwrap().existence.is_open());
            assert!(mentions(drain(&published)).is_empty());
        }

        // Matching registry evidence ends the fence. An open fenced pane
        // takes the canonical descriptor; a closed one stays closed.
        agent.session = Some(HostedSession::Terminal(current.clone()));
        agent.console = Some(current.clone());
        model.agents = vec![agent.clone()];
        model.sequence += 1;
        drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
        app.publish_view_changes();
        assert!(app.shared_views.quarantined.is_empty());
        assert!(mentions(drain(&published)).is_empty());
        let card = app.card_state(managed).unwrap();
        if close_it {
            assert!(!card.existence.is_open(), "an explicit close is final");
            assert!(!app.workspace.order().any(|id| id == managed));
            continue;
        }
        assert!(card.existence.is_open());
        assert_eq!(card.kind, canonical.kind);
        assert_eq!(card.title, canonical.title);
        assert_eq!(
            card.width_fraction, canonical.width_fraction,
            "an edit made while fenced was never shared; the view's own wins"
        );
        let in_channel = |id: ChannelId| {
            app.durable
                .channels
                .iter()
                .find(|entry| entry.id == id)
                .unwrap()
                .cards
                .contains(&managed)
        };
        assert_eq!(in_channel(channel), !elsewhere);
        assert_eq!(in_channel(other), elsewhere);
        assert_eq!(
            app.chrome_ui.user_titles.get(&managed).map(String::as_str),
            Some("Canonical user title")
        );
        assert!(app.shared_views.attached.contains(&managed));
    }
}

/// A managed terminal can fail to attach before a snapshot fences it, as
/// when the daemon is replaced while the app runs. Neither its body nor
/// any Retry route may reattach or relaunch its stale session.
#[test]
fn fenced_terminal_whose_attachment_failed_cannot_be_retried() {
    use crate::agent_execution::{Agent, HostedSession, Origin, Placement, Spec, Surface, Target};
    use crate::workspace_model::views::{Pane, Scope, View};
    let (mut app, project, repository) = app_with_project();
    let channel = ChannelId::new();
    let sibling = app.durable.cards[0].id;
    let CardKind::Terminal(mut registered) = super::super::tests::terminal_card_state().kind else {
        unreachable!()
    };
    registered.launch.working_directory = "/repo".into();
    let live = |boot: &str| TerminalContinuity::Live {
        host_session_id: crate::model::HostSessionId::new("managed-host"),
        boot_id: crate::model::BootId::new(boot),
        host_incarnation: crate::model::HostIncarnation::new("managed-incarnation"),
    };
    registered.continuity = live("previous-boot");
    let mut current = registered.clone();
    current.continuity = live("current-boot");
    let agent = Agent {
        id: crate::model::AgentId::new(),
        name: None,
        origin: Origin::User,
        spec: Spec {
            provider: crate::agent::Provider::Claude,
            account: None,
            target: Target {
                repository: repository.clone(),
                directory: "/repo".into(),
                placement: Some(Placement::Channel(channel)),
            },
            surface: Surface::Terminal,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        },
        placement: Placement::Channel(channel),
        session: Some(HostedSession::Terminal(registered.clone())),
        console: Some(registered.clone()),
        created_ms: 0,
        closed: false,
    };
    let managed = CardId::from_uuid(agent.id.as_uuid());
    let mut kept = super::super::tests::terminal_card_state();
    kept.id = managed;
    kept.kind = CardKind::Terminal(registered);
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.push(kept);
    let mut local_channel = local(channel, project, "Channel", 0);
    local_channel.cards = vec![sibling, managed];
    local_channel.focused = Some(managed);
    durable.channels.push(local_channel);
    durable.active_channel = Some(channel);
    let mut model = registry(vec![shared(channel, &repository, "Channel", 0)]);
    model.views.push(View {
        scope: Scope::Channel(channel),
        revision: 3,
        panes: vec![Pane {
            id: managed,
            title: "Canonical title".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.8,
            kind: CardKind::Terminal(current),
            scope: Some("/repo".into()),
        }],
        retired: vec![],
    });
    model.agents.push(agent);
    let _published = intercept_publishes(&mut app);
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(model)));
    assert!(app.shared_views.quarantined.contains(&managed));

    app.cards
        .insert(managed, CardRuntime::Failed("connection lost".into()));
    drop(app.update(Message::RetryCard(managed)));
    assert!(
        matches!(app.cards.get(&managed), Some(CardRuntime::Failed(_))),
        "Retry must leave a fenced pane as it is"
    );
    assert!(!app.terminal_start_in_flight(managed));
    assert!(!app.terminal_retry_pending(managed));
}

#[test]
fn unloaded_spawn_label_is_stable_across_opening_and_all_chrome_surfaces() {
    use crate::agent_execution::agent_label;
    let (mut app, agent, pane, destination) = unloaded_chat_label_fixture();
    assert!(
        app.adopt_execution_agent(&agent).is_none(),
        "unloaded agent waits for its accepted label"
    );
    assert!(app.card_state(pane.id).is_none());
    let label = agent_label(
        "reviewer",
        Some("Review the one-row cards\nKeep details available"),
    );
    // A publisher receipt can overtake the observer's label read. It
    // installs the canonical agent without exposing a provisional pane.
    app.workspace_model.agents.clear();
    let sequence = app.workspace_model.sequence + 1;
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Committed {
            receipt: EditReceipt {
                identity: app.workspace_model.identity,
                sequence,
                effects: EditEffects {
                    agents: vec![agent.clone()],
                    ..Default::default()
                },
            },
            edit: ModelEdit::AgentsChanged,
            views: ViewChanges::default(),
        })),
    );
    assert_eq!(app.workspace_model.sequence, sequence);
    assert!(
        app.workspace_model
            .agents
            .iter()
            .any(|held| held.id == agent.id)
    );
    assert!(app.card_state(pane.id).is_none());
    // Metadata alone retries adoption, even though the canonical edge is
    // already installed and the model sequence does not advance again.
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::AgentLabels(
            vec![(agent.id, label.clone())],
        ))),
    );
    assert_eq!(app.workspace_model.sequence, sequence);
    assert!(
        app.chat_start_pending(pane.id),
        "the conversation is still unloaded"
    );
    assert_eq!(
        app.card_display_title(app.card_state(pane.id).unwrap()),
        label
    );
    // A restored source descriptor can still carry the old provider
    // default. Accepted identity wins without loading the conversation.
    app.card_state_mut(pane.id).unwrap().title = crate::agent::Provider::Claude.chat_title().into();
    assert_eq!(
        app.card_display_title(app.card_state(pane.id).unwrap()),
        label
    );
    drop(app.sync_chrome());
    assert_eq!(
        app.card_chrome[&pane.id].title, label,
        "tabs and overflow use shared chrome"
    );
    drop(app.select_channel(destination));
    drop(app.focus_workspace_card(pane.id));
    drop(app.sync_window_title());
    assert!(
        app.core.window.header_title.contains(&label),
        "window title follows the same label"
    );
    assert!(
        !app.apply_reported_title(pane.id, "Review the one-row cards".into()),
        "opening may not replace spawn identity"
    );
    assert_eq!(
        app.card_display_title(app.card_state(pane.id).unwrap()),
        label
    );
    app.chrome_ui
        .user_titles
        .insert(pane.id, "My reviewer".into());
    assert_eq!(
        app.card_display_title(app.card_state(pane.id).unwrap()),
        "My reviewer"
    );
    app.chrome_ui.user_titles.remove(&pane.id);
    let CardKind::Chat(current) = &mut app.card_state_mut(pane.id).unwrap().kind else {
        unreachable!()
    };
    current.id = TerminalId::new();
    assert!(app.apply_reported_title(pane.id, "Replacement conversation".into()));
    assert_eq!(
        app.card_display_title(app.card_state(pane.id).unwrap()),
        "Replacement conversation"
    );
}

#[test]
fn unloaded_spawn_label_reaches_the_sidebar_attention_row_without_a_model_change() {
    use crate::agent_execution::{Placement, agent_label};
    let (mut app, mut agent, mut pane, _) = unloaded_chat_label_fixture();
    let project = app.sidebar_model.projects[0].id;
    let worktree = crate::model::WorktreeId::new();
    let path = PathBuf::from("/repo");
    app.sidebar_model.projects[0]
        .worktrees
        .push(crate::sidebar::SidebarWorktree::from_durable(
            worktree,
            path.clone(),
            Some("main".into()),
        ));
    let placement = Placement::Worktree {
        repository: agent.spec.target.repository.clone(),
        path: path.clone(),
    };
    agent.placement = placement.clone();
    agent.spec.target.placement = Some(placement.clone());
    pane.title = crate::agent::Provider::Claude.chat_title().into();
    pane.scope = Some(path.clone());
    app.workspace_model.channels.clear();
    app.workspace_model.views = vec![crate::workspace_model::views::View {
        scope: placement,
        revision: 9,
        panes: vec![pane.clone()],
        retired: vec![],
    }];
    app.workspace_model.agents = vec![agent.clone()];
    Arc::make_mut(&mut app.durable).channels.clear();
    let label = agent_label("reviewer", Some("Review the one-row cards"));
    app.chrome_ui.agent_labels.insert(agent.id, label.clone());
    assert!(app.adopt_execution_agent(&agent).is_some());
    Arc::make_mut(&mut app.durable)
        .card_scopes
        .insert(pane.id, path);
    app.sidebar_state
        .observe_agent_update(pane.id, project, worktree);
    drop(
        app.update(Message::Sidebar(crate::sidebar::SidebarAction::SetFilter(
            crate::sidebar::SidebarFilter::Attention,
        ))),
    );
    app.refresh_sidebar_channels();
    let row_title = |app: &Cagents| {
        app.sidebar_model
            .projects
            .iter()
            .flat_map(|project| &project.panes)
            .find(|row| row.id == pane.id)
            .expect("the unloaded unchanneled agent has an attention row")
            .title
            .clone()
    };
    assert!(app.chat_start_pending(pane.id));
    assert_eq!(row_title(&app), label);

    // A restored pane may arrive before accepted metadata. Its row must
    // refresh even when the label is the only new observation.
    app.chrome_ui.agent_labels.remove(&agent.id);
    app.card_state_mut(pane.id).unwrap().title = pane.title.clone();
    app.refresh_attention_card_rows();
    assert_eq!(row_title(&app), pane.title);
    let sequence = app.workspace_model.sequence;
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::AgentLabels(
            vec![(agent.id, label.clone())],
        ))),
    );
    assert_eq!(app.workspace_model.sequence, sequence);
    assert_eq!(row_title(&app), label);
    assert!(app.chat_start_pending(pane.id));
    assert!(!app.apply_reported_title(pane.id, "Review the one-row cards".into()));
    app.refresh_attention_card_rows();
    assert_eq!(row_title(&app), label);

    drop(app.update_chrome(super::super::chrome_ui::Action::Rename(pane.id)));
    drop(
        app.update_chrome(super::super::chrome_ui::Action::RenameChanged(
            "My reviewer".into(),
        )),
    );
    drop(app.update_chrome(super::super::chrome_ui::Action::RenameSubmit));
    assert_eq!(row_title(&app), "My reviewer");
    app.chrome_ui.user_titles.remove(&pane.id);
    let CardKind::Chat(current) = &mut app.card_state_mut(pane.id).unwrap().kind else {
        unreachable!()
    };
    current.id = TerminalId::new();
    assert!(app.apply_reported_title(pane.id, "Replacement conversation".into()));
    app.refresh_attention_card_rows();
    assert_eq!(row_title(&app), "Replacement conversation");
}

#[test]
fn attention_unpublished_managed_adoption_requires_complete_source_proof() {
    let (mut app, mut agent, _, _) = managed_display_fixture();
    app.workspace_model.views.clear();
    let crate::agent_execution::HostedSession::Terminal(source) = agent.session.as_mut().unwrap()
    else {
        panic!("terminal fixture");
    };
    source.continuity = TerminalContinuity::Live {
        host_session_id: crate::model::HostSessionId::new(""),
        boot_id: crate::model::BootId::new(""),
        host_incarnation: crate::model::HostIncarnation::new(""),
    };
    agent.console.as_mut().unwrap().continuity = source.continuity.clone();
    app.workspace_model.agents = vec![agent.clone()];
    let original = app.durable.clone();
    let count = app.cards.len();
    app.chrome_ui
        .agent_labels
        .insert(agent.id, "Agent task".into());
    assert!(app.adopt_execution_agent(&agent).is_none());
    assert!(Arc::ptr_eq(&original, &app.durable));
    assert_eq!(app.cards.len(), count);
    assert!(
        app.card_state(CardId::from_uuid(agent.id.as_uuid()))
            .is_none()
    );
}

#[test]
fn attention_first_model_sync_adopts_current_managed_membership_before_stale_placement() {
    for worktree in [false, true] {
        let (mut app, agent, pane, destination) = managed_display_fixture();
        let original = agent.channel().unwrap();
        if worktree {
            app.workspace_model.views[1].scope = crate::agent_execution::Placement::Worktree {
                repository: agent.spec.target.repository.clone(),
                path: "/repo".into(),
            };
        }
        let incoming = std::mem::take(&mut app.workspace_model);
        drop(app.apply_workspace_model_update(ModelUpdate::Connected(incoming)));
        let target = app
            .card_state(pane.id)
            .expect("first incoming snapshot adopted its exact managed pane");
        assert_eq!(target.kind, pane.kind);
        assert_eq!(target.title, pane.title);
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == original)
                .unwrap()
                .cards
                .is_empty()
        );
        if worktree {
            assert!(
                !app.durable
                    .channels
                    .iter()
                    .any(|channel| channel.cards.contains(&pane.id))
            );
            assert_eq!(
                app.durable.card_scopes.get(&pane.id),
                Some(&PathBuf::from("/repo"))
            );
        } else {
            assert!(
                app.durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == destination)
                    .unwrap()
                    .cards
                    .contains(&pane.id)
            );
        }
        assert_eq!(
            app.workspace_model.agents[0], agent,
            "display adoption preserves immutable launch and execution ownership"
        );
    }
}

#[test]
fn attention_first_model_sync_relocates_existing_exact_managed_runtime_without_rehosting() {
    let (mut app, agent, pane, destination) = managed_display_fixture();
    let mut original_only = app.workspace_model.clone();
    original_only.views.clear();
    let display = crate::workspace_model::views::managed_display(&original_only, &agent).unwrap();
    app.chrome_ui
        .agent_labels
        .insert(agent.id, "Agent task".into());
    assert!(app.adopt_execution_agent_display(&agent, display).is_some());
    let runtime = app.cards.remove(&pane.id).unwrap();
    app.cards.insert(pane.id, runtime);
    let actor = app.card_state(pane.id).unwrap().kind.clone();
    drop(app.reconcile_workspace_model());
    assert_eq!(app.card_state(pane.id).unwrap().kind, actor);
    assert!(matches!(
        app.cards.get(&pane.id),
        Some(CardRuntime::Loading)
    ));
    assert!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == destination)
            .unwrap()
            .cards
            .contains(&pane.id)
    );
    assert!(
        !app.durable
            .channels
            .iter()
            .find(|channel| channel.id == agent.channel().unwrap())
            .unwrap()
            .cards
            .contains(&pane.id)
    );
}

#[test]
fn repository_refresh_projects_managed_adoptions_outside_its_repository() {
    let (mut app, agent, pane, destination) = managed_display_fixture();
    let mut original_only = app.workspace_model.clone();
    original_only.views.clear();
    let display = crate::workspace_model::views::managed_display(&original_only, &agent).unwrap();
    app.chrome_ui
        .agent_labels
        .insert(agent.id, "Agent task".into());
    assert!(app.adopt_execution_agent_display(&agent, display).is_some());
    app.shared_views.initialize(
        crate::workspace_model::views::Scope::Channel(destination),
        vec![pane.clone()],
    );
    let actor = app.card_state(pane.id).unwrap().kind.clone();
    assert_ne!(app.card_state(pane.id).unwrap().title, pane.title);
    drop(app.reconcile_workspace_model_for_repository(Path::new("/unrelated")));
    let current = app.card_state(pane.id).unwrap();
    assert_eq!(current.kind, actor);
    assert_eq!(current.title, pane.title);
    assert_eq!(current.width_fraction, pane.width_fraction);
    assert!(
        app.durable
            .channels
            .iter()
            .any(|channel| channel.id == destination && channel.cards.contains(&pane.id))
    );
    assert!(matches!(
        app.cards.get(&pane.id),
        Some(CardRuntime::Loading)
    ));
}

#[test]
fn repository_refresh_relocates_managed_display_into_a_worktree_without_changing_its_source() {
    use crate::workspace_model::views::{Pane, Scope, View};
    let (mut app, agent, mut pane, unrelated) = managed_display_fixture();
    let mut original_only = app.workspace_model.clone();
    original_only.views.clear();
    let display = crate::workspace_model::views::managed_display(&original_only, &agent).unwrap();
    assert!(app.adopt_execution_agent_display(&agent, display).is_some());
    let source = app.card_state(pane.id).unwrap().kind.clone();
    let path = PathBuf::from("/repo/relocated-worktree");
    let scope = Scope::Worktree {
        repository: agent.spec.target.repository.clone(),
        path: path.clone(),
    };
    pane.scope = Some(path.clone());
    app.workspace_model.views[1].scope = scope.clone();
    app.workspace_model.views[1].panes = vec![pane.clone()];
    let sentinel = app.workspace.focused().unwrap();
    assert_ne!(sentinel, pane.id);
    let sentinel_pane = Pane::from_card(
        app.card_state(sentinel).unwrap(),
        app.durable.card_scopes.get(&sentinel).cloned(),
    )
    .unwrap();
    let mut unrelated_view = View::empty(unrelated);
    unrelated_view.panes.push(sentinel_pane.clone());
    app.workspace_model.views.push(unrelated_view);
    let channel = Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|channel| channel.id == unrelated)
        .unwrap();
    channel.cards = vec![sentinel];
    channel.focused = Some(sentinel);
    app.shared_views
        .initialize(Scope::Channel(unrelated), vec![sentinel_pane]);
    app.sidebar_model.projects[0]
        .worktrees
        .push(crate::sidebar::SidebarWorktree::from_durable(
            crate::model::WorktreeId::new(),
            path.clone(),
            Some("relocated".into()),
        ));
    app.known_worktree_roots.insert(path.clone());
    app.shared_views
        .initialize(scope.clone(), vec![pane.clone()]);
    Arc::make_mut(&mut app.durable).active_channel = Some(unrelated);
    let before = (
        app.workspace.focused(),
        app.workspace.camera().offset(),
        app.workspace.order().collect::<Vec<_>>(),
    );
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;

    drop(app.reconcile_workspace_model_for_repository(Path::new("/unrelated")));

    let current = app.card_state(pane.id).unwrap();
    assert_eq!(current.kind, source);
    assert_eq!(current.title, pane.title);
    assert_eq!(current.width_fraction, pane.width_fraction);
    assert_eq!(app.durable.card_scopes.get(&pane.id), Some(&path));
    assert_eq!(app.worktree_owners.get(&pane.id), Some(&path));
    assert!(
        app.durable
            .channels
            .iter()
            .all(|channel| !channel.cards.contains(&pane.id))
    );
    assert!(matches!(
        app.cards.get(&pane.id),
        Some(CardRuntime::Loading)
    ));
    assert_eq!(app.durable.active_channel, Some(unrelated));
    assert_eq!(
        (
            app.workspace.focused(),
            app.workspace.camera().offset(),
            app.workspace.order().collect::<Vec<_>>()
        ),
        before
    );
    assert!(!std::iter::from_fn(|| rx.try_recv().ok()).any(|edit|
        matches!(edit, ModelEdit::ChangeView { scope: edited, .. } | ModelEdit::SeedView { scope: edited, .. } if edited == scope)));
}

#[test]
fn observed_managed_terminal_exit_projects_without_live_adoption_or_relaunch() {
    for relaunch_available in [false, true] {
        let (mut app, agent, mut pane, destination) = managed_display_fixture();
        let CardKind::Terminal(session) = &mut pane.kind else {
            unreachable!()
        };
        let exit = crate::model::TerminalExit {
            code: Some(1),
            signal: None,
            occurred_at_unix_ms: 1,
        };
        session.continuity = if relaunch_available {
            TerminalContinuity::RelaunchAvailable {
                recipe: session.launch.clone(),
                last_exit: Some(exit),
            }
        } else {
            TerminalContinuity::Exited { last_exit: exit }
        };
        app.workspace_model.views[1].panes[0] = pane.clone();
        assert!(
            crate::workspace_model::views::managed_display(&app.workspace_model, &agent).is_none(),
            "an ended terminal grants no live adoption authority"
        );
        let incoming = std::mem::take(&mut app.workspace_model);
        drop(app.apply_workspace_model_update(ModelUpdate::Connected(incoming.clone())));
        // An ended pane cannot use the eager live-agent adoption path.
        // Its passive shared projection waits for the publisher's seed
        // acknowledgements, just as an ordinary saved terminal does.
        for view in &incoming.views {
            drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: view.scope.clone(),
                operation: None,
                model: incoming.clone(),
            }));
        }
        assert!(
            app.shared_views.quarantined.is_empty() && app.shared_views.blocked.is_empty(),
            "a normal console exit must not fence its pane or channel"
        );
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        assert!(matches!(
            app.cards.get(&pane.id),
            Some(CardRuntime::Completed)
        ));
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == destination)
                .unwrap()
                .cards
                .contains(&pane.id)
        );
        assert_eq!(app.workspace_model.agents[0], agent);
    }
}

#[test]
fn ended_managed_terminal_cannot_substitute_another_launch_recipe() {
    for changed in ["launch", "restart"] {
        let (mut app, _, _, _) = managed_display_fixture();
        let CardKind::Terminal(session) = &mut app.workspace_model.views[1].panes[0].kind else {
            unreachable!()
        };
        let mut recipe = session.launch.clone();
        if changed == "launch" {
            session.launch.working_directory = "/replacement".into();
            recipe = session.launch.clone();
        } else {
            recipe.program = "replacement".into();
        }
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: Some(crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            }),
        };
        let incoming = std::mem::take(&mut app.workspace_model);
        drop(app.apply_workspace_model_update(ModelUpdate::Connected(incoming)));
        assert!(!app.shared_views.quarantined.is_empty());
        assert!(
            app.durable
                .channels
                .iter()
                .all(|channel| channel.cards.is_empty())
        );
    }
}

#[test]
fn attention_model_sync_never_recreates_removed_replaced_or_ambiguous_managed_panes_at_old_target()
{
    for state in ["removed", "replaced", "ambiguous"] {
        let (mut app, _, pane, _) = managed_display_fixture();
        match state {
            "removed" => app.workspace_model.views[1].panes.clear(),
            "replaced" => {
                let CardKind::Terminal(session) = &mut app.workspace_model.views[1].panes[0].kind
                else {
                    unreachable!()
                };
                session.id = TerminalId::new();
            }
            _ => {
                let mut duplicate = app.workspace_model.views[1].clone();
                duplicate.scope = crate::agent_execution::Placement::Channel(ChannelId::new());
                app.workspace_model.views.push(duplicate);
            }
        }
        let incoming = std::mem::take(&mut app.workspace_model);
        drop(app.apply_workspace_model_update(ModelUpdate::Connected(incoming)));
        assert!(
            app.card_state(pane.id).is_none(),
            "{state} canonical evidence cannot fall back to the original placement"
        );
        assert!(
            !app.durable
                .channels
                .iter()
                .any(|channel| channel.cards.contains(&pane.id))
        );
        if state != "removed" {
            assert!(
                app.shared_views.quarantined.contains(&pane.id),
                "ordinary projection must not bypass the managed replacement/ambiguity fence"
            );
        }
    }
}

/// Whether a view edit acts on this pane, rather than anchoring beside it.
fn change_mentions(change: &crate::workspace_model::views::Change, card: CardId) -> bool {
    change.pane_id() == card
}

/// A window whose project is keyed the way a real one is: by the common Git
/// directory, which is what `ProjectState.repository` holds and therefore
/// what the shared model keys a channel on. Keying the fixture on the work
/// tree instead is what let a channel that no window could adopt pass every
/// test in this module.
fn app_with_project() -> (Cagents, ProjectId, PathBuf) {
    let mut app = super::super::tests::context_inspector_test_app(true);
    let project = ProjectId::new();
    let repository = PathBuf::from("/repo/.git");
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "repo".into(),
        repository.clone(),
        Vec::new(),
        Vec::new(),
    ));
    (app, project, repository)
}

fn shared(id: ChannelId, repository: &Path, title: &str, ordinal: u32) -> ModelChannel {
    ModelChannel {
        title_source: Default::default(),
        id,
        repository: repository.to_path_buf(),
        title: title.to_owned(),
        ordinal,
        checkout: None,
    }
}

fn local(id: ChannelId, project: ProjectId, title: &str, ordinal: u32) -> ProjectChannel {
    ProjectChannel {
        title_source: Default::default(),
        id,
        project_id: project,
        title: title.to_owned(),
        ordinal,
        shared: false,
        cards: Vec::new(),
        focused: None,
        checkout: None,
    }
}

/// A registry with an identity and the channels it holds, as a window that
/// has reconciled against it would have recorded.
fn registry(channels: Vec<ModelChannel>) -> WorkspaceModel {
    WorkspaceModel {
        navigation_orders: Vec::new(),
        identity: uuid::Uuid::new_v4(),
        sequence: channels.len() as u64,
        projects: Vec::new(),
        channels,
        closed_projects: Vec::new(),
        views: Vec::new(),
        agents: Vec::new(),
    }
}

fn rank_receipt(app: &Cagents, id: ChannelId, ordinal: u32, sequence: u64) -> ModelUpdate {
    let local = app
        .durable
        .channels
        .iter()
        .find(|channel| channel.id == id)
        .unwrap();
    let repository = &app
        .sidebar_model
        .project(local.project_id)
        .unwrap()
        .repository;
    let mut model = app.workspace_model.clone();
    model.sequence = sequence;
    // A server snapshot contains assigned project/channel ranks, not the
    // pending placeholders a client folds from observer edges.
    for project in &mut model.projects {
        if project.ordinal == PENDING_ORDINAL {
            project.ordinal = 1;
        }
    }
    model
        .channels
        .retain(|channel| channel.id != id && channel.ordinal != PENDING_ORDINAL);
    model
        .channels
        .push(shared(id, repository, &local.title, ordinal));
    ModelUpdate::Ranked { channel: id, model }
}

/// The real observer and publisher deliver canonical effects, rather
/// than the legacy Edited message used by metadata-only unit fixtures.
#[test]
fn delta_channel_titles_save_once_per_burst_and_keep_canonical_names() {
    use crate::workspace_model::views::View;
    for committed in [false, true] {
        let (mut app, project, repository) = app_with_project();
        let channel = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(local(channel, project, "Original", 0));
        app.workspace_model = registry(vec![shared(channel, &repository, "Original", 0)]);
        app.workspace_model.views.push(View::empty(channel));
        let identity = app.workspace_model.identity;
        let before = app.durable.clone();
        let generation = app.navigation_save_generation;
        let saved = super::super::terminal_output_tests::persists();
        for index in 0..5 {
            let sequence = 10 + index;
            let title = format!("Renamed {index}");
            let edit = ModelEdit::RetitleChannel {
                id: channel,
                title: title.clone(),
            };
            let effects = EditEffects {
                channels: vec![shared(channel, &repository, &title, 0)],
                ..Default::default()
            };
            let update = if committed {
                ModelUpdate::Committed {
                    receipt: EditReceipt {
                        identity,
                        sequence,
                        effects,
                    },
                    edit,
                    views: ViewChanges::default(),
                }
            } else {
                ModelUpdate::Changed {
                    sequence,
                    edit,
                    changes: EdgeChanges {
                        identity: Some(identity),
                        effects,
                        ..Default::default()
                    },
                }
            };
            drop(app.apply_workspace_model_update(update));
            assert_eq!(app.workspace_model.channel(channel).unwrap().title, title);
            assert_eq!(app.durable.channels[0].title, title);
            assert!(
                app.navigation_save.is_some(),
                "the canonical title is saved soon"
            );
        }
        assert_eq!(app.navigation_save_generation, generation.wrapping_add(1));
        assert_eq!(super::super::terminal_output_tests::persists(), saved);
        assert_eq!(app.durable.cards, before.cards);
        assert_eq!(app.durable.card_scopes, before.card_scopes);
        // A rejected suggestion's request text is not canonical. Empty
        // effects preserve the manually chosen name on both paths.
        let edit = ModelEdit::SuggestChannelTitle {
            id: channel,
            title: "Refused suggestion".into(),
            source: crate::model::ChannelTitleSource::Provider,
        };
        let update = if committed {
            ModelUpdate::Committed {
                receipt: EditReceipt {
                    identity,
                    sequence: 15,
                    effects: EditEffects::default(),
                },
                edit,
                views: ViewChanges::default(),
            }
        } else {
            ModelUpdate::Changed {
                sequence: 15,
                edit,
                changes: EdgeChanges {
                    identity: Some(identity),
                    ..Default::default()
                },
            }
        };
        drop(app.apply_workspace_model_update(update));
        assert_eq!(app.durable.channels[0].title, "Renamed 4");
        assert_eq!(app.navigation_save_generation, generation.wrapping_add(1));
    }
}

#[test]
fn a_closed_identity_baseline_runs_the_restore_ready_empty_channel_sweep() {
    use crate::workspace_model::views::View;
    let (mut app, project, repository) = app_with_project();
    let channel = ChannelId::new();
    Arc::make_mut(&mut app.durable)
        .channels
        .push(local(channel, project, "", 0));
    app.workspace_model = WorkspaceModel::default();
    let requests = intercept_publishes(&mut app);
    app.allow_empty_channel_sweep();
    assert_eq!(app.channel_ui.sweep, super::super::channel_ui::Sweep::Ready);
    // Untitled: a name the person gave would keep it.
    let mut model = registry(vec![shared(channel, &repository, "", 0)]);
    model.views.push(View::empty(channel));
    let home = crate::workspace_model::ChannelIncarnation {
        registry: model.identity,
        channel,
        creation_ordinal: 0,
    };
    drop(
        app.apply_workspace_model_update(ModelUpdate::ConnectedClosed {
            model,
            closed: vec![],
        }),
    );
    assert_eq!(app.channel_ui.sweep, super::super::channel_ui::Sweep::Done);
    assert!(drain(&requests).iter().any(|edit| matches!(
        edit,
        ModelEdit::RetireEmptyChannel { home: actual, expected_view_revision: 0 }
            if *actual == home
    )));
}

#[test]
fn a_reconnecting_window_applies_a_missed_project_close() {
    let (mut app, project, repository) = app_with_project();
    let mut model = registry(Vec::new());
    model.closed_projects.push(repository.clone());
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(model)));
    assert!(app.project_ui.closing.contains_key(&project));
    let generation = app.project_ui.close_generation[&project];
    drop(
        app.handle_project_action(super::super::project_ui::Action::ProjectSessionsClosed(
            project,
            generation,
            Ok(super::super::project_ui::ProjectSessionClosure::for_roots(
                vec![repository.clone()],
            )),
        )),
    );
    assert!(app.sidebar_model.project(project).is_none());
    assert!(app.project_ui.closed.contains(&repository));
}

#[test]
fn a_late_close_callback_cannot_override_a_reopen() {
    let (mut app, project, repository) = app_with_project();
    let mut model = registry(Vec::new());
    model.sequence = 1;
    model.closed_projects.push(repository.clone());
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(model)));
    assert!(app.project_ui.closing.contains_key(&project));
    let generation = app.project_ui.close_generation[&project];

    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: 2,
        edit: ModelEdit::ReopenProject {
            kind: Default::default(),
            repository: repository.clone(),
            name: "repo".into(),
        },
    }));
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: 1,
        edit: ModelEdit::CloseProject {
            repository: repository.clone(),
        },
    }));
    drop(
        app.handle_project_action(super::super::project_ui::Action::ProjectSessionsClosed(
            project,
            generation,
            Ok(super::super::project_ui::ProjectSessionClosure::for_roots(
                vec![repository.clone()],
            )),
        )),
    );
    assert!(app.sidebar_model.project(project).is_some());
    assert!(!app.project_ui.closed.contains(&repository));
}

#[test]
fn an_unchanged_explicit_open_acknowledges_the_pending_reopen() {
    let (mut app, project, repository) = app_with_project();
    app.project_ui.reopening.insert(project, repository.clone());
    let mut model = registry(Vec::new());
    model.projects.push(ModelProject {
        kind: Default::default(),
        repository: repository.clone(),
        name: "repo".into(),
        ordinal: 1,
    });
    drop(app.apply_workspace_model_update(ModelUpdate::Reopened {
        repository: repository.clone(),
        model,
    }));
    assert!(!app.project_ui.is_reopening(&repository));
    assert!(app.sidebar_model.project(project).is_some());
}

/// Replaces the publish queue with one the test can read, which is how the
/// window's side of a publish is observed without a daemon.
fn intercept_publishes(app: &mut Cagents) -> TestModelRequests {
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    rx
}

fn drain(rx: &TestModelRequests) -> Vec<ModelEdit> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

/// A real checkout, because adopting a project runs the repository loader
/// for real: what is being asserted is what that load does to this window.
///
/// `None` when there is no usable `git`, so the suite skips rather than
/// failing on a machine without one.
fn git_checkout() -> Option<tempfile::TempDir> {
    let checkout = tempfile::tempdir().expect("temporary directory");
    let initialized = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(checkout.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    initialized
        .is_ok_and(|status| status.success())
        .then_some(checkout)
}

/// Runs a task to its first window message.
///
/// The repository load is the whole subject here - the window does nothing
/// observable until it finishes - so the test drives it rather than
/// synthesising the message it produces, which is the one thing that would
/// hide the flag under test.
fn first_message(task: Task<Message>) -> Option<Message> {
    use cosmic::iced::futures::StreamExt;

    let mut stream = cosmic::iced::runtime::task::into_stream(task)?;
    // A tokio runtime rather than a bare executor: the loader runs on
    // `spawn_blocking`, which panics outside one.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime")
        .block_on(async move {
            while let Some(action) = stream.next().await {
                if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(message)) = action
                {
                    return Some(message);
                }
            }
            None
        })
}

#[test]
fn shared_folder_reopen_loads_new_rows_and_preserves_acknowledged_terminal_membership() {
    use crate::agent::{Activity, ActivityState, Provider};
    use crate::workspace_model::views::{Change, Scope, View};

    fn sidebar_frame(
        app: &Cagents,
        renderer: &mut cosmic::Renderer,
        navigation: &crate::sidebar::NavigationCache,
        cache: cosmic::iced::runtime::user_interface::Cache,
    ) -> (
        cosmic::iced::runtime::user_interface::Cache,
        Vec<(String, [f64; 4])>,
    ) {
        use cosmic::iced::{
            Size,
            advanced::{clipboard, mouse},
        };
        let shortcuts = crate::settings::KeyboardShortcuts::default();
        let mut ui = cosmic::iced::runtime::UserInterface::build(
            crate::sidebar::view_cached(
                &app.sidebar_model,
                &app.sidebar_state,
                crate::sidebar::SidebarMode::Expanded,
                280.0,
                false,
                navigation,
                &shortcuts,
                false,
            ),
            Size::new(280.0, 900.0),
            cache,
            renderer,
        );
        let mut messages = Vec::new();
        ui.update(
            &[],
            mouse::Cursor::Unavailable,
            renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert!(messages.is_empty());
        ui.draw(
            renderer,
            &cosmic::theme::active(),
            &cosmic::iced::advanced::renderer::Style::default(),
            mouse::Cursor::Unavailable,
        );
        let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let rows = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| {
                let label = node.node().label()?;
                let bounds = node.node().bounds()?;
                (node.node().role() == iced_accessibility::accesskit::Role::Button
                    && !node.node().is_hidden()
                    && !node.node().is_disabled()
                    && label.starts_with("Open project "))
                .then(|| {
                    (
                        label.to_owned(),
                        [bounds.x0, bounds.y0, bounds.x1, bounds.y1],
                    )
                })
            })
            .collect();
        (ui.into_cache(), rows)
    }

    fn reopen(app: &mut Cagents, path: &Path, sequence: u64) {
        let task = app.update(Message::WorkspaceModelObserved(ModelUpdate::Edited {
            sequence,
            edit: ModelEdit::ReopenProject {
                repository: path.to_path_buf(),
                name: path.file_name().unwrap().to_string_lossy().into_owned(),
                kind: ProjectKind::Folder,
            },
        }));
        let loaded = first_message(task).expect("the actual model edit schedules its loader");
        assert!(matches!(&loaded, Message::RepositoryLoaded {
            open_cards: false,
            result: Ok(loaded),
            ..
        } if loaded.project.repository == path && loaded.project.kind == ProjectKind::Folder));
        drop(app.update(loaded));
    }

    for explicitly_filed in [false, true] {
        let mut renderer = super::super::tests::headless_renderer();
        let navigation = crate::sidebar::NavigationCache::default();
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().canonicalize().unwrap().join("parent");
        let nested = parent.join("nested");
        let control = directory.path().join("control");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(&control).unwrap();
        let mut app = super::super::tests::context_inspector_test_app(true);
        let control_card = app.workspace.focused().unwrap();
        let CardKind::Terminal(mut session) = app.card_state(control_card).unwrap().kind.clone()
        else {
            panic!("the original control pane is a terminal")
        };
        if let CardKind::Terminal(control_session) =
            &mut app.card_state_mut(control_card).unwrap().kind
        {
            control_session.launch.working_directory = control.clone();
        }
        drop(app.select_worktree(control.clone(), false));
        app.refresh_ui.focused = false;
        let focused = app.workspace.focused();
        let strip = app.workspace.order().collect::<Vec<_>>();
        let published = intercept_publishes(&mut app);
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                registry(Vec::new()),
            ))),
        );
        reopen(&mut app, &parent, 1);
        assert!(app.known_worktree_roots.contains(&parent));
        assert_eq!(app.worktree_scope_for(&nested), Some(parent.clone()));
        let parent_key = app.sidebar_model.worktree_at(&parent).unwrap();
        let scope = app.worktree_view_scope(&parent).unwrap();
        assert!(matches!(&scope, Scope::Worktree { path, .. } if *path == parent));

        // A real seed acknowledgment precedes the newly observed session.
        // Unlike the old NO_REGISTRY fixture, subsequent publication and
        // canonical projection therefore both run through the App reducer.
        let mut model = app.workspace_model.clone();
        model.sequence = 2;
        model.views.push(View::empty(scope.clone()));
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::ViewApplied {
                scope: scope.clone(),
                operation: None,
                model,
            })),
        );
        drain(&published);
        session.id = TerminalId::new();
        session.launch.working_directory = nested.clone();
        session.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("shared-folder-test"),
            boot_id: crate::model::BootId::new("shared-folder-test"),
            host_incarnation: crate::model::HostIncarnation::new("shared-folder-test"),
        };
        let terminal = session.id;
        let mut descriptor = crate::terminal::ipc::TerminalDescriptor::relaunchable(
            terminal,
            session.launch.clone(),
            None,
        );
        descriptor.session = session.clone();
        assert!(!app.durable.cards.iter().any(|card| {
            matches!(&card.kind, CardKind::Terminal(current) if current.id == terminal)
        }));
        drop(app.update(Message::HostedTerminalsObserved(vec![descriptor])));
        let cards = app
            .durable
            .cards
            .iter()
            .filter(
                |card| matches!(&card.kind, CardKind::Terminal(current) if current.id == terminal),
            )
            .map(|card| card.id)
            .collect::<Vec<_>>();
        assert_eq!(cards.len(), 1);
        let card = cards[0];
        assert!(
            !app.durable.card_scopes.contains_key(&card),
            "the known Folder boundary must prevent active-control fallback filing"
        );
        if explicitly_filed {
            Arc::make_mut(&mut app.durable)
                .card_scopes
                .insert(card, parent.clone());
        }
        let working = Activity {
            provider: Provider::Omp,
            state: ActivityState::Working,
        };
        app.sidebar_state.set_agent_activity(
            card,
            parent_key.project_id,
            parent_key.worktree_id,
            working,
        );
        app.sidebar_state
            .observe_agent_update(card, parent_key.project_id, parent_key.worktree_id);
        app.publish_view_changes();
        let inserts = drain(&published)
            .into_iter()
            .filter_map(|edit| match edit {
                ModelEdit::ChangeView {
                    scope: actual,
                    operation,
                    expected_revision,
                    change: change @ Change::Insert { .. },
                    ..
                } if actual == scope => Some((operation, expected_revision, change)),
                _ => None,
            })
            .filter(
                |(_, _, change)| matches!(change, Change::Insert { pane, .. } if pane.id == card),
            )
            .collect::<Vec<_>>();
        assert_eq!(
            inserts.len(),
            1,
            "the real publish queue must contain the exact new pane"
        );
        let (operation, expected_revision, change) = inserts.into_iter().next().unwrap();
        let mut acknowledged = app.workspace_model.clone();
        acknowledged.sequence = 3;
        let view = acknowledged
            .views
            .iter_mut()
            .find(|view| view.scope == scope)
            .unwrap();
        assert_eq!(
            expected_revision, view.revision,
            "the captured real request must target this exact simulated CAS basis"
        );
        assert!(view.apply(&change).unwrap());
        view.revision += 1;
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::ViewApplied {
                scope,
                operation: Some(operation),
                model: acknowledged,
            })),
        );
        assert_eq!(
            app.durable.card_scopes.get(&card),
            Some(&parent),
            "daemon-accepted membership is authoritative in both pre-ACK cases"
        );
        let (retained_cache, before_rows) = sidebar_frame(
            &app,
            &mut renderer,
            &navigation,
            cosmic::iced::runtime::user_interface::Cache::default(),
        );
        assert!(
            before_rows
                .iter()
                .any(|(label, _)| label == "Open project parent, agent working")
        );
        assert!(
            !before_rows
                .iter()
                .any(|(label, _)| label == "Open project nested")
        );
        reopen(&mut app, &nested, 4);
        let nested_key = app
            .sidebar_model
            .worktree_at(&nested)
            .expect("the model-driven nested Folder must have its own row");
        assert_ne!(nested_key.project_id, parent_key.project_id);
        assert_eq!(
            app.sidebar_model
                .projects
                .iter()
                .filter(|project| project.repository == parent)
                .count(),
            1
        );
        assert_eq!(
            app.sidebar_model
                .projects
                .iter()
                .filter(|project| project.repository == nested)
                .count(),
            1
        );
        let (_, after_rows) = sidebar_frame(&app, &mut renderer, &navigation, retained_cache);
        for expected in ["Open project parent, agent working", "Open project nested"] {
            let matches = after_rows
                .iter()
                .filter(|(label, _)| label == expected)
                .collect::<Vec<_>>();
            assert_eq!(
                matches.len(),
                1,
                "the retained real sidebar must expose {expected}"
            );
            let bounds = matches[0].1;
            assert!(
                bounds[0] >= 0.0
                    && bounds[1] >= 0.0
                    && bounds[2] > bounds[0]
                    && bounds[3] > bounds[1]
                    && bounds[2] <= 280.0
                    && bounds[3] <= 900.0
            );
        }
        assert_eq!(
            app.card_worktree(app.card_state(card).unwrap()),
            Some(parent.as_path())
        );
        assert_eq!(
            app.sidebar_state
                .agent_activity(parent_key.project_id, parent_key.worktree_id),
            Some(ActivityState::Working)
        );
        assert!(
            app.sidebar_state
                .has_agent_update(parent_key.project_id, parent_key.worktree_id)
        );
        assert_eq!(
            app.sidebar_state
                .agent_activity(nested_key.project_id, nested_key.worktree_id),
            None
        );
        assert!(
            !app.sidebar_state
                .has_agent_update(nested_key.project_id, nested_key.worktree_id)
        );
        assert_eq!(app.durable.card_scopes.get(&card), Some(&parent));
        assert_eq!(app.active_workspace.as_ref(), Some(&control));
        assert_eq!(app.workspace.focused(), focused);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), strip);
        assert!(
            matches!(&app.card_state(card).unwrap().kind, CardKind::Terminal(current) if *current == session)
        );
    }
}

/// A project this window has never seen - opened from a shell with
/// `cagents project open`, or in another window - arrives as a sidebar row
/// and nothing else.
///
/// Adoption used to read the repository the way a user opening one in this
/// window does, which selects its worktree and starts a terminal in it. In
/// every window at once: the user's active worktree was replaced under
/// them by a repository someone else named, and a terminal they never
/// started appeared in it. A project the registry names is news, not an
/// instruction.
#[test]
fn a_fresh_window_adopts_the_shared_folder_kind_without_git_rediscovery() {
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().canonicalize().unwrap();
    std::fs::create_dir(repository.join(".git")).unwrap();
    let mut app = super::super::tests::context_inspector_test_app(false);
    app.workspace_model = WorkspaceModel {
        projects: vec![ModelProject {
            repository: repository.clone(),
            name: "Folder".into(),
            kind: ProjectKind::Folder,
            ordinal: 1,
        }],
        ..registry(Vec::new())
    };
    let message = first_message(app.reconcile_workspace_model()).unwrap();
    drop(app.update(message));
    let project = app.sidebar_model.projects.first().unwrap();
    assert_eq!(project.repository, repository);
    assert_eq!(project.kind, ProjectKind::Folder);
    assert!(project.worktrees.is_empty());
    assert!(app.durable.cards.is_empty());
}

#[test]
fn an_adopted_project_is_a_sidebar_row_and_takes_neither_a_card_nor_the_place() {
    let Some(checkout) = git_checkout() else {
        eprintln!("skipping: `git` is not available on PATH");
        return;
    };
    let (mut app, _, _) = app_with_project();
    let cards = app.durable.cards.len();
    let focused = app.workspace.focused();
    // The key the CLI and a window both derive: the common Git directory.
    let repository = canonical_repository(&checkout.path().join(".git")).expect("an absolute key");
    app.workspace_model = WorkspaceModel {
        projects: vec![ModelProject {
            kind: Default::default(),
            repository: repository.clone(),
            name: "opened elsewhere".into(),
            ordinal: 1,
        }],
        ..registry(Vec::new())
    };

    let loaded = first_message(app.reconcile_workspace_model())
        .expect("adopting a project reads the repository it names");
    let _ = app.update(loaded);

    assert!(
        app.sidebar_model
            .projects
            .iter()
            .any(|project| project.repository == repository),
        "the adopted project is in this window's sidebar"
    );
    assert_eq!(
        app.durable.cards.len(),
        cards,
        "adoption starts no terminal, so no card appears"
    );
    assert_eq!(
        app.active_workspace, None,
        "and it does not move the user into the repository it just heard about"
    );
    assert_eq!(app.workspace.focused(), focused);
}

/// A channel another window created arrives as a scope, not as a
/// conversation: N windows adopting one channel must not start N chats in
/// it, and the cards inside a channel are each window's own business.
///
/// It also has to reach the sidebar. The key the channel carries is the
/// project's common Git directory, and when the CLI keyed it on the work
/// tree instead, adoption found no owner and retained the channel forever:
/// the model held it, the store never did, and no window showed it.
#[test]
fn an_adopted_channel_arrives_empty_and_starts_no_conversation() {
    let (mut app, project, repository) = app_with_project();
    let cards = app.durable.cards.len();
    let id = ChannelId::new();
    app.workspace_model = registry(vec![shared(id, &repository, "created elsewhere", 7)]);

    let _ = app.reconcile_workspace_model();

    let adopted = app
        .durable
        .channels
        .iter()
        .find(|channel| channel.id == id)
        .expect("the channel is adopted into this window");
    assert_eq!(adopted.project_id, project);
    assert_eq!(adopted.title, "created elsewhere");
    assert_eq!(adopted.ordinal, 7);
    assert!(
        adopted.cards.is_empty(),
        "an adopted channel holds no cards"
    );
    assert_eq!(adopted.focused, None);
    assert_eq!(
        app.durable.cards.len(),
        cards,
        "adoption starts no conversation, so no card appears"
    );
    app.refresh_sidebar_channels();
    let rows =
        crate::sidebar::channels_in_display_order(app.sidebar_model.projects[0].channels.iter());
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![id],
        "an adopted channel is a row, not just a record"
    );
}

/// A channel's checkout is part of what it is, so every window shows it
/// working in the same directory: one made here tells the registry, and one
/// the registry holds arrives with it.
#[test]
fn a_channels_checkout_is_published_and_adopted() {
    let (mut app, project, repository) = app_with_project();
    let published = intercept_publishes(&mut app);
    let feature = PathBuf::from("/repo-feature");
    let mine = app
        .create_channel(
            project,
            "feature".into(),
            Default::default(),
            Some(feature.clone()),
        )
        .unwrap();
    assert!(drain(&published).iter().any(|edit| matches!(
        edit,
        ModelEdit::CreateChannel { id, checkout: Some(checkout), .. }
            if *id == mine && *checkout == feature
    )));

    let theirs = ChannelId::new();
    let mut elsewhere = shared(theirs, &repository, "created elsewhere", 7);
    elsewhere.checkout = Some(feature.clone());
    app.workspace_model = registry(vec![elsewhere]);
    let _ = app.reconcile_workspace_model();
    let adopted = |app: &Cagents| {
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == theirs)
            .expect("the channel is adopted into this window")
            .checkout
            .clone()
    };
    assert_eq!(adopted(&app), Some(feature));

    app.workspace_model.channels[0].checkout = None;
    app.workspace_model.sequence += 1;
    let _ = app.reconcile_workspace_model();
    assert_eq!(adopted(&app), None, "the registry's checkout wins");
}

/// The registry's rank decides the order, so an adopted channel created
/// after a local one renders above it - in every window, whichever one made
/// which.
#[test]
fn a_newer_adopted_channel_renders_above_an_older_local_one() {
    let (mut app, project, repository) = app_with_project();
    let mine = ChannelId::new();
    Arc::make_mut(&mut app.durable)
        .channels
        .push(local(mine, project, "mine", 3));
    let adopted = ChannelId::new();
    app.workspace_model = registry(vec![
        shared(mine, &repository, "mine", 3),
        shared(adopted, &repository, "theirs", 9),
    ]);

    let _ = app.reconcile_workspace_model();
    app.refresh_sidebar_channels();

    let rows =
        crate::sidebar::channels_in_display_order(app.sidebar_model.projects[0].channels.iter());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, adopted, "the newer channel is on top");
    assert_eq!(rows[1].id, mine);
}

/// A channel deleted anywhere disappears everywhere, and its cards are
/// detached locally after the initiating client has retired host work.
#[test]
fn a_deleted_channel_detaches_its_cards_without_a_second_host_close() {
    let (mut app, project, repository) = app_with_project();
    let id = ChannelId::new();
    let conversation = app.durable.cards[0].id;
    let mut channel = local(id, project, "doomed", 4);
    channel.cards = vec![conversation];
    channel.focused = Some(conversation);
    Arc::make_mut(&mut app.durable).channels.push(channel);
    app.workspace_model = registry(vec![shared(id, &repository, "doomed", 4)]);
    let published = intercept_publishes(&mut app);

    let _ = app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: 5,
        edit: ModelEdit::DeleteChannel { id },
    });
    assert!(app.channel_ui.was_deleted(id));
    assert!(!app.channel_ui.is_deleting(id));
    assert!(
        !drain(&published).iter().any(
            |edit| matches!(edit, ModelEdit::CreateChannel { id: revived, .. } if *revived == id)
        ),
        "an in-flight deletion must not resurrect an unacknowledged channel"
    );

    assert!(
        !app.durable.channels.iter().any(|channel| channel.id == id),
        "the channel is gone from this window too"
    );
    let card = app
        .durable
        .cards
        .iter()
        .find(|card| card.id == conversation)
        .expect("the card is tombstoned rather than forgotten");
    assert!(
        !card.existence.is_open(),
        "a conversation that lived only in the channel goes with it"
    );
}

/// A window is not sent the echo of its own deletion, so its cached model
/// still names the channel afterwards, and the next reconciliation - any
/// other window's edit, a repository finishing loading, or a read taken
/// before the registry applied the deletion - adopted it straight back as
/// an empty channel.
#[test]
fn a_channel_this_window_deleted_is_never_adopted_back() {
    let (mut app, project, repository) = app_with_project();
    let id = ChannelId::new();
    let mut channel = local(id, project, "doomed", 4);
    channel.shared = true;
    Arc::make_mut(&mut app.durable).channels.push(channel);
    let model = registry(vec![shared(id, &repository, "doomed", 4)]);
    app.workspace_model = model.clone();
    let published = intercept_publishes(&mut app);

    let _ = app.delete_channel(id);
    assert!(!app.durable.channels.iter().any(|channel| channel.id == id));
    assert!(
        drain(&published)
            .iter()
            .any(|edit| matches!(edit, ModelEdit::DeleteChannel { id: deleted } if *deleted == id))
    );

    let _ = app.reconcile_workspace_model();
    let _ = app.apply_workspace_model_update(ModelUpdate::Read(model));
    assert!(
        !app.durable.channels.iter().any(|channel| channel.id == id),
        "a model that predates the deletion does not revive the channel"
    );
    assert!(drain(&published).is_empty());
}

fn held_project(repository: &Path) -> ModelProject {
    ModelProject {
        repository: repository.to_path_buf(),
        name: "repo".into(),
        kind: ProjectKind::Git,
        ordinal: 1,
    }
}

/// Every repository load publishes the project it opened. Restoring a
/// workspace loads every project, and the registry already holds each
/// one exactly: none of those edits may be sent, since each would commit
/// nothing and return a receipt to project.
#[test]
fn opening_a_project_the_registry_holds_identically_publishes_nothing() {
    let (mut app, project, repository) = app_with_project();
    app.sidebar_model.projects[0] = app.sidebar_model.projects[0]
        .clone()
        .with_kind(ProjectKind::Git);
    app.workspace_model = registry(Vec::new());
    app.workspace_model.projects.push(held_project(&repository));
    let published = intercept_publishes(&mut app);

    app.publish_project_opened(project);
    assert!(drain(&published).is_empty());

    // A different name is news, and so is a project the registry lacks,
    // tombstoned, or not yet connected.
    app.workspace_model.projects[0].name = "renamed elsewhere".into();
    app.publish_project_opened(project);
    app.workspace_model.projects.clear();
    app.workspace_model.closed_projects.push(repository.clone());
    app.publish_project_opened(project);
    app.workspace_model = WorkspaceModel::default();
    app.publish_project_opened(project);
    let open = ModelEdit::OpenProject {
        repository: repository.clone(),
        name: "repo".into(),
        kind: ProjectKind::Git,
    };
    assert_eq!(drain(&published), vec![open.clone(), open.clone(), open]);

    // An explicit reopen is always the user's intent, never redundant.
    app.workspace_model = registry(Vec::new());
    app.workspace_model.projects.push(held_project(&repository));
    app.project_ui.reopening.insert(project, repository.clone());
    app.publish_project_opened(project);
    assert!(matches!(
        drain(&published).as_slice(),
        [ModelEdit::ReopenProject { .. }]
    ));
}

/// The registry answers an `OpenProject` of a project it holds with the
/// row it already had. That receipt changes nothing this window projects,
/// so it must neither reconcile the workspace nor save it.
#[test]
fn a_receipt_confirming_a_held_project_neither_reconciles_nor_projects() {
    let (mut app, _project, repository) = app_with_project();
    app.workspace_model = registry(Vec::new());
    app.workspace_model.sequence = 7;
    app.workspace_model.projects.push(held_project(&repository));
    let identity = app.workspace_model.identity;
    let held = app.workspace_model.clone();
    let edit = ModelEdit::OpenProject {
        repository: repository.clone(),
        name: "repo".into(),
        kind: ProjectKind::Git,
    };
    let receipt = |project: ModelProject| ModelUpdate::Committed {
        receipt: EditReceipt {
            identity,
            sequence: 7,
            effects: EditEffects {
                projects: vec![project],
                ..Default::default()
            },
        },
        edit: edit.clone(),
        views: ViewChanges::default(),
    };
    // Discovery is not primed, so a save would build a topology.
    app.shared_views.topology_build_count.set(0);
    app.shared_views.structural_pass_count.set(0);

    let (_, projection) =
        app.apply_workspace_model_update_projection(receipt(held_project(&repository)));

    assert_eq!(projection, Projection::Unchanged);
    // Through the window's update too, where the projection oracle checks
    // that skipping the projection left nothing stale.
    drop(
        app.update(Message::WorkspaceModelObserved(receipt(held_project(
            &repository,
        )))),
    );
    assert_eq!(app.shared_views.structural_pass_count.get(), 0);
    assert_eq!(app.shared_views.topology_build_count.get(), 0);
    assert_eq!(app.workspace_model, held);
    assert_eq!(
        app.shared_views.metadata_revisions[&MetadataKey::Project(repository.clone())],
        7,
        "the confirmed row still fences older metadata"
    );

    // A receipt that renames the project is a change, reconciled as before.
    let mut renamed = held_project(&repository);
    renamed.name = "renamed".into();
    let (_, projection) = app.apply_workspace_model_update_projection(receipt(renamed));
    assert_eq!(projection, Projection::Full);
    assert_eq!(app.shared_views.structural_pass_count.get(), 1);
    assert_eq!(app.workspace_model.projects[0].name, "renamed");
}

/// Background repository loads that finish together reconcile together;
/// any other message reconciles them first, so no other handler sees a
/// loaded repository unreconciled.
#[test]
fn a_deferred_repository_reconciliation_runs_before_any_other_message() {
    let (mut app, _project, repository) = app_with_project();
    app.workspace_model = registry(Vec::new());
    app.workspace_model.projects.push(held_project(&repository));
    drop(app.defer_repository_reconciliation(repository.clone()));
    drop(app.defer_repository_reconciliation(repository.clone()));
    assert_eq!(app.pending_repository_reconciliation, vec![repository]);
    app.shared_views.structural_pass_count.set(0);

    drop(app.update(Message::ReportedTitleSaveDue));

    assert!(app.pending_repository_reconciliation.is_empty());
    assert_eq!(app.shared_views.structural_pass_count.get(), 1);
    // The armed message finds nothing left to do.
    drop(app.update(Message::RepositoryReconciliationDue));
    assert_eq!(app.shared_views.structural_pass_count.get(), 1);
}

/// A connection that delivers the registry's model is a message like any
/// other: the loads waiting for their reconciliation get it first, against
/// the model they loaded under, and the connection then reconciles all.
#[test]
fn a_connection_while_loads_wait_reconciles_them_first() {
    let (mut app, _project, repository) = app_with_project();
    app.workspace_model = registry(Vec::new());
    app.workspace_model.projects.push(held_project(&repository));
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(app.workspace_model.identity);
    drop(app.defer_repository_reconciliation(repository.clone()));
    app.shared_views.structural_pass_count.set(0);
    let mut model = app.workspace_model.clone();
    model.sequence += 1;

    drop(app.update(Message::WorkspaceModelObserved(
        ModelUpdate::ConnectedClosed {
            model,
            closed: Vec::new(),
        },
    )));

    assert!(app.pending_repository_reconciliation.is_empty());
    assert_eq!(
        app.shared_views.structural_pass_count.get(),
        2,
        "the waiting repository pass, then the connection's complete one"
    );
    drop(app.update(Message::RepositoryReconciliationDue));
    assert_eq!(app.shared_views.structural_pass_count.get(), 2);
}

/// Without a registry a repository load has nothing to reconcile against,
/// so it schedules nothing: connecting reconciles every project.
#[test]
fn a_load_before_the_registry_answers_defers_nothing() {
    let (mut app, _project, repository) = app_with_project();
    assert_eq!(app.workspace_model.identity, NO_REGISTRY);
    drop(app.defer_repository_reconciliation(repository));
    assert!(app.pending_repository_reconciliation.is_empty());
}

/// The registry can commit other windows' edits between this window's last
/// observed edge and its open: the confirming receipt then carries a later
/// sequence. The row is still the one held, so nothing is projected, but
/// the model's clock and the row's fence move to that sequence.
#[test]
fn a_confirming_receipt_ahead_of_the_model_advances_only_its_clocks() {
    let (mut app, _project, repository) = app_with_project();
    app.workspace_model = registry(Vec::new());
    app.workspace_model.sequence = 7;
    app.workspace_model.projects.push(held_project(&repository));
    let held = app.workspace_model.clone();
    app.shared_views.topology_build_count.set(0);
    app.shared_views.structural_pass_count.set(0);

    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Committed {
            receipt: EditReceipt {
                identity: held.identity,
                sequence: 12,
                effects: EditEffects {
                    projects: vec![held_project(&repository)],
                    ..Default::default()
                },
            },
            edit: ModelEdit::OpenProject {
                repository: repository.clone(),
                name: "repo".into(),
                kind: ProjectKind::Git,
            },
            views: ViewChanges::default(),
        })),
    );

    // Discovery is not primed, so a save would have built a topology.
    assert_eq!(app.shared_views.structural_pass_count.get(), 0);
    assert_eq!(app.shared_views.topology_build_count.get(), 0);
    assert_eq!(app.workspace_model.sequence, 12);
    assert_eq!(
        app.shared_views.metadata_revisions[&MetadataKey::Project(repository.clone())],
        12
    );
    assert_eq!(
        WorkspaceModel {
            sequence: held.sequence,
            ..app.workspace_model.clone()
        },
        held
    );
}

/// A channel this window introduces names its project. When the registry
/// already holds that project as this window has it, the channel alone is
/// sent; when it does not, the project is opened first.
#[test]
fn introducing_a_channel_opens_its_project_only_when_the_registry_lacks_it() {
    let (mut app, project, repository) = app_with_project();
    app.sidebar_model.projects[0] = app.sidebar_model.projects[0]
        .clone()
        .with_kind(ProjectKind::Git);
    let id = ChannelId::new();
    Arc::make_mut(&mut app.durable)
        .channels
        .push(local(id, project, "mine", 4));
    app.workspace_model = registry(Vec::new());
    app.workspace_model.projects.push(held_project(&repository));
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(app.workspace_model.identity);
    let published = intercept_publishes(&mut app);

    // Seeding the channel's view is the same either way.
    let introduced = |published: &TestModelRequests| {
        drain(published)
            .into_iter()
            .filter(|edit| !matches!(edit, ModelEdit::SeedView { .. }))
            .collect::<Vec<_>>()
    };
    drop(app.reconcile_workspace_model());
    assert!(matches!(
        introduced(&published).as_slice(),
        [ModelEdit::CreateChannel { id: created, .. }] if *created == id
    ));

    app.workspace_model.projects.clear();
    drop(app.reconcile_workspace_model());
    assert!(matches!(
        introduced(&published).as_slice(),
        [
            ModelEdit::OpenProject { repository: opened, .. },
            ModelEdit::CreateChannel { id: created, .. },
        ] if *opened == repository && *created == id
    ));
}

/// A load skips opening a project the registry it knows holds. If that
/// registry is then replaced, its successor was never told; joining it
/// sends the open the load skipped, for loaded projects only.
/// Reconnecting to the same registry, or joining one that holds the
/// project, sends nothing.
#[test]
fn joining_a_replacement_registry_reopens_projects_it_lacks() {
    let (mut app, project, repository) = app_with_project();
    app.sidebar_model.projects[0] = app.sidebar_model.projects[0]
        .clone()
        .with_kind(ProjectKind::Git);
    app.workspace_model = registry(Vec::new());
    app.workspace_model.projects.push(held_project(&repository));
    let first = app.workspace_model.clone();
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(first.identity);
    let published = intercept_publishes(&mut app);
    app.publish_project_opened(project);
    assert!(drain(&published).is_empty());

    let open = ModelEdit::OpenProject {
        repository: repository.clone(),
        name: "repo".into(),
        kind: ProjectKind::Git,
    };
    let opens = |edits: Vec<ModelEdit>| edits.into_iter().filter(|edit| *edit == open).count();
    let mut reconnected = first.clone();
    reconnected.projects.clear();
    reconnected.sequence += 1;
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(reconnected)));
    assert_eq!(
        opens(drain(&published)),
        0,
        "the same registry already answered for its own project rows"
    );

    // Beside it: a project still loading, one whose checkout a later read
    // could not reach, one this window closed, and one the successor
    // tombstoned. None of them is opened.
    let others = ["loading", "unreachable", "closed", "tombstoned"].map(|name| {
        let repository = PathBuf::from(format!("/{name}/.git"));
        let mut row = SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            name.into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        );
        if name != "loading" {
            app.project_ui.opened.insert(repository.clone());
        }
        if name == "unreachable" {
            row.unreachable = Some("the checkout is gone".into());
        }
        if name == "closed" {
            app.project_ui.closed.insert(repository.clone());
        }
        if name == "tombstoned" {
            // Being reopened, so the successor's tombstone does not close
            // it: only the tombstone itself keeps a plain open from going.
            app.project_ui.reopening.insert(row.id, repository.clone());
        }
        app.sidebar_model.projects.push(row);
        repository
    });
    let mut successor = registry(Vec::new());
    successor.closed_projects.push(others[3].clone());
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(successor)));
    let edits = drain(&published);
    assert!(
        !edits.iter().any(|edit| matches!(edit,
            ModelEdit::OpenProject { repository, .. } if others.contains(repository))),
        "{edits:?}"
    );
    assert_eq!(opens(edits), 1);

    let mut holding = registry(Vec::new());
    holding.projects.push(held_project(&repository));
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(holding)));
    assert_eq!(opens(drain(&published)), 0);

    // Only a complete pass introduces: a repository pass leaves it owed.
    app.workspace_model.projects.clear();
    app.shared_views.unintroduced_projects = true;
    drop(app.reconcile_workspace_model_for_repository(&repository));
    assert_eq!(opens(drain(&published)), 0);
    assert!(app.shared_views.unintroduced_projects);
    drop(app.reconcile_workspace_model());
    assert_eq!(opens(drain(&published)), 1);
    assert!(!app.shared_views.unintroduced_projects);
}

/// Before a window has shared a channel, the registry's silence about that
/// channel carries no information at all. Retiring on it would tombstone the
/// cards inside a channel nobody deleted, so the channel is published
/// instead - which is also the whole migration for a workspace that predates
/// the registry.
#[test]
fn a_window_joining_with_an_unshared_channel_keeps_and_publishes_it() {
    let (mut app, project, repository) = app_with_project();
    let id = ChannelId::new();
    let conversation = app.durable.cards[0].id;
    let mut channel = local(id, project, "mine", 0);
    channel.title_source = crate::model::ChannelTitleSource::Prompt;
    channel.cards = vec![conversation];
    channel.focused = Some(conversation);
    Arc::make_mut(&mut app.durable).channels.push(channel);
    let published = intercept_publishes(&mut app);

    let _ = app.apply_workspace_model_update(ModelUpdate::Connected(registry(Vec::new())));

    let kept = app
        .durable
        .channels
        .iter()
        .find(|channel| channel.id == id)
        .expect("a registry that never held a channel cannot retire it");
    assert_eq!(
        kept.cards,
        vec![conversation],
        "the cards inside it are untouched"
    );
    assert!(
        app.durable
            .cards
            .iter()
            .find(|card| card.id == conversation)
            .is_some_and(|card| card.existence.is_open()),
        "nothing was tombstoned"
    );
    let edits = drain(&published);
    assert!(edits.iter().any(|edit| matches!(edit,ModelEdit::SeedView{scope,panes,..} if *scope==crate::workspace_model::views::Scope::Channel(id) && panes.iter().any(|pane|pane.id==conversation))));
    assert_eq!(
        edits
            .into_iter()
            .filter(|edit| !matches!(edit, ModelEdit::SeedView { .. }))
            .collect::<Vec<_>>(),
        vec![
            ModelEdit::OpenProject {
                kind: Default::default(),
                repository: repository.clone(),
                name: "repo".into(),
            },
            ModelEdit::CreateChannel {
                id,
                repository,
                // The title the user has already seen, not an empty one: a
                // channel being introduced is not a channel being created.
                title: "mine".into(),
                title_source: crate::model::ChannelTitleSource::Prompt,
                checkout: None,
            },
        ],
        "the project comes first, or the channel would have no owner"
    );
}

/// A channel this window made in a worktree that was removed, or stopped being
/// a worktree, before the registry heard of it is introduced in the main
/// checkout, where its panes already start: the registry would refuse such a
/// checkout every time the channel was offered.
#[test]
fn an_unshared_channel_whose_worktree_fails_is_published_in_the_main_checkout() {
    let (mut app, project, _) = app_with_project();
    let directory = tempfile::tempdir().unwrap();
    let removed = ChannelId::new();
    let broken = ChannelId::new();
    let mut gone = local(removed, project, "gone", 0);
    gone.checkout = Some(directory.path().join("removed-worktree"));
    let mut plain = local(broken, project, "plain directory", 1);
    plain.checkout = Some(directory.path().to_path_buf());
    Arc::make_mut(&mut app.durable)
        .channels
        .extend([gone, plain]);
    let published = intercept_publishes(&mut app);

    let _ = app.apply_workspace_model_update(ModelUpdate::Connected(registry(Vec::new())));

    let created = drain(&published)
        .into_iter()
        .filter_map(|edit| match edit {
            ModelEdit::CreateChannel { id, checkout, .. } => Some((id, checkout)),
            _ => None,
        })
        .collect::<HashMap<_, _>>();
    assert_eq!(created.get(&removed), Some(&None));
    assert_eq!(created.get(&broken), Some(&None));
}

/// Once this window has recorded that the registry holds a channel, the
/// registry's silence about it does mean something: it was deleted somewhere
/// else, and this window honours that through its own deletion path.
#[test]
fn a_window_joining_after_a_shared_channel_was_deleted_elsewhere_retires_it() {
    let (mut app, project, _repository) = app_with_project();
    let id = ChannelId::new();
    let conversation = app.durable.cards[0].id;
    let identity = uuid::Uuid::new_v4();
    let mut channel = local(id, project, "shared then deleted", 4);
    channel.shared = true;
    channel.cards = vec![conversation];
    channel.focused = Some(conversation);
    let durable = Arc::make_mut(&mut app.durable);
    durable.channels.push(channel);
    durable.workspace_model_identity = Some(identity);
    let published = intercept_publishes(&mut app);

    let _ = app.apply_workspace_model_update(ModelUpdate::Connected(WorkspaceModel {
        identity,
        ..registry(Vec::new())
    }));

    let _ = app.handle_channel_ui(super::super::channel_ui::Action::SessionsClosed(
        id,
        uuid::Uuid::new_v4(),
        Ok(()),
    ));

    assert!(
        !app.durable.channels.iter().any(|channel| channel.id == id),
        "a channel this window shared and the registry no longer has was \
             deleted elsewhere"
    );
    assert!(
        app.durable
            .cards
            .iter()
            .find(|card| card.id == conversation)
            .is_some_and(|card| !card.existence.is_open()),
        "it retires through the local deletion path, which tombstones its \
             cards rather than erasing them"
    );
    // The retirement goes through `delete_channel`, which publishes the
    // deletion right back - a no-op the registry answers cheaply, and the
    // price of one deletion path that behaves identically whoever asked.
    // What must never happen is a create: that would argue with a deletion
    // and resurrect the channel in every other window.
    assert!(
        !drain(&published)
            .iter()
            .any(|edit| matches!(edit, ModelEdit::CreateChannel { .. })),
        "a deletion is honoured, never argued with by republishing"
    );
}

/// A shared flag is a claim about one registry. A replaced daemon or a wiped
/// data directory is a different registry, so every flag is stale and
/// nothing may be retired on its authority.
#[test]
fn a_replaced_registry_retires_nothing_and_is_told_what_this_window_has() {
    let (mut app, project, _repository) = app_with_project();
    let id = ChannelId::new();
    let mut channel = local(id, project, "mine", 4);
    channel.shared = true;
    let durable = Arc::make_mut(&mut app.durable);
    durable.channels.push(channel);
    durable.workspace_model_identity = Some(uuid::Uuid::new_v4());
    let published = intercept_publishes(&mut app);

    let replacement = registry(Vec::new());
    let identity = replacement.identity;
    let _ = app.apply_workspace_model_update(ModelUpdate::Connected(replacement));

    let kept = app
        .durable
        .channels
        .iter()
        .find(|channel| channel.id == id)
        .expect("a registry this window never shared with cannot retire it");
    assert!(
        !kept.shared,
        "the flag was a claim about the registry that is gone"
    );
    assert_eq!(app.durable.workspace_model_identity, Some(identity));
    assert!(
        drain(&published)
            .iter()
            .any(|edit| matches!(edit, ModelEdit::CreateChannel { id: published, .. } if *published == id)),
        "the replacement registry is told about it instead"
    );
}

/// Publisher replies and observer echoes use separate connections. A saved
/// channel can receive its real rank before its create echo reaches the
/// observer; a later channel's reply must not make that first one pending.
#[test]
fn startup_republication_keeps_ranked_channels_stable_when_create_echoes_arrive_late() {
    fn ranks(app: &Cagents) -> Vec<(ChannelId, u32)> {
        crate::sidebar::channels_in_display_order(&app.sidebar_model.projects[0].channels)
            .into_iter()
            .map(|channel| (channel.id, channel.ordinal))
            .collect()
    }
    let (mut app, project, repository) = app_with_project();
    let first = ChannelId::new();
    let second = ChannelId::new();
    let mut first_saved = local(first, project, "Saved first", 20);
    first_saved.shared = true;
    let mut second_saved = local(second, project, "Saved second", 21);
    second_saved.shared = true;
    let durable = Arc::make_mut(&mut app.durable);
    durable.channels = vec![first_saved, second_saved];
    durable.active_channel = Some(second);
    durable.workspace_model_identity = Some(uuid::Uuid::new_v4());
    assert_eq!(app.workspace_model.identity, NO_REGISTRY);
    let published = intercept_publishes(&mut app);
    let replacement = registry(Vec::new());
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(replacement)));
    let edits = drain(&published);
    let creates = edits
        .iter()
        .filter_map(|edit| match edit {
            ModelEdit::CreateChannel { id, .. } => Some((*id, edit.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        creates.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [first, second]
    );
    assert!(
        app.workspace_model.channels.is_empty(),
        "publication is not an observer acknowledgement"
    );
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: 1,
        edit: ModelEdit::OpenProject {
            repository: repository.clone(),
            name: "repo".into(),
            kind: ProjectKind::Git,
        },
    }));
    // The reply wins the race against the originating create's observer
    // echo, precisely the interleaving possible during startup republishing.
    let first_reply = rank_receipt(&app, first, 1, 2);
    let ModelUpdate::Ranked {
        model: first_snapshot,
        ..
    } = first_reply.clone()
    else {
        unreachable!()
    };
    drop(app.apply_workspace_model_update(first_reply));
    assert_eq!(ranks(&app), [(second, 2), (first, 1)]);
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: 2,
        edit: creates[0].1.clone(),
    }));
    assert_eq!(ranks(&app), [(second, 2), (first, 1)]);
    drop(app.apply_workspace_model_update(rank_receipt(&app, second, 2, 4)));
    assert_eq!(
        ranks(&app),
        [(second, 2), (first, 1)],
        "a late echo cannot turn the first authoritative rank back into a newer provisional rank"
    );
    // The observer took this A-only read before B's publisher committed,
    // but delivered it after the B reply. It must not retire selected B.
    drop(app.apply_workspace_model_update(ModelUpdate::Read(first_snapshot)));
    assert_eq!(ranks(&app), [(second, 2), (first, 1)]);
    assert_eq!(app.durable.active_channel, Some(second));
    assert!(
        app.durable
            .channels
            .iter()
            .any(|channel| channel.id == second)
    );
    assert_eq!(app.shared_views.snapshot_sequence, 4);
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: 4,
        edit: creates[1].1.clone(),
    }));
    assert_eq!(ranks(&app), [(second, 2), (first, 1)]);
    let mut settled = app.workspace_model.clone();
    settled.sequence = 5;
    settled.channels = vec![
        shared(first, &repository, "Saved first", 1),
        shared(second, &repository, "Saved second", 2),
    ];
    drop(app.apply_workspace_model_update(ModelUpdate::Read(settled)));
    assert_eq!(ranks(&app), [(second, 2), (first, 1)]);
    assert!(app.durable.channels.iter().all(|channel| channel.shared));
}

#[test]
fn delayed_rank_receipts_preserve_newer_metadata_and_do_not_revive_deleted_channels() {
    let (mut app, project, repository) = app_with_project();
    let id = ChannelId::new();
    app.workspace_model = registry(Vec::new());
    Arc::make_mut(&mut app.durable)
        .channels
        .push(local(id, project, "Original", 1));
    let receipt = rank_receipt(&app, id, 7, 2);
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: 2,
        edit: ModelEdit::CreateChannel {
            id,
            repository: repository.clone(),
            title: "Original".into(),
            title_source: Default::default(),
            checkout: None,
        },
    }));
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: 3,
        edit: ModelEdit::RetitleChannel {
            id,
            title: "Newer user title".into(),
        },
    }));
    assert_eq!(
        app.workspace_model.channel(id).unwrap().ordinal,
        PENDING_ORDINAL
    );
    drop(app.apply_workspace_model_update(receipt.clone()));
    let canonical = app.workspace_model.channel(id).unwrap();
    assert_eq!(canonical.ordinal, 7);
    assert_eq!(canonical.title, "Newer user title");
    assert_eq!(app.workspace_model.sequence, 3);
    assert_eq!(app.shared_views.snapshot_sequence, 2);
    let local = app
        .durable
        .channels
        .iter()
        .find(|channel| channel.id == id)
        .unwrap();
    assert_eq!(local.ordinal, 7);
    assert_eq!(local.title, "Newer user title");
    assert!(local.shared);
    // Ordinary remote pending channels still take a newer provisional rank.
    let remote = ChannelId::new();
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: 4,
        edit: ModelEdit::CreateChannel {
            id: remote,
            repository,
            title: "Still pending".into(),
            title_source: Default::default(),
            checkout: None,
        },
    }));
    let before_duplicate = app.durable.channels.clone();
    drop(app.apply_workspace_model_update(receipt.clone()));
    assert_eq!(app.durable.channels, before_duplicate);
    assert_eq!(
        app.workspace_model.channel(remote).unwrap().ordinal,
        PENDING_ORDINAL
    );
    assert_eq!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == remote)
            .unwrap()
            .ordinal,
        8
    );
    let mut newer = app.workspace_model.clone();
    newer.sequence = 5;
    newer
        .channels
        .iter_mut()
        .find(|channel| channel.id == id)
        .unwrap()
        .ordinal = 9;
    newer
        .channels
        .iter_mut()
        .find(|channel| channel.id == remote)
        .unwrap()
        .ordinal = 10;
    drop(app.apply_workspace_model_update(ModelUpdate::Read(newer)));
    drop(app.apply_workspace_model_update(receipt.clone()));
    assert_eq!(app.workspace_model.channel(id).unwrap().ordinal, 9);
    assert_eq!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .unwrap()
            .ordinal,
        9
    );
    // A real newer delete goes through the application's retirement path.
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: 6,
        edit: ModelEdit::DeleteChannel { id },
    }));
    assert!(app.workspace_model.channel(id).is_none());
    let durable = Arc::clone(&app.durable);
    let model = app.workspace_model.clone();
    drop(app.apply_workspace_model_update(receipt));
    assert_eq!(app.durable, durable);
    assert_eq!(app.workspace_model, model);
    assert!(app.workspace_model.channel(id).is_none());
}

#[test]
fn rank_receipts_repair_duplicate_proof_and_reject_unknown_foreign_or_stale_sources() {
    for invalid in [
        "unknown",
        "foreign",
        "stale",
        "pending",
        "absent",
        "newer-delete",
    ] {
        let (mut app, project, _) = app_with_project();
        let id = ChannelId::new();
        app.workspace_model = registry(Vec::new());
        app.workspace_model.sequence = 10;
        app.shared_views.snapshot_sequence = 5;
        let mut local = local(id, project, "Retained", 7);
        local.shared = true;
        Arc::make_mut(&mut app.durable).channels.push(local);
        let ModelUpdate::Ranked { mut model, .. } = rank_receipt(&app, id, 7, 10) else {
            unreachable!()
        };
        match invalid {
            "unknown" => model.identity = NO_REGISTRY,
            "foreign" => model.identity = uuid::Uuid::new_v4(),
            "stale" => model.sequence = 4,
            "pending" => model.channels[0].ordinal = PENDING_ORDINAL,
            "absent" => Arc::make_mut(&mut app.durable).channels.clear(),
            "newer-delete" => model.sequence = 9,
            _ => unreachable!(),
        }
        let durable = Arc::clone(&app.durable);
        let current = app.workspace_model.clone();
        let before_snapshot = app.shared_views.snapshot_sequence;
        let published = intercept_publishes(&mut app);
        drop(app.apply_workspace_model_update(ModelUpdate::Ranked { channel: id, model }));
        assert_eq!(app.durable, durable, "{invalid}");
        assert_eq!(app.workspace_model, current, "{invalid}");
        assert_eq!(
            app.shared_views.snapshot_sequence, before_snapshot,
            "{invalid}"
        );
        assert!(drain(&published).is_empty(), "{invalid}");
    }
    // A duplicate real acknowledgement must repair a missing canonical
    // cache even when local rank/shared already match. Only complete proof
    // may advance this sequence and fence older observer snapshots.
    let (mut app, project, _) = app_with_project();
    let id = ChannelId::new();
    app.workspace_model = registry(Vec::new());
    app.workspace_model.sequence = 10;
    let mut local = local(id, project, "Retained", 7);
    local.shared = true;
    Arc::make_mut(&mut app.durable).channels.push(local);
    app.refresh_sidebar_channels();
    assert!(app.workspace_model.channel(id).is_none());
    drop(app.apply_workspace_model_update(rank_receipt(&app, id, 7, 10)));
    assert_eq!(app.workspace_model.channel(id).unwrap().ordinal, 7);
    assert_eq!(app.shared_views.snapshot_sequence, 10);
    assert_eq!(app.durable.channels[0].ordinal, 7);
    assert!(app.durable.channels[0].shared);
}

/// The rank the registry assigns replaces the provisional one a local
/// creation started with, so the window that created a channel and the
/// window that adopted it agree on where it sorts. The same answer is the
/// proof the registry holds it.
#[test]
fn six_creations_stay_above_saved_mixed_rows_through_pending_echoes_rank_leap_and_reopen() {
    use crate::sidebar::{SelectionRow, SidebarAction};
    use crate::workspace_model::{NavigationItem, NavigationOrder};
    fn channel_rows(app: &Cagents) -> Vec<ChannelId> {
        crate::sidebar::visible_selection_rows(&app.sidebar_model, &app.sidebar_state, false)
            .into_iter()
            .filter_map(|row| match row {
                SelectionRow::Channel(id) => Some(id),
                _ => None,
            })
            .collect()
    }
    let (mut app, project, repository) = app_with_project();
    // A connected publisher keeps a channel's seed pending until the
    // registry answers, as a running window's does.
    let (tx, _requests) = test_model_channel();
    app.workspace_model_tx = tx;
    let storage = tempfile::tempdir().unwrap();
    let database = storage.path().join("channel-order.sqlite3");
    let boot = crate::model::BootId::new("channel-order-reopen");
    let (session, _) = crate::session::SessionRuntime::open(&database, boot.clone()).unwrap();
    app.session = Some(session);
    let main = crate::model::WorktreeId::new();
    app.sidebar_model.projects[0].worktrees = vec![crate::sidebar::SidebarWorktree::from_durable(
        main,
        "/repo".into(),
        Some("main".into()),
    )];
    let saved: Vec<_> = (0..6)
        .map(|index| {
            let mut channel = local(
                ChannelId::new(),
                project,
                &format!("Saved {index}"),
                index + 20,
            );
            channel.shared = true;
            channel
        })
        .collect();
    Arc::make_mut(&mut app.durable).channels = saved.clone();
    let manual: Vec<_> = std::iter::once(NavigationItem::Worktree("/repo".into()))
        .chain(
            saved
                .iter()
                .map(|channel| NavigationItem::Channel(channel.id)),
        )
        .collect();
    app.workspace_model = registry(
        saved
            .iter()
            .map(|channel| shared(channel.id, &repository, &channel.title, channel.ordinal))
            .collect(),
    );
    app.workspace_model.navigation_orders.push(NavigationOrder {
        repository: repository.clone(),
        items: manual.clone(),
    });
    drop(app.reconcile_workspace_model());
    // Put real user work in an existing channel before creating others.
    // Opening it through the actual update creates its inert native draft;
    // typing promotes the placeholder, without starting a provider.
    drop(
        app.update(Message::ChannelUi(super::super::channel_ui::Action::Open(
            saved[2].id,
        ))),
    );
    // Its composer waits until the channel's shared panes are admitted.
    assert!(app.workspace.cards().is_empty());
    app.acknowledge_view_edit(
        crate::workspace_model::views::Scope::Channel(saved[2].id),
        None,
    );
    drop(app.reconcile_shared_views());
    let draft_card = app.workspace.focused().expect("the old composer opens");
    let target = match app.cards.get(&draft_card) {
        Some(CardRuntime::Chat(runtime)) => runtime.target,
        _ => panic!("the channel's default pane must be a native draft"),
    };
    let draft = "Keep this edited draft  \nUnicode 🦀 and exact trailing spaces  ";
    drop(
        app.update(Message::ChatUi(super::super::chat_ui::Action::Composer(
            target,
            cosmic::widget::text_editor::Action::Edit(cosmic::widget::text_editor::Edit::Paste(
                Arc::new(draft.to_owned()),
            )),
        ))),
    );
    assert_eq!(app.persistent_workspace().chat_drafts[&draft_card], draft);
    let saved = app.durable.channels.clone();
    let published = intercept_publishes(&mut app);
    let mut created = Vec::new();
    for _ in 0..6 {
        drop(app.update(Message::ChannelUi(
            super::super::channel_ui::Action::Create(project),
        )));
        let id = app
            .durable
            .active_channel
            .expect("creation selects its exact new channel");
        created.push(id);
        // Inspect the real update's returned projection before any manual
        // refresh; the reveal task must address a row already mounted next frame.
        assert_eq!(
            &channel_rows(&app)[..created.len()],
            created.iter().rev().copied().collect::<Vec<_>>()
        );
        assert!(!app.modal_is_open());
        assert!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == id)
                .unwrap()
                .focused
                .is_some(),
            "the new composer opens immediately"
        );
    }
    let edits = drain(&published);
    assert_eq!(
        edits
            .iter()
            .filter(|edit| matches!(edit, ModelEdit::CreateChannel { .. }))
            .count(),
        6
    );
    assert!(
        !edits.iter().any(|edit| matches!(
            edit,
            ModelEdit::ReorderNavigation { .. } | ModelEdit::ResetNavigation { .. }
        )),
        "creation must not rewrite the user's saved relative order"
    );
    // Folded creates prove existence but still carry the pending sentinel.
    // They must neither poison max+1 nor turn their source order into ties.
    for (index, id) in created.iter().enumerate() {
        drop(app.apply_workspace_model_update(ModelUpdate::Edited {
            sequence: 100 + index as u64,
            edit: ModelEdit::CreateChannel {
                id: *id,
                repository: repository.clone(),
                title: String::new(),
                title_source: Default::default(),
                checkout: None,
            },
        }));
        assert_eq!(
            &channel_rows(&app)[..6],
            created.iter().rev().copied().collect::<Vec<_>>()
        );
        assert_ne!(app.next_provisional_ordinal(), PENDING_ORDINAL);
    }
    drop(app.apply_workspace_model_update(rank_receipt(&app, created[0], 600, 100)));
    assert_eq!(
        &channel_rows(&app)[..6],
        created.iter().rev().copied().collect::<Vec<_>>()
    );
    assert_eq!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == created[1])
            .unwrap()
            .ordinal,
        601
    );
    assert_eq!(
        app.workspace_model.channel(created[1]).unwrap().ordinal,
        PENDING_ORDINAL,
        "a provisional UI rank never claims a registry assignment"
    );
    for (index, id) in created.iter().enumerate().skip(1) {
        drop(app.apply_workspace_model_update(rank_receipt(
            &app,
            *id,
            600 + index as u32,
            100 + index as u64,
        )));
        assert_eq!(
            &channel_rows(&app)[..6],
            created.iter().rev().copied().collect::<Vec<_>>(),
            "each acknowledgement preserves the still newer pending creations"
        );
    }
    let mut settled = app.workspace_model.clone();
    settled.sequence = 200;
    for (index, id) in created.iter().enumerate() {
        settled
            .channels
            .iter_mut()
            .find(|channel| channel.id == *id)
            .unwrap()
            .ordinal = 600 + index as u32;
    }
    drop(app.apply_workspace_model_update(ModelUpdate::Read(settled.clone())));
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: 101,
        edit: ModelEdit::CreateChannel {
            id: created[0],
            repository: repository.clone(),
            title: "stale echo".into(),
            title_source: Default::default(),
            checkout: None,
        },
    }));
    assert_eq!(
        &channel_rows(&app)[..6],
        created.iter().rev().copied().collect::<Vec<_>>()
    );
    assert_eq!(app.sidebar_model.projects[0].navigation_order, manual);
    for original in &saved {
        assert_eq!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == original.id),
            Some(original),
            "saved channel data survives all six creations"
        );
    }
    let rows =
        crate::sidebar::visible_selection_rows(&app.sidebar_model, &app.sidebar_state, false);
    assert_eq!(rows[7], SelectionRow::Worktree(project, main));
    assert_eq!(app.persistent_workspace().chat_drafts[&draft_card], draft);
    assert!(app.shared_chat_has_local_work(draft_card));
    let before_restart = app.durable.channels.clone();
    // No explicit snapshot here: creation and model reconciliation must have
    // saved their own result through the production persistence path.
    let mut session = app.session.take().unwrap();
    session.shutdown();
    assert_eq!(session.last_error(), None);
    let (mut restored_session, restored) =
        crate::session::SessionRuntime::open(&database, boot).unwrap();
    let restored = restored.expect("the automatic writes saved this workspace");
    assert_eq!(restored.chat_drafts[&draft_card], draft);
    // Empty default composers are intentionally omitted from storage, so
    // compare identities/ranks and the edited channel's real membership.
    assert_eq!(
        restored
            .channels
            .iter()
            .map(|channel| (channel.id, channel.ordinal))
            .collect::<Vec<_>>(),
        before_restart
            .iter()
            .map(|channel| (channel.id, channel.ordinal))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        restored
            .channels
            .iter()
            .find(|channel| channel.id == saved[2].id),
        saved.iter().find(|channel| channel.id == saved[2].id)
    );
    let (mut reopened, _, _) = app_with_project();
    reopened.sidebar_model = app.sidebar_model.clone();
    for project in &mut reopened.sidebar_model.projects {
        project.channels.clear();
    }
    reopened.chat_ui = super::super::chat_ui::State::with_drafts(restored.chat_drafts.clone());
    reopened.durable = Arc::new(restored);
    reopened.workspace_model = settled.clone();
    reopened.refresh_sidebar_channels();
    assert_eq!(channel_rows(&reopened), channel_rows(&app));
    assert_eq!(reopened.durable.active_channel, created.last().copied());
    drop(
        reopened.update(Message::ChannelUi(super::super::channel_ui::Action::Open(
            saved[2].id,
        ))),
    );
    assert_eq!(reopened.workspace.focused(), Some(draft_card));
    assert_eq!(
        reopened.persistent_workspace().chat_drafts[&draft_card],
        draft
    );
    restored_session.shutdown();
    // Another window derives exactly the same order from authoritative IDs.
    let (mut peer, _, _) = app_with_project();
    peer.sidebar_model = app.sidebar_model.clone();
    peer.workspace_model = settled;
    drop(peer.reconcile_workspace_model());
    assert_eq!(channel_rows(&peer), channel_rows(&app));
    // Activity and ordinary reopening are not a creation or reorder intent.
    let before = channel_rows(&app);
    drop(app.handle_channel_ui(super::super::channel_ui::Action::Open(saved[2].id)));
    app.refresh_sidebar_channels();
    assert_eq!(channel_rows(&app), before);
    assert!(
        matches!(crate::sidebar::workspace_cycle(&app.sidebar_model, &app.sidebar_state)[0], SidebarAction::OpenChannel { channel_id } if channel_id == created[5])
    );
}

#[test]
fn folded_remote_creations_and_pending_rank_ceiling_keep_newest_first_without_wrap() {
    let (mut app, project, repository) = app_with_project();
    app.workspace_model = registry(Vec::new());
    let ids: Vec<_> = (0..6).map(|_| ChannelId::new()).collect();
    for (index, id) in ids.iter().enumerate() {
        drop(app.apply_workspace_model_update(ModelUpdate::Edited {
            sequence: index as u64 + 1,
            edit: ModelEdit::CreateChannel {
                id: *id,
                repository: repository.clone(),
                title: format!("Remote {index}"),
                title_source: Default::default(),
                checkout: None,
            },
        }));
        let rows =
            crate::sidebar::channels_in_display_order(&app.sidebar_model.projects[0].channels);
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            ids[..=index].iter().rev().copied().collect::<Vec<_>>()
        );
    }
    let old = ChannelId::new();
    let mut high = local(old, project, "Last authoritative rank", PENDING_ORDINAL - 1);
    high.shared = true;
    Arc::make_mut(&mut app.durable).channels.insert(0, high);
    app.workspace_model.channels.insert(
        0,
        shared(
            old,
            &repository,
            "Last authoritative rank",
            PENDING_ORDINAL - 1,
        ),
    );
    app.sort_durable_channels();
    app.refresh_sidebar_channels();
    let rows = crate::sidebar::channels_in_display_order(&app.sidebar_model.projects[0].channels);
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        ids.iter().rev().copied().chain([old]).collect::<Vec<_>>()
    );
    assert!(
        app.durable
            .channels
            .iter()
            .filter(|channel| channel.id != old)
            .all(|channel| channel.ordinal == PENDING_ORDINAL)
    );
    assert_eq!(
        app.next_provisional_ordinal(),
        PENDING_ORDINAL,
        "saturation stays explicitly pending rather than wrapping into an older authoritative rank"
    );
}

#[test]
fn the_registry_rank_replaces_the_provisional_one_and_records_the_sharing() {
    let (mut app, project, _repository) = app_with_project();
    let id = ChannelId::new();
    app.workspace_model = registry(Vec::new());
    let provisional = app.next_provisional_ordinal();
    Arc::make_mut(&mut app.durable)
        .channels
        .push(local(id, project, "", provisional));
    assert_ne!(provisional, 11);

    let _ = app.apply_workspace_model_update(rank_receipt(&app, id, 11, 1));

    let ranked = app
        .durable
        .channels
        .iter()
        .find(|channel| channel.id == id)
        .unwrap();
    assert_eq!(ranked.ordinal, 11);
    assert!(
        ranked.shared,
        "the registry answered about it, so it holds it"
    );
}

/// A window reconciles on its own events too - a repository finishing
/// loading is one - and those can happen before any daemon has answered.
/// The cached model is empty then, and that emptiness is the window's own,
/// not a registry's: acting on it would publish prematurely and, for a
/// channel a previous session had shared, retire it against nobody's word.
#[test]
fn a_window_that_has_reached_no_registry_yet_neither_retires_nor_publishes() {
    let (mut app, project, _repository) = app_with_project();
    let id = ChannelId::new();
    let mut channel = local(id, project, "mine", 4);
    channel.shared = true;
    Arc::make_mut(&mut app.durable).channels.push(channel);
    let published = intercept_publishes(&mut app);
    assert_eq!(app.workspace_model.identity, NO_REGISTRY);

    let _ = app.reconcile_workspace_model();

    assert!(
        app.durable.channels.iter().any(|channel| channel.id == id),
        "no registry has spoken, so nothing it did not say can be acted on"
    );
    assert!(drain(&published).is_empty());
}

/// Reconciling against a model whose navigation orders the sidebar already
/// shows leaves the sidebar's revision alone, so the navigation list keeps
/// its cache; a changed order still reaches the sidebar.
#[test]
fn reconciling_unchanged_navigation_orders_keeps_the_sidebar_revision() {
    use crate::sidebar::Revisioned;
    use crate::workspace_model::{NavigationItem, NavigationOrder};

    let (mut app, _, repository) = app_with_project();
    let mut model = registry(Vec::new());
    model.projects.push(ModelProject {
        repository: repository.clone(),
        name: "repo".into(),
        kind: ProjectKind::Git,
        ordinal: 0,
    });
    model.navigation_orders.push(NavigationOrder {
        repository: repository.clone(),
        items: vec![NavigationItem::Worktree("/repo/worktree".into())],
    });
    app.workspace_model = model;
    drop(app.reconcile_workspace_model());
    assert_eq!(
        app.sidebar_model.projects[0].navigation_order,
        vec![NavigationItem::Worktree("/repo/worktree".into())]
    );
    let settled = Revisioned::revision(&app.sidebar_model.projects);

    drop(app.reconcile_workspace_model());
    assert_eq!(
        Revisioned::revision(&app.sidebar_model.projects),
        settled,
        "an order the sidebar already shows takes no new revision"
    );

    app.workspace_model.navigation_orders[0].items =
        vec![NavigationItem::Worktree("/repo/other".into())];
    drop(app.reconcile_workspace_model());
    assert_ne!(Revisioned::revision(&app.sidebar_model.projects), settled);
    assert_eq!(
        app.sidebar_model.projects[0].navigation_order,
        vec![NavigationItem::Worktree("/repo/other".into())]
    );
}

/// A new shared channel and an agent change, as the observer reports a
/// spawn elsewhere: edges whose only consequence here is the complete
/// reconciliation that adopts what they installed.
fn spawn_burst(baseline: &WorkspaceModel, repository: &Path) -> (ChannelId, Vec<ModelUpdate>) {
    let created = shared(ChannelId::new(), repository, "Spawned", 1);
    // The agent's start, then its rename: each edge changes its record, as
    // an edge that changed nothing would skip the reconciliation.
    let mut agent = crate::agent_execution::Agent::fixture_placed_in(created.id);
    agent.spec.target.repository = repository.to_path_buf();
    let mut renamed = agent.clone();
    renamed.name = Some("worker".into());
    let agents_changed =
        |sequence: u64, agent: &crate::agent_execution::Agent| ModelUpdate::Changed {
            sequence: baseline.sequence + sequence,
            edit: ModelEdit::AgentsChanged,
            changes: EdgeChanges {
                identity: Some(baseline.identity),
                effects: EditEffects {
                    agents: vec![agent.clone()],
                    ..Default::default()
                },
                ..Default::default()
            },
        };
    let edges = vec![
        ModelUpdate::Changed {
            sequence: baseline.sequence + 1,
            edit: ModelEdit::CreateChannel {
                id: created.id,
                repository: repository.to_path_buf(),
                title: created.title.clone(),
                title_source: Default::default(),
                checkout: None,
            },
            changes: EdgeChanges {
                identity: Some(baseline.identity),
                effects: EditEffects {
                    channels: vec![created.clone()],
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        agents_changed(2, &agent),
        agents_changed(3, &renamed),
    ];
    (created.id, edges)
}

fn window_with_registry() -> (Cagents, WorkspaceModel, PathBuf) {
    let (mut app, _, repository) = app_with_project();
    let mut baseline = registry(vec![shared(ChannelId::new(), &repository, "First", 0)]);
    baseline.sequence = 100;
    assert!(app.install_workspace_model_snapshot(baseline.clone()));
    drop(app.reconcile_workspace_model());
    (app, baseline, repository)
}

/// Delivers `updates` as the observer's stream does when they queued
/// together: the first as the message, the rest waiting in the channel.
fn deliver_run(app: &mut Cagents, updates: Vec<ModelUpdate>) {
    let (sender, receiver) = async_channel::unbounded();
    app.workspace_model_rx = receiver;
    let mut updates = updates.into_iter();
    let first = updates.next().expect("a run has a first update");
    for update in updates {
        sender.send_blocking(update).unwrap();
    }
    drop(app.update(Message::WorkspaceModelArrived(
        super::super::ingress::Arrival::detached(first),
    )));
    assert!(
        app.workspace_model_rx.is_empty(),
        "the update drains its run"
    );
}

fn channel_ids(app: &Cagents) -> Vec<ChannelId> {
    app.durable
        .channels
        .iter()
        .map(|channel| channel.id)
        .collect()
}

#[test]
fn a_run_of_observed_edges_reconciles_once_and_ends_where_each_alone_would() {
    let titles = |app: &Cagents| {
        app.durable
            .channels
            .iter()
            .map(|channel| channel.title.clone())
            .collect::<Vec<_>>()
    };
    // Each edge alone: one complete reconciliation apiece.
    let (mut each, baseline, repository) = window_with_registry();
    let (created, edges) = spawn_burst(&baseline, &repository);
    let count = edges.len();
    let before = complete_reconciliations();
    for edge in edges {
        drop(each.update(Message::WorkspaceModelObserved(edge)));
    }
    assert_eq!(complete_reconciliations() - before, count);
    assert!(channel_ids(&each).contains(&created));

    // The same edges as the run the observer stream delivers: one
    // reconciliation, after the last edge, and the same window.
    let (mut run, baseline, repository) = window_with_registry();
    let (created, edges) = spawn_burst(&baseline, &repository);
    let before = complete_reconciliations();
    deliver_run(&mut run, edges);
    assert_eq!(
        complete_reconciliations() - before,
        1,
        "a run reconciles once, after its last edge"
    );
    assert!(channel_ids(&run).contains(&created));
    assert_eq!(titles(&run), titles(&each));
    assert_eq!(run.workspace_model.sequence, each.workspace_model.sequence);
    assert!(
        run.observed_run.is_none(),
        "the run's account closes with it"
    );
}

#[test]
fn a_reconnection_in_a_run_meets_the_window_reconciled_and_leaves_nothing_owed() {
    let (mut app, baseline, repository) = window_with_registry();
    let (created, mut edges) = spawn_burst(&baseline, &repository);
    edges.truncate(1);
    // A reconnection's complete model reconciles inside its own handler. It
    // meets the window as the edge before it left it, reconciled, and its
    // own complete pass leaves the run nothing to do at its end.
    let mut connected = app.workspace_model.clone();
    connected.sequence = baseline.sequence + 1;
    connected
        .channels
        .push(shared(created, &repository, "Spawned", 1));
    edges.push(ModelUpdate::ConnectedClosed {
        model: connected,
        closed: Vec::new(),
    });
    let before = complete_reconciliations();
    deliver_run(&mut app, edges);
    assert_eq!(
        complete_reconciliations() - before,
        2,
        "the edge's owed pass, then the reconnection's own, and none after"
    );
    assert!(channel_ids(&app).contains(&created));
}

#[test]
fn a_run_applies_its_edges_in_the_order_they_were_sent() {
    let edges = |baseline: &WorkspaceModel, repository: &Path, id: ChannelId| {
        let created = shared(id, repository, "Spawned", 1);
        let retitled = |sequence: u64, title: &str| ModelUpdate::Changed {
            sequence: baseline.sequence + sequence,
            edit: ModelEdit::RetitleChannel {
                id,
                title: title.into(),
            },
            changes: EdgeChanges {
                identity: Some(baseline.identity),
                effects: EditEffects {
                    channels: vec![ModelChannel {
                        title: title.into(),
                        ..created.clone()
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        };
        vec![
            ModelUpdate::Changed {
                sequence: baseline.sequence + 1,
                edit: ModelEdit::CreateChannel {
                    id,
                    repository: repository.to_path_buf(),
                    title: created.title.clone(),
                    title_source: Default::default(),
                    checkout: None,
                },
                changes: EdgeChanges {
                    identity: Some(baseline.identity),
                    effects: EditEffects {
                        channels: vec![created.clone()],
                        ..Default::default()
                    },
                    ..Default::default()
                },
            },
            retitled(2, "Parser cache"),
            ModelUpdate::Changed {
                sequence: baseline.sequence + 3,
                edit: ModelEdit::AgentsChanged,
                changes: EdgeChanges {
                    identity: Some(baseline.identity),
                    ..Default::default()
                },
            },
            retitled(4, "Parser cache work"),
        ]
    };
    let title = |app: &Cagents, id: ChannelId| {
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .map(|channel| channel.title.clone())
    };
    let id = ChannelId::new();
    let (mut each, baseline, repository) = window_with_registry();
    for edge in edges(&baseline, &repository, id) {
        drop(each.update(Message::WorkspaceModelObserved(edge)));
    }
    let (mut run, baseline, repository) = window_with_registry();
    deliver_run(&mut run, edges(&baseline, &repository, id));
    assert_eq!(title(&run, id).as_deref(), Some("Parser cache work"));
    assert_eq!(title(&run, id), title(&each, id));
    assert_eq!(run.workspace_model.sequence, baseline.sequence + 4);
    assert_eq!(run.workspace_model.sequence, each.workspace_model.sequence);
}

#[test]
fn an_update_of_another_kind_meets_the_window_reconciled_after_the_edges_before_it() {
    let (mut app, baseline, repository) = window_with_registry();
    let (created, mut edges) = spawn_burst(&baseline, &repository);
    let last = edges.pop().unwrap();
    // A failed reopen reconciles nothing itself; one at a time, it would
    // follow the edges' own reconciliation.
    edges.push(ModelUpdate::ReopenFailed {
        repository: repository.clone(),
        error: "refused".into(),
    });
    edges.push(last);
    let before = complete_reconciliations();
    deliver_run(&mut app, edges);
    assert_eq!(
        complete_reconciliations() - before,
        2,
        "the edges before it share one reconciliation, the edge after it another"
    );
    assert!(channel_ids(&app).contains(&created));
}

#[test]
fn agent_labels_share_the_reconciliation_of_the_edges_they_precede() {
    let (mut app, baseline, repository) = window_with_registry();
    let (created, mut edges) = spawn_burst(&baseline, &repository);
    let agent = crate::model::AgentId::new();
    edges.insert(
        0,
        ModelUpdate::AgentLabels(vec![(agent, "Tighten the parser cache".into())]),
    );
    let before = complete_reconciliations();
    deliver_run(&mut app, edges);
    assert_eq!(complete_reconciliations() - before, 1);
    assert!(channel_ids(&app).contains(&created));
    assert_eq!(
        app.chrome_ui.agent_labels.get(&agent).map(String::as_str),
        Some("Tighten the parser cache")
    );
}

/// One observer edge, as the real observer delivers it with its change set.
fn changed_edge(
    identity: uuid::Uuid,
    sequence: u64,
    edit: ModelEdit,
    effects: EditEffects,
    views: Vec<crate::workspace_model::views::View>,
) -> ModelUpdate {
    ModelUpdate::Changed {
        sequence,
        edit,
        changes: EdgeChanges {
            identity: Some(identity),
            views: ViewChanges {
                sequence,
                views,
                ..Default::default()
            },
            effects,
        },
    }
}

/// Applies `update` and answers whether it ran a complete reconciliation.
fn reconciles(app: &mut Cagents, update: ModelUpdate) -> bool {
    let before = complete_reconciliations();
    drop(app.apply_workspace_model_update(update));
    complete_reconciliations() != before
}

/// A copy of an edge this window already installed, such as the observer's
/// echo of its own receipt, changes no row and no view, so it skips the
/// complete reconciliation. Each kind an agent start produces is covered, and
/// the reconcile oracle checks every skip against a complete reconciliation.
#[test]
fn an_edge_that_changes_nothing_skips_the_complete_reconciliation() {
    use crate::workspace_model::{
        delta::ViewRevision,
        views::{Change, Pane, Scope, View},
    };
    let (mut app, project, repository) = app_with_project();
    let channel = ChannelId::new();
    let mut row = local(channel, project, "Work", 1);
    row.shared = true;
    Arc::make_mut(&mut app.durable).channels.push(row);
    app.workspace_model = registry(vec![shared(channel, &repository, "Work", 1)]);
    app.workspace_model.views.push(View::empty(channel));
    let identity = app.workspace_model.identity;
    let scope = Scope::Channel(channel);
    let pane = Pane {
        id: crate::model::CardId::new(),
        title: "docs".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        scope: None,
        kind: crate::model::CardKind::Browser(crate::model::BrowserSession {
            id: crate::model::BrowserId::new(),
            current_url: "https://example.com".parse().unwrap(),
            history: vec![],
            history_index: 0,
            scroll_x: 0.,
            scroll_y: 0.,
            fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
        }),
    };
    let revised = |before, after| EditEffects {
        views: vec![ViewRevision {
            scope: scope.clone(),
            before: Some(before),
            after: Some(after),
        }],
        ..Default::default()
    };
    let inserted = View {
        scope: scope.clone(),
        revision: 5,
        panes: vec![pane.clone()],
        retired: vec![],
    };
    let insert = ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: 0,
        operation: uuid::Uuid::new_v4(),
        change: Change::Insert {
            pane: pane.clone(),
            before: None,
        },
    };
    let mut agent = crate::agent_execution::Agent::fixture_placed_in(channel);
    agent.spec.target.repository = repository.clone();
    let agents = || EditEffects {
        agents: vec![agent.clone()],
        ..Default::default()
    };
    let mut renamed = agent.clone();
    renamed.name = Some("worker".into());
    let other = ChannelId::new();
    let seed = ModelEdit::SeedView {
        scope: Scope::Channel(other),
        panes: vec![],
        closed: vec![],
    };
    let seeded = View::empty(other);
    let seed_effects = || EditEffects {
        views: vec![ViewRevision {
            scope: Scope::Channel(other),
            before: None,
            after: Some(seeded.revision),
        }],
        ..Default::default()
    };
    let retitle = ModelEdit::RetitleChannel {
        id: channel,
        title: "Work".into(),
    };
    let titled = || EditEffects {
        channels: vec![shared(channel, &repository, "Work", 1)],
        ..Default::default()
    };
    // (kind, first delivery, its echo)
    let cases = [
        (
            "ChangeView.Insert",
            changed_edge(
                identity,
                10,
                insert.clone(),
                revised(0, 5),
                vec![inserted.clone()],
            ),
            changed_edge(identity, 10, insert, revised(0, 5), vec![inserted.clone()]),
        ),
        (
            "AgentsChanged",
            changed_edge(identity, 11, ModelEdit::AgentsChanged, agents(), vec![]),
            changed_edge(identity, 11, ModelEdit::AgentsChanged, agents(), vec![]),
        ),
        (
            "AgentsChanged, a later edge repeating the record",
            changed_edge(
                identity,
                12,
                ModelEdit::AgentsChanged,
                EditEffects {
                    agents: vec![renamed.clone()],
                    ..Default::default()
                },
                vec![],
            ),
            changed_edge(
                identity,
                13,
                ModelEdit::AgentsChanged,
                EditEffects {
                    agents: vec![renamed],
                    ..Default::default()
                },
                vec![],
            ),
        ),
        (
            "SeedView",
            changed_edge(
                identity,
                14,
                seed.clone(),
                seed_effects(),
                vec![seeded.clone()],
            ),
            changed_edge(identity, 14, seed, seed_effects(), vec![seeded]),
        ),
        (
            "RetitleChannel to the held title",
            changed_edge(identity, 15, retitle.clone(), titled(), vec![]),
            changed_edge(identity, 16, retitle, titled(), vec![]),
        ),
    ];
    for (kind, first, echo) in cases {
        if !kind.starts_with("RetitleChannel") {
            assert!(reconciles(&mut app, first), "{kind}: a change reconciles");
        } else {
            drop(app.apply_workspace_model_update(first));
        }
        let before = complete_reconciliations();
        let (_, projected) = app.apply_workspace_model_update_effect(echo);
        assert_eq!(
            complete_reconciliations(),
            before,
            "{kind}: an edge that changes nothing skips the reconciliation"
        );
        assert!(!projected, "{kind}: and the projection after it");
    }
    // Work only a complete reconciliation finishes is not skipped, even for
    // an edge that changed nothing.
    app.pending_repository_reconciliation
        .push(repository.clone());
    assert!(reconciles(
        &mut app,
        changed_edge(
            identity,
            17,
            ModelEdit::AgentsChanged,
            EditEffects::default(),
            vec![]
        ),
    ));
    assert!(app.pending_repository_reconciliation.is_empty());
    // Nor is a worktree discovered since the last reconciliation, which
    // decides where a card in it belongs.
    app.known_worktree_roots
        .insert(repository.join(".worktrees/feature"));
    assert!(reconciles(
        &mut app,
        changed_edge(
            identity,
            18,
            ModelEdit::AgentsChanged,
            EditEffects::default(),
            vec![]
        ),
    ));
    assert!(!reconciles(
        &mut app,
        changed_edge(
            identity,
            19,
            ModelEdit::AgentsChanged,
            EditEffects::default(),
            vec![]
        ),
    ));
    // An edge older than the last complete snapshot, holding a view revision
    // the window already passed, changes nothing either. (The snapshot
    // sequence is set by hand: only the fence matters here.)
    app.shared_views.snapshot_sequence = 40;
    let stale = View {
        scope: scope.clone(),
        revision: 3,
        panes: vec![],
        retired: vec![],
    };
    assert!(!reconciles(
        &mut app,
        changed_edge(
            identity,
            20,
            ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: 0,
                operation: uuid::Uuid::new_v4(),
                change: Change::Remove { id: pane.id },
            },
            revised(2, 3),
            vec![stale],
        ),
    ));
    assert_eq!(
        app.workspace_model
            .views
            .iter()
            .find(|view| view.scope == scope),
        Some(&inserted)
    );
    // An edge whose kind settles a lifecycle intent still reconciles when it
    // is older than the snapshot and carries no row.
    for (sequence, edit) in [
        (21, ModelEdit::DeleteChannel { id: other }),
        (
            22,
            ModelEdit::CloseProject {
                repository: repository.join("elsewhere"),
            },
        ),
    ] {
        assert!(
            reconciles(
                &mut app,
                changed_edge(identity, sequence, edit, EditEffects::default(), vec![]),
            ),
            "a stale lifecycle edge reconciles"
        );
    }
    // A newer view still reconciles.
    let removed = View {
        scope: scope.clone(),
        revision: 50,
        panes: vec![],
        retired: vec![pane.id],
    };
    assert!(reconciles(
        &mut app,
        changed_edge(
            identity,
            50,
            ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: 5,
                operation: uuid::Uuid::new_v4(),
                change: Change::Remove { id: pane.id },
            },
            EditEffects {
                closed: vec![pane.id],
                ..revised(5, 50)
            },
            vec![removed],
        ),
    ));
}

/// A window holding a name for `channel` until its pane arrives, which has
/// reconciled the edge that brought the pane, and that edge to deliver again.
fn window_holding_a_channel_name() -> (Cagents, ChannelId, ModelUpdate) {
    use crate::workspace_model::{
        delta::ViewRevision,
        views::{Change, Pane, Scope, View},
    };
    let (mut app, project, repository) = app_with_project();
    let channel = ChannelId::new();
    let mut row = local(channel, project, "", 1);
    row.shared = true;
    row.cards = vec![app.durable.cards[0].id];
    row.focused = row.cards.first().copied();
    Arc::make_mut(&mut app.durable).channels.push(row);
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    app.workspace_model = registry(vec![shared(channel, &repository, "", 1)]);
    app.workspace_model.views.push(View::empty(channel));
    let identity = app.workspace_model.identity;
    let scope = Scope::Channel(channel);
    // The window's chat, whose pane header and window title take the
    // channel's name.
    let session = crate::chat::Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: crate::model::TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Omp,
            recipe: crate::model::LaunchRecipe {
                unset_environment: Default::default(),
                program: "/bin/sh".into(),
                arguments: Vec::new(),
                working_directory: PathBuf::from("/tmp"),
                environment: Default::default(),
            },
            session_id: None,
            draft_model: None,
        },
        proof: None,
        terminal: None,
    };
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards[0].title = session.launch.provider.chat_title().into();
    durable.cards[0].kind = crate::model::CardKind::Chat(session);
    let card = &durable.cards[0];
    // Already shared, as the pane the edge carries.
    app.shared_views.attached.insert(card.id);
    let pane = Pane {
        id: card.id,
        title: card.title.clone(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        scope: None,
        kind: card.kind.clone(),
    };
    let insert = changed_edge(
        identity,
        10,
        ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: 0,
            operation: uuid::Uuid::nil(),
            change: Change::Insert {
                pane: pane.clone(),
                before: None,
            },
        },
        EditEffects {
            views: vec![ViewRevision {
                scope: scope.clone(),
                before: Some(0),
                after: Some(5),
            }],
            ..Default::default()
        },
        vec![View {
            scope,
            revision: 5,
            panes: vec![pane],
            retired: vec![],
        }],
    );
    assert!(reconciles(&mut app, insert.clone()));
    app.remember_channel_name(
        channel,
        "Parser cache".into(),
        crate::model::ChannelTitleSource::Provider,
    );
    (app, channel, insert)
}

fn channel_title(app: &Cagents, channel: ChannelId) -> Option<&str> {
    app.durable
        .channels
        .iter()
        .find(|held| held.id == channel)
        .map(|held| held.title.as_str())
}

/// The naming stage a complete reconciliation ends with also runs for an edge
/// that skips it: a name held until its channel's pane arrived is applied, and
/// the edge declares the chrome projection the new name needs. Declaring none
/// would leave the derived state stale, which the projection oracle reports.
#[test]
fn a_skipped_edge_applies_a_held_channel_name_and_projects_its_chrome() {
    let (mut app, channel, echo) = window_holding_a_channel_name();
    // Derived state current before the edge, so only what it leaves shows.
    drop(app.project_workspace(false));
    let before = complete_reconciliations();
    let (_, projection) = app.apply_workspace_model_updates(vec![echo]);
    assert_eq!(complete_reconciliations(), before, "the echo skips");
    assert_eq!(channel_title(&app, channel), Some("Parser cache"));
    assert_eq!(projection, Projection::Chrome);
    let skipped = super::projection_oracle::Snapshot::capture(&app);
    drop(app.project_workspace(false));
    let projected = super::projection_oracle::Snapshot::capture(&app);
    assert!(
        !skipped.differences(&projected).is_empty(),
        "the new name is stale until projected"
    );
    // Through an update, the projection oracle checks the chrome projection
    // the edge declares.
    let (mut app, channel, echo) = window_holding_a_channel_name();
    let before = complete_reconciliations();
    drop(app.update(Message::WorkspaceModelObserved(echo)));
    assert_eq!(complete_reconciliations(), before, "the echo skips");
    assert_eq!(channel_title(&app, channel), Some("Parser cache"));
}
