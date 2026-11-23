//! A pane that another window or an agent moves out of the channel this
//! window shows.
//!
//! The shared model moves the pane, and this window applies the move as it
//! applies any other client's edit. Left at that, a window reading the pane
//! would be left on the channel it came from, often now empty, with the only
//! clue an unread mark in the sidebar. So a window that was showing the pane
//! goes with it and says where it went, offering the way back; a window that
//! showed the channel but was reading another pane stays where it is and
//! offers to open the pane where it went.
use super::*;
use crate::model::ChannelId;
use crate::workspace_model::ModelEdit;
use crate::workspace_model::views::{Change, Scope};

/// How long the notice stays while the window is in front: long enough to
/// read and act on a move the person did not make.
const LIFETIME: Duration = Duration::from_secs(15);

/// The pane a peer's transfer took out of the channel this window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Departure {
    pub(super) card: CardId,
    pub(super) from: ChannelId,
    pub(super) to: ChannelId,
    /// This window was showing that very pane, so it follows it.
    pub(super) follow: bool,
}

#[derive(Clone, Debug)]
pub(super) struct Notice {
    departure: Departure,
    /// What the notice says, fixed when the pane moved so that a channel
    /// renamed later doesn't rewrite it under the reader.
    text: String,
    deadline: Instant,
}

impl Notice {
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    #[cfg(test)]
    pub(super) fn deadline(&self) -> Instant {
        self.deadline
    }
}

#[derive(Clone, Debug)]
pub enum Action {
    /// Back to the channel the pane left, after following it.
    GoBack,
    /// To the pane in its new channel, after staying behind.
    Open,
    Dismiss,
    Expire,
}

#[derive(Hash)]
struct Expiry(Instant);

fn expiry_stream(expiry: &Expiry) -> impl cosmic::iced::futures::Stream<Item = Message> + use<> {
    let deadline = tokio::time::Instant::from_std(expiry.0);
    cosmic::iced::futures::stream::once(async move {
        tokio::time::sleep_until(deadline).await;
        Message::MovedPane(Action::Expire)
    })
}

