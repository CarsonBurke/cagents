//! Every question the application asks opens with the keyboard on its safe
//! answer, unless it is routine, and Escape takes it away, whether it is
//! asked in a dialog or in place.
//!
//! The test walks [`Question::ALL`] and opens each question through the
//! update its user starts it with. A [`confirmation_surface`] or an
//! [`inline_question`] must name its question, every question is in that
//! list, and the exhaustive `match` in [`open`] asks how a new one opens, so
//! a new question is tested as soon as it exists.
//!
//! [`confirmation_surface`]: super::dialog_ui::confirmation_surface
//! [`inline_question`]: super::dialog_ui::inline_question

use super::dialog_ui::{Consequence, Placement, Question};
use super::*;
use cosmic::iced::advanced::widget::{Operation, Tree};
use cosmic::iced::advanced::{Layout, layout};
use iced::futures::StreamExt;

/// A question just opened, with the task that opened it.
struct Opened {
    app: Cagents,
    opening: Task<Message>,
    dismiss: widget::Id,
    confirm: widget::Id,
    /// Where the question is drawn.
    view: for<'a> fn(&'a Cagents) -> Option<Element<'a, Message>>,
    /// What the scenario needs alive while the question is open.
    _keep: Box<dyn std::any::Any>,
}

fn dialog(app: &Cagents) -> Option<Element<'_, Message>> {
    cosmic::Application::dialog(app)
}

fn paste_dialog(app: &Cagents) -> Option<Element<'_, Message>> {
    app.terminal_paste_dialog()
}

fn focused_card(app: &Cagents) -> Option<(CardId, &CardRuntime)> {
    let card = app.workspace.focused()?;
    Some((card, app.cards.get(&card)?))
}

fn automation_pane(app: &Cagents) -> Option<Element<'_, Message>> {
    match focused_card(app)? {
        (_, CardRuntime::Automations) => Some(app.automation_ui.view().map(Message::AutomationUi)),
        _ => None,
    }
}

fn close_refusal(app: &Cagents) -> Option<Element<'_, Message>> {
    match focused_card(app)? {
        (_, CardRuntime::Chat(runtime)) => runtime.close_refusal_view(),
        _ => None,
    }
}

fn review_popover(app: &Cagents) -> Option<Element<'_, Message>> {
    match focused_card(app)? {
        (card, CardRuntime::PullRequests(pane)) => Some(
            pane.review
                .review_popup_for_test()
                .map(move |action| Message::PullRequestReview { card, action }),
        ),
        _ => None,
    }
}

fn reset_controls(app: &Cagents) -> Option<Element<'_, Message>> {
    account_ui::resets::tests::reset_controls(app)
}

/// An automation shown in its focused pane, asked about by `action`.
fn automation_question(action: crate::automation_ui::Action) -> Opened {
    let mut app = tests::context_inspector_test_app(true);
    drop(app.open_automations());
    app.automation_ui.show_automation_for_test();
    let opening = app.update(Message::AutomationUi(action));
    Opened {
        view: automation_pane,
        ..opened(
            app,
            opening,
            crate::automation_ui::confirmation_cancel_id(),
            crate::automation_ui::confirmation_confirm_id(),
        )
    }
}

/// A focused pull request pane whose review can be submitted or discarded,
/// asked about by the review action `prepare` after `before`.
fn review_question(
    old_head: bool,
    before: &[crate::pull_request_review::discussion_ui::Action],
    prepare: crate::pull_request_review::discussion_ui::Action,
) -> Opened {
    use crate::pull_request_review::discussion_ui::tests as review;
    let (mut app, _) = tests::pull_request_test_app(&[1]);
    let (card, _) = app.add_pull_requests_card("/repo".into());
    drop(app.workspace.focus(card));
    let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) else {
        panic!("a pull request pane");
    };
    pane.review = review::ready();
    if old_head {
        review::with_old_head_review(&mut pane.review);
    }
    let send = |app: &mut Cagents, action| {
        let Some(CardRuntime::PullRequests(pane)) = app.cards.get(&card) else {
            panic!("a pull request pane");
        };
        let action = pane.review.review_action(action);
        app.update(Message::PullRequestReview { card, action })
    };
    for action in before {
        drop(send(&mut app, action.clone()));
    }
    let opening = send(&mut app, prepare);
    let Some(CardRuntime::PullRequests(pane)) = app.cards.get(&card) else {
        panic!("a pull request pane");
    };
    let (dismiss, confirm) = pane.review.review_question_ids();
    Opened {
        view: review_popover,
        ..opened(app, opening, dismiss, confirm)
    }
}

