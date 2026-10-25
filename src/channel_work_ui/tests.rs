use super::*;
use crate::{
    channel_work::{Claim, Receipt},
    model::ChannelId,
};
use cosmic::{
    Theme,
    iced::{self, advanced::widget::operation, mouse},
};

type Ui<'a> = iced::runtime::UserInterface<'a, Action, Theme, cosmic::Renderer>;
type Cache = iced::runtime::user_interface::Cache;

fn home(n: u128) -> Home {
    Home {
        registry: Uuid::from_u128(1),
        channel: ChannelId::from_uuid(Uuid::from_u128(n)),
        creation_ordinal: n as u32,
    }
}
fn target(number: u64) -> Target {
    Target::PullRequest(
        PullRequestKey::new(
            GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
            number,
        )
        .unwrap(),
    )
}
fn entry(number: u64, role: Role) -> Entry {
    Entry {
        id: Uuid::from_u128(100 + u128::from(number)),
        target: target(number),
        claims: vec![Claim {
            source: Source::User,
            role,
            active: true,
            confirmed_generation: 0,
        }],
        contexts: Vec::new(),
        suppressed: false,
        user_ended: false,
        observation: None,
        last_terminal_generation: 0,
        ever_claimed: true,
        revision: 1,
    }
}
fn snapshot(home: Home, entries: Vec<Entry>, revision: u64) -> Snapshot {
    let (history, entries) = entries
        .into_iter()
        .partition(|entry| entry.bucket() == Bucket::History);
    Snapshot {
        home,
        lifecycle: HomeLifecycle::Active,
        revision,
        entries,
        history,
        next_current: None,
        next_history: None,
    }
}
fn load(state: &mut State, effect: Effect, snapshot: Snapshot) {
    assert!(
        snapshot.validate().is_ok(),
        "fixture must satisfy the real DTO contract"
    );
    let Effect::Read { ticket, home, .. } = effect else {
        panic!("expected read")
    };
    assert_eq!(snapshot.home, home);
    assert!(
        state
            .update(Action::Loaded {
                ticket,
                result: Ok(snapshot)
            })
            .is_none()
    );
}
fn ready(home: Home, entries: Vec<Entry>) -> State {
    let mut state = State::default();
    let read = state.open(home, "Feature discussion".into()).unwrap();
    load(&mut state, read, snapshot(home, entries, 1));
    state
}
fn row(state: &State, number: u64) -> RowOwner {
    let view = state.view.as_ref().unwrap();
    let entry = view
        .snapshot
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .chain(&view.snapshot.as_ref().unwrap().history)
        .find(|entry| entry.target == target(number))
        .unwrap();
    RowOwner {
        view: view.owner,
        id: entry.id,
        target: entry.target.clone(),
        revision: entry.revision,
        bucket: entry.bucket(),
    }
}
fn refresh(state: &mut State, snapshot: Snapshot) {
    let read = state.refresh().unwrap();
    load(state, read, snapshot);
}
fn draw(ui: &mut Ui<'_>, renderer: &mut cosmic::Renderer) {
    ui.draw(
        renderer,
        &cosmic::theme::active(),
        &iced::advanced::renderer::Style::default(),
        mouse::Cursor::Unavailable,
    );
}
fn cached<'a>(
    state: &'a State,
    cache: Cache,
    renderer: &mut cosmic::Renderer,
    width: f32,
    height: f32,
) -> Ui<'a> {
    let mut ui = iced::runtime::UserInterface::build(
        state.view(width, height),
        iced::Size::new(width, height),
        cache,
        renderer,
    );
    draw(&mut ui, renderer);
    ui
}
fn bounds(ui: &Ui<'_>, name: &str) -> iced::Rectangle {
    let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let node = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .find(|node| node.node().label() == Some(name))
        .unwrap_or_else(|| panic!("missing actual named control: {name}"));
    let b = node.node().bounds().unwrap();
    let bounds = iced::Rectangle {
        x: b.x0 as f32,
        y: b.y0 as f32,
        width: (b.x1 - b.x0) as f32,
        height: (b.y1 - b.y0) as f32,
    };
    assert!(
        bounds.width > 0.0 && bounds.height > 0.0,
        "{name}: {bounds:?}"
    );
    bounds
}
fn events(
    ui: &mut Ui<'_>,
    renderer: &mut cosmic::Renderer,
    point: iced::Point,
    buttons: &[mouse::Event],
) -> Vec<Action> {
    let events = std::iter::once(iced::Event::Mouse(mouse::Event::CursorMoved {
        position: point,
    }))
    .chain(buttons.iter().cloned().map(iced::Event::Mouse))
    .collect::<Vec<_>>();
    let mut messages = Vec::new();
    let _ = ui.update(
        &events,
        mouse::Cursor::Available(point),
        renderer,
        &mut iced::advanced::clipboard::Null,
        &mut messages,
    );
    draw(ui, renderer);
    messages
}
fn click(ui: &mut Ui<'_>, renderer: &mut cosmic::Renderer, point: iced::Point) -> Vec<Action> {
    events(
        ui,
        renderer,
        point,
        &[
            mouse::Event::ButtonPressed(mouse::Button::Left),
            mouse::Event::ButtonReleased(mouse::Button::Left),
        ],
    )
}
fn open_name(number: u64) -> String {
    format!("Open {}", target_label(&target(number)))
}
fn end_name(number: u64) -> String {
    format!("End tracking {} here", target_label(&target(number)))
}

#[test]
fn retained_row_press_cannot_move_to_another_pr_home_or_mutable_revision() {
    for scenario in 0..4 {
        let mut state = ready(home(10), vec![entry(1, Role::Contribute)]);
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mutable = scenario == 3;
        let name = if mutable { end_name(1) } else { open_name(1) };
        let mut ui = cached(&state, Cache::new(), &mut renderer, 800.0, 900.0);
        let point = bounds(&ui, &name).center();
        assert!(
            events(
                &mut ui,
                &mut renderer,
                point,
                &[mouse::Event::ButtonPressed(mouse::Button::Left)]
            )
            .is_empty()
        );
        let cache = ui.into_cache();
        let next_number = if scenario == 0 { 2 } else { 1 };
        let mut next = entry(next_number, Role::Contribute);
        if mutable {
            next.revision = 2;
        }
        if scenario == 1 || scenario == 2 {
            let owner = state.owner().unwrap();
            state.update(Action::Close(owner));
            let next_home = if scenario == 1 { home(11) } else { home(10) };
            let read = state.open(next_home, "Feature discussion".into()).unwrap();
            load(&mut state, read, snapshot(next_home, vec![next], 2));
        } else {
            refresh(&mut state, snapshot(home(10), vec![next], 2));
        }
        let mut ui = cached(&state, cache, &mut renderer, 800.0, 900.0);
        let next_name = if mutable {
            end_name(next_number)
        } else {
            open_name(next_number)
        };
        assert!(
            bounds(&ui, &next_name).contains(point),
            "replacement must occupy the held hit area"
        );
        assert!(
            events(
                &mut ui,
                &mut renderer,
                point,
                &[mouse::Event::ButtonReleased(mouse::Button::Left)]
            )
            .is_empty(),
            "scenario {scenario} moved a native press"
        );
        let messages = click(&mut ui, &mut renderer, point);
        let _ = ui.into_cache();
        assert_eq!(messages.len(), 1);
        let effect = state.update(messages.into_iter().next().unwrap()).unwrap();
        if mutable {
            assert!(
                matches!(effect, Effect::Mutate { mutation, .. } if mutation.home == home(10) && mutation.expected_revision == 2 && mutation.change == Change::End { target: target(1) })
            );
        } else {
            assert!(
                matches!(effect, Effect::OpenPullRequest { home: effect_home, key, .. } if Some(effect_home) == state.home() && key.number == next_number)
            );
        }
    }
}

#[test]
fn unrelated_ledger_refresh_preserves_held_row_and_exact_navigation_destination() {
    for mutable in [false, true] {
        let mut state = ready(home(10), vec![entry(1, Role::Contribute)]);
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut ui = cached(&state, Cache::new(), &mut renderer, 800.0, 900.0);
        let name = if mutable { end_name(1) } else { open_name(1) };
        let point = bounds(&ui, &name).center();
        assert!(
            events(
                &mut ui,
                &mut renderer,
                point,
                &[mouse::Event::ButtonPressed(mouse::Button::Left)]
            )
            .is_empty()
        );
        let cache = ui.into_cache();
        refresh(
            &mut state,
            snapshot(
                home(10),
                vec![entry(1, Role::Contribute), entry(2, Role::Review)],
                8,
            ),
        );
        let mut ui = cached(&state, cache, &mut renderer, 800.0, 900.0);
        assert!(bounds(&ui, &name).contains(point));
        let actions = events(
            &mut ui,
            &mut renderer,
            point,
            &[mouse::Event::ButtonReleased(mouse::Button::Left)],
        );
        let _ = ui.into_cache();
        assert_eq!(
            actions.len(),
            1,
            "unchanged owner must retain a valid held click"
        );
        let effect = state.update(actions.into_iter().next().unwrap()).unwrap();
        if mutable {
            assert!(
                matches!(effect, Effect::Mutate { mutation, .. } if mutation.expected_revision == 1 && mutation.change == Change::End { target: target(1) })
            );
        } else {
            assert!(
                matches!(effect, Effect::OpenPullRequest { key, url, .. } if key.number == 1 && url.as_str() == "https://github.com/owner/repo/pull/1")
            );
        }
    }
}

fn completed(mutation: &Mutation, revision: u64, entries: Vec<Entry>) -> Reply {
    Reply {
        receipt: Receipt {
            operation: mutation.operation,
            home: mutation.home,
            revision,
            sequence: revision,
            changed: true,
        },
        snapshot: snapshot(mutation.home, entries, revision),
    }
}
fn begin_end(state: &mut State, number: u64) -> (Ticket, Mutation) {
    match state.update(Action::End(row(state, number))).unwrap() {
        Effect::Mutate {
            ticket, mutation, ..
        } => (ticket, mutation),
        _ => panic!("expected mutation"),
    }
}

#[test]
fn close_unknown_lookup_missing_and_retry_retain_original_mutation_and_home() {
    let mut state = ready(home(10), vec![entry(1, Role::Contribute)]);
    let (ticket, original) = begin_end(&mut state, 1);
    state.update(Action::Applied {
        ticket,
        result: Err(Failure::Unavailable),
    });
    state.escape();
    let read = state.open(home(11), "Other channel".into()).unwrap();
    load(
        &mut state,
        read,
        snapshot(home(11), vec![entry(2, Role::Contribute)], 1),
    );
    assert!(state.pending_for(home(10)).is_some());
    assert!(state.pending_for(home(11)).is_none());
    let wrong = state.owner().unwrap();
    assert!(
        state
            .update(Action::Retry(wrong, original.operation))
            .is_none()
    );
    let read = state.open(home(10), "Original".into()).unwrap();
    load(
        &mut state,
        read,
        snapshot(home(10), vec![entry(1, Role::Contribute)], 1),
    );
    let owner = state.owner().unwrap();
    let Effect::Lookup {
        ticket: lookup,
        home: observed,
        operation,
    } = state
        .update(Action::Check(owner, original.operation))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!((observed, operation), (original.home, original.operation));
    state.update(Action::LookedUp {
        ticket: lookup,
        operation,
        result: Ok(None),
    });
    assert!(!state.writable(owner));
    let Effect::Mutate {
        ticket: retry,
        mutation,
        ..
    } = state.update(Action::Retry(owner, operation)).unwrap()
    else {
        panic!()
    };
    assert_eq!(
        mutation, original,
        "retry may not change target, role, expected revision or operation"
    );
    // A pre-effect refusal of a retry cannot settle an earlier ambiguous write.
    state.update(Action::Applied {
        ticket: retry,
        result: Err(Failure::Refused("Changed revision".into())),
    });
    assert_eq!(
        state.pending_for(home(10)).unwrap().delivery,
        Delivery::Unknown
    );
    let mut ended = entry(1, Role::Contribute);
    ended.user_ended = true;
    ended.revision = 2;
    let mut reply = completed(&original, 2, vec![ended]);
    // The original End receipt remains immutable, while another user has
    // already resumed this relation. Adopt the current snapshot, not an old
    // snapshot associated with the original successful operation.
    let mut resumed = entry(1, Role::Review);
    resumed.revision = 3;
    reply.snapshot = snapshot(home(10), vec![resumed], 7);
    let Effect::Lookup { ticket: lookup, .. } =
        state.update(Action::Check(owner, operation)).unwrap()
    else {
        panic!()
    };
    let projection = state
        .update(Action::LookedUp {
            ticket: lookup,
            operation,
            result: Ok(Some(reply.receipt)),
        })
        .unwrap();
    assert!(state.pending.is_empty());
    // Confirmation does not carry a current projection. A separate read can
    // observe later work without rewriting the immutable original receipt.
    load(&mut state, projection, reply.snapshot);
    let current = state.view.as_ref().unwrap().snapshot.as_ref().unwrap();
    assert_eq!(current.revision, 7);
    assert!(current.history.is_empty());
    assert_eq!(current.entries[0].target, target(1));
    assert_eq!(current.entries[0].revision, 3);
    assert_eq!(current.entries[0].role(), Some(Role::Review));
}

