//! Names a channel from its conversation's first prompt when the chat's
//! loaded window starts after that prompt, as a long conversation's does.
//!
//! A snapshot only names a channel from history it shows from the start (see
//! `snapshot_channel_name`). A channel whose automatic name was lost, or one a
//! long conversation was resumed into, keeps "New channel" until its provider
//! reports a title. Opening its chat reads
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
    /// Names the channel `target` is the primary chat of from its
    /// conversation's opening, when the channel has no name and nothing the
    /// chat has shown names it. Each pane's conversation is read at most once
    /// while the window runs; a later attach of the same conversation, after a
    /// reconnect or when the pane becomes primary, offers what that read found.
    /// The task is the read, when one starts.
    pub(super) fn request_opening_name(&mut self, target: Target) -> Option<Task<Message>> {
        // The transcript is on the device, which a remote window cannot read.
        if crate::device::is_remote()
            || self
                .chat_ui
                .channel_names
                .get(&target)
                .is_none_or(|observation| observation.title.is_some())
            || !self.channel_lacks_name(target.card)
        {
            return None;
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return None;
        };
        let launch = runtime.session.launch.clone();
        let Some(conversation) = launch.session_id.clone() else {
            return None;
        };
        if launch.provider == Provider::Omp {
            return None;
        }
        let key = (target.card, conversation);
        match self.chat_ui.opening_names.get(&key) {
            Some(Some(Some(title))) => {
                let title = title.clone();
                self.offer_opening_name(target, title);
                return None;
            }
            // Read already, or being read.
            Some(_) => return None,
            None => {}
        }
        self.chat_ui.opening_names.insert(key.clone(), None);
        tracing::debug!(card = %target.card, provider = ?launch.provider, "reading a conversation's opening to name its channel");
        let (card, conversation) = key;
        Some(cosmic::task::future(async move {
            let title = match READS.acquire().await {
                Ok(_permit) => tokio::task::spawn_blocking(move || read(&launch))
                    .await
                    .ok()
                    .flatten(),
                Err(_) => None,
            };
            Message::ChatUi(Action::OpeningName(card, conversation, title))
        }))
    }

    /// Keeps what the opening of `card`'s conversation named, and offers it
    /// through the pane's current runtime if it still shows that conversation.
    pub(super) fn opening_name_read(
        &mut self,
        card: CardId,
        conversation: String,
        title: Option<String>,
    ) {
        tracing::debug!(%card, named = title.is_some(), "read a conversation's opening");
        self.chat_ui
            .opening_names
            .insert((card, conversation.clone()), Some(title.clone()));
        let Some(title) = title else {
            return;
        };
        let runtime = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime.as_ref()),
            _ => self.chat_ui.parked.get(&card).map(Box::as_ref),
        };
        if let Some(target) = runtime
            .filter(|runtime| {
                runtime.session.launch.session_id.as_deref() == Some(conversation.as_str())
            })
            .map(|runtime| runtime.target)
        {
            self.offer_opening_name(target, title);
        }
    }

    /// Offers `title` as the name of the channel `target` is the primary chat
    /// of, unless the channel has a name or the chat has named it meanwhile.
    fn offer_opening_name(&mut self, target: Target, title: String) {
        if !self.channel_lacks_name(target.card) {
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
        // Newer Codex versions report a prompt as a completed item.
        write(
            &path,
            &[
                meta(json!("cli")),
                json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"input_text","text":"Cache the pricing lookups"}]}}}),
            ],
        );
        assert_eq!(read(&codex).as_deref(), Some("Cache the pricing lookups"));
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
            // A new channel, and one whose automatic name was cleared, are
            // both empty; a new one's source is manual.
            title: String::new(),
            title_source: ChannelTitleSource::Manual,
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
        let reads = |task: Option<Task<Message>>| task.is_some();

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
        // An opening with no task in it leaves the channel as it was, and is
        // not read again.
        drop(app.update(Message::ChatUi(Action::OpeningName(card, ID.into(), None))));
        assert_eq!(app.durable.channels[0].title, "");
        assert!(!reads(app.request_opening_name(target)));
        // A read for a conversation the pane no longer shows names nothing.
        app.opening_name_read(card, "another".into(), Some("Stale task".into()));
        assert_eq!(app.durable.channels[0].title, "");

        app.chat_ui.opening_names.clear();
        assert!(reads(app.request_opening_name(target)));
        app.opening_name_read(card, ID.into(), Some("Fix the token refresh race".into()));
        let named = |app: &Cagents| {
            (
                app.durable.channels[0].title.clone(),
                app.durable.channels[0].title_source,
            )
        };
        let found = (
            "Fix the token refresh race".to_owned(),
            ChannelTitleSource::Prompt,
        );
        assert_eq!(named(&app), found);
        let suggested =
            |requests: &super::super::super::workspace_model_sync::TestModelRequests| {
                std::iter::from_fn(|| requests.try_recv().ok()).any(|edit| matches!(
                edit,
                ModelEdit::SuggestConversationTitle { id: channel, card: pane, expected_revision: 3, title, source: ChannelTitleSource::Prompt }
                    if channel == id && pane == card && title == "Fix the token refresh race"
            ))
            };
        assert!(suggested(&requests));

        // The pane attaching again, as after a reconnect, offers what the
        // read found without reading again.
        let channel = &mut Arc::make_mut(&mut app.durable).channels[0];
        channel.title = String::new();
        app.chat_ui.channel_names.insert(target, unnamed());
        assert!(!reads(app.request_opening_name(target)));
        assert_eq!(named(&app), found);
        assert!(suggested(&requests));

        // So does a new view of the channel, as when the pane becomes its
        // primary chat or its name is cleared elsewhere.
        Arc::make_mut(&mut app.durable).channels[0].title = String::new();
        app.chat_ui.channel_names.insert(
            target,
            ChannelNameObservation {
                last_view: Some((id, 3)),
                ..unnamed()
            },
        );
        app.workspace_model.views[0].revision = 4;
        assert!(
            cosmic::iced::runtime::task::into_stream(app.refresh_channel_title_suggestions())
                .is_none()
        );
        assert_eq!(named(&app), found);
        assert!(
            std::iter::from_fn(|| requests.try_recv().ok()).any(|edit| matches!(
                edit,
                ModelEdit::SuggestConversationTitle {
                    expected_revision: 4,
                    ..
                }
            ))
        );

        // A name the reader gave while the read ran is never replaced.
        let channel = &mut Arc::make_mut(&mut app.durable).channels[0];
        channel.title = "Auth work".into();
        channel.title_source = ChannelTitleSource::Manual;
        app.chat_ui.channel_names.insert(target, unnamed());
        app.opening_name_read(card, ID.into(), Some("Fix the token refresh race".into()));
        assert_eq!(app.durable.channels[0].title, "Auth work");
        assert!(app.chat_ui.channel_names[&target].title.is_none());
        app.chat_ui.opening_names.clear();
        assert!(!reads(app.request_opening_name(target)));
    }
}
