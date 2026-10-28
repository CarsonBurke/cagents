//! Reducer/task-correlation tests. Typed replies are supplied at the real App
//! message boundary; returned futures are deliberately never polled. These
//! tests do not claim a socket, SQL transaction, thread, or native UI ran.
use super::*;
use crate::{
    agent::Provider,
    agent_execution::{Origin, Spec, Surface, Target},
    app::{automation_approval::Action as ApprovalAction, workspace_model_sync::ModelUpdate},
    automations::{
        Definition, DirectoryIdentity, ProposalPage, ProposalReview, Schedule, SchedulePattern,
        SessionMode,
    },
    workspace_model::WorkspaceModel,
};

type Flight = (u64, u64, Read);

fn proposal(number: u128) -> Proposal {
    let definition = Definition {
        name: format!("Request {number}"),
        prompt: "Review only the approved project".into(),
        agent: Spec {
            provider: Provider::Codex,
            account: Some("codex/system".parse().unwrap()),
            target: Target {
                repository: "/fixture".into(),
                directory: "/fixture".into(),
                placement: None,
            },
            surface: Surface::Chat,
            model: Some("offline".into()),
            reasoning_effort: Some("low".into()),
            permission_mode: None,
        },
        schedule: Schedule {
            timezone: "America/Vancouver".into(),
            pattern: SchedulePattern::Weekdays {
                hour: 9,
                minute: 30,
            },
        },
        session_mode: SessionMode::Fresh,
        precheck: None,
    };
    Proposal {
        id: uuid::Uuid::from_u128(number),
        revision: 1,
        owner: Origin::User,
        requested: definition.clone(),
        definition,
        target_identity: DirectoryIdentity {
            device: 1,
            inode: 2,
        },
        channel_title: None,
        created_ms: 1,
        state: ProposalState::Pending,
    }
}

fn global(app: &mut Cagents, action: Action) -> Task<Message> {
    app.update(Message::AutomationApproval(ApprovalAction::Global(action)))
}

fn flight(app: &Cagents) -> Flight {
    app.automation_approval
        .global
        .flight
        .clone()
        .expect("one owned read")
}

fn complete(app: &mut Cagents, flight: Flight, result: Result<Response, String>) -> Task<Message> {
    let (nonce, epoch, read) = flight;
    global(
        app,
        Action::Loaded {
            nonce,
            epoch,
            read,
            result: Arc::new(result),
        },
    )
}

fn model(app: &mut Cagents, identity: uuid::Uuid, sequence: u64, connected: bool) {
    let model = WorkspaceModel {
        identity,
        sequence,
        ..WorkspaceModel::default()
    };
    let update = if connected {
        ModelUpdate::Connected(model)
    } else {
        ModelUpdate::Read(model)
    };
    drop(app.update(Message::WorkspaceModelObserved(update)));
}

fn app() -> Cagents {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.refresh_ui.focused = true;
    app.core.set_keyboard_nav(true);
    model(&mut app, uuid::Uuid::from_u128(10_000), 1, true);
    assert!(matches!(flight(&app).2, Read::Delivery));
    app
}

fn delivery(identity: uuid::Uuid, sequence: u64, total: u64, rows: &[Proposal]) -> Response {
    Response::ProposalDelivery(ProposalDelivery {
        identity,
        automation_proposal_sequence: sequence,
        pending_automation_proposal_count: total,
        pending_automation_proposals: rows.iter().map(ProposalSummary::from).collect(),
    })
}

fn install(app: &mut Cagents, rows: &[Proposal]) -> Key {
    let held = flight(app);
    let response = delivery(app.workspace_model.identity, 1, rows.len() as u64, rows);
    drop(complete(app, held, Ok(response)));
    assert!(app.automation_approval.global.flight.is_none());
    app.automation_approval.global.key.unwrap()
}

fn review(identity: uuid::Uuid, proposal: &Proposal) -> Response {
    Response::ProposalReview(ProposalReview {
        identity,
        proposal: Box::new(proposal.clone()),
    })
}

fn open(app: &mut Cagents, proposal: &Proposal) {
    let key = app.automation_approval.global.key.unwrap();
    let task = global(
        app,
        Action::Open {
            key,
            id: proposal.id,
            revision: proposal.revision,
        },
    );
    assert!(task.units() > 0, "the full consent read must be scheduled");
    drop(task);
    let held = flight(app);
    drop(complete(app, held, Ok(review(key.identity, proposal))));
    assert_eq!(app.automation_approval.proposal.as_ref(), Some(proposal));
    assert_eq!(
        app.automation_approval.global.reviewed_identity,
        Some(key.identity)
    );
    assert!(app.automation_approval_open());
}