#[test]
fn mismatched_receipts_and_late_page_reads_never_clear_or_replace_current_state() {
    let mut state = ready(home(10), vec![entry(1, Role::Contribute)]);
    let (ticket, original) = begin_end(&mut state, 1);
    let old_read = state.refresh().unwrap();
    let mut wrong = completed(&original, 2, vec![entry(1, Role::Contribute)]);
    wrong.receipt.operation = Uuid::new_v4();
    state.update(Action::Applied {
        ticket,
        result: Ok(wrong),
    });
    assert_eq!(state.pending.len(), 1);
    let owner = state.owner().unwrap();
    let Effect::Lookup {
        ticket: lookup,
        operation,
        ..
    } = state
        .update(Action::Check(owner, original.operation))
        .unwrap()
    else {
        panic!()
    };
    let mut ended = entry(1, Role::Contribute);
    ended.user_ended = true;
    ended.revision = 2;
    let fresh = completed(&original, 2, vec![ended]);
    let projection = state
        .update(Action::LookedUp {
            ticket: lookup,
            operation,
            result: Ok(Some(fresh.receipt.clone())),
        })
        .unwrap();
    assert!(state.pending.is_empty());
    assert!(state.view.as_ref().unwrap().loading.is_some());
    // Same global revision is possible for different history pages. The old
    // read ticket must have been retired when the latest mutation reply won.
    let Effect::Read {
        ticket: stale_ticket,
        ..
    } = old_read
    else {
        panic!()
    };
    state.update(Action::Loaded {
        ticket: stale_ticket,
        result: Ok(snapshot(home(10), vec![entry(2, Role::Review)], 2)),
    });
    assert_ne!(
        state.view.as_ref().unwrap().snapshot.as_ref(),
        Some(&fresh.snapshot),
        "a receipt must not pretend to contain the current projection"
    );
    load(&mut state, projection, fresh.snapshot.clone());
    assert_eq!(
        state.view.as_ref().unwrap().snapshot.as_ref(),
        Some(&fresh.snapshot)
    );
    let other_read = state.open(home(11), "Other".into()).unwrap();
    load(
        &mut state,
        other_read,
        snapshot(home(11), vec![entry(3, Role::Review)], 3),
    );
    state.update(Action::Applied {
        ticket,
        result: Ok(fresh),
    });
    assert_eq!(state.home(), Some(home(11)));
    assert_eq!(
        state
            .view
            .as_ref()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .entries[0]
            .target,
        target(3)
    );
}

#[test]
fn pending_limit_never_evicts_unknown_changes_and_keeps_reads_and_navigation() {
    let mut state = State::default();
    for n in 10..10 + MAX_PENDING as u128 {
        let read = state.open(home(n), "Pending".into()).unwrap();
        load(
            &mut state,
            read,
            snapshot(home(n), vec![entry(1, Role::Contribute)], 1),
        );
        let (ticket, _) = begin_end(&mut state, 1);
        state.update(Action::Applied {
            ticket,
            result: Err(Failure::Unavailable),
        });
        state.escape();
    }
    let original = state
        .pending
        .iter()
        .map(|pending| pending.mutation.clone())
        .collect::<Vec<_>>();
    let read = state.open(home(99), "Still readable".into()).unwrap();
    load(
        &mut state,
        read,
        snapshot(home(99), vec![entry(2, Role::Contribute)], 1),
    );
    assert!(!state.writable(state.owner().unwrap()));
    assert!(
        state
            .update(Action::Attach(state.owner().unwrap()))
            .is_none()
    );
    assert!(state.refresh().is_some());
    state.escape();
    assert!(!state.is_open());
    assert_eq!(
        state
            .pending
            .iter()
            .map(|pending| pending.mutation.clone())
            .collect::<Vec<_>>(),
        original
    );
}

fn filled_form(state: &mut State) -> FormOwner {
    state.update(Action::Attach(state.owner().unwrap()));
    let owner = state.view.as_ref().unwrap().form.as_ref().unwrap().owner;
    state.update(Action::Field(owner, Field::Repository, "owner/repo".into()));
    state.update(Action::Field(owner, Field::Number, "7".into()));
    owner
}
#[test]
fn real_form_submit_callbacks_are_draft_owned_and_keyboard_uses_current_role() {
    let mut state = ready(home(10), vec![]);
    let old = filled_form(&mut state);
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = cached(&state, Cache::new(), &mut renderer, 400.0, 1600.0);
    let point = bounds(&ui, "Track PR").center();
    let old_submit = click(&mut ui, &mut renderer, point);
    let cache = ui.into_cache();
    assert!(matches!(old_submit.as_slice(), [Action::Submit { owner, .. }] if *owner == old));
    state.update(Action::Dismiss(old));
    let confirmation = state
        .view
        .as_ref()
        .unwrap()
        .confirmation
        .as_ref()
        .unwrap()
        .id;
    state.update(Action::Discard(state.owner().unwrap(), confirmation));
    let current = filled_form(&mut state);
    assert_ne!(old, current);
    assert!(
        state
            .update(old_submit.into_iter().next().unwrap())
            .is_none()
    );
    state.update(Action::Role(old, 1));
    assert_eq!(
        state.view.as_ref().unwrap().form.as_ref().unwrap().role,
        Role::Contribute
    );
    state.update(Action::Role(current, 2));
    let mut ui = cached(&state, cache, &mut renderer, 400.0, 1600.0);
    ui.operate(
        &renderer,
        &mut operation::focusable::focus::<()>(form_input_id(current, Field::Number)),
    );
    let key = iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter);
    let enter = iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::Enter),
        location: iced::keyboard::Location::Standard,
        modifiers: iced::keyboard::Modifiers::default(),
        text: None,
        repeat: false,
    });
    let mut actions = Vec::new();
    let _ = ui.update(
        &[enter],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut iced::advanced::clipboard::Null,
        &mut actions,
    );
    let _ = ui.into_cache();
    assert!(
        matches!(actions.as_slice(), [Action::Submit { owner, role: Role::Review, target: actual, revision: 0 }] if *owner == current && *actual == target(7))
    );
    let effect = state.update(actions.into_iter().next().unwrap()).unwrap();
    assert!(
        matches!(effect, Effect::Mutate { mutation, .. } if mutation.change == Change::Claim { target: target(7), role: Role::Review })
    );
}

#[test]
fn current_entry_cas_and_lifecycle_refuse_stale_role_actions_without_retargeting() {
    let mut state = ready(home(10), vec![entry(1, Role::Contribute)]);
    let old = row(&state, 1);
    state.update(Action::Correct(old.clone()));
    let form = state.view.as_ref().unwrap().form.as_ref().unwrap().owner;
    let mut changed = entry(1, Role::Contribute);
    changed.revision = 2;
    refresh(&mut state, snapshot(home(10), vec![changed], 2));
    assert!(state.update(Action::End(old)).is_none());
    assert!(
        state
            .update(Action::Submit {
                owner: form,
                target: target(1),
                role: Role::Contribute,
                revision: 1
            })
            .is_none()
    );
    assert!(state.pending.is_empty());
    let mut archived = state
        .view
        .as_ref()
        .unwrap()
        .snapshot
        .as_ref()
        .unwrap()
        .clone();
    for entry in &mut archived.entries {
        entry.user_ended = true;
    }
    archived.history.append(&mut archived.entries);
    archived.lifecycle = HomeLifecycle::Archived;
    archived.revision = 3;
    refresh(&mut state, archived);
    assert!(state.update(Action::Correct(row(&state, 1))).is_none());
    assert!(
        state
            .update(Action::Attach(state.owner().unwrap()))
            .is_none()
    );
    assert!(
        matches!(state.update(Action::Open(row(&state, 1))), Some(Effect::OpenPullRequest { key, .. }) if key.number == 1)
    );
}

#[test]
fn bounded_history_pages_validate_home_and_replace_instead_of_accumulating() {
    let mut historical = entry(1, Role::Contribute);
    historical.user_ended = true;
    let mut state = ready(home(10), vec![historical]);
    {
        let current = state.view.as_mut().unwrap().snapshot.as_mut().unwrap();
        current.revision = 20;
        current.next_history = Some(10);
    }
    let owner = state.owner().unwrap();
    let Effect::Read { ticket, page, .. } = state.update(Action::Earlier(owner, 10)).unwrap()
    else {
        panic!()
    };
    assert_eq!(
        page,
        Page {
            current_before: None,
            before: Some(10),
            limit: 50
        }
    );
    let mut second = entry(2, Role::Contribute);
    second.user_ended = true;
    let mut next = snapshot(home(10), vec![second], 20);
    next.next_history = Some(9);
    state.update(Action::Loaded {
        ticket,
        result: Ok(next),
    });
    let view = state.view.as_ref().unwrap();
    assert_eq!(view.snapshot.as_ref().unwrap().history.len(), 1);
    assert_eq!(view.snapshot.as_ref().unwrap().history[0].target, target(2));
    let Effect::Read { ticket, .. } = state.refresh().unwrap() else {
        panic!()
    };
    state.update(Action::Loaded {
        ticket,
        result: Ok(snapshot(home(11), vec![], 3)),
    });
    assert_eq!(
        state
            .view
            .as_ref()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .history[0]
            .target,
        target(2)
    );
    assert!(state.view.as_ref().unwrap().error.is_some());
}

#[test]
fn narrow_render_separates_review_from_owned_work_and_exposes_real_controls() {
    for width in [300.0, 900.0] {
        let mut reviewed = entry(2, Role::Review);
        let session = crate::model::TerminalId::from_uuid(Uuid::from_u128(99));
        reviewed.claims.push(Claim {
            source: Source::Session(crate::channel_work::Contributor {
                session,
                lineage: crate::agent_execution::Lineage(session),
                agent: None,
                run: None,
            }),
            role: Role::Contribute,
            active: true,
            confirmed_generation: 0,
        });
        let state = ready(home(10), vec![entry(1, Role::Contribute), reviewed]);
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let ui = cached(&state, Cache::new(), &mut renderer, width, 600.0);
        let open = bounds(&ui, &open_name(1));
        assert!(open.x >= 0.0 && open.x + open.width <= width + 0.01);
        assert!(bounds(&ui, "Track PR…").width > 0.0);
        let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let labels = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| node.node().label())
            .collect::<Vec<_>>();
        assert!(labels.contains(&"Working on"));
        assert!(labels.contains(&"Reviews and references"));
        assert!(
            labels.contains(&"On this page: 1 work relations · 1 review or reference relations")
        );
    }
}

