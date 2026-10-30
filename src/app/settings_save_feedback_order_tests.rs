//! Ordinary settings-save feedback from real writes and held original callbacks.
//! Private executable/XDG fixtures inherit authority and never launch a UI.

use super::*;
use cosmic::iced::futures::StreamExt;
use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    process::{Child, Command},
};

const ROOT: &str = "CAGENTS_TEST_SETTINGS_SAVE_FEEDBACK_ORDER_ROOT";

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn isolated(name: &str, exercise: impl FnOnce(&Path)) {
    if let Some(root) = std::env::var_os(ROOT) {
        let root = PathBuf::from(root);
        assert_eq!(
            std::env::current_exe().unwrap().parent(),
            Some(root.join("bin").as_path())
        );
        assert!(
            !crate::companion::Product::Ui
                .beside_current()
                .unwrap()
                .exists()
        );
        exercise(&root);
        return;
    }

    let executable = std::env::current_exe().unwrap();
    // Give the private alias its own inode without changing the source ELF.
    let directory = tempfile::Builder::new()
        .prefix("settings-save-order-")
        .tempdir_in(executable.parent().unwrap())
        .or_else(|_| tempfile::tempdir())
        .unwrap();
    let root = directory.path();
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    for child in ["bin", "run", "config", "data", "cache"] {
        let path = root.join(child);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let private_executable = root.join("bin/settings-save-order-test");
    fs::copy(&executable, &private_executable).unwrap();
    let stdout = root.join("stdout");
    let stderr = root.join("stderr");
    // Inherit the caller's complete authority. Only fixture-owned XDG paths
    // change; this must never turn an agent invocation into a User invocation.
    let mut command = Command::new(private_executable);
    command
        .args([
            "--exact",
            &format!("app::settings_save_feedback_order_tests::{name}"),
            "--nocapture",
            "--test-threads=1",
            "--color=never",
        ])
        .env(ROOT, root)
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(fs::File::create(&stdout).unwrap())
        .stderr(fs::File::create(&stderr).unwrap());
    let mut child = OwnedChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(45);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.0.kill();
            let _ = child.0.wait();
            panic!("isolated settings feedback test exceeded 45 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = |path| {
        let mut text = String::new();
        fs::File::open(path)
            .unwrap()
            .take(64 * 1024)
            .read_to_string(&mut text)
            .unwrap();
        text
    };
    assert!(
        status.success(),
        "isolated settings feedback test failed:\n{}\n{}",
        output(&stdout),
        output(&stderr)
    );
    let stdout = output(&stdout);
    let expected = format!("test app::settings_save_feedback_order_tests::{name} ... ok");
    assert_eq!(
        stdout
            .lines()
            .filter(|line| *line == "running 1 test")
            .count(),
        1
    );
    assert_eq!(stdout.lines().filter(|line| *line == expected).count(), 1);
    let summaries = stdout
        .lines()
        .filter(|line| line.starts_with("test result:"))
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 1);
    assert!(
        summaries[0].starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; ")
    );
}

fn fixture(root: &Path) -> Cagents {
    let mut app = super::tests::context_inspector_test_app(false);
    let path = root.join("settings.toml");
    settings::save(&path, &app.settings).unwrap();
    app.settings = settings::load(&path).unwrap();
    app.settings_base = app.settings.clone();
    app.settings_path = Some(path);
    app.settings_write_blocked = false;
    let (session, _) = SessionRuntime::open(
        root.join("workspace.sqlite3"),
        crate::model::BootId::new("settings-save-order-feedback"),
    )
    .unwrap();
    app.session = Some(session);
    app
}

fn messages(task: Task<Message>) -> Vec<Message> {
    let Some(mut stream) = iced::runtime::task::into_stream(task) else {
        return Vec::new();
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let mut messages = Vec::new();
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                    messages.push(message);
                }
            }
            messages
        })
}

fn setting_result(app: &mut Cagents, value: bool) -> Message {
    let outputs = messages(app.update(Message::SettingsChanged {
        field: SettingsField::ConfirmMultilinePaste,
        value: value.to_string(),
    }));
    let mut saves = outputs
        .into_iter()
        .filter(|message| matches!(message, Message::SettingsSaved { .. }));
    let saved = saves.next().expect("the public setting action must save");
    assert!(saves.next().is_none());
    saved
}

