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

use super::*;
use crate::question::{Consequence, Placement, Question};
use crate::repository::test_fixture::isolated_fixture_case;
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

fn pull_request_pane(app: &Cagents) -> Option<Element<'_, Message>> {
    match focused_card(app)? {
        (card, CardRuntime::PullRequests(pane)) => Some(app.pull_request_card_view(card, pane)),
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
    // The popover opens from the review's summary, beside the files.
    pane.file_panel_collapsed = Some(false);
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
    // Asked on the accounts page, in the pane in front.
    drop(app.update(Message::AccountUi(account_ui::Action::OpenPane)));
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

fn install_update() -> Opened {
    let mut app = tests::context_inspector_test_app(false);
    let poll = update_ui::tests::review_ready(&mut app, update_ui::tests::awaiting_confirmation());
    let opening = app.update(poll);
    opened(app, opening, id("update.cancel"), id("update.confirm"))
}

/// A focused pull request pane, its review shown beside the files, whose
/// Check out pull request found a worktree already holding the branch,
/// offered once preparing finishes.
fn use_existing_checkout() -> Opened {
    existing_checkout_offered(|_, _| ())
}

/// [`use_existing_checkout`], with `meanwhile` done to the app while the
/// checkout prepares.
fn existing_checkout_offered(meanwhile: impl FnOnce(&mut Cagents, CardId)) -> Opened {
    use crate::pull_request_review::checkout_ui::tests::ExistingCheckout;
    let fixture = ExistingCheckout::new();
    let (mut app, project) = tests::pull_request_test_app(&[1]);
    let (card, _) = app.add_pull_requests_card("/repo".into());
    drop(app.workspace.focus(card));
    let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) else {
        panic!("a pull request pane");
    };
    pane.file_panel_collapsed = Some(false);
    fixture.open_review(&mut pane.review);
    let generation = pane.review.selection_generation();
    meanwhile(&mut app, card);
    let opening = app.update(Message::PullRequestCheckoutReady {
        card,
        project,
        generation,
        target: fixture.target(),
        result: Ok(
            crate::pull_request_review::checkout::PreparedCheckout::reconciling(
                fixture.plan(),
                "feature",
            ),
        ),
    });
    let (dismiss, confirm, _) = existing_checkout_ids(&app, card);
    Opened {
        view: pull_request_pane,
        _keep: Box::new(fixture),
        ..opened(app, opening, dismiss, confirm)
    }
}

/// The existing-checkout offer's dismissal and committing action in
/// `card`, and Check out pull request, which asks it.
fn existing_checkout_ids(app: &Cagents, card: CardId) -> (widget::Id, widget::Id, widget::Id) {
    let Some(CardRuntime::PullRequests(pane)) = app.cards.get(&card) else {
        panic!("a pull request pane");
    };
    pane.review.existing_checkout_ids()
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
        Question::InstallUpdate => install_update(),
        Question::UseExistingCheckout => use_existing_checkout(),
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
    // The existing-checkout offer's plan runs Git, which runs outside the
    // parallel test process.
    if isolated_fixture_case(concat!(
        module_path!(),
        "::every_question_opens_on_its_safe_answer_unless_routine_and_escape_dismisses_it"
    )) {
        return;
    }
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
            Consequence::Irreversible | Consequence::Authorization | Consequence::Report => {
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
    // The existing-checkout offer's plan runs Git, which runs outside the
    // parallel test process.
    if isolated_fixture_case(concat!(
        module_path!(),
        "::escape_on_an_inline_question_returns_focus_to_what_asked_it"
    )) {
        return;
    }
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
            Question::UseExistingCheckout => {
                let (card, _) = focused_card(&app).expect("the pull request pane");
                let (_, _, check_out) = existing_checkout_ids(&app, card);
                focused_in(pull_request_pane(&app), task, check_out)
            }
            _ => unreachable!("{question:?} is not asked in place"),
        };
        assert_eq!(
            returned_to.0, returned_to.1,
            "Escape on {question:?} returns focus to what asked it"
        );
    }
}