#[test]
fn dirty_form_leave_confirmation_preserves_intent_and_discard_uses_captured_destination() {
    let mut state = ready(home(10), vec![entry(1, Role::Contribute)]);
    let draft = filled_form(&mut state);
    let owner = state.owner().unwrap();
    assert!(
        matches!(state.open(home(11), "Destination".into()), Some(Effect::Focus { owner: focused, .. }) if focused == owner)
    );
    assert_eq!(state.home(), Some(home(10)));
    assert!(state.has_confirmation());
    let first_confirmation = state
        .view
        .as_ref()
        .unwrap()
        .confirmation
        .as_ref()
        .unwrap()
        .id;
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = cached(&state, Cache::new(), &mut renderer, 400.0, 1600.0);
    let point = bounds(&ui, "Keep editing").center();
    let actions = click(&mut ui, &mut renderer, point);
    let cache = ui.into_cache();
    assert!(
        matches!(actions.as_slice(), [Action::Keep(actual, id)] if *actual == owner && *id == first_confirmation)
    );
    state.update(actions.into_iter().next().unwrap());
    assert_eq!(
        state.view.as_ref().unwrap().form.as_ref().unwrap().owner,
        draft
    );
    assert_eq!(
        state
            .view
            .as_ref()
            .unwrap()
            .form
            .as_ref()
            .unwrap()
            .repository,
        "owner/repo"
    );
    assert!(!state.has_confirmation());
    // A fresh close prompt must not adopt callbacks from the old switch prompt.
    state.update(Action::Close(owner));
    let second_confirmation = state
        .view
        .as_ref()
        .unwrap()
        .confirmation
        .as_ref()
        .unwrap()
        .id;
    state.update(Action::Discard(owner, first_confirmation));
    assert!(state.is_open());
    let mut ui = cached(&state, cache, &mut renderer, 400.0, 1600.0);
    let point = bounds(&ui, "Discard draft").center();
    let actions = click(&mut ui, &mut renderer, point);
    let _ = ui.into_cache();
    assert!(
        matches!(actions.as_slice(), [Action::Discard(actual, id)] if *actual == owner && *id == second_confirmation)
    );
    assert!(
        matches!(state.update(actions.into_iter().next().unwrap()), Some(Effect::Close { home: closed }) if closed == home(10))
    );
    assert!(!state.is_open());
    // A confirmed channel switch uses the original captured Home, not another
    // navigation attempt while the prompt was visible.
    let read = state.open(home(10), "Original".into()).unwrap();
    load(&mut state, read, snapshot(home(10), vec![], 1));
    filled_form(&mut state);
    state.open(home(11), "First destination".into());
    assert!(state.open(home(12), "Later destination".into()).is_none());
    let view = state.view.as_ref().unwrap();
    let discard = Action::Discard(view.owner, view.confirmation.as_ref().unwrap().id);
    assert!(
        matches!(state.update(discard), Some(Effect::Read { home: destination, .. }) if destination == home(11))
    );
    assert_eq!(state.home(), Some(home(11)));
}

#[test]
fn explicit_refresh_is_network_typed_but_background_refresh_is_read_only() {
    let mut state = ready(home(10), vec![entry(1, Role::Contribute)]);
    assert!(
        matches!(state.refresh(), Some(Effect::Read { home: requested, .. }) if requested == home(10))
    );
    let ticket = state.view.as_ref().unwrap().loading.unwrap();
    state.update(Action::Loaded {
        ticket,
        result: Ok(snapshot(home(10), vec![entry(1, Role::Contribute)], 1)),
    });
    state.view.as_mut().unwrap().page.before = Some(10);
    assert!(
        matches!(state.update(Action::Refresh(state.owner().unwrap())), Some(Effect::Refresh { home: requested, .. }) if requested == home(10))
    );
    assert_eq!(state.view.as_ref().unwrap().page.before, Some(10));
    assert_eq!(state.view.as_ref().unwrap().read_page.unwrap().before, None);
}

#[test]
fn context_disclosure_captures_desired_state_and_retires_native_press_when_label_changes() {
    let mut state = ready(home(10), vec![entry(1, Role::Contribute)]);
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = cached(&state, Cache::new(), &mut renderer, 800.0, 900.0);
    let point = bounds(&ui, "Why here?").center();
    let expand = click(&mut ui, &mut renderer, point);
    assert!(matches!(expand.as_slice(), [Action::Details(_, true)]));
    assert!(
        events(
            &mut ui,
            &mut renderer,
            point,
            &[mouse::Event::ButtonPressed(mouse::Button::Left)]
        )
        .is_empty()
    );
    let cache = ui.into_cache();
    let expand = expand.into_iter().next().unwrap();
    state.update(expand.clone());
    let expanded_id = state.view.as_ref().unwrap().details;
    assert!(expanded_id.is_some());
    state.update(expand);
    assert_eq!(
        state.view.as_ref().unwrap().details,
        expanded_id,
        "a queued expand must not toggle an expanded row closed"
    );
    let mut ui = cached(&state, cache, &mut renderer, 800.0, 900.0);
    let hide = bounds(&ui, "Hide context");
    assert!(hide.contains(point));
    assert!(
        events(
            &mut ui,
            &mut renderer,
            point,
            &[mouse::Event::ButtonReleased(mouse::Button::Left)]
        )
        .is_empty(),
        "Why-here hold must not become Hide-context"
    );
    let actions = click(&mut ui, &mut renderer, hide.center());
    let _ = ui.into_cache();
    assert!(matches!(actions.as_slice(), [Action::Details(_, false)]));
    let collapse = actions.into_iter().next().unwrap();
    state.update(collapse.clone());
    state.update(collapse);
    assert_eq!(state.view.as_ref().unwrap().details, None);
}

#[test]
fn current_pages_replace_both_buckets_and_keep_history_cursor_independent() {
    let mut state = ready(home(10), vec![entry(1, Role::Contribute)]);
    {
        let view = state.view.as_mut().unwrap();
        view.page.before = Some(90);
        let snapshot = view.snapshot.as_mut().unwrap();
        snapshot.revision = 100;
        snapshot.next_current = Some(40);
    }
    let owner = state.owner().unwrap();
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = cached(&state, Cache::new(), &mut renderer, 900.0, 1100.0);
    let point = bounds(&ui, "Next current page").center();
    let mut actions = click(&mut ui, &mut renderer, point);
    let _ = ui.into_cache();
    assert!(matches!(actions.as_slice(), [Action::NextCurrent(_, 40)]));
    let next = actions.remove(0);
    let Effect::Read { ticket, page, .. } = state.update(next.clone()).unwrap() else {
        panic!()
    };
    assert_eq!(page.current_before, Some(40));
    assert_eq!(
        page.before,
        Some(90),
        "current navigation preserves history position"
    );
    let mut confirmation = entry(2, Role::Contribute);
    confirmation.claims.clear();
    confirmation.ever_claimed = false;
    assert_eq!(confirmation.bucket(), Bucket::NeedsConfirmation);
    let mut second = snapshot(home(10), vec![confirmation], 100);
    second.next_current = Some(30);
    assert!(second.validate().is_ok());
    state.update(Action::Loaded {
        ticket,
        result: Ok(second.clone()),
    });
    assert!(
        state.update(next).is_none(),
        "queued old page navigation cannot skip the displayed cursor"
    );
    let current = state.view.as_ref().unwrap().snapshot.as_ref().unwrap();
    assert_eq!(current.entries.len(), 1);
    assert_eq!(
        current.entries[0].target,
        target(2),
        "old page must not accumulate"
    );
    let ui = cached(&state, Cache::new(), &mut renderer, 900.0, 1100.0);
    let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let labels = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .filter_map(|node| node.node().label())
        .collect::<Vec<_>>();
    assert!(labels.contains(&"No ongoing work on this page"));
    assert!(bounds(&ui, "Latest current work").width > 0.0);
    drop(ui);
    let Effect::Read { ticket, .. } = state.update(Action::NextCurrent(owner, 30)).unwrap() else {
        panic!()
    };
    let mut nondecreasing = second.clone();
    nondecreasing.next_current = Some(30);
    state.update(Action::Loaded {
        ticket,
        result: Ok(nondecreasing),
    });
    assert_eq!(
        state.view.as_ref().unwrap().snapshot.as_ref(),
        Some(&second)
    );
    assert!(state.view.as_ref().unwrap().error.is_some());
    let Effect::Read { page, ticket, .. } = state.update(Action::LatestCurrent(owner)).unwrap()
    else {
        panic!()
    };
    assert_eq!(page.current_before, None);
    assert_eq!(page.before, Some(90));
    state.update(Action::Loaded {
        ticket,
        result: Ok(snapshot(home(10), vec![entry(3, Role::Review)], 100)),
    });
    assert_eq!(
        state
            .view
            .as_ref()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .entries[0]
            .target,
        target(3)
    );
    let Effect::Refresh { ticket, .. } = state.update(Action::Refresh(owner)).unwrap() else {
        panic!()
    };
    assert_eq!(
        state.view.as_ref().unwrap().read_page.unwrap(),
        Page {
            current_before: None,
            before: None,
            limit: PAGE_SIZE
        }
    );
    state.update(Action::Loaded {
        ticket,
        result: Err(Failure::Unavailable),
    });
    assert_eq!(
        state.view.as_ref().unwrap().page.before,
        Some(90),
        "failed latest read preserves displayed page position"
    );
}

#[test]
fn receipt_confirmation_settles_without_projection_and_never_rebinds_offscreen() {
    for wrong in 0..5 {
        let mut state = ready(home(10), vec![entry(1, Role::Contribute)]);
        let (ticket, mutation) = begin_end(&mut state, 1);
        state.update(Action::Applied {
            ticket,
            result: Err(Failure::Unavailable),
        });
        let owner = state.owner().unwrap();
        let Effect::Lookup {
            ticket, operation, ..
        } = state
            .update(Action::Check(owner, mutation.operation))
            .unwrap()
        else {
            panic!()
        };
        let mut receipt = completed(&mutation, 2, Vec::new()).receipt;
        match wrong {
            0 => receipt.home = home(11),
            1 => receipt.operation = Uuid::new_v4(),
            2 => receipt.revision = 3,
            3 => receipt.changed = false,
            _ => {}
        }
        let effect = state.update(Action::LookedUp {
            ticket,
            operation,
            result: Ok(Some(receipt)),
        });
        if wrong < 4 {
            assert!(effect.is_none());
            assert_eq!(
                state.pending_for(home(10)).unwrap().delivery,
                Delivery::Unknown
            );
            assert!(!state.writable(owner));
        } else {
            assert!(state.pending.is_empty());
            let Effect::Read {
                ticket,
                home: observed,
                ..
            } = effect.unwrap()
            else {
                panic!()
            };
            assert_eq!(observed, mutation.home);
            state.update(Action::Loaded {
                ticket,
                result: Err(Failure::Unavailable),
            });
            assert!(
                state.pending.is_empty(),
                "projection failure cannot undo a confirmed operation"
            );
            assert_eq!(
                state
                    .view
                    .as_ref()
                    .unwrap()
                    .snapshot
                    .as_ref()
                    .unwrap()
                    .entries[0]
                    .revision,
                1,
                "no receipt-derived current status"
            );
            assert!(
                state
                    .view
                    .as_ref()
                    .unwrap()
                    .notice
                    .as_ref()
                    .unwrap()
                    .contains("confirmed")
            );
        }
    }
    let mut state = ready(home(10), vec![entry(1, Role::Contribute)]);
    let (ticket, mutation) = begin_end(&mut state, 1);
    state.update(Action::Applied {
        ticket,
        result: Err(Failure::Unavailable),
    });
    let owner = state.owner().unwrap();
    let Effect::Lookup {
        ticket, operation, ..
    } = state
        .update(Action::Check(owner, mutation.operation))
        .unwrap()
    else {
        panic!()
    };
    let other_read = state.open(home(11), "Other".into()).unwrap();
    load(
        &mut state,
        other_read,
        snapshot(home(11), vec![entry(2, Role::Review)], 1),
    );
    assert!(
        state
            .update(Action::LookedUp {
                ticket,
                operation,
                result: Ok(Some(completed(&mutation, 2, Vec::new()).receipt))
            })
            .is_none(),
        "offscreen confirmation must not read/focus another home"
    );
    assert!(state.pending.is_empty());
    assert_eq!(state.home(), Some(home(11)));
    assert!(state.view.as_ref().unwrap().notice.is_none());
}

fn labels(ui: &Ui<'_>) -> Vec<String> {
    let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
    nodes
        .root()
        .iter()
        .chain(nodes.children())
        .filter_map(|node| node.node().label().map(str::to_owned))
        .collect()
}