impl Cagents {
    /// The departure `edit` would make from the channel this window shows,
    /// read before the edit is applied. Only another client's transfer
    /// counts: this window's own move already shows the pane where the
    /// person put it, and its late echo finds the pane gone from here.
    pub(super) fn pane_departure(&self, edit: &ModelEdit) -> Option<Departure> {
        let ModelEdit::ChangeView {
            scope: Scope::Channel(from),
            operation,
            change:
                Change::Transfer {
                    id: card,
                    destination: Scope::Channel(to),
                    ..
                },
            ..
        } = edit
        else {
            return None;
        };
        if from == to
            || self.durable.active_channel != Some(*from)
            || self.shared_views.is_requested_transfer(*operation)
            || self
                .shared_views
                .pending
                .iter()
                .any(|(pending, _, _)| pending == operation)
        {
            return None;
        }
        let shown = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == *from)
            .is_some_and(|channel| channel.cards.contains(card));
        shown.then(|| Departure {
            card: *card,
            from: *from,
            to: *to,
            follow: self.workspace.focused() == Some(*card) && self.may_follow(),
        })
    }

    /// Whether the window may change channel on its own: not while it is
    /// closing, saving or held still for an update, as when it goes to a
    /// pane asking for attention.
    fn may_follow(&self) -> bool {
        self.exit.is_none()
            && !self.workspace_inputs_blocked()
            && !self.focused_repository_is_saving()
            && !self.update_prepared
    }

    /// After the transfer is applied: follows the pane when this window was
    /// showing it, and says where it went either way.
    pub(super) fn pane_departed(&mut self, mut departure: Departure) -> Task<Message> {
        // A channel this window cannot show is not followed: it stays here
        // and offers the pane instead, without touching the sidebar first.
        departure.follow &= self.channel_directory(departure.to).is_some();
        let Some(destination) = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == departure.to)
            .filter(|channel| channel.cards.contains(&departure.card))
            .cloned()
        else {
            // The pane is not where the edit put it, as when a later edit
            // already moved it on: there is nothing true to say.
            return Task::none();
        };
        let place = channel_ui::channel_display_title(
            &destination,
            self.sidebar_model.project(destination.project_id),
        );
        let task = if departure.follow {
            Arc::make_mut(&mut self.durable)
                .channels
                .iter_mut()
                .find(|channel| channel.id == departure.to)
                .expect("found above")
                .focused = Some(departure.card);
            self.reveal_channel_card(departure.to, departure.card)
        } else {
            Task::none()
        };
        let text = if departure.follow {
            format!("Moved to {place}")
        } else {
            // The pane's own name: a chat leading a named channel is shown
            // by that channel's title, which here would read as the
            // destination moving into itself.
            let pane = self
                .card_state(departure.card)
                .map(|card| self.card_display_title_with_chat_title(card, || None))
                .filter(|title| !title.is_empty())
                .unwrap_or("A pane");
            format!("{pane} moved to {place}")
        };
        self.pane_follow = Some(Notice {
            departure,
            text,
            deadline: Instant::now() + LIFETIME,
        });
        task
    }

    fn reveal_channel_card(&mut self, channel: ChannelId, card: CardId) -> Task<Message> {
        self.select_sidebar_rows(
            sidebar::SelectionRow::Channel(channel),
            sidebar::SelectionIntent::Navigate,
        );
        let scope = self.navigate_to_channel(channel);
        let focus = self.choose_workspace_card(card);
        Task::batch([scope, focus])
    }

    pub(super) fn update_pane_follow(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Expire => {
                let now = Instant::now();
                match &mut self.pane_follow {
                    // Behind another window nobody is reading it yet.
                    Some(notice) if !self.refresh_ui.focused => notice.deadline = now + LIFETIME,
                    Some(notice) if notice.deadline <= now => self.pane_follow = None,
                    _ => {}
                }
                Task::none()
            }
            Action::Dismiss => {
                self.pane_follow = None;
                Task::none()
            }
            Action::GoBack => {
                let Some(notice) = self.pane_follow.take() else {
                    return Task::none();
                };
                // As the sidebar opens it: a channel the move emptied and
                // retired is gone, and so is the way back.
                let from = notice.departure.from;
                if !self
                    .durable
                    .channels
                    .iter()
                    .any(|channel| channel.id == from)
                {
                    return Task::none();
                }
                self.select_sidebar_rows(
                    sidebar::SelectionRow::Channel(from),
                    sidebar::SelectionIntent::Navigate,
                );
                self.handle_channel_ui(channel_ui::Action::Open(from))
            }
            Action::Open => {
                let Some(notice) = self.pane_follow.take() else {
                    return Task::none();
                };
                let Departure { card, to, .. } = notice.departure;
                if channels_holding(&self.durable.channels, card).any(|channel| channel.id == to) {
                    self.reveal_channel_card(to, card)
                } else {
                    Task::none()
                }
            }
        }
    }

    pub(super) fn pane_follow_notice(&self) -> Option<&Notice> {
        self.pane_follow.as_ref()
    }

    /// Run after every update: the notice goes once it no longer describes
    /// this window. That is when the person goes to another channel, or,
    /// for a pane left elsewhere, when that pane leaves the channel the
    /// notice names and Open would have nowhere true to go.
    pub(super) fn retire_pane_follow_notice(&mut self) {
        let Some(notice) = &self.pane_follow else {
            return;
        };
        let Departure {
            card,
            from,
            to,
            follow,
        } = notice.departure;
        let here = if follow { to } else { from };
        let current = self.durable.active_channel == Some(here)
            && (follow
                || channels_holding(&self.durable.channels, card).any(|channel| channel.id == to));
        if !current {
            self.pane_follow = None;
        }
    }

    /// The countdown runs only while the person can see the notice, so
    /// coming back to the window gives them the whole of it.
    pub(super) fn pane_follow_focus_regained(&mut self) {
        if let Some(notice) = &mut self.pane_follow {
            notice.deadline = Instant::now() + LIFETIME;
        }
    }

    pub(super) fn pane_follow_subscription(&self) -> Subscription<Message> {
        self.pane_follow
            .as_ref()
            .map_or_else(Subscription::none, |notice| {
                Subscription::run_with(Expiry(notice.deadline), expiry_stream)
            })
    }

    pub(super) fn pane_follow_banner(&self) -> Element<'_, Message> {
        let Some(notice) = self.pane_follow_notice() else {
            return iced::widget::Space::new().height(Length::Shrink).into();
        };
        let severity = NoticeSeverity::Success;
        let enabled = !self.modal_is_open();
        let (label, name, action) = if notice.departure.follow {
            (
                "Go back",
                "Go back to the channel the pane left",
                Action::GoBack,
            )
        } else {
            ("Open", "Open the pane in its new channel", Action::Open)
        };
        let back = self
            .durable
            .channels
            .iter()
            .any(|channel| channel.id == notice.departure.from);
        let mut actions = Vec::new();
        if back || !notice.departure.follow {
            actions.push(
                widget::button::text(label)
                    .id(widget::Id::new("pane-follow.action"))
                    .name(name)
                    .class(notice_surface_button(severity))
                    .on_press_maybe(enabled.then_some(Message::MovedPane(action)))
                    .into(),
            );
        }
        actions.push(crate::interaction::tooltip(
            widget::button::icon(Icon::Close)
                .id(widget::Id::new("pane-follow.dismiss"))
                .name("Dismiss the notice")
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .class(notice_surface_button(severity))
                .on_press_maybe(enabled.then_some(Message::MovedPane(Action::Dismiss))),
            "Dismiss",
        ));
        notice_surface(
            severity,
            design::body(notice.text()).width(Length::Fill).into(),
            Some(
                widget::row::with_children(actions)
                    .spacing(space::XS)
                    .align_y(Alignment::Center)
                    .into(),
            ),
            true,
        )
    }
}
