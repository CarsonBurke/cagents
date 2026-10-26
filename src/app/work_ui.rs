use super::*;
use crate::work_ui::{Action, Effect, Watch};
use cosmic::iced::futures::SinkExt;

/// How often relative times on the board are redrawn.
const CLOCK: Duration = Duration::from_secs(30);

impl Cagents {
    /// Called before both constructor returns and in the common update
    /// postlude, after the workspace's actual visibility is current.
    pub(super) fn sync_work_watch_visibility(&mut self) {
        let visible = self.workspace_surfaces_visible()
            && self.workspace.mounted_visibility().any(|(id, visibility)| {
                visibility == VisibilityClass::Visible
                    && matches!(self.cards.get(&id), Some(CardRuntime::Work))
                    && self
                        .card_state(id)
                        .is_some_and(|card| card.kind == CardKind::Work && card.existence.is_open())
            });
        self.work_ui.set_listening(visible);
    }

    /// Only an initialized visible listener holds daemon demand. Window and
    /// keyboard focus do not hide a side-by-side Work pane.
    pub(super) fn work_subscription(&self) -> Subscription<Message> {
        let Some(epoch) = self.work_ui.listening_epoch() else {
            return Subscription::none();
        };
        let watch = Subscription::run_with(epoch, watch_board).map(Message::WorkUi);
        if self.refresh_ui.focused {
            Subscription::batch([
                watch,
                cosmic::iced::time::every(CLOCK).map(|_| Message::WorkUi(Action::Tick)),
            ])
        } else {
            watch
        }
    }

    pub(super) fn update_work_ui(&mut self, action: Action) -> Task<Message> {
        let Some(effect) = self.work_ui.update(action) else {
            return Task::none();
        };
        match effect {
            Effect::Refresh => control(
                |client| client.refresh_work_board(),
                |result| Action::Refreshed(result),
            ),
            Effect::Pin {
                project,
                item,
                column,
            } => control(
                move |client| client.pin_work_card(project, item, column),
                |result| Action::Pinned(result),
            ),
            Effect::OpenWorktree { project, path } => {
                let opened = self
                    .sidebar_model
                    .projects
                    .iter()
                    .find(|candidate| candidate.repository == project)
                    .and_then(|project| {
                        let worktree = project
                            .worktrees
                            .iter()
                            .find(|worktree| worktree.path == path)?;
                        Some(SidebarAction::OpenWorktree {
                            project_id: project.id,
                            worktree_id: worktree.id,
                            path: worktree.path.clone(),
                        })
                    });
                match opened {
                    Some(action) => self.update(Message::Sidebar(action)),
                    // A worktree the sidebar has not listed yet still opens.
                    None => self.select_worktree(path, false),
                }
            }
            Effect::OpenPullRequest {
                project,
                number,
                url,
            } => {
                let Ok(url) = Url::parse(&url) else {
                    return Task::none();
                };
                match self
                    .sidebar_model
                    .projects
                    .iter()
                    .find(|candidate| candidate.repository == project)
                {
                    Some(project) => {
                        self.update(Message::Sidebar(SidebarAction::OpenPullRequest {
                            project_id: project.id,
                            number,
                            url,
                        }))
                    }
                    None => self.open_clicked_link(url),
                }
            }
            Effect::OpenUrl(url) => match Url::parse(&url) {
                Ok(url) if matches!(url.scheme(), "https" | "http") => self.open_clicked_link(url),
                _ => Task::none(),
            },
            // Session links find the session in whichever window shows it.
            Effect::FocusSession(session) => {
                self.handle_chat_ui(chat_ui::Action::OpenSession(session.to_string()))
            }
            Effect::Copy(text) => {
                self.status = Some(TransientNotice::success(format!("Copied {text}")));
                iced::clipboard::write(text).map(cosmic::Action::App)
            }
        }
    }
}

/// Runs one blocking daemon request off the UI thread.
fn control(
    request: impl FnOnce(&mut crate::control::ControlClient) -> Result<(), crate::control::ControlError>
    + Send
    + 'static,
    answer: impl FnOnce(Result<(), String>) -> Action + Send + 'static,
) -> Task<Message> {
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            let socket =
                crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
            let mut client = crate::control::ControlClient::connect(&socket)
                .map_err(|error| error.to_string())?;
            request(&mut client).map_err(|error| error.to_string())
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string()));
        Message::WorkUi(answer(result))
    })
}

/// The daemon's boards, from a thread that holds the subscription.
///
/// Dropping the stream, which iced does when the last Work pane closes, hangs
/// up the connection: the thread's blocking read then fails, it finds no one
/// listening, and the daemon forgets the watcher.
fn watch_board(epoch: &u64) -> impl cosmic::iced::futures::Stream<Item = Action> + use<> {
    let epoch = *epoch;
    cosmic::iced::stream::channel(4, async move |mut output| {
        let (sender, receiver) = async_channel::bounded(1);
        let hangup = Arc::new(Mutex::new(None));
        let held = hangup.clone();
        let spawned = std::thread::Builder::new()
            .name("cagents-work-board".into())
            .spawn(move || {
                while !sender.is_closed() {
                    let watched = watch(epoch, &sender, &held);
                    // A connection this side gave up on must not stay open
                    // through the retry delay, where the daemon would keep
                    // queueing boards for it.
                    if let Some(hangup) = held.lock().unwrap_or_else(|p| p.into_inner()).take() {
                        hangup.hang_up();
                    }
                    if sender.is_closed() {
                        break;
                    }
                    if let Err(error) = watched
                        && sender.send_blocking(Watch::Lost { epoch, error }).is_err()
                    {
                        break;
                    }
                    std::thread::sleep(crate::daemon_health::retry_delay());
                }
            });
        if let Err(error) = spawned {
            let _ = output
                .send(Action::Watched(Watch::Lost {
                    epoch,
                    error: format!("the board could not be watched: {error}"),
                }))
                .await;
            return;
        }
        let _hangup = HangUpOnDrop(hangup);
        while let Ok(watch) = receiver.recv().await {
            if output.send(Action::Watched(watch)).await.is_err() {
                break;
            }
        }
    })
}

