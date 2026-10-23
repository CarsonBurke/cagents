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
        Effect::Mutate { ticket, mutation } => (ticket, mutation),
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
    state.update(Action::LookedUp {
        ticket: lookup,
        operation,
        result: Ok(Some(reply)),
    });
    assert!(state.pending.is_empty());
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
    state.update(Action::LookedUp {
        ticket: lookup,
        operation,
        result: Ok(Some(fresh.clone())),
    });
    assert!(state.pending.is_empty());
    assert!(state.view.as_ref().unwrap().loading.is_none());
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
        assert!(labels.contains(&"Review and reference"));
        assert!(labels.contains(&"1 work relations · 1 review or reference relations"));
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
