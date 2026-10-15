use std::{cell::Cell, collections::HashMap, mem, rc::Rc, sync::{Arc, LazyLock}, time::Duration};

use cosmic::iced::advanced::Renderer as _;
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use cosmic::iced::clipboard::dnd::DndDestinationRectangle;
use cosmic::iced::widget;
use cosmic::iced::{
    Event, Length, Point, Rectangle, Size, Subscription, Vector, keyboard, mouse, time::Instant,
    window,
};
use cosmic::{Element, Renderer, Task, Theme};

use crate::{
    input_trace::InputTrace,
    model::CardId,
    settings::{KeyboardShortcuts, ShortcutBinding, ShortcutKey, format_chords, parse_shortcut},
    workspace::{
        FocusDirection, FocusHandoff, FocusTarget, ViewportProjection, VisibilityClass, Workspace,
    },
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportSample {
    pub offset: f32,
    pub width: f32,
    /// Pointer input moves the camera; native layout feedback only acknowledges it.
    pub user_input: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkspaceViewportEvent {
    Scrolled(ViewportSample),
    Resized(f32),
    PointerFocused(CardId),
    /// The pointer moved into this pane and rested there for
    /// [`HOVER_ACTIVATION_DWELL`] without a click or a key press in between.
    PointerRested(CardId),
    Reordered {
        card: CardId,
        before: Option<CardId>,
    },
    DragScrolled(f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalShortcut {
    ToggleProjects,
    RenamePane,
    NewBrowser,
    OpenFolder,
    NewWorktree,
    CloseOtherPanes,
    ClosePanesToRight,
    CopyPath,
    RevealInProjects,
    ForkConversation,
    FocusSidebarTree,
    RestartDaemon,
    ToggleTheme,
    ResumeConversation,
    SwitchPane,
    ReviewConversationChanges,
    FocusProject1,
    FocusProject2,
    FocusProject3,
    FocusProject4,
    FocusProject5,
    FocusProject6,
    FocusProject7,
    FocusProject8,
    FocusProject9,
    SettingsGeneral,
    SettingsAppearance,
    SettingsTerminal,
    SettingsLinks,
    SettingsKeyboard,
    SettingsNativeChat,
    SettingsEditor,
    SettingsAccounts,
    SettingsNotifications,
    SettingsAbout,
    MovePaneToChannel,

    DiffCopy,
    DiffCopyOld,
    DiffCopyNew,
    DiffCopyPatch,
    DiffCopyLineReference,
    DiffOpenInEditor,
    DiffSelectAll,

    NextDiagnostic,
    PreviousDiagnostic,

    EndConversation,

    EditorUndo,
    EditorRedo,
    EditorCut,
    EditorCopy,
    EditorPaste,
    EditorSelectAll,

    MarkAllRead,
    SearchInProject,
    CloneRepository,
    CopyBranch,
    OpenOrCreatePullRequest,
    NewWorktreeFromBranch,
    MoveProjectUp,
    MoveProjectDown,
    ResetSidebarOrder,
    RevealInFileManager,
    OpenRepositoryUrl,
    OpenInExternalEditor,
    DeleteWorktree,
    RemoveProject,
    DeleteSelectedRows,
    RenameChannel,
    MarkChannelRead,
    DeleteChannel,
    MarkWorktreeRead,
    RenameFile,
    DuplicateFile,
    CopyRelativePath,
    StageSelectedChanges,
    UnstageSelectedChanges,
    DiscardSelectedChanges,
    OpenFile,
    RevealInFiles,
    IgnoreChangePath,
    GitFetch,
    GitPush,
    GitPublish,
    GitSync,
    GitSyncRebase,
    GitStash,
    GitStashUntracked,
    GitStashes,
    GitApplyStash,
    GitPopStash,
    GitDropStash,
    GitCommit,
    GitAmend,
    GitRefresh,
    GitAbort,
    GitContinue,
    GitOutput,
    GitAcceptOurs,
    GitAcceptTheirs,
    EditorComplete,
    GoToDefinition,
    FindReferences,
    ShowHover,
    ShowDiagnostics,
    EditorSave,
    EditorDiscardEdits,
    EditorEdit,
    EditorPreview,
    EditorDiff,


    CommandPalette,
    NewThread,
    ChatAccount,
    ChatModel,
    ChatPermissions,
    ToggleInspector,
    OpenSettings,
    MovePaneLeft,
    MovePaneRight,
    ReopenPane,
    FocusPane1,
    FocusPane2,
    FocusPane3,
    FocusPane4,
    FocusPane5,
    FocusPane6,
    FocusPane7,
    FocusPane8,
    FocusPane9,
    FocusLastPane,
    NewTerminal,
    NewChannel,
    NextAttention,
    PreviousAttention,
    ClonePane,
    CloseFocusedCard,
    FocusProjectSearch,
    FindInPane,
    FocusAdjacent(FocusDirection),
    CyclePaneWidth,
    /// Moves between the top-level workspaces: every project's worktrees and
    /// channels, most recently used first, then in the order the sidebar
    /// lists them.
    CycleWorkspace(FocusDirection),
    /// Flips the viewed mark on the file the focused review pane shows.
    ReviewToggleViewed,
    ReviewNextFile,
    ReviewPreviousFile,
    ReviewNextUnviewed,
    ReviewNextChange,
    ReviewPreviousChange,
}

pub const fn git_command_bit(command: GlobalShortcut) -> Option<u32> {
    match command {
        GlobalShortcut::GitFetch => Some(1 << 0),
        GlobalShortcut::GitPush => Some(1 << 1),
        GlobalShortcut::GitPublish => Some(1 << 2),
        GlobalShortcut::GitSync => Some(1 << 3),
        GlobalShortcut::GitSyncRebase => Some(1 << 4),
        GlobalShortcut::GitStash => Some(1 << 5),
        GlobalShortcut::GitStashUntracked => Some(1 << 6),
        GlobalShortcut::GitStashes => Some(1 << 7),
        GlobalShortcut::GitApplyStash => Some(1 << 8),
        GlobalShortcut::GitPopStash => Some(1 << 9),
        GlobalShortcut::GitDropStash => Some(1 << 10),
        GlobalShortcut::GitCommit => Some(1 << 11),
        GlobalShortcut::GitAmend => Some(1 << 12),
        GlobalShortcut::GitRefresh => Some(1 << 13),
        GlobalShortcut::GitAbort => Some(1 << 14),
        GlobalShortcut::GitContinue => Some(1 << 15),
        GlobalShortcut::GitOutput => Some(1 << 16),
        _ => None,
    }
}

impl GlobalShortcut {
    pub const fn sidebar_command_bit(self) -> Option<u64> {
        match self {
            Self::CopyPath => Some(1 << 0),
            Self::NewTerminal => Some(1 << 1),
            Self::NewThread => Some(1 << 2),
            Self::NewChannel => Some(1 << 3),
            Self::NewWorktree => Some(1 << 4),
            Self::CopyBranch => Some(1 << 5),
            Self::OpenOrCreatePullRequest => Some(1 << 6),
            Self::NewWorktreeFromBranch => Some(1 << 7),
            Self::MoveProjectUp => Some(1 << 8),
            Self::MoveProjectDown => Some(1 << 9),
            Self::ResetSidebarOrder => Some(1 << 10),
            Self::RevealInFileManager => Some(1 << 11),
            Self::OpenRepositoryUrl => Some(1 << 12),
            Self::OpenInExternalEditor => Some(1 << 13),
            Self::DeleteWorktree => Some(1 << 14),
            Self::RemoveProject => Some(1 << 15),
            Self::DeleteSelectedRows => Some(1 << 16),
            Self::RenameChannel => Some(1 << 17),
            Self::MarkChannelRead => Some(1 << 18),
            Self::DeleteChannel => Some(1 << 19),
            Self::MarkWorktreeRead => Some(1 << 20),
            Self::RenameFile => Some(1 << 21),
            Self::DuplicateFile => Some(1 << 22),
            Self::CopyRelativePath => Some(1 << 23),
            Self::StageSelectedChanges => Some(1 << 24),
            Self::UnstageSelectedChanges => Some(1 << 25),
            Self::DiscardSelectedChanges => Some(1 << 26),
            Self::OpenFile => Some(1 << 27),
            Self::RevealInFiles => Some(1 << 28),
            Self::IgnoreChangePath => Some(1 << 29),
            Self::GitAcceptOurs => Some(1 << 30),
            Self::GitAcceptTheirs => Some(1 << 31),
            _ => None,
        }
    }

    /// These commands change the active workspace or create/focus a pane.
    /// Native editing and focus-only sidebar actions can continue during saves.
    pub fn blocked_during_save(self) -> bool {
        let id = self.id();
        (id.starts_with("git_") && !matches!(self, Self::GitStashes | Self::GitOutput | Self::GitRefresh | Self::GitAmend)) || id.starts_with("settings_") || id.starts_with("focus_project_")
            || id.starts_with("focus_pane_")
            || matches!(self, Self::NewTerminal | Self::NewThread | Self::NewBrowser
                | Self::NewChannel | Self::NewWorktree | Self::ClonePane | Self::ReopenPane
                | Self::OpenSettings | Self::ToggleInspector | Self::FocusLastPane
                | Self::FocusAdjacent(_) | Self::CycleWorkspace(_)
                | Self::NextAttention | Self::PreviousAttention
                | Self::MovePaneToChannel | Self::DiffOpenInEditor | Self::OpenFile
                | Self::DeleteChannel)
    }

    pub const fn id(self) -> &'static str {
        match self {
            Self::ToggleProjects => "toggle_projects",
            Self::RenamePane => "rename_pane",
            Self::NewBrowser => "new_browser",
            Self::OpenFolder => "open_folder",
            Self::NewWorktree => "new_worktree",
            Self::CloseOtherPanes => "close_other_panes",
            Self::ClosePanesToRight => "close_panes_to_right",
            Self::CopyPath => "copy_path",
            Self::RevealInProjects => "reveal_in_projects",
            Self::ForkConversation => "fork_conversation",
            Self::FocusSidebarTree => "focus_sidebar_tree",
            Self::RestartDaemon => "restart_daemon",
            Self::ToggleTheme => "toggle_theme",
            Self::ResumeConversation => "resume_conversation",
            Self::SwitchPane => "switch_pane",
            Self::ReviewConversationChanges => "review_conversation_changes",
            Self::FocusProject1 => "focus_project_1",
            Self::FocusProject2 => "focus_project_2",
            Self::FocusProject3 => "focus_project_3",
            Self::FocusProject4 => "focus_project_4",
            Self::FocusProject5 => "focus_project_5",
            Self::FocusProject6 => "focus_project_6",
            Self::FocusProject7 => "focus_project_7",
            Self::FocusProject8 => "focus_project_8",
            Self::FocusProject9 => "focus_project_9",
            Self::SettingsGeneral => "settings_general",
            Self::SettingsAppearance => "settings_appearance",
            Self::SettingsTerminal => "settings_terminal",
            Self::SettingsLinks => "settings_links",
            Self::SettingsKeyboard => "settings_keyboard",
            Self::SettingsNativeChat => "settings_native_chat",
            Self::SettingsEditor => "settings_editor",
            Self::SettingsAccounts => "settings_accounts",
            Self::SettingsNotifications => "settings_notifications",
            Self::SettingsAbout => "settings_about",
            Self::MovePaneToChannel => "move_pane_to_channel",

            Self::DiffCopy => "diff_copy",
            Self::DiffCopyOld => "diff_copy_old",
            Self::DiffCopyNew => "diff_copy_new",
            Self::DiffCopyPatch => "diff_copy_patch",
            Self::DiffCopyLineReference => "diff_copy_line_reference",
            Self::DiffOpenInEditor => "diff_open_in_editor",
            Self::DiffSelectAll => "diff_select_all",

            Self::NextDiagnostic => "next_diagnostic",
            Self::PreviousDiagnostic => "previous_diagnostic",

            Self::EndConversation => "end_conversation",

            Self::EditorUndo => "editor_undo",
            Self::EditorRedo => "editor_redo",
            Self::EditorCut => "editor_cut",
            Self::EditorCopy => "editor_copy",
            Self::EditorPaste => "editor_paste",
            Self::EditorSelectAll => "editor_select_all",

            Self::MarkAllRead => "mark_all_read",
            Self::SearchInProject => "search_in_project",
            Self::CloneRepository => "clone_repository",
            Self::CopyBranch => "copy_branch",
            Self::OpenOrCreatePullRequest => "open_or_create_pull_request",
            Self::NewWorktreeFromBranch => "new_worktree_from_branch",
            Self::MoveProjectUp => "move_project_up",
            Self::MoveProjectDown => "move_project_down",
            Self::ResetSidebarOrder => "reset_sidebar_order",
            Self::RevealInFileManager => "reveal_in_file_manager",
            Self::OpenRepositoryUrl => "open_repository_url",
            Self::OpenInExternalEditor => "open_in_external_editor",
            Self::DeleteWorktree => "delete_worktree",
            Self::RemoveProject => "remove_project",
            Self::DeleteSelectedRows => "delete_selected_rows",
            Self::RenameChannel => "rename_channel",
            Self::MarkChannelRead => "mark_channel_read",
            Self::DeleteChannel => "delete_channel",
            Self::MarkWorktreeRead => "mark_worktree_read",
            Self::RenameFile => "rename_file",
            Self::DuplicateFile => "duplicate_file",
            Self::CopyRelativePath => "copy_relative_path",
            Self::StageSelectedChanges => "stage_selected_changes",
            Self::UnstageSelectedChanges => "unstage_selected_changes",
            Self::DiscardSelectedChanges => "discard_selected_changes",
            Self::OpenFile => "open_file",
            Self::RevealInFiles => "reveal_in_files",
            Self::IgnoreChangePath => "ignore_change_path",
            Self::GitFetch => "git_fetch",
            Self::GitPush => "git_push",
            Self::GitPublish => "git_publish",
            Self::GitSync => "git_sync",
            Self::GitSyncRebase => "git_sync_rebase",
            Self::GitStash => "git_stash",
            Self::GitStashUntracked => "git_stash_untracked",
            Self::GitStashes => "git_stashes",
            Self::GitApplyStash => "git_apply_stash",
            Self::GitPopStash => "git_pop_stash",
            Self::GitDropStash => "git_drop_stash",
            Self::GitCommit => "git_commit",
            Self::GitAmend => "git_amend",
            Self::GitRefresh => "git_refresh",
            Self::GitAbort => "git_abort",
            Self::GitContinue => "git_continue",
            Self::GitOutput => "git_output",
            Self::GitAcceptOurs => "git_accept_ours",
            Self::GitAcceptTheirs => "git_accept_theirs",
            Self::EditorComplete => "editor_complete",
            Self::GoToDefinition => "go_to_definition",
            Self::FindReferences => "find_references",
            Self::ShowHover => "show_hover",
            Self::ShowDiagnostics => "show_diagnostics",
            Self::EditorSave => "editor_save",
            Self::EditorDiscardEdits => "editor_discard_edits",
            Self::EditorEdit => "editor_edit",
            Self::EditorPreview => "editor_preview",
            Self::EditorDiff => "editor_diff",

            Self::NextAttention => "next_attention",
            Self::PreviousAttention => "previous_attention",
            Self::CommandPalette => "command_palette",
            Self::NewThread => "new_native_thread",
            Self::ChatPermissions => "chat_permissions",
            Self::ChatModel => "chat_model",
            Self::ChatAccount => "chat_account",
            Self::ToggleInspector => "toggle_inspector",
            Self::OpenSettings => "open_settings",
            Self::MovePaneLeft => "move_pane_left",
            Self::MovePaneRight => "move_pane_right",
            Self::ReopenPane => "reopen_pane",
            Self::FocusPane1 => "focus_pane_1",
            Self::FocusPane2 => "focus_pane_2",
            Self::FocusPane3 => "focus_pane_3",
            Self::FocusPane4 => "focus_pane_4",
            Self::FocusPane5 => "focus_pane_5",
            Self::FocusPane6 => "focus_pane_6",
            Self::FocusPane7 => "focus_pane_7",
            Self::FocusPane8 => "focus_pane_8",
            Self::FocusPane9 => "focus_pane_9",
            Self::FocusLastPane => "focus_last_pane",
            Self::NewTerminal => "new_terminal",
            Self::NewChannel => "new_channel",
            Self::ClonePane => "clone_pane",
            Self::CloseFocusedCard => "close_focused_column",
            Self::FocusProjectSearch => "search_projects",
            Self::FindInPane => "find_in_pane",
            Self::FocusAdjacent(FocusDirection::Left) => "previous_column",
            Self::FocusAdjacent(FocusDirection::Right) => "next_column",
            Self::CyclePaneWidth => "cycle_pane_width",
            Self::CycleWorkspace(FocusDirection::Left) => "previous_workspace",
            Self::CycleWorkspace(FocusDirection::Right) => "next_workspace",
            Self::ReviewToggleViewed => "review_toggle_viewed",
            Self::ReviewNextFile => "review_next_file",
            Self::ReviewPreviousFile => "review_previous_file",
            Self::ReviewNextUnviewed => "review_next_unviewed",
            Self::ReviewNextChange => "review_next_change",
            Self::ReviewPreviousChange => "review_previous_change",
        }
    }
}

/// Where a shortcut applies, which decides when its chord is withheld from
/// the process inside a card.
///
/// This is the workspace's answer to VS Code's "commands to skip shell": a
/// focused terminal receives every chord except those of the actions whose
/// scope reaches over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutScope {
    /// Anywhere in the workspace, a focused terminal included.
    Workspace,
    /// Anywhere but a focused terminal, which receives the chord as input.
    /// For an action whose chord means something to a shell and the same kind
    /// of thing in every other pane, such as `Ctrl+F`.
    OutsideTerminal,
    /// Leave editing chords to focused text controls.
    OutsideTextInput,
    /// Commands rooted in the actual focused sidebar row or file surface.
    Sidebar,
    /// Only while a pull request pane has focus; a terminal or editor with
    /// focus receives the chord as ordinary input.
    Review,
    /// Only in a native chat pane; other panes retain the configured keys.
    Chat,
    /// Native editable code buffer only.
    Editor,
    /// Undo/Redo in a native buffer or its mounted live working comparison.
    EditorHistory,
    /// Repository content or its non-text pane controls, including local diff.
    Repository,
}

impl ShortcutScope {
    pub fn is_native_editor(self) -> bool { matches!(self, Self::Editor | Self::EditorHistory) }

    /// These scopes implement editing vocabulary on the actual content surface.
    fn owns_editing_chord(self, focused: FocusedPane) -> bool {
        (focused == FocusedPane::Editor && self.is_native_editor())
            || (focused == FocusedPane::WorkingDiff && matches!(self, Self::EditorHistory | Self::Review))
    }

    /// Whether an action of this scope acts, and so withholds its chord from
    /// the card, while `focused` has focus.
    pub fn applies_to(self, focused: FocusedPane) -> bool {
        match self {
            Self::Workspace => true,
            Self::OutsideTerminal => focused != FocusedPane::Terminal,
            Self::OutsideTextInput => !focused.is_text_editing(),
            Self::Sidebar => matches!(focused, FocusedPane::SidebarRows | FocusedPane::SidebarFiles | FocusedPane::SidebarChanges),
            Self::Review => matches!(focused, FocusedPane::Review | FocusedPane::WorkingDiff),
            Self::Chat => matches!(focused, FocusedPane::Chat | FocusedPane::ChatPane),
            Self::Editor => focused == FocusedPane::Editor,
            Self::EditorHistory => matches!(focused, FocusedPane::Editor | FocusedPane::WorkingDiff),
            Self::Repository => matches!(focused, FocusedPane::Editor | FocusedPane::Review | FocusedPane::WorkingDiff | FocusedPane::RepositoryPane),
        }
    }
}

/// The kind of the focused pane, as far as shortcut scopes care.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FocusedPane {
    SidebarRows,
    SidebarFiles,
    SidebarChanges,
    /// Repository pane chrome or non-editable content; text fields keep TextInput.
    RepositoryPane,
    /// A terminal, which forwards the keys it is given to its process.
    Terminal,
    /// A pull request or immutable comparison pane.
    Review,
    /// A mounted live index-to-working-buffer comparison.
    WorkingDiff,
    /// The editable native chat composer.
    Chat,
    /// Non-text controls or transcript content inside a native chat pane.
    ChatPane,
    /// The native code editor.
    Editor,
    /// A text control outside the focused pane (sidebar, inspector, find bar).
    TextInput,
    /// A non-text control outside a pane.
    OutsideWorkspace,
    /// Any other pane, or none.
    #[default]
    Other,
}

impl FocusedPane {
    pub fn is_text_editing(self) -> bool {
        matches!(self, Self::Editor | Self::WorkingDiff | Self::Chat | Self::TextInput)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutDescriptor {
    pub action: GlobalShortcut,
    pub label: &'static str,
    pub repeatable: bool,
    pub scope: ShortcutScope,
}

/// Application context reduced to capabilities so registry availability is
/// deterministic and can be tested without launching native panes.
#[derive(Debug, Default, Clone, Copy)]
pub struct CommandContext {
    /// Some mask records an originating sidebar target, including a stale target with no actions.
    pub sidebar_commands: Option<u64>,
    pub git_commands: u32,
    pub save_in_flight: bool,
    pub focused: FocusedPane,
    pub pane_index: Option<usize>,
    pub pane_count: usize,
    pub project_count: usize,
    pub sidebar_has_rows: bool,
    pub active_git_project: bool,
    pub pane_has_path: bool,
    pub pane_revealable: bool,
    pub pane_cloneable: bool,
    pub pane_external_editor: bool,
    pub pane_moveable: bool,
    pub can_fork: bool,
    pub can_end_conversation: bool,
    pub chat_account: bool,
    pub chat_model: bool,
    pub chat_permissions: bool,
    pub can_switch: bool,
    pub can_review: bool,
    pub can_find: bool,
    pub daemon_can_restart: bool,
    pub can_reopen: bool,
    pub has_unread: bool,
    pub editor_commands: u8,
    pub editor_surface_commands: u8,
    pub editor_language_commands: u8,
    pub diff_commands: u8,
    pub review_commands: u8,
    pub can_navigate_diagnostics: bool,
    pub can_search_project: bool,
    pub has_attention: bool,
    pub can_cycle_workspace: bool,
}

/// Display order shared by the Keyboard table and its registry coverage checks.
pub const SHORTCUT_CATEGORIES: [&str; 6] = ["Panes", "Navigation", "Chat", "Review", "Editor", "Git"];

impl ShortcutDescriptor {
    pub const fn id(self) -> &'static str {
        self.action.id()
    }

    pub fn category(self) -> &'static str {
        if self.id().starts_with("git_") { return "Git"; }
        if self.scope == ShortcutScope::Sidebar { return "Navigation"; }
        let id = self.id();
        if id.starts_with("settings_")
            || matches!(
                self.action,
                GlobalShortcut::OpenSettings
                    | GlobalShortcut::ToggleTheme
                    | GlobalShortcut::RestartDaemon
            )
        {
            "Navigation"
        } else if id.starts_with("focus_project_")
            || matches!(
                self.action,
                GlobalShortcut::OpenFolder
                    | GlobalShortcut::CloneRepository
                    | GlobalShortcut::SearchInProject
                    | GlobalShortcut::MarkAllRead
                    | GlobalShortcut::CommandPalette
                    | GlobalShortcut::NextAttention
                    | GlobalShortcut::PreviousAttention
                    | GlobalShortcut::NewWorktree
                    | GlobalShortcut::FocusSidebarTree
                    | GlobalShortcut::FocusProjectSearch
                    | GlobalShortcut::ToggleProjects
                    | GlobalShortcut::CycleWorkspace(_)
            )
        {
            "Navigation"
        } else if self.scope == ShortcutScope::Chat
            || matches!(self.action, GlobalShortcut::ResumeConversation | GlobalShortcut::NewThread)
        {
            "Chat"
        } else if matches!(self.scope, ShortcutScope::Editor | ShortcutScope::EditorHistory | ShortcutScope::Repository) {
            "Editor"
        } else if self.scope == ShortcutScope::Review {
            "Review"
        } else {
            "Panes"
        }
    }

    pub fn default_chords(self) -> Vec<String> {
        default_shortcut_keys(self.action).to_vec()
    }

    pub fn available(self, context: &CommandContext) -> bool {
        if context.save_in_flight && self.action.blocked_during_save() { return false; }
        if let Some(bit) = git_command_bit(self.action) { return context.git_commands & bit != 0; }
        if let Some(mask) = context.sidebar_commands
            && let Some(bit) = self.action.sidebar_command_bit()
        { return mask & bit != 0; }
        if self.scope == ShortcutScope::Sidebar { return false; }
        // OutsideTerminal controls chord reservation; explicit palette
        // invocation remains available when the terminal has its own finder.
        if matches!(self.scope, ShortcutScope::Chat | ShortcutScope::Review | ShortcutScope::Editor | ShortcutScope::EditorHistory)
            && !self.scope.applies_to(context.focused)
        {
            return false;
        }
        if let Some(bit) = review_command_bit(self.action) { return context.review_commands & bit != 0; }
        if let Some(bit) = diff_command_bit(self.action) {
            return context.diff_commands & bit != 0;
        }
        if let Some(bit) = editor_language_command_bit(self.action) { return context.editor_language_commands & bit != 0; }
        if let Some(bit) = editor_surface_command_bit(self.action) { return context.editor_surface_commands & bit != 0; }
        if let Some(bit) = editor_command_bit(self.action) {
            return context.editor_commands & bit != 0;
        }
        let pane = context.pane_index.is_some();
        match self.action {
            GlobalShortcut::NextDiagnostic | GlobalShortcut::PreviousDiagnostic => context.can_navigate_diagnostics,
            GlobalShortcut::MovePaneToChannel => context.pane_moveable,
            GlobalShortcut::OpenInExternalEditor => context.pane_external_editor,
            GlobalShortcut::NewWorktree => context.active_git_project,
            GlobalShortcut::EndConversation => context.can_end_conversation,
            GlobalShortcut::MarkAllRead => context.has_unread,
            GlobalShortcut::NextAttention | GlobalShortcut::PreviousAttention => context.has_attention,
            GlobalShortcut::CycleWorkspace(_) => context.can_cycle_workspace,
            GlobalShortcut::SearchInProject => context.can_search_project,
            GlobalShortcut::ChatAccount => context.chat_account,
            GlobalShortcut::ChatModel => context.chat_model,
            GlobalShortcut::ChatPermissions => context.chat_permissions,
            GlobalShortcut::FindInPane => context.can_find,
            GlobalShortcut::CloseOtherPanes => pane && context.pane_count > 1,
            GlobalShortcut::ClosePanesToRight | GlobalShortcut::MovePaneRight => context
                .pane_index
                .is_some_and(|index| index + 1 < context.pane_count),
            GlobalShortcut::MovePaneLeft => context.pane_index.is_some_and(|index| index > 0),
            GlobalShortcut::CopyPath => context.pane_has_path,
            GlobalShortcut::RevealInProjects => context.pane_revealable,
            GlobalShortcut::ClonePane => context.pane_cloneable,
            GlobalShortcut::RenamePane
            | GlobalShortcut::CloseFocusedCard
            | GlobalShortcut::CyclePaneWidth => pane,
            GlobalShortcut::ReopenPane => context.can_reopen,
            GlobalShortcut::FocusAdjacent(_) => context.pane_count > 1,
            GlobalShortcut::ForkConversation => context.can_fork,
            GlobalShortcut::ReviewConversationChanges => context.can_review,
            GlobalShortcut::SwitchPane => context.can_switch,
            GlobalShortcut::FocusSidebarTree => context.sidebar_has_rows,
            GlobalShortcut::FocusProjectSearch => context.project_count > 0,
            GlobalShortcut::RestartDaemon => context.daemon_can_restart,
            GlobalShortcut::FocusProject1 => context.project_count > 0,
            GlobalShortcut::FocusProject2 => context.project_count > 1,
            GlobalShortcut::FocusProject3 => context.project_count > 2,
            GlobalShortcut::FocusProject4 => context.project_count > 3,
            GlobalShortcut::FocusProject5 => context.project_count > 4,
            GlobalShortcut::FocusProject6 => context.project_count > 5,
            GlobalShortcut::FocusProject7 => context.project_count > 6,
            GlobalShortcut::FocusProject8 => context.project_count > 7,
            GlobalShortcut::FocusProject9 => context.project_count > 8,
            GlobalShortcut::FocusPane1 => context.pane_count > 0,
            GlobalShortcut::FocusPane2 => context.pane_count > 1,
            GlobalShortcut::FocusPane3 => context.pane_count > 2,
            GlobalShortcut::FocusPane4 => context.pane_count > 3,
            GlobalShortcut::FocusPane5 => context.pane_count > 4,
            GlobalShortcut::FocusPane6 => context.pane_count > 5,
            GlobalShortcut::FocusPane7 => context.pane_count > 6,
            GlobalShortcut::FocusPane8 => context.pane_count > 7,
            GlobalShortcut::FocusPane9 => context.pane_count > 8,
            GlobalShortcut::FocusLastPane => context.pane_count > 0,
            _ => true,
        }
    }
}

/// How long the pointer must rest in a pane it moved into before hover
/// activation gives that pane focus. Long enough that sweeping across the
/// strip - toward the inspector, say - activates nothing on the way; short
/// enough to read as a response to stopping there. It matches the tooltip
/// delay, the app's other reading of a pointer that stopped on purpose.
pub const HOVER_ACTIVATION_DWELL: Duration = Duration::from_millis(300);

pub const GLOBAL_SHORTCUT_DESCRIPTORS: &[ShortcutDescriptor] = &[
    ShortcutDescriptor {
        action: GlobalShortcut::ChatPermissions,
        label: "Choose chat permission mode",
        repeatable: false,
        scope: ShortcutScope::Chat,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ChatModel,
        label: "Choose chat model",
        repeatable: false,
        scope: ShortcutScope::Chat,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ChatAccount,
        label: "Choose chat account",
        repeatable: false,
        scope: ShortcutScope::Chat,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ToggleProjects,
        label: "Toggle Projects sidebar",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::RenamePane,
        label: "Rename pane",
        repeatable: false,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CommandPalette,
        label: "Open command palette",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::NewThread,
        label: "New thread",
        repeatable: false,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ToggleInspector,
        label: "Toggle inspector",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::OpenSettings,
        label: "Open settings",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::MovePaneLeft,
        label: "Move pane left",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::MovePaneRight,
        label: "Move pane right",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReopenPane,
        label: "Reopen closed pane",
        repeatable: false,
        scope: ShortcutScope::OutsideTextInput,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane1,
        label: "Focus pane 1",
        repeatable: false,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane2,
        label: "Focus pane 2",
        repeatable: false,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane3,
        label: "Focus pane 3",
        repeatable: false,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane4,
        label: "Focus pane 4",
        repeatable: false,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane5,
        label: "Focus pane 5",
        repeatable: false,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane6,
        label: "Focus pane 6",
        repeatable: false,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane7,
        label: "Focus pane 7",
        repeatable: false,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane8,
        label: "Focus pane 8",
        repeatable: false,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane9,
        label: "Focus pane 9",
        repeatable: false,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusLastPane,
        label: "Focus last pane",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::NewTerminal,
        label: "New terminal",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::NewChannel,
        label: "New channel",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ClonePane,
        label: "Clone pane",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CloseFocusedCard,
        label: "Close pane",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusProjectSearch,
        label: "Search projects",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FindInPane,
        label: "Find in pane",
        repeatable: false,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusAdjacent(FocusDirection::Left),
        label: "Previous pane",
        repeatable: true,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusAdjacent(FocusDirection::Right),
        label: "Next pane",
        repeatable: true,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CyclePaneWidth,
        label: "Cycle pane width",
        repeatable: true,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CycleWorkspace(FocusDirection::Left),
        label: "Previous workspace",
        repeatable: true,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CycleWorkspace(FocusDirection::Right),
        label: "Next workspace",
        repeatable: true,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::NextAttention,
        label: "Next pane needing attention",
        repeatable: true,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::PreviousAttention,
        label: "Previous pane needing attention",
        repeatable: true,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReviewToggleViewed,
        label: "Mark reviewed file viewed",
        repeatable: false,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReviewNextFile,
        label: "Next reviewed file",
        repeatable: true,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReviewPreviousFile,
        label: "Previous reviewed file",
        repeatable: true,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReviewNextUnviewed,
        label: "Next unviewed file",
        repeatable: true,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReviewNextChange,
        label: "Next changed hunk",
        repeatable: true,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReviewPreviousChange,
        label: "Previous changed hunk",
        repeatable: true,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::NewBrowser,
        label: "New browser pane",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::OpenFolder,
        label: "Open folder / add project",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::NewWorktree,
        label: "New worktree",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CloseOtherPanes,
        label: "Close other panes",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ClosePanesToRight,
        label: "Close panes to the right",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CopyPath,
        label: "Copy path",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::RevealInProjects,
        label: "Reveal in Projects",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ForkConversation,
        label: "Fork conversation",
        repeatable: false,
        scope: ShortcutScope::Chat,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusSidebarTree,
        label: "Focus Projects tree",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::RestartDaemon,
        label: "Restart daemon",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ToggleTheme,
        label: "Toggle theme",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ResumeConversation,
        label: "Resume conversation",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::SwitchPane,
        label: "Switch conversation view",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReviewConversationChanges,
        label: "Review conversation changes",
        repeatable: false,
        scope: ShortcutScope::Chat,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusProject1,
        label: "Go to project 1",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusProject2,
        label: "Go to project 2",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusProject3,
        label: "Go to project 3",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusProject4,
        label: "Go to project 4",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusProject5,
        label: "Go to project 5",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusProject6,
        label: "Go to project 6",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusProject7,
        label: "Go to project 7",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusProject8,
        label: "Go to project 8",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusProject9,
        label: "Go to project 9",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::SettingsGeneral,
        label: "Open settings: General",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::SettingsAppearance,
        label: "Open settings: Appearance",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::SettingsTerminal,
        label: "Open settings: Terminal",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::SettingsLinks,
        label: "Open settings: Links",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::SettingsKeyboard,
        label: "Open settings: Keyboard",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::SettingsNativeChat,
        label: "Open settings: Native chat",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::SettingsEditor,
        label: "Open settings: Editor",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::SettingsAccounts,
        label: "Open settings: Accounts",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::SettingsNotifications,
        label: "Open settings: Notifications",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::SettingsAbout,
        label: "Open settings: About",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::MovePaneToChannel,
        label: "Move pane to channel…",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },

    ShortcutDescriptor {
        action: GlobalShortcut::DiffCopy,
        label: "Copy",
        repeatable: false,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::DiffCopyOld,
        label: "Copy old side",
        repeatable: false,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::DiffCopyNew,
        label: "Copy new side",
        repeatable: false,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::DiffCopyPatch,
        label: "Copy as patch",
        repeatable: false,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::DiffCopyLineReference,
        label: "Copy line reference",
        repeatable: false,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::DiffOpenInEditor,
        label: "Open working file in editor at line",
        repeatable: false,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::DiffSelectAll,
        label: "Select all diff rows",
        repeatable: false,
        scope: ShortcutScope::Review,
    },

    ShortcutDescriptor {
        action: GlobalShortcut::NextDiagnostic,
        label: "Next diagnostic",
        repeatable: true,
        scope: ShortcutScope::Editor,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::PreviousDiagnostic,
        label: "Previous diagnostic",
        repeatable: true,
        scope: ShortcutScope::Editor,
    },

    ShortcutDescriptor {
        action: GlobalShortcut::EndConversation,
        label: "Close conversation…",
        repeatable: false,
        scope: ShortcutScope::Chat,
    },

    ShortcutDescriptor {
        action: GlobalShortcut::EditorUndo,
        label: "Undo",
        repeatable: true,
        scope: ShortcutScope::EditorHistory,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::EditorRedo,
        label: "Redo",
        repeatable: true,
        scope: ShortcutScope::EditorHistory,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::EditorCut,
        label: "Cut",
        repeatable: false,
        scope: ShortcutScope::Editor,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::EditorCopy,
        label: "Copy",
        repeatable: false,
        scope: ShortcutScope::Editor,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::EditorPaste,
        label: "Paste",
        repeatable: false,
        scope: ShortcutScope::Editor,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::EditorSelectAll,
        label: "Select all",
        repeatable: false,
        scope: ShortcutScope::Editor,
    },

    ShortcutDescriptor {
        action: GlobalShortcut::MarkAllRead,
        label: "Mark all as read",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::SearchInProject,
        label: "Search in project",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CloneRepository,
        label: "Clone repository",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },

    ShortcutDescriptor { action: GlobalShortcut::CopyBranch, label: "Copy branch", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::OpenOrCreatePullRequest, label: "Open or create pull request…", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::NewWorktreeFromBranch, label: "New worktree from this branch…", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::MoveProjectUp, label: "Move project up", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::MoveProjectDown, label: "Move project down", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::ResetSidebarOrder, label: "Reset sidebar order", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::RevealInFileManager, label: "Reveal in file manager", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::OpenRepositoryUrl, label: "Open repository on GitHub", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::OpenInExternalEditor, label: "Open in external editor", repeatable: false, scope: ShortcutScope::OutsideTerminal },
    ShortcutDescriptor { action: GlobalShortcut::DeleteWorktree, label: "Delete worktree…", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::RemoveProject, label: "Remove project…", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::DeleteSelectedRows, label: "Delete selected sidebar rows…", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::RenameChannel, label: "Rename channel…", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::MarkChannelRead, label: "Mark channel read", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::DeleteChannel, label: "Delete channel…", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::MarkWorktreeRead, label: "Mark worktree read", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::RenameFile, label: "Rename file…", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::DuplicateFile, label: "Duplicate file…", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::CopyRelativePath, label: "Copy relative path", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::StageSelectedChanges, label: "Stage selected changes", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::UnstageSelectedChanges, label: "Unstage selected changes", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::DiscardSelectedChanges, label: "Discard selected changes…", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::OpenFile, label: "Open file", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::RevealInFiles, label: "Reveal in Files", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::IgnoreChangePath, label: "Add to .gitignore", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::GitFetch, label: "Fetch", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitPush, label: "Push", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitPublish, label: "Publish branch…", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitSync, label: "Sync", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitSyncRebase, label: "Sync with rebase", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitStash, label: "Stash changes", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitStashUntracked, label: "Stash including new files", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitStashes, label: "Show or hide stashes", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitApplyStash, label: "Apply stash…", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitPopStash, label: "Pop stash…", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitDropStash, label: "Drop stash…", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitCommit, label: "Commit changes", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitAmend, label: "Toggle amend commit", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitRefresh, label: "Refresh Git status", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitAbort, label: "Abort Git operation", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitContinue, label: "Continue Git operation", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitOutput, label: "Show or hide Git output", repeatable: false, scope: ShortcutScope::Workspace },
    ShortcutDescriptor { action: GlobalShortcut::GitAcceptOurs, label: "Accept ours and stage", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::GitAcceptTheirs, label: "Accept theirs and stage", repeatable: false, scope: ShortcutScope::Sidebar },
    ShortcutDescriptor { action: GlobalShortcut::EditorComplete, label: "Trigger completion", repeatable: false, scope: ShortcutScope::Editor },
    ShortcutDescriptor { action: GlobalShortcut::GoToDefinition, label: "Go to definition", repeatable: false, scope: ShortcutScope::Editor },
    ShortcutDescriptor { action: GlobalShortcut::FindReferences, label: "Find references", repeatable: false, scope: ShortcutScope::Editor },
    ShortcutDescriptor { action: GlobalShortcut::ShowHover, label: "Show hover information", repeatable: false, scope: ShortcutScope::Editor },
    ShortcutDescriptor { action: GlobalShortcut::ShowDiagnostics, label: "Show diagnostics", repeatable: false, scope: ShortcutScope::Editor },
    ShortcutDescriptor { action: GlobalShortcut::EditorSave, label: "Save file", repeatable: false, scope: ShortcutScope::Repository },
    ShortcutDescriptor { action: GlobalShortcut::EditorDiscardEdits, label: "Discard unsaved edits", repeatable: false, scope: ShortcutScope::Repository },
    ShortcutDescriptor { action: GlobalShortcut::EditorEdit, label: "Edit file", repeatable: false, scope: ShortcutScope::Repository },
    ShortcutDescriptor { action: GlobalShortcut::EditorPreview, label: "Preview file", repeatable: false, scope: ShortcutScope::Repository },
    ShortcutDescriptor { action: GlobalShortcut::EditorDiff, label: "View diff", repeatable: false, scope: ShortcutScope::Repository },

];

static DEFAULT_KEYBOARD_SHORTCUTS: LazyLock<KeyboardShortcuts> = LazyLock::new(KeyboardShortcuts::default);

pub fn default_shortcut_keys(action: GlobalShortcut) -> &'static [String] {
    shortcut_keys(action, &DEFAULT_KEYBOARD_SHORTCUTS)
}