#[test]
fn decision_watch_loss_and_transport_error_keep_truthful_close_and_old_result_fences() {
    fn dialog(app: &Cagents) -> (Vec<String>, Vec<Message>) {
        use cosmic::iced::{
            Event, Point, Rectangle, Size,
            advanced::{Layout, Shell, clipboard, layout, widget::Tree},
            mouse,
        };
        use iced_accessibility::accesskit::Role;

        let renderer = crate::app::tests::headless_renderer();
        let mut element = app
            .automation_approval_dialog()
            .expect("the exact review remains open");
        let mut tree = Tree::new(element.as_widget());
        let size = Size::new(1000.0, 900.0);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let labels = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect();
        let buttons: Vec<_> = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter(|node| {
                node.node().role() == Role::Button && node.node().label() == Some("Not now")
            })
            .collect();
        assert_eq!(
            buttons.len(),
            1,
            "select the actual button, not its paragraph"
        );
        assert!(
            !buttons[0].node().is_disabled(),
            "local dismissal remains available"
        );
        let rect = buttons[0].node().bounds().expect("mounted dismiss control");
        let point = Point::new(
            ((rect.x0 + rect.x1) / 2.0) as f32,
            ((rect.y0 + rect.y1) / 2.0) as f32,
        );
        let mut messages = Vec::new();
        for event in [
            mouse::Event::ButtonPressed(mouse::Button::Left),
            mouse::Event::ButtonReleased(mouse::Button::Left),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(event),
                Layout::new(&node),
                mouse::Cursor::Available(point),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::with_size(size),
            );
        }
        (labels, messages)
    }

    for watch_lost in [true, false] {
        let mut app = app();
        let a = proposal(1);
        let b = proposal(2);
        let key = install(&mut app, &[a.clone(), b.clone()]);
        open(&mut app, &a);
        let old_generation = app.automation_approval.generation;
        let task = app.update(Message::AutomationApproval(ApprovalAction::Decide {
            id: a.id,
            revision: a.revision,
            generation: old_generation,
            approved: true,
        }));
        assert!(task.units() > 0);
        drop(task); // Deliberately unpolled: scheduling is not proof of a sent RPC.
        assert!(app.automation_approval.deciding);
        if watch_lost {
            drop(app.update(Message::WorkspaceModelObserved(
                ModelUpdate::ProposalWatchLost,
            )));
            assert!(app.automation_approval.global.review_stale);
            assert!(!app.automation_approval.global.can_decide());
            assert_ne!(app.automation_approval.generation, old_generation);
        } else {
            drop(
                app.update(Message::AutomationApproval(ApprovalAction::Finished {
                    generation: old_generation,
                    result: Arc::new(Err("connection closed before a receipt arrived".into())),
                })),
            );
        }
        assert!(!app.automation_approval.deciding);
        assert_eq!(app.automation_approval.proposal.as_ref(), Some(&a));
        let (labels, mut messages) = dialog(&app);
        let visible = labels.join("\n");
        if watch_lost {
            assert!(
                visible.contains("Closing this review does not cancel a decision already sent.")
            );
        } else {
            assert!(visible.contains("Could not confirm the decision outcome."));
            assert!(visible.contains("A decision already sent may still complete."));
            assert!(visible.contains("connection closed before a receipt arrived"));
        }
        assert!(!visible.contains("keeps it pending"));
        assert!(
            matches!(messages.as_slice(), [Message::AutomationApproval(ApprovalAction::Dismiss { id, revision, generation })]
            if *id == a.id && *revision == a.revision && *generation == app.automation_approval.generation)
        );
        drop(app.update(messages.pop().unwrap()));
        assert!(!app.automation_approval_open());
        assert!(!app.automation_approval.deciding);

        if watch_lost {
            model(&mut app, key.identity, 2, true);
            let held = flight(&app);
            drop(complete(
                &mut app,
                held,
                Ok(delivery(
                    key.identity,
                    key.sequence,
                    2,
                    &[a.clone(), b.clone()],
                )),
            ));
        }
        open(&mut app, &b);
        let replacement_generation = app.automation_approval.generation;
        let mut receipt = a.clone();
        receipt.state = ProposalState::Declined;
        let late = app.update(Message::AutomationApproval(ApprovalAction::Finished {
            generation: old_generation,
            result: Arc::new(Ok(receipt)),
        }));
        assert_eq!(
            late.units(),
            0,
            "old results cannot replace or close another review"
        );
        drop(late);
        assert_eq!(app.automation_approval.proposal.as_ref(), Some(&b));
        assert_eq!(app.automation_approval.generation, replacement_generation);
        assert!(app.automation_approval.error.is_none());
        assert!(!app.automation_approval.deciding);
    }
}

#[test]
fn dirty_model_edge_and_reconnect_keep_the_owned_slot_until_matching_completion() {
    let mut app = app();
    let original = flight(&app);
    let identity = app.workspace_model.identity;
    let old_row = proposal(1);
    model(&mut app, identity, 2, false);
    model(&mut app, identity, 3, false);
    assert_eq!(
        flight(&app),
        original,
        "a wake cannot release the existing read"
    );
    assert!(app.automation_approval.global.dirty);
    assert_eq!(app.automation_approval.global.nonce, original.0);
    drop(complete(
        &mut app,
        original.clone(),
        Ok(delivery(identity, 1, 0, &[])),
    ));
    let trailing = flight(&app);
    assert_eq!(trailing.0, original.0 + 1);
    assert!(matches!(trailing.2, Read::Delivery));
    assert!(!app.automation_approval.global.dirty);
    // A duplicated old callback cannot retire the trailing read.
    let duplicate = complete(
        &mut app,
        original,
        Ok(delivery(identity, 1, 1, &[old_row.clone()])),
    );
    assert_eq!(duplicate.units(), 0);
    drop(duplicate);
    assert_eq!(flight(&app), trailing);

    drop(app.update(Message::WorkspaceModelObserved(
        ModelUpdate::ProposalWatchLost,
    )));
    assert_eq!(flight(&app), trailing);
    assert!(!app.automation_approval.global.connected);
    model(&mut app, identity, 4, true);
    assert_eq!(
        flight(&app),
        trailing,
        "reconnect must not create an overlapping logical read"
    );
    assert!(app.automation_approval.global.epoch > trailing.1);
    assert!(app.automation_approval.global.key.is_none());
    drop(complete(
        &mut app,
        trailing.clone(),
        Ok(delivery(identity, 2, 1, &[old_row])),
    ));
    let current = flight(&app);
    assert_eq!(current.0, trailing.0 + 1);
    assert_eq!(current.1, app.automation_approval.global.epoch);
    assert!(
        app.automation_approval.global.key.is_none(),
        "old-epoch data must not be adopted"
    );
    assert!(app.automation_approval.global.pending.is_empty());
    let current_row = proposal(2);
    drop(complete(
        &mut app,
        current,
        Ok(delivery(identity, 3, 1, &[current_row.clone()])),
    ));
    assert!(app.automation_approval.global.flight.is_none());
    assert_eq!(
        app.automation_approval.global.pending,
        vec![ProposalSummary::from(&current_row)]
    );
    assert_eq!(app.automation_approval.global.key.unwrap().sequence, 3);
}

#[test]
fn latest_explicit_open_wins_before_a_slow_full_review_can_open_a_modal() {
    let mut app = app();
    let a = proposal(1);
    let b = proposal(2);
    let key = install(&mut app, &[a.clone(), b.clone()]);
    drop(global(
        &mut app,
        Action::Open {
            key,
            id: a.id,
            revision: a.revision,
        },
    ));
    let a_read = flight(&app);
    drop(global(
        &mut app,
        Action::Open {
            key,
            id: b.id,
            revision: b.revision,
        },
    ));
    assert_eq!(flight(&app), a_read);
    assert!(
        matches!(app.automation_approval.global.wanted, Some(Read::Open { id, .. }) if id == b.id)
    );
    assert!(!app.modal_is_open());
    drop(complete(
        &mut app,
        a_read.clone(),
        Ok(review(key.identity, &a)),
    ));
    assert!(
        !app.modal_is_open(),
        "slow A must not appear even for one intermediate frame"
    );
    assert!(app.automation_approval.available.is_none());
    let b_read = flight(&app);
    assert_eq!(b_read.0, a_read.0 + 1);
    assert!(matches!(b_read.2, Read::Open { id, .. } if id == b.id));
    drop(complete(
        &mut app,
        b_read.clone(),
        Ok(review(key.identity, &b)),
    ));
    assert_eq!(app.automation_approval.proposal, Some(b.clone()));
    assert_eq!(
        app.automation_approval.global.reviewed_identity,
        Some(key.identity)
    );
    assert_eq!(
        app.automation_approval.focus, 0,
        "Not now remains the default control"
    );
    let generation = app.automation_approval.generation;
    drop(complete(&mut app, a_read, Ok(review(key.identity, &a))));
    drop(complete(&mut app, b_read, Ok(review(key.identity, &b))));
    assert_eq!(app.automation_approval.generation, generation);
    assert_eq!(app.automation_approval.proposal, Some(b));
}