fn watch(
    epoch: u64,
    sender: &async_channel::Sender<Watch>,
    hangup: &Mutex<Option<crate::control::Hangup>>,
) -> Result<(), String> {
    // Never start a daemon merely to watch it; the window starts one as soon
    // as it needs one, and this reconnects then.
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client =
        crate::control::ControlClient::connect(&socket).map_err(|error| error.to_string())?;
    *hangup.lock().unwrap_or_else(|p| p.into_inner()) =
        Some(client.hangup().map_err(|error| error.to_string())?);
    // The pane closed while this connected.
    if sender.is_closed() {
        return Ok(());
    }
    let (mut revision, board) = client.watch_work().map_err(|error| error.to_string())?;
    sender
        .send_blocking(Watch::Board {
            epoch,
            revision,
            board: Arc::new(board),
            first: true,
        })
        .map_err(|_| "the Work pane closed".to_owned())?;
    loop {
        let (next, board) = client
            .next_work_board(revision)
            .map_err(|error| error.to_string())?;
        revision = next;
        sender
            .send_blocking(Watch::Board {
                epoch,
                revision,
                board: Arc::new(board),
                first: false,
            })
            .map_err(|_| "the Work pane closed".to_owned())?;
    }
}

struct HangUpOnDrop(Arc<Mutex<Option<crate::control::Hangup>>>);

impl Drop for HangUpOnDrop {
    fn drop(&mut self) {
        if let Some(hangup) = self.0.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            hangup.hang_up();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_visible_work_keeps_its_listener_without_keyboard_or_window_focus() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.context_inspector_open = false;
        app.window_width = 1_800.0;
        app.workspace.resize_viewport(1_400.0);
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), None);
        drop(app.add_utility_card(CardKind::Work));
        let work = app.workspace.focused().unwrap();
        app.workspace
            .set_card_width(work, CardWidth::Fixed(600.0))
            .unwrap();
        app.workspace.native_scroll(0.0, 1_400.0);
        assert_eq!(
            app.workspace.visibility(work),
            Some(VisibilityClass::Visible)
        );
        // This is the exported constructor hook, before any App message.
        app.sync_work_watch_visibility();
        let original = app.work_ui.listening_epoch().unwrap();

        drop(app.add_utility_card(CardKind::Settings));
        let settings = app.workspace.focused().unwrap();
        app.workspace
            .set_card_width(settings, CardWidth::Fixed(600.0))
            .unwrap();
        app.workspace.native_scroll(0.0, 1_400.0);
        app.workspace.focus_without_scrolling(settings).unwrap();
        assert_ne!(work, settings);
        assert_eq!(
            app.workspace.visibility(work),
            Some(VisibilityClass::Visible)
        );
        app.refresh_ui.focused = false;
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), Some(original));
        app.refresh_ui.focused = true;
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), Some(original));

        // A real camera/viewport change leaves the still-open Work offscreen.
        app.workspace.native_scroll(1_000.0, 500.0);
        assert_ne!(
            app.workspace.visibility(work),
            Some(VisibilityClass::Visible)
        );
        assert!(app.card_state(work).unwrap().existence.is_open());
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), None);
        app.workspace.native_scroll(0.0, 1_400.0);
        app.sync_work_watch_visibility();
        let revealed = app.work_ui.listening_epoch().unwrap();
        assert!(revealed > original);

        // Current Carson's overlay helper, not the removed main-view switch.
        app.window_width = 400.0;
        app.context_inspector_open = true;
        assert!(app.context_inspector_is_overlay());
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), None);
        app.context_inspector_open = false;
        app.sync_work_watch_visibility();
        assert!(app.work_ui.listening_epoch().unwrap() > revealed);
    }

    #[test]
    fn a_visible_failed_work_placeholder_has_no_listener_until_its_runtime_is_work() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.context_inspector_open = false;
        app.window_width = 1_800.0;
        app.workspace.resize_viewport(1_400.0);
        drop(app.add_utility_card(CardKind::Work));
        let work = app.workspace.focused().unwrap();
        app.workspace.native_scroll(0.0, 1_400.0);
        assert_eq!(
            app.workspace.visibility(work),
            Some(VisibilityClass::Visible)
        );
        assert!(app.card_state(work).unwrap().existence.is_open());
        assert!(matches!(app.cards.get(&work), Some(CardRuntime::Work)));

        // The synthetic latency constructor makes this exact replacement:
        // durable kind and on-screen geometry alone do not render the board.
        app.cards.insert(
            work,
            CardRuntime::Failed("Synthetic failure placeholder".into()),
        );
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), None);
        assert_eq!(
            app.workspace.visibility(work),
            Some(VisibilityClass::Visible)
        );
        assert!(app.card_state(work).unwrap().existence.is_open());

        app.cards.insert(work, CardRuntime::Work);
        app.sync_work_watch_visibility();
        let original = app.work_ui.listening_epoch().unwrap();
        app.cards
            .insert(work, CardRuntime::Failed("Runtime replaced".into()));
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), None);
        app.cards.insert(work, CardRuntime::Work);
        app.sync_work_watch_visibility();
        assert!(app.work_ui.listening_epoch().unwrap() > original);
        app.refresh_ui.focused = false;
        app.sync_work_watch_visibility();
        assert!(app.work_ui.listening_epoch().is_some());
    }
}