pub fn shortcut_descriptor(action: GlobalShortcut) -> &'static ShortcutDescriptor {
    GLOBAL_SHORTCUT_DESCRIPTORS
        .iter()
        .find(|descriptor| descriptor.action == action)
        .expect("every dispatched global shortcut has a descriptor")
}

/// Every configured chord; newer actions may be explicitly unbound.
pub fn shortcut_keys(action: GlobalShortcut, shortcuts: &KeyboardShortcuts) -> &[String] {
    match action {
        GlobalShortcut::ToggleProjects => &shortcuts.toggle_projects,
        GlobalShortcut::RenamePane => &shortcuts.rename_pane,
        GlobalShortcut::NewBrowser => &shortcuts.new_browser,
        GlobalShortcut::OpenFolder => &shortcuts.open_folder,
        GlobalShortcut::NewWorktree => &shortcuts.new_worktree,
        GlobalShortcut::CloseOtherPanes => &shortcuts.close_other_panes,
        GlobalShortcut::ClosePanesToRight => &shortcuts.close_panes_to_right,
        GlobalShortcut::CopyPath => &shortcuts.copy_path,
        GlobalShortcut::RevealInProjects => &shortcuts.reveal_in_projects,
        GlobalShortcut::ForkConversation => &shortcuts.fork_conversation,
        GlobalShortcut::FocusSidebarTree => &shortcuts.focus_sidebar_tree,
        GlobalShortcut::RestartDaemon => &shortcuts.restart_daemon,
        GlobalShortcut::ToggleTheme => &shortcuts.toggle_theme,
        GlobalShortcut::ResumeConversation => &shortcuts.resume_conversation,
        GlobalShortcut::SwitchPane => &shortcuts.switch_pane,
        GlobalShortcut::ReviewConversationChanges => &shortcuts.review_conversation_changes,
        GlobalShortcut::FocusProject1 => &shortcuts.focus_project_1,
        GlobalShortcut::FocusProject2 => &shortcuts.focus_project_2,
        GlobalShortcut::FocusProject3 => &shortcuts.focus_project_3,
        GlobalShortcut::FocusProject4 => &shortcuts.focus_project_4,
        GlobalShortcut::FocusProject5 => &shortcuts.focus_project_5,
        GlobalShortcut::FocusProject6 => &shortcuts.focus_project_6,
        GlobalShortcut::FocusProject7 => &shortcuts.focus_project_7,
        GlobalShortcut::FocusProject8 => &shortcuts.focus_project_8,
        GlobalShortcut::FocusProject9 => &shortcuts.focus_project_9,
        GlobalShortcut::SettingsGeneral => &shortcuts.settings_general,
        GlobalShortcut::SettingsAppearance => &shortcuts.settings_appearance,
        GlobalShortcut::SettingsTerminal => &shortcuts.settings_terminal,
        GlobalShortcut::SettingsLinks => &shortcuts.settings_links,
        GlobalShortcut::SettingsKeyboard => &shortcuts.settings_keyboard,
        GlobalShortcut::SettingsNativeChat => &shortcuts.settings_native_chat,
        GlobalShortcut::SettingsEditor => &shortcuts.settings_editor,
        GlobalShortcut::SettingsAccounts => &shortcuts.settings_accounts,
        GlobalShortcut::SettingsNotifications => &shortcuts.settings_notifications,
        GlobalShortcut::SettingsAbout => &shortcuts.settings_about,
        GlobalShortcut::MovePaneToChannel => &shortcuts.move_pane_to_channel,

        GlobalShortcut::DiffCopy => &shortcuts.diff_copy,
        GlobalShortcut::DiffCopyOld => &shortcuts.diff_copy_old,
        GlobalShortcut::DiffCopyNew => &shortcuts.diff_copy_new,
        GlobalShortcut::DiffCopyPatch => &shortcuts.diff_copy_patch,
        GlobalShortcut::DiffCopyLineReference => &shortcuts.diff_copy_line_reference,
        GlobalShortcut::DiffOpenInEditor => &shortcuts.diff_open_in_editor,
        GlobalShortcut::DiffSelectAll => &shortcuts.diff_select_all,

        GlobalShortcut::NextDiagnostic => &shortcuts.next_diagnostic,
        GlobalShortcut::PreviousDiagnostic => &shortcuts.previous_diagnostic,

        GlobalShortcut::EndConversation => &shortcuts.end_conversation,

        GlobalShortcut::EditorUndo => &shortcuts.editor_undo,
        GlobalShortcut::EditorRedo => &shortcuts.editor_redo,
        GlobalShortcut::EditorCut => &shortcuts.editor_cut,
        GlobalShortcut::EditorCopy => &shortcuts.editor_copy,
        GlobalShortcut::EditorPaste => &shortcuts.editor_paste,
        GlobalShortcut::EditorSelectAll => &shortcuts.editor_select_all,

        GlobalShortcut::MarkAllRead => &shortcuts.mark_all_read,
        GlobalShortcut::SearchInProject => &shortcuts.search_in_project,
        GlobalShortcut::CloneRepository => &shortcuts.clone_repository,
        GlobalShortcut::CopyBranch => &shortcuts.copy_branch,
        GlobalShortcut::OpenOrCreatePullRequest => &shortcuts.open_or_create_pull_request,
        GlobalShortcut::NewWorktreeFromBranch => &shortcuts.new_worktree_from_branch,
        GlobalShortcut::MoveProjectUp => &shortcuts.move_project_up,
        GlobalShortcut::MoveProjectDown => &shortcuts.move_project_down,
        GlobalShortcut::ResetSidebarOrder => &shortcuts.reset_sidebar_order,
        GlobalShortcut::RevealInFileManager => &shortcuts.reveal_in_file_manager,
        GlobalShortcut::OpenRepositoryUrl => &shortcuts.open_repository_url,
        GlobalShortcut::OpenInExternalEditor => &shortcuts.open_in_external_editor,
        GlobalShortcut::DeleteWorktree => &shortcuts.delete_worktree,
        GlobalShortcut::RemoveProject => &shortcuts.remove_project,
        GlobalShortcut::DeleteSelectedRows => &shortcuts.delete_selected_rows,
        GlobalShortcut::RenameChannel => &shortcuts.rename_channel,
        GlobalShortcut::MarkChannelRead => &shortcuts.mark_channel_read,
        GlobalShortcut::DeleteChannel => &shortcuts.delete_channel,
        GlobalShortcut::MarkWorktreeRead => &shortcuts.mark_worktree_read,
        GlobalShortcut::RenameFile => &shortcuts.rename_file,
        GlobalShortcut::DuplicateFile => &shortcuts.duplicate_file,
        GlobalShortcut::CopyRelativePath => &shortcuts.copy_relative_path,
        GlobalShortcut::StageSelectedChanges => &shortcuts.stage_selected_changes,
        GlobalShortcut::UnstageSelectedChanges => &shortcuts.unstage_selected_changes,
        GlobalShortcut::DiscardSelectedChanges => &shortcuts.discard_selected_changes,
        GlobalShortcut::OpenFile => &shortcuts.open_file,
        GlobalShortcut::RevealInFiles => &shortcuts.reveal_in_files,
        GlobalShortcut::IgnoreChangePath => &shortcuts.ignore_change_path,
        GlobalShortcut::GitFetch => &shortcuts.git_fetch,
        GlobalShortcut::GitPush => &shortcuts.git_push,
        GlobalShortcut::GitPublish => &shortcuts.git_publish,
        GlobalShortcut::GitSync => &shortcuts.git_sync,
        GlobalShortcut::GitSyncRebase => &shortcuts.git_sync_rebase,
        GlobalShortcut::GitStash => &shortcuts.git_stash,
        GlobalShortcut::GitStashUntracked => &shortcuts.git_stash_untracked,
        GlobalShortcut::GitStashes => &shortcuts.git_stashes,
        GlobalShortcut::GitApplyStash => &shortcuts.git_apply_stash,
        GlobalShortcut::GitPopStash => &shortcuts.git_pop_stash,
        GlobalShortcut::GitDropStash => &shortcuts.git_drop_stash,
        GlobalShortcut::GitCommit => &shortcuts.git_commit,
        GlobalShortcut::GitAmend => &shortcuts.git_amend,
        GlobalShortcut::GitRefresh => &shortcuts.git_refresh,
        GlobalShortcut::GitAbort => &shortcuts.git_abort,
        GlobalShortcut::GitContinue => &shortcuts.git_continue,
        GlobalShortcut::GitOutput => &shortcuts.git_output,
        GlobalShortcut::GitAcceptOurs => &shortcuts.git_accept_ours,
        GlobalShortcut::GitAcceptTheirs => &shortcuts.git_accept_theirs,
        GlobalShortcut::EditorComplete => &shortcuts.editor_complete,
        GlobalShortcut::GoToDefinition => &shortcuts.go_to_definition,
        GlobalShortcut::FindReferences => &shortcuts.find_references,
        GlobalShortcut::ShowHover => &shortcuts.show_hover,
        GlobalShortcut::ShowDiagnostics => &shortcuts.show_diagnostics,
        GlobalShortcut::EditorSave => &shortcuts.editor_save,
        GlobalShortcut::EditorDiscardEdits => &shortcuts.editor_discard_edits,
        GlobalShortcut::EditorEdit => &shortcuts.editor_edit,
        GlobalShortcut::EditorPreview => &shortcuts.editor_preview,
        GlobalShortcut::EditorDiff => &shortcuts.editor_diff,


        GlobalShortcut::CommandPalette => &shortcuts.command_palette,
        GlobalShortcut::NewThread => &shortcuts.new_native_thread,
        GlobalShortcut::ChatPermissions => &shortcuts.chat_permissions,
        GlobalShortcut::ChatModel => &shortcuts.chat_model,
        GlobalShortcut::ChatAccount => &shortcuts.chat_account,
        GlobalShortcut::ToggleInspector => &shortcuts.toggle_inspector,
        GlobalShortcut::OpenSettings => &shortcuts.open_settings,
        GlobalShortcut::MovePaneLeft => &shortcuts.move_pane_left,
        GlobalShortcut::MovePaneRight => &shortcuts.move_pane_right,
        GlobalShortcut::ReopenPane => &shortcuts.reopen_pane,
        GlobalShortcut::FocusPane1 => &shortcuts.focus_pane_1,
        GlobalShortcut::FocusPane2 => &shortcuts.focus_pane_2,
        GlobalShortcut::FocusPane3 => &shortcuts.focus_pane_3,
        GlobalShortcut::FocusPane4 => &shortcuts.focus_pane_4,
        GlobalShortcut::FocusPane5 => &shortcuts.focus_pane_5,
        GlobalShortcut::FocusPane6 => &shortcuts.focus_pane_6,
        GlobalShortcut::FocusPane7 => &shortcuts.focus_pane_7,
        GlobalShortcut::FocusPane8 => &shortcuts.focus_pane_8,
        GlobalShortcut::FocusPane9 => &shortcuts.focus_pane_9,
        GlobalShortcut::FocusLastPane => &shortcuts.focus_last_pane,
        GlobalShortcut::NewTerminal => &shortcuts.new_terminal,
        GlobalShortcut::NewChannel => &shortcuts.new_channel,
        GlobalShortcut::ClonePane => &shortcuts.clone_pane,
        GlobalShortcut::CloseFocusedCard => &shortcuts.close_focused_column,
        GlobalShortcut::FocusProjectSearch => &shortcuts.search_projects,
        GlobalShortcut::FindInPane => &shortcuts.find_in_pane,
        GlobalShortcut::FocusAdjacent(FocusDirection::Left) => &shortcuts.previous_column,
        GlobalShortcut::FocusAdjacent(FocusDirection::Right) => &shortcuts.next_column,
        GlobalShortcut::CyclePaneWidth => &shortcuts.cycle_pane_width,
        GlobalShortcut::CycleWorkspace(FocusDirection::Left) => &shortcuts.previous_workspace,
        GlobalShortcut::CycleWorkspace(FocusDirection::Right) => &shortcuts.next_workspace,
        GlobalShortcut::NextAttention => &shortcuts.next_attention,
        GlobalShortcut::PreviousAttention => &shortcuts.previous_attention,
        GlobalShortcut::ReviewToggleViewed => &shortcuts.review_toggle_viewed,
        GlobalShortcut::ReviewNextFile => &shortcuts.review_next_file,
        GlobalShortcut::ReviewPreviousFile => &shortcuts.review_previous_file,
        GlobalShortcut::ReviewNextUnviewed => &shortcuts.review_next_unviewed,
        GlobalShortcut::ReviewNextChange => &shortcuts.review_next_change,
        GlobalShortcut::ReviewPreviousChange => &shortcuts.review_previous_change,
    }
}