#[test]
fn foreign_page_identity_clock_or_cursor_preserves_rows_and_schedules_one_refresh() {
    for mismatch in ["identity", "clock", "after"] {
        let mut app = app();
        let a = proposal(1);
        let key = install(&mut app, &[a.clone()]);
        let after = Some(a.id);
        drop(global(
            &mut app,
            Action::Page {
                key,
                after,
                start: None,
            },
        ));
        let held = flight(&app);
        assert!(matches!(held.2, Read::Page { after: observed, .. } if observed == after));
        let b = proposal(2);
        let mut page = ProposalPage {
            identity: key.identity,
            automation_proposal_sequence: key.sequence,
            after,
            proposals: vec![ProposalSummary::from(&b)],
        };
        match mismatch {
            "identity" => page.identity = uuid::Uuid::from_u128(20_000),
            "clock" => page.automation_proposal_sequence += 1,
            "after" => page.after = None,
            _ => unreachable!(),
        }
        drop(complete(
            &mut app,
            held.clone(),
            Ok(Response::ProposalPage(page.clone())),
        ));
        assert_eq!(
            app.automation_approval.global.pending,
            vec![ProposalSummary::from(&a)],
            "{mismatch}"
        );
        assert_eq!(app.automation_approval.global.after, None, "{mismatch}");
        assert!(!app.modal_is_open());
        assert!(app.automation_approval.global.error.is_some());
        let refresh = flight(&app);
        assert_eq!(refresh.0, held.0 + 1);
        assert!(matches!(refresh.2, Read::Delivery));
        assert!(!app.automation_approval.global.dirty);
        // Repeating that same invalid completion cannot schedule another task.
        let stale = complete(&mut app, held, Ok(Response::ProposalPage(page)));
        assert_eq!(stale.units(), 0);
        drop(stale);
        assert_eq!(flight(&app), refresh);
        drop(complete(
            &mut app,
            refresh,
            Ok(delivery(key.identity, key.sequence, 1, &[a])),
        ));
        assert!(app.automation_approval.global.flight.is_none());
        assert!(app.automation_approval.global.error.is_none());
    }
}

#[test]
fn reconnect_and_registry_replacement_revoke_old_decisions_and_old_acknowledgements() {
    let mut app = app();
    let a = proposal(1);
    let b = proposal(2);
    let key = install(&mut app, &[a.clone(), b.clone()]);
    open(&mut app, &a);
    let old_generation = app.automation_approval.generation;
    let child = crate::agent_execution::Authority::session("unit-only-reviewed-scope".into());
    let (request, identity, retained) =
        app.automation_approval
            .global
            .decision_request(&a, false, child.clone());
    assert_eq!(retained, child);
    assert!(!request.allowed_from_hosted_conversation());
    assert_eq!(identity, Some(key.identity));
    assert!(
        matches!(request, Request::ResolveReviewedProposal { identity, proposal, approved: false }
        if identity == key.identity && proposal.as_ref() == &a)
    );
    let deciding = app.update(Message::AutomationApproval(ApprovalAction::Decide {
        id: a.id,
        revision: a.revision,
        generation: old_generation,
        approved: false,
    }));
    assert!(deciding.units() > 0);
    drop(deciding); // The decision future is NOT polled or sent.
    assert!(app.automation_approval.deciding);
    let mut declined = a.clone();
    declined.state = ProposalState::Declined;
    model(&mut app, key.identity, 2, true);
    assert!(app.automation_approval.global.review_stale);
    assert!(!app.automation_approval.global.can_decide());
    assert!(!app.automation_approval.deciding);
    assert_eq!(app.automation_approval.proposal, Some(a.clone()));
    let obsolete = app.update(Message::AutomationApproval(ApprovalAction::Decide {
        id: a.id,
        revision: a.revision,
        generation: old_generation,
        approved: true,
    }));
    assert_eq!(
        obsolete.units(),
        0,
        "no old scoped or legacy decision may be scheduled"
    );
    drop(obsolete);
    drop(
        app.update(Message::AutomationApproval(ApprovalAction::Finished {
            generation: old_generation,
            result: Arc::new(Ok(declined.clone())),
        })),
    );
    assert_eq!(app.automation_approval.proposal, Some(a.clone()));
    // An unchanged proposal clock after reconnect still needs the exact review.
    let reconnect = flight(&app);
    drop(complete(
        &mut app,
        reconnect,
        Ok(delivery(
            key.identity,
            key.sequence,
            2,
            &[a.clone(), b.clone()],
        )),
    ));
    let refresh = flight(&app);
    assert!(matches!(refresh.2, Read::Review { id, .. } if id == a.id));
    assert!(app.automation_approval.global.review_stale);
    drop(complete(&mut app, refresh, Ok(review(key.identity, &a))));
    assert!(app.automation_approval.global.can_decide());

    // A replacement registry cannot authorize the old full consent by ID alone.
    let replacement = uuid::Uuid::from_u128(30_000);
    model(&mut app, replacement, 1, true);
    let replacement_read = flight(&app);
    drop(complete(
        &mut app,
        replacement_read,
        Ok(delivery(replacement, 1, 1, &[b.clone()])),
    ));
    assert!(
        app.automation_approval.global.flight.is_none(),
        "foreign full-review RPC must not be queued"
    );
    assert_eq!(app.automation_approval.proposal, Some(a.clone()));
    assert!(!app.automation_approval.global.can_decide());
    let current_generation = app.automation_approval.generation;
    let refused = app.update(Message::AutomationApproval(ApprovalAction::Decide {
        id: a.id,
        revision: a.revision,
        generation: current_generation,
        approved: true,
    }));
    assert_eq!(
        refused.units(),
        0,
        "a current generation alone must not demote old consent to legacy Resolve"
    );
    drop(refused);
    drop(
        app.update(Message::AutomationApproval(ApprovalAction::Dismiss {
            id: a.id,
            revision: a.revision,
            generation: current_generation,
        })),
    );
    assert!(!app.automation_approval_open());
    open(&mut app, &b);
    let b_generation = app.automation_approval.generation;
    // Even while B has an outstanding decision, A's ACK must not dismiss it.
    let b_decision = app.update(Message::AutomationApproval(ApprovalAction::Decide {
        id: b.id,
        revision: b.revision,
        generation: b_generation,
        approved: false,
    }));
    assert!(b_decision.units() > 0);
    drop(b_decision);
    drop(
        app.update(Message::AutomationApproval(ApprovalAction::Finished {
            generation: old_generation,
            result: Arc::new(Ok(declined.clone())),
        })),
    );
    assert_eq!(app.automation_approval.proposal, Some(b.clone()));
    assert!(app.automation_approval.deciding);
    assert_eq!(app.automation_approval.generation, b_generation);
    let mut b_receipt = b.clone();
    b_receipt.state = ProposalState::Declined;
    drop(
        app.update(Message::AutomationApproval(ApprovalAction::Finished {
            generation: b_generation,
            result: Arc::new(Ok(b_receipt)),
        })),
    );
    assert!(
        !app.automation_approval_open(),
        "the matching immutable receipt still works"
    );
    drop(
        app.update(Message::AutomationApproval(ApprovalAction::Finished {
            generation: old_generation,
            result: Arc::new(Ok(declined)),
        })),
    );
    assert!(!app.automation_approval_open());
}