fn success_generation(message: &Message) -> u64 {
    let Message::SettingsSaved {
        result: Ok(saved), ..
    } = message
    else {
        panic!("the actual settings task must succeed: {message:?}");
    };
    saved.generation
}

fn failure(message: &Message) -> (u64, String) {
    let Message::SettingsSaved {
        generation,
        result: Err(error),
    } = message
    else {
        panic!("private storage must refuse the actual settings task: {message:?}");
    };
    (*generation, error.clone())
}

fn replace_settings_with_directory(app: &Cagents) {
    let path = app.settings_path.as_ref().unwrap();
    fs::remove_file(path).unwrap();
    fs::create_dir(path).unwrap();
}

fn repair_settings_file(app: &Cagents) {
    let path = app.settings_path.as_ref().unwrap();
    if fs::symlink_metadata(path).unwrap().is_dir() {
        fs::remove_dir(path).unwrap();
    } else {
        fs::remove_file(path).unwrap();
    }
    settings::save(path, &app.settings_base).unwrap();
}

fn assert_error(app: &Cagents, error: &str) {
    assert_eq!(app.settings_save_error.as_deref(), Some(error));
    assert!(matches!(&app.settings_feedback,
        Some(SettingsFeedback::Error(current)) if current == error));
    assert!(!app.settings_write_blocked);
}

fn notices(app: &Cagents) -> Vec<TransientNotice> {
    app.notices
        .recent()
        .map(|entry| entry.notice.clone())
        .collect()
}

#[test]
fn older_settings_success_preserves_newer_failure_until_current_retry() {
    isolated(
        "older_settings_success_preserves_newer_failure_until_current_retry",
        |root| {
            let mut app = fixture(root);
            let original = app.settings.confirm_multiline_paste;
            let held = setting_result(&mut app, !original);
            let older = success_generation(&held);
            assert_eq!(app.settings_ack_generation, 0);
            assert_eq!(
                settings::load(app.settings_path.as_ref().unwrap())
                    .unwrap()
                    .confirm_multiline_paste,
                !original
            );

            replace_settings_with_directory(&app);
            let current = setting_result(&mut app, original);
            let (latest, error) = failure(&current);
            assert!(latest > older);
            drop(app.update(current));
            assert_error(&app, &error);
            let current_notices = notices(&app);

            drop(app.update(held));
            assert_eq!(app.settings_ack_generation, older);
            assert_eq!(app.settings_save_generation, latest);
            assert_eq!(app.settings.confirm_multiline_paste, original);
            assert_error(&app, &error);
            assert_eq!(notices(&app), current_notices);

            repair_settings_file(&app);
            // Retrying the same visible value still dispatches a genuine save
            // because its current error remains actionable.
            let retry = setting_result(&mut app, original);
            let saved = success_generation(&retry);
            assert!(saved > latest);
            drop(app.update(retry));
            assert_eq!(app.settings_ack_generation, saved);
            assert!(app.settings_save_error.is_none());
            assert!(matches!(
                app.settings_feedback,
                Some(SettingsFeedback::Saved)
            ));
            assert_eq!(
                settings::load(app.settings_path.as_ref().unwrap())
                    .unwrap()
                    .confirm_multiline_paste,
                original
            );
        },
    );
}