/// An inline question doesn't hold the keyboard, so Escape a focused
/// control captured, such as a composer interrupting its agent, was that
/// control's alone and leaves the question asked. Forget home's account
/// menu is a popup, which takes Escape however it arrives; the question
/// answers it there, back to the menu.
#[test]
fn a_captured_escape_answers_no_inline_question_outside_a_popup() {
    // The existing-checkout offer's plan runs Git, which runs outside the
    // parallel test process.
    if isolated_fixture_case(concat!(
        module_path!(),
        "::a_captured_escape_answers_no_inline_question_outside_a_popup"
    )) {
        return;
    }
    for &question in Question::ALL {
        if question.placement() != Placement::Inline {
            continue;
        }
        let in_popup = question == Question::ForgetHome;
        let Opened {
            mut app,
            view,
            _keep,
            ..
        } = open(question);
        let task = app.update(Message::KeyEvent {
            event: dialog_ui::tests::key_press(keyboard::key::Named::Escape),
            captured: true,
        });
        deliver(&mut app, task);
        if in_popup {
            assert_eq!(asked(view(&app)), None, "its menu answers {question:?}");
            assert!(
                account_ui::table_tests::open_menu(&app).is_some(),
                "and stays open"
            );
        } else {
            assert_eq!(
                asked(view(&app)),
                Some(question),
                "a captured Escape leaves {question:?} asked"
            );
        }
    }
}

/// A pane's question is the focused pane's to answer: Escape in another
/// pane leaves it asked.
#[test]
fn escape_in_another_pane_leaves_a_pane_question_asked() {
    let Opened { mut app, _keep, .. } = open(Question::RemoveAutomation);
    let automations = app.workspace.focused().expect("the automations pane");
    let chat = chat_ui::tests::working_chat(&mut app, "elsewhere");
    drop(app.workspace.focus(chat));
    let task = app.update(escape());
    deliver(&mut app, task);
    drop(app.workspace.focus(automations));
    assert_eq!(
        asked(automation_pane(&app)),
        Some(Question::RemoveAutomation),
        "Escape in the chat leaves the automation's question asked"
    );
}

/// A popup drawn in front of the pane takes Escape before the question
/// behind it: the first press closes the device menu, the second answers.
#[test]
fn a_popup_in_front_takes_escape_before_the_pane_question() {
    let Opened { mut app, _keep, .. } = open(Question::RemoveAutomation);
    app.device_ui.menu_open = true;
    let task = app.update(escape());
    deliver(&mut app, task);
    assert!(!app.device_ui.menu_open, "the first Escape closes the menu");
    assert_eq!(
        asked(automation_pane(&app)),
        Some(Question::RemoveAutomation),
        "and leaves the question behind it asked"
    );
    let task = app.update(escape());
    deliver(&mut app, task);
    assert_eq!(asked(automation_pane(&app)), None, "the second answers it");
}

/// With no question asked, Escape closes the review popover, as its
/// Cancel does.
#[test]
fn escape_closes_a_review_popover_that_asks_nothing() {
    let Opened { mut app, _keep, .. } = open(Question::SubmitReview);
    let task = app.update(escape());
    deliver(&mut app, task);
    let Some((_, CardRuntime::PullRequests(pane))) = focused_card(&app) else {
        panic!("a pull request pane");
    };
    assert_eq!(pane.review.review_question(), None, "the first answers");
    assert!(pane.review.review_escape().is_some(), "the popover is open");
    let task = app.update(escape());
    deliver(&mut app, task);
    let Some((_, CardRuntime::PullRequests(pane))) = focused_card(&app) else {
        panic!("a pull request pane");
    };
    assert!(
        pane.review.review_escape().is_none(),
        "the second closes the popover"
    );
}