#[test]
fn same_clock_recovery_clears_transport_error_without_resetting_the_later_page() {
    let mut app = app();
    let first: Vec<_> = (1..=64).map(proposal).collect();
    let later: Vec<_> = (65..=128).map(proposal).collect();
    let identity = app.workspace_model.identity;
    let first_read = flight(&app);
    drop(complete(
        &mut app,
        first_read,
        Ok(delivery(identity, 9, 128, &first)),
    ));
    let key = app.automation_approval.global.key.unwrap();
    let after = first.last().map(|row| row.id);
    drop(global(&mut app, Action::Toggle));
    drop(global(
        &mut app,
        Action::Page {
            key,
            after,
            start: Some(PROPOSAL_PAGE_SIZE as u64),
        },
    ));
    let page_read = flight(&app);
    drop(complete(
        &mut app,
        page_read,
        Ok(Response::ProposalPage(ProposalPage {
            identity,
            automation_proposal_sequence: 9,
            after,
            proposals: later.iter().map(ProposalSummary::from).collect(),
        })),
    ));
    let page = app.automation_approval.global.pending.clone();
    let generation = app.automation_approval.generation;
    let focused = app.workspace.focused();
    drop(global(&mut app, Action::Refresh));
    let failing = flight(&app);
    drop(complete(
        &mut app,
        failing,
        Err("test transport interruption".into()),
    ));
    assert!(app.automation_approval.global.error.is_some());
    assert!(app.automation_approval.global.flight.is_none());
    assert_eq!(app.automation_approval.global.pending, page);
    drop(global(&mut app, Action::Refresh));
    let recovery = flight(&app);
    drop(complete(
        &mut app,
        recovery,
        Ok(delivery(identity, 9, 128, &first)),
    ));
    assert!(app.automation_approval.global.error.is_none());
    assert_eq!(app.automation_approval.global.key, Some(key));
    assert_eq!(app.automation_approval.global.after, after);
    assert_eq!(app.automation_approval.global.pending, page);
    assert!(app.automation_approval.global.expanded);
    assert!(app.automation_approval.global.flight.is_none());
    assert_eq!(app.automation_approval.generation, generation);
    assert_eq!(app.workspace.focused(), focused);
    assert!(!app.automation_approval_open());
    assert!(app.automation_approval.global.reviewed_identity.is_none());
    assert!(app.automation_approval.global.authorized.is_none());
}

#[test]
fn scoped_discovery_refusal_cannot_turn_the_legacy_request_into_user_authority() {
    use crate::agent_execution::Authority;
    let mut app = app();
    let held = flight(&app);
    drop(complete(
        &mut app,
        held,
        Err("synthetic scoped discovery refusal".into()),
    ));
    assert!(!app.automation_approval.global.has_seen_delivery());
    assert!(app.automation_approval.global.error.is_some());

    // Keep the legitimate pre-capability, per-project discovery route. These
    // are the existing App fixture/project types, not an alternate transport.
    app.sidebar_model.projects.push(
        SidebarProject::new(
            ProjectId::new(),
            crate::repository::RepositoryId::new(),
            "Fixture".into(),
            "/fixture".into(),
            Vec::new(),
            Vec::new(),
        )
        .with_kind(ProjectKind::Folder),
    );
    app.active_workspace = Some("/fixture".into());
    let tick = app.update(Message::AutomationApproval(ApprovalAction::Tick));
    assert!(tick.units() > 0);
    drop(tick); // No legacy request is actually sent.
    let generation = app.automation_approval.generation;
    let proposal = proposal(1);
    drop(
        app.update(Message::AutomationApproval(ApprovalAction::Read {
            generation,
            result: Arc::new(Ok(vec![proposal.clone()])),
        })),
    );
    assert_eq!(app.automation_approval.available, Some(proposal.clone()));
    drop(
        app.update(Message::AutomationApproval(ApprovalAction::Open {
            id: proposal.id,
            revision: proposal.revision,
        })),
    );
    assert_eq!(app.automation_approval.proposal, Some(proposal.clone()));
    assert!(app.automation_approval.global.reviewed_identity.is_none());

    // Exercise the actual decision request builder with explicit synthetic
    // scopes; no process environment or credentials are read/changed here.
    // The real Decide caller supplies Authority::from_environment once.
    let child = Authority::session("unit-only-proposal-scope".into());
    let (request, identity, retained) =
        app.automation_approval
            .global
            .decision_request(&proposal, false, child.clone());
    assert_eq!(retained, child);
    assert_eq!(identity, None);
    assert!(
        matches!(request, Request::ResolveProposal { id, revision, approved: false }
        if id == proposal.id && revision == proposal.revision)
    );
    assert!(!request.allowed_from_hosted_conversation());
    let user = Authority::default();
    let (user_request, identity, retained) =
        app.automation_approval
            .global
            .decision_request(&proposal, false, user.clone());
    assert_eq!(retained, user);
    assert_eq!(identity, None);
    assert_eq!(
        user_request, request,
        "normal User intent keeps its original request shape"
    );
}

#[test]
fn an_exact_already_resolved_notice_reports_its_terminal_state_without_a_modal_or_decision() {
    for approved in [false, true] {
        let mut app = app();
        let proposal = proposal(1);
        let key = install(&mut app, &[proposal.clone()]);
        let task = app.open_proposal_notice(key.identity, proposal.id, proposal.revision);
        assert!(task.units() > 0);
        drop(task);
        let held = flight(&app);
        assert!(matches!(held.2,Read::Open{id,..} if id==proposal.id));
        let mut terminal = proposal.clone();
        terminal.state = if approved {
            ProposalState::Approved {
                automation: uuid::Uuid::from_u128(900),
                execution_revision: 1,
            }
        } else {
            ProposalState::Declined
        };
        drop(complete(
            &mut app,
            held,
            Ok(review(key.identity, &terminal)),
        ));
        assert!(!app.modal_is_open());
        assert!(!app.automation_approval.deciding);
        assert!(app.automation_approval.proposal.is_none());
        assert!(app.automation_approval.global.authorized.is_none());
        assert!(app.automation_approval.global.pending.is_empty());
        assert!(app.automation_approval.global.latest_open.is_none());
        assert!(matches!(flight(&app).2, Read::Delivery));
        assert!(app.status.is_some());
    }
}