#[test]
fn channel_home_keeps_multiple_full_pr_keys_and_declared_roles_without_checkout_ownership() {
    use crate::channel_work::{CheckoutContext, Freshness, Observation, PrState};
    let home = home(10);
    let mut contributed = entry(1, Role::Contribute);
    let mut stewarded = entry(2, Role::Steward);
    stewarded.target = Target::PullRequest(
        PullRequestKey::new(
            GitHubRepoKey::new("github.com", None, "fork", "other").unwrap(),
            1,
        )
        .unwrap(),
    );
    contributed.observation = Some(Observation {
        generation: 1,
        state: PrState::Open { draft: false },
        freshness: Freshness::Stale,
        title: Some("Fix parser".into()),
        node_id: None,
        head: None,
        observed_at_ms: 1,
    });
    contributed.contexts.push(CheckoutContext {
        id: Uuid::from_u128(990),
        registry: home.registry,
        repository: "/nonexistent/project".into(),
        path: "/nonexistent/deleted-checkout".into(),
        branch: Some("same-branch".into()),
        head: Some("a".repeat(40)),
        generation: 1,
        dirty: Some(true),
    });
    stewarded.contexts = contributed.contexts.clone();
    let mut reviewed = entry(3, Role::Review);
    let session = crate::model::TerminalId::from_uuid(Uuid::from_u128(999));
    reviewed.claims.push(Claim {
        source: Source::Session(crate::channel_work::Contributor {
            session,
            lineage: crate::agent_execution::Lineage(session),
            agent: None,
            run: None,
        }),
        role: Role::Contribute,
        active: true,
        confirmed_generation: 0,
    });
    let reference = entry(4, Role::Reference);
    let mut ended = entry(5, Role::Contribute);
    ended.user_ended = true;
    let mut observed = entry(6, Role::Reference);
    observed.claims.clear();
    observed.ever_claimed = false;
    let entries = vec![
        contributed.clone(),
        stewarded.clone(),
        reviewed,
        reference,
        observed,
        ended,
    ];
    for width in [300.0, 1000.0] {
        let mut state = ready(home, entries.clone());
        let read = state.open(home, "Parser delivery".into()).unwrap();
        load(&mut state, read, snapshot(home, entries.clone(), 2));
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let ui = cached(&state, Cache::new(), &mut renderer, width, 3000.0);
        let text = labels(&ui);
        assert!(text.iter().any(|s| s == "Parser delivery"));
        assert!(text.iter().any(|s| s == "Working on"));
        assert!(text.iter().any(|s| s == "Reviews and references"));
        assert!(
            text.iter()
                .any(|s| s == "On this page: 2 work relations · 2 review or reference relations")
        );
        assert!(
            text.iter()
                .any(|s| s == "Contributing · Last known: Open PR")
        );
        assert!(
            text.iter()
                .any(|s| s == "Stewarding · PR status not yet observed")
        );
        assert!(
            text.iter()
                .any(|s| s == "Review only · PR status not yet observed")
        );
        assert!(!text.iter().any(|s| s == "Confirm previous work"));
        for item in [&contributed, &stewarded] {
            let hit = bounds(&ui, &format!("Open {}", target_label(&item.target)));
            assert!(hit.x >= 0.0 && hit.x + hit.width <= width + 0.01);
        }
        let _ = ui.into_cache();
        for item in [&contributed, &stewarded] {
            let mut ui = cached(&state, Cache::new(), &mut renderer, width, 3000.0);
            let hit = bounds(&ui, &format!("Open {}", target_label(&item.target))).center();
            let mut messages = click(&mut ui, &mut renderer, hit);
            let _ = ui.into_cache();
            let Effect::OpenPullRequest {
                home: captured,
                key,
                url,
            } = state.update(messages.remove(0)).unwrap()
            else {
                panic!()
            };
            assert_eq!(captured, home);
            assert_eq!(Target::PullRequest(key), item.target);
            assert_eq!(Some(url), item.target.pull_request_url().unwrap());
        }
        let owner = state.owner().unwrap();
        state.update(Action::Tab(owner, Bucket::NeedsConfirmation));
        let ui = cached(&state, Cache::new(), &mut renderer, width, 1500.0);
        assert!(
            labels(&ui)
                .iter()
                .any(|s| s == &format!("Confirm role… for {}", target_label(&target(6))))
        );
        let _ = ui.into_cache();
        state.update(Action::Tab(owner, Bucket::History));
        let ui = cached(&state, Cache::new(), &mut renderer, width, 1500.0);
        assert!(
            labels(&ui)
                .iter()
                .any(|s| s == &format!("Open {}", target_label(&target(5))))
        );
        assert!(labels(&ui).iter().any(|s| s == "Parser delivery"));
    }
}

#[test]
fn reopened_pr_fresh_review_shows_previous_work_until_explicit_captured_role_confirmation() {
    use crate::channel_work::{
        Contributor, Freshness, HomeState, Observation, Origin, PollOutcome, PrState,
        observe_entry, reduce_entry,
    };
    let home = home(10);
    let header = HomeState {
        home,
        lifecycle: HomeLifecycle::Active,
        revision: 1000,
    };
    let actor = |id| {
        let session = crate::model::TerminalId::from_uuid(Uuid::from_u128(id));
        Origin::Actor {
            home,
            actor: Contributor {
                session,
                lineage: crate::agent_execution::Lineage(session),
                agent: None,
                run: None,
            },
            contexts: Vec::new(),
        }
    };
    let claim = |old: Option<&Entry>, role, actor: &Origin, id| {
        reduce_entry(
            &header,
            old,
            &Mutation {
                home,
                expected_revision: old.map_or(0, |e| e.revision),
                operation: Uuid::from_u128(id),
                change: Change::Claim {
                    target: target(1),
                    role,
                },
            },
            actor,
        )
        .unwrap()
        .entry
    };
    let observe = |old: &Entry, generation, state| {
        observe_entry(
            &header,
            old,
            generation,
            PollOutcome::Success(Observation {
                generation,
                state,
                freshness: Freshness::Current,
                title: Some("Same PR".into()),
                node_id: None,
                head: None,
                observed_at_ms: generation,
            }),
        )
        .unwrap()
        .entry
    };
    let original = claim(None, Role::Contribute, &actor(10), 100);
    let reopened = observe(
        &observe(&original, 1, PrState::Merged),
        2,
        PrState::Open { draft: false },
    );
    let review = claim(Some(&reopened), Role::Review, &actor(11), 101);
    assert_eq!(review.bucket(), Bucket::Ongoing);
    assert!(!review.is_ongoing_work());
    let revision = review.revision;
    let mut state = State::default();
    let read = state.open(home, "Release discussion".into()).unwrap();
    load(&mut state, read, snapshot(home, vec![review.clone()], 1000));
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = cached(&state, Cache::new(), &mut renderer, 800.0, 1800.0);
    let text = labels(&ui);
    assert!(text.iter().any(|s| s == "Confirm previous work"));
    assert!(
        text.iter()
            .any(|s| s == "Previously contributing · Open PR")
    );
    assert!(
        text.iter()
            .any(|s| s == "No contributed or stewarded work on this page.")
    );
    assert!(!text.iter().any(|s| s == "Reviews and references"));
    assert!(!text.iter().any(|s| s == "Contributing · Open PR"));
    let hit = bounds(
        &ui,
        &format!("Confirm work role… for {}", target_label(&target(1))),
    )
    .center();
    let mut messages = click(&mut ui, &mut renderer, hit);
    let _ = ui.into_cache();
    assert!(matches!(
        state.update(messages.remove(0)),
        Some(Effect::Focus { .. })
    ));
    assert!(
        state.pending.is_empty(),
        "opening the form does not claim work"
    );
    let mut ui = cached(&state, Cache::new(), &mut renderer, 800.0, 1800.0);
    let hit = bounds(&ui, "Apply tracking").center();
    let mut messages = click(&mut ui, &mut renderer, hit);
    let _ = ui.into_cache();
    let Effect::Mutate {
        ticket, mutation, ..
    } = state.update(messages.remove(0)).unwrap()
    else {
        panic!()
    };
    assert_eq!(mutation.expected_revision, revision);
    assert_eq!(mutation.home, home);
    assert_eq!(
        mutation.change,
        Change::Claim {
            target: target(1),
            role: Role::Contribute
        }
    );
    let confirmed = reduce_entry(&header, Some(&review), &mutation, &Origin::User)
        .unwrap()
        .entry;
    assert!(confirmed.is_ongoing_work());
    let mut reply = completed(&mutation, confirmed.revision, vec![confirmed]);
    reply.snapshot.revision = 1001;
    reply.receipt.sequence = 1001;
    state.update(Action::Applied {
        ticket,
        result: Ok(reply),
    });
    let ui = cached(&state, Cache::new(), &mut renderer, 800.0, 1800.0);
    let text = labels(&ui);
    assert!(!text.iter().any(|s| s == "Confirm previous work"));
    assert!(text.iter().any(|s| s == "Contributing · Open PR"));
}

#[test]
fn home_native_pr_hold_rejects_same_number_other_repository_and_keeps_current_target_on_metadata_refresh()
 {
    let mut state = ready(home(10), vec![entry(1, Role::Contribute)]);
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = cached(&state, Cache::new(), &mut renderer, 800.0, 1200.0);
    let hit = bounds(&ui, &open_name(1)).center();
    assert!(
        events(
            &mut ui,
            &mut renderer,
            hit,
            &[mouse::Event::ButtonPressed(mouse::Button::Left)]
        )
        .is_empty()
    );
    let cache = ui.into_cache();
    let mut fork = entry(1, Role::Contribute);
    fork.target = Target::PullRequest(
        PullRequestKey::new(
            GitHubRepoKey::new("github.com", None, "other", "repo").unwrap(),
            1,
        )
        .unwrap(),
    );
    refresh(&mut state, snapshot(home(10), vec![fork.clone()], 2));
    let name = format!("Open {}", target_label(&fork.target));
    let mut ui = cached(&state, cache, &mut renderer, 800.0, 1200.0);
    assert!(bounds(&ui, &name).contains(hit));
    assert!(
        events(
            &mut ui,
            &mut renderer,
            hit,
            &[mouse::Event::ButtonReleased(mouse::Button::Left)]
        )
        .is_empty()
    );
    let mut messages = click(&mut ui, &mut renderer, hit);
    let _ = ui.into_cache();
    assert!(
        matches!(state.update(messages.remove(0)), Some(Effect::OpenPullRequest { key, .. }) if Target::PullRequest(key.clone()) == fork.target)
    );
    let mut ui = cached(&state, Cache::new(), &mut renderer, 800.0, 1200.0);
    let hit = bounds(&ui, &name).center();
    assert!(
        events(
            &mut ui,
            &mut renderer,
            hit,
            &[mouse::Event::ButtonPressed(mouse::Button::Left)]
        )
        .is_empty()
    );
    let cache = ui.into_cache();
    fork.revision = 2;
    refresh(&mut state, snapshot(home(10), vec![fork.clone()], 3));
    let mut ui = cached(&state, cache, &mut renderer, 800.0, 1200.0);
    assert!(bounds(&ui, &name).contains(hit));
    let mut messages = events(
        &mut ui,
        &mut renderer,
        hit,
        &[mouse::Event::ButtonReleased(mouse::Button::Left)],
    );
    let _ = ui.into_cache();
    assert_eq!(messages.len(), 1);
    assert!(
        matches!(state.update(messages.remove(0)), Some(Effect::OpenPullRequest { home: actual, key, .. }) if actual == home(10) && Target::PullRequest(key.clone()) == fork.target)
    );
}