/// A usage reset asked in a pane behind the focused one is still
/// withdrawn by Escape, as it always was, but the keyboard stays where the
/// person is rather than jumping into that pane.
#[test]
fn a_usage_reset_withdrawn_behind_the_focused_pane_leaves_focus_alone() {
    let Opened { mut app, _keep, .. } = open(Question::UseUsageReset);
    let chat = chat_ui::tests::working_chat(&mut app, "in-front");
    drop(app.workspace.focus(chat));
    let task = app.update(escape());
    let account = crate::account::Account::system(crate::agent::Provider::Claude).id();
    assert!(app.inline_question().is_none(), "Escape withdrew the reset");
    assert_ne!(
        tests::focused(&app, task),
        Some(account_ui::resets::reset_control_id(
            &account, "accounts", "start"
        )),
        "and left the keyboard out of the accounts pane"
    );
}

/// The existing-checkout offer arrives after preparing, so it takes the
/// keyboard only from someone still in its pane, where it shows and nothing
/// is in front of it; and Escape doesn't answer it where it doesn't show.
#[test]
fn an_existing_checkout_offer_takes_the_keyboard_only_where_it_is_seen() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::an_existing_checkout_offer_takes_the_keyboard_only_where_it_is_seen"
    )) {
        return;
    }
    let elsewhere = existing_checkout_offered(|app, _| {
        let chat = chat_ui::tests::working_chat(app, "elsewhere");
        drop(app.workspace.focus(chat));
    });
    assert!(
        takes_nothing(elsewhere.opening),
        "another pane in front keeps the keyboard"
    );
    let behind_popover = existing_checkout_offered(|app, card| {
        let Some(CardRuntime::PullRequests(pane)) = app.cards.get(&card) else {
            panic!("a pull request pane");
        };
        let action = pane
            .review
            .review_action(crate::pull_request_review::discussion_ui::Action::ToggleReview);
        drop(app.update(Message::PullRequestReview { card, action }));
    });
    assert!(
        takes_nothing(behind_popover.opening),
        "the review popover in front keeps the keyboard"
    );
    let Opened {
        mut app,
        opening,
        dismiss,
        _keep,
        ..
    } = existing_checkout_offered(|app, card| {
        let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) else {
            panic!("a pull request pane");
        };
        pane.file_panel_collapsed = Some(true);
    });
    assert!(takes_nothing(opening), "a hidden offer takes nothing");
    let task = app.update(escape());
    deliver(&mut app, task);
    let card = app.workspace.focused().expect("the pull request pane");
    let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) else {
        panic!("a pull request pane");
    };
    pane.file_panel_collapsed = Some(false);
    assert_eq!(
        asked(pull_request_pane(&app)),
        Some(Question::UseExistingCheckout),
        "and Escape left it to find when the files show again"
    );
    assert_eq!(
        tests::focused(
            &app,
            Question::UseExistingCheckout.focus(dismiss.clone(), dismiss.clone())
        ),
        Some(dismiss),
        "where it is still answerable"
    );
}

/// Whether `task` leaves every widget alone, whatever else it does. A focus
/// operation whose target isn't drawn blurs every control instead, so a
/// probe of who holds the keyboard afterwards can't tell taking nothing from
/// taking it away.
fn takes_nothing(task: Task<Message>) -> bool {
    let Some(mut stream) = iced::runtime::task::into_stream(task) else {
        return true;
    };
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let mut operated = false;
    while let Some(action) = executor.block_on(stream.next()) {
        operated |= matches!(action, iced::runtime::Action::Widget(_));
    }
    !operated
}