#[test]
fn notice_activation_refuses_foreign_registry_and_never_uses_cached_legacy_consent() {
    let mut app = app();
    let proposal = proposal(1);
    let key = install(&mut app, &[proposal.clone()]);
    app.automation_approval.available = Some(proposal.clone());
    let wrong =
        app.open_proposal_notice(uuid::Uuid::from_u128(999), proposal.id, proposal.revision);
    assert_eq!(wrong.units(), 0);
    drop(wrong);
    assert!(app.automation_approval.global.flight.is_none());
    let task = app.open_proposal_notice(key.identity, proposal.id, proposal.revision);
    assert!(task.units() > 0);
    drop(task);
    assert!(!app.modal_is_open());
    let held = flight(&app);
    assert!(matches!(held.2,Read::Open{id,..} if id==proposal.id));
    drop(complete(
        &mut app,
        held,
        Ok(review(key.identity, &proposal)),
    ));
    assert_eq!(app.automation_approval.proposal, Some(proposal));
    assert_eq!(
        app.automation_approval.global.reviewed_identity,
        Some(key.identity)
    );
}

#[test]
fn an_initial_delivery_error_keeps_a_rendered_retry_without_fabricating_a_count_or_polling() {
    // Use the same native widget operation/event seam as the existing approval
    // dialog tests. This clicks the production banner, not a test-only action.
    fn click_refresh(app: &Cagents) -> Vec<Message> {
        use cosmic::iced::{
            Event, Point, Rectangle, Size,
            advanced::{
                Layout, Shell, clipboard, layout,
                widget::{Operation, Tree, operation::Focusable},
            },
            mouse,
        };
        struct Bounds(Vec<Rectangle>);
        impl Operation for Bounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                _: Option<&widget::Id>,
                bounds: Rectangle,
                _: &mut dyn Focusable,
            ) {
                self.0.push(bounds);
            }
        }
        let renderer = crate::app::tests::headless_renderer();
        let mut element = app
            .global_proposal_banner()
            .expect("the initial failure must remain visible without a count");
        let mut tree = Tree::new(element.as_widget());
        let size = Size::new(1000.0, 900.0);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        let mut bounds = Bounds(Vec::new());
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut bounds);
        assert_eq!(
            bounds.0.len(),
            1,
            "unknown count offers only explicit Refresh"
        );
        let point: Point = bounds.0[0].center();
        let mut messages = Vec::new();
        for event in [
            mouse::Event::ButtonPressed(mouse::Button::Left),
            mouse::Event::ButtonReleased(mouse::Button::Left),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(event),
                Layout::new(&node),
                mouse::Cursor::Available(point),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::with_size(size),
            );
        }
        messages
    }

    let mut app = app(); // Actual Connected handler owns the first Delivery.
    let identity = app.workspace_model.identity;
    let sequence = app.workspace_model.sequence;
    let original = flight(&app);
    assert!(app.automation_approval.global.total.is_none());
    assert!(app.global_proposal_banner().is_none());
    drop(complete(
        &mut app,
        original.clone(),
        Err("unavailable".into()),
    ));
    assert!(app.automation_approval.global.total.is_none());
    assert!(app.automation_approval.global.error.is_some());
    assert!(app.automation_approval.global.flight.is_none());
    assert!(!app.automation_approval.global.dirty);
    model(&mut app, identity, sequence, false);
    assert!(app.automation_approval.global.flight.is_none());
    assert_eq!(app.automation_approval.global.nonce, original.0);

    let mut messages = click_refresh(&app);
    assert!(matches!(
        messages.as_slice(),
        [Message::AutomationApproval(ApprovalAction::Global(
            Action::Refresh
        ))]
    ));
    let task = app.update(messages.pop().unwrap());
    assert!(
        task.units() > 0,
        "the actual Refresh message schedules one read"
    );
    drop(task); // No connection/SQL future is polled by this source test.
    let retry = flight(&app);
    assert_eq!(retry.0, original.0 + 1);
    assert!(matches!(retry.2, Read::Delivery));
    assert!(click_refresh(&app).is_empty(), "busy Refresh is disabled");
    // Previously queued clicks cannot create overlapping reads. They retain
    // at most one trailing intent, the same rule as a model edge during IO.
    for _ in 0..2 {
        let task = global(&mut app, Action::Refresh);
        assert_eq!(task.units(), 0);
        drop(task);
        assert_eq!(flight(&app), retry);
    }
    assert!(app.automation_approval.global.dirty);
    let rows = [proposal(801), proposal(802)];
    drop(complete(
        &mut app,
        retry,
        Ok(delivery(identity, 1, 2, &rows)),
    ));
    assert_eq!(app.automation_approval.global.total, Some(2));
    assert_eq!(
        app.automation_approval.global.pending,
        rows.iter().map(ProposalSummary::from).collect::<Vec<_>>()
    );
    assert!(app.automation_approval.global.error.is_none());
    let trailing = flight(&app);
    assert_eq!(trailing.0, original.0 + 2);
    assert!(!app.automation_approval.global.dirty);
    drop(complete(
        &mut app,
        trailing,
        Ok(delivery(identity, 1, 2, &rows)),
    ));
    assert!(app.automation_approval.global.flight.is_none());
    assert!(app.automation_approval.global.error.is_none());
    assert_eq!(app.automation_approval.global.total, Some(2));
    assert!(!app.automation_approval_open());
    assert!(app.automation_approval.global.reviewed_identity.is_none());
}

#[test]
fn proposal_discovery_has_no_time_based_subscription_even_when_focused_with_a_project() {
    use crate::{model::ProjectKind, workspace_model::ModelProject};
    use cosmic::iced::advanced::subscription::into_recipes;
    let mut app = app();
    for projects in [
        vec![],
        vec![ModelProject {
            repository: "/fixture".into(),
            name: "Visible project".into(),
            kind: ProjectKind::Folder,
            ordinal: 1,
        }],
    ] {
        let snapshot = WorkspaceModel {
            identity: app.workspace_model.identity,
            sequence: app.workspace_model.sequence + 1,
            projects,
            ..WorkspaceModel::default()
        };
        drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(snapshot))));
        for focused in [false, true] {
            app.refresh_ui.focused = focused;
            assert!(into_recipes(app.automation_approval_subscription()).is_empty());
        }
    }
    assert!(
        !app.sidebar_model.projects.is_empty(),
        "the original focused/project timer condition is actually present"
    );
}