#[test]
fn narrow_home_keeps_wrapped_channel_identity_outside_scroll_and_partial_page_claims_honest() {
    #[derive(Clone, Copy)]
    struct Body {
        viewport: iced::Rectangle,
        content: iced::Rectangle,
        translation: iced::Vector,
    }
    fn body(ui: &mut Ui<'_>, renderer: &cosmic::Renderer, name: &str) -> (Body, iced::Rectangle) {
        use iced::advanced::widget::Operation;
        use iced_accessibility::{A11yId, IdEq};
        // Accessibility identifies the semantic control, never its geometry.
        let target = {
            let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
            nodes
                .root()
                .iter()
                .chain(nodes.children())
                .find(|node| node.node().label() == Some(name))
                .expect("the real PR button is named")
                .id()
                .clone()
        };
        struct Probe {
            scrolls: Vec<Body>,
            target: A11yId,
            buttons: Vec<iced::Rectangle>,
        }
        impl Operation for Probe {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                _: Option<&widget::Id>,
                viewport: iced::Rectangle,
                content: iced::Rectangle,
                translation: iced::Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                self.scrolls.push(Body {
                    viewport,
                    content,
                    translation,
                });
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: &mut dyn operation::Focusable,
            ) {
                if id.is_some_and(|id| IdEq::eq(&A11yId::from(id.clone()), &self.target)) {
                    self.buttons.push(bounds);
                }
            }
        }
        let mut probe = Probe {
            scrolls: Vec::new(),
            target,
            buttons: Vec::new(),
        };
        ui.operate(renderer, &mut probe);
        assert_eq!(
            probe.scrolls.len(),
            1,
            "the mounted leaf has one content scroller"
        );
        assert_eq!(
            probe.buttons.len(),
            1,
            "the operation finds the exact PR button"
        );
        (probe.scrolls[0], probe.buttons[0])
    }
    fn rendered(bounds: iced::Rectangle, translation: iced::Vector) -> iced::Rectangle {
        // Scrollable draws and hit-tests with this translation. Button's
        // operation exposes layout geometry, independent of AX bounds policy.
        iced::Rectangle {
            x: bounds.x - translation.x,
            y: bounds.y - translation.y,
            ..bounds
        }
    }
    fn assert_accessible_bounds(actual: iced::Rectangle, rendered: iced::Rectangle) {
        let differences = [
            actual.x - rendered.x,
            actual.y - rendered.y,
            actual.width - rendered.width,
            actual.height - rendered.height,
        ];
        assert!(
            differences
                .iter()
                .all(|difference| difference.abs() <= 0.01),
            "the named accessible control must match its painted and hit-tested bounds: {actual:?} vs {rendered:?}"
        );
    }
    let title =
        "Parser delivery and follow-up across the core library and its downstream integrations";
    let mut state = State::default();
    let read = state.open(home(10), title.into()).unwrap();
    let mut page = snapshot(
        home(10),
        vec![entry(1, Role::Review), entry(2, Role::Reference)],
        2,
    );
    page.next_current = Some(20);
    load(&mut state, read, page);
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = cached(&state, Cache::new(), &mut renderer, 300.0, 600.0);
    let title_bounds = bounds(&ui, title);
    let refresh_bounds = bounds(&ui, "Refresh channel work");
    let close_bounds = bounds(&ui, "Close channel work");
    for b in [title_bounds, refresh_bounds, close_bounds] {
        assert!(b.x >= 0.0 && b.x + b.width <= 300.01);
        assert!(b.y >= 0.0 && b.y + b.height <= 600.01);
    }
    let text = labels(&ui);
    assert!(
        text.iter()
            .any(|s| s == "No contributed or stewarded work on this page.")
    );
    assert!(!text.iter().any(|s| s == "No ongoing work is tracked here"));
    assert!(text.iter().any(|s| s == "Reviews and references"));
    assert!(text.iter().any(|s| s == "Next current page"));
    let (before, button_before) = body(&mut ui, &renderer, &open_name(1));
    assert!(before.viewport.height > 0.0 && before.viewport.height < 600.0);
    assert!(before.viewport.y >= title_bounds.y + title_bounds.height);
    assert!(before.viewport.x >= 0.0 && before.viewport.y >= 0.0);
    assert!(before.viewport.x + before.viewport.width <= 300.01);
    assert!(before.viewport.y + before.viewport.height <= 600.01);
    assert!(
        before.content.height > before.viewport.height,
        "the actual mounted content must overflow its bounded body"
    );
    let first_before = rendered(button_before, before.translation);
    assert_accessible_bounds(bounds(&ui, &open_name(1)), first_before);
    // Place the first real PR control near the body's center without assuming
    // a font-dependent content height or scrolling past that control.
    let distance = (first_before.center_y() - before.viewport.center_y())
        .max(1.0)
        .min(before.content.height - before.viewport.height);
    assert!(
        events(
            &mut ui,
            &mut renderer,
            before.viewport.center(),
            &[mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels {
                    x: 0.0,
                    y: -distance
                }
            }]
        )
        .is_empty()
    );
    assert_eq!(bounds(&ui, title), title_bounds);
    assert_eq!(bounds(&ui, "Refresh channel work"), refresh_bounds);
    assert_eq!(bounds(&ui, "Close channel work"), close_bounds);
    let (after, button_after) = body(&mut ui, &renderer, &open_name(1));
    assert_eq!(after.viewport, before.viewport);
    assert!(
        after.translation.y > before.translation.y,
        "the native wheel must change the real scroll offset"
    );
    let first_after = rendered(button_after, after.translation);
    assert_accessible_bounds(bounds(&ui, &open_name(1)), first_after);
    assert!(
        first_after.y < first_before.y,
        "the real content scrolls independently of the channel header"
    );
    assert!(first_after.y >= after.viewport.y);
    assert!(first_after.y + first_after.height <= after.viewport.y + after.viewport.height);
    let hit = first_after.center();
    assert!(after.viewport.contains(hit));
    let mut messages = click(&mut ui, &mut renderer, hit);
    assert!(matches!(messages.as_slice(), [Action::Open(owner)] if owner.target == target(1)));
    let _ = ui.into_cache();
    let Some(Effect::OpenPullRequest {
        home: actual, key, ..
    }) = state.update(messages.remove(0))
    else {
        panic!("the translated native hit must open its exact PR")
    };
    assert_eq!(actual, home(10));
    assert_eq!(Target::PullRequest(key), target(1));
}

struct PasteClipboard(String);
impl iced::advanced::Clipboard for PasteClipboard {
    fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
        Some(self.0.clone())
    }
    fn write(&mut self, _: iced::advanced::clipboard::Kind, value: String) {
        self.0 = value;
    }
}
fn repository_paste(ui: &mut Ui<'_>, renderer: &mut cosmic::Renderer, value: &str) -> Vec<Action> {
    let press = |name: &str, code| {
        let key = iced::keyboard::Key::Character(name.into());
        iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: iced::keyboard::key::Physical::Code(code),
            location: iced::keyboard::Location::Standard,
            modifiers: iced::keyboard::Modifiers::COMMAND,
            text: None,
            repeat: false,
        })
    };
    let mut messages = Vec::new();
    let mut clipboard = PasteClipboard(value.into());
    let _ = ui.update(
        &[
            press("a", iced::keyboard::key::Code::KeyA),
            press("v", iced::keyboard::key::Code::KeyV),
            iced::Event::Keyboard(iced::keyboard::Event::KeyReleased {
                key: iced::keyboard::Key::Character("v".into()),
                modified_key: iced::keyboard::Key::Character("v".into()),
                physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::KeyV),
                location: iced::keyboard::Location::Standard,
                modifiers: iced::keyboard::Modifiers::COMMAND,
            }),
        ],
        mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard,
        &mut messages,
    );
    messages
}

#[test]
fn repository_native_url_paste_fills_full_fork_port_and_u64_then_explicit_submit_keeps_role_and_cas()
 {
    let url = "https://Forge.Example:8443/Fork/Repository/pull/18446744073709551615/";
    let expected = Target::PullRequest(
        PullRequestKey::new(
            GitHubRepoKey::new("forge.example", Some(8443), "fork", "repository").unwrap(),
            u64::MAX,
        )
        .unwrap(),
    );
    let mut existing = entry(1, Role::Contribute);
    existing.target = expected.clone();
    existing.revision = 7;
    let mut state = State::default();
    let read = state.open(home(10), "Delivery".into()).unwrap();
    load(&mut state, read, snapshot(home(10), vec![existing], 9));
    state.update(Action::Attach(state.owner().unwrap()));
    let owner = state.view.as_ref().unwrap().form.as_ref().unwrap().owner;
    state.update(Action::Role(owner, 2));
    state.update(Action::Field(owner, Field::Number, "17".into()));
    state.update(Action::Field(owner, Field::Repository, "owner/repo".into()));
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = cached(&state, Cache::new(), &mut renderer, 800.0, 1800.0);
    let old_submit_point = bounds(&ui, "Track PR").center();
    let old_submit = click(&mut ui, &mut renderer, old_submit_point).remove(0);
    ui.operate(
        &renderer,
        &mut operation::focusable::focus::<()>(form_input_id(owner, Field::Repository)),
    );
    let mut messages = repository_paste(&mut ui, &mut renderer, url);
    let cache = ui.into_cache();
    assert!(
        matches!(messages.as_slice(), [Action::Field(actual, Field::Repository, value)] if *actual == owner && value == url)
    );
    assert!(state.update(messages.remove(0)).is_none());
    assert!(
        state.pending.is_empty(),
        "paste does not submit or declare work"
    );
    let form = state.view.as_ref().unwrap().form.as_ref().unwrap();
    assert_eq!(form.owner, owner);
    assert_eq!(form.host, "forge.example:8443");
    assert_eq!(form.repository, url, "the edited URL remains intact");
    assert_eq!(form.number, u64::MAX.to_string());
    assert_eq!(form.role, Role::Review);
    assert_eq!(form.expected_revision, 0);
    assert!(form.dirty());
    assert_eq!(form.target().unwrap(), expected);
    assert!(
        state.update(old_submit).is_none(),
        "the old target submit cannot apply the pasted identity"
    );
    assert!(state.pending.is_empty());
    let mut ui = cached(&state, cache, &mut renderer, 800.0, 1800.0);
    let point = bounds(&ui, "Track PR").center();
    let mut messages = click(&mut ui, &mut renderer, point);
    let _ = ui.into_cache();
    let Effect::Mutate { mutation, .. } = state.update(messages.remove(0)).unwrap() else {
        panic!()
    };
    assert_eq!(mutation.home, home(10));
    assert_eq!(mutation.expected_revision, 7);
    assert_eq!(
        mutation.change,
        Change::Claim {
            target: expected.clone(),
            role: Role::Review
        }
    );
    assert!(
        state
            .update(Action::Field(
                owner,
                Field::Repository,
                "https://github.com/other/repo/pull/2".into()
            ))
            .is_none()
    );
    assert_eq!(
        state
            .view
            .as_ref()
            .unwrap()
            .form
            .as_ref()
            .unwrap()
            .target()
            .unwrap(),
        expected
    );
    assert_eq!(state.pending[0].mutation, mutation);
}