/// The same binding that dispatch reserves, expressed for native menu hints.
/// Menus display the first configured chord; the palette/settings list all.
pub fn menu_shortcut(
    action: GlobalShortcut,
    shortcuts: &KeyboardShortcuts,
) -> Option<cosmic::widget::menu::KeyBind> {
    menu_keybind(shortcut_keys(action, shortcuts).first()?)
}

/// Parse one configured chord for a native menu, including named keys.
pub fn menu_keybind(raw: &str) -> Option<cosmic::widget::menu::KeyBind> {
    use cosmic::widget::menu::{KeyBind, key_bind::Modifier};
    use keyboard::{Key, key::Named};
    let binding = crate::settings::parse_shortcut(raw).ok()?;
    let modifiers = [
        (binding.control, Modifier::Ctrl),
        (binding.alt, Modifier::Alt),
        (binding.logo, Modifier::Super),
        (binding.shift, Modifier::Shift),
    ]
    .into_iter()
    .filter_map(|(held, modifier)| held.then_some(modifier))
    .collect();
    let key = match binding.key {
        ShortcutKey::Character(ch) => Key::Character(ch.to_string().into()),
        ShortcutKey::ArrowLeft => Key::Named(Named::ArrowLeft),
        ShortcutKey::ArrowRight => Key::Named(Named::ArrowRight),
        ShortcutKey::ArrowUp => Key::Named(Named::ArrowUp),
        ShortcutKey::ArrowDown => Key::Named(Named::ArrowDown),
        ShortcutKey::Tab => Key::Named(Named::Tab),
        ShortcutKey::Space => Key::Named(Named::Space),
        ShortcutKey::PageUp => Key::Named(Named::PageUp),
        ShortcutKey::PageDown => Key::Named(Named::PageDown),
        ShortcutKey::Function(number) => Key::Named(match number {
            1 => Named::F1,
            2 => Named::F2,
            3 => Named::F3,
            4 => Named::F4,
            5 => Named::F5,
            6 => Named::F6,
            7 => Named::F7,
            8 => Named::F8,
            9 => Named::F9,
            10 => Named::F10,
            11 => Named::F11,
            12 => Named::F12,
            _ => return None,
        }),
    };
    Some(KeyBind { modifiers, key })
}

/// The chords bound to `action`, in the one-line form the settings pane
/// reads back.
pub fn shortcut_chords(action: GlobalShortcut, shortcuts: &KeyboardShortcuts) -> String {
    format_chords(shortcut_keys(action, shortcuts))
}

pub fn shortcut_tooltip(action: GlobalShortcut, shortcuts: &KeyboardShortcuts) -> String {
    shortcut_tooltip_named(action, shortcuts, shortcut_descriptor(action).label)
}

pub fn shortcut_tooltip_named(
    action: GlobalShortcut,
    shortcuts: &KeyboardShortcuts,
    label: &str,
) -> String {
    if shortcut_keys(action, shortcuts).is_empty() {
        return format!("{label} — unbound");
    }
    format!("{label} — {}", shortcut_chords(action, shortcuts))
}

#[derive(Debug, Default)]
pub struct FrameClock {
    last: Option<Instant>,
}

impl FrameClock {
    pub fn elapsed(&mut self, frame: Instant) -> Option<Duration> {
        let Some(previous) = self.last else {
            self.last = Some(frame);
            return None;
        };
        let elapsed = frame.checked_duration_since(previous)?;
        self.last = Some(frame);
        // Occluded Wayland windows can receive frames only once per second.
        // Bound catch-up work without starving their animations indefinitely.
        Some(elapsed.min(Duration::from_millis(250)))
    }

    /// The frame this clock last advanced to.
    ///
    /// The shell re-runs the whole update-and-layout pass for one
    /// `RedrawRequested` while any pass publishes a message, rebuilding the
    /// view in between, so a view that read the wall clock would measure a
    /// different animation on every pass and never settle. Animated geometry
    /// reads the frame instead, which makes the view a function of the frame.
    pub fn frame(&self) -> Option<Instant> {
        self.last
    }

    /// Measures the next frame from `now`, the moment an animation began.
    ///
    /// A clock with no frame behind it spends its first one finding out when
    /// it is, so an animation started on an unset clock stood still for a
    /// frame before moving. Started here, that frame advances it by the time
    /// that has really passed since the input that began it.
    pub fn start(&mut self, now: Instant) {
        self.last = Some(now);
    }

    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// What a card needs to know about itself to render.
///
/// The card's widget identities are deliberately absent: they are stable per
/// card for the app's whole lifetime, so rebuilding them here would allocate
/// once per mounted card per frame. The owner passes them in instead.
#[derive(Debug, Clone, Copy)]
pub struct CardViewContext {
    pub id: CardId,
    pub focused: bool,
}

pub fn card_widget_id(id: CardId) -> widget::Id {
    widget::Id::new(format!("cagents-workspace-card-{id}"))
}

pub fn card_focus_id(id: CardId) -> widget::Id {
    widget::Id::new(format!("cagents-workspace-card-{id}-focus"))
}

pub fn animation_frames<Message, F>(app_window: window::Id, map: F) -> Subscription<Message>
where
    Message: Send + 'static,
    F: Fn(Instant) -> Message + Send + Clone + 'static,
{
    fn matching_window(
        (expected, (actual, instant)): (window::Id, (window::Id, Instant)),
    ) -> Option<Instant> {
        (expected == actual).then_some(instant)
    }

    window::frames()
        .with(app_window)
        .filter_map(matching_window)
        .map(map)
}

pub fn scroll_to<Message: 'static>(id: widget::Id, offset: f32) -> Task<Message> {
    widget::scrollable::scroll_to(
        id,
        widget::scrollable::AbsoluteOffset {
            x: Some(offset.max(0.0)),
            y: None,
        },
    )
}

pub fn focus_card<Message: 'static>(id: CardId) -> Task<Message> {
    widget::operation::focus(card_focus_id(id))
}

/// Complete a loading pane's focus handoff without taking focus from a
/// control the user chose while the content was arriving.
pub fn focus_after_loading<Message: Send + 'static>(
    placeholder: widget::Id,
    target: widget::Id,
) -> Task<Message> {
    use cosmic::iced::advanced::widget::operation::{self, Outcome, focusable};
    struct Restore {
        foreign_focus: bool,
        placeholder: widget::Id,
        target: widget::Id,
    }
    impl<Message: Send + 'static> Operation<Message> for Restore {
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            _bounds: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            self.foreign_focus |= state.is_focused() && id != Some(&self.placeholder);
        }
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Message>)) {
            operate(self);
        }
        fn finish(&self) -> Outcome<Message> {
            if self.foreign_focus {
                Outcome::None
            } else {
                Outcome::Chain(Box::new(focusable::focus(self.target.clone())))
            }
        }
    }
    cosmic::iced::advanced::widget::operate(Restore {
        foreign_focus: false,
        placeholder,
        target,
    })
}

/// End-of-strip navigation returns the keyboard from chrome without taking
/// it from a find bar, composer, or other control already inside the pane.
pub fn focus_card_if_outside<Message: Send + 'static>(
    container: widget::Id,
    target: widget::Id,
) -> Task<Message> {
    use cosmic::iced::advanced::widget::operation::{self, Outcome, focusable};

    struct Restore {
        query: Box<dyn Operation<focusable::Count>>,
        target: widget::Id,
    }

    impl<Message: Send + 'static> Operation<Message> for Restore {
        fn container(&mut self, id: Option<&widget::Id>, bounds: Rectangle) {
            self.query.container(id, bounds);
        }

        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            self.query.focusable(id, bounds, state);
        }

        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Message>)) {
            self.query.traverse(&mut |query| {
                operate(&mut operation::black_box(query));
            });
        }

        fn finish(&self) -> Outcome<Message> {
            match self.query.finish() {
                Outcome::Some(count) if count.total > 0 && count.focused.is_none() => {
                    Outcome::Chain(Box::new(focusable::focus(self.target.clone())))
                }
                // An unmounted pane has no controls: do not blur the window
                // by targeting an input that has not been rendered yet.
                _ => Outcome::None,
            }
        }
    }

    cosmic::iced::advanced::widget::operate(Restore {
        query: Box::new(operation::scoped(container, focusable::count())),
        target,
    })
}

pub fn focus_handoff<Message, F>(
    handoff: FocusHandoff,
    toolbar_focus_id: widget::Id,
    card_focus: F,
) -> Task<Message>
where
    Message: Send + 'static,
    F: FnOnce(CardId) -> widget::Id,
{
    match handoff.target {
        FocusTarget::Card(id) => widget::operation::focus(card_focus(id)),
        FocusTarget::WorkspaceToolbar => widget::operation::focus(toolbar_focus_id),
        FocusTarget::Unchanged => Task::none(),
    }
}

pub fn focus_handoff_then<Message, F>(
    handoff: FocusHandoff,
    toolbar_focus_id: widget::Id,
    card_focus: F,
    after_focus: Message,
) -> Task<Message>
where
    Message: Send + 'static,
    F: FnOnce(CardId) -> widget::Id,
{
    focus_handoff(handoff, toolbar_focus_id, card_focus).chain(Task::done(after_focus))
}

/// The action a key press triggers while `focused` has focus. A chord whose
/// action does not apply there triggers nothing, since
/// [`is_global_shortcut_chord`] left it to the card.
pub fn global_shortcut(
    event: &keyboard::Event,
    shortcuts: &KeyboardShortcuts,
    focused: FocusedPane,
) -> Option<GlobalShortcut> {
    let keyboard::Event::KeyPressed {
        key,
        physical_key,
        modifiers,
        repeat,
        ..
    } = event
    else {
        return None;
    };
    let descriptor = global_shortcut_descriptor(key, *physical_key, *modifiers, shortcuts, focused)?;
    if focused.is_text_editing() && !descriptor.scope.owns_editing_chord(focused)
        && text_editing_conflict(descriptor.action, key, *physical_key, *modifiers)
    {
        return None;
    }
    (shortcut_applies(descriptor, focused, shortcuts) && (!*repeat || descriptor.repeatable))
        .then_some(
            if focused == FocusedPane::Terminal
                && descriptor.action == GlobalShortcut::FocusProjectSearch
            {
                GlobalShortcut::FindInPane
            } else {
                descriptor.action
            },
        )
}

/// Whether a chord is withheld from the process inside a card: whether it is
/// bound to an action whose scope applies to the `focused` pane. So a terminal
/// never loses an arrow chord to a review pane that is not there, nor its
/// forward-char to project search.
pub(crate) fn is_global_shortcut_chord(
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
    shortcuts: &KeyboardShortcuts,
    focused: FocusedPane,
) -> bool {
    crate::tab_history::is_chord(key, modifiers)
        || global_shortcut_descriptor(key, physical_key, modifiers, shortcuts, focused).is_some_and(
            |descriptor| {
                !(focused == FocusedPane::Editor && descriptor.scope.is_native_editor()) && !(focused.is_text_editing() && !descriptor.scope.owns_editing_chord(focused)
                    && text_editing_conflict(descriptor.action, key, physical_key, modifiers))
                    && shortcut_applies(descriptor, focused, shortcuts)
            },
        )
}

/// Native text controls retain their editing vocabulary even when a global
/// action is rebound to it. Find is context-dependent and handled by scope.
fn text_editing_conflict(
    action: GlobalShortcut,
    key: &keyboard::Key,
    physical: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
) -> bool {
    text_editing_chord(key, physical, modifiers)
        || (action != GlobalShortcut::FindInPane
            && modifiers.control()
            && !modifiers.shift()
            && !modifiers.alt()
            && !modifiers.logo()
            && latin_key(key, physical) == Some('f'))
}

fn text_editing_chord(
    key: &keyboard::Key,
    physical: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
) -> bool {
    use keyboard::key::Named;
    if modifiers.control() && !modifiers.alt() && !modifiers.logo() {
        if matches!(latin_key(key, physical), Some('z'))
            || (!modifiers.shift()
                && matches!(latin_key(key, physical), Some('y' | 'a' | 'c' | 'v' | 'x')))
        {
            return true;
        }
        return matches!(
            key,
            keyboard::Key::Named(
                Named::ArrowLeft
                    | Named::ArrowRight
                    | Named::Home
                    | Named::End
                    | Named::Backspace
                    | Named::Delete
            )
        );
    }
    modifiers.alt()
        && !modifiers.control()
        && !modifiers.logo()
        && matches!(
            key,
            keyboard::Key::Named(
                Named::ArrowLeft | Named::ArrowRight | Named::Backspace | Named::Delete
            )
        )
}

fn shortcut_applies(
    descriptor: &ShortcutDescriptor,
    focused: FocusedPane,
    shortcuts: &KeyboardShortcuts,
) -> bool {
    if focused == FocusedPane::Terminal {
        let keys = shortcut_keys(descriptor.action, shortcuts);
        let action = shortcuts
            .actions()
            .iter()
            .position(|candidate| std::ptr::eq(candidate.as_slice(), keys));
        if action.is_some_and(|index| {
            shortcuts
                .terminal_passthrough_actions
                .iter()
                .any(|name| name == KeyboardShortcuts::action_names()[index])
        }) {
            return false;
        }
    }
    // Sidebar rows may reuse NewThread for their captured worktree. Other
    // pane commands must not act on the background selected card. The app's
    // current row capability mask rejects project/channel/stale NewThread.
    let outside_pane = matches!(focused,
        FocusedPane::TextInput | FocusedPane::OutsideWorkspace
            | FocusedPane::SidebarFiles | FocusedPane::SidebarChanges)
        || (focused == FocusedPane::SidebarRows && descriptor.action != GlobalShortcut::NewThread);
    if outside_pane && matches!(
        descriptor.action,
        GlobalShortcut::FindInPane
            | GlobalShortcut::NewThread
            | GlobalShortcut::RenamePane
            | GlobalShortcut::CloseFocusedCard
            | GlobalShortcut::ClonePane
            | GlobalShortcut::ChatAccount
            | GlobalShortcut::ChatModel
            | GlobalShortcut::ChatPermissions
    ) {
        return false;
    }
    // The saved list is the explicit per-action terminal policy. Removing a
    // shell-safe default opts that action into interception intentionally.
    if focused == FocusedPane::Terminal && descriptor.scope == ShortcutScope::OutsideTerminal {
        return true;
    }
    descriptor.scope.applies_to(focused)
}

fn global_shortcut_descriptor(
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
    shortcuts: &KeyboardShortcuts,
    focused: FocusedPane,
) -> Option<&'static ShortcutDescriptor> {
    GLOBAL_SHORTCUT_DESCRIPTORS.iter().find(|descriptor| {
        shortcut_applies(descriptor, focused, shortcuts)
            && !(focused.is_text_editing() && !descriptor.scope.owns_editing_chord(focused) && text_editing_conflict(descriptor.action, key, physical_key, modifiers))
            && shortcut_keys(descriptor.action, shortcuts).iter().any(|chord| {
                parse_shortcut(chord).is_ok_and(|binding| shortcut_matches(binding, key, physical_key, modifiers))
            })
    })
}

pub const fn review_command_bit(command: GlobalShortcut) -> Option<u8> {
    Some(match command {
        GlobalShortcut::ReviewToggleViewed => 1,
        GlobalShortcut::ReviewNextFile => 2,
        GlobalShortcut::ReviewPreviousFile => 4,
        GlobalShortcut::ReviewNextUnviewed => 8,
        GlobalShortcut::ReviewNextChange => 16,
        GlobalShortcut::ReviewPreviousChange => 32,
        _ => return None,
    })
}

pub const fn diff_command_bit(command: GlobalShortcut) -> Option<u8> {
    Some(match command {
        GlobalShortcut::DiffCopy => 1,
        GlobalShortcut::DiffCopyOld => 2,
        GlobalShortcut::DiffCopyNew => 4,
        GlobalShortcut::DiffCopyPatch => 8,
        GlobalShortcut::DiffCopyLineReference => 16,
        GlobalShortcut::DiffOpenInEditor => 32,
        GlobalShortcut::DiffSelectAll => 64,
        _ => return None,
    })
}

pub fn shortcut_descriptor_by_id(id: &str) -> Option<&'static ShortcutDescriptor> {
    GLOBAL_SHORTCUT_DESCRIPTORS.iter().find(|descriptor| descriptor.id() == id)
}

pub const fn editor_language_command_bit(command: GlobalShortcut) -> Option<u8> {
    match command {
        GlobalShortcut::EditorComplete => Some(1 << 0),
        GlobalShortcut::GoToDefinition => Some(1 << 1),
        GlobalShortcut::FindReferences => Some(1 << 2),
        GlobalShortcut::ShowHover => Some(1 << 3),
        GlobalShortcut::ShowDiagnostics => Some(1 << 4),
        _ => None,
    }
}

pub const fn editor_surface_command_bit(command: GlobalShortcut) -> Option<u8> {
    match command {
        GlobalShortcut::EditorSave => Some(1 << 0),
        GlobalShortcut::EditorDiscardEdits => Some(1 << 1),
        GlobalShortcut::EditorEdit => Some(1 << 2),
        GlobalShortcut::EditorPreview => Some(1 << 3),
        GlobalShortcut::EditorDiff => Some(1 << 4),
        _ => None,
    }
}

pub const fn editor_command_bit(command: GlobalShortcut) -> Option<u8> {
    Some(match command {
        GlobalShortcut::EditorUndo => 1,
        GlobalShortcut::EditorRedo => 2,
        GlobalShortcut::EditorCut => 4,
        GlobalShortcut::EditorCopy => 8,
        GlobalShortcut::EditorPaste => 16,
        GlobalShortcut::EditorSelectAll => 32,
        _ => return None,
    })
}

/// Native code-buffer widgets dispatch registered editing actions once, before
/// the application sees the same captured event.
pub fn editor_shortcut(key: &keyboard::Key, physical: keyboard::key::Physical, modifiers: keyboard::Modifiers, shortcuts: &KeyboardShortcuts) -> Option<GlobalShortcut> {
    global_shortcut_descriptor(key, physical, modifiers, shortcuts, FocusedPane::Editor)
        .filter(|descriptor| descriptor.scope.is_native_editor()).map(|descriptor| descriptor.action)
}

fn shortcut_matches(
    binding: ShortcutBinding,
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
) -> bool {
    // Read flag by flag, so a lock modifier the user happens to be holding is
    // simply not consulted.
    binding.control == modifiers.control()
        && binding.alt == modifiers.alt()
        && binding.shift == modifiers.shift()
        && binding.logo == modifiers.logo()
        && match binding.key {
            ShortcutKey::Character(expected) if expected.is_ascii_digit() => {
                physical_digit(physical_key) == Some(expected)
            }
            ShortcutKey::Character(expected) => latin_key(key, physical_key) == Some(expected),
            ShortcutKey::ArrowLeft => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::ArrowLeft)
            ),
            ShortcutKey::ArrowRight => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::ArrowRight)
            ),
            ShortcutKey::ArrowUp => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::ArrowUp)
            ),
            ShortcutKey::ArrowDown => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::ArrowDown)
            ),
            ShortcutKey::Tab => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::Tab)
            ),
            ShortcutKey::Space => matches!(key.as_ref(), keyboard::Key::Named(keyboard::key::Named::Space)) || matches!(key.as_ref(), keyboard::Key::Character(value) if value == " "),
            ShortcutKey::PageUp => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::PageUp)
            ),
            ShortcutKey::PageDown => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::PageDown)
            ),
            ShortcutKey::Function(number) => {
                use keyboard::key::Named;
                let keys = [
                    Named::F1,
                    Named::F2,
                    Named::F3,
                    Named::F4,
                    Named::F5,
                    Named::F6,
                    Named::F7,
                    Named::F8,
                    Named::F9,
                    Named::F10,
                    Named::F11,
                    Named::F12,
                ];
                keys.get(usize::from(number).saturating_sub(1))
                    .is_some_and(|expected| key.as_ref() == keyboard::Key::Named(*expected))
            }
        }
}

fn latin_key(key: &keyboard::Key, physical_key: keyboard::key::Physical) -> Option<char> {
    if let keyboard::Key::Character(value) = key.as_ref() {
        let mut characters = value.chars();
        if let (Some(character), None) = (characters.next(), characters.next())
            && character.is_ascii_graphic()
        {
            return Some(character.to_ascii_lowercase());
        }
    }
    key.to_latin(physical_key)
        .map(|key| key.to_ascii_lowercase())
}

fn physical_digit(physical_key: keyboard::key::Physical) -> Option<char> {
    use keyboard::key::Code;

    let keyboard::key::Physical::Code(code) = physical_key else {
        return None;
    };
    match code {
        Code::Digit0 => Some('0'),
        Code::Digit1 => Some('1'),
        Code::Digit2 => Some('2'),
        Code::Digit3 => Some('3'),
        Code::Digit4 => Some('4'),
        Code::Digit5 => Some('5'),
        Code::Digit6 => Some('6'),
        Code::Digit7 => Some('7'),
        Code::Digit8 => Some('8'),
        Code::Digit9 => Some('9'),
        _ => None,
    }
}