#[test]
fn foreign_project_watch_reads_reconnect_and_decision_receipts_refresh_the_global_badge_without_ticks()
 {
    use crate::{model::ProjectKind, workspace_model::ModelEdit, workspace_model::ModelProject};
    let mut app = app();
    let identity = app.workspace_model.identity;
    let initial = flight(&app);
    drop(complete(
        &mut app,
        initial,
        Ok(delivery(identity, 1, 0, &[])),
    ));
    let snapshot = |sequence| WorkspaceModel {
        identity,
        sequence,
        projects: vec![ModelProject {
            repository: "/fixture".into(),
            name: "Visible project".into(),
            kind: ProjectKind::Folder,
            ordinal: 1,
        }],
        ..WorkspaceModel::default()
    };
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            snapshot(2),
        ))),
    );
    let current = flight(&app);
    drop(complete(
        &mut app,
        current,
        Ok(delivery(identity, 1, 0, &[])),
    ));
    app.refresh_ui.focused = false;
    // This is the observer's actual pair for a ranked AgentsChanged edge:
    // fold its metadata, then install the complete authoritative read. No Tick.
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Edited {
            sequence: 3,
            edit: ModelEdit::AgentsChanged,
        })),
    );
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            snapshot(3),
        ))),
    );
    let pushed = flight(&app);
    assert!(matches!(pushed.2, Read::Delivery));
    let mut foreign = proposal(901);
    foreign.definition.agent.target.repository = "/other-project".into();
    foreign.definition.agent.target.directory = "/other-project".into();
    foreign.requested = foreign.definition.clone();
    assert!(
        app.sidebar_model
            .projects
            .iter()
            .all(|project| project.repository != foreign.definition.agent.target.repository)
    );
    drop(complete(
        &mut app,
        pushed,
        Ok(delivery(identity, 2, 1, &[foreign.clone()])),
    ));
    assert_eq!(app.automation_approval.global.total, Some(1));
    assert_eq!(
        app.automation_approval.global.pending,
        vec![ProposalSummary::from(&foreign)]
    );
    assert!(app.global_proposal_banner().is_some());
    assert!(!app.automation_approval_open(), "a push never steals focus");

    drop(app.update(Message::WorkspaceModelObserved(
        ModelUpdate::ProposalWatchLost,
    )));
    assert!(!app.automation_approval.global.connected);
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
            snapshot(3),
        ))),
    );
    let reconnect = flight(&app);
    assert!(matches!(reconnect.2, Read::Delivery));
    drop(complete(
        &mut app,
        reconnect,
        Ok(delivery(identity, 2, 1, &[foreign.clone()])),
    ));
    app.refresh_ui.focused = true;
    open(&mut app, &foreign); // Actual exact scoped full-review handler.
    let generation = app.automation_approval.generation;
    let decision = app.update(Message::AutomationApproval(ApprovalAction::Decide {
        id: foreign.id,
        revision: foreign.revision,
        generation,
        approved: false,
    }));
    assert!(decision.units() > 0);
    drop(decision); // The test supplies the correlated typed receipt, not IO.
    let mut declined = foreign;
    declined.state = ProposalState::Declined;
    drop(
        app.update(Message::AutomationApproval(ApprovalAction::Finished {
            generation,
            result: Arc::new(Ok(declined)),
        })),
    );
    assert!(!app.automation_approval_open());
    let after_decision = flight(&app);
    assert!(matches!(after_decision.2, Read::Delivery));
    drop(complete(
        &mut app,
        after_decision,
        Ok(delivery(identity, 3, 0, &[])),
    ));
    assert_eq!(app.automation_approval.global.total, Some(0));
    assert!(app.automation_approval.global.pending.is_empty());
    assert!(app.automation_approval.global.flight.is_none());
}

#[test]
fn notice_open_queued_during_delivery_keeps_the_latest_exact_subject_after_clock_advance() {
    for latest_b in [false, true] {
        let mut app = app();
        let a = proposal(200);
        let b = proposal(201);
        let page: Vec<_> = (1..=64).map(proposal).collect();
        let identity = app.workspace_model.identity;
        let initial = flight(&app);
        drop(complete(
            &mut app,
            initial,
            Ok(delivery(identity, 1, 66, &page)),
        ));
        let key = app.automation_approval.global.key.unwrap();
        assert!(app.automation_approval.global.flight.is_none());
        drop(
            app.update(Message::NotificationUi(notification_ui::Action::HostBound(
                crate::model::HostSessionProof {
                    host_session_id: crate::model::HostSessionId::new("queued-notice"),
                    boot_id: crate::model::BootId::new("queued-notice"),
                    host_incarnation: crate::model::HostIncarnation::new("queued-notice"),
                },
            ))),
        );
        model(&mut app, identity, 2, false);
        let held_delivery = flight(&app);
        assert!(matches!(held_delivery.2, Read::Delivery));
        let generation = app.automation_approval.generation;
        let focused = app.workspace.focused();
        let destination = app.active_workspace_destination();
        for selected in std::iter::once(&a).chain(latest_b.then_some(&b)) {
            drop(
                app.update(Message::NotificationUi(notification_ui::Action::Activated(
                    crate::notifications::Target::AutomationProposal {
                        registry: identity,
                        id: selected.id,
                        revision: selected.revision,
                    },
                ))),
            );
            assert_eq!(flight(&app), held_delivery);
            assert!(matches!(
                &app.automation_approval.global.wanted,
                Some(Read::Open { key: queued, id, revision, .. })
                    if *queued == key && *id == selected.id && *revision == selected.revision
            ));
        }
        let selected = if latest_b { &b } else { &a };
        assert!(!app.modal_is_open());
        // The activated subject is beyond the first 64 rows. An unrelated
        // creation advances the delivery clock without changing its consent.
        drop(complete(
            &mut app,
            held_delivery.clone(),
            Ok(delivery(identity, 2, 67, &page)),
        ));
        let current = flight(&app);
        let Read::Open {
            key: current_key,
            id,
            revision,
            generation: current_generation,
            focused: current_focus,
            destination: current_destination,
        } = &current.2
        else {
            panic!("the explicit notice still owns the next full review");
        };
        let rekeyed = *current_key;
        assert_eq!(current_key.identity, identity);
        assert_eq!(current_key.epoch, key.epoch);
        assert_eq!((*id, *revision), (selected.id, selected.revision));
        assert_eq!(*current_generation, generation);
        assert_eq!(*current_focus, focused);
        assert_eq!(*current_destination, destination);
        assert!(app.automation_approval.available.is_none());
        let duplicate = complete(
            &mut app,
            held_delivery,
            Ok(delivery(identity, 1, 66, &page)),
        );
        assert_eq!(duplicate.units(), 0);
        drop(duplicate);
        assert_eq!(
            flight(&app),
            current,
            "an old delivery cannot release this read"
        );
        drop(complete(&mut app, current, Ok(review(identity, selected))));
        assert_eq!(
            app.automation_approval.proposal.as_ref(),
            Some(selected),
            "a queued notice must open its exact full review after the delivery clock advances"
        );
        assert_eq!(rekeyed.sequence, 2);
        assert_eq!(
            app.automation_approval.global.reviewed_identity,
            Some(identity)
        );
        assert_eq!(app.automation_approval.generation, generation + 1);
        assert_eq!(app.automation_approval.focus, 0, "Not now remains neutral");
        assert!(!app.automation_approval.deciding);
        assert!(app.automation_approval.global.flight.is_none());
        assert!(app.automation_approval.global.wanted.is_none());
    }
}

