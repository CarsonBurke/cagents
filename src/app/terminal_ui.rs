use super::*;

#[derive(Clone, Debug)]
pub enum Action {
    Open(CardId),
    Close(CardId),
    Query(CardId, String),
    Case(CardId),
    Next(CardId, bool),
    Focus(CardId, bool),
    Context(
        CardId,
        iced::Point,
        Option<crate::terminal::SearchMatch>,
        Option<Url>,
    ),
    DismissContext(CardId),
    Copy(CardId, String),
    CopySelection(CardId, crate::terminal::SearchMatch),
    PasteClipboard(CardId),
    Paste(CardId, String),
    ConfirmPaste(CardId, bool),
    SelectAll(CardId),
    Clear(CardId),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closed_find_rejects_delayed_results_for_its_current_request() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let card = CardId::new();
        app.terminal_find.insert(
            card,
            Find {
                query: "needle".into(),
                request: 7,
                ..Find::default()
            },
        );
        app.terminal_search_results(
            card,
            7,
            "needle".into(),
            vec![crate::terminal::SearchMatch {
                start_row: 12,
                start_column: 0,
                end_row: 12,
                end_column: 6,
            }],
            true,
        );
        let find = &app.terminal_find[&card];
        assert!(find.matches.is_empty());
        assert!(!find.truncated);
    }

    #[test]
    fn paste_confirmation_dismisses_after_its_runtime_has_retired() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let card = CardId::new();
        app.terminal_paste = Some((card, "command\ncommand".into()));
        app.cards.insert(card, CardRuntime::Loading);
        drop(app.handle_terminal_find(Action::ConfirmPaste(card, false)));
        assert!(app.terminal_paste.is_none());
    }
}

#[derive(Default)]
pub(super) struct Find {
    pub open: bool,
    query: String,
    request: u64,
    case_sensitive: bool,
    pub matches: Arc<[crate::terminal::SearchMatch]>,
    pub selected: usize,
    truncated: bool,
    pub focused: bool,
    pub select_all_request: u64,
    pub selection_reset: u64,
    pub pending_copy: Option<uuid::Uuid>,
}

pub(super) struct Context {
    pub card: CardId,
    pub selection: Option<crate::terminal::SearchMatch>,
    pub link: Option<Url>,
    pub point: iced::Point,
}

fn input_id(card: CardId) -> iced::widget::Id {
    iced::widget::Id::from(format!("terminal-find-{card}"))
}