/// Matches a parsed chord against a keyboard event using the product's one
/// chord grammar.
///
/// `ceditor` builds on this crate and shares [`parse_shortcut`]'s grammar, so
/// it has to share the matcher too: a forked matcher made the same configured
/// chord fire on a different physical key in each shell, and digit chords the
/// parser accepts never fired at all without the physical-digit fallback.
#[must_use]
pub fn shortcut_event_matches(binding: ShortcutBinding, event: &keyboard::Event) -> bool {
    let keyboard::Event::KeyPressed {
        key,
        physical_key,
        modifiers,
        repeat: false,
        ..
    } = event
    else {
        return false;
    };
    shortcut_matches(binding, key, *physical_key, *modifiers)
}

/// The application's constructors for the events the strip reports.
pub struct Events<ViewportMessage> {
    /// Scroll offsets and width changes, which the owner applies to the
    /// workspace so the next frame's projection matches what was drawn.
    pub viewport: ViewportMessage,
}

/// Renders the workspace strip.
///
/// `viewport_width` is the width the strip is laid out against. It is a
/// parameter rather than something this function discovers through
/// `widget::responsive` because that wrapper reports no accessibility nodes,
/// which hid the whole workspace from screen readers; the owner already tracks
/// the width through `Workspace::resize_viewport`.
///
/// `container_id` supplies each card's stable container identity, which
/// scroll-to-card operations address. It is a lookup rather than something this
/// function rebuilds because `widget::Id::new` allocates, and every mounted
/// card would pay that on every redraw; the owner is expected to return a
/// cached clone.
///
/// `input` is the trace the owner's [`crate::input_trace::observe`] keeps for
/// the whole window, which hover activation reads.
pub fn view<'a, Message, BuildCard, ContainerId, ViewportMessage>(
    workspace: &'a Workspace,
    shortcuts: &'a KeyboardShortcuts,
    focused_pane: FocusedPane,
    focus_follows_mouse: bool,
    input: &InputTrace,
    viewport_width: f32,
    scroll_id: widget::Id,
    build_card: BuildCard,
    container_id: ContainerId,
    events: Events<ViewportMessage>,
) -> Element<'a, Message>
where
    Message: Clone + 'static,
    BuildCard: Fn(CardViewContext) -> Element<'a, Message> + 'a,
    ContainerId: Fn(CardId) -> widget::Id + 'a,
    ViewportMessage: Fn(WorkspaceViewportEvent) -> Message + Clone + 'a,
{
    let Events {
        viewport: on_viewport,
    } = events;
    let projection = workspace.projection(viewport_width);
    let strip =
        VirtualStrip::new(
            &projection,
            Arc::clone(workspace.card_indices()),
            workspace.focused(),
            projection
                .mounted
                .iter()
                .filter_map(|mounted| {
                    let card = workspace.card(mounted.id)?;
                    let focused = workspace.focused() == Some(card.id);
                    let context = CardViewContext {
                        id: card.id,
                        focused,
                    };
                    let content =
                        cosmic::widget::id_container(build_card(context), container_id(card.id));
                    let on_screen = mounted.visibility == VisibilityClass::Visible;
                    Some(VirtualCard {
                        id: card.id,
                        x: mounted.start,
                        width: mounted.width,
                        content_width: mounted.content_width,
                        on_screen,
                        content: CardSurface::new(content)
                            .traced_by(input)
                            .on_screen(on_screen)
                            .on_pointer_focus(on_viewport(WorkspaceViewportEvent::PointerFocused(
                                card.id,
                            )))
                            // The active pane has nothing to gain from a rest.
                            .on_pointer_rest((focus_follows_mouse && !focused).then(|| {
                                on_viewport(WorkspaceViewportEvent::PointerRested(card.id))
                            }))
                            .into(),
                    })
                })
                .collect(),
        );
    let on_viewport_for_scroll = on_viewport.clone();
    let scroll_input = Rc::new(Cell::new(false));
    let input_for_scroll = Rc::clone(&scroll_input);
    // Reordering belongs to the tab strip's explicit title handles. A wrapper
    // around pane bodies intercepts Alt drags before terminals can route them
    // to mouse-tracking applications.
    let scrollable = cosmic::widget::scrollable::horizontal(strip)
        .direction(widget::scrollable::Direction::Horizontal(
            widget::scrollable::Scrollbar::hidden(),
        ))
        .id(scroll_id.clone())
        .class(cosmic::style::iced::Scrollable::Minimal)
        .width(Length::Fill)
        .height(Length::Fill)
        .on_scroll(move |viewport| {
            on_viewport_for_scroll(WorkspaceViewportEvent::Scrolled(ViewportSample {
                offset: viewport.absolute_offset().x,
                width: viewport.bounds().width,
                user_input: input_for_scroll.get(),
            }))
        });

    // Scrollable exposes a composite ID that Iced inherits during diff even
    // when its requested ID changes. Key its entire subtree by worktree so
    // programmatic scroll operations cannot keep targeting the departed strip.
    WorkspaceSurface::new(
        cosmic::widget::id_container(scrollable, scroll_id.clone()),
        shortcuts,
        focused_pane,
        viewport_width,
        move |width: f32| on_viewport(WorkspaceViewportEvent::Resized(width)),
    )
    .with_keyboard_trace(input)
    .with_scroll_input(scroll_input)
    .with_initial_scroll(scroll_id, workspace.camera().offset())
    .into()
}

struct CardSurface<'a, Message> {
    content: Element<'a, Message>,
    on_pointer_focus: Option<Message>,
    on_pointer_rest: Option<Message>,
    input: InputTrace,
    on_screen: bool,
}

impl<'a, Message> CardSurface<'a, Message> {
    fn new(content: impl Into<Element<'a, Message>>) -> Self {
        Self {
            content: content.into(),
            on_pointer_focus: None,
            on_pointer_rest: None,
            input: InputTrace::default(),
            on_screen: false,
        }
    }

    /// Whether the workspace classes this pane as visible, which a pane the
    /// camera is still travelling to is before any of it is in the viewport.
    /// Such a pane takes everything but the pointer; see `update`.
    fn on_screen(mut self, on_screen: bool) -> Self {
        self.on_screen = on_screen;
        self
    }

    /// The window's record of its input, which hover activation reads for
    /// where each motion began and for presses the pane never received.
    /// Without it the pane never counts the pointer as having crossed in.
    fn traced_by(mut self, input: &InputTrace) -> Self {
        self.input = input.clone();
        self
    }

    fn on_pointer_focus(mut self, message: Message) -> Self {
        self.on_pointer_focus = Some(message);
        self
    }

    /// Published once the pointer has moved into this pane and rested there;
    /// see [`CardPointerState`] for what counts.
    fn on_pointer_rest(mut self, message: Option<Message>) -> Self {
        self.on_pointer_rest = message;
        self
    }

    fn child_layout<'layout>(layout: Layout<'layout>) -> Layout<'layout> {
        layout
            .children()
            .next()
            .expect("card surface content layout")
            .with_virtual_offset(layout.virtual_offset())
    }
}

/// Hover activation follows deliberate pointer movement only.
///
/// An entry counts when the pointer itself crossed into the pane: where the
/// motion began, according to the window's [`InputTrace`] and placed against
/// the pane where the pane is now, lay outside it. The trace, not the pane,
/// remembers that start, because a pane misses motions: over an overlay that
/// captured them, or while it was scrolled out of the mounted band. Being
/// uncovered is not an entry - a menu closing over the pane, the pointer
/// returning from a popup surface, the strip scrolling under a still pointer,
/// or a pane mounting beneath it all leave the pointer where it was.
///
/// An entry must then rest for [`HOVER_ACTIVATION_DWELL`], so sweeping across
/// the strip activates nothing on the way. A key or button pressed anywhere
/// before then cancels it, so a pointer that drifts while the reader types or
/// runs a shortcut leaves their keystrokes going where they were going. The
/// pointer must still be over the pane when the dwell ends, which a pane the
/// strip moved away meanwhile is not.
#[derive(Default)]
struct CardPointerState {
    /// When a crossing into the pane began the dwell it is still serving.
    entered_at: Option<Instant>,
    selecting: bool,
}

/// How late past its dwell a rest may still be reported. A pane scrolled out
/// of view receives no frames, so its dwell ends at the first frame it gets
/// back, however long after the pointer stopped there that is.
const HOVER_ACTIVATION_LATENESS: Duration = Duration::from_secs(1);

/// The cursor's position in the pane's coordinates, including while the
/// strip's own edge keeps it from counting as over the pane.
fn cursor_point(cursor: mouse::Cursor) -> Option<Point> {
    match cursor {
        mouse::Cursor::Available(point) | mouse::Cursor::Levitating(point) => Some(point),
        mouse::Cursor::Unavailable => None,
    }
}

fn clipped_viewport(bounds: Rectangle, viewport: &Rectangle) -> Option<Rectangle> {
    bounds.intersection(viewport)
}

fn should_forward_to_raw_children(event_captured: bool) -> bool {
    !event_captured
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for CardSurface<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<CardPointerState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(CardPointerState::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let child = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
            .move_to(Point::ORIGIN);
        layout::Node::with_children(child.size(), vec![child])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Self::child_layout(layout),
                renderer,
                operation,
            );
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let pointer = tree.state.downcast_mut::<CardPointerState>();
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(button)) => {
                pointer.selecting |= *button == mouse::Button::Left;
                pointer.entered_at = None;
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                pointer.selecting = false
            }
            Event::Mouse(mouse::Event::CursorLeft) => *pointer = CardPointerState::default(),
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let clip = clipped_viewport(layout.bounds(), viewport);
                let hovered = !pointer.selecting
                    && !shell.is_event_captured()
                    && clip.is_some_and(|clip| cursor.is_over(clip));
                // Where the motion began is replayed through the current
                // translation, so content scrolled under a still pointer did
                // not cross anything.
                let crossed_in = hovered
                    && self
                        .input
                        .previous_position()
                        .zip(cursor_point(cursor))
                        .zip(clip)
                        .is_some_and(|((previous, local), clip)| {
                            !clip.contains(previous + (local - *position))
                        });
                if !hovered {
                    pointer.entered_at = None;
                } else if crossed_in && self.on_pointer_rest.is_some() {
                    let now = Instant::now();
                    pointer.entered_at = Some(now);
                    shell.request_redraw_at(now + HOVER_ACTIVATION_DWELL);
                }
            }
            Event::Window(window::Event::RedrawRequested(now)) => {
                if let Some(entered) = pointer.entered_at {
                    let due = entered + HOVER_ACTIVATION_DWELL;
                    if *now < due {
                        shell.request_redraw_at(due);
                    } else {
                        pointer.entered_at = None;
                        let rested = now.saturating_duration_since(due)
                            <= HOVER_ACTIVATION_LATENESS
                            && !self.input.pressed_since(entered)
                            && clipped_viewport(layout.bounds(), viewport)
                                .is_some_and(|clip| cursor.is_over(clip));
                        // Gone when the pane became active in the meantime.
                        if rested && let Some(message) = &self.on_pointer_rest {
                            shell.publish(message.clone());
                        }
                    }
                }
            }
            _ => {}
        }
        let clipped = match clipped_viewport(layout.bounds(), viewport) {
            Some(clipped) => clipped,
            // None of this pane can be pointed at, but one the camera is
            // travelling to already holds the keyboard: what is typed during
            // the journey is meant for it.
            None if self.on_screen && !matches!(event, Event::Mouse(_) | Event::Touch(_)) => {
                Rectangle::new(layout.bounds().position(), Size::ZERO)
            }
            None => return,
        };
        // Observe before the child can capture the press, but do not capture it
        // ourselves: text selection, caret placement and controls still get it.
        if !shell.is_event_captured()
            && matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_)))
            && cursor.is_over(clipped)
            && let Some(message) = &self.on_pointer_focus
        {
            shell.publish(message.clone());
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            Self::child_layout(layout),
            cursor,
            renderer,
            clipboard,
            shell,
            &clipped,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let Some(clipped) = clipped_viewport(layout.bounds(), viewport) else {
            return mouse::Interaction::default();
        };
        if !cursor.is_over(clipped) {
            return mouse::Interaction::default();
        }
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            Self::child_layout(layout),
            cursor,
            &clipped,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let Some(clipped) = clipped_viewport(layout.bounds(), viewport) else {
            return;
        };
        renderer.with_layer(clipped, |renderer| {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                _theme,
                style,
                Self::child_layout(layout),
                cursor,
                &clipped,
            );
        });
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        // `overlay` is the one pass whose layout is not already on screen: the
        // workspace scrollable leaves card layouts in content space and reports
        // the camera separately as `translation`, so the card's visible
        // rectangle is its bounds plus that translation. Testing the
        // untranslated bounds against the screen-space viewport dropped every
        // overlay raised inside a card whose content-space position sat outside
        // the visible rect, which is every card past the second one however far
        // the camera had scrolled it into view.
        let clipped = clipped_viewport(layout.bounds() + translation, viewport)?;
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            Self::child_layout(layout),
            renderer,
            &clipped,
            translation,
        )
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        state: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content
            .as_widget()
            .a11y_nodes(Self::child_layout(layout), &state.children[0], cursor)
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            Self::child_layout(layout),
            renderer,
            destinations,
        );
    }
}

impl<'a, Message: Clone + 'a> From<CardSurface<'a, Message>> for Element<'a, Message> {
    fn from(surface: CardSurface<'a, Message>) -> Self {
        Element::new(surface)
    }
}

struct VirtualCard<'a, Message> {
    id: CardId,
    x: f32,
    width: f32,
    content_width: f32,
    /// Whether the workspace classes the card as visible. A pane the camera
    /// is still travelling to is, without yet being inside the viewport.
    on_screen: bool,
    content: Element<'a, Message>,
}

impl<Message> VirtualCard<'_, Message> {
    /// The part of `viewport` this card may draw in and take pointer input
    /// from. An opening card is laid out wider than its slot, and what lies
    /// past the slot's edge belongs to its neighbour.
    fn viewport(&self, bounds: Rectangle, viewport: &Rectangle) -> Rectangle {
        if self.content_width <= self.width {
            return *viewport;
        }
        let slot = Rectangle {
            width: self.width,
            ..bounds
        };
        slot.intersection(viewport)
            .unwrap_or(Rectangle::new(slot.position(), Size::ZERO))
    }

    /// The pointer as this card may see it. Widgets test the pointer against
    /// their own bounds, which for an opening card reach under its neighbour.
    fn cursor(&self, viewport: &Rectangle, cursor: mouse::Cursor) -> mouse::Cursor {
        if self.content_width <= self.width || cursor.is_over(*viewport) {
            cursor
        } else {
            mouse::Cursor::Unavailable
        }
    }
}

struct VirtualStrip<'a, Message> {
    total_width: f32,
    retained: Arc<HashMap<CardId, usize>>,
    focused: Option<CardId>,
    cards: Vec<VirtualCard<'a, Message>>,
}

impl<'a, Message> VirtualStrip<'a, Message> {
    fn new(
        projection: &ViewportProjection,
        retained: Arc<HashMap<CardId, usize>>,
        focused: Option<CardId>,
        cards: Vec<VirtualCard<'a, Message>>,
    ) -> Self {
        debug_assert_eq!(
            cards.iter().map(|card| card.id).collect::<Vec<_>>(),
            projection.mounted_ids().collect::<Vec<_>>()
        );
        Self {
            total_width: projection.total_width,
            retained,
            focused,
            cards,
        }
    }
}

#[derive(Default)]
struct VirtualStripState {
    mounted: Vec<CardId>,
    detached: HashMap<CardId, Tree>,
    unfocus_on_layout: std::collections::HashSet<CardId>,
    retained: Arc<HashMap<CardId, usize>>,
}

// Iced extracts named states before diffing. A departing virtual card must
// reclaim those states before being parked outside the traversed child tree;
// otherwise the next frame drops them and remounting downcasts State::None.
fn reclaim_named_states(tree: &mut Tree) {
    if let Some(widget::Id(iced_accessibility::id::Internal::Custom(_, name))) = &tree.id {
        if let Some((state, children)) = tree::NAMED.with(|named| named.borrow_mut().remove(name)) {
            tree.state = state;
            for (index, child) in children {
                if let Some(slot) = tree.children.get_mut(index) {
                    *slot = child;
                }
            }
        }
    }
    for child in &mut tree.children {
        reclaim_named_states(child);
    }
}

impl<Message> Widget<Message, Theme, Renderer> for VirtualStrip<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<VirtualStripState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(VirtualStripState {
            mounted: self.cards.iter().map(|card| card.id).collect(),
            detached: HashMap::new(),
            unfocus_on_layout: Default::default(),
            retained: Arc::clone(&self.retained),
        })
    }

    fn children(&self) -> Vec<Tree> {
        self.cards
            .iter()
            .map(|card| Tree::new(&card.content))
            .collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<VirtualStripState>();
        let membership_changed = !Arc::ptr_eq(&state.retained, &self.retained);
        if state
            .mounted
            .iter()
            .copied()
            .eq(self.cards.iter().map(|card| card.id))
        {
            // Camera movement normally leaves the mounted band unchanged.
            // Diff those children in place, keeping their identity and storage.
            for (card, child) in self.cards.iter_mut().zip(&mut tree.children) {
                child.diff(&mut card.content);
            }
        } else {
            let mut children = mem::take(&mut tree.children);
            let previous_band = mem::take(&mut state.mounted);
            for (id, mut child) in previous_band.iter().copied().zip(children.drain(..)) {
                reclaim_named_states(&mut child);
                state.detached.insert(id, child);
            }
            for card in &mut self.cards {
                let parked = state.detached.remove(&card.id);
                if parked.is_some()
                    && !previous_band.contains(&card.id)
                    && self.focused != Some(card.id)
                {
                    // A focus operation cannot visit a parked tree. Returning
                    // an inactive pane must not revive its old input focus.
                    state.unfocus_on_layout.insert(card.id);
                }
                let mut child = parked.unwrap_or_else(|| Tree::new(&card.content));
                child.diff(&mut card.content);
                children.push(child);
                state.mounted.push(card.id);
            }
            tree.children = children;
        }
        // Detached trees are retained for focus/selection continuity, but need
        // pruning only after structural changes, never on animation frames.
        if membership_changed {
            state
                .detached
                .retain(|id, _| self.retained.contains_key(id));
            state
                .unfocus_on_layout
                .retain(|id| self.retained.contains_key(id));
            state.retained = Arc::clone(&self.retained);
        }
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.total_width), Length::Fill)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.resolve(
            Length::Fixed(self.total_width),
            Length::Fill,
            Size::new(self.total_width, 0.0),
        );
        let pending = &mut tree
            .state
            .downcast_mut::<VirtualStripState>()
            .unfocus_on_layout;
        let children = self
            .cards
            .iter_mut()
            .zip(&mut tree.children)
            .map(|(card, state)| {
                let child_size = Size::new(card.content_width, size.height);
                let child_limits = layout::Limits::new(child_size, child_size);
                let node = card
                    .content
                    .as_widget_mut()
                    .layout(state, renderer, &child_limits)
                    .move_to(Point::new(card.x, 0.0));
                if pending.remove(&card.id) {
                    card.content.as_widget_mut().operate(
                        state,
                        Layout::new(&node),
                        renderer,
                        &mut cosmic::iced::advanced::widget::operation::focusable::unfocus::<()>(),
                    );
                }
                node
            })
            .collect();
        layout::Node::with_children(size, children)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((card, state), child_layout) in self
                .cards
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                card.content.as_widget_mut().operate(
                    state,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    renderer,
                    operation,
                );
            }
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        for ((card, state), child_layout) in self
            .cards
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            let viewport = &card.viewport(child_layout.bounds(), viewport);
            let cursor = card.cursor(viewport, cursor);
            // A pane the camera is travelling to already holds the keyboard
            // and is about to be looked at, so it is shown events as though it
            // had arrived: keys typed during the journey reach it, and it
            // sizes itself before it is seen. Panes merely parked in the
            // overscan still get pointer events alone.
            if card.on_screen
                || child_layout.bounds().intersects(viewport)
                || matches!(
                    event,
                    Event::Mouse(
                        mouse::Event::CursorMoved { .. }
                            | mouse::Event::CursorLeft
                            | mouse::Event::ButtonPressed(_)
                            | mouse::Event::ButtonReleased(_)
                    )
                )
            {
                card.content.as_widget_mut().update(
                    state,
                    event,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    cursor,
                    renderer,
                    clipboard,
                    shell,
                    viewport,
                );
            }
        }
        // Wheel input belongs to terminal/file content, never the outer strip.
        // Consume any remainder after children so the native scrollable cannot
        // turn a horizontal trackpad gesture into pane navigation.
        if matches!(event, Event::Mouse(mouse::Event::WheelScrolled { .. }))
            && cursor.is_over(*viewport)
        {
            shell.capture_event();
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.cards
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .filter_map(|((card, state), child_layout)| {
                let viewport = card.viewport(child_layout.bounds(), viewport);
                child_layout
                    .bounds()
                    .intersection(&viewport)
                    .is_some_and(|visible| cursor.is_over(visible))
                    .then(|| {
                        card.content.as_widget().mouse_interaction(
                            state,
                            child_layout.with_virtual_offset(layout.virtual_offset()),
                            cursor,
                            &viewport,
                            renderer,
                        )
                    })
            })
            .max()
            .unwrap_or_default()
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let Some(clipped_viewport) = layout.bounds().intersection(viewport) else {
            return;
        };
        for ((card, state), child_layout) in
            self.cards.iter().zip(&tree.children).zip(layout.children())
        {
            let viewport = card.viewport(child_layout.bounds(), &clipped_viewport);
            if !child_layout.bounds().intersects(&viewport) {
                continue;
            }
            card.content.as_widget().draw(
                state,
                renderer,
                theme,
                style,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                cursor,
                &viewport,
            );
        }
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        let children = self
            .cards
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
            .filter_map(|((card, state), child_layout)| {
                card.content.as_widget_mut().overlay(
                    state,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    renderer,
                    viewport,
                    translation,
                )
            })
            .collect::<Vec<_>>();
        (!children.is_empty()).then(|| overlay::Group::with_children(children).overlay())
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        state: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        iced_accessibility::A11yTree::join(
            self.cards
                .iter()
                .zip(&state.children)
                .zip(layout.children())
                .map(|((card, child_state), child_layout)| {
                    card.content.as_widget().a11y_nodes(
                        child_layout.with_virtual_offset(layout.virtual_offset()),
                        child_state,
                        cursor,
                    )
                }),
        )
    }

    fn drag_destinations(
        &self,
        state: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        for ((card, child_layout), child_state) in self
            .cards
            .iter()
            .zip(layout.children())
            .zip(&state.children)
        {
            card.content.as_widget().drag_destinations(
                child_state,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                renderer,
                destinations,
            );
        }
    }
}

impl<'a, Message: 'a> From<VirtualStrip<'a, Message>> for Element<'a, Message> {
    fn from(strip: VirtualStrip<'a, Message>) -> Self {
        Element::new(strip)
    }
}

struct WorkspaceSurface<'a, Message> {
    content: Element<'a, Message>,
    shortcuts: &'a KeyboardShortcuts,
    /// The kind of the focused card; see [`is_global_shortcut_chord`].
    focused_pane: FocusedPane,
    /// The strip viewport width the owner already holds, which is what the
    /// enclosing `view` laid this surface's content out against.
    ///
    /// Size reporting lives in this widget instead of a `widget::sensor`
    /// wrapper because `sensor` reports no accessibility nodes, and wrapping
    /// the strip in one hid every card from screen readers.
    reported_width: f32,
    on_resize: Box<dyn Fn(f32) -> Message + 'a>,
    initial_scroll: Option<(widget::Id, f32)>,
    scroll_input: Option<Rc<Cell<bool>>>,
    input: InputTrace,
}

#[derive(Default)]
struct WorkspaceSurfaceState {
    initialized_scroll: Option<widget::Id>,
}

impl<'a, Message> WorkspaceSurface<'a, Message> {
    fn new(
        content: impl Into<Element<'a, Message>>,
        shortcuts: &'a KeyboardShortcuts,
        focused_pane: FocusedPane,
        reported_width: f32,
        on_resize: impl Fn(f32) -> Message + 'a,
    ) -> Self {
        Self {
            content: content.into(),
            shortcuts,
            focused_pane,
            reported_width,
            on_resize: Box::new(on_resize),
            initial_scroll: None,
            scroll_input: None,
            input: InputTrace::default(),
        }
    }

    fn with_keyboard_trace(mut self, input: &InputTrace) -> Self {
        self.input = input.clone();
        self
    }

    fn with_initial_scroll(mut self, id: widget::Id, offset: f32) -> Self {
        self.initial_scroll = Some((id, offset));
        self
    }

