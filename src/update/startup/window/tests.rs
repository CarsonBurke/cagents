use super::*;
use cosmic::{
    Application,
    iced::{
        Size,
        advanced::mouse,
        runtime::user_interface::{Cache, UserInterface},
    },
};

fn window(free: bool) -> Window {
    Window {
        core: Core::default(),
        request: Request::new(false, Some("window-2".into()), None, None, false).unwrap(),
        snapshot: Snapshot {
            installation_lock: if free {
                super::super::LockState::Free
            } else {
                super::super::LockState::Held
            },
            helper_lock: super::super::LockState::Free,
            job: None,
        },
        follow: Follow::default(),
        polling: false,
        working: false,
        details: false,
        notice: None,
        context_ready: true,
    }
}
fn rendered(
    window: &Window,
    width: f32,
    height: f32,
    click: Option<&str>,
) -> (Vec<String>, Vec<Message>) {
    use iced::advanced::{clipboard, renderer};
    let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut ui = UserInterface::build(
        window.view().map(cosmic::Action::App),
        Size::new(width, height),
        Cache::default(),
        &mut renderer,
    );
    let mut messages = Vec::new();
    ui.update(
        &[],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    ui.draw(
        &mut renderer,
        &cosmic::theme::active(),
        &renderer::Style::default(),
        mouse::Cursor::Unavailable,
    );
    let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let labels = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .filter_map(|node| node.node().label().map(str::to_owned))
        .collect();
    if let Some(label) = click {
        let buttons = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node.node().label() == Some(label)
                    && !node.node().is_disabled()
                    && !node.node().is_hidden()
            })
            .collect::<Vec<_>>();
        assert_eq!(buttons.len(), 1, "one actual enabled {label}");
        let node = buttons[0];
        let bounds = node.node().bounds().unwrap();
        assert!(bounds.x1 > bounds.x0 && bounds.y1 > bounds.y0);
        assert!(
            bounds.x0 >= 0.0
                && bounds.y0 >= 0.0
                && bounds.x1 <= width as f64
                && bounds.y1 <= height as f64
        );
        use iced_accessibility::accesskit::{Action, ActionRequest, NodeId, TreeId};
        let id = NodeId::from(node.id().clone());
        let (_, status) = ui.update(
            &[iced::Event::A11y(
                iced::advanced::widget::Id::from(u128::from(id.0) as u64),
                ActionRequest {
                    action: Action::Click,
                    target_tree: TreeId::ROOT,
                    target_node: id,
                    data: None,
                },
            )],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(status, [iced::event::Status::Captured]);
    }
    (
        labels,
        messages
            .into_iter()
            .map(|message| match message {
                cosmic::Action::App(message) => message,
                _ => panic!("unexpected startup effect"),
            })
            .collect(),
    )
}

#[test]
fn real_narrow_and_roomy_buttons_keep_details_progress_and_close_reachable() {
    for (width, height) in [(420.0, 340.0), (640.0, 440.0)] {
        let mut state = window(false);
        state.snapshot.helper_lock = super::super::LockState::Held;
        state.snapshot.job = Some(super::super::Job {
            id: "rendered-operation".into(),
            version: Some(format!("1.2.4+{}", "longtarget".repeat(10))),
            phase: crate::update::UpdatePhase::WaitingForIdle,
            waiting: 2,
            blocked: 0,
            daemon_timeout: false,
        });
        for action in ["Technical details", "Existing window", "Close"] {
            let (labels, messages) = rendered(&state, width, height, Some(action));
            assert_eq!(messages.len(), 1);
            assert!(
                labels
                    .iter()
                    .any(|label| label.contains("Waiting for conversations to finish"))
            );
        }
        let (_, mut messages) = rendered(&state, width, height, Some("Technical details"));
        drop(state.update(messages.pop().unwrap()));
        assert!(state.details);
        let (labels, _) = rendered(&state, width, height, None);
        assert!(
            labels
                .iter()
                .any(|label| label.contains("Installation: locked"))
        );
        assert!(
            labels
                .iter()
                .all(|label| !label.contains("os error") && !label.contains("status.json"))
        );
    }
}

#[test]
fn actual_open_callback_still_refuses_held_lock_and_allows_only_one_inflight_request() {
    let mut state = window(true);
    let (_, mut messages) = rendered(&state, 420.0, 340.0, Some("Open cagents"));
    let action = messages.pop().unwrap();
    state.snapshot.installation_lock = super::super::LockState::Held;
    drop(state.update(action));
    assert!(!state.working, "queued click cannot bypass changed lock");
    state.snapshot.installation_lock = super::super::LockState::Free;
    let (_, mut messages) = rendered(&state, 420.0, 340.0, Some("Open cagents"));
    drop(state.update(messages.pop().unwrap()));
    assert!(state.working && state.follow.auto_opened);
    drop(state.update(Message::Open));
    assert!(state.working);
    // First task deliberately unpolled: rendered callback/state proof, no launcher.
}

#[test]
fn actual_reopen_process_receives_exact_original_device_scoped_window_and_source_arguments() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("inert-ui");
    let output = root.path().join("arguments.json");
    std::fs::write(&script, b"#!/usr/bin/python3\nimport os,json,sys\nwith open(os.environ['FIXTURE_ARGUMENT_OUTPUT'],'x') as f: json.dump(sys.argv[1:],f)\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let request = Request::new(
        false,
        Some("device-0123456789abcdef".into()),
        Some("device-fedcba9876543210".into()),
        Some("build-host".into()),
        true,
    )
    .unwrap();
    let mut command = reopen_command(&script, &request);
    command.env("FIXTURE_ARGUMENT_OUTPUT", &output);
    let mut child = command.spawn().unwrap();
    assert!(child.wait().unwrap().success());
    let actual: Vec<String> = serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    assert_eq!(
        actual,
        [
            "--device",
            "build-host",
            "--device-window-scoped",
            "--window",
            "device-0123456789abcdef",
            "--from-window",
            "device-fedcba9876543210"
        ]
    );
    assert!(
        !actual
            .iter()
            .any(|arg| arg.contains("update-worker") || arg.contains("daemon"))
    );
}