#[test]
fn delivery_clock_rekey_does_not_survive_reconnect_or_registry_replacement() {
    for replacement in [false, true] {
        let mut app = app();
        let a = proposal(1);
        let key = install(&mut app, &[a.clone()]);
        drop(
            app.update(Message::NotificationUi(notification_ui::Action::HostBound(
                crate::model::HostSessionProof {
                    host_session_id: crate::model::HostSessionId::new("retired-notice"),
                    boot_id: crate::model::BootId::new("retired-notice"),
                    host_incarnation: crate::model::HostIncarnation::new("retired-notice"),
                },
            ))),
        );
        model(&mut app, key.identity, 2, false);
        let old = flight(&app);
        drop(
            app.update(Message::NotificationUi(notification_ui::Action::Activated(
                crate::notifications::Target::AutomationProposal {
                    registry: key.identity,
                    id: a.id,
                    revision: a.revision,
                },
            ))),
        );
        assert!(matches!(
            app.automation_approval.global.wanted,
            Some(Read::Open { .. })
        ));
        let identity = if replacement {
            uuid::Uuid::from_u128(50_000)
        } else {
            key.identity
        };
        model(&mut app, identity, 3, true);
        assert_eq!(flight(&app), old, "retirement retains the physical slot");
        assert!(app.automation_approval.global.wanted.is_none());
        assert!(app.automation_approval.global.latest_open.is_none());
        drop(complete(
            &mut app,
            old,
            Ok(delivery(key.identity, 2, 1, &[a.clone()])),
        ));
        let current = flight(&app);
        assert!(matches!(current.2, Read::Delivery));
        assert!(current.1 > key.epoch);
        drop(complete(
            &mut app,
            current,
            Ok(delivery(identity, 3, 1, &[a])),
        ));
        assert!(!app.modal_is_open());
        assert!(app.automation_approval.proposal.is_none());
        assert!(app.automation_approval.global.authorized.is_none());
        assert!(app.automation_approval.global.reviewed_identity.is_none());
        assert!(app.automation_approval.global.flight.is_none());
    }
}

#[test]
fn rekeyed_notice_open_still_refuses_foreign_revision_focus_and_transport_results() {
    for refusal in ["identity", "revision", "focus", "transport"] {
        let mut app = app();
        let a = proposal(1);
        let key = install(&mut app, &[a.clone()]);
        drop(
            app.update(Message::NotificationUi(notification_ui::Action::HostBound(
                crate::model::HostSessionProof {
                    host_session_id: crate::model::HostSessionId::new("refused-notice"),
                    boot_id: crate::model::BootId::new("refused-notice"),
                    host_incarnation: crate::model::HostIncarnation::new("refused-notice"),
                },
            ))),
        );
        model(&mut app, key.identity, 2, false);
        let held = flight(&app);
        drop(
            app.update(Message::NotificationUi(notification_ui::Action::Activated(
                crate::notifications::Target::AutomationProposal {
                    registry: key.identity,
                    id: a.id,
                    revision: a.revision,
                },
            ))),
        );
        drop(complete(
            &mut app,
            held,
            Ok(delivery(key.identity, 2, 1, &[a.clone()])),
        ));
        let current = flight(&app);
        assert!(matches!(current.2, Read::Open { id, revision, .. }
            if id == a.id && revision == a.revision));
        let generation = app.automation_approval.generation;
        let response = match refusal {
            "identity" => Ok(review(uuid::Uuid::from_u128(60_000), &a)),
            "revision" => {
                let mut changed = a.clone();
                changed.revision += 1;
                Ok(review(key.identity, &changed))
            }
            "focus" => {
                let window = cosmic::iced::window::Id::RESERVED;
                app.core.set_main_window_id(Some(window));
                drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
                    window, false,
                ))));
                assert!(!app.refresh_ui.focused);
                Ok(review(key.identity, &a))
            }
            "transport" => Err("synthetic scoped full-review refusal".into()),
            _ => unreachable!(),
        };
        drop(complete(&mut app, current, response));
        assert!(!app.modal_is_open(), "{refusal}");
        assert!(app.automation_approval.proposal.is_none(), "{refusal}");
        assert!(app.automation_approval.available.is_none(), "{refusal}");
        assert!(
            app.automation_approval.global.authorized.is_none(),
            "{refusal}"
        );
        assert!(
            app.automation_approval.global.latest_open.is_none(),
            "{refusal}"
        );
        assert!(!app.automation_approval.deciding, "{refusal}");
        assert_eq!(app.automation_approval.generation, generation, "{refusal}");
        assert!(app.automation_approval.global.flight.is_none(), "{refusal}");
        assert!(app.automation_approval.global.wanted.is_none(), "{refusal}");
    }
}