    fn with_scroll_input(mut self, input: Rc<Cell<bool>>) -> Self {
        self.scroll_input = Some(input);
        self
    }

    fn child_layout<'layout>(layout: Layout<'layout>) -> Layout<'layout> {
        layout
            .children()
            .next()
            .expect("workspace surface content layout")
            .with_virtual_offset(layout.virtual_offset())
    }
}

impl<Message> Widget<Message, Theme, Renderer> for WorkspaceSurface<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<WorkspaceSurfaceState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(WorkspaceSurfaceState::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let content = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
            .move_to(Point::ORIGIN);
        if let Some((id, offset)) = &self.initial_scroll {
            let state = tree.state.downcast_mut::<WorkspaceSurfaceState>();
            if state.initialized_scroll.as_ref() != Some(id) {
                // Startup operations may run before the strip exists. Seed the
                // new native scroll state after layout, even when its width
                // already equals the restored model's width. Do this only once
                // per strip; later user scrolling remains native state.
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    Layout::new(&content),
                    renderer,
                    &mut cosmic::iced::advanced::widget::operation::scrollable::scroll_to::<()>(
                        id.clone(),
                        widget::scrollable::AbsoluteOffset {
                            x: Some(offset.max(0.0)),
                            y: None,
                        },
                    ),
                );
                state.initialized_scroll = Some(id.clone());
            }
        }
        layout::Node::with_children(content.size(), vec![content])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Self::child_layout(layout),
                renderer,
                operation,
            );
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let content_layout = Self::child_layout(layout);

        match event {
            Event::Keyboard(
                keyboard::Event::KeyPressed {
                    key,
                    physical_key,
                    modifiers,
                    ..
                }
                | keyboard::Event::KeyReleased {
                    key,
                    physical_key,
                    modifiers,
                    ..
                },
            ) if is_global_shortcut_chord(
                key,
                *physical_key,
                *modifiers,
                self.shortcuts,
                self.input.keyboard_focus().unwrap_or(self.focused_pane),
            ) =>
            {
                shell.capture_event();
                return;
            }
            _ => {}
        }
        if !should_forward_to_raw_children(shell.is_event_captured()) {
            return;
        }
        // Native controls may ignore an editing chord. Ignored input still
        // belongs to that control, rather than the selected raw terminal or
        // browser behind it. In-pane find/address controls retain their card.
        if matches!(
            event,
            Event::Keyboard(keyboard::Event::KeyPressed { .. }) | Event::InputMethod(_)
        ) && matches!(
            self.input.keyboard_focus(),
            Some(FocusedPane::TextInput | FocusedPane::OutsideWorkspace | FocusedPane::SidebarRows | FocusedPane::SidebarFiles | FocusedPane::SidebarChanges)
        ) && self.input.keyboard_card().is_none()
        {
            return;
        }

        let Some(clipped) = clipped_viewport(layout.bounds(), viewport) else {
            return;
        };
        if let Some(input) = &self.scroll_input {
            input.set(matches!(
                event,
                Event::Mouse(mouse::Event::WheelScrolled { .. }) | Event::Touch(_)
            ));
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            content_layout,
            cursor,
            renderer,
            clipboard,
            shell,
            &clipped,
        );
        if let Some(input) = &self.scroll_input {
            input.set(false);
        }
        // Reported on redraw rather than from `layout`, because layout runs
        // during view building where publishing a message would fight the
        // reflow it triggers.
        // Publish after native scroll feedback from the old layout, so that
        // feedback cannot cancel the newly computed focused-card reveal.
        //
        // The comparison is against the width the owner already holds rather
        // than against a width remembered here, because the shell re-runs
        // this whole pass for the same frame while any pass publishes, and
        // rebuilds the view in between. A remembered width would be reported
        // afresh by a pass that changed nothing the owner reads - a height
        // change, or a strip remounted with fresh widget state - and each
        // such report costs another full update-and-layout pass. Comparing
        // against the owner's own value makes the second pass over one frame
        // silent as soon as the owner has stored what was reported.
        if let Event::Window(window::Event::RedrawRequested(_)) = event {
            let measured = layout.bounds().width;
            if measured.is_finite() && measured >= 0.0 && measured != self.reported_width {
                shell.publish((self.on_resize)(measured));
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        if let Some(clipped) = clipped_viewport(layout.bounds(), viewport) {
            self.content.as_widget().mouse_interaction(
                &tree.children[0],
                Self::child_layout(layout),
                cursor,
                &clipped,
                renderer,
            )
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let Some(clipped) = clipped_viewport(layout.bounds(), viewport) else {
            return;
        };
        renderer.with_layer(clipped, |renderer| {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                Self::child_layout(layout),
                cursor,
                &clipped,
            );
        });
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        // Same contract as the card surface: during `overlay` the layout is in
        // the coordinate space of whatever scrolls this surface, and
        // `translation` is what puts it on screen. The strip mounts above the
        // workspace scrollable so the translation is usually zero, but reading
        // it keeps the surface correct the day something scrolls it.
        let clipped = clipped_viewport(layout.bounds() + translation, viewport)?;
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            Self::child_layout(layout),
            renderer,
            &clipped,
            translation,
        )
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        state: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content
            .as_widget()
            .a11y_nodes(Self::child_layout(layout), &state.children[0], cursor)
    }

    fn drag_destinations(
        &self,
        state: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        let mut inner = cosmic::iced::advanced::clipboard::DndDestinationRectangles::new();
        self.content.as_widget().drag_destinations(
            &state.children[0],
            Self::child_layout(layout),
            renderer,
            &mut inner,
        );
        let mut rectangles = inner.into_rectangles();
        clip_drag_destinations(&mut rectangles, layout.bounds());
        destinations.append(&mut rectangles);
    }
}

/// Keeps only the part of each drop target that the strip shows.
///
/// The strip mounts cards past its edges and scrolls them without clipping
/// their drop targets, so a card scrolled half out of view would otherwise
/// reach under the sidebar and take a drop aimed there.
fn clip_drag_destinations(rectangles: &mut Vec<DndDestinationRectangle>, bounds: Rectangle) {
    let (left, top) = (f64::from(bounds.x), f64::from(bounds.y));
    let (right, bottom) = (
        left + f64::from(bounds.width),
        top + f64::from(bounds.height),
    );
    rectangles.retain_mut(|destination| {
        let rectangle = &mut destination.rectangle;
        let x = rectangle.x.max(left);
        let y = rectangle.y.max(top);
        let width = (rectangle.x + rectangle.width).min(right) - x;
        let height = (rectangle.y + rectangle.height).min(bottom) - y;
        *rectangle = cosmic::iced::clipboard::dnd::Rectangle {
            x,
            y,
            width,
            height,
        };
        width > 0.0 && height > 0.0
    });
}

