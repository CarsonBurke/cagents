use super::*;
use crate::work_ui::{Action, Effect, Watch};
use cosmic::iced::futures::SinkExt;

mod transport;

/// How often relative times on the board are redrawn.
const CLOCK: Duration = Duration::from_secs(30);

impl Cagents {
    /// The board subscription runs while this window shows a Work pane, so a
    /// closed pane stops the daemon assembling boards for it.
    pub(super) fn work_subscription(&self) -> Subscription<Message> {
        let open = self
            .durable
            .cards
            .iter()
            .any(|card| card.kind == CardKind::Work && card.existence.is_open());
        if !open {
            return Subscription::none();
        }
        let watch = Subscription::run(watch_board).map(Message::WorkUi);
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
    let context = transport::Context::capture();
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            let mut client = context.connect(true)?;
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
fn watch_board() -> impl cosmic::iced::futures::Stream<Item = Action> {
    let context = transport::Context::capture();
    cosmic::iced::stream::channel(4, async move |mut output| {
        let (sender, receiver) = async_channel::bounded(1);
        let hangup = Arc::new(Mutex::new(None));
        let held = hangup.clone();
        let spawned = std::thread::Builder::new()
            .name("cagents-work-board".into())
            .spawn(move || {
                while !sender.is_closed() {
                    let watched = watch(&context, &sender, &held);
                    // A connection this side gave up on must not stay open
                    // through the retry delay, where the daemon would keep
                    // queueing boards for it.
                    if let Some(hangup) = held.lock().unwrap_or_else(|p| p.into_inner()).take() {
                        hangup.hang_up();
                    }
                    if let Err(error) = watched
                        && sender.send_blocking(Watch::Lost(error)).is_err()
                    {
                        break;
                    }
                    std::thread::sleep(crate::daemon_health::retry_delay());
                }
            });
        if let Err(error) = spawned {
            let _ = output
                .send(Action::Watched(Watch::Lost(format!(
                    "the board could not be watched: {error}"
                ))))
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
    context: &transport::Context,
    sender: &async_channel::Sender<Watch>,
    hangup: &Mutex<Option<crate::control::Hangup>>,
) -> Result<(), String> {
    // Never start a daemon merely to watch it; the window starts one as soon
    // as it needs one, and this reconnects then.
    let mut client = context.connect(false)?;
    *hangup.lock().unwrap_or_else(|p| p.into_inner()) =
        Some(client.hangup().map_err(|error| error.to_string())?);
    // The pane closed while this connected.
    if sender.is_closed() {
        return Ok(());
    }
    let (mut revision, board) = client.watch_work().map_err(|error| error.to_string())?;
    sender
        .send_blocking(Watch::Board {
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
