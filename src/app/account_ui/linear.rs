//! Linear keys are a separate integration from provider CLI subscriptions.
//! Only redacted inputs and operation status cross the UI message boundary.
use super::*;
use crate::linear::{api::Client, credentials::{self, Secret}};

#[derive(Default)]
pub(super) struct State {
    workspace: String,
    secret: Secret,
    busy: Option<crate::model::OperationId>,
    status: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Action {
    Workspace(String),
    Secret(Secret),
    Store,
    Test,
    Remove,
    Finished { id: crate::model::OperationId, result: Result<String, String> },
}

impl Action {
    pub(super) fn blocked_while_modal(&self) -> bool {
        !matches!(self, Self::Finished { .. })
    }
}

fn message(action: Action) -> Message {
    Message::AccountUi(super::Action::Linear(action))
}

pub(super) fn update(state: &mut State, action: Action) -> Task<Message> {
    if let Action::Finished { id, result } = action {
        if state.busy == Some(id) {
            state.busy = None;
            state.status = Some(result.unwrap_or_else(|error| error));
        }
        return Task::none();
    }
    if state.busy.is_some() { return Task::none(); }
    match action {
        Action::Workspace(workspace) => {
            state.workspace = workspace;
            state.status = None;
            // A draft key cannot accidentally follow a workspace edit.
            state.secret = Secret::default();
            Task::none()
        }
        Action::Secret(secret) => {
            state.secret = secret;
            state.status = None;
            Task::none()
        }
        operation @ (Action::Store | Action::Test | Action::Remove) => {
            let workspace = state.workspace.trim().to_owned();
            if let Err(error) = crate::linear::api::workspace_variable((!workspace.is_empty()).then_some(workspace.as_str())) {
                state.status = Some(error.message);
                return Task::none();
            }
            let secret = std::mem::take(&mut state.secret);
            let id = crate::model::OperationId::new();
            state.busy = Some(id);
            state.status = None;
            cosmic::task::future(async move {
                let result = tokio::task::spawn_blocking(move || {
                    let workspace = (!workspace.is_empty()).then_some(workspace.as_str());
                    match operation {
                        Action::Store => credentials::store(workspace, &secret)
                            .map(|()| "Linear key saved in the desktop keyring.".to_owned()),
                        Action::Remove => credentials::remove(workspace)
                            .map(|()| "Saved Linear key removed. An environment override, if set, still applies.".to_owned()),
                        Action::Test => Client::from_workspace(workspace).and_then(|client| client.request(
                            "query { viewer { id } }", serde_json::json!({}), false,
                        )).map(|_| "Linear authentication succeeded.".to_owned()),
                        _ => unreachable!(),
                    }.map_err(|error| error.message)
                }).await.map_err(|_| "Linear credential worker unavailable".to_owned()).and_then(|result| result);
                message(Action::Finished { id, result })
            })
        }
        Action::Finished { .. } => unreachable!(),
    }
}

pub(super) fn view(state: &State) -> Element<'_, Message> {
    let editable = state.busy.is_none();
    let mut workspace = crate::ui_controls::field::text("Default workspace", &state.workspace)
        .id(widget::Id::new("accounts.linear.workspace"));
    let mut secret = crate::ui_controls::field::text("Linear personal API key", &state.secret.0)
        .secure(true)
        .id(widget::Id::new("accounts.linear.key"));
    if editable {
        workspace = workspace.on_input(|value| message(Action::Workspace(value)));
        secret = secret.on_input(|value| message(Action::Secret(Secret(value))));
    }
    let controls = widget::row::with_children(vec![
        crate::ui_controls::button::primary("Save key")
            .on_press_maybe((editable && !state.secret.0.is_empty()).then_some(message(Action::Store))).into(),
        crate::ui_controls::button::quiet("Test")
            .on_press_maybe(editable.then_some(message(Action::Test))).into(),
        crate::ui_controls::button::quiet("Remove saved key")
            .on_press_maybe(editable.then_some(message(Action::Remove))).into(),
    ]).spacing(space::S);
    let mut rows: Vec<Element<'_, Message>> = vec![
        design::title("Linear").into(),
        design::caption("Keys are stored in the desktop keyring, never in settings. Environment variables override saved keys. For a named workspace, enter its alias; leave blank for the default.").into(),
        workspace.into(), secret.into(), controls.into(),
    ];
    if state.busy.is_some() {
        rows.push(design::caption("Checking the desktop keyring or Linear…").into());
    } else if let Some(status) = &state.status {
        rows.push(design::caption(status).into());
    }
    widget::column::with_children(rows).spacing(space::S).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_message(task: Task<Message>) -> Message {
        use cosmic::iced::futures::StreamExt;
        let mut stream = cosmic::iced::runtime::task::into_stream(task).expect("credential task");
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
            while let Some(action) = stream.next().await {
                if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                    return message;
                }
            }
            panic!("credential task produced no result")
        })
    }

    #[test]
    fn store_and_remove_use_keyring_without_serializing_or_logging_the_key() {
        const CHILD: &str = "CAGENTS_LINEAR_UI_FIXTURE_CHILD";
        const NAME: &str = "app::account_ui::linear::tests::store_and_remove_use_keyring_without_serializing_or_logging_the_key";
        if std::env::var_os(CHILD).is_some() {
            use std::os::unix::fs::PermissionsExt;
            let root = PathBuf::from(std::env::var_os("LINEAR_UI_FIXTURE").unwrap());
            let key = "fixture-linear-ui-private-key";
            let mut state = State::default();
            let _ = update(&mut state, Action::Secret(Secret(key.into())));
            let task = update(&mut state, Action::Store);
            assert!(state.secret.0.is_empty(), "the draft must clear as saving begins");
            let message = first_message(task);
            assert!(!format!("{message:?}").contains(key), "save result must not log the key");
            let Message::AccountUi(super::super::Action::Linear(action)) = message else { panic!("wrong save result") };
            let _ = update(&mut state, action);
            assert!(state.busy.is_none());
            assert!(state.status.as_deref().unwrap().contains("saved"));
            assert!(std::fs::read(root.join("vault")).unwrap() == key.as_bytes());
            assert_eq!(std::fs::metadata(root.join("vault")).unwrap().permissions().mode() & 0o077, 0);
            assert!(!toml::to_string(&AppSettings::default()).unwrap().contains(key));
            assert!(!std::fs::read_to_string(root.join("argv")).unwrap().contains(key));
            let task = update(&mut state, Action::Remove);
            let message = first_message(task);
            assert!(!format!("{message:?}").contains(key));
            let Message::AccountUi(super::super::Action::Linear(action)) = message else { panic!("wrong remove result") };
            let _ = update(&mut state, action);
            assert!(state.busy.is_none());
            assert!(state.status.as_deref().unwrap().contains("removed"));
            assert!(!root.join("vault").exists());
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let bin = fixture.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let helper = bin.join("secret-tool");
        std::fs::write(&helper, r#"#!/bin/sh
set -eu
printf '%s\n' "$@" > "$LINEAR_UI_FIXTURE/argv"
case "$1" in
  store) umask 077; cat > "$LINEAR_UI_FIXTURE/vault" ;;
  clear) rm -f -- "$LINEAR_UI_FIXTURE/vault" ;;
  lookup) test -f "$LINEAR_UI_FIXTURE/vault" && cat "$LINEAR_UI_FIXTURE/vault" ;;
  *) exit 2 ;;
esac
"#).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env_clear()
            .env(CHILD, "1")
            .env("LINEAR_UI_FIXTURE", fixture.path())
            .env("HOME", fixture.path())
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .output().unwrap();
        assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "isolated fixture test must actually run");
    }

    #[test]
    fn key_input_is_redacted_and_never_enters_settings() {
        let mut state = State::default();
        let secret = Secret("fixture-linear-private-key".into());
        let action = Action::Secret(secret.clone());
        assert!(!format!("{action:?}").contains(&secret.0));
        let _ = update(&mut state, action);
        assert_eq!(state.secret.0, secret.0);
        let _ = update(&mut state, Action::Workspace("TEST".into()));
        assert!(state.secret.0.is_empty());
        assert!(!toml::to_string(&AppSettings::default()).unwrap().contains("fixture-linear-private-key"));
    }

    #[test]
    fn late_credential_results_cannot_overwrite_a_new_operation() {
        let current = crate::model::OperationId::new();
        let mut state = State { busy: Some(current), ..State::default() };
        let _ = update(&mut state, Action::Finished { id: crate::model::OperationId::new(), result: Ok("stale".into()) });
        assert_eq!(state.busy, Some(current));
        assert!(state.status.is_none());
    }
}
