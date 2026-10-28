use super::*;
use crate::work_ui::{Action, Effect, Watch, WatchOwner};
use cosmic::iced::futures::SinkExt;

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
        let watch =
            Subscription::run_with(self.work_ui.watch_owner(), watch_board).map(Message::WorkUi);
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
            Effect::Refresh { ticket } => control(
                |client| client.refresh_work_board(),
                move |result| Action::Refreshed { ticket, result },
            ),
            Effect::Pin {
                ticket,
                project,
                item,
                column,
            } => control(
                move |client| client.pin_work_card(project, item, column),
                move |result| Action::Pinned { ticket, result },
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
fn watch_board(owner: &WatchOwner) -> impl cosmic::iced::futures::Stream<Item = Action> + use<> {
    let owner = *owner;
    cosmic::iced::stream::channel(4, async move |mut output| {
        let (sender, receiver) = async_channel::bounded(1);
        let hangup = Arc::new(Mutex::new(None));
        let held = hangup.clone();
        let spawned = std::thread::Builder::new()
            .name("cagents-work-board".into())
            .spawn(move || {
                while !sender.is_closed() {
                    let watched = watch(&sender, &held);
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
                .send(Action::Watched {
                    owner,
                    watch: Watch::Lost(format!("the board could not be watched: {error}")),
                })
                .await;
            return;
        }
        let _hangup = HangUpOnDrop(hangup);
        while let Ok(watch) = receiver.recv().await {
            if output.send(Action::Watched { owner, watch }).await.is_err() {
                break;
            }
        }
    })
}

fn watch(
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::Hasher;

    fn watch_recipes(app: &Cagents) -> Vec<u64> {
        iced::advanced::subscription::into_recipes(app.work_subscription())
            .into_iter()
            .map(|recipe| {
                let mut hash = iced::advanced::subscription::Hasher::default();
                recipe.hash(&mut hash);
                hash.finish()
            })
            .collect()
    }

    #[test]
    fn work_watch_subscription_retires_with_the_closed_pane_before_reopen() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.refresh_ui.focused = false;
        assert!(watch_recipes(&app).is_empty());
        drop(app.open_utility_card(CardKind::Work));
        let work = app.workspace.focused().unwrap();
        let owner = app.work_ui.watch_owner();
        let before = watch_recipes(&app);
        assert_eq!(before.len(), 1, "inspect exactly the Work watch recipe");
        assert_eq!(before, watch_recipes(&app), "redraw retains its producer");
        drop(app.update_work_ui(Action::Query("retained query".into())));
        drop(app.update_work_ui(Action::Watched {
            owner,
            watch: Watch::Lost("current connection lost".into()),
        }));
        assert_eq!(before, watch_recipes(&app), "reconnect stays in this pane");

        // No subscription tracking occurs between these real close/reopen
        // transitions, as when Iced drains both from one message batch.
        app.workspace.request_close(work).unwrap();
        drop(app.commit_card_removal(work, None).unwrap());
        assert!(watch_recipes(&app).is_empty());
        drop(app.open_utility_card(CardKind::Work));
        let after = watch_recipes(&app);
        assert_eq!(after.len(), 1);
        assert_ne!(owner, app.work_ui.watch_owner());
        assert_ne!(
            before, after,
            "the prior producer must be cancelled even when no empty recipe was tracked"
        );
        assert_eq!(after, watch_recipes(&app));
    }
}