#[test]
fn older_settings_failure_preserves_newer_failure_or_pending_feedback() {
    isolated(
        "older_settings_failure_preserves_newer_failure_or_pending_feedback",
        |root| {
            for pending in [false, true] {
                let case = root.join(if pending { "pending" } else { "failed" });
                fs::create_dir(&case).unwrap();
                let mut app = fixture(&case);
                let original = app.settings.confirm_multiline_paste;
                replace_settings_with_directory(&app);
                let held = setting_result(&mut app, !original);
                let (older, old_error) = failure(&held);
                assert!(app.settings_save_error.is_none());
                repair_settings_file(&app);

                let latest_error;
                let current;
                if pending {
                    // Capture the actual new save without polling its task.
                    current = app.update(Message::SettingsChanged {
                        field: SettingsField::ConfirmMultilinePaste,
                        value: original.to_string(),
                    });
                    latest_error = None;
                    assert!(matches!(
                        app.settings_feedback,
                        Some(SettingsFeedback::Pending)
                    ));
                } else {
                    let path = app.settings_path.as_ref().unwrap();
                    let target = case.join("owned-target.toml");
                    settings::save(&target, &app.settings_base).unwrap();
                    fs::remove_file(path).unwrap();
                    std::os::unix::fs::symlink(&target, path).unwrap();
                    let failed = setting_result(&mut app, original);
                    let (generation, error) = failure(&failed);
                    assert!(generation > older);
                    assert_ne!(
                        error, old_error,
                        "the two real failures must be distinguishable"
                    );
                    drop(app.update(failed));
                    assert_error(&app, &error);
                    latest_error = Some(error);
                    current = Task::none();
                }
                let latest = app.settings_save_generation;
                assert!(latest > older);
                let current_notices = notices(&app);
                drop(app.update(held));
                assert_eq!(app.settings_ack_generation, 0);
                assert_eq!(app.settings_save_generation, latest);
                assert_eq!(notices(&app), current_notices);
                if let Some(error) = latest_error {
                    assert_error(&app, &error);
                } else {
                    assert!(app.settings_save_error.is_none());
                    assert!(matches!(
                        app.settings_feedback,
                        Some(SettingsFeedback::Pending)
                    ));
                    let mut callbacks = messages(current)
                        .into_iter()
                        .filter(|message| matches!(message, Message::SettingsSaved { .. }));
                    let saved = callbacks.next().unwrap();
                    assert!(callbacks.next().is_none());
                    assert_eq!(success_generation(&saved), latest);
                    drop(app.update(saved));
                    assert_eq!(app.settings_ack_generation, latest);
                    assert!(matches!(
                        app.settings_feedback,
                        Some(SettingsFeedback::Saved)
                    ));
                    assert_eq!(
                        settings::load(app.settings_path.as_ref().unwrap())
                            .unwrap()
                            .confirm_multiline_paste,
                        original
                    );
                }
            }
        },
    );
}

#[test]
fn newer_lane_receipt_forwarded_by_older_settings_task_settles_current_feedback() {
    isolated(
        "newer_lane_receipt_forwarded_by_older_settings_task_settles_current_feedback",
        |root| {
            let mut app = fixture(root);
            let original = app.settings.confirm_multiline_paste;
            replace_settings_with_directory(&app);
            let failed = setting_result(&mut app, !original);
            let (_, error) = failure(&failed);
            drop(app.update(failed));
            assert_error(&app, &error);
            repair_settings_file(&app);

            let held_task = app.update(Message::SettingsChanged {
                field: SettingsField::ConfirmMultilinePaste,
                value: original.to_string(),
            });
            let older_dispatch = app.settings_save_generation;
            let latest_callback = setting_result(&mut app, !original);
            let latest = success_generation(&latest_callback);
            assert!(latest > older_dispatch);
            assert_error(&app, &error);
            let mut callbacks = messages(held_task)
                .into_iter()
                .filter(|message| matches!(message, Message::SettingsSaved { .. }));
            let forwarded = callbacks.next().unwrap();
            assert!(callbacks.next().is_none());
            let Message::SettingsSaved { generation, .. } = &forwarded else {
                unreachable!()
            };
            assert_eq!(*generation, older_dispatch);
            assert_eq!(success_generation(&forwarded), latest);
            drop(app.update(forwarded));
            assert_eq!(app.settings_ack_generation, latest);
            assert!(app.settings_save_error.is_none());
            assert!(matches!(
                app.settings_feedback,
                Some(SettingsFeedback::Saved)
            ));
            assert_eq!(
                settings::load(app.settings_path.as_ref().unwrap())
                    .unwrap()
                    .confirm_multiline_paste,
                !original
            );

            // The original current callback remains an old success once a
            // later genuine failure is delivered. Its duplicate receipt must
            // not clear that newer error merely because it equals the ack.
            replace_settings_with_directory(&app);
            let newer_failure = setting_result(&mut app, original);
            let (newest, error) = failure(&newer_failure);
            assert!(newest > latest);
            drop(app.update(newer_failure));
            let current_notices = notices(&app);
            drop(app.update(latest_callback));
            assert_eq!(app.settings_ack_generation, latest);
            assert_error(&app, &error);
            assert_eq!(notices(&app), current_notices);
        },
    );
}
