//! Names a channel from its conversation's first prompt when the chat's
//! loaded window starts after that prompt, as a long conversation's does.
//!
//! A snapshot only names a channel from history it shows from the start (see
//! `snapshot_channel_name`). A channel whose automatic name was lost keeps
//! "New channel" until its provider reports a title. Opening its chat reads
//! the head of the conversation's transcript once, off the window's thread,
//! and offers the first prompt's name through the observation that snapshot
//! names use, so it is published with the same shared-view proof and never
//! replaces a name the reader gave.
use super::*;

/// Transcript heads read at once. A window that starts with many unnamed
/// channels queues their reads rather than contending for the disk.
static READS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// The channel name the conversation `launch` resumes opens with: the first
/// prompt in its transcript's head that names a task. `None` for a provider
/// whose history this cannot locate by conversation, a missing, unreadable or
/// linked transcript, or one with no such prompt within
/// [`resume_ui::HEAD_BYTES`].
pub(super) fn read(launch: &chat::Launch) -> Option<String> {
    let id = launch.session_id.as_deref()?;
    let home = launch.provider.session_root(&launch.recipe)?;
    let path = match launch.provider {
        Provider::Claude => {
            // Claude files a conversation under its canonical directory. A
            // removed worktree keeps the saved path, which was canonical.
            let directory = &launch.recipe.working_directory;
            let cwd = directory
                .canonicalize()
                .unwrap_or_else(|_| directory.clone());
            crate::agent::claude_transcript(&home, &cwd, id).ok()?
        }
        Provider::Codex => crate::agent::codex_rollout(&home, id).ok()?,
        Provider::Omp => return None,
    };
    super::super::resume_ui::first_prompt(
        launch.provider,
        &path,
        super::super::resume_ui::HEAD_BYTES,
        &AtomicBool::new(false),
        crate::workspace_model::channel_title_from_prompt,
    )
}

