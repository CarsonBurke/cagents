use super::*;

#[derive(Debug, Clone)]
pub enum Destination {
    Local(TerminalId),
    Saved(crate::persistence::SavedConversation),
    Window(String, TerminalId),
}

fn unique_destination(
    prefix: &str,
    candidates: Vec<(TerminalId, Destination)>,
) -> Result<Destination, String> {
    let mut matches = candidates
        .into_iter()
        .filter(|(session, _)| session.to_string().starts_with(prefix));
    match (matches.next(), matches.next()) {
        (Some((_, destination)), None) => Ok(destination),
        (None, _) => Err(format!("Session {prefix} is no longer available.")),
        _ => Err(format!("More than one session starts with {prefix}.")),
    }
}

impl Cagents {
    pub(super) fn open_session_link(&mut self, prefix: String) -> Task<Message> {
        if let Ok(session) = prefix.parse::<TerminalId>()
            && self.durable.cards.iter().any(|card| {
                card.existence.is_open() && notification_ui::card_session(card) == Some(session)
            })
        {
            return self.session_link_resolved(Ok(Destination::Local(session)));
        }
        // Include all windows before accepting an abbreviated identity. A
        // locally unique prefix can still identify another window's session.
        let mut local = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .filter_map(notification_ui::card_session)
            .map(|id| (id, Destination::Local(id)))
            .collect::<Vec<_>>();
        for card in self
            .durable
            .cards
            .iter()
            .filter(|card| !card.existence.is_open())
            .chain(self.recently_closed.iter().map(|(card, _)| card))
        {
            if let CardKind::Chat(session) = &card.kind
                && session.launch.session_id.is_some()
                && !local.iter().any(|(id, _)| *id == session.id)
            {
                local.push((
                    session.id,
                    Destination::Saved(crate::persistence::SavedConversation {
                        card: card.id,
                        title: card.title.clone(),
                        session: session.clone(),
                        open: false,
                        updated_at_unix_ms: 0,
                    }),
                ));
            }
        }
        let store = self.session.as_ref().map(SessionRuntime::operation_store);
        let current_path = self
            .session
            .as_ref()
            .map(|session| session.database_path().to_owned());
        let current_slot = instance::window_slot().to_owned();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let mut candidates = local;
                if let Some(store) = store {
                    for entry in store
                        .saved_conversations()
                        .map_err(|error| error.to_string())?
                    {
                        if !entry.open && !candidates.iter().any(|(id, _)| *id == entry.session.id)
                        {
                            candidates.push((entry.session.id, Destination::Saved(entry)));
                        }
                    }
                }
                for path in instance::window_database_paths().map_err(|error| error.to_string())? {
                    if current_path.as_ref() == Some(&path) || !path.is_file() {
                        continue;
                    }
                    let slot = if path
                        == instance::window_database_path(instance::PRIMARY_WINDOW)
                            .map_err(|error| error.to_string())?
                    {
                        instance::PRIMARY_WINDOW.to_owned()
                    } else {
                        path.parent()
                            .and_then(Path::file_name)
                            .and_then(|name| name.to_str())
                            .ok_or_else(|| "Invalid saved window directory.".to_owned())?
                            .to_owned()
                    };
                    if slot == current_slot
                        || !instance::window_matches_selected_device(&slot)
                            .map_err(|error| error.to_string())?
                    {
                        continue;
                    }
                    for id in crate::persistence::read_navigable_session_ids(&path)
                        .map_err(|error| error.to_string())?
                    {
                        candidates.push((id, Destination::Window(slot.clone(), id)));
                    }
                }
                unique_destination(&prefix, candidates)
            })
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
            Message::ChatUi(Action::SessionResolved(result))
        })
    }

    pub(super) fn session_link_resolved(
        &mut self,
        result: Result<Destination, String>,
    ) -> Task<Message> {
        match result {
            Ok(Destination::Local(session)) => self.update_notifications(
                notification_ui::Action::Activated(crate::notifications::Target::Session(session)),
            ),
            Ok(Destination::Saved(entry)) => {
                if self.modal_is_open() {
                    self.status = Some(TransientNotice::warning(
                        "Finish the open dialog, then open the session link again.",
                    ));
                    self.activate_main_window(None)
                } else {
                    Task::batch([
                        self.resume_saved_conversation(entry),
                        self.activate_main_window(None),
                    ])
                }
            }
            Ok(Destination::Window(slot, session)) => {
                let Some(window) = self.core.main_window_id() else {
                    return handoff_session_window(slot, session, None);
                };
                iced::window::run_with_handle(window, |handle| {
                    matches!(
                        handle.as_raw(),
                        iced::runtime::window::raw_window_handle::RawWindowHandle::Wayland(_)
                    )
                })
                .map(move |wayland| {
                    cosmic::Action::App(Message::ChatUi(Action::SessionLinkWindowBackend(
                        slot.clone(),
                        session,
                        wayland,
                    )))
                })
            }
            Err(error) => {
                self.status = Some(TransientNotice::warning(error));
                Task::none()
            }
        }
    }
    pub(super) fn session_window_backend(
        &self,
        slot: String,
        session: TerminalId,
        wayland: bool,
    ) -> Task<Message> {
        if !wayland {
            return handoff_session_window(slot, session, None);
        }
        use iced::runtime::{Action as RuntimeAction, platform_specific};
        let window = self.core.main_window_id();
        iced::runtime::task::oneshot(move |channel| {
            RuntimeAction::PlatformSpecific(platform_specific::Action::Wayland(
                platform_specific::wayland::Action::Activation(
                    platform_specific::wayland::activation::Action::RequestToken {
                        app_id: Some(instance::APPLICATION_ID.to_owned()),
                        window,
                        channel,
                    },
                ),
            ))
        })
        .map(move |token| {
            cosmic::Action::App(Message::ChatUi(Action::SessionLinkToken(
                slot.clone(),
                session,
                token,
            )))
        })
    }
}