/// Preparing goes to GitHub and Git before the offer arrives, so it takes
/// the keyboard only from Check out pull request, which asked, or from
/// nowhere: a control the person moved to in the meantime keeps it.
#[test]
fn an_existing_checkout_offer_leaves_a_control_the_person_moved_to() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::an_existing_checkout_offer_leaves_a_control_the_person_moved_to"
    )) {
        return;
    }
    let focus = |id: &widget::Id| -> Task<Message> { iced::widget::operation::focus(id.clone()) };
    let Opened {
        app,
        opening,
        dismiss,
        _keep,
        ..
    } = existing_checkout_offered(|_, _| {});
    let card = app.workspace.focused().expect("the pull request pane");
    let (_, _, checkout) = existing_checkout_ids(&app, card);
    assert_eq!(
        tests::focused(&app, focus(&checkout)),
        Some(checkout.clone()),
        "Check out pull request is drawn beside the offer"
    );
    assert_eq!(
        tests::focused(&app, focus(&checkout).chain(opening)),
        Some(dismiss),
        "the offer takes the keyboard from the control that asked"
    );
    let Opened {
        app,
        opening,
        _keep,
        ..
    } = existing_checkout_offered(|_, _| {});
    let elsewhere = crate::sidebar::search_toggle_focus_id();
    assert_eq!(
        tests::focused(&app, focus(&elsewhere)),
        Some(elsewhere.clone()),
        "the sidebar's search is drawn and takes the keyboard"
    );
    assert_eq!(
        tests::focused(&app, focus(&elsewhere).chain(opening)),
        Some(elsewhere),
        "but not from a control the person moved to"
    );
}

/// The review popover hangs from the side panel's header, so Escape leaves
/// it, and any question it asks, alone while the panel is hidden.
#[test]
fn escape_leaves_a_review_popover_hidden_with_its_panel() {
    let Opened { mut app, _keep, .. } = open(Question::SubmitReview);
    let card = app.workspace.focused().expect("the pull request pane");
    let collapse = |app: &mut Cagents, collapsed| {
        let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) else {
            panic!("a pull request pane");
        };
        pane.file_panel_collapsed = Some(collapsed);
    };
    collapse(&mut app, true);
    let task = app.update(escape());
    deliver(&mut app, task);
    collapse(&mut app, false);
    let Some((_, CardRuntime::PullRequests(pane))) = focused_card(&app) else {
        panic!("a pull request pane");
    };
    assert_eq!(
        pane.review.review_question(),
        Some(Question::SubmitReview),
        "the question is still asked when the panel shows again"
    );
}

/// Answering the existing-checkout offer keeps the keyboard on the control
/// that answered, which goes on to report the reconciliation, rather than
/// dropping it as the question becomes its outcome.
#[test]
fn confirming_an_existing_checkout_keeps_the_keyboard_on_its_control() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::confirming_an_existing_checkout_keeps_the_keyboard_on_its_control"
    )) {
        return;
    }
    use cosmic::iced::advanced::widget::operation;
    let Opened {
        mut app,
        confirm,
        _keep,
        ..
    } = use_existing_checkout();
    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
    let mut renderer = tests::headless_renderer();
    // Frames as the runtime builds them, each from the last one's widget
    // state, named or not.
    let mut cache = Some(Cache::new());
    let mut frame = |app: &Cagents, operation: &mut dyn Operation| {
        let mut ui = UserInterface::build(
            pull_request_pane(app).expect("the pane is focused"),
            iced::Size::new(1600.0, 1200.0),
            cache.take().expect("the last frame's cache"),
            &mut renderer,
        );
        ui.operate(&renderer, operation);
        // A probe reports what it found as it finishes.
        let _ = operation.finish();
        cache = Some(ui.into_cache());
    };
    // Tab took the person to the committing action.
    frame(
        &app,
        &mut operation::focusable::focus::<()>(confirm.clone()),
    );
    let press = dialog_ui::tests::enter_control(pull_request_pane(&app).unwrap(), confirm.clone());
    // The reconciliation it starts runs Git on a worker; this frame is the
    // one right after.
    drop(app.update(press));
    assert_eq!(
        app.inline_question().map(|(question, _)| question),
        None,
        "Enter answered the offer"
    );
    let found = Arc::new(std::sync::Mutex::new(None));
    let sink = Arc::clone(&found);
    frame(
        &app,
        &mut operation::map(operation::focusable::find_focused(), move |id| {
            *sink.lock().unwrap() = Some(id);
        }),
    );
    assert_eq!(
        found.lock().unwrap().clone(),
        Some(confirm),
        "the control that answered keeps the keyboard"
    );
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
