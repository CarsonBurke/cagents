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
        opening: false,
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
    if click == Some("Check diagnostics") {
        use iced::advanced::widget::{Operation, operation};
        #[derive(Default)]
        struct BodyViewport(Vec<iced::Rectangle>);
        impl Operation for BodyViewport {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                bounds: iced::Rectangle,
                _: iced::Rectangle,
                _: iced::Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                self.0.push(bounds);
            }
        }
        let mut body = BodyViewport::default();
        ui.operate(&renderer, &mut body);
        assert_eq!(body.0.len(), 1, "one actual startup body viewport");
        let point = body.0[0].center();
        // Diagnostics follows the details in the scrolling body. Reveal it
        // through ordinary input before querying its current accessible bounds.
        ui.update(
            &[
                iced::Event::Mouse(mouse::Event::CursorMoved { position: point }),
                iced::Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Pixels {
                        x: 0.0,
                        y: -100_000.0,
                    },
                }),
            ],
            mouse::Cursor::Available(point),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert!(messages.is_empty(), "scrolling must not invoke an action");
    }
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
            startup_delayed: false,
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
        let job = state.snapshot.job.as_mut().unwrap();
        job.phase = crate::update::UpdatePhase::Installing;
        job.waiting = 0;
        job.startup_delayed = true;
        for action in ["Check diagnostics", "Existing window", "Close"] {
            let (labels, messages) = rendered(&state, width, height, Some(action));
            assert_eq!(messages.len(), 1);
            assert!(
                labels
                    .iter()
                    .any(|label| label == "Waiting for saved sessions to restore")
            );
            assert!(
                labels
                    .iter()
                    .any(|label| label.contains("taking longer than expected"))
            );
            assert!(!labels.iter().any(|label| label == "Installing cagents"));
        }
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
            "device-fedcba9876543210",
            super::super::RECOVERY_ENTRY_ARGUMENT
        ]
    );
    assert!(
        !actual
            .iter()
            .any(|arg| arg.contains("update-worker") || arg.contains("daemon"))
    );
}

#[test]
fn failed_reopen_then_retry_keeps_exact_context_and_waits_for_real_readiness() {
    use std::{os::unix::fs::PermissionsExt, sync::mpsc, time::Instant};
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("inert-ui");
    std::fs::write(
        &script,
        r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
output = pathlib.Path(os.environ['FIXTURE_OUTPUT'])
output.write_text(json.dumps({'arguments': sys.argv[1:], 'handshake': os.environ.get('CAGENTS_UI_LAUNCH_HANDSHAKE'), 'owned': os.environ.get('CAGENTS_UI_FAILURE_SCREEN_OWNED')}))
if os.environ.get('FIXTURE_FAIL') == '1':
    print('error: fixture startup refused', flush=True)
    sys.exit(1)
release = pathlib.Path(os.environ['FIXTURE_RELEASE'])
deadline = time.monotonic() + 5
while not release.exists() and time.monotonic() < deadline:
    time.sleep(.01)
if not release.exists():
    sys.exit(71)
print('ready', flush=True)
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (index, request) in [
        Request::new(
            false,
            Some("device-0123456789abcdef".into()),
            Some("device-fedcba9876543210".into()),
            Some("build-host".into()),
            true,
        )
        .unwrap(),
        Request::new(true, None, Some("window-2".into()), None, false).unwrap(),
    ]
    .into_iter()
    .enumerate()
    {
        // This is the capability-free context carried by the detached error
        // screen, including the already-scoped bit that prevents double hashing.
        let restored: Request =
            serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        let mut expected = request.arguments();
        expected.push(super::super::RECOVERY_ENTRY_ARGUMENT.into());
        let failure = root.path().join(format!("failed-{index}.json"));
        let mut first = reopen_command(&script, &restored);
        first
            .env("FIXTURE_FAIL", "1")
            .env("FIXTURE_OUTPUT", &failure);
        assert!(
            wait_for_reopen(&mut first).is_err(),
            "spawn is not readiness"
        );
        let failed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&failure).unwrap()).unwrap();
        assert_eq!(failed["arguments"], serde_json::json!(expected));

        let output = root.path().join(format!("retry-{index}.json"));
        let release = root.path().join(format!("release-{index}"));
        let mut second = reopen_command(&script, &restored);
        second
            .env_remove("FIXTURE_FAIL")
            .env("FIXTURE_OUTPUT", &output)
            .env("FIXTURE_RELEASE", &release);
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender.send(wait_for_reopen(&mut second)).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while !output.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(output.exists(), "the exact retry child actually started");
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        std::fs::write(&release, b"fixture parent permits readiness").unwrap();
        receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
        let retried: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
        assert_eq!(retried["arguments"], serde_json::json!(expected));
        assert_eq!(retried["handshake"], "1");
        assert_eq!(retried["owned"], "1");
    }
}