#[test]
fn url_paste_invalid_inputs_do_not_partially_retarget_and_old_form_or_fixed_target_cannot_accept_it()
 {
    let url = "https://github.com/fork/repo/pull/88";
    let mut state = ready(home(10), vec![entry(1, Role::Contribute)]);
    let old = filled_form(&mut state);
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = cached(&state, Cache::new(), &mut renderer, 800.0, 1800.0);
    ui.operate(
        &renderer,
        &mut operation::focusable::focus::<()>(form_input_id(old, Field::Repository)),
    );
    let old_paste = repository_paste(&mut ui, &mut renderer, url).remove(0);
    let cache = ui.into_cache();
    state.update(Action::Dismiss(old));
    let owner = state.owner().unwrap();
    let confirmation = state
        .view
        .as_ref()
        .unwrap()
        .confirmation
        .as_ref()
        .unwrap()
        .id;
    state.update(Action::Discard(owner, confirmation));
    let current = filled_form(&mut state);
    state.update(Action::Field(
        current,
        Field::Host,
        "current.example:9443".into(),
    ));
    state.update(Action::Field(current, Field::Number, "29".into()));
    state.update(Action::Role(current, 1));
    let mut ui = cached(&state, cache, &mut renderer, 800.0, 1800.0);
    assert!(
        repository_paste(&mut ui, &mut renderer, url).is_empty(),
        "replacement draft does not inherit old native input focus"
    );
    let _ = ui.into_cache();
    assert!(state.update(old_paste).is_none());
    for invalid in [
        "https://github.com/fork/repo/pull/",
        "http://github.com/fork/repo/pull/88",
        "https://example:secret@github.com/fork/repo/pull/88",
        "https://github.com/fork/repo/pull/88?query",
        "https://github.com/fork/repo/pull/88#fragment",
        " https://github.com/fork/repo/pull/88 ",
    ] {
        let mut ui = cached(&state, Cache::new(), &mut renderer, 800.0, 1800.0);
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(form_input_id(current, Field::Repository)),
        );
        let mut messages = repository_paste(&mut ui, &mut renderer, invalid);
        let _ = ui.into_cache();
        assert_eq!(messages.len(), 1);
        assert!(state.update(messages.remove(0)).is_none());
        let form = state.view.as_ref().unwrap().form.as_ref().unwrap();
        assert_eq!(form.repository, invalid);
        assert_eq!(form.host, "current.example:9443");
        assert_eq!(form.number, "29");
        assert_eq!(form.role, Role::Steward);
        assert_eq!(form.owner, current);
        assert!(form.target().is_err());
        assert!(state.pending.is_empty());
    }
    state.update(Action::Dismiss(current));
    let confirmation = state
        .view
        .as_ref()
        .unwrap()
        .confirmation
        .as_ref()
        .unwrap()
        .id;
    state.update(Action::Discard(owner, confirmation));
    let row = row(&state, 1);
    state.update(Action::Correct(row.clone()));
    let form = state.view.as_ref().unwrap().form.as_ref().unwrap();
    let fixed_owner = form.owner;
    assert_eq!(form.fixed, Some(target(1)));
    state.update(Action::Field(fixed_owner, Field::Repository, url.into()));
    let form = state.view.as_ref().unwrap().form.as_ref().unwrap();
    assert_eq!(form.fixed, Some(target(1)));
    assert_eq!(form.expected_revision, row.revision);
    assert!(form.host.is_empty() && form.repository.is_empty() && form.number.is_empty());
    assert!(state.pending.is_empty());
}

fn role_enter(ui: &mut Ui<'_>, renderer: &mut cosmic::Renderer) -> Vec<Action> {
    let key = iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter);
    let mut messages = Vec::new();
    let _ = ui.update(
        &[iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::Enter),
            location: iced::keyboard::Location::Standard,
            modifiers: iced::keyboard::Modifiers::default(),
            text: None,
            repeat: false,
        })],
        mouse::Cursor::Unavailable,
        renderer,
        &mut iced::advanced::clipboard::Null,
        &mut messages,
    );
    draw(ui, renderer);
    messages
}

fn assert_role_choices(ui: &Ui<'_>, width: f32, selected: &str) {
    let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let mut rectangles = Vec::new();
    for name in ROLES {
        let choices = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node.node().label() == Some(name)
            })
            .collect::<Vec<_>>();
        assert_eq!(choices.len(), 1, "one actual role button for {name}");
        let description = choices[0]
            .node()
            .description()
            .expect("role meaning and selection announced");
        assert_eq!(description.starts_with("Selected role. "), name == selected);
        assert!(description.contains(
            ROLE_VALUES[ROLES.iter().position(|label| *label == name).unwrap()].description()
        ));
        let b = bounds(ui, name);
        assert!(b.x.is_finite() && b.y.is_finite() && b.width.is_finite() && b.height.is_finite());
        assert!(
            b.x >= 0.0 && b.x + b.width <= width && b.y >= 0.0 && b.y + b.height <= 1600.0,
            "role choice remains visible at {width}: {name}: {b:?}"
        );
        for previous in &rectangles {
            assert!(!b.intersects(previous), "role choices overlap at {width}");
        }
        rectangles.push(b);
    }
}

#[test]
fn visible_role_choices_pointer_and_retained_keyboard_select_reference_for_the_actual_submit() {
    for width in [300.0, 520.0, 900.0] {
        for keyboard in [false, true] {
            let mut state = ready(home(10), vec![]);
            let owner = filled_form(&mut state);
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let mut ui = cached(&state, Cache::new(), &mut renderer, width, 1600.0);
            assert_role_choices(&ui, width, "Contributing");
            let actions = if keyboard {
                ui.operate(
                    &renderer,
                    &mut operation::focusable::focus::<()>(role_id(owner, Role::Reference)),
                );
                let cache = ui.into_cache();
                // A normal unchanged cached frame preserves this native focus.
                ui = cached(&state, cache, &mut renderer, width, 1600.0);
                role_enter(&mut ui, &mut renderer)
            } else {
                let point = bounds(&ui, "Reference").center();
                click(&mut ui, &mut renderer, point)
            };
            let cache = ui.into_cache();
            assert!(
                matches!(actions.as_slice(), [Action::Role(actual, 3)] if *actual == owner),
                "width={width}, keyboard={keyboard}: {actions:?}"
            );
            for action in actions {
                assert!(state.update(action).is_none());
            }
            let mut ui = cached(&state, cache, &mut renderer, width, 1600.0);
            assert_role_choices(&ui, width, "Reference");
            let point = bounds(&ui, "Track PR").center();
            let submit = click(&mut ui, &mut renderer, point);
            let _ = ui.into_cache();
            assert!(
                matches!(submit.as_slice(), [Action::Submit { owner: actual, role: Role::Reference, target: actual_target, revision: 0 }]
                if *actual == owner && *actual_target == target(7)),
                "{submit:?}"
            );
            let effect = state.update(submit.into_iter().next().unwrap()).unwrap();
            assert!(matches!(effect, Effect::Mutate { mutation, .. }
                if mutation.home == home(10) && mutation.expected_revision == 0
                && mutation.change == Change::Claim { target: target(7), role: Role::Reference }));
        }
    }
}

#[test]
fn role_choice_queued_callback_and_native_hold_cannot_move_to_a_replaced_form() {
    for width in [300.0, 900.0] {
        let mut state = ready(home(10), vec![]);
        let old = filled_form(&mut state);
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut ui = cached(&state, Cache::new(), &mut renderer, width, 1600.0);
        let point = bounds(&ui, "Reference").center();
        let queued = click(&mut ui, &mut renderer, point);
        assert!(matches!(queued.as_slice(), [Action::Role(owner, 3)] if *owner == old));
        assert!(
            events(
                &mut ui,
                &mut renderer,
                point,
                &[mouse::Event::ButtonPressed(mouse::Button::Left)]
            )
            .is_empty()
        );
        let cache = ui.into_cache();
        state.update(Action::Dismiss(old));
        let confirmation = state
            .view
            .as_ref()
            .unwrap()
            .confirmation
            .as_ref()
            .unwrap()
            .id;
        state.update(Action::Discard(state.owner().unwrap(), confirmation));
        let current = filled_form(&mut state);
        assert_ne!(old, current);
        for action in queued {
            assert!(state.update(action).is_none());
        }
        let mut ui = cached(&state, cache, &mut renderer, width, 1600.0);
        assert_role_choices(&ui, width, "Contributing");
        assert!(bounds(&ui, "Reference").contains(point));
        assert!(
            events(
                &mut ui,
                &mut renderer,
                point,
                &[mouse::Event::ButtonReleased(mouse::Button::Left)]
            )
            .is_empty(),
            "replacement cannot borrow the old native role press"
        );
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(role_id(old, Role::Reference)),
        );
        assert!(
            role_enter(&mut ui, &mut renderer).is_empty(),
            "retired role ID cannot focus the new form"
        );
        let fresh = click(&mut ui, &mut renderer, point);
        let cache = ui.into_cache();
        assert!(matches!(fresh.as_slice(), [Action::Role(owner, 3)] if *owner == current));
        for action in fresh {
            assert!(state.update(action).is_none());
        }
        let ui = cached(&state, cache, &mut renderer, width, 1600.0);
        assert_role_choices(&ui, width, "Reference");
        let _ = ui.into_cache();
    }
}

#[test]
fn explicit_host_and_number_edits_detach_raw_url_and_submit_only_the_overridden_identity() {
    const URL: &str = "https://Forge.Example:8443/Fork/Repository/pull/18446744073709551615/";
    for (field, replacement, host, port, number) in [
        (
            Field::Host,
            "another.example:9443",
            "another.example",
            Some(9443),
            u64::MAX,
        ),
        (Field::Number, "1568", "forge.example", Some(8443), 1568),
    ] {
        let mut state = ready(home(10), vec![]);
        state.update(Action::Attach(state.owner().unwrap()));
        let owner = state.view.as_ref().unwrap().form.as_ref().unwrap().owner;
        state.update(Action::Field(owner, Field::Repository, URL.into()));
        state.update(Action::Role(owner, 3));
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut ui = cached(&state, Cache::new(), &mut renderer, 800.0, 1800.0);
        let point = bounds(&ui, "Track PR").center();
        let old_submit = click(&mut ui, &mut renderer, point).remove(0);
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(form_input_id(owner, field)),
        );
        let messages = repository_paste(&mut ui, &mut renderer, replacement);
        let cache = ui.into_cache();
        assert!(
            matches!(messages.as_slice(), [Action::Field(actual, kind, value)] if *actual == owner && std::mem::discriminant(kind) == std::mem::discriminant(&field) && value == replacement)
        );
        assert!(state.update(messages.into_iter().next().unwrap()).is_none());
        let form = state.view.as_ref().unwrap().form.as_ref().unwrap();
        assert_eq!(
            form.repository, "fork/repository",
            "an explicit split edit detaches the raw URL"
        );
        assert_eq!(form.role, Role::Reference);
        assert!(
            state.update(old_submit).is_none(),
            "a captured pre-override Submit cannot claim the old URL"
        );
        assert!(state.pending.is_empty());
        let mut ui = cached(&state, cache, &mut renderer, 800.0, 1800.0);
        let point = bounds(&ui, "Track PR").center();
        let messages = click(&mut ui, &mut renderer, point);
        let _ = ui.into_cache();
        let expected = Target::PullRequest(
            PullRequestKey::new(
                GitHubRepoKey::new(host, port, "fork", "repository").unwrap(),
                number,
            )
            .unwrap(),
        );
        assert!(
            matches!(messages.as_slice(), [Action::Submit { owner: actual, target, role: Role::Reference, revision: 0 }] if *actual == owner && target == &expected)
        );
        let Some(Effect::Mutate { mutation, .. }) =
            state.update(messages.into_iter().next().unwrap())
        else {
            panic!("the explicit current split identity must submit");
        };
        assert_eq!(mutation.home, home(10));
        assert_eq!(mutation.expected_revision, 0);
        assert_eq!(
            mutation.change,
            Change::Claim {
                target: expected,
                role: Role::Reference
            }
        );
    }
}

#[test]
fn incomplete_url_edit_disables_submit_and_refuses_a_previously_captured_complete_target() {
    const URL: &str = "https://Forge.Example:8443/Fork/Repository/pull/1568";
    let mut state = ready(home(10), vec![]);
    let owner = filled_form(&mut state);
    state.update(Action::Field(owner, Field::Repository, URL.into()));
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = cached(&state, Cache::new(), &mut renderer, 800.0, 1800.0);
    let point = bounds(&ui, "Track PR").center();
    let old_submit = click(&mut ui, &mut renderer, point).remove(0);
    ui.operate(
        &renderer,
        &mut operation::focusable::focus::<()>(form_input_id(owner, Field::Repository)),
    );
    let incomplete = URL.trim_end_matches("1568");
    let messages = repository_paste(&mut ui, &mut renderer, incomplete);
    let cache = ui.into_cache();
    assert!(
        matches!(messages.as_slice(), [Action::Field(actual, Field::Repository, value)] if *actual == owner && value == incomplete)
    );
    state.update(messages.into_iter().next().unwrap());
    assert!(state.update(old_submit).is_none());
    assert!(state.pending.is_empty());
    let form = state.view.as_ref().unwrap().form.as_ref().unwrap();
    assert_eq!(form.repository, incomplete);
    assert_eq!(form.host, "forge.example:8443");
    assert_eq!(
        form.number, "1568",
        "last derived fields are not permission to submit an incomplete URL"
    );
    assert!(form.target().is_err());
    let mut ui = cached(&state, cache, &mut renderer, 800.0, 1800.0);
    let point = bounds(&ui, "Track PR").center();
    assert!(click(&mut ui, &mut renderer, point).is_empty());
    ui.operate(
        &renderer,
        &mut operation::focusable::focus::<()>(form_input_id(owner, Field::Repository)),
    );
    let messages = repository_paste(&mut ui, &mut renderer, URL);
    let cache = ui.into_cache();
    state.update(messages.into_iter().next().unwrap());
    let mut ui = cached(&state, cache, &mut renderer, 800.0, 1800.0);
    let point = bounds(&ui, "Track PR").center();
    let messages = click(&mut ui, &mut renderer, point);
    let _ = ui.into_cache();
    let expected = Target::PullRequest(PullRequestKey::from_url(URL).unwrap())
        .canonicalized()
        .unwrap();
    assert!(
        matches!(messages.as_slice(), [Action::Submit { owner: actual, target, role: Role::Contribute, revision: 0 }] if *actual == owner && target == &expected)
    );
}