#[test]
fn internal_request_is_bounded_to_window_context_and_rejects_unknown_capability_fields() {
    assert!(Request::new(true, Some("window-2".into()), None, None, false).is_err());
    assert!(Request::new(false, Some("../other".into()), None, None, false).is_err());
    assert!(serde_json::from_str::<Request>(r#"{"new_window":false,"slot":null,"source":null,"device":null,"scoped":false,"confirmation":"forged"}"#).is_err());
}

#[test]
fn lost_original_device_context_never_falls_back_to_a_local_open_or_activation() {
    let mut state = window(true);
    state.context_ready = false;
    drop(state.update(Message::Open));
    drop(state.update(Message::Focus));
    assert!(!state.working && !state.follow.auto_opened);
}

#[test]
fn diagnostic_details_are_historical_fixed_categories_without_private_records() {
    use crate::diagnostics::{Lifecycle, Observation, Persistence, Product, ProductReport, Report};
    let report = Report {
        schema_version: 1,
        cli_version: crate::VERSION,
        cli_protocol: crate::terminal_ipc::PROTOCOL_VERSION,
        path: "/PRIVATE_DIAGNOSTICS_PATH".into(),
        holders_readable: false,
        holders: Vec::new(),
        products: vec![ProductReport {
            product: Product::Ui,
            available: true,
            readable: true,
            records: Vec::new(),
            last_observed: Some(Observation {
                run: uuid::Uuid::new_v4(),
                pid: 123,
                at_unix_ms: 0,
                version: "PRIVATE_VERSION_RECORD".into(),
                protocol: 1,
                lifecycle: Lifecycle::StartupFailed,
                clean_shutdown: None,
                persistence: Persistence::Unknown,
                final_flush: None,
                skipped_events: 0,
            }),
        }],
    };
    let summary = diagnostic_summary(&report);
    assert!(
        summary.contains("historical, not a live health check")
            && summary.contains("cagents-ui: startup failure recorded")
    );
    assert!(
        !summary.contains("PRIVATE_DIAGNOSTICS_PATH")
            && !summary.contains("PRIVATE_VERSION_RECORD")
            && !summary.contains("123")
    );
}

fn observed_operation(id: &str, complete: bool) -> Snapshot {
    use super::super::{Job, LockState};
    Snapshot {
        installation_lock: if complete {
            LockState::Free
        } else {
            LockState::Held
        },
        helper_lock: if complete {
            LockState::Free
        } else {
            LockState::Held
        },
        job: Some(Job {
            id: id.into(),
            version: Some("1.2.4".into()),
            phase: if complete {
                crate::update::UpdatePhase::Complete
            } else {
                crate::update::UpdatePhase::Installing
            },
            waiting: 0,
            blocked: 0,
            daemon_timeout: false,
        }),
    }
}

#[test]
fn operation_change_observed_during_focus_or_diagnostics_still_requires_explicit_open() {
    for diagnostics in [false, true] {
        let mut state = window(false);
        drop(state.update(Message::Observed(observed_operation("original", false))));
        drop(state.update(if diagnostics {
            Message::Diagnostics
        } else {
            Message::Focus
        }));
        assert!(state.working);
        drop(state.update(Message::Observed(observed_operation("replacement", false))));
        drop(state.update(if diagnostics {
            Message::Diagnosed("Historical startup evidence".into())
        } else {
            Message::Focused(Ok(false))
        }));
        drop(state.update(Message::Observed(observed_operation("original", true))));
        assert!(
            !state.working && !state.follow.auto_opened,
            "busy actions must not hide an observed replacement operation"
        );
        // The permanent automatic-return fence retains deliberate recovery.
        drop(state.update(Message::Open));
        assert!(state.working && state.follow.auto_opened);
        // Tasks are deliberately unpolled: callback/state proof, no launcher.
    }
}

#[test]
fn first_leased_operation_observed_while_busy_returns_once_after_the_action_finishes() {
    for diagnostics in [false, true] {
        let mut state = window(false);
        drop(state.update(if diagnostics {
            Message::Diagnostics
        } else {
            Message::Focus
        }));
        drop(state.update(Message::Observed(observed_operation("original", false))));
        drop(state.update(Message::Observed(observed_operation("original", true))));
        assert!(state.working && !state.follow.auto_opened);
        drop(state.update(if diagnostics {
            Message::Diagnosed("Historical startup evidence".into())
        } else {
            Message::Focused(Ok(false))
        }));
        assert!(!state.working && !state.follow.auto_opened);
        drop(state.update(Message::Observed(observed_operation("original", true))));
        assert!(state.working && state.follow.auto_opened);
        drop(state.update(Message::Opened(Err("Fixture launch failure"))));
        drop(state.update(Message::Observed(observed_operation("original", true))));
        assert!(
            !state.working && state.follow.auto_opened,
            "a failed automatic launch must leave explicit retry, not a retry loop"
        );
        // Tasks are deliberately unpolled: callback/state proof, no launcher.
    }
}
