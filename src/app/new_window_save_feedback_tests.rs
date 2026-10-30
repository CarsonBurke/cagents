//! Real New Window receipts with an absent private UI companion. The process
//! launcher cannot create a window, and the settings/workspace saves are real.

use super::*;
use cosmic::iced::futures::StreamExt;
use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    process::{Child, Command},
};

const ROOT: &str = "CAGENTS_TEST_NEW_WINDOW_SAVE_FEEDBACK_ROOT";

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
    // Prefer the same filesystem so a hard link avoids copying the Rust ELF.
    let directory = tempfile::Builder::new()
        .prefix("new-window-settings-")
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
    let private_executable = root.join("bin/settings-feedback-test");
    if fs::hard_link(&executable, &private_executable).is_err() {
        fs::copy(&executable, &private_executable).unwrap();
    }
    let stdout = root.join("stdout");
    let stderr = root.join("stderr");
    // Inherit the caller's complete authority. Only fixture-owned XDG paths
    // change; this must never turn an agent invocation into a User invocation.
    let mut command = Command::new(private_executable);
    command
        .args([
            "--exact",
            &format!("app::new_window_save_feedback_tests::{name}"),
            "--nocapture",
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
        output(stdout),
        output(stderr)
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
        crate::model::BootId::new("new-window-settings-feedback"),
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

fn fail_setting_save(app: &mut Cagents, value: bool) -> String {
    let path = app.settings_path.clone().unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let message = setting_result(app, value);
    let Message::SettingsSaved {
        generation,
        result: Err(error),
    } = &message
    else {
        panic!("the directory must fail real settings storage: {message:?}");
    };
    assert_eq!(*generation, app.settings_save_generation);
    let error = error.clone();
    drop(app.update(message));
    assert_eq!(app.settings_save_error.as_deref(), Some(error.as_str()));
    assert!(
        matches!(&app.settings_feedback, Some(SettingsFeedback::Error(actual)) if actual == &error)
    );
    assert!(!app.settings_write_blocked);
    assert!(app.settings_pending.is_empty());
    error
}

fn repair_settings_file(app: &Cagents) {
    let path = app.settings_path.as_ref().unwrap();
    fs::remove_dir(path).unwrap();
    settings::save(path, &app.settings_base).unwrap();
}

fn new_window_receipt(app: &mut Cagents) -> (Message, u64, String) {
    assert!(app.settings_pending.is_empty());
    let mut outputs = messages(app.update(Message::NewWindow))
        .into_iter()
        .filter(|message| matches!(message, Message::NewWindowOpened { .. }));
    let message = outputs
        .next()
        .expect("the actual New Window task must finish");
    assert!(outputs.next().is_none());
    let Message::NewWindowOpened {
        saved: Ok(saved),
        result: Err(error),
    } = &message
    else {
        panic!("real saves must finish before the absent companion refuses launch: {message:?}");
    };
    assert_eq!(saved.generation, app.settings_save_generation);
    assert!(
        error.contains("os error 2"),
        "the launcher must reach the absent companion, without another gate: {error}"
    );
    assert_eq!(
        settings::load(app.settings_path.as_ref().unwrap())
            .unwrap()
            .confirm_multiline_paste,
        app.settings.confirm_multiline_paste
    );
    let (generation, error) = (saved.generation, error.clone());
    (message, generation, error)
}

fn assert_launch_error_retained(app: &Cagents, error: &str) {
    assert!(app.notices.recent().any(|entry| {
        entry.notice.severity == NoticeSeverity::Error
            && entry.notice.message == format!("Could not open a new window: {error}")
    }));
}

#[test]
fn new_window_save_receipt_clears_a_repaired_settings_error() {
    isolated(
        "new_window_save_receipt_clears_a_repaired_settings_error",
        |root| {
            let mut app = fixture(root);
            let value = !app.settings.confirm_multiline_paste;
            fail_setting_save(&mut app, value);
            let failed_generation = app.settings_save_generation;
            repair_settings_file(&app);
            let (receipt, generation, launch_error) = new_window_receipt(&mut app);
            assert!(generation > failed_generation);
            drop(app.update(receipt));
            assert_eq!(app.settings_ack_generation, generation);
            assert!(app.settings_save_error.is_none());
            assert!(matches!(
                app.settings_feedback,
                Some(SettingsFeedback::Saved)
            ));
            assert_launch_error_retained(&app, &launch_error);
        },
    );
}

#[test]
fn an_older_new_window_receipt_preserves_a_newer_settings_error() {
    isolated(
        "an_older_new_window_receipt_preserves_a_newer_settings_error",
        |root| {
            let mut app = fixture(root);
            let value = !app.settings.confirm_multiline_paste;
            fail_setting_save(&mut app, value);
            repair_settings_file(&app);
            let (receipt, generation, launch_error) = new_window_receipt(&mut app);
            let current_error = fail_setting_save(&mut app, !value);
            assert!(app.settings_save_generation > generation);
            drop(app.update(receipt));
            assert_eq!(app.settings_ack_generation, generation);
            assert_eq!(
                app.settings_save_error.as_deref(),
                Some(current_error.as_str())
            );
            assert!(
                matches!(&app.settings_feedback, Some(SettingsFeedback::Error(error)) if error == &current_error)
            );
            assert_launch_error_retained(&app, &launch_error);
        },
    );
}

#[test]
fn ordinary_settings_retry_keeps_clearing_its_repaired_error() {
    isolated(
        "ordinary_settings_retry_keeps_clearing_its_repaired_error",
        |root| {
            let mut app = fixture(root);
            let value = !app.settings.confirm_multiline_paste;
            fail_setting_save(&mut app, value);
            repair_settings_file(&app);
            let saved = setting_result(&mut app, value);
            assert!(matches!(
                &saved,
                Message::SettingsSaved { result: Ok(_), .. }
            ));
            drop(app.update(saved));
            assert!(app.settings_save_error.is_none());
            assert!(matches!(
                app.settings_feedback,
                Some(SettingsFeedback::Saved)
            ));
            assert_eq!(
                settings::load(app.settings_path.as_ref().unwrap())
                    .unwrap()
                    .confirm_multiline_paste,
                value
            );
        },
    );
}