fn opened(
    app: Cagents,
    opening: Task<Message>,
    dismiss: widget::Id,
    confirm: widget::Id,
) -> Opened {
    Opened {
        app,
        opening,
        dismiss,
        confirm,
        view: dialog,
        _keep: Box::new(()),
    }
}

fn id(name: &'static str) -> widget::Id {
    widget::Id::new(name)
}

/// A terminal whose close the daemon has just answered with `outcome`.
fn terminal_close(outcome: terminal_close::CloseOutcome) -> Opened {
    let (mut app, card) = terminal_close::tests::retry_close_fixture();
    drop(app.begin_card_close(card));
    let intent = app.terminal_close.clone().expect("a close under way");
    let opening = app.terminal_close_finished(
        card,
        intent.terminal,
        intent.proof,
        intent.generation,
        outcome,
    );
    opened(
        app,
        opening,
        id("dialog.terminal-close.cancel"),
        id("dialog.terminal-close.confirm"),
    )
}

/// An automation draft with an unsaved name.
fn dirty_automation(app: &mut Cagents) {
    use crate::automation_ui::{Action, Field};
    drop(app.update(Message::AutomationUi(Action::New)));
    drop(app.update(Message::AutomationUi(Action::Field(
        Field::Name,
        "Keep this draft".into(),
    ))));
    assert!(app.automation_ui.draft_dirty());
}

fn project_command(remove: bool) -> Opened {
    use crate::sidebar::{ProjectCommand, SidebarWorktree};
    let repository = PathBuf::from("/repo");
    let primary = SidebarWorktree::from_durable(
        crate::model::WorktreeId::new(),
        repository.clone(),
        Some("main".into()),
    );
    let linked = SidebarWorktree::from_durable(
        crate::model::WorktreeId::new(),
        repository.join("worktrees/topic"),
        Some("topic".into()),
    );
    let (mut app, directory, project) =
        project_ui::tests::deletable_app(&repository, &linked, &primary);
    let opening = app.update(Message::ProjectUi(project_ui::Action::Command {
        project_id: project,
        worktree_id: (!remove).then_some(linked.id),
        command: if remove {
            ProjectCommand::Remove
        } else {
            ProjectCommand::Delete
        },
    }));
    let (dismiss, confirm) = if remove {
        (
            "dialog.project.remove.cancel",
            "dialog.project.remove.confirm",
        )
    } else {
        ("dialog.project.close", "dialog.project.confirm")
    };
    Opened {
        _keep: Box::new(directory),
        ..opened(app, opening, id(dismiss), id(confirm))
    }
}

// Each question's `Cagents` lives in a function of its own: an unoptimized
// [`open`] holding every arm's would outgrow a test thread's stack.

fn forget_home() -> Opened {
    let mut app = tests::context_inspector_test_app(false);
    let (account, home) = account_ui::table_tests::linked_home(&mut app);
    let opening = app.update(Message::AccountUi(account_ui::Action::Remove(
        account.clone(),
    )));
    Opened {
        view: account_ui::table_tests::asking_menu,
        _keep: Box::new(home),
        ..opened(
            app,
            opening,
            account_ui::forget_cancel_id(&account),
            account_ui::forget_confirm_id(&account),
        )
    }
}

fn end_close_holders() -> Opened {
    let mut app = tests::context_inspector_test_app(false);
    let (card, refusal) = chat_ui::tests::refusing_close(&mut app);
    let opening = app.update(refusal);
    Opened {
        view: close_refusal,
        ..opened(
            app,
            opening,
            chat_ui::close_refusal_dismiss_id(card),
            chat_ui::close_refusal_end_id(card),
        )
    }
}

fn use_usage_reset() -> Opened {
    let mut app = tests::context_inspector_test_app(false);
    let start = account_ui::resets::tests::spendable_reset(&mut app);
    let opening = app.update(start);
    let account = crate::account::Account::system(crate::agent::Provider::Claude).id();
    Opened {
        view: reset_controls,
        ..opened(
            app,
            opening,
            account_ui::resets::reset_control_id(&account, "accounts", "cancel"),
            account_ui::resets::reset_control_id(&account, "accounts", "confirm"),
        )
    }
}