fn local_draft(state: &mut State, directory: &str) -> FormOwner {
    state.update(Action::AttachLocal(state.owner().unwrap()));
    let owner = state.view.as_ref().unwrap().form.as_ref().unwrap().owner;
    state.update(Action::LocalPath(owner, directory.into()));
    owner
}
fn inspect_local(state: &mut State) -> Ticket {
    let form = state.view.as_ref().unwrap().form.as_ref().unwrap();
    let local = form.local.as_ref().unwrap();
    let action = Action::InspectLocal {
        owner: form.owner,
        path_generation: local.path_generation,
        directory: local.directory.clone(),
        role: form.role,
    };
    let Effect::PrepareLocal {
        ticket,
        home: original,
        directory,
    } = state.update(action).unwrap()
    else {
        panic!()
    };
    assert_eq!(Some(original), state.home());
    assert_eq!(directory, PathBuf::from("/repo/local"));
    ticket
}
fn selected(home: Home, revision: u64) -> LocalSelection {
    LocalSelection {
        selection: Uuid::new_v4(),
        home,
        expected_revision: revision,
        context: crate::channel_work::CheckoutContext {
            id: Uuid::from_u128(800),
            registry: home.registry,
            repository: "/repo".into(),
            path: "/repo/local".into(),
            branch: Some("topic".into()),
            head: Some("a".repeat(40)),
            generation: 1,
            dirty: Some(true),
        },
    }
}
fn local_submit(state: &State) -> Action {
    let form = state.view.as_ref().unwrap().form.as_ref().unwrap();
    let selection = form.local.as_ref().unwrap().selected.as_ref().unwrap();
    Action::SubmitLocal {
        owner: form.owner,
        selection: selection.selection,
        target: selection.context.id,
        role: form.role,
        revision: selection.expected_revision,
    }
}

#[test]
fn local_native_input_inspects_before_explicit_tracking_and_uses_selected_point_cas() {
    let mut state = ready(home(10), vec![]);
    let owner = local_draft(&mut state, "");
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = cached(&state, Cache::new(), &mut renderer, 500.0, 1600.0);
    ui.operate(
        &renderer,
        &mut operation::focusable::focus::<()>(local_path_id(owner)),
    );
    let key = iced::keyboard::Key::Character("/".into());
    let input = iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::Slash),
        location: iced::keyboard::Location::Standard,
        modifiers: iced::keyboard::Modifiers::default(),
        text: Some("/repo/local".into()),
        repeat: false,
    });
    let mut messages = Vec::new();
    let _ = ui.update(
        &[input],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut iced::advanced::clipboard::Null,
        &mut messages,
    );
    let cache = ui.into_cache();
    assert!(
        matches!(messages.as_slice(), [Action::LocalPath(actual, value)] if *actual == owner && value == "/repo/local")
    );
    state.update(messages.remove(0));
    let mut ui = cached(&state, cache, &mut renderer, 500.0, 1600.0);
    let point = bounds(&ui, "Inspect checkout").center();
    let mut messages = click(&mut ui, &mut renderer, point);
    let _ = ui.into_cache();
    let Effect::PrepareLocal {
        ticket,
        home: original,
        directory,
    } = state.update(messages.remove(0)).unwrap()
    else {
        panic!()
    };
    assert_eq!(original, home(10));
    assert_eq!(directory, PathBuf::from("/repo/local"));
    assert!(state.pending.is_empty(), "inspection sends no mutation");
    let selection = selected(home(10), 7);
    assert!(selection.validate().is_ok());
    state.update(Action::PreparedLocal {
        ticket,
        result: Ok(selection.clone()),
    });
    let mut ui = cached(&state, Cache::new(), &mut renderer, 500.0, 1800.0);
    let point = bounds(&ui, "Track selected local checkout").center();
    let mut messages = click(&mut ui, &mut renderer, point);
    let _ = ui.into_cache();
    let Effect::Mutate {
        mutation,
        selection: actual,
        ..
    } = state.update(messages.remove(0)).unwrap()
    else {
        panic!()
    };
    assert_eq!(actual, Some(selection.selection));
    assert_eq!(
        mutation.expected_revision, 7,
        "an empty display page must not replace exact point CAS with zero"
    );
    assert_eq!(
        mutation.change,
        Change::Claim {
            target: Target::Local(selection.context.id),
            role: Role::Contribute
        }
    );
}

#[test]
fn local_cached_track_press_is_owned_by_selection_role_and_draft_not_poll_metadata() {
    for scenario in 0..4 {
        let mut state = ready(home(10), vec![]);
        let owner = local_draft(&mut state, "/repo/local");
        let ticket = inspect_local(&mut state);
        let original = selected(home(10), 7);
        state.update(Action::PreparedLocal {
            ticket,
            result: Ok(original.clone()),
        });
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut ui = cached(&state, Cache::new(), &mut renderer, 1200.0, 1800.0);
        let point = bounds(&ui, "Track selected local checkout").center();
        let old_action = click(&mut ui, &mut renderer, point).remove(0);
        assert!(
            events(
                &mut ui,
                &mut renderer,
                point,
                &[mouse::Event::ButtonPressed(mouse::Button::Left)]
            )
            .is_empty()
        );
        let cache = ui.into_cache();
        match scenario {
            0 => {
                let ticket = inspect_local(&mut state);
                state.update(Action::PreparedLocal {
                    ticket,
                    result: Ok(selected(home(10), 7)),
                });
            }
            1 => {
                state.update(Action::Role(owner, 2));
            }
            2 => {
                state.update(Action::Dismiss(owner));
                let view = state.owner().unwrap();
                let confirmation = state
                    .view
                    .as_ref()
                    .unwrap()
                    .confirmation
                    .as_ref()
                    .unwrap()
                    .id;
                state.update(Action::Discard(view, confirmation));
                local_draft(&mut state, "/repo/local");
                let ticket = inspect_local(&mut state);
                state.update(Action::PreparedLocal {
                    ticket,
                    result: Ok(selected(home(10), 7)),
                });
            }
            _ => {
                refresh(&mut state, snapshot(home(10), Vec::new(), 50));
            }
        }
        if scenario < 3 {
            assert!(
                state.update(old_action).is_none(),
                "queued stale selected action must not mutate"
            );
        }
        let mut ui = cached(&state, cache, &mut renderer, 1200.0, 1800.0);
        let current = bounds(&ui, "Track selected local checkout");
        assert!(
            current.contains(point),
            "replacement must occupy the held hit area"
        );
        let released = events(
            &mut ui,
            &mut renderer,
            point,
            &[mouse::Event::ButtonReleased(mouse::Button::Left)],
        );
        if scenario < 3 {
            assert!(
                released.is_empty(),
                "old held Track must not become a different selected operation"
            );
        } else {
            assert!(
                matches!(released.as_slice(), [Action::SubmitLocal { selection, revision: 7, .. }] if *selection == original.selection),
                "unrelated polling preserves valid held intent"
            );
        }
        let mut fresh = click(&mut ui, &mut renderer, current.center());
        let _ = ui.into_cache();
        let Effect::Mutate {
            selection: actual,
            mutation,
            ..
        } = state.update(fresh.remove(0)).unwrap()
        else {
            panic!()
        };
        let expected = state
            .view
            .as_ref()
            .unwrap()
            .form
            .as_ref()
            .unwrap()
            .local
            .as_ref()
            .unwrap()
            .selected
            .as_ref()
            .unwrap();
        assert_eq!(actual, Some(expected.selection));
        assert_eq!(mutation.expected_revision, 7);
        assert_eq!(
            mutation.change,
            Change::Claim {
                target: Target::Local(expected.context.id),
                role: if scenario == 1 {
                    Role::Review
                } else {
                    Role::Contribute
                }
            }
        );
    }
}

#[test]
fn local_inspection_replies_require_current_home_draft_path_generation_and_ticket() {
    let mut state = ready(home(10), vec![]);
    let owner = local_draft(&mut state, "/repo/local");
    let ticket = inspect_local(&mut state);
    state.update(Action::LocalPath(owner, "/repo/other".into()));
    state.update(Action::LocalPath(owner, "/repo/local".into()));
    state.update(Action::PreparedLocal {
        ticket,
        result: Ok(selected(home(10), 0)),
    });
    let local = state
        .view
        .as_ref()
        .unwrap()
        .form
        .as_ref()
        .unwrap()
        .local
        .as_ref()
        .unwrap();
    assert!(
        local.selected.is_none() && local.inspecting.is_none(),
        "A→B→A must retire old inspection, without wedging new Inspect"
    );
    for invalid in 0..4 {
        let ticket = inspect_local(&mut state);
        let mut selection = selected(home(10), 0);
        match invalid {
            0 => selection.home = home(11),
            1 => selection.context.registry = Uuid::new_v4(),
            2 => selection.context.id = Uuid::nil(),
            _ => selection.selection = Uuid::nil(),
        }
        state.update(Action::PreparedLocal {
            ticket,
            result: Ok(selection),
        });
        assert!(
            state
                .view
                .as_ref()
                .unwrap()
                .form
                .as_ref()
                .unwrap()
                .local
                .as_ref()
                .unwrap()
                .selected
                .is_none()
        );
    }
    let old_ticket = inspect_local(&mut state);
    state.update(Action::Dismiss(owner));
    let confirmation = state
        .view
        .as_ref()
        .unwrap()
        .confirmation
        .as_ref()
        .unwrap()
        .id;
    state.update(Action::Discard(state.owner().unwrap(), confirmation));
    let current_owner = local_draft(&mut state, "/repo/local");
    assert_ne!(owner, current_owner);
    assert!(
        state
            .update(Action::InspectLocal {
                owner: current_owner,
                path_generation: state
                    .view
                    .as_ref()
                    .unwrap()
                    .form
                    .as_ref()
                    .unwrap()
                    .local
                    .as_ref()
                    .unwrap()
                    .path_generation,
                directory: "/repo/local".into(),
                role: Role::Contribute,
            })
            .is_none(),
        "dismissal cannot create another outstanding client read"
    );
    state.update(Action::PreparedLocal {
        ticket: old_ticket,
        result: Ok(selected(home(10), 0)),
    });
    let current_ticket = inspect_local(&mut state);
    state.update(Action::PreparedLocal {
        ticket: old_ticket,
        result: Ok(selected(home(10), 0)),
    });
    assert_eq!(
        state
            .view
            .as_ref()
            .unwrap()
            .form
            .as_ref()
            .unwrap()
            .local
            .as_ref()
            .unwrap()
            .inspecting
            .as_ref()
            .unwrap()
            .ticket,
        current_ticket
    );
    state.update(Action::PreparedLocal {
        ticket: current_ticket,
        result: Ok(selected(home(10), 0)),
    });
    assert!(
        state
            .view
            .as_ref()
            .unwrap()
            .form
            .as_ref()
            .unwrap()
            .local
            .as_ref()
            .unwrap()
            .selected
            .is_some(),
        "current draft response is the positive control"
    );
    let mut wrong = local_submit(&state);
    if let Action::SubmitLocal { revision, .. } = &mut wrong {
        *revision = 1;
    }
    assert!(
        state.update(wrong).is_none(),
        "loaded-page or fabricated revision must not replace selected point CAS"
    );
    let mut wrong = local_submit(&state);
    if let Action::SubmitLocal { target, .. } = &mut wrong {
        *target = Uuid::new_v4();
    }
    assert!(
        state.update(wrong).is_none(),
        "selected identity cannot retarget to another arbitrary local UUID"
    );
    let ticket = inspect_local(&mut state);
    state.update(Action::Close(state.owner().unwrap()));
    let confirmation = state
        .view
        .as_ref()
        .unwrap()
        .confirmation
        .as_ref()
        .unwrap()
        .id;
    state.update(Action::Discard(state.owner().unwrap(), confirmation));
    let read = state.open(home(11), "Other".into()).unwrap();
    load(&mut state, read, snapshot(home(11), Vec::new(), 1));
    local_draft(&mut state, "/repo/local");
    state.update(Action::PreparedLocal {
        ticket,
        result: Ok(selected(home(10), 0)),
    });
    assert_eq!(state.home(), Some(home(11)));
    assert!(
        state
            .view
            .as_ref()
            .unwrap()
            .form
            .as_ref()
            .unwrap()
            .local
            .as_ref()
            .unwrap()
            .selected
            .is_none()
    );
    assert!(state.pending.is_empty());
}