#[test]
fn staged_replacement_keeps_recovery_presentation_and_original_scope_through_exec() {
    use std::os::unix::ffi::OsStringExt;

    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("staged-ui");
    std::fs::write(
        &script,
        r#"#!/usr/bin/python3
import json, os, pathlib, sys
expected = os.environ['FIXTURE_EXPECT_OWNED'] == '1'
assert (os.environ.get('CAGENTS_UI_FAILURE_SCREEN_OWNED') == '1') == expected
assert sys.argv[1:].count('--startup-recovery-parent') == int(expected)
assert os.environ.get('CAGENTS_UI_LAUNCH_HANDSHAKE') == '1'
assert os.environ.get('CAGENTS_UI_LAUNCH_RECOVERY_PROGRESS') == '1'
for key in [b'CAGENTS_AGENT_AUTHORITY', b'CAGENTS_AGENT_TOKEN', b'CAGENTS_SESSION_TOKEN']:
    assert os.environb[key].hex() == os.environ['FIXTURE_EXPECT_ORIGIN_HEX']
output = pathlib.Path(os.environ['FIXTURE_OUTPUT'])
if 'FIXTURE_REPLACED_PID' not in os.environ:
    os.environ['FIXTURE_REPLACED_PID'] = str(os.getpid())
    os.execv(sys.executable, [sys.executable, __file__, *sys.argv[1:]])
assert int(os.environ['FIXTURE_REPLACED_PID']) == os.getpid()
output.write_text(json.dumps({'arguments': sys.argv[1:], 'owned': expected,
    'same_process': True, 'origin_preserved': True}))
print('error: fixture staged startup refused', flush=True)
sys.exit(1)
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let request = Request::new(
        false,
        Some("device-0123456789abcdef".into()),
        Some("device-fedcba9876543210".into()),
        Some("build-host".into()),
        true,
    )
    .unwrap();
    let expected = request.arguments();
    for owned in [false, true] {
        for (index, (origin, hex)) in [
            (
                std::ffi::OsString::from("fixture-scope"),
                "666978747572652d73636f7065",
            ),
            (std::ffi::OsString::new(), ""),
            (std::ffi::OsString::from_vec(vec![0xff]), "ff"),
        ]
        .into_iter()
        .enumerate()
        {
            let output = root.path().join(format!("staged-{owned}-{index}.json"));
            // This is the exact staged replacement builder called by UI entry,
            // after its original presentation flag has been consumed. The
            // replacement keeps normal readiness and rejects startup below.
            let mut incoming = expected
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>();
            let mut forwarded = expected.clone();
            if owned {
                // The next staged exec receives the original argv, including
                // its private marker. It must forward exactly one copy.
                incoming.push(super::super::RECOVERY_ENTRY_ARGUMENT.into());
                incoming.push(super::super::RECOVERY_ENTRY_ARGUMENT.into());
                forwarded.push(super::super::RECOVERY_ENTRY_ARGUMENT.into());
            }
            let mut command = super::super::staged_ui_command(&script, incoming, owned);
            command
                .env("FIXTURE_EXPECT_OWNED", if owned { "1" } else { "0" })
                .env("FIXTURE_EXPECT_ORIGIN_HEX", hex)
                .env("FIXTURE_OUTPUT", &output)
                .env_remove("FIXTURE_REPLACED_PID");
            for key in [
                crate::agent_execution::AUTHORITY_ENV,
                "CAGENTS_AGENT_TOKEN",
                "CAGENTS_SESSION_TOKEN",
            ] {
                // An explicit inert scope in the disposable process also
                // exercises present empty/non-Unicode values. No parent
                // credential is read, removed or converted to User authority.
                command.env(key, &origin);
            }
            assert!(wait_for_reopen(&mut command).is_err());
            let receipt: serde_json::Value =
                serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
            assert_eq!(receipt["arguments"], serde_json::json!(forwarded));
            assert_eq!(receipt["owned"], owned);
            assert_eq!(receipt["same_process"], true);
            assert_eq!(receipt["origin_preserved"], true);
        }
    }
}

#[test]
fn exact_open_intent_never_substitutes_another_window_after_missing_target() {
    let request = Request::new(
        false,
        Some("device-0123456789abcdef".into()),
        Some("device-fedcba9876543210".into()),
        Some("build-host".into()),
        true,
    )
    .unwrap();
    let mut attempted = Vec::new();
    assert!(
        !request
            .focus_exact_with(|slot| {
                attempted.push(slot.to_owned());
                Ok(false)
            })
            .unwrap()
    );
    assert_eq!(attempted, ["device-0123456789abcdef"]);
    assert!(
        request
            .focus_exact_with(|_| Err(std::io::Error::other("unavailable")))
            .is_err()
    );
}

#[test]
fn opening_screen_retains_request_and_recovery_controls_after_failed_startup() {
    let mut state = window(true);
    let context = serde_json::to_value(&state.request).unwrap();
    drop(state.update(Message::Open));
    assert!(state.working && state.opening);
    let (labels, messages) = rendered(&state, 420.0, 340.0, Some("Close"));
    assert_eq!(messages.len(), 1);
    assert!(labels.iter().any(|label| label == "Opening your workspace"));
    assert!(
        labels
            .iter()
            .any(|label| label.contains("acknowledges startup"))
    );
    drop(state.update(Message::Opened(Err("Fixture startup refused"))));
    assert!(!state.working && !state.opening);
    assert_eq!(serde_json::to_value(&state.request).unwrap(), context);
    let (_, messages) = rendered(&state, 420.0, 340.0, Some("Open cagents"));
    assert_eq!(messages.len(), 1, "explicit retry remains available");
    // Startup tasks are unpolled here; the sibling process fixture exercises
    // the real readiness boundary without launching any native application.
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
            startup_delayed: false,
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
