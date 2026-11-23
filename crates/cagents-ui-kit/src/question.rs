//! The questions the application asks before an action that costs something,
//! and the surfaces that ask them.

use cosmic::{Element, iced, widget};

use crate::dialog_ui::MAX_HEIGHT;
use crate::settings;

/// What a confirmation's committing action costs, which decides where the
/// keyboard starts and whether the person may stop being asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consequence {
    /// Common and recoverable: the committing action takes focus, so Enter
    /// answers the question the person meant to ask, and the dialog offers
    /// "Don't ask again".
    Routine(settings::Confirmation),
    /// Can't be undone: the dismissal takes focus, so a reflexive Enter
    /// keeps everything as it is. The committing action is one Tab, Shift+Tab
    /// or arrow away, and the question is always asked.
    Irreversible,
    /// Lets an agent act with the permissions shown: the person reads what
    /// they agree to before agreeing, so the dismissal takes focus, and the
    /// question is always asked.
    Authorization,
    /// Nothing to answer: the dialog reports an outcome, and its one way out
    /// takes focus.
    Report,
}

impl Consequence {
    /// The control a confirmation opens with keyboard focus on.
    pub fn initial_focus(self, dismiss: widget::Id, confirm: widget::Id) -> widget::Id {
        match self {
            Self::Routine(_) => confirm,
            Self::Irreversible | Self::Authorization | Self::Report => dismiss,
        }
    }
}

/// Declares [`Question`] and the list of all of it the tests walk, together,
/// so no question can be left off the list.
macro_rules! questions {
    ($($(#[$doc:meta])* $question:ident,)*) => {
        /// Every question the application asks, in a [`confirmation_surface`] or
        /// in place in an [`inline_question`]. Each names what its committing
        /// action costs, so where the keyboard starts is decided here, once,
        /// rather than by whichever code opens it: a question that loses work
        /// can't open with Enter on its destructive button because its opener
        /// reused a routine question's focus, or forgot to place focus at all.
        ///
        /// Only questions whose action is common and recoverable, and which the
        /// person may choose not to be asked again, are routine (#2711).
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Question {
            $($(#[$doc])* $question,)*
        }

        impl Question {
            /// Every question, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$question,)*];
        }
    };
}

questions! {
    /// Closing a chat that may still be working; its conversation stays in
    /// history.
    StopWorkingChat,
    /// Closing a chat whose first message is still on its way: the message
    /// is discarded with it.
    StopSendingFirstMessage,
    /// Closing a tab whose first message couldn't start: the unsent message
    /// is gone for good.
    DiscardUnsentMessage,
    /// Closing a terminal whose processes are still running; the pane can be
    /// opened again.
    CloseActiveTerminal,
    /// Closing a terminal pane beside processes Close couldn't rule out,
    /// which then keep running unseen.
    LeaveTerminalProcessesRunning,
    /// Why a terminal pane was kept rather than closed.
    TerminalPaneKept,
    /// Resetting every setting to its default.
    ResetSettings,
    /// Discarding uncommitted changes, which Git can't bring back.
    DiscardChanges,
    /// Deleting several sidebar rows at once.
    DeleteSidebarSelection,
    /// Removing a project, its channels and their sessions.
    RemoveProject,
    /// Deleting a worktree from disk.
    DeleteWorktree,
    /// Deleting a channel and its panes.
    DeleteChannel,
    /// Closing the window with unsaved work.
    CloseWithUnsavedWork,
    /// Discarding an automation's unsaved changes.
    DiscardAutomationDraft,
    /// Pasting lines a program without bracketed paste may each run at once.
    PasteMultipleLines,
    /// Removing an automation, asked in its pane.
    RemoveAutomation,
    /// Enabling an automation's schedule, which then launches an agent with
    /// the permissions shown, asked in its pane.
    EnableAutomation,
    /// Running an automation now with the permissions shown, asked in its
    /// pane.
    RunAutomationNow,
    /// Forgetting a linked account home, asked in the account's menu.
    ForgetHome,
    /// A chat Close the daemon refused, on the pane it was about, offering
    /// to end the processes that hold it.
    EndCloseHolders,
    /// Deleting a pending pull request review and its draft comments from
    /// GitHub, asked in the review popover.
    DiscardPendingReview,
    /// Publishing a pull request review, asked in the review popover.
    SubmitReview,
    /// Spending one of an account's usage resets, asked beside its usage.
    UseUsageReset,
    /// Installing an application update, which reopens every window, may
    /// stop live work, and can't be taken back to the version before.
    InstallUpdate,
    /// Reconciling a checkout that already holds a pull request's branch
    /// before opening it, asked in the pull request pane: depending on what
    /// it holds, updating the branch or setting its upstream in Git, which
    /// the app has no undo for.
    UseExistingCheckout,
}

/// Where a question is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// In a [`confirmation_surface`] over the window, which holds the
    /// keyboard until it is answered.
    Dialog,
    /// In an [`inline_question`], in place of or beside what it is about.
    /// Its opener focuses where [`Question::focus`] says all the same, and
    /// Escape answers it with its dismissal while it is in front.
    Inline,
}

impl Question {
    pub const fn consequence(self) -> Consequence {
        match self {
            Self::StopWorkingChat => Consequence::Routine(settings::Confirmation::StopWorkingChat),
            Self::CloseActiveTerminal => {
                Consequence::Routine(settings::Confirmation::CloseActiveTerminal)
            }
            Self::TerminalPaneKept => Consequence::Report,
            Self::StopSendingFirstMessage
            | Self::DiscardUnsentMessage
            | Self::LeaveTerminalProcessesRunning
            | Self::ResetSettings
            | Self::DiscardChanges
            | Self::DeleteSidebarSelection
            | Self::RemoveProject
            | Self::DeleteWorktree
            | Self::DeleteChannel
            | Self::CloseWithUnsavedWork
            | Self::DiscardAutomationDraft
            | Self::PasteMultipleLines
            | Self::RemoveAutomation
            | Self::ForgetHome
            | Self::EndCloseHolders
            | Self::DiscardPendingReview
            | Self::SubmitReview
            | Self::UseUsageReset
            | Self::InstallUpdate
            | Self::UseExistingCheckout => Consequence::Irreversible,
            Self::EnableAutomation | Self::RunAutomationNow => Consequence::Authorization,
        }
    }

    pub const fn placement(self) -> Placement {
        match self {
            Self::RemoveAutomation
            | Self::EnableAutomation
            | Self::RunAutomationNow
            | Self::ForgetHome
            | Self::EndCloseHolders
            | Self::DiscardPendingReview
            | Self::SubmitReview
            | Self::UseUsageReset
            | Self::UseExistingCheckout => Placement::Inline,
            Self::StopWorkingChat
            | Self::StopSendingFirstMessage
            | Self::DiscardUnsentMessage
            | Self::CloseActiveTerminal
            | Self::LeaveTerminalProcessesRunning
            | Self::TerminalPaneKept
            | Self::ResetSettings
            | Self::DiscardChanges
            | Self::DeleteSidebarSelection
            | Self::RemoveProject
            | Self::DeleteWorktree
            | Self::DeleteChannel
            | Self::CloseWithUnsavedWork
            | Self::DiscardAutomationDraft
            | Self::PasteMultipleLines
            | Self::InstallUpdate => Placement::Dialog,
        }
    }

    /// The setting that skips this question, if the person may skip it.
    pub const fn skippable(self) -> Option<settings::Confirmation> {
        match self.consequence() {
            Consequence::Routine(confirmation) => Some(confirmation),
            Consequence::Irreversible | Consequence::Authorization | Consequence::Report => None,
        }
    }

    /// The control this question opens with keyboard focus on.
    pub fn initial_focus(self, dismiss: widget::Id, confirm: widget::Id) -> widget::Id {
        self.consequence().initial_focus(dismiss, confirm)
    }

    /// Moves keyboard focus to where this question opens.
    pub fn focus<M: Send + 'static>(
        self,
        dismiss: widget::Id,
        confirm: widget::Id,
    ) -> iced::Task<cosmic::Action<M>> {
        self.focus_task(dismiss, confirm).map(cosmic::Action::App)
    }

    /// [`Self::focus`] for a surface with messages of its own.
    pub fn focus_task<T: Send + 'static>(
        self,
        dismiss: widget::Id,
        confirm: widget::Id,
    ) -> iced::Task<T> {
        iced::widget::operation::focus(self.initial_focus(dismiss, confirm))
    }

    /// [`Self::focus_task`] for a question that arrives once work `asker`
    /// started has finished: it takes the keyboard from `asker` or from
    /// nowhere, but not from a control the person moved to meanwhile, such
    /// as a comment they began typing.
    pub fn focus_task_after<T: Send + 'static>(
        self,
        asker: widget::Id,
        dismiss: widget::Id,
        confirm: widget::Id,
    ) -> iced::Task<T> {
        crate::workspace_widget::focus_after_loading(asker, self.initial_focus(dismiss, confirm))
    }

    /// The id of the surface asking it, by which a test finds which question
    /// is on screen.
    pub fn surface_id(self) -> widget::Id {
        widget::Id::new(format!("dialog.question.{self:?}"))
    }
}