#[test]
fn selected_local_unknown_retry_keeps_original_selection_and_receipt_settles_without_projection() {
    let mut state = ready(home(10), Vec::new());
    let owner = local_draft(&mut state, "/repo/local");
    let inspection = inspect_local(&mut state);
    let original = selected(home(10), 7);
    state.update(Action::PreparedLocal {
        ticket: inspection,
        result: Ok(original.clone()),
    });
    let Effect::Mutate {
        ticket,
        mutation,
        selection,
    } = state.update(local_submit(&state)).unwrap()
    else {
        panic!()
    };
    assert_eq!(selection, Some(original.selection));
    state.update(Action::Applied {
        ticket,
        result: Err(Failure::Unavailable),
    });
    state.update(Action::LocalPath(owner, "/repo/other".into()));
    state.escape();
    let read = state.open(home(11), "Other".into()).unwrap();
    load(&mut state, read, snapshot(home(11), Vec::new(), 1));
    assert_eq!(
        state.pending_for(home(10)).unwrap().selection,
        Some(original.selection)
    );
    let read = state.open(home(10), "Original".into()).unwrap();
    load(&mut state, read, snapshot(home(10), Vec::new(), 8));
    let owner = state.owner().unwrap();
    let Effect::Mutate {
        ticket,
        mutation: retried,
        selection,
    } = state
        .update(Action::Retry(owner, mutation.operation))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(selection, Some(original.selection));
    assert_eq!(
        retried, mutation,
        "retry must remain ApplySelection of exact original intent"
    );
    state.update(Action::Applied {
        ticket,
        result: Err(Failure::Refused("Selection expired".into())),
    });
    assert_eq!(
        state.pending_for(home(10)).unwrap().delivery,
        Delivery::Unknown
    );
    let Effect::Lookup {
        ticket, operation, ..
    } = state
        .update(Action::Check(owner, mutation.operation))
        .unwrap()
    else {
        panic!()
    };
    state.update(Action::LookedUp {
        ticket,
        operation,
        result: Ok(None),
    });
    assert!(state.pending_for(home(10)).is_some());
    let Effect::Lookup {
        ticket, operation, ..
    } = state
        .update(Action::Check(owner, mutation.operation))
        .unwrap()
    else {
        panic!()
    };
    let effect = state
        .update(Action::LookedUp {
            ticket,
            operation,
            result: Ok(Some(completed(&mutation, 8, Vec::new()).receipt)),
        })
        .unwrap();
    assert!(state.pending.is_empty());
    let Effect::Read { ticket, .. } = effect else {
        panic!()
    };
    state.update(Action::Loaded {
        ticket,
        result: Err(Failure::Unavailable),
    });
    assert!(state.pending.is_empty());
    assert_eq!(state.home(), Some(home(10)));
}

#[test]
fn local_draft_keep_discard_retains_path_and_defers_exact_destination() {
    let mut state = ready(home(10), Vec::new());
    let owner = local_draft(&mut state, "/repo/local");
    let ticket = inspect_local(&mut state);
    state.open(home(11), "Destination".into());
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = cached(&state, Cache::new(), &mut renderer, 400.0, 1600.0);
    let point = bounds(&ui, "Keep editing").center();
    let mut messages = click(&mut ui, &mut renderer, point);
    let _ = ui.into_cache();
    state.update(messages.remove(0));
    assert_eq!(
        state.view.as_ref().unwrap().form.as_ref().unwrap().owner,
        owner
    );
    assert_eq!(
        state
            .view
            .as_ref()
            .unwrap()
            .form
            .as_ref()
            .unwrap()
            .local
            .as_ref()
            .unwrap()
            .directory,
        "/repo/local"
    );
    state.open(home(11), "Destination".into());
    let mut ui = cached(&state, Cache::new(), &mut renderer, 400.0, 1600.0);
    let point = bounds(&ui, "Discard draft").center();
    let mut messages = click(&mut ui, &mut renderer, point);
    let _ = ui.into_cache();
    let Effect::Read {
        home: opened,
        ticket: read,
        ..
    } = state.update(messages.remove(0)).unwrap()
    else {
        panic!()
    };
    assert_eq!(opened, home(11));
    state.update(Action::PreparedLocal {
        ticket,
        result: Ok(selected(home(10), 0)),
    });
    state.update(Action::Loaded {
        ticket: read,
        result: Ok(snapshot(home(11), Vec::new(), 1)),
    });
    assert_eq!(state.home(), Some(home(11)));
    assert!(state.view.as_ref().unwrap().form.is_none());
}

#[test]
fn correcting_and_ending_a_known_local_row_remain_plain_registered_intent() {
    let mut local = entry(1, Role::Contribute);
    local.target = Target::Local(Uuid::from_u128(800));
    local.revision = 3;
    let mut state = ready(home(10), vec![]);
    refresh(&mut state, snapshot(home(10), vec![local], 3));
    let view = state.view.as_ref().unwrap();
    let current_entry = &view.snapshot.as_ref().unwrap().entries[0];
    let row = RowOwner {
        view: view.owner,
        id: current_entry.id,
        target: current_entry.target.clone(),
        revision: current_entry.revision,
        bucket: current_entry.bucket(),
    };
    state.update(Action::Correct(row.clone()));
    let form = state.view.as_ref().unwrap().form.as_ref().unwrap();
    assert!(form.local.is_none());
    let action = Action::Submit {
        owner: form.owner,
        target: row.target.clone(),
        role: Role::Contribute,
        revision: 3,
    };
    assert!(
        matches!(state.update(action), Some(Effect::Mutate { selection: None, mutation, .. }) if mutation.expected_revision == 3)
    );
    // A separate state proves End does not need a new arbitrary local selection.
    let mut state = ready(home(10), vec![]);
    let mut local = entry(1, Role::Contribute);
    local.target = row.target.clone();
    local.revision = 3;
    refresh(&mut state, snapshot(home(10), vec![local], 3));
    let mut current = row;
    current.view = state.owner().unwrap();
    assert!(
        matches!(state.update(Action::End(current)), Some(Effect::Mutate { selection: None, mutation, .. }) if mutation.change == Change::End { target: Target::Local(Uuid::from_u128(800)) })
    );
}

#[test]
fn checkout_change_observations_distinguish_unknown_without_blocking_explicit_local_intent() {
    for (dirty, preview, context_suffix) in [
        (
            Some(true),
            "Uncommitted changes were observed.",
            " · local changes observed",
        ),
        (
            Some(false),
            "No uncommitted changes were observed.",
            " · no local changes observed",
        ),
        (
            None,
            "Working-tree changes not inspected.",
            " · changes not inspected",
        ),
    ] {
        let mut state = ready(home(10), vec![]);
        local_draft(&mut state, "/repo/local");
        let ticket = inspect_local(&mut state);
        let mut selection = selected(home(10), 7);
        selection.context.dirty = dirty;
        assert!(selection.validate().is_ok());
        state.update(Action::PreparedLocal {
            ticket,
            result: Ok(selection.clone()),
        });
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut ui = cached(&state, Cache::new(), &mut renderer, 500.0, 2000.0);
        let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let labels = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| node.node().label())
            .collect::<Vec<_>>();
        assert!(labels.contains(&preview));
        if dirty.is_none() {
            assert!(!labels.contains(&"No uncommitted changes were observed."));
            assert!(!labels.contains(&"Uncommitted changes were observed."));
        }
        let point = bounds(&ui, "Track selected local checkout").center();
        let mut messages = click(&mut ui, &mut renderer, point);
        let _ = ui.into_cache();
        assert!(matches!(state.update(messages.remove(0)),
            Some(Effect::Mutate { selection: Some(actual), mutation, .. })
            if actual == selection.selection && mutation.expected_revision == 7
                && mutation.change == Change::Claim { target: Target::Local(selection.context.id), role: Role::Contribute }
        ));

        let mut local = entry(1, Role::Contribute);
        local.target = Target::Local(selection.context.id);
        local.contexts = vec![selection.context.clone()];
        let mut state = ready(home(10), vec![local.clone()]);
        let owner = RowOwner {
            view: state.owner().unwrap(),
            id: local.id,
            target: local.target.clone(),
            revision: local.revision,
            bucket: local.bucket(),
        };
        state.update(Action::Details(owner, true));
        let ui = cached(&state, Cache::new(), &mut renderer, 500.0, 2000.0);
        let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let labels = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| node.node().label())
            .collect::<Vec<_>>();
        let expected = format!("topic · /repo/local{context_suffix}");
        assert!(labels.contains(&expected.as_str()));
        if dirty.is_none() {
            assert!(
                !labels
                    .iter()
                    .any(|label| label.contains("no local changes observed"))
            );
        }
    }
}

#[test]
fn local_client_inspection_slot_survives_dismissal_home_changes_and_duplicate_replies() {
    let mut state = ready(home(10), Vec::new());
    local_draft(&mut state, "/repo/local");
    let first = inspect_local(&mut state);
    for n in 11..43 {
        let owner = state.owner().unwrap();
        state.update(Action::Close(owner));
        let confirmation = state
            .view
            .as_ref()
            .unwrap()
            .confirmation
            .as_ref()
            .unwrap()
            .id;
        state.update(Action::Discard(owner, confirmation));
        let read = state.open(home(n), "Other channel".into()).unwrap();
        load(&mut state, read, snapshot(home(n), Vec::new(), 1));
        let owner = local_draft(&mut state, "/repo/local");
        let path_generation = state
            .view
            .as_ref()
            .unwrap()
            .form
            .as_ref()
            .unwrap()
            .local
            .as_ref()
            .unwrap()
            .path_generation;
        assert!(
            state
                .update(Action::InspectLocal {
                    owner,
                    path_generation,
                    directory: "/repo/local".into(),
                    role: Role::Contribute,
                })
                .is_none()
        );
        assert_eq!(state.local_inspection, Some(first));
        assert!(state.pending.is_empty());
    }
    assert!(
        state
            .update(Action::PreparedLocal {
                ticket: first,
                result: Ok(selected(home(10), 0)),
            })
            .is_none()
    );
    assert!(state.local_inspection.is_none());
    assert!(
        state
            .view
            .as_ref()
            .unwrap()
            .form
            .as_ref()
            .unwrap()
            .local
            .as_ref()
            .unwrap()
            .selected
            .is_none()
    );
    let current = inspect_local(&mut state);
    assert!(
        state
            .update(Action::PreparedLocal {
                ticket: first,
                result: Err(Failure::Unavailable),
            })
            .is_none()
    );
    assert_eq!(
        state.local_inspection,
        Some(current),
        "old duplicate cannot release successor"
    );
    assert!(
        state
            .update(Action::PreparedLocal {
                ticket: current,
                result: Err(Failure::Unavailable),
            })
            .is_none(),
        "failure does not automatically start another client read"
    );
    assert!(state.local_inspection.is_none());
    assert!(state.pending.is_empty(), "inspections never submit a claim");
    let explicit = inspect_local(&mut state);
    assert_ne!(
        explicit, current,
        "only another explicit inspection starts after failure"
    );
}