impl<'a, Message: 'a> From<WorkspaceSurface<'a, Message>> for Element<'a, Message> {
    fn from(surface: WorkspaceSurface<'a, Message>) -> Self {
        Element::new(surface)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn palette_command_registry_ids_are_unique_and_context_capabilities_gate_actions() {
        use super::*;
        let mut ids = std::collections::HashSet::new();
        for descriptor in GLOBAL_SHORTCUT_DESCRIPTORS {
            assert!(
                ids.insert(descriptor.id()),
                "duplicate command id {}",
                descriptor.id()
            );
            assert!(!descriptor.category().is_empty());
            assert_eq!(
                descriptor.default_chords(),
                shortcut_keys(descriptor.action, &KeyboardShortcuts::default())
            );
        }
        let context = CommandContext::default();
        for command in [
            GlobalShortcut::NewWorktree,
            GlobalShortcut::CloseOtherPanes,
            GlobalShortcut::ClosePanesToRight,
            GlobalShortcut::CopyPath,
            GlobalShortcut::ForkConversation,
            GlobalShortcut::FocusSidebarTree,
            GlobalShortcut::FocusProject1,
            GlobalShortcut::RestartDaemon,
        ] {
            assert!(
                !shortcut_descriptor(command).available(&context),
                "{command:?}"
            );
        }
        let context = CommandContext {
            focused: FocusedPane::Chat,
            pane_index: Some(0),
            pane_count: 2,
            project_count: 1,
            sidebar_has_rows: true,
            active_git_project: true,
            pane_has_path: true,
            can_fork: true,
            daemon_can_restart: true,
            ..CommandContext::default()
        };
        for command in [
            GlobalShortcut::NewWorktree,
            GlobalShortcut::CloseOtherPanes,
            GlobalShortcut::ClosePanesToRight,
            GlobalShortcut::CopyPath,
            GlobalShortcut::ForkConversation,
            GlobalShortcut::FocusSidebarTree,
            GlobalShortcut::FocusProject1,
            GlobalShortcut::RestartDaemon,
        ] {
            assert!(
                shortcut_descriptor(command).available(&context),
                "{command:?}"
            );
        }
        assert!(!shortcut_descriptor(GlobalShortcut::FocusProject2).available(&context));
        assert!(
            !shortcut_descriptor(GlobalShortcut::ClosePanesToRight).available(&CommandContext {
                pane_index: Some(1),
                ..context
            })
        );
        assert!(
            !shortcut_descriptor(GlobalShortcut::ForkConversation).available(&CommandContext {
                focused: FocusedPane::Terminal,
                ..context
            })
        );
    }

    #[test]
    fn palette_menu_hint_tracks_the_current_configured_chord() {
        use super::*;
        let mut settings = KeyboardShortcuts::default();
        assert!(menu_shortcut(GlobalShortcut::CopyPath, &settings).is_none());
        settings.copy_path = vec!["Ctrl+Alt+Shift+K".into()];
        let hint = menu_shortcut(GlobalShortcut::CopyPath, &settings).unwrap();
        assert!(hint.matches(
            keyboard::Modifiers::CTRL | keyboard::Modifiers::ALT | keyboard::Modifiers::SHIFT,
            &keyboard::Key::Character("k".into()),
            None
        ));
        settings.copy_path.clear();
        assert!(menu_shortcut(GlobalShortcut::CopyPath, &settings).is_none());
    }

    use super::*;
    use crate::settings::parse_chords;
    use crate::workspace::{CardWidth, WorkspaceCard};
    use std::collections::HashSet;

    #[test]
    fn drop_targets_end_at_the_edges_of_the_strip() {
        use cosmic::iced::clipboard::dnd::{DndAction, Rectangle as DndRectangle};

        let destination = |id, x| DndDestinationRectangle {
            id,
            rectangle: DndRectangle {
                x,
                y: 0.0,
                width: 400.0,
                height: 600.0,
            },
            mime_types: Vec::new(),
            actions: DndAction::Copy,
            preferred: DndAction::Copy,
        };
        // A strip beside a 240-wide sidebar, under a 40-high header.
        let strip = Rectangle::new(Point::new(240.0, 40.0), Size::new(800.0, 500.0));
        let mut rectangles = vec![
            destination(1, 0.0),
            destination(2, 400.0),
            destination(3, 1040.0),
        ];
        clip_drag_destinations(&mut rectangles, strip);
        let kept: Vec<_> = rectangles
            .iter()
            .map(|destination| {
                let rectangle = &destination.rectangle;
                (
                    destination.id,
                    rectangle.x,
                    rectangle.y,
                    rectangle.width,
                    rectangle.height,
                )
            })
            .collect();
        assert_eq!(
            kept,
            // The card scrolled half out of view keeps only its visible part;
            // the one wholly past the edge takes no drops at all.
            vec![
                (1, 240.0, 40.0, 160.0, 500.0),
                (2, 400.0, 40.0, 400.0, 500.0)
            ]
        );
    }

    #[test]
    fn card_cursor_uses_translated_child_coordinates_without_reviving_blocked_hover() {
        use cosmic::iced::advanced::{clipboard, renderer::Headless};
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let content = cosmic::widget::text_editor::Content::with_text("draft");
        let editor = widget::text_editor::TextEditor::new(&content)
            .width(100.0)
            .height(Length::Fill)
            .on_action(|_| "edit");
        let button = cosmic::widget::button::text("Action")
            .width(Length::Fixed(100.0))
            .height(Length::Fill)
            .on_press("click");
        let row = cosmic::widget::row::with_children(vec![editor.into(), button.into()])
            .height(Length::Fill);
        let mut surface: Element<'_, &str> =
            CardSurface::new(row).on_pointer_rest(Some("rest")).into();
        let mut tree = Tree::new(&surface);
        let node = surface
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::new(200.0, 80.0), Size::new(200.0, 80.0)),
            )
            .move_to(Point::new(100.0, 0.0));
        let viewport = Rectangle::new(Point::new(100.0, 0.0), Size::new(200.0, 80.0));
        let mut messages = Vec::new();
        let row_layout = Layout::new(&node).children().next().unwrap();
        let mut controls = row_layout.children();
        let editor_center = controls.next().unwrap().bounds().center();
        let button_center = controls.next().unwrap().bounds().center();
        for (position, expected) in [
            (editor_center, mouse::Interaction::Text),
            (button_center, mouse::Interaction::Pointer),
        ] {
            let cursor = mouse::Cursor::Available(position);
            surface.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::CursorMoved {
                    position: position - Vector::new(100.0, 0.0),
                }),
                Layout::new(&node),
                cursor,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            assert_eq!(
                surface.as_widget().mouse_interaction(
                    &tree,
                    Layout::new(&node),
                    cursor,
                    &viewport,
                    &renderer,
                ),
                expected
            );
            for blocked in [mouse::Cursor::Unavailable, cursor.levitate()] {
                assert_eq!(
                    surface.as_widget().mouse_interaction(
                        &tree,
                        Layout::new(&node),
                        blocked,
                        &viewport,
                        &renderer,
                    ),
                    mouse::Interaction::None
                );
            }
        }
        // Moving about inside the pane never crossed into it.
        assert!(messages.is_empty());
    }

    /// Drives one [`CardSurface`] spanning x 100..300 of the window, beneath
    /// the window's input trace, in the window's own coordinates unless a
    /// sample says otherwise.
    struct RestProbe {
        renderer: Renderer,
        surface: Element<'static, &'static str>,
        tree: Tree,
        node: layout::Node,
        messages: Vec<&'static str>,
        input: InputTrace,
        /// The cursor as the pane last saw it, which frames report too.
        cursor: mouse::Cursor,
    }

    impl RestProbe {
        const OUTSIDE: Point = Point::new(50.0, 40.0);
        const INSIDE: Point = Point::new(150.0, 40.0);
        /// Where the pointer is over the next pane along.
        const NEIGHBOUR: Point = Point::new(350.0, 40.0);

        fn new() -> Self {
            use cosmic::iced::advanced::renderer::Headless;
            let renderer = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(Renderer::new(
                    cosmic::iced::Font::default(),
                    cosmic::iced::Pixels(14.0),
                    Some("tiny-skia"),
                ))
                .unwrap();
            let input = InputTrace::default();
            let mut surface = crate::input_trace::observe(
                CardSurface::new(
                    widget::Space::new()
                        .width(Length::Fill)
                        .height(Length::Fill),
                )
                .traced_by(&input)
                .on_pointer_focus("click")
                .on_pointer_rest(Some("rest")),
                &input,
            );
            let mut tree = Tree::new(&surface);
            let node = surface
                .as_widget_mut()
                .layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::new(200.0, 80.0), Size::new(200.0, 80.0)),
                )
                .move_to(Point::new(100.0, 0.0));
            Self {
                renderer,
                surface,
                tree,
                node,
                messages: Vec::new(),
                input,
                cursor: mouse::Cursor::Unavailable,
            }
        }

        fn send(&mut self, event: Event, cursor: mouse::Cursor) {
            use cosmic::iced::advanced::clipboard;
            self.surface.as_widget_mut().update(
                &mut self.tree,
                &event,
                Layout::new(&self.node),
                cursor,
                &self.renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut self.messages),
                &Rectangle::new(Point::new(100.0, 0.0), Size::new(200.0, 80.0)),
            );
        }

        /// Input the window delivered somewhere other than the pane: to an
        /// overlay that captured it, or to a widget that captured it before
        /// the strip, or while the pane was not mounted.
        fn elsewhere(&self, event: &Event) {
            self.input.record(event);
        }

        /// The pointer at `window` in the window, which the pane sees as
        /// `local` once the strip's scroll translation is applied.
        fn move_translated(&mut self, window: Point, local: mouse::Cursor) {
            self.cursor = local;
            self.send(
                Event::Mouse(mouse::Event::CursorMoved { position: window }),
                local,
            );
        }

        fn move_to(&mut self, position: Point) {
            self.move_translated(position, mouse::Cursor::Available(position));
        }

        /// Delivers the frame `after` now.
        fn frame(&mut self, after: Duration) {
            self.frame_with(after, self.cursor);
        }

        /// Delivers the frame `after` now, with the pane seeing the cursor at
        /// `cursor` by then.
        fn frame_with(&mut self, after: Duration, cursor: mouse::Cursor) {
            self.send(
                Event::Window(window::Event::RedrawRequested(Instant::now() + after)),
                cursor,
            );
        }

        /// Frames before and after the dwell elapses, and everything the pane
        /// published since the last look.
        fn rest(&mut self) -> Vec<&'static str> {
            self.frame(Duration::ZERO);
            self.frame(HOVER_ACTIVATION_DWELL * 2);
            self.frame(HOVER_ACTIVATION_DWELL * 3);
            mem::take(&mut self.messages)
        }
    }

    fn moved(position: Point) -> Event {
        Event::Mouse(mouse::Event::CursorMoved { position })
    }

    #[test]
    fn a_pane_activates_on_hover_only_after_the_pointer_crosses_in_and_rests() {
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.frame(Duration::ZERO);
        assert!(probe.messages.is_empty(), "activation waits for the dwell");
        // Moving about inside the pane keeps the dwell it started, and the
        // rest is reported once however many frames follow.
        probe.move_to(Point::new(180.0, 60.0));
        assert_eq!(probe.rest(), ["rest"]);

        // Sweeping through on the way somewhere else activates nothing.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.move_to(RestProbe::NEIGHBOUR);
        assert!(probe.rest().is_empty());
    }

    #[test]
    fn typing_or_clicking_before_the_dwell_keeps_focus_where_it_is() {
        let key = Event::Keyboard(key_press(
            "a",
            keyboard::key::Code::KeyA,
            keyboard::Modifiers::empty(),
            false,
        ));
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.send(key.clone(), mouse::Cursor::Available(RestProbe::INSIDE));
        // Further drift inside the pane does not re-arm what typing cancelled.
        probe.move_to(Point::new(160.0, 40.0));
        assert!(probe.rest().is_empty());

        // A shortcut the workspace captures, or a key a focused input
        // elsewhere takes, never reaches the pane, and cancels it all the same.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.elsewhere(&key);
        assert!(probe.rest().is_empty());

        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.send(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            mouse::Cursor::Available(RestProbe::INSIDE),
        );
        // The click activates the pane itself; the dwell adds nothing.
        assert_eq!(probe.rest(), ["click"]);
    }

    /// The pane was entered, but by the time the dwell ends it is no longer
    /// where the pointer rests.
    #[test]
    fn a_pane_that_left_the_pointer_during_the_dwell_is_not_activated() {
        // Keyboard navigation or a new pane moves the strip, and the pointer,
        // which never moved, now rests over a neighbour.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.frame_with(
            HOVER_ACTIVATION_DWELL * 2,
            mouse::Cursor::Available(RestProbe::NEIGHBOUR),
        );
        // Nor does it activate once scrolled back under the pointer.
        assert!(probe.rest().is_empty());

        // The pane scrolled out of view, where it receives no frames, and its
        // first one back comes long after the pointer stopped over it.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.frame(HOVER_ACTIVATION_DWELL + HOVER_ACTIVATION_LATENESS * 2);
        assert!(probe.rest().is_empty());
    }

    /// A pane that comes out from under something the pointer was already
    /// over was not entered: the reader never moved toward it.
    #[test]
    fn uncovering_a_pane_under_a_still_pointer_is_not_an_entry() {
        // A dropdown drawn over the pane leaves it the motions beneath, but
        // with no cursor, and then closes under the pointer.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_translated(RestProbe::INSIDE, mouse::Cursor::Unavailable);
        assert!(probe.rest().is_empty(), "an overlay covers the pane");
        probe.move_to(Point::new(151.0, 40.0));
        assert!(probe.rest().is_empty());

        // A context menu opened from outside the pane captures every motion
        // over it, and then an item is chosen over the pane.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.elsewhere(&moved(RestProbe::INSIDE));
        probe.move_to(Point::new(151.0, 40.0));
        assert!(probe.rest().is_empty());

        // A popup surface outside the window closes, returning the pointer.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.send(
            Event::Mouse(mouse::Event::CursorLeft),
            mouse::Cursor::Unavailable,
        );
        probe.move_to(RestProbe::INSIDE);
        assert!(probe.rest().is_empty());

        // The strip scrolls the pane under a pointer that stays put.
        let mut probe = RestProbe::new();
        probe.move_translated(
            RestProbe::INSIDE,
            mouse::Cursor::Available(RestProbe::NEIGHBOUR),
        );
        probe.move_to(Point::new(151.0, 40.0));
        assert!(probe.rest().is_empty());

        // A pane mounted beneath the pointer has no sample to cross from.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::INSIDE);
        assert!(probe.rest().is_empty());

        // A pane remounted after a far jump last saw the pointer where it was
        // before; the pointer has since moved on beneath other panes and
        // stopped where the pane now comes to lie.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.elsewhere(&moved(Point::new(149.0, 40.0)));
        probe.elsewhere(&moved(RestProbe::INSIDE));
        probe.move_to(Point::new(151.0, 40.0));
        assert!(probe.rest().is_empty());
    }

    #[test]
    fn parked_editor_preserves_focus_across_named_state_extraction() {
        use cosmic::iced::advanced::{renderer::Headless, widget::operation};
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let ids = [CardId::new(), CardId::new()];
        let retained = Arc::new(HashMap::from([(ids[0], 0), (ids[1], 1)]));
        let content = cosmic::widget::text_editor::Content::with_text("retained draft");
        let editor = widget::Id::new("parked-editor");
        let make = |index: usize| -> Element<'_, ()> {
            let child = widget::text_editor::TextEditor::new(&content)
                .id(if index == 0 {
                    editor.clone()
                } else {
                    widget::Id::new("other-editor")
                })
                .on_action(|_| ());
            VirtualStrip {
                total_width: 320.0,
                retained: retained.clone(),
                focused: Some(ids[0]),
                cards: vec![VirtualCard {
                    id: ids[index],
                    x: 0.0,
                    width: 320.0,
                    content_width: 320.0,
                    on_screen: true,
                    content: cosmic::widget::id_container(
                        child,
                        widget::Id::new(format!("parked-card-{index}")),
                    )
                    .into(),
                }],
            }
            .into()
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(320.0, 200.0));
        let mut element = make(0);
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(editor.clone()),
        );
        for index in [1, 1, 0] {
            // Reproduce the real runtime, which extracts names before every
            // view diff and drops unclaimed names at the next frame.
            tree::NAMED.with(|cache| *cache.borrow_mut() = tree.take_all_named());
            element = make(index);
            tree.diff(&mut element);
            element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
        }
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut focused = operation::focusable::find_focused();
        let mut probe = operation::black_box::<_, ()>(&mut focused);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
        drop(probe);
        assert!(matches!(focused.finish(), operation::Outcome::Some(id) if id == editor));
        tree::NAMED.with(|cache| cache.borrow_mut().clear());
    }

    #[test]
    fn remounting_a_retargeted_editor_does_not_restore_stale_focus() {
        use cosmic::iced::advanced::{renderer::Headless, widget::operation};

        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let ids = [CardId::new(), CardId::new()];
        let editors = [
            widget::Id::new("retarget-old"),
            widget::Id::new("retarget-new"),
        ];
        let retained = Arc::new(HashMap::from([(ids[0], 0), (ids[1], 1)]));
        let contents = [
            cosmic::widget::text_editor::Content::with_text("old destination draft"),
            cosmic::widget::text_editor::Content::with_text("new destination draft"),
        ];
        let make = |indices: &[usize], focused: usize| -> Element<'_, ()> {
            VirtualStrip {
                total_width: 640.0,
                retained: retained.clone(),
                focused: Some(ids[focused]),
                cards: indices
                    .iter()
                    .map(|&index| VirtualCard {
                        id: ids[index],
                        x: index as f32 * 320.0,
                        width: 320.0,
                        content_width: 320.0,
                        on_screen: true,
                        content: cosmic::widget::id_container(
                            widget::text_editor::TextEditor::new(&contents[index])
                                .id(editors[index].clone())
                                .on_action(|_| ()),
                            widget::Id::new(format!("retarget-card-{index}")),
                        )
                        .into(),
                    })
                    .collect(),
            }
            .into()
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(640.0, 200.0));
        let mut element = make(&[0], 0);
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(editors[0].clone()),
        );

        // Retargeting parks the old destination before the focus operation
        // visits the newly mounted destination. The parked tree is unreachable.
        tree::NAMED.with(|cache| *cache.borrow_mut() = tree.take_all_named());
        element = make(&[1], 1);
        tree.diff(&mut element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(editors[1].clone()),
        );

        // Camera movement brings the old destination back without requesting
        // widget focus. Both editors are now traversable in the same strip.
        tree::NAMED.with(|cache| *cache.borrow_mut() = tree.take_all_named());
        element = make(&[0, 1], 1);
        tree.diff(&mut element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut focus_count = FocusedWidgets::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut focus_count);
        assert_eq!(focus_count.0, vec![editors[1].clone()]);

        // The diff implementation also temporarily parks cards that remain
        // mounted while their neighbours leave. That storage shuffle must
        // not blur a control that never left the traversable widget tree.
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(editors[0].clone()),
        );
        tree::NAMED.with(|cache| *cache.borrow_mut() = tree.take_all_named());
        element = make(&[0], 1);
        tree.diff(&mut element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut focus_count = FocusedWidgets::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut focus_count);
        assert_eq!(focus_count.0, vec![editors[0].clone()]);
        tree::NAMED.with(|cache| cache.borrow_mut().clear());
    }

    #[test]
    fn loading_focus_preserves_anonymous_and_sidebar_controls() {
        use cosmic::iced::advanced::widget::operation::{Focusable, Outcome};
        use cosmic::iced::futures::{StreamExt, executor::block_on};

        struct Focus(bool);
        impl Focusable for Focus {
            fn is_focused(&self) -> bool {
                self.0
            }
            fn focus(&mut self) {
                self.0 = true;
            }
            fn unfocus(&mut self) {
                self.0 = false;
            }
        }

        for foreign_id in [None, Some(widget::Id::new("sidebar.search"))] {
            let placeholder = widget::Id::new("loading-pane.header");
            let target = widget::Id::new("loaded-pane.editor");
            let mut header = Focus(false);
            let mut foreign = Focus(true);
            let mut editor = Focus(false);
            let mut stream = cosmic::iced::runtime::task::into_stream(focus_after_loading::<()>(
                placeholder.clone(),
                target.clone(),
            ))
            .expect("loading handoff must issue a widget operation");
            while let Some(action) = block_on(stream.next()) {
                let cosmic::iced::runtime::Action::Widget(mut operation) = action else {
                    continue;
                };
                loop {
                    let bounds = Rectangle::default();
                    operation.focusable(Some(&placeholder), bounds, &mut header);
                    operation.focusable(foreign_id.as_ref(), bounds, &mut foreign);
                    operation.focusable(Some(&target), bounds, &mut editor);
                    match operation.finish() {
                        Outcome::Chain(next) => operation = next,
                        _ => break,
                    }
                }
            }
            assert!(foreign.0, "loading must preserve the chosen control");
            assert!(!editor.0, "the loaded editor must not take its keyboard");
        }
    }

    #[derive(Default)]
    struct FocusedWidgets(Vec<widget::Id>);

    impl Operation for FocusedWidgets {
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            _bounds: Rectangle,
            state: &mut dyn cosmic::iced::advanced::widget::operation::Focusable,
        ) {
            if state.is_focused()
                && let Some(id) = id
            {
                self.0.push(id.clone());
            }
        }

        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
    }

    #[test]
    fn projection_never_exposes_offscreen_cards_to_the_widget_builder() {
        let mut workspace = Workspace::default();
        workspace.resize_viewport(320.0);
        for index in 0..20 {
            workspace
                .add_card(
                    WorkspaceCard::new(
                        CardId::new(),
                        format!("Card {index}"),
                        CardWidth::Fixed(320.0),
                    )
                    .unwrap(),
                    index,
                    false,
                )
                .unwrap();
        }
        workspace.native_scroll(3_200.0, 320.0);
        let projection = workspace.projection(320.0);
        assert!(projection.mounted.len() < workspace.cards().len());
        assert!(
            projection
                .mounted
                .iter()
                .all(|mounted| mounted.visibility.is_mounted())
        );
    }
    #[test]
    fn an_opening_card_draws_and_takes_the_pointer_only_inside_its_slot() {
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(900.0, 600.0));
        let card = |width, content_width| VirtualCard::<()> {
            id: CardId::new(),
            x: 300.0,
            width,
            content_width,
            on_screen: true,
            content: widget::Space::new().into(),
        };
        let bounds = |card: &VirtualCard<'_, ()>| {
            Rectangle::new(
                Point::new(card.x, 0.0),
                Size::new(card.content_width, 600.0),
            )
        };
        let inside = mouse::Cursor::Available(Point::new(320.0, 50.0));
        let beside = mouse::Cursor::Available(Point::new(420.0, 50.0));

        let opening = card(60.0, 300.0);
        let slot = opening.viewport(bounds(&opening), &viewport);
        assert_eq!(
            slot,
            Rectangle::new(Point::new(300.0, 0.0), Size::new(60.0, 600.0))
        );
        assert_eq!(opening.cursor(&slot, inside), inside);
        // Its contents reach under the neighbour there, which owns the pointer.
        assert!(beside.is_over(bounds(&opening)));
        assert_eq!(opening.cursor(&slot, beside), mouse::Cursor::Unavailable);

        // A slot past the viewport's edge shows and receives nothing.
        let hidden = Rectangle::new(Point::ORIGIN, Size::new(250.0, 600.0));
        let slot = opening.viewport(bounds(&opening), &hidden);
        assert!(!bounds(&opening).intersects(&slot));
        assert_eq!(opening.cursor(&slot, inside), mouse::Cursor::Unavailable);

        // A settled card keeps the strip's viewport and every pointer event,
        // including the moves and releases that end a drag outside it.
        let settled = card(300.0, 300.0);
        assert_eq!(settled.viewport(bounds(&settled), &viewport), viewport);
        let outside = mouse::Cursor::Available(Point::new(10.0, 50.0));
        assert_eq!(settled.cursor(&viewport, outside), outside);
    }

    #[test]
    fn mounted_column_layout_and_clips_are_pairwise_disjoint() {
        let mut workspace = Workspace::default();
        workspace.resize_viewport(700.0);
        for (index, width) in [
            CardWidth::Fraction(1.0),
            CardWidth::Fixed(900.0),
            CardWidth::Fraction(0.45),
        ]
        .into_iter()
        .enumerate()
        {
            workspace
                .add_card(
                    WorkspaceCard::new(CardId::new(), format!("Card {index}"), width).unwrap(),
                    index,
                    index == 0,
                )
                .unwrap();
        }
        workspace.native_scroll(500.0, 700.0);
        let projection = workspace.projection(700.0);
        let viewport = Rectangle::new(Point::new(500.0, 0.0), Size::new(700.0, 640.0));
        let columns: Vec<_> = projection
            .mounted
            .iter()
            .map(|card| {
                Rectangle::new(
                    Point::new(card.start, 0.0),
                    Size::new(card.width, viewport.height),
                )
            })
            .collect();

        assert!(projection.mounted_are_disjoint());
        assert!(
            columns
                .windows(2)
                .all(|pair| pair[0].x + pair[0].width <= pair[1].x)
        );
        for (column, clip) in columns
            .iter()
            .filter_map(|column| clipped_viewport(*column, &viewport).map(|clip| (column, clip)))
        {
            assert!(clip.x >= column.x);
            assert!(clip.x + clip.width <= column.x + column.width);
            assert!(clip.y >= column.y);
            assert!(clip.y + clip.height <= column.y + column.height);
            assert!(clip.x >= viewport.x);
            assert!(clip.x + clip.width <= viewport.x + viewport.width);
        }
    }

    #[test]
    fn card_body_starts_below_header_and_remains_inside_its_column_clip() {
        const HEADER_HEIGHT: f32 = 44.0;
        let column = Rectangle::new(Point::new(12.0, 0.0), Size::new(480.0, 640.0));
        let viewport = Rectangle::new(Point::new(0.0, 0.0), Size::new(700.0, 640.0));
        let card_clip = clipped_viewport(column, &viewport).unwrap();
        let body = Rectangle::new(
            Point::new(column.x, column.y + HEADER_HEIGHT),
            Size::new(column.width, column.height - HEADER_HEIGHT),
        );
        let body_clip = clipped_viewport(body, &card_clip).unwrap();

        assert!(body_clip.y >= column.y + HEADER_HEIGHT);
        assert!(body_clip.x >= card_clip.x);
        assert!(body_clip.x + body_clip.width <= card_clip.x + card_clip.width);
        assert!(body_clip.y + body_clip.height <= card_clip.y + card_clip.height);
    }

    #[test]
    fn frame_filter_rejects_other_windows() {
        let expected = window::Id::unique();
        let other = window::Id::unique();
        let instant = Instant::now();
        let match_frame = |(wanted, (actual, at)): (window::Id, (window::Id, Instant))| {
            (wanted == actual).then_some(at)
        };
        assert_eq!(match_frame((expected, (expected, instant))), Some(instant));
        assert_eq!(match_frame((expected, (other, instant))), None);
    }

    #[test]
    fn card_container_and_title_focus_ids_are_stable_and_distinct() {
        let first = CardId::new();
        let second = CardId::new();
        assert_eq!(
            card_widget_id(first).to_string(),
            card_widget_id(first).to_string()
        );
        assert_eq!(
            card_focus_id(first).to_string(),
            card_focus_id(first).to_string()
        );
        assert_ne!(
            card_widget_id(first).to_string(),
            card_focus_id(first).to_string()
        );
        assert_ne!(
            card_focus_id(first).to_string(),
            card_focus_id(second).to_string()
        );
    }

    #[test]
    fn viewport_events_keep_scroll_offsets_out_of_resize_notifications() {
        let scrolled = WorkspaceViewportEvent::Scrolled(ViewportSample {
            offset: 240.0,
            width: 800.0,
            user_input: true,
        });
        let resized = WorkspaceViewportEvent::Resized(800.0);

        assert_eq!(
            scrolled,
            WorkspaceViewportEvent::Scrolled(ViewportSample {
                offset: 240.0,
                width: 800.0,
                user_input: true,
            })
        );
        assert_eq!(resized, WorkspaceViewportEvent::Resized(800.0));
        assert_ne!(scrolled, resized);
    }

    fn key_press(
        character: &str,
        code: keyboard::key::Code,
        modifiers: keyboard::Modifiers,
        repeat: bool,
    ) -> keyboard::Event {
        let key = keyboard::Key::Character(character.into());
        keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat,
        }
    }

    fn named_key_press(
        named: keyboard::key::Named,
        code: keyboard::key::Code,
        modifiers: keyboard::Modifiers,
        repeat: bool,
    ) -> keyboard::Event {
        let key = keyboard::Key::Named(named);
        keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat,
        }
    }

    #[test]
    fn editing_focus_preserves_redo_and_sidebar_find() {
        use keyboard::key::Code;
        let shortcuts = KeyboardShortcuts::default();
        let redo = key_press(
            "z",
            Code::KeyZ,
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            false,
        );
        for focused in [
            FocusedPane::Editor,
            FocusedPane::Chat,
            FocusedPane::TextInput,
        ] {
            assert_eq!(global_shortcut(&redo, &shortcuts, focused), (focused == FocusedPane::Editor).then_some(GlobalShortcut::EditorRedo));
            assert!(!is_global_shortcut_chord(
                &keyboard::Key::Character("z".into()),
                keyboard::key::Physical::Code(Code::KeyZ),
                keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
                &shortcuts,
                focused
            ));
        }
        assert_eq!(
            global_shortcut(&redo, &shortcuts, FocusedPane::Terminal),
            Some(GlobalShortcut::ReopenPane)
        );
        assert_eq!(
            global_shortcut(
                &key_press("f", Code::KeyF, keyboard::Modifiers::CTRL, false),
                &shortcuts,
                FocusedPane::TextInput
            ),
            None
        );
        assert_eq!(
            global_shortcut(
                &key_press("t", Code::KeyT, keyboard::Modifiers::CTRL, false),
                &shortcuts,
                FocusedPane::TextInput
            ),
            None
        );
    }

    #[test]
    fn a_custom_action_cannot_replace_find_in_a_focused_editor() {
        use keyboard::key::Code;
        let mut shortcuts = KeyboardShortcuts::default();
        shortcuts.find_in_pane.clear();
        shortcuts.rename_pane = vec!["Ctrl+F".into()];
        let event = key_press("f", Code::KeyF, keyboard::Modifiers::CTRL, false);
        assert_eq!(
            global_shortcut(&event, &shortcuts, FocusedPane::Editor),
            None
        );
        assert_eq!(
            global_shortcut(&event, &shortcuts, FocusedPane::Other),
            Some(GlobalShortcut::RenamePane)
        );
    }

    #[test]
    fn terminal_defaults_preserve_shell_chords_and_offer_shifted_find() {
        use keyboard::key::{Code, Named};
        let shortcuts = KeyboardShortcuts::default();
        for (key, code, modifiers) in [
            ("f", Code::KeyF, keyboard::Modifiers::CTRL),
            ("w", Code::KeyW, keyboard::Modifiers::CTRL),
            ("t", Code::KeyT, keyboard::Modifiers::CTRL),
            ("r", Code::KeyR, keyboard::Modifiers::ALT),
        ] {
            assert_eq!(
                global_shortcut(
                    &key_press(key, code, modifiers, false),
                    &shortcuts,
                    FocusedPane::Terminal
                ),
                None
            );
        }
        for (key, code) in [
            ("1", Code::Digit1),
            ("2", Code::Digit2),
            ("3", Code::Digit3),
            ("4", Code::Digit4),
            ("5", Code::Digit5),
            ("6", Code::Digit6),
            ("7", Code::Digit7),
            ("8", Code::Digit8),
            ("9", Code::Digit9),
        ] {
            assert_eq!(
                global_shortcut(
                    &key_press(key, code, keyboard::Modifiers::ALT, false),
                    &shortcuts,
                    FocusedPane::Terminal
                ),
                None
            );
        }
        assert_eq!(
            global_shortcut(
                &named_key_press(Named::F2, Code::F2, keyboard::Modifiers::empty(), false),
                &shortcuts,
                FocusedPane::Terminal
            ),
            None
        );
        let find = key_press(
            "f",
            Code::KeyF,
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            false,
        );
        assert_eq!(
            global_shortcut(&find, &shortcuts, FocusedPane::Terminal),
            Some(GlobalShortcut::FindInPane)
        );
        let mut passthrough = shortcuts;
        passthrough
            .terminal_passthrough_actions
            .push("search_projects".into());
        assert_eq!(
            global_shortcut(&find, &passthrough, FocusedPane::Terminal),
            None
        );
    }

    #[test]
    fn global_shortcuts_match_configured_chords_and_repeat_presses() {
        use keyboard::key::Code;

        let shortcuts = KeyboardShortcuts::default();
        let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
        for (character, code, modifiers, action) in [
            ("T", Code::KeyT, ctrl_shift, GlobalShortcut::NewTerminal),
            ("G", Code::KeyG, ctrl_shift, GlobalShortcut::NewChannel),
            ("D", Code::KeyD, ctrl_shift, GlobalShortcut::ClonePane),
            (
                "W",
                Code::KeyW,
                ctrl_shift,
                GlobalShortcut::CloseFocusedCard,
            ),
        ] {
            assert_eq!(
                global_shortcut(
                    &key_press(character, code, modifiers, false),
                    &shortcuts,
                    FocusedPane::Terminal,
                ),
                Some(action)
            );
            assert_eq!(
                global_shortcut(
                    &key_press(character, code, modifiers, true),
                    &shortcuts,
                    FocusedPane::Terminal,
                ),
                None,
                "holding a creation or close shortcut must not repeat it"
            );
            assert!(is_global_shortcut_chord(
                &keyboard::Key::Character(character.into()),
                keyboard::key::Physical::Code(code),
                modifiers,
                &shortcuts,
                FocusedPane::Terminal,
            ));
        }
        assert_eq!(
            global_shortcut(
                &key_press(
                    "F",
                    Code::KeyF,
                    keyboard::Modifiers::CTRL | keyboard::Modifiers::CAPS_LOCK,
                    false,
                ),
                &shortcuts,
                FocusedPane::Other,
            ),
            Some(GlobalShortcut::FindInPane)
        );
        assert_eq!(
            global_shortcut(
                &key_press("f", Code::KeyF, keyboard::Modifiers::CTRL, true),
                &shortcuts,
                FocusedPane::Other,
            ),
            None,
            "held Ctrl shortcuts must not repeat"
        );
        for repeat in [false, true] {
            assert_eq!(
                global_shortcut(
                    &key_press("A", Code::KeyA, keyboard::Modifiers::ALT, repeat),
                    &shortcuts,
                    FocusedPane::Other,
                ),
                None,
                "Alt+A must be forwarded on initial and repeated key presses"
            );
        }

        // Column navigation repeats while held, and is reserved against the
        // process inside a card.
        for (named, code, direction) in [
            (
                keyboard::key::Named::PageUp,
                Code::PageUp,
                FocusDirection::Left,
            ),
            (
                keyboard::key::Named::PageDown,
                Code::PageDown,
                FocusDirection::Right,
            ),
        ] {
            for repeat in [false, true] {
                let event = named_key_press(named, code, keyboard::Modifiers::CTRL, repeat);
                assert_eq!(
                    global_shortcut(&event, &shortcuts, FocusedPane::Terminal),
                    Some(GlobalShortcut::FocusAdjacent(direction)),
                    "Ctrl+Page must navigate on initial and repeated presses"
                );
                assert!(is_global_shortcut_chord(
                    &keyboard::Key::Named(named),
                    keyboard::key::Physical::Code(code),
                    keyboard::Modifiers::CTRL,
                    &shortcuts,
                    FocusedPane::Terminal,
                ));
            }
            // The unmodified and shifted paging keys stay the terminal's.
            for modifiers in [keyboard::Modifiers::empty(), keyboard::Modifiers::SHIFT] {
                assert!(!is_global_shortcut_chord(
                    &keyboard::Key::Named(named),
                    keyboard::key::Physical::Code(code),
                    modifiers,
                    &shortcuts,
                    FocusedPane::Terminal,
                ));
            }
        }

        let mut customized = shortcuts;
        customized.new_terminal = vec!["Ctrl+Shift+N".to_owned()];
        customized.new_native_thread.clear();
        assert_eq!(
            global_shortcut(
                &key_press("n", Code::KeyN, ctrl_shift, false),
                &customized,
                FocusedPane::Other,
            ),
            Some(GlobalShortcut::NewTerminal)
        );
        customized.new_terminal = vec!["Ctrl+Shift+1".to_owned()];
        assert_eq!(
            global_shortcut(
                &key_press("!", Code::Digit1, ctrl_shift, false),
                &customized,
                FocusedPane::Other,
            ),
            Some(GlobalShortcut::NewTerminal)
        );
    }

    #[test]
    fn new_tab_chord_is_reserved_outside_terminals_and_does_not_repeat() {
        use keyboard::key::Code;

        let shortcuts = KeyboardShortcuts::default();
        for focused in [FocusedPane::Other, FocusedPane::Review] {
            assert!(is_global_shortcut_chord(
                &keyboard::Key::Character("t".into()),
                keyboard::key::Physical::Code(Code::KeyT),
                keyboard::Modifiers::CTRL,
                &shortcuts,
                focused,
            ));
            for repeat in [false, true] {
                assert_eq!(
                    global_shortcut(
                        &key_press("t", Code::KeyT, keyboard::Modifiers::CTRL, repeat),
                        &shortcuts,
                        focused,
                    ),
                    (!repeat).then_some(GlobalShortcut::NewThread),
                );
            }
        }
    }

    /// Shell chords outside the workspace's defaults reach a focused terminal.
    #[test]
    fn default_shortcuts_leave_shell_keys_to_a_focused_terminal() {
        use keyboard::key::{Code, Named};

        let shortcuts = KeyboardShortcuts::default();
        let character = |character: &str, code, modifiers| {
            (
                keyboard::Key::Character(character.into()),
                keyboard::key::Physical::Code(code),
                modifiers,
            )
        };
        let named = |named, code, modifiers| {
            (
                keyboard::Key::Named(named),
                keyboard::key::Physical::Code(code),
                modifiers,
            )
        };
        let ctrl = keyboard::Modifiers::CTRL;
        let alt = keyboard::Modifiers::ALT;
        for (key, physical, modifiers) in [
            character("r", Code::KeyR, ctrl),
            character("e", Code::KeyE, ctrl),
            character("g", Code::KeyG, ctrl),
            character("p", Code::KeyP, ctrl),
            character("q", Code::KeyQ, alt),
            character("w", Code::KeyW, alt),
            character("e", Code::KeyE, alt),
            character("g", Code::KeyG, alt),
            character("f", Code::KeyF, alt),
            character("b", Code::KeyB, alt),
            character(".", Code::Period, alt),
            named(Named::ArrowLeft, Code::ArrowLeft, alt),
            named(Named::ArrowRight, Code::ArrowRight, alt),
            named(Named::ArrowLeft, Code::ArrowLeft, ctrl),
            named(Named::ArrowRight, Code::ArrowRight, ctrl),
            named(Named::ArrowUp, Code::ArrowUp, alt),
            named(Named::ArrowDown, Code::ArrowDown, alt),
        ] {
            assert!(
                !is_global_shortcut_chord(
                    &key,
                    physical,
                    modifiers,
                    &shortcuts,
                    FocusedPane::Terminal
                ),
                "{modifiers:?}+{key:?} was withheld from a focused terminal"
            );
            let press = keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key.clone(),
                physical_key: physical,
                location: keyboard::Location::Standard,
                modifiers,
                text: None,
                repeat: false,
            };
            assert_eq!(
                global_shortcut(&press, &shortcuts, FocusedPane::Terminal),
                None,
                "{modifiers:?}+{key:?} acted over a focused terminal"
            );
        }
    }

    /// Project search and pane find are distinct commands from every pane.
    #[test]
    fn project_search_and_pane_find_have_distinct_workspace_chords() {
        use keyboard::key::Code;

        let shortcuts = KeyboardShortcuts::default();
        for focused in [FocusedPane::Other, FocusedPane::Review, FocusedPane::Chat] {
            for (modifiers, action) in [
                (keyboard::Modifiers::CTRL, GlobalShortcut::FindInPane),
                (
                    keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
                    GlobalShortcut::FocusProjectSearch,
                ),
            ] {
                let event = key_press("f", Code::KeyF, modifiers, false);
                assert_eq!(global_shortcut(&event, &shortcuts, focused), Some(action));
                assert!(is_global_shortcut_chord(
                    &keyboard::Key::Character("f".into()),
                    keyboard::key::Physical::Code(Code::KeyF),
                    modifiers,
                    &shortcuts,
                    focused,
                ));
            }
        }

        // A custom close chord releases the default to the terminal.
        let customized = KeyboardShortcuts {
            close_focused_column: vec!["Ctrl+Shift+W".into()],
            ..KeyboardShortcuts::default()
        };
        assert_eq!(
            global_shortcut(
                &key_press(
                    "w",
                    Code::KeyW,
                    keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
                    false
                ),
                &customized,
                FocusedPane::Terminal,
            ),
            Some(GlobalShortcut::CloseFocusedCard)
        );
        assert_eq!(
            global_shortcut(
                &key_press("w", Code::KeyW, keyboard::Modifiers::CTRL, false),
                &customized,
                FocusedPane::Terminal
            ),
            None
        );
    }

    #[test]
    fn pane_close_and_width_defaults_apply_outside_a_terminal() {
        use keyboard::key::Code;
        let shortcuts = KeyboardShortcuts::default();
        for focused in [FocusedPane::Review, FocusedPane::Other] {
            for (character, code, modifiers, action) in [
                (
                    "w",
                    Code::KeyW,
                    keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
                    GlobalShortcut::CloseFocusedCard,
                ),
                (
                    "r",
                    Code::KeyR,
                    keyboard::Modifiers::ALT,
                    GlobalShortcut::CyclePaneWidth,
                ),
            ] {
                assert!(is_global_shortcut_chord(
                    &keyboard::Key::Character(character.into()),
                    keyboard::key::Physical::Code(code),
                    modifiers,
                    &shortcuts,
                    focused
                ));
                assert_eq!(
                    global_shortcut(
                        &key_press(character, code, modifiers, false),
                        &shortcuts,
                        focused
                    ),
                    Some(action)
                );
                assert_eq!(
                    global_shortcut(
                        &key_press(character, code, modifiers, true),
                        &shortcuts,
                        focused
                    ),
                    (action == GlobalShortcut::CyclePaneWidth).then_some(action),
                    "width cycling repeats while closing a pane does not"
                );
            }
        }
    }

    #[test]
    fn customized_creation_shortcuts_release_defaults_and_reserve_only_bound_chords() {
        use keyboard::key::Code;

        let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
        let mut shortcuts = KeyboardShortcuts {
            clone_pane: vec!["Alt+Shift+E".into()],
            new_channel: vec!["Ctrl+Shift+N".into()],
            new_native_thread: vec![],
            ..KeyboardShortcuts::default()
        };
        for (character, code, default_character, default_code, action) in [
            ("e", Code::KeyE, "d", Code::KeyD, GlobalShortcut::ClonePane),
            ("n", Code::KeyN, "g", Code::KeyG, GlobalShortcut::NewChannel),
        ] {
            let modifiers = if action == GlobalShortcut::ClonePane {
                keyboard::Modifiers::ALT | keyboard::Modifiers::SHIFT
            } else {
                ctrl_shift
            };
            assert_eq!(
                global_shortcut(
                    &key_press(character, code, modifiers, false),
                    &shortcuts,
                    FocusedPane::Other,
                ),
                Some(action)
            );
            assert_eq!(
                global_shortcut(
                    &key_press(character, code, modifiers, true),
                    &shortcuts,
                    FocusedPane::Other,
                ),
                None
            );
            assert!(is_global_shortcut_chord(
                &keyboard::Key::Character(character.into()),
                keyboard::key::Physical::Code(code),
                modifiers,
                &shortcuts,
                FocusedPane::Terminal,
            ));
            assert!(!is_global_shortcut_chord(
                &keyboard::Key::Character(default_character.into()),
                keyboard::key::Physical::Code(default_code),
                ctrl_shift,
                &shortcuts,
                FocusedPane::Terminal,
            ));
        }

        shortcuts.clone_pane.clear();
        shortcuts.new_channel.clear();
        for (character, code, modifiers) in [
            (
                "e",
                Code::KeyE,
                keyboard::Modifiers::ALT | keyboard::Modifiers::SHIFT,
            ),
            ("n", Code::KeyN, ctrl_shift),
        ] {
            assert!(!is_global_shortcut_chord(
                &keyboard::Key::Character(character.into()),
                keyboard::key::Physical::Code(code),
                modifiers,
                &shortcuts,
                FocusedPane::Terminal,
            ));
        }
    }

    #[test]
    fn captured_typing_is_not_forwarded_to_raw_card_children() {
        assert!(!should_forward_to_raw_children(true));
        assert!(should_forward_to_raw_children(false));
    }

    #[test]
    fn wheel_scrolls_card_content_without_moving_the_workspace() {
        use cosmic::iced::advanced::{clipboard, renderer::Headless};
        use cosmic::iced::{Font, Pixels};

        #[derive(Debug, Clone)]
        enum Observation {
            Content(f32),
            Workspace(WorkspaceViewportEvent),
        }

        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                Font::default(),
                Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let mut workspace = Workspace::default();
        workspace.resize_viewport(400.0);
        for index in 0..3 {
            workspace
                .add_card(
                    WorkspaceCard::new(CardId::new(), "Scrollable", CardWidth::Fixed(320.0))
                        .unwrap(),
                    index,
                    index == 0,
                )
                .unwrap();
        }
        let shortcuts = KeyboardShortcuts::default();
        let mut element = view(
            &workspace,
            &shortcuts,
            FocusedPane::Other,
            true,
            &InputTrace::default(),
            400.0,
            widget::Id::unique(),
            |_| {
                widget::scrollable(widget::Space::new().width(Length::Fill).height(2_000.0))
                    .height(Length::Fill)
                    .on_scroll(|viewport| Observation::Content(viewport.absolute_offset().y))
                    .into()
            },
            card_widget_id,
            Events {
                viewport: Observation::Workspace,
            },
        );
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(400.0, 300.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(viewport.size(), viewport.size()),
        );
        let mut messages = Vec::new();
        let mut clipboard = clipboard::Null;
        for delta in [
            mouse::ScrollDelta::Pixels {
                x: -120.0,
                y: -120.0,
            },
            mouse::ScrollDelta::Pixels { x: -120.0, y: 0.0 },
            mouse::ScrollDelta::Lines { x: -3.0, y: 0.0 },
        ] {
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::WheelScrolled { delta }),
                Layout::new(&node),
                mouse::Cursor::Available(Point::new(100.0, 100.0)),
                &renderer,
                &mut clipboard,
                &mut shell,
                &viewport,
            );
        }
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, Observation::Content(offset) if *offset > 0.0))
        );
        assert!(!messages.iter().any(|message| matches!(
            message,
            Observation::Workspace(WorkspaceViewportEvent::Scrolled(_))
        )));
    }

    /// A leaf that reports every input admission it is shown, as its card.
    struct KeyProbe(CardId);

    impl Widget<Option<CardId>, Theme, Renderer> for KeyProbe {
        fn size(&self) -> Size<Length> {
            Size::new(Length::Fill, Length::Fill)
        }
        fn layout(
            &mut self,
            _tree: &mut Tree,
            _renderer: &Renderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            layout::Node::new(limits.max())
        }
        fn draw(
            &self,
            _tree: &Tree,
            _renderer: &mut Renderer,
            _theme: &Theme,
            _style: &renderer::Style,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _viewport: &Rectangle,
        ) {
        }
        fn update(
            &mut self,
            _tree: &mut Tree,
            event: &Event,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _renderer: &Renderer,
            _clipboard: &mut dyn Clipboard,
            shell: &mut Shell<'_, Option<CardId>>,
            _viewport: &Rectangle,
        ) {
            if matches!(
                event,
                Event::Keyboard(keyboard::Event::KeyPressed { .. })
                    | Event::InputMethod(cosmic::iced::advanced::input_method::Event::Commit(_))
            ) {
                shell.publish(Some(self.0));
            }
        }
    }

    #[test]
    fn sidebar_regions_resolve_row_commands_without_background_pane_actions() {
        let shortcuts = KeyboardShortcuts::default();
        for focused in [FocusedPane::SidebarRows, FocusedPane::SidebarFiles, FocusedPane::SidebarChanges] {
            for action in [GlobalShortcut::RenamePane, GlobalShortcut::CloseFocusedCard, GlobalShortcut::ClonePane, GlobalShortcut::ChatAccount, GlobalShortcut::ChatModel, GlobalShortcut::ChatPermissions, GlobalShortcut::FindInPane] {
                assert!(!shortcut_applies(shortcut_descriptor(action), focused, &shortcuts), "{action:?} must not target a background pane from {focused:?}");
            }
            assert!(shortcut_applies(shortcut_descriptor(GlobalShortcut::NewTerminal), focused, &shortcuts));
            assert_eq!(shortcut_applies(shortcut_descriptor(GlobalShortcut::NewThread), focused, &shortcuts), focused == FocusedPane::SidebarRows);
        }
    }

    #[test]
    fn ignored_sidebar_chords_never_reach_the_selected_raw_card() {
        use cosmic::iced::advanced::clipboard;
        use cosmic::iced::advanced::widget::operation::focusable;
        use keyboard::key::Code;
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let shortcuts = KeyboardShortcuts::default();
        let trace = InputTrace::default();
        let sidebar = widget::Id::new("raw-isolation.sidebar");
        let card = CardId::new();
        let raw = WorkspaceSurface::new(
            Element::new(KeyProbe(card)),
            &shortcuts,
            FocusedPane::Terminal,
            400.0,
            |_| None,
        )
        .with_keyboard_trace(&trace);
        let row: Element<'_, Option<CardId>> = widget::row::with_children(vec![
            widget::text_input("Search projects", "query")
                .id(sidebar.clone())
                .on_input(|_| None)
                .width(200)
                .into(),
            Element::new(raw),
        ])
        .into();
        let mut element = crate::input_trace::observe_keyboard_focus(row, &trace, Vec::new());
        let mut tree = Tree::new(element.as_widget());
        let viewport = Rectangle::with_size(Size::new(600.0, 240.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(viewport.size(), viewport.size()),
        );
        let mut focus = focusable::focus::<()>(sidebar);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut focus);
        let mut messages = Vec::new();
        let mut clipboard = clipboard::Null;
        for event in [
            keyboard::Event::ModifiersChanged(keyboard::Modifiers::CTRL),
            key_press("f", Code::KeyF, keyboard::Modifiers::CTRL, false),
            key_press("t", Code::KeyT, keyboard::Modifiers::CTRL, false),
            keyboard::Event::ModifiersChanged(
                keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            ),
            key_press(
                "z",
                Code::KeyZ,
                keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
                false,
            ),
        ] {
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &Event::Keyboard(event),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard,
                &mut shell,
                &viewport,
            );
        }
        assert_eq!(trace.keyboard_focus(), Some(FocusedPane::TextInput));
        assert!(
            messages.is_empty(),
            "ignored sidebar chords must not enter raw card input: {messages:?}"
        );
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &Event::InputMethod(cosmic::iced::advanced::input_method::Event::Commit(
                "sidebar-only".into(),
            )),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard,
            &mut shell,
            &viewport,
        );
        assert!(
            messages.iter().all(Option::is_none),
            "IME input must not enter the raw card: {messages:?}"
        );
    }

    #[test]
    fn in_pane_inputs_keep_raw_delivery_while_external_inputs_do_not() {
        use crate::input_trace::KeyboardRegion;
        use cosmic::iced::advanced::clipboard;
        use keyboard::key::Code;
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let shortcuts = KeyboardShortcuts::default();
        let card = CardId::new();
        for (region, expected) in [
            (KeyboardRegion::from(FocusedPane::TextInput), false),
            (KeyboardRegion::from(FocusedPane::OutsideWorkspace), false),
            (KeyboardRegion::from(FocusedPane::SidebarRows), false),
            (KeyboardRegion::from(FocusedPane::SidebarFiles), false),
            (KeyboardRegion::from(FocusedPane::SidebarChanges), false),
            (
                KeyboardRegion {
                    pane: FocusedPane::TextInput,
                    card: Some(card),
                },
                true,
            ),
            (
                KeyboardRegion {
                    pane: FocusedPane::Terminal,
                    card: Some(card),
                },
                true,
            ),
        ] {
            let trace = InputTrace::with_keyboard_region(region);
            let mut surface = WorkspaceSurface::new(
                Element::new(KeyProbe(card)),
                &shortcuts,
                FocusedPane::Terminal,
                400.0,
                |_| None,
            )
            .with_keyboard_trace(&trace);
            let mut tree = Tree::new(&surface as &dyn Widget<Option<CardId>, Theme, Renderer>);
            let viewport = Rectangle::with_size(Size::new(400.0, 200.0));
            let node = surface.layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(viewport.size(), viewport.size()),
            );
            for event in [
                Event::Keyboard(key_press("f", Code::KeyF, keyboard::Modifiers::CTRL, false)),
                Event::InputMethod(cosmic::iced::advanced::input_method::Event::Commit("input".into())),
            ] {
                let mut messages = Vec::new();
                let mut shell = Shell::new(&mut messages);
                surface.update(
                    &mut tree,
                    &event,
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut clipboard::Null,
                    &mut shell,
                    &viewport,
                );
                assert_eq!(
                    messages,
                    if expected {
                        vec![Some(card)]
                    } else {
                        Vec::new()
                    },
                    "{region:?}"
                );
            }
        }
    }

    /// A far jump hands its destination the keyboard at once, but the pane
    /// only enters the viewport when the camera gets there. The strip showed
    /// keyboard events to panes inside the viewport alone, so what was typed
    /// during the journey reached nobody.
    #[test]
    fn keys_typed_while_the_camera_travels_reach_the_pane_it_is_travelling_to() {
        use cosmic::iced::advanced::{clipboard, renderer::Headless};
        use cosmic::iced::{Font, Pixels};

        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                Font::default(),
                Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let mut workspace = Workspace::default();
        workspace.resize_viewport(400.0);
        for index in 0..8 {
            workspace
                .add_card(
                    WorkspaceCard::new(CardId::new(), "Pane", CardWidth::Fixed(480.0)).unwrap(),
                    index,
                    index == 0,
                )
                .unwrap();
        }
        let ids: Vec<_> = workspace.order().collect();
        workspace.focus(ids[6]).unwrap();
        assert_eq!(workspace.camera().offset(), 0.0);

        let shortcuts = KeyboardShortcuts::default();
        let mut element = view(
            &workspace,
            &shortcuts,
            FocusedPane::Other,
            false,
            &InputTrace::default(),
            400.0,
            widget::Id::unique(),
            |context| Element::new(KeyProbe(context.id)),
            card_widget_id,
            // Whatever the strip reports about itself is not a key press.
            Events { viewport: |_| None },
        );
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(400.0, 300.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(viewport.size(), viewport.size()),
        );
        let mut messages = Vec::new();
        let mut clipboard = clipboard::Null;
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character("a".into()),
                modified_key: keyboard::Key::Character("a".into()),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyA),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: Some("a".into()),
                repeat: false,
            }),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard,
            &mut shell,
            &viewport,
        );
        let mut reached: Vec<_> = messages
            .into_iter()
            .flatten()
            .map(|id| ids.iter().position(|candidate| *candidate == id).unwrap())
            .collect();
        reached.sort_unstable();
        // The pane on screen sees the key as before, and so does the
        // destination. The pane parked in the overscan beside it does not.
        assert_eq!(
            workspace
                .projection(400.0)
                .mounted_ids()
                .collect::<Vec<_>>(),
            [ids[0], ids[1], ids[6]]
        );
        assert_eq!(reached, [0, 6]);
    }

    /// The shell re-runs one `RedrawRequested` through update and layout while
    /// any pass publishes a message, rebuilding the view in between, and warns
    /// once a frame has cost three of those passes. The strip surface reports
    /// its own measured width from that pass, so the report has to be a fixed
    /// point: once the owner holds the reported width, a further pass over the
    /// same frame must publish nothing - including a pass whose height changed,
    /// and a pass whose strip was remounted with fresh widget state, neither of
    /// which changes the width the owner reads.
    #[test]
    fn native_scroll_samples_distinguish_redraw_feedback_from_user_wheels() {
        use cosmic::iced::advanced::{clipboard, renderer::Headless};
        use cosmic::iced::{Font, Pixels};
        use std::time::Instant;

        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                Font::default(),
                Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let shortcuts = KeyboardShortcuts::default();
        let scroll_input = Rc::new(Cell::new(false));
        let callback_input = Rc::clone(&scroll_input);
        // Exercise the native scrollable directly: VirtualStrip deliberately
        // consumes wheels over pane content, which has its own regression.
        let scrollable = cosmic::widget::scrollable::horizontal(
            widget::Space::new().width(1_280.0).height(Length::Fill),
        )
        .direction(widget::scrollable::Direction::Horizontal(
            widget::scrollable::Scrollbar::hidden(),
        ))
        .width(Length::Fill)
        .height(Length::Fill)
        .on_scroll(move |viewport| {
            WorkspaceViewportEvent::Scrolled(ViewportSample {
                offset: viewport.absolute_offset().x,
                width: viewport.bounds().width,
                user_input: callback_input.get(),
            })
        });
        let mut element: Element<'_, WorkspaceViewportEvent> = WorkspaceSurface::new(
            scrollable,
            &shortcuts,
            FocusedPane::Other,
            400.0,
            WorkspaceViewportEvent::Resized,
        )
        .with_scroll_input(scroll_input)
        .into();
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(400.0, 300.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(viewport.size(), viewport.size()),
        );
        let mut messages = Vec::new();
        for event in [
            Event::Window(window::Event::RedrawRequested(Instant::now())),
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: -120.0, y: 0.0 },
            }),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(Point::new(100.0, 100.0)),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        let samples: Vec<_> = messages
            .into_iter()
            .filter_map(|event| match event {
                WorkspaceViewportEvent::Scrolled(sample) => Some(sample),
                _ => None,
            })
            .collect();
        assert!(samples.iter().any(|sample| !sample.user_input));
        assert!(
            samples
                .iter()
                .any(|sample| sample.user_input && sample.offset > 0.0)
        );
    }

    #[test]
    fn a_reported_viewport_width_is_not_reported_twice_in_one_frame() {
        use cosmic::iced::advanced::{clipboard, renderer::Headless};
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{Font, Pixels};
        use std::time::Instant;

        let mut renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                Font::default(),
                Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let mut workspace = Workspace::default();
        for index in 0..4 {
            workspace
                .add_card(
                    WorkspaceCard::new(CardId::new(), "Card", CardWidth::Fraction(0.5)).unwrap(),
                    index,
                    index == 0,
                )
                .unwrap();
        }
        let shortcuts = KeyboardShortcuts::default();
        let scroll_id = widget::Id::unique();
        let redraw = Event::Window(window::Event::RedrawRequested(Instant::now()));

        // One pass of the shell's loop: build the view the owner would build
        // for the width it currently holds, deliver the frame to it, and
        // report back what the surface asked of the owner.
        let mut pass = |workspace: &Workspace, cache: Cache, height: f32| {
            let mut messages: Vec<WorkspaceViewportEvent> = Vec::new();
            let mut interface = UserInterface::build(
                view(
                    workspace,
                    &shortcuts,
                    FocusedPane::Other,
                    false,
                    &InputTrace::default(),
                    workspace.viewport_width(),
                    scroll_id.clone(),
                    |_| {
                        widget::Space::new()
                            .width(Length::Fill)
                            .height(Length::Fill)
                            .into()
                    },
                    card_widget_id,
                    Events {
                        viewport: |event| event,
                    },
                ),
                Size::new(900.0, height),
                cache,
                &mut renderer,
            );
            let _ = interface.update(
                std::slice::from_ref(&redraw),
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            let widths: Vec<f32> = messages
                .iter()
                .filter_map(|message| match message {
                    WorkspaceViewportEvent::Resized(width) => Some(*width),
                    _ => None,
                })
                .collect();
            (interface.into_cache(), widths)
        };

        let (cache, reported) = pass(&workspace, Cache::new(), 600.0);
        assert_eq!(
            reported,
            vec![900.0],
            "a surface whose measured width disagrees with the owner's has to say so once",
        );
        for width in reported {
            let _ = workspace.resize_viewport(width);
        }

        let (cache, settled) = pass(&workspace, cache, 600.0);
        assert!(
            settled.is_empty(),
            "the owner already holds this width, so a second pass over the same frame must \
             publish nothing; it reported {settled:?}",
        );

        let (_, after_height_change) = pass(&workspace, cache, 400.0);
        assert!(
            after_height_change.is_empty(),
            "a shorter surface is the same viewport width, so reporting it again buys the owner \
             nothing and costs the frame a whole extra pass; it reported {after_height_change:?}",
        );

        let (_, after_remount) = pass(&workspace, Cache::new(), 600.0);
        assert!(
            after_remount.is_empty(),
            "a strip remounted with fresh widget state still measures the width the owner holds; \
             it reported {after_remount:?}",
        );
    }
    struct AccessibleProbe {
        node: u64,
        expected_virtual_offset: Vector,
    }

    impl Widget<(), Theme, Renderer> for AccessibleProbe {
        fn tag(&self) -> tree::Tag {
            tree::Tag::of::<u64>()
        }

        fn state(&self) -> tree::State {
            tree::State::new(self.node)
        }

        fn size(&self) -> Size<Length> {
            Size::new(Length::Fill, Length::Fill)
        }

        fn layout(
            &mut self,
            _tree: &mut Tree,
            _renderer: &Renderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            layout::Node::new(limits.max())
        }

        fn draw(
            &self,
            _tree: &Tree,
            _renderer: &mut Renderer,
            _theme: &Theme,
            _style: &renderer::Style,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _viewport: &Rectangle,
        ) {
        }

        fn a11y_nodes(
            &self,
            layout: Layout<'_>,
            state: &Tree,
            _cursor: mouse::Cursor,
        ) -> iced_accessibility::A11yTree {
            use iced_accessibility::accesskit::{Node, Role};

            assert_eq!(layout.virtual_offset(), self.expected_virtual_offset);
            iced_accessibility::A11yTree::leaf(
                Node::new(Role::Button),
                *state.state.downcast_ref::<u64>(),
            )
        }
    }

    #[test]
    fn scrolling_restores_card_state_but_closed_cards_do_not_keep_it() {
        use iced_accessibility::A11yId;

        let first = CardId::new();
        let second = CardId::new();
        let retained = Arc::new(HashMap::from([(first, 0), (second, 1)]));
        let make_strip = |retained, id, node| VirtualStrip {
            total_width: 200.0,
            retained,
            focused: Some(id),
            cards: vec![VirtualCard {
                id,
                x: 0.0,
                width: 100.0,
                content_width: 100.0,
                on_screen: true,
                content: Element::new(AccessibleProbe {
                    node,
                    expected_virtual_offset: Vector::ZERO,
                }),
            }],
        };
        let mut strip: Element<'_, ()> = Element::new(make_strip(Arc::clone(&retained), first, 1));
        let mut tree = Tree::new(&strip);
        let layout = layout::Node::with_children(
            Size::new(200.0, 80.0),
            vec![layout::Node::new(Size::new(100.0, 80.0))],
        );
        for (id, initial, expected) in [
            (first, 2, 1), // The same band diffs in place.
            (second, 3, 3),
            (first, 4, 1), // Remount restores the first card's state.
        ] {
            strip = Element::new(make_strip(Arc::clone(&retained), id, initial));
            tree.diff(&mut strip);
            assert!(
                strip
                    .as_widget()
                    .a11y_nodes(Layout::new(&layout), &tree, mouse::Cursor::Unavailable,)
                    .contains(&A11yId::from(expected as u64))
            );
        }

        // Close the first card while only the second is mounted. Reusing its
        // identity afterwards must create fresh state, not revive a stale tree.
        strip = Element::new(make_strip(Arc::clone(&retained), second, 5));
        tree.diff(&mut strip);
        strip = Element::new(make_strip(
            Arc::new(HashMap::from([(second, 0)])),
            second,
            6,
        ));
        tree.diff(&mut strip);
        strip = Element::new(make_strip(retained, first, 7));
        tree.diff(&mut strip);
        assert!(
            strip
                .as_widget()
                .a11y_nodes(Layout::new(&layout), &tree, mouse::Cursor::Unavailable,)
                .contains(&A11yId::from(7_u64))
        );
    }

    #[test]
    fn workspace_wrappers_preserve_accessible_card_descendants() {
        use iced_accessibility::A11yId;

        let cards = [CardId::new(), CardId::new()];
        let strip = VirtualStrip {
            total_width: 200.0,
            focused: Some(cards[0]),
            retained: Arc::new(
                cards
                    .into_iter()
                    .enumerate()
                    .map(|(index, id)| (id, index))
                    .collect(),
            ),
            cards: cards
                .into_iter()
                .enumerate()
                .map(|(index, id)| VirtualCard {
                    id,
                    x: index as f32 * 100.0,
                    width: 100.0,
                    content_width: 100.0,
                    on_screen: true,
                    content: CardSurface::new(Element::new(AccessibleProbe {
                        node: index as u64 + 1,
                        expected_virtual_offset: Vector::new(32.0, 0.0),
                    }))
                    .into(),
                })
                .collect(),
        };
        let shortcuts = KeyboardShortcuts::default();
        let surface: Element<'_, ()> = Element::new(WorkspaceSurface::new(
            strip,
            &shortcuts,
            FocusedPane::Other,
            0.0,
            |_| (),
        ));
        let tree = Tree::new(&surface);
        let card_node = || {
            layout::Node::with_children(
                Size::new(100.0, 80.0),
                vec![layout::Node::new(Size::new(100.0, 80.0))],
            )
        };
        let strip_node = layout::Node::with_children(
            Size::new(200.0, 80.0),
            vec![card_node(), card_node().move_to(Point::new(100.0, 0.0))],
        );
        let surface_node = layout::Node::with_children(Size::new(200.0, 80.0), vec![strip_node]);
        let accessible = surface.as_widget().a11y_nodes(
            Layout::new(&surface_node).with_virtual_offset(Vector::new(32.0, 0.0)),
            &tree,
            mouse::Cursor::Unavailable,
        );

        assert!(accessible.contains(&A11yId::from(1_u64)));
        assert!(accessible.contains(&A11yId::from(2_u64)));
        assert_eq!(accessible.root().len(), 2);
    }

    #[test]
    fn opening_card_contents_are_built_before_their_space_is_ready() {
        let mut workspace = Workspace::default();
        workspace.resize_viewport(900.0);
        let first = CardId::new();
        let opening = CardId::new();
        workspace
            .add_card(
                WorkspaceCard::new(first, "First", CardWidth::Fixed(300.0)).unwrap(),
                0,
                true,
            )
            .unwrap();
        workspace
            .add_card(
                WorkspaceCard::new(opening, "Opening", CardWidth::Fixed(300.0)).unwrap(),
                1,
                true,
            )
            .unwrap();
        let built = std::cell::RefCell::new(Vec::new());
        let shortcuts = KeyboardShortcuts::default();
        let input = InputTrace::default();
        let render = |workspace: &Workspace| {
            let element: Element<'_, ()> = view(
                workspace,
                &shortcuts,
                FocusedPane::Other,
                false,
                &input,
                900.0,
                widget::Id::unique(),
                |context| {
                    built.borrow_mut().push(context.id);
                    widget::Space::new().into()
                },
                card_widget_id,
                Events { viewport: |_| () },
            );
            drop(element);
            built.take()
        };
        assert_eq!(render(&workspace), [first, opening]);
        workspace.tick(Duration::from_millis(220));
        assert_eq!(render(&workspace), [first, opening]);
    }

    #[test]
    fn composed_workspace_view_exposes_every_mounted_card_to_a_screen_reader() {
        use cosmic::iced::advanced::renderer::Headless;
        use iced_accessibility::A11yId;

        const VIEWPORT_WIDTH: f32 = 700.0;
        const HEIGHT: f32 = 480.0;

        let mut workspace = Workspace::default();
        workspace.resize_viewport(VIEWPORT_WIDTH);
        let cards: Vec<CardId> = (0..8).map(|_| CardId::new()).collect();
        for (index, id) in cards.iter().copied().enumerate() {
            workspace
                .add_card(
                    WorkspaceCard::new(id, format!("Card {index}"), CardWidth::Fixed(320.0))
                        .unwrap(),
                    index,
                    index == 0,
                )
                .unwrap();
        }
        let projection = workspace.projection(VIEWPORT_WIDTH);
        let mounted: Vec<CardId> = projection.mounted_ids().collect();
        assert!(mounted.len() < cards.len());

        let probe_id = |id: CardId| {
            cards
                .iter()
                .position(|card| *card == id)
                .expect("probe for a card the workspace holds") as u64
                + 1
        };
        let shortcuts = KeyboardShortcuts::default();
        let mut composed: Element<'_, ()> = view(
            &workspace,
            &shortcuts,
            FocusedPane::Other,
            true,
            &InputTrace::default(),
            VIEWPORT_WIDTH,
            widget::Id::unique(),
            |context| {
                Element::new(AccessibleProbe {
                    node: probe_id(context.id),
                    expected_virtual_offset: Vector::ZERO,
                })
            },
            card_widget_id,
            Events { viewport: |_| () },
        );
        let mut tree = Tree::new(&composed);
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let surface_node = composed.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(VIEWPORT_WIDTH, HEIGHT)),
        );
        let accessible = composed.as_widget().a11y_nodes(
            Layout::new(&surface_node),
            &tree,
            mouse::Cursor::Unavailable,
        );

        assert!(
            !accessible.root().is_empty(),
            "the workspace must reach a screen reader through every wrapper the view builds"
        );
        for id in &mounted {
            assert!(
                accessible.contains(&A11yId::from(probe_id(*id))),
                "mounted card {id} is missing from the accessibility tree"
            );
        }
        assert_eq!(
            cards
                .iter()
                .filter(|id| accessible.contains(&A11yId::from(probe_id(**id))))
                .count(),
            mounted.len(),
            "the accessibility tree must expose the mounted cards and nothing else"
        );
    }

    #[test]
    fn only_configured_global_chords_are_reserved_from_card_children() {
        use keyboard::key::{Code, Physical};

        let shortcuts = KeyboardShortcuts::default();
        let reserved = |character, code, modifiers| {
            is_global_shortcut_chord(
                &keyboard::Key::Character(character),
                Physical::Code(code),
                modifiers,
                &shortcuts,
                FocusedPane::Other,
            )
        };
        let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
        assert!(reserved("t".into(), Code::KeyT, ctrl_shift));
        assert!(reserved("t".into(), Code::KeyT, keyboard::Modifiers::CTRL));
        assert!(!reserved("q".into(), Code::KeyQ, keyboard::Modifiers::CTRL));
        assert!(!reserved("q".into(), Code::KeyQ, keyboard::Modifiers::ALT));
        assert!(!reserved("a".into(), Code::KeyA, ctrl_shift));
        assert!(!reserved("w".into(), Code::KeyW, keyboard::Modifiers::CTRL));
        assert!(reserved("w".into(), Code::KeyW, ctrl_shift));
        assert!(!reserved("w".into(), Code::KeyW, keyboard::Modifiers::ALT));
        assert!(reserved("r".into(), Code::KeyR, keyboard::Modifiers::ALT));
        assert!(!reserved("r".into(), Code::KeyR, ctrl_shift));
        assert!(!reserved(
            "t".into(),
            Code::KeyT,
            keyboard::Modifiers::CTRL | keyboard::Modifiers::ALT,
        ));
        assert!(!reserved(
            "t".into(),
            Code::KeyT,
            ctrl_shift | keyboard::Modifiers::ALT,
        ));

        let customized = KeyboardShortcuts {
            new_native_thread: vec!["Ctrl+Shift+N".into()],
            ..shortcuts
        };
        assert!(!is_global_shortcut_chord(
            &keyboard::Key::Character("t".into()),
            Physical::Code(Code::KeyT),
            keyboard::Modifiers::CTRL,
            &customized,
            FocusedPane::Other,
        ));
    }

    #[test]
    fn diff_hunk_aliases_use_review_scope_and_leave_other_panes_untouched() {
        use keyboard::key::{Code, Named};
        let shortcuts = KeyboardShortcuts::default();
        let events = [
            (
                key_press("j", Code::KeyJ, keyboard::Modifiers::ALT, false),
                GlobalShortcut::ReviewNextChange,
            ),
            (
                key_press("k", Code::KeyK, keyboard::Modifiers::ALT, false),
                GlobalShortcut::ReviewPreviousChange,
            ),
            (
                named_key_press(Named::F7, Code::F7, keyboard::Modifiers::empty(), false),
                GlobalShortcut::ReviewNextChange,
            ),
            (
                named_key_press(Named::F7, Code::F7, keyboard::Modifiers::SHIFT, false),
                GlobalShortcut::ReviewPreviousChange,
            ),
        ];
        for (event, expected) in events {
            assert_eq!(
                global_shortcut(&event, &shortcuts, FocusedPane::Review),
                Some(expected)
            );
            for focused in [FocusedPane::Terminal, FocusedPane::Chat, FocusedPane::Other] {
                assert_eq!(global_shortcut(&event, &shortcuts, focused), None);
            }
        }
    }

    /// The review chords are arrow chords a terminal program may well use,
    /// so they are withheld from a card's process only while a pull request
    /// or repository diff pane has focus.
    #[test]
    fn review_chords_are_reserved_only_while_a_review_pane_has_focus() {
        use keyboard::key::{Code, Physical};

        let shortcuts = KeyboardShortcuts::default();
        let reserved = |named, code, modifiers, focused| {
            is_global_shortcut_chord(
                &keyboard::Key::Named(named),
                Physical::Code(code),
                modifiers,
                &shortcuts,
                focused,
            )
        };
        for (named, code) in [
            (keyboard::key::Named::ArrowUp, Code::ArrowUp),
            (keyboard::key::Named::ArrowDown, Code::ArrowDown),
        ] {
            for focused in [FocusedPane::Terminal, FocusedPane::Other] {
                assert!(
                    !reserved(named, code, keyboard::Modifiers::ALT, focused),
                    "{named:?} reached no {focused:?} pane"
                );
            }
            assert!(
                reserved(named, code, keyboard::Modifiers::ALT, FocusedPane::Review),
                "{named:?} bypassed the review pane"
            );
        }
        // Workspace chords stay reserved either way.
        for focused in [
            FocusedPane::Terminal,
            FocusedPane::Review,
            FocusedPane::Other,
        ] {
            assert!(reserved(
                keyboard::key::Named::PageUp,
                Code::PageUp,
                keyboard::Modifiers::CTRL,
                focused,
            ));
        }
        let step = named_key_press(
            keyboard::key::Named::ArrowDown,
            Code::ArrowDown,
            keyboard::Modifiers::ALT,
            true,
        );
        assert_eq!(
            global_shortcut(&step, &shortcuts, FocusedPane::Review),
            Some(GlobalShortcut::ReviewNextFile),
            "stepping files repeats while held"
        );
        assert_eq!(
            global_shortcut(&step, &shortcuts, FocusedPane::Terminal),
            None,
            "a review chord acts on no other pane"
        );
    }

    #[test]
    fn held_page_navigation_crosses_four_cards_and_stops_at_edges() {
        use keyboard::key::{Code, Named};

        let mut workspace = Workspace::default();
        let cards = [CardId::new(), CardId::new(), CardId::new(), CardId::new()];
        for (index, id) in cards.into_iter().enumerate() {
            workspace
                .add_card(
                    WorkspaceCard::new(id, format!("Card {index}"), CardWidth::Fixed(320.0))
                        .unwrap(),
                    index,
                    false,
                )
                .unwrap();
        }
        workspace.focus(cards[0]).unwrap();

        let shortcuts = KeyboardShortcuts::default();
        for repeat in [false, true, true, true] {
            let shortcut = global_shortcut(
                &named_key_press(
                    Named::PageDown,
                    Code::PageDown,
                    keyboard::Modifiers::CTRL,
                    repeat,
                ),
                &shortcuts,
                FocusedPane::Terminal,
            );
            let Some(GlobalShortcut::FocusAdjacent(direction)) = shortcut else {
                panic!("Ctrl+PageDown must remain a navigation shortcut on repeated key presses");
            };
            workspace.focus_adjacent(direction).unwrap();
        }
        assert_eq!(workspace.focused(), Some(cards[3]));

        for repeat in [false, true, true, true] {
            let shortcut = global_shortcut(
                &named_key_press(
                    Named::PageUp,
                    Code::PageUp,
                    keyboard::Modifiers::CTRL,
                    repeat,
                ),
                &shortcuts,
                FocusedPane::Terminal,
            );
            let Some(GlobalShortcut::FocusAdjacent(direction)) = shortcut else {
                panic!("Ctrl+PageUp must remain a navigation shortcut on repeated key presses");
            };
            workspace.focus_adjacent(direction).unwrap();
        }
        assert_eq!(workspace.focused(), Some(cards[0]));
    }

    #[test]
    fn chat_selection_shortcuts_only_capture_keys_in_native_chat() {
        use keyboard::key::Code;
        let shortcuts = KeyboardShortcuts::default();
        let modifiers = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
        for (key, code, action) in [
            ("a", Code::KeyA, GlobalShortcut::ChatAccount),
            ("m", Code::KeyM, GlobalShortcut::ChatModel),
            ("o", Code::KeyO, GlobalShortcut::ChatPermissions),
        ] {
            for focused in [
                FocusedPane::Terminal,
                FocusedPane::Review,
                FocusedPane::Other,
                FocusedPane::Chat,
                FocusedPane::ChatPane,
            ] {
                assert_eq!(
                    global_shortcut(&key_press(key, code, modifiers, false), &shortcuts, focused),
                    matches!(focused, FocusedPane::Chat | FocusedPane::ChatPane).then_some(action),
                );
                assert_eq!(
                    is_global_shortcut_chord(
                        &keyboard::Key::Character(key.into()),
                        keyboard::key::Physical::Code(code),
                        modifiers,
                        &shortcuts,
                        focused
                    ),
                    matches!(focused, FocusedPane::Chat | FocusedPane::ChatPane),
                );
                assert_eq!(
                    global_shortcut(&key_press(key, code, modifiers, true), &shortcuts, focused),
                    None
                );
            }
        }
        let customized = KeyboardShortcuts {
            chat_model: vec!["Alt+M".into()],
            ..shortcuts
        };
        assert_eq!(
            global_shortcut(
                &key_press("m", Code::KeyM, keyboard::Modifiers::ALT, false),
                &customized,
                FocusedPane::Chat
            ),
            Some(GlobalShortcut::ChatModel)
        );
        assert_eq!(
            global_shortcut(
                &key_press("m", Code::KeyM, modifiers, false),
                &customized,
                FocusedPane::Chat
            ),
            None
        );
    }

    #[test]
    fn shortcut_tooltips_follow_customized_and_unbound_bindings() {
        let mut shortcuts = KeyboardShortcuts::default();
        shortcuts.search_projects = vec!["Alt+S".into(), "Ctrl+Shift+F".into()];
        assert_eq!(
            shortcut_tooltip(GlobalShortcut::FocusProjectSearch, &shortcuts),
            "Search projects — Alt+S, Ctrl+Shift+F"
        );
        shortcuts.close_focused_column.clear();
        assert_eq!(
            shortcut_tooltip_named(
                GlobalShortcut::CloseFocusedCard,
                &shortcuts,
                "Close terminal"
            ),
            "Close terminal — unbound"
        );
    }

    #[test]
    fn diff_clipboard_bindings_remap_and_respect_text_input_focus() {
        use keyboard::key::Code;
        let mut shortcuts = KeyboardShortcuts::default();
        for (key, code, action) in [("c", Code::KeyC, GlobalShortcut::DiffCopy), ("a", Code::KeyA, GlobalShortcut::DiffSelectAll)] {
            let event = key_press(key, code, keyboard::Modifiers::CTRL, false);
            assert_eq!(global_shortcut(&event, &shortcuts, FocusedPane::Review), Some(action));
            for focused in [FocusedPane::TextInput, FocusedPane::Chat, FocusedPane::Terminal, FocusedPane::Other] {
                assert_eq!(global_shortcut(&event, &shortcuts, focused), None);
            }
        }
        shortcuts.diff_copy = vec!["Ctrl+Alt+C".into()];
        shortcuts.diff_select_all.clear();
        assert!(shortcuts.fault().is_none());
        assert_eq!(global_shortcut(&key_press("c", Code::KeyC, keyboard::Modifiers::CTRL, false), &shortcuts, FocusedPane::Review), None);
        assert_eq!(global_shortcut(&key_press("a", Code::KeyA, keyboard::Modifiers::CTRL, false), &shortcuts, FocusedPane::Review), None);
        assert_eq!(global_shortcut(&key_press("c", Code::KeyC, keyboard::Modifiers::CTRL | keyboard::Modifiers::ALT, false), &shortcuts, FocusedPane::Review), Some(GlobalShortcut::DiffCopy));
        let descriptor = shortcut_descriptor(GlobalShortcut::DiffCopy);
        let mut context = CommandContext { focused: FocusedPane::Review, ..Default::default() };
        assert!(!descriptor.available(&context));
        context.diff_commands = diff_command_bit(GlobalShortcut::DiffCopy).unwrap();
        assert!(descriptor.available(&context));
        context.focused = FocusedPane::TextInput;
        assert!(!descriptor.available(&context));
    }

    #[test]
    fn working_diff_history_owns_its_chords_without_stealing_immutable_review_reopen() {
        use keyboard::key::Code;
        let mut shortcuts = KeyboardShortcuts::default();
        let undo = key_press("z", Code::KeyZ, keyboard::Modifiers::CTRL, false);
        let redo = key_press("z", Code::KeyZ, keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT, false);
        assert_eq!(global_shortcut(&undo, &shortcuts, FocusedPane::WorkingDiff), Some(GlobalShortcut::EditorUndo));
        assert_eq!(global_shortcut(&redo, &shortcuts, FocusedPane::WorkingDiff), Some(GlobalShortcut::EditorRedo));
        assert_eq!(global_shortcut(&redo, &shortcuts, FocusedPane::Review), Some(GlobalShortcut::ReopenPane));
        assert!(is_global_shortcut_chord(&keyboard::Key::Character("z".into()), keyboard::key::Physical::Code(Code::KeyZ), keyboard::Modifiers::CTRL, &shortcuts, FocusedPane::WorkingDiff));
        assert!(!is_global_shortcut_chord(&keyboard::Key::Character("z".into()), keyboard::key::Physical::Code(Code::KeyZ), keyboard::Modifiers::CTRL, &shortcuts, FocusedPane::Editor));
        assert_eq!(global_shortcut(&key_press("c", Code::KeyC, keyboard::Modifiers::CTRL, false), &shortcuts, FocusedPane::WorkingDiff), Some(GlobalShortcut::DiffCopy));
        for focused in [FocusedPane::TextInput, FocusedPane::SidebarRows, FocusedPane::SidebarFiles, FocusedPane::SidebarChanges, FocusedPane::Chat, FocusedPane::Terminal] {
            assert_ne!(global_shortcut(&undo, &shortcuts, focused), Some(GlobalShortcut::EditorUndo));
        }
        shortcuts.editor_undo = vec!["Alt+F9".into()];
        assert!(global_shortcut(&undo, &shortcuts, FocusedPane::WorkingDiff).is_none());
        let mut context = CommandContext { focused: FocusedPane::WorkingDiff, editor_commands: 1, ..Default::default() };
        assert!(shortcut_descriptor(GlobalShortcut::EditorUndo).available(&context));
        context.editor_commands = 0;
        assert!(!shortcut_descriptor(GlobalShortcut::EditorUndo).available(&context));
    }

    #[test]
    fn every_shortcut_descriptor_has_a_unique_configurable_binding() {
        let shortcuts = KeyboardShortcuts::default();
        assert!(shortcuts.fault().is_none(), "defaults obey binding scope and reservation policy");
        let mut identities = HashSet::new();
        for descriptor in GLOBAL_SHORTCUT_DESCRIPTORS {
            assert!(!descriptor.label.is_empty());
            assert!(identities.insert(descriptor.id()), "registered IDs are unique");
            assert_eq!(descriptor.scope.is_native_editor(), crate::settings::is_editor_command(descriptor.id()), "native keyboard routing and configuration share the editor scope for {}", descriptor.id());
            let index = KeyboardShortcuts::action_names().iter().position(|id| *id == descriptor.id()).expect("every descriptor is configurable");
            let keys = shortcut_keys(descriptor.action, &shortcuts);
            assert_eq!(keys, shortcuts.actions()[index]);
            if keys.is_empty() {
                assert!(!crate::settings::shortcut_required(index), "only optional actions may start unbound");
            }
            for chord in keys {
                assert!(parse_shortcut(chord).is_ok());
            }
            // The one-line form the dialog shows has to read back as the same
            // chords, or editing a two-chord action there would drop one.
            assert_eq!(
                parse_chords(&shortcut_chords(descriptor.action, &shortcuts)),
                *keys
            );
        }
    }

    #[test]
    fn frame_clock_bounds_throttled_frames_without_rewinding_for_stale_frames() {
        let start = Instant::now();
        let mut clock = FrameClock::default();
        assert_eq!(clock.elapsed(start), None);
        assert_eq!(
            clock.elapsed(start + Duration::from_millis(16)),
            Some(Duration::from_millis(16))
        );
        assert_eq!(
            clock.elapsed(start + Duration::from_secs(2)),
            Some(Duration::from_millis(250))
        );
        assert_eq!(clock.elapsed(start + Duration::from_secs(1)), None);
        assert_eq!(
            clock.elapsed(start + Duration::from_millis(2_016)),
            Some(Duration::from_millis(16))
        );
    }

    #[test]
    fn a_started_frame_clock_advances_on_its_first_frame() {
        let start = Instant::now();
        let mut clock = FrameClock::default();
        clock.start(start);
        assert_eq!(clock.frame(), Some(start));
        assert_eq!(
            clock.elapsed(start + Duration::from_millis(9)),
            Some(Duration::from_millis(9))
        );
        // Restarting discards whatever was measured before, and a frame the
        // shell drew before the restart measures nothing.
        clock.start(start + Duration::from_secs(5));
        assert_eq!(clock.elapsed(start + Duration::from_secs(4)), None);
        assert_eq!(
            clock.elapsed(start + Duration::from_millis(5_016)),
            Some(Duration::from_millis(16))
        );
    }

    /// A leaf that always raises an overlay anchored to its own translated
    /// bounds, so a test can tell whether the pane chain forwards `overlay`
    /// and with which translation.
    struct OverlayProbe;

    struct ProbeOverlay {
        anchor: Rectangle,
    }

    impl Widget<(), Theme, Renderer> for OverlayProbe {
        fn size(&self) -> Size<Length> {
            Size::new(Length::Fill, Length::Fill)
        }
        fn layout(
            &mut self,
            _tree: &mut Tree,
            _renderer: &Renderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            layout::Node::new(limits.max())
        }
        fn draw(
            &self,
            _tree: &Tree,
            _renderer: &mut Renderer,
            _theme: &Theme,
            _style: &renderer::Style,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _viewport: &Rectangle,
        ) {
        }
        fn overlay<'a>(
            &'a mut self,
            _tree: &'a mut Tree,
            layout: Layout<'a>,
            _renderer: &Renderer,
            _viewport: &Rectangle,
            translation: Vector,
        ) -> Option<overlay::Element<'a, (), Theme, Renderer>> {
            Some(overlay::Element::new(Box::new(ProbeOverlay {
                anchor: layout.bounds() + translation,
            })))
        }
    }

    impl overlay::Overlay<(), Theme, Renderer> for ProbeOverlay {
        fn layout(&mut self, _renderer: &Renderer, _bounds: Size) -> layout::Node {
            layout::Node::new(Size::new(10.0, 10.0)).move_to(self.anchor.position())
        }
        fn draw(
            &self,
            _renderer: &mut Renderer,
            _theme: &Theme,
            _style: &renderer::Style,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
        ) {
        }
    }

    /// Every card an overlay can be raised from, paired with the screen
    /// position its overlay has to land on.
    fn expected_overlay_positions(projection: &ViewportProjection, viewport: f32) -> Vec<f32> {
        let mut positions: Vec<f32> = projection
            .mounted
            .iter()
            .map(|card| card.start - projection.offset)
            .filter(|x| *x < viewport && x + 400.0 > 0.0)
            .collect();
        positions.sort_by(f32::total_cmp);
        positions
    }

    #[test]
    fn restored_camera_initializes_native_scroll_once_before_the_first_frame() {
        use cosmic::iced::advanced::{renderer::Headless, widget::operation};
        const VIEWPORT: f32 = 600.0;
        const HEIGHT: f32 = 400.0;
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let mut workspace = Workspace::default();
        workspace.resize_viewport(VIEWPORT);
        for index in 0..4 {
            workspace
                .add_card(
                    WorkspaceCard::new(
                        CardId::new(),
                        format!("Pane {index}"),
                        CardWidth::Fixed(400.0),
                    )
                    .unwrap(),
                    index,
                    false,
                )
                .unwrap();
        }
        // Park the camera at the far end. The cards now on screen sit at
        // content-space positions well outside the visible rect, which is the
        // only arrangement that tells a correct visibility test from one that
        // compares content space against screen space: a workspace of one or
        // two cards has the two spaces on top of each other.
        let offset = workspace.geometry().max_offset(VIEWPORT);
        assert!(
            offset > VIEWPORT,
            "the fixture must scroll past a screenful"
        );
        workspace.native_scroll(offset, VIEWPORT);
        let projection = workspace.projection(VIEWPORT);
        let expected = expected_overlay_positions(&projection, VIEWPORT);
        assert!(
            expected.iter().any(|x| *x > 0.0),
            "the fixture must place a card the camera moved into view"
        );

        let scroll_id = widget::Id::new("workspace.scroll");
        let shortcuts = KeyboardShortcuts::default();
        let mut element: Element<'_, ()> = view(
            &workspace,
            &shortcuts,
            FocusedPane::Other,
            false,
            &InputTrace::default(),
            VIEWPORT,
            scroll_id.clone(),
            |_| Element::new(OverlayProbe),
            card_widget_id,
            Events { viewport: |_| () },
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(VIEWPORT, HEIGHT)),
        );
        // No startup scroll task or resize message is delivered. The initial
        // width already matches the model, as it can after restoring a window.
        let visible = Rectangle::new(Point::ORIGIN, Size::new(VIEWPORT, HEIGHT));
        let mut restored_overlay = element
            .as_widget_mut()
            .overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &visible,
                Vector::ZERO,
            )
            .expect("a mounted card raised an overlay");
        let raised = restored_overlay
            .as_overlay_mut()
            .layout(&renderer, visible.size());
        let mut positions: Vec<f32> = raised
            .children()
            .iter()
            .map(|child| child.bounds().x)
            .collect();
        positions.sort_by(f32::total_cmp);
        assert_eq!(positions, expected);
        drop(raised);
        drop(restored_overlay);

        // A later native scroll must survive relayout with the same widget
        // state, even before its feedback has reached the workspace model.
        let moved_offset = offset - 100.0;
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::scrollable::scroll_to::<()>(
                scroll_id,
                operation::scrollable::AbsoluteOffset {
                    x: Some(moved_offset),
                    y: None,
                },
            ),
        );
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(VIEWPORT, HEIGHT)),
        );
        let mut raised = element
            .as_widget_mut()
            .overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &visible,
                Vector::ZERO,
            )
            .expect("native-scrolled panes retain their overlays");
        let raised = raised.as_overlay_mut().layout(&renderer, visible.size());
        let mut positions: Vec<f32> = raised
            .children()
            .iter()
            .map(|child| child.bounds().x)
            .collect();
        positions.sort_by(f32::total_cmp);
        let mut moved = projection;
        moved.offset = moved_offset;
        assert_eq!(positions, expected_overlay_positions(&moved, VIEWPORT));
    }
}