/// A [`crate::dialog_ui::confirmation_surface`] asking `question`, which
/// identifies it; its opener focuses where [`Question::focus`] says.
pub fn confirmation_surface<'a, M: 'a>(
    question: Question,
    content: impl Into<Element<'a, M>>,
    actions: impl Into<Element<'a, M>>,
    max_width: f32,
) -> Element<'a, M> {
    tall_confirmation_surface(question, content, actions, max_width, MAX_HEIGHT)
}

/// A [`confirmation_surface`] that grows to `max_height`, rather than
/// [`MAX_HEIGHT`], before its body scrolls.
pub fn tall_confirmation_surface<'a, M: 'a>(
    question: Question,
    content: impl Into<Element<'a, M>>,
    actions: impl Into<Element<'a, M>>,
    max_width: f32,
    max_height: f32,
) -> Element<'a, M> {
    debug_assert_eq!(question.placement(), Placement::Dialog);
    crate::dialog_ui::tall_confirmation_surface(
        question.surface_id(),
        content,
        actions,
        max_width,
        max_height,
    )
}

/// A [`crate::dialog_ui::inline_question`] asking `question`. Its opener
/// focuses where [`Question::focus`] says, and Escape answers it with its
/// dismissal.
pub fn inline_question<'a, M: 'a>(
    question: Question,
    content: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    debug_assert_eq!(question.placement(), Placement::Inline);
    crate::dialog_ui::inline_question(question.surface_id(), content)
}

pub fn dont_ask_again_id(confirmation: settings::Confirmation) -> widget::Id {
    widget::Id::new(format!("dialog.dont-ask.{}", confirmation.key()))
}