pub(super) fn handoff_session_window(
    slot: String,
    session: TerminalId,
    token: Option<String>,
) -> Task<Message> {
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            if instance::activate_window_session(&slot, session, token.as_deref())
                .map_err(|error| error.to_string())?
            {
                return Ok(());
            }
            instance::launch_window(false, Some(&slot), None).map_err(|error| error.to_string())?;
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if instance::activate_window_session(&slot, session, token.as_deref())
                    .map_err(|error| error.to_string())?
                {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err("The session's window did not finish opening.".to_owned());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string()));
        // A successful handoff leaves the originating pane in place.
        Message::ChatUi(Action::SessionLinkOpened(result))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_window_link_requests_a_token_for_its_native_origin_window() {
        use cosmic::iced::futures::StreamExt as _;
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let window = window::Id::unique();
        app.core.set_main_window_id(Some(window));
        let session = TerminalId::new();
        let task = app.session_window_backend("peer".into(), session, true);
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let mut stream = iced::runtime::task::into_stream(task).unwrap();
            let Some(iced::runtime::Action::PlatformSpecific(iced::runtime::platform_specific::Action::Wayland(
                iced::runtime::platform_specific::wayland::Action::Activation(
                    iced::runtime::platform_specific::wayland::activation::Action::RequestToken { app_id, window: origin, channel }
                )
            ))) = stream.next().await else { panic!("expected native activation token request"); };
            assert_eq!(origin, Some(window));
            assert_eq!(app_id.as_deref(), Some(instance::APPLICATION_ID));
            channel.send(Some("click-token".into())).unwrap();
            assert!(matches!(stream.next().await,
                Some(iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(Action::SessionLinkToken(slot, id, Some(token))))))
                if slot == "peer" && id == session && token == "click-token"));
        });
    }

    #[test]
    fn abbreviated_session_ids_are_resolved_across_windows() {
        let first = TerminalId::from_uuid(
            uuid::Uuid::parse_str("12345678-0000-0000-0000-000000000001").unwrap(),
        );
        let second = TerminalId::from_uuid(
            uuid::Uuid::parse_str("12345678-0000-0000-0000-000000000002").unwrap(),
        );
        let candidates = vec![
            (first, Destination::Local(first)),
            (second, Destination::Window("other".into(), second)),
        ];
        assert!(
            unique_destination("12345678", candidates.clone())
                .unwrap_err()
                .contains("More than one")
        );
        assert!(
            matches!(unique_destination(&second.to_string(), candidates), Ok(Destination::Window(slot, id)) if slot == "other" && id == second)
        );
        assert!(
            unique_destination("missing", Vec::new())
                .unwrap_err()
                .contains("no longer available")
        );
    }
}