impl Cagents {
    /// Reads the opening of the conversation `target` shows, once per pane
    /// and conversation, when it is the primary chat of a channel with no
    /// name and nothing it has shown names it.
    pub(super) fn request_opening_name(&mut self, target: Target) -> Task<Message> {
        // The transcript is on the device, which a remote window cannot read.
        if crate::device::is_remote()
            || self
                .chat_ui
                .channel_names
                .get(&target)
                .is_none_or(|observation| observation.title.is_some())
            || !self.channel_lacks_name(target.card)
        {
            return Task::none();
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let launch = runtime.session.launch.clone();
        let Some(conversation) = launch.session_id.clone() else {
            return Task::none();
        };
        if launch.provider == Provider::Omp
            || !self
                .chat_ui
                .opening_names
                .insert((target.card, conversation.clone()))
        {
            return Task::none();
        }
        cosmic::task::future(async move {
            let title = match READS.acquire().await {
                Ok(_permit) => tokio::task::spawn_blocking(move || read(&launch))
                    .await
                    .ok()
                    .flatten(),
                Err(_) => None,
            };
            Message::ChatUi(Action::OpeningName(target, conversation, title))
        })
    }

    /// Offers the name read from a conversation's opening, unless the pane
    /// has moved on to another conversation, its chat named the channel in
    /// the meantime, or the channel has a name.
    pub(super) fn opening_name_read(
        &mut self,
        target: Target,
        conversation: String,
        title: Option<String>,
    ) {
        let Some(title) = title else {
            return;
        };
        if self.chat_runtime_mut(target).is_none_or(|runtime| {
            runtime.session.launch.session_id.as_deref() != Some(conversation.as_str())
        }) || !self.channel_lacks_name(target.card)
        {
            return;
        }
        let Some(observation) = self.chat_ui.channel_names.get_mut(&target) else {
            return;
        };
        if observation.title.is_some() {
            return;
        }
        let source = crate::model::ChannelTitleSource::Prompt;
        observation.title = Some((title.clone(), source));
        observation.pending = true;
        self.publish_observed_channel_name(target, title, source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    const ID: &str = "0199a2b4-5c6d-7e8f-9a0b-1c2d3e4f5a6b";

    fn launch(provider: Provider, home: &Path, cwd: &Path) -> chat::Launch {
        let recipe = crate::model::LaunchRecipe {
            program: provider.executable().into(),
            arguments: Vec::new(),
            working_directory: cwd.to_path_buf(),
            environment: [("HOME".into(), home.to_string_lossy().into_owned())].into(),
            unset_environment: Default::default(),
        };
        chat::Launch {
            provider,
            recipe,
            session_id: Some(ID.into()),
        }
    }

    fn write(path: &Path, records: &[serde_json::Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(path).unwrap();
        for record in records {
            writeln!(file, "{record}").unwrap();
        }
    }

    fn claude_user(cwd: &Path, content: serde_json::Value) -> serde_json::Value {
        json!({"type":"user","sessionId":ID,"cwd":cwd,"message":{"role":"user","content":content}})
    }

    fn claude_path(home: &Path, cwd: &Path) -> PathBuf {
        let folder: String = cwd
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        home.join(".claude/projects")
            .join(folder)
            .join(format!("{ID}.jsonl"))
    }

    /// The opening names the channel by the first prompt the reader wrote,
    /// past injected context and greetings, however long the history after.
    #[test]
    fn a_claude_opening_names_the_channel_by_its_first_task() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let cwd = directory.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let mut records = vec![
            json!({"type":"file-history-snapshot","messageId":"m"}),
            claude_user(&cwd, json!("<system-reminder>ctx</system-reminder>")),
            claude_user(
                &cwd,
                json!("<command-name>/clear</command-name><command-args></command-args>"),
            ),
            claude_user(&cwd, json!("hi")),
            claude_user(
                &cwd,
                json!([{"type":"text","text":"Fix the token refresh race in the auth client"}]),
            ),
        ];
        records
            .extend((0..2_000).map(|turn| claude_user(&cwd, json!(format!("Later task {turn}")))));
        write(&claude_path(&home, &cwd), &records);
        assert_eq!(
            read(&launch(Provider::Claude, &home, &cwd)).as_deref(),
            Some("Fix the token refresh race in the auth client")
        );
    }

    /// Nothing names the channel from a missing, cut-off or linked transcript,
    /// one whose first prompt lies beyond the head, or a provider this cannot
    /// locate by conversation.
    #[test]
    fn an_unreadable_opening_names_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let cwd = directory.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let claude = launch(Provider::Claude, &home, &cwd);
        assert_eq!(read(&claude), None, "missing");

        let path = claude_path(&home, &cwd);
        let whole = claude_user(&cwd, json!("Fix the token refresh race")).to_string();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &whole[..whole.len() / 2]).unwrap();
        assert_eq!(read(&claude), None, "truncated");

        let filler = json!({"type":"progress","data":"x".repeat(200_000)});
        let mut records = vec![filler; (resume_ui::HEAD_BYTES / 200_000 + 1) as usize];
        records.push(claude_user(&cwd, json!("Fix the token refresh race")));
        write(&path, &records);
        assert_eq!(read(&claude), None, "beyond the head");

        let elsewhere = directory.path().join("elsewhere.jsonl");
        write(
            &elsewhere,
            &[claude_user(&cwd, json!("Fix the token refresh race"))],
        );
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
        assert_eq!(read(&claude), None, "linked");

        assert_eq!(read(&launch(Provider::Omp, &home, &cwd)), None);
        let mut unconfirmed = claude;
        unconfirmed.session_id = None;
        assert_eq!(read(&unconfirmed), None);
    }

    /// A Codex rollout is found by its thread and named by its first prompt,
    /// past the instructions Codex injects; a subagent's rollout names nothing.
    #[test]
    fn a_codex_opening_names_the_channel_by_its_first_task() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let cwd = directory.path().join("project");
        let path = home
            .join(".codex/sessions/2026/11/17")
            .join(format!("rollout-2026-11-17T10-00-00-{ID}.jsonl"));
        let meta = |source: serde_json::Value| json!({"type":"session_meta","payload":{"id":ID,"cwd":cwd,"source":source}});
        let prompt = |text: &str| json!({"type":"event_msg","payload":{"type":"user_message","message":text}});
        write(
            &path,
            &[
                meta(json!("cli")),
                prompt("<user_instructions>\nBe brief.\n</user_instructions>"),
                prompt("<environment_context>\n<cwd>/x</cwd>\n</environment_context>"),
                prompt("Add retries to the webhook sender"),
                prompt("And a test"),
            ],
        );
        let codex = launch(Provider::Codex, &home, &cwd);
        assert_eq!(
            read(&codex).as_deref(),
            Some("Add retries to the webhook sender")
        );
        write(
            &path,
            &[
                meta(json!({"subagent":"review"})),
                prompt("Add retries to the webhook sender"),
            ],
        );
        assert_eq!(read(&codex), None);
    }