#[test]
fn full_final_approval_page_disables_next_without_losing_first_or_refresh() {
    fn click(app: &Cagents, label: &str) -> (bool, Vec<Message>) {
        use cosmic::iced::{
            Event, Point, Rectangle, Size,
            advanced::{Layout, Shell, clipboard, layout, widget::Tree},
            mouse,
        };
        use iced_accessibility::accesskit::Role;
        let renderer = crate::app::tests::headless_renderer();
        let mut element = app.global_proposal_banner().expect("expanded requests");
        let mut tree = Tree::new(element.as_widget());
        let size = Size::new(1000.0, 900.0);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let buttons: Vec<_> = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter(|node| node.node().role() == Role::Button && node.node().label() == Some(label))
            .collect();
        assert_eq!(buttons.len(), 1, "unique {label} button");
        let enabled = !buttons[0].node().is_disabled();
        let rect = buttons[0].node().bounds().expect("mounted page control");
        let point = Point::new(
            ((rect.x0 + rect.x1) / 2.0) as f32,
            ((rect.y0 + rect.y1) / 2.0) as f32,
        );
        let mut messages = Vec::new();
        for event in [
            mouse::Event::ButtonPressed(mouse::Button::Left),
            mouse::Event::ButtonReleased(mouse::Button::Left),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(event),
                Layout::new(&node),
                mouse::Cursor::Available(point),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::with_size(size),
            );
        }
        (enabled, messages)
    }
    fn complete_page(app: &mut Cagents, rows: &[Proposal]) {
        let held = flight(app);
        let Read::Page { key, after, .. } = held.2.clone() else {
            panic!("actual page read")
        };
        drop(complete(
            app,
            held,
            Ok(Response::ProposalPage(ProposalPage {
                identity: key.identity,
                automation_proposal_sequence: key.sequence,
                after,
                proposals: rows.iter().map(ProposalSummary::from).collect(),
            })),
        ));
    }
    fn next_page(app: &mut Cagents, rows: &[Proposal]) {
        let (enabled, messages) = click(app, "Next requests");
        assert!(enabled, "the selected next page is reachable");
        assert_eq!(messages.len(), 1);
        for message in messages {
            drop(app.update(message));
        }
        complete_page(app, rows);
    }
    let mut failures = Vec::new();
    for total in [PROPOSAL_PAGE_SIZE * 2, PROPOSAL_PAGE_SIZE * 2 + 1] {
        let mut app = app();
        let rows: Vec<_> = (1..=total).map(|number| proposal(number as u128)).collect();
        let identity = app.workspace_model.identity;
        let held = flight(&app);
        drop(complete(
            &mut app,
            held,
            Ok(delivery(
                identity,
                9,
                total as u64,
                &rows[..PROPOSAL_PAGE_SIZE],
            )),
        ));
        drop(global(&mut app, Action::Toggle));
        let (enabled, messages) = click(&app, "Next requests");
        assert!(enabled);
        assert_eq!(messages.len(), 1);
        // Both actions come from the first rendered page. The second can be
        // delivered after the first read has already installed its result.
        let (enabled, delayed_next) = click(&app, "Next requests");
        assert!(enabled);
        assert_eq!(delayed_next.len(), 1);
        for message in messages {
            drop(app.update(message));
        }
        let held = flight(&app);
        let Read::Page { key, after, .. } = held.2.clone() else {
            panic!("actual Next read")
        };
        let last_full = &rows[PROPOSAL_PAGE_SIZE..PROPOSAL_PAGE_SIZE * 2];
        drop(complete(
            &mut app,
            held,
            Ok(Response::ProposalPage(ProposalPage {
                identity,
                automation_proposal_sequence: key.sequence,
                after,
                proposals: last_full.iter().map(ProposalSummary::from).collect(),
            })),
        ));
        assert_eq!(
            click(&app, "Next requests").0,
            total > PROPOSAL_PAGE_SIZE * 2,
            "the ordinary final-page boundary is correct before the replay"
        );
        for message in delayed_next {
            drop(app.update(message));
        }
        complete_page(&mut app, last_full);
        let (enabled, messages) = click(&app, "Next requests");
        if total == PROPOSAL_PAGE_SIZE * 2 {
            if enabled || !messages.is_empty() {
                failures.push("a delayed current-page Next must not expose an empty page");
            }
        } else {
            assert!(enabled, "the final partial page remains reachable");
            assert_eq!(messages.len(), 1);
            for message in messages {
                drop(app.update(message));
            }
            let held = flight(&app);
            let Read::Page { key, after, .. } = held.2.clone() else {
                panic!()
            };
            drop(complete(
                &mut app,
                held,
                Ok(Response::ProposalPage(ProposalPage {
                    identity,
                    automation_proposal_sequence: key.sequence,
                    after,
                    proposals: vec![ProposalSummary::from(rows.last().unwrap())],
                })),
            ));
            let (enabled, messages) = click(&app, "Next requests");
            assert!(!enabled);
            assert!(messages.is_empty());
        }
        drop(global(&mut app, Action::Refresh));
        let held = flight(&app);
        drop(complete(
            &mut app,
            held,
            Ok(delivery(
                identity,
                9,
                total as u64,
                &rows[..PROPOSAL_PAGE_SIZE],
            )),
        ));
        let (enabled, messages) = click(&app, "Next requests");
        if enabled || !messages.is_empty() {
            failures.push("same-snapshot Refresh must preserve the final-page boundary");
        }
        let (enabled, messages) = click(&app, "First requests");
        assert!(enabled);
        assert_eq!(messages.len(), 1);
        for message in messages {
            drop(app.update(message));
        }
        let held = flight(&app);
        let Read::Page { key, after, .. } = held.2.clone() else {
            panic!()
        };
        assert_eq!(after, None);
        drop(complete(
            &mut app,
            held,
            Ok(Response::ProposalPage(ProposalPage {
                identity,
                automation_proposal_sequence: key.sequence,
                after,
                proposals: rows[..PROPOSAL_PAGE_SIZE]
                    .iter()
                    .map(ProposalSummary::from)
                    .collect(),
            })),
        ));
        assert!(
            click(&app, "Next requests").0,
            "First restores forward navigation"
        );
    }

    // Replaying a cursor from an older page must also retain its proven rank.
    // Recovering only the current page's start would miss this four-page case.
    let mut app = app();
    let rows: Vec<_> = (1..=PROPOSAL_PAGE_SIZE * 4)
        .map(|number| proposal(number as u128))
        .collect();
    let identity = app.workspace_model.identity;
    let held = flight(&app);
    drop(complete(
        &mut app,
        held,
        Ok(delivery(
            identity,
            9,
            rows.len() as u64,
            &rows[..PROPOSAL_PAGE_SIZE],
        )),
    ));
    drop(global(&mut app, Action::Toggle));
    let (enabled, delayed_next) = click(&app, "Next requests");
    assert!(enabled);
    assert_eq!(delayed_next.len(), 1);
    for page in 1..4 {
        next_page(
            &mut app,
            &rows[PROPOSAL_PAGE_SIZE * page..PROPOSAL_PAGE_SIZE * (page + 1)],
        );
    }
    let (enabled, messages) = click(&app, "Next requests");
    assert!(!enabled, "the ordinary four-page boundary is correct");
    assert!(messages.is_empty());
    for message in delayed_next {
        drop(app.update(message));
    }
    complete_page(&mut app, &rows[PROPOSAL_PAGE_SIZE..PROPOSAL_PAGE_SIZE * 2]);
    for page in 2..4 {
        next_page(
            &mut app,
            &rows[PROPOSAL_PAGE_SIZE * page..PROPOSAL_PAGE_SIZE * (page + 1)],
        );
    }
    let (enabled, messages) = click(&app, "Next requests");
    if enabled || !messages.is_empty() {
        failures.push("a delayed older-page Next must not expose an empty page");
    }
    assert!(failures.is_empty(), "{failures:?}");
}