/// Opens `question` the way its user does.
fn open(question: Question) -> Opened {
    match question {
        Question::StopWorkingChat => {
            let mut app = tests::context_inspector_test_app(false);
            let card = chat_ui::tests::working_chat(&mut app, "working");
            let opening = app.update(Message::CloseCard(card));
            opened(
                app,
                opening,
                id("dialog.chat-close.cancel"),
                id("dialog.chat-close.confirm"),
            )
        }
        Question::StopSendingFirstMessage => {
            let mut app = tests::context_inspector_test_app(false);
            let card = chat_ui::tests::chat_sending_its_first_message(&mut app);
            // Losing the message is asked about even by someone who chose
            // not to be asked before stopping a working chat.
            app.settings
                .set_asks(settings::Confirmation::StopWorkingChat, false);
            let opening = app.update(Message::CloseCard(card));
            opened(
                app,
                opening,
                id("dialog.chat-close.cancel"),
                id("dialog.chat-close.confirm"),
            )
        }
        Question::DiscardUnsentMessage => {
            let (mut app, card) = chat_ui::blocked_recovery_tests::blocked_app(true, true, true);
            let opening = app.update(Message::CloseCard(card));
            opened(
                app,
                opening,
                id("dialog.chat-close.cancel"),
                id("dialog.chat-close.confirm"),
            )
        }
        Question::CloseActiveTerminal => terminal_close(terminal_close::CloseOutcome::Active),
        Question::LeaveTerminalProcessesRunning => terminal_close(
            terminal_close::CloseOutcome::Kept(daemon_recovery::KeptPane {
                reason: "ssh-agent is still running.".into(),
                unreadable: vec![crate::upgrade::UnreadableProcess::fixture(
                    1,
                    10,
                    "ssh-agent",
                )],
            }),
        ),
        Question::TerminalPaneKept => terminal_close(terminal_close::CloseOutcome::Kept(
            "The terminal is still starting.".to_owned().into(),
        )),
        Question::ResetSettings => {
            let mut app = tests::context_inspector_test_app(true);
            let opening = app.update(Message::ResetSettings);
            opened(
                app,
                opening,
                id("dialog.reset-settings.cancel"),
                id("dialog.reset-settings.confirm"),
            )
        }
        Question::DiscardChanges => {
            let mut app = tests::context_inspector_test_app(false);
            let opening = app.update(Message::Sidebar(SidebarAction::RequestDiscard {
                key: tests::discard_key(),
                request: tests::discard_request(false),
            }));
            opened(
                app,
                opening,
                sidebar::discard_cancel_id(),
                sidebar::discard_confirm_id(),
            )
        }
        Question::DeleteSidebarSelection => {
            let (mut app, _, channels) = sidebar_selection_ui::tests::fixture();
            sidebar_selection_ui::tests::select(&mut app, &channels[..2]);
            let opening = app.begin_sidebar_group_delete();
            opened(
                app,
                opening,
                id("dialog.sidebar-selection.close"),
                sidebar_selection_ui::focus_id(),
            )
        }
        Question::RemoveProject => project_command(true),
        Question::DeleteWorktree => project_command(false),
        Question::DeleteChannel => {
            let (mut app, channel, _, _) = channel_ui::tests::channel_with_a_conversation();
            let opening = app.update(Message::ChannelUi(channel_ui::Action::Delete(channel)));
            opened(
                app,
                opening,
                id("dialog.channel.close"),
                id("dialog.channel.confirm"),
            )
        }
        Question::CloseWithUnsavedWork => {
            let mut app = tests::context_inspector_test_app(true);
            let window = window::Id::unique();
            app.core.set_main_window_id(Some(window));
            dirty_automation(&mut app);
            let opening = app.update(Message::WindowCloseRequested(window));
            opened(
                app,
                opening,
                id(TRANSIENT_CLOSE_CANCEL),
                id(TRANSIENT_CLOSE_DISCARD),
            )
        }
        Question::DiscardAutomationDraft => {
            let mut app = tests::context_inspector_test_app(true);
            dirty_automation(&mut app);
            let opening = app.update(Message::AutomationUi(crate::automation_ui::Action::Select(
                uuid::Uuid::new_v4(),
            )));
            opened(
                app,
                opening,
                crate::automation_ui::draft_dialog_keep_id(),
                crate::automation_ui::draft_dialog_discard_id(),
            )
        }
        Question::PasteMultipleLines => {
            let (mut app, card, peer) = terminal_output_tests::fixture();
            app.settings.confirm_multiline_paste = true;
            let opening = app.update(Message::TerminalFind(terminal_ui::Action::Paste(
                card,
                "make\nmake install".into(),
            )));
            Opened {
                view: paste_dialog,
                _keep: Box::new(peer),
                ..opened(
                    app,
                    opening,
                    id("terminal-paste-cancel"),
                    id("terminal-paste-confirm"),
                )
            }
        }
        Question::RemoveAutomation => automation_question(crate::automation_ui::Action::Remove),
        Question::EnableAutomation => automation_question(crate::automation_ui::Action::Enable),
        Question::RunAutomationNow => automation_question(crate::automation_ui::Action::RunNow),
        Question::ForgetHome => forget_home(),
        Question::EndCloseHolders => end_close_holders(),
        // Asked from the open review, where its Discard pending review…
        // control stands.
        Question::DiscardPendingReview => review_question(
            true,
            &[crate::pull_request_review::discussion_ui::Action::ToggleReview],
            crate::pull_request_review::discussion_ui::Action::PrepareDiscard,
        ),
        Question::SubmitReview => review_question(
            false,
            &[
                crate::pull_request_review::discussion_ui::Action::ToggleReview,
                crate::pull_request_review::discussion_ui::Action::Event(
                    crate::github::ReviewEvent::Approve,
                ),
            ],
            crate::pull_request_review::discussion_ui::Action::PrepareSubmit,
        ),
        Question::UseUsageReset => use_usage_reset(),
    }
}