    /// An unnamed channel's opening is read once per pane and conversation,
    /// and its name is offered only while the channel still has none: a name
    /// the reader gave, or one the chat found meanwhile, stays.
    #[test]
    fn an_opening_names_an_unnamed_channel_once_and_never_over_another_name() {
        use crate::model::{ChannelId, ChannelTitleSource, ProjectChannel, ProjectId};
        use crate::workspace_model::{
            ModelEdit,
            views::{Pane, Scope, View},
        };
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card =
            super::super::tests::live_chat_for_provider(&mut app, Provider::Claude, Some(ID));
        let target = super::super::tests::chat_runtime(&app, card).target;
        app.chat_runtime_mut(target)
            .unwrap()
            .session
            .launch
            .session_id = Some(ID.into());
        let id = ChannelId::new();
        Arc::make_mut(&mut app.durable).channels = vec![ProjectChannel {
            id,
            project_id: ProjectId::new(),
            title: String::new(),
            title_source: ChannelTitleSource::Prompt,
            ordinal: 0,
            shared: true,
            cards: vec![card],
            focused: Some(card),
        }];
        app.workspace_model.views = vec![View {
            scope: Scope::Channel(id),
            revision: 3,
            retired: Vec::new(),
            panes: vec![
                Pane::from_card(
                    app.card_state(card).unwrap(),
                    app.durable.card_scopes.get(&card).cloned(),
                )
                .unwrap(),
            ],
        }];
        let (tx, requests) = super::super::super::workspace_model_sync::test_model_channel();
        app.workspace_model_tx = tx;
        let unnamed = || ChannelNameObservation {
            session: Some(ID.into()),
            ..Default::default()
        };
        let reads = |task: Task<Message>| cosmic::iced::runtime::task::into_stream(task).is_some();

        // A window whose chat already named the channel reads nothing.
        app.chat_ui.channel_names.insert(
            target,
            ChannelNameObservation {
                title: Some(("Shown task".into(), ChannelTitleSource::Prompt)),
                ..unnamed()
            },
        );
        assert!(!reads(app.request_opening_name(target)));

        app.chat_ui.channel_names.insert(target, unnamed());
        assert!(reads(app.request_opening_name(target)));
        assert!(
            !reads(app.request_opening_name(target)),
            "the same conversation is read once"
        );
        // An opening with no task in it leaves the channel as it was.
        drop(app.update(Message::ChatUi(Action::OpeningName(
            target,
            ID.into(),
            None,
        ))));
        assert_eq!(app.durable.channels[0].title, "");
        // A read for a conversation the pane no longer shows is dropped.
        app.opening_name_read(target, "another".into(), Some("Stale task".into()));
        assert_eq!(app.durable.channels[0].title, "");

        app.opening_name_read(target, ID.into(), Some("Fix the token refresh race".into()));
        assert_eq!(
            (
                app.durable.channels[0].title.as_str(),
                app.durable.channels[0].title_source
            ),
            ("Fix the token refresh race", ChannelTitleSource::Prompt)
        );
        assert!(std::iter::from_fn(|| requests.try_recv().ok()).any(|edit| matches!(
            edit,
            ModelEdit::SuggestConversationTitle { id: channel, card: pane, expected_revision: 3, title, source: ChannelTitleSource::Prompt }
                if channel == id && pane == card && title == "Fix the token refresh race"
        )));

        // A name the reader gave while the read ran is never replaced.
        let channel = &mut Arc::make_mut(&mut app.durable).channels[0];
        channel.title = "Auth work".into();
        channel.title_source = ChannelTitleSource::Manual;
        app.chat_ui.channel_names.insert(target, unnamed());
        app.opening_name_read(target, ID.into(), Some("Fix the token refresh race".into()));
        assert_eq!(app.durable.channels[0].title, "Auth work");
        assert!(app.chat_ui.channel_names[&target].title.is_none());
        app.chat_ui.opening_names.clear();
        assert!(!reads(app.request_opening_name(target)));
    }
}
