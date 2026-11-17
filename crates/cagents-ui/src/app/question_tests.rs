//! Every question the application asks opens with the keyboard on its safe
//! answer, unless it is routine, and Escape takes it away.
//!
//! The test walks [`Question::ALL`] and opens each question through the
//! update its user starts it with, so a new question can't slip past: adding
//! one to [`Question`] is the only way to give it a confirmation surface, and
//! the exhaustive `match` in [`open`] then asks for the way it opens.

use super::dialog_ui::{Consequence, Question};
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
        .into_iter()
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
    for question in Question::ALL {
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
        // A question is a dialog over the window's middle, not a sheet
        // across it.
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
        let expected = match question.consequence() {
            Consequence::Routine(_) => confirm.clone(),
            Consequence::Irreversible | Consequence::Report => dismiss.clone(),
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

/// The routine questions are exactly the ones a person may choose not to be
/// asked again, as #2711 decided; every other one keeps its dismissal first.
#[test]
fn only_skippable_questions_are_routine() {
    let routine = Question::ALL
        .into_iter()
        .filter_map(Question::skippable)
        .collect::<Vec<_>>();
    assert_eq!(routine, settings::Confirmation::ALL);
    assert_eq!(
        Question::DiscardUnsentMessage.consequence(),
        Consequence::Irreversible,
        "an unsent message is gone for good once its tab closes"
    );
}