/// The question a dialog asks, found by its surface's id.
fn asked(element: Option<Element<'_, Message>>) -> Option<Question> {
    struct Find(Vec<widget::Id>);
    impl Operation for Find {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn container(&mut self, id: Option<&widget::Id>, _bounds: iced::Rectangle) {
            self.0.extend(id.cloned());
        }
    }
    let mut element = element?;
    let renderer = tests::headless_renderer();
    let mut tree = Tree::new(element.as_widget_mut());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(1600.0, 1200.0)),
    );
    let mut find = Find(Vec::new());
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut find);
    Question::ALL
        .iter()
        .copied()
        .find(|question| find.0.contains(&question.surface_id()))
}

/// Applies the application messages `task` yields, as the runtime would.
fn deliver(app: &mut Cagents, task: Task<Message>) {
    let Some(mut stream) = iced::runtime::task::into_stream(task) else {
        return;
    };
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let mut messages = Vec::new();
    while let Some(action) = executor.block_on(stream.next()) {
        if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
            messages.push(message);
        }
    }
    for message in messages {
        drop(app.update(message));
    }
}

fn escape() -> Message {
    Message::KeyEvent {
        event: dialog_ui::tests::key_press(keyboard::key::Named::Escape),
        captured: false,
    }
}

/// A question that loses work opens on its dismissal, so a reflexive Enter
/// keeps everything; only a routine question opens on its answer. Escape
/// dismisses every one of them.
#[test]
fn every_question_opens_on_its_safe_answer_unless_routine_and_escape_dismisses_it() {
    for &question in Question::ALL {
        let Opened {
            mut app,
            opening,
            dismiss,
            confirm,
            view,
            _keep,
        } = open(question);
        assert_eq!(
            asked(view(&app)),
            Some(question),
            "{question:?} is on screen"
        );
        match question.placement() {
            // A question is a dialog over the window's middle, not a sheet
            // across it.
            Placement::Dialog => {
                let (window, _, _, surface) = dialog_ui::tests::action_geometry(
                    view(&app).unwrap(),
                    iced::Size::new(1600.0, 900.0),
                    &question.surface_id(),
                );
                assert!(
                    surface.width > 0.0
                        && surface.width <= window.width / 2.0
                        && (surface.center_x() - window.center_x()).abs() < 1.0,
                    "{question:?} sits centered in the window: {surface:?} in {window:?}"
                );
            }
            // Asked in place, it holds no dialog over the window, and Escape
            // goes to it.
            Placement::Inline => {
                assert!(
                    cosmic::Application::dialog(&app).is_none(),
                    "{question:?} is asked in place, not in a dialog"
                );
                assert_eq!(
                    app.inline_question().map(|(asked, _)| asked),
                    Some(question),
                    "{question:?} is the inline question Escape answers"
                );
            }
        }
        let expected = match question.consequence() {
            Consequence::Routine(_) => confirm.clone(),
            Consequence::Irreversible | Consequence::Consent | Consequence::Report => {
                dismiss.clone()
            }
        };
        assert_eq!(
            tests::focused_element(view(&app).unwrap(), opening),
            Some(expected),
            "{question:?} opens with focus on the answer its consequence picks"
        );
        if !matches!(question.consequence(), Consequence::Routine(_)) {
            assert_ne!(dismiss, confirm, "{question:?} names two answers");
            // Enter on that focus dismisses the question.
            let enter = dialog_ui::tests::enter_control(view(&app).unwrap(), dismiss);
            drop(app.update(enter));
            assert_ne!(
                asked(view(&app)),
                Some(question),
                "Enter on {question:?}'s opening focus dismisses it"
            );
        }

        let Opened {
            mut app,
            view,
            _keep,
            ..
        } = open(question);
        let task = app.update(escape());
        deliver(&mut app, task);
        assert_ne!(
            asked(view(&app)),
            Some(question),
            "Escape dismisses {question:?}"
        );
    }
}