impl Cagents {
    pub(super) fn handle_terminal_find(&mut self, action: Action) -> Task<Message> {
        if let Action::ConfirmPaste(card, confirmed) = &action {
            let pending = self.terminal_paste.take();
            if *confirmed
                && let Some((target, text)) = pending
                && target == *card
                && ended_terminal(self.card_state(*card)).is_none()
                && !self.terminal_close_pending(*card)
                && let Some(CardRuntime::Terminal(handle)) = self.cards.get(card)
            {
                let _ = handle.paste(&text, crate::terminal::PasteOrigin::Clipboard);
            }
            return iced::widget::operation::focus(workspace_widget::card_focus_id(*card))
                .map(cosmic::Action::App);
        }
        let card = match &action {
            Action::Open(card)
            | Action::Close(card)
            | Action::Query(card, _)
            | Action::Case(card)
            | Action::Next(card, _)
            | Action::Focus(card, _)
            | Action::Context(card, ..)
            | Action::DismissContext(card)
            | Action::Copy(card, _)
            | Action::CopySelection(card, _)
            | Action::PasteClipboard(card)
            | Action::Paste(card, _)
            | Action::ConfirmPaste(card, _)
            | Action::SelectAll(card)
            | Action::Clear(card) => *card,
        };
        let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card) else {
            return Task::none();
        };
        let opening = matches!(&action, Action::Open(_));
        let restore_query_focus = opening || matches!(&action, Action::Case(_));
        let find = self.terminal_find.entry(card).or_default();
        match action {
            Action::Context(_, point, selection, link) => {
                self.terminal_context = Some(Context {
                    card,
                    selection,
                    link,
                    point,
                });
                return Task::none();
            }
            Action::DismissContext(_) => {
                self.terminal_context = None;
                return Task::none();
            }
            Action::Copy(_, text) => {
                self.terminal_context = None;
                return iced::clipboard::write(text).map(cosmic::Action::App);
            }
            Action::CopySelection(_, selection) => {
                self.terminal_context = None;
                let request = uuid::Uuid::new_v4();
                find.pending_copy = Some(request);
                if let Err(error) = handle.copy_selection(request, selection) {
                    return cosmic::task::message(Message::TerminalError {
                        card,
                        message: error.to_string(),
                    });
                }
                return Task::none();
            }
            Action::PasteClipboard(_) => {
                self.terminal_context = None;
                return iced::clipboard::read().and_then(move |text| {
                    cosmic::task::message(Message::TerminalFind(Action::Paste(card, text)))
                });
            }
            Action::Paste(_, text) => {
                self.terminal_context = None;
                if ended_terminal(self.card_state(card)).is_some()
                    || self.terminal_close_pending(card)
                {
                    return Task::none();
                }
                if self.settings.confirm_multiline_paste
                    && !handle.snapshot().scroll.bracketed_paste
                    && text.contains(['\n', '\r'])
                {
                    self.terminal_paste = Some((card, text));
                    return iced::widget::operation::focus(iced::widget::Id::from(
                        "terminal-paste-cancel",
                    ))
                    .map(cosmic::Action::App);
                }
                let _ = handle.paste(&text, crate::terminal::PasteOrigin::Clipboard);
                return Task::none();
            }
            Action::ConfirmPaste(..) => {
                unreachable!("paste confirmation handled before runtime lookup")
            }
            Action::SelectAll(_) => {
                self.terminal_context = None;
                find.select_all_request = find.select_all_request.wrapping_add(1);
                return Task::none();
            }
            Action::Clear(_) => {
                self.terminal_context = None;
                let _ = handle.clear_scrollback();
                // History rows are rebased by clearing scrollback.
                find.selection_reset = find.selection_reset.wrapping_add(1);
                find.request = find.request.wrapping_add(1);
                find.matches = Arc::default();
                find.selected = 0;
                find.truncated = false;
                if find.open {
                    let _ = handle.search(find.request, find.query.clone(), find.case_sensitive);
                }
                return Task::none();
            }
            Action::Open(_) => {
                self.terminal_context = None;
                find.open = true;
                // Reopening must search the current live or retained history, rather
                // than reuse matches from before output changed or completion.
            }
            Action::Close(_) => {
                find.open = false;
                find.focused = false;
                return iced::widget::operation::focus(workspace_widget::card_focus_id(card))
                    .map(cosmic::Action::App);
            }
            Action::Focus(_, focused) => {
                find.focused = focused;
                return Task::none();
            }
            Action::Next(_, backwards) => {
                find.focused = true;
                if !find.matches.is_empty() {
                    find.selected = if backwards {
                        (find.selected + find.matches.len() - 1) % find.matches.len()
                    } else {
                        (find.selected + 1) % find.matches.len()
                    };
                    let _ = handle.scroll_to_row(find.matches[find.selected].start_row);
                }
                return iced::widget::operation::focus(input_id(card)).map(cosmic::Action::App);
            }
            Action::Query(_, query) => find.query = query,
            Action::Case(_) => find.case_sensitive = !find.case_sensitive,
        }
        find.request = find.request.wrapping_add(1);
        find.matches = Arc::default();
        find.selected = 0;
        find.truncated = false;
        let focus = if restore_query_focus {
            find.focused = true;
            let focus = iced::widget::operation::focus(input_id(card));
            if opening {
                focus.chain(iced::widget::operation::select_all(input_id(card)))
            } else {
                focus
            }
            .map(cosmic::Action::App)
        } else {
            Task::none()
        };
        if let Err(error) = handle.search(find.request, find.query.clone(), find.case_sensitive) {
            return Task::batch([
                focus,
                cosmic::task::message(Message::TerminalError {
                    card,
                    message: error.to_string(),
                }),
            ]);
        }
        focus
    }

    pub(super) fn refresh_completed_terminal_find(&mut self, card: CardId) {
        let find = self.terminal_find.entry(card).or_default();
        find.selection_reset = find.selection_reset.wrapping_add(1);
        find.request = find.request.wrapping_add(1);
        find.matches = Arc::default();
        find.selected = 0;
        find.pending_copy = None;
        if find.open
            && let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card)
        {
            let _ = handle.search(find.request, find.query.clone(), find.case_sensitive);
        }
    }

    pub(super) fn terminal_search_results(
        &mut self,
        card: CardId,
        request: u64,
        query: String,
        matches: Vec<crate::terminal::SearchMatch>,
        truncated: bool,
    ) {
        let Some(find) = self.terminal_find.get_mut(&card) else {
            return;
        };
        if !find.open || find.request != request || find.query != query {
            return;
        }
        find.matches = matches.into();
        find.selected = 0;
        find.truncated = truncated;
        if let Some(first) = find.matches.first()
            && let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card)
        {
            let _ = handle.scroll_to_row(first.start_row);
        }
    }

    pub(super) fn terminal_find_view(&self, card: CardId) -> Option<Element<'_, Message>> {
        let find = self.terminal_find.get(&card).filter(|find| find.open)?;
        let message = |action| Message::TerminalFind(action);
        let input = crate::ui_controls::field::search("Find in terminal scrollback", &find.query)
            .id(input_id(card))
            .on_input(move |query| Message::TerminalFind(Action::Query(card, query)))
            .on_focus(message(Action::Focus(card, true)))
            .on_unfocus(message(Action::Focus(card, false)))
            .width(Length::Fill);
        let count = if find.query.is_empty() {
            String::new()
        } else if find.matches.is_empty() {
            "No matches".into()
        } else {
            format!(
                "{} of {}{}",
                find.selected + 1,
                find.matches.len(),
                if find.truncated { "+" } else { "" }
            )
        };
        Some(
            widget::row::with_children(vec![
                input.into(),
                widget::text(count).into(),
                crate::ui_controls::button::quiet(if find.case_sensitive {
                    "Aa ✓"
                } else {
                    "Aa"
                })
                .on_press(message(Action::Case(card)))
                .into(),
                crate::ui_controls::button::quiet("↑")
                    .on_press(message(Action::Next(card, true)))
                    .into(),
                crate::ui_controls::button::quiet("↓")
                    .on_press(message(Action::Next(card, false)))
                    .into(),
                crate::ui_controls::button::quiet("×")
                    .on_press(message(Action::Close(card)))
                    .into(),
            ])
            .spacing(4)
            .align_y(Alignment::Center)
            .padding(4)
            .into(),
        )
    }

    pub(super) fn terminal_menu(&self, card: CardId) -> Option<Element<'_, Message>> {
        let context = self
            .terminal_context
            .as_ref()
            .filter(|context| context.card == card)?;
        let command = |label, action| {
            widget::button::text(label)
                .width(Length::Fill)
                .on_press(Message::TerminalFind(action))
                .into()
        };
        let mut items = Vec::new();
        let mut copy = widget::button::text("Copy").width(Length::Fill);
        if let Some(selection) = context.selection {
            copy = copy.on_press(Message::TerminalFind(Action::CopySelection(
                card, selection,
            )));
        }
        items.push(copy.into());
        items.push(command("Paste", Action::PasteClipboard(card)));
        items.push(command("Select all", Action::SelectAll(card)));
        items.push(command("Clear scrollback", Action::Clear(card)));
        items.push(command("Find…", Action::Open(card)));
        if let Some(link) = &context.link {
            items.push(
                widget::button::text("Open link")
                    .width(Length::Fill)
                    .on_press(Message::OpenLink(
                        card,
                        link.clone(),
                        settings::LinkOpenGesture::ControlClick,
                    ))
                    .into(),
            );
            items.push(command(
                "Copy link",
                Action::Copy(card, link.as_str().to_owned()),
            ));
        }
        Some(
            widget::column::with_children(items)
                .width(Length::Fixed(200.0))
                .into(),
        )
    }

    pub(super) fn terminal_paste_dialog(&self) -> Option<Element<'_, Message>> {
        let (card, text) = self.terminal_paste.as_ref()?;
        let card = *card;
        Some(dialog_ui::overlay(dialog_ui::surface(widget::column::with_children(vec![
            crate::design::title("Paste multiple lines?").into(),
            notice_body("This program does not use bracketed paste. Each newline may run a command immediately."),
            widget::text(format!("{} lines · {} characters", text.lines().count(), text.chars().count())).into(),
            widget::row::with_children(vec![
                crate::ui_controls::button::secondary("Cancel").id(widget::Id::from("terminal-paste-cancel"))
                    .on_press(Message::TerminalFind(Action::ConfirmPaste(card, false))).into(),
                crate::ui_controls::button::primary("Paste").on_press(Message::TerminalFind(Action::ConfirmPaste(card, true))).into(),
            ]).spacing(8).into(),
        ]).spacing(12), 480.0), Some(Message::TerminalFind(Action::ConfirmPaste(card, false))), 1.0))
    }
}