/// A question asked in place hands the keyboard back when Escape dismisses
/// it: to the control that asked, or for a refused close to the
/// conversation's composer, so the next key lands somewhere.
#[test]
fn escape_on_an_inline_question_returns_focus_to_what_asked_it() {
    for &question in Question::ALL {
        if question.placement() != Placement::Inline {
            continue;
        }
        let Opened { mut app, _keep, .. } = open(question);
        let task = app.update(escape());
        let returned_to = match question {
            Question::RemoveAutomation
            | Question::EnableAutomation
            | Question::RunAutomationNow => focused_in(
                automation_pane(&app),
                task,
                crate::automation_ui::question_opener_id(question),
            ),
            Question::ForgetHome => {
                let account =
                    account_ui::table_tests::menu_account(&app).expect("the menu stays open");
                focused_in(
                    account_ui::table_tests::open_menu(&app),
                    task,
                    account_ui::forget_item_id(&account),
                )
            }
            Question::DiscardPendingReview | Question::SubmitReview => {
                let Some((_, CardRuntime::PullRequests(pane))) = focused_card(&app) else {
                    panic!("a pull request pane");
                };
                let (discard, submit) = pane.review.review_opener_ids();
                let opener = if question == Question::SubmitReview {
                    submit
                } else {
                    discard
                };
                focused_in(review_popover(&app), task, opener)
            }
            Question::EndCloseHolders => {
                let (card, _) = focused_card(&app).expect("the refusing chat");
                (
                    tests::focused(&app, task),
                    Some(card_control_id(card, "chat-composer")),
                )
            }
            Question::UseUsageReset => {
                let account = crate::account::Account::system(crate::agent::Provider::Claude).id();
                focused_in(
                    reset_controls(&app),
                    task,
                    account_ui::resets::reset_control_id(&account, "accounts", "start"),
                )
            }
            _ => unreachable!("{question:?} is not asked in place"),
        };
        assert_eq!(
            returned_to.0, returned_to.1,
            "Escape on {question:?} returns focus to what asked it"
        );
    }
}

fn focused_in(
    view: Option<Element<'_, Message>>,
    task: Task<Message>,
    expected: widget::Id,
) -> (Option<widget::Id>, Option<widget::Id>) {
    (
        tests::focused_element(view.expect("the asking view stays on screen"), task),
        Some(expected),
    )
}

/// The routine questions are exactly the ones a person may choose not to be
/// asked again, as #2711 decided; every other one keeps its dismissal first.
#[test]
fn only_skippable_questions_are_routine() {
    let routine = Question::ALL
        .iter()
        .copied()
        .filter_map(Question::skippable)
        .collect::<Vec<_>>();
    assert_eq!(routine, settings::Confirmation::ALL);
    for question in [
        Question::StopSendingFirstMessage,
        Question::DiscardUnsentMessage,
    ] {
        assert_eq!(
            question.consequence(),
            Consequence::Irreversible,
            "{question:?} loses a message nothing brings back"
        );
    }
}
