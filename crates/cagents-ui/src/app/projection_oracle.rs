//! A check that a projection narrower than `Full` leaves everything the
//! workspace derives as the full projection would.
//!
//! An update that declares a narrower [`Projection`] claims that the rest of
//! the derived state is still current. After such an update this check
//! captures that state, runs the full projection over it, captures it again
//! and reports any part that changed: that part was stale, so the narrower
//! projection's claim was wrong for the message. A handler that returns
//! before the projection makes the same claim, and is checked the same way.
//!
//! What it compares is the state the full projection writes, listed in
//! [`Snapshot`]. The tasks the projection returns are not compared: those it
//! returns for a change, such as a source control read, follow from state
//! it also writes.
//!
//! Every unit test built with debug assertions runs it, and fails on a
//! difference. A development build runs it when `CAGENTS_PROJECTION_ORACLE`
//! is set and logs each difference under the `cagents::projection_oracle`
//! target instead. Such a session keeps what each check's full projection
//! repaired and runs the tasks it returned, so it behaves as though every
//! update projected everything, and shows no stale state a wrong
//! declaration leaves: the log is the evidence. It adds full projections to
//! narrower updates, so optimized timing workloads run without it, and the
//! measurements that run with debug assertions turn it off with [`disable`].

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

thread_local! {
    /// Set while the check's own full projection runs, so the tests' counts
    /// of projections, chrome syncs and owner resolutions count only the
    /// updates' own work.
    static VERIFYING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// How many updates are running on this thread: one inside another
    /// is dispatched part way through it.
    static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Whether the outermost update running brought the derived state up to
    /// date before its message.
    static SETTLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Whether the outermost update running has reached its projection,
    /// rather than returning from its handler before it.
    static PROJECTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The last outermost update, when it returned before its projection
    /// from state that was not brought up to date first, so that only the
    /// next one settled can tell whether it left the derived state stale.
    static SKIPPED: std::cell::RefCell<Option<Box<str>>> = const { std::cell::RefCell::new(None) };
    #[cfg(test)]
    static DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(test)]
    static UNSETTLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The last outermost update's message, which a development build names
    /// when the next one finds the derived state stale.
    #[cfg(not(test))]
    static PREVIOUS: std::cell::RefCell<Box<str>> = std::cell::RefCell::default();
}

/// An update under way, from [`Cagents::settle_projection`] until it drops.
pub(super) struct Running(());

impl Drop for Running {
    fn drop(&mut self) {
        DEPTH.set(DEPTH.get() - 1);
    }
}

/// Whether updates check their projections.
pub(super) fn enabled() -> bool {
    #[cfg(test)]
    {
        cfg!(debug_assertions) && !DISABLED.get()
    }
    #[cfg(not(test))]
    {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ENABLED.get_or_init(|| {
            let enabled = std::env::var_os("CAGENTS_PROJECTION_ORACLE").is_some();
            if enabled {
                // So a log without reports shows the check ran.
                tracing::info!(
                    target: "cagents::projection_oracle",
                    "checking every narrower update projection against the full one"
                );
            }
            enabled
        })
    }
}

/// Turns the check off on this thread, for a workload that times updates.
#[cfg(test)]
pub(super) fn disable() {
    DISABLED.set(true);
}

/// Stops bringing the derived state up to date before updates on this
/// thread, so a test sees only what the updates themselves project. Each
/// narrower projection is still checked.
#[cfg(test)]
pub(super) fn disable_settling() {
    UNSETTLED.set(true);
}

/// Whether the check's own full projection is running.
#[cfg(test)]
pub(super) fn verifying() -> bool {
    VERIFYING.get()
}

/// A map's entries as text, ordered by their text, so two maps holding the
/// same entries read the same whatever order they iterate in.
pub(super) fn sorted_entries<K: Debug, V: Debug>(map: impl IntoIterator<Item = (K, V)>) -> String {
    let mut entries = map
        .into_iter()
        .map(|(key, value)| format!("{key:?}: {value:?}"))
        .collect::<Vec<_>>();
    entries.sort_unstable();
    format!("{{{}}}", entries.join(", "))
}

/// A set's items as text, ordered by their text.
pub(super) fn sorted_items<T: Debug>(set: impl IntoIterator<Item = T>) -> String {
    let mut items = set
        .into_iter()
        .map(|item| format!("{item:?}"))
        .collect::<Vec<_>>();
    items.sort_unstable();
    format!("{{{}}}", items.join(", "))
}

/// Fails unless the derived state is what a full projection makes it: the
/// updates so far have left nothing stale.
#[cfg(test)]
pub(super) fn assert_projected(app: &mut Cagents) {
    let current = Snapshot::capture(app);
    VERIFYING.set(true);
    drop(app.project_workspace(false));
    VERIFYING.set(false);
    let differences = current.differences(&Snapshot::capture(app));
    assert!(
        differences.is_empty(),
        "derived state is stale:\n{}",
        differences.join("\n")
    );
}

/// Fails the test, or logs the report in a development build.
fn report(report: &str) {
    #[cfg(test)]
    panic!("{report}");
    #[cfg(not(test))]
    tracing::error!(target: "cagents::projection_oracle", "{report}");
}

/// Everything the full projection writes, as comparable values.
#[derive(Debug, PartialEq)]
pub(super) struct Snapshot {
    sidebar_model: SidebarModel,
    sidebar_state: SidebarState,
    channel_failures: String,
    card_chrome: BTreeMap<CardId, String>,
    card_indices: BTreeMap<CardId, usize>,
    terminal_ordinals: BTreeMap<CardId, u32>,
    terminal_next_ordinals: BTreeMap<PathBuf, u32>,
    pane_titles: Vec<(CardId, String)>,
    window_title: String,
    pane_inspectors: String,
    pull_request_inspectors: String,
    transcript_probes: String,
    automation_reads: String,
    worktree_owners: BTreeMap<CardId, PathBuf>,
    protections: BTreeSet<PathBuf>,
    /// The panes whose working directory and find bar are remembered:
    /// the chrome forgets closed ones.
    terminal_directories: BTreeSet<CardId>,
    terminal_finds: BTreeSet<CardId>,
    /// The source control, branch comparison and history reads that are
    /// current: binding the source control to other work cancels them.
    source_reads: String,
}

impl Snapshot {
    pub(super) fn capture(app: &Cagents) -> Self {
        #[cfg_attr(not(test), expect(unused_mut))]
        let mut sidebar_state = app.sidebar_state.clone();
        #[cfg(test)]
        sidebar_state.forget_attention_rebuilds();
        Self {
            sidebar_model: app.sidebar_model.clone(),
            sidebar_state,
            channel_failures: app.channel_ui.projection_view(),
            card_chrome: app
                .card_chrome
                .iter()
                .map(|(card, chrome)| (*card, format!("{chrome:?}")))
                .collect(),
            card_indices: app
                .card_indices
                .iter()
                .map(|(card, index)| (*card, *index))
                .collect(),
            terminal_ordinals: app
                .chrome_ui
                .terminal_ordinals
                .iter()
                .map(|(card, ordinal)| (*card, *ordinal))
                .collect(),
            terminal_next_ordinals: app
                .chrome_ui
                .terminal_next_ordinals
                .iter()
                .map(|(scope, ordinal)| (scope.clone(), *ordinal))
                .collect(),
            pane_titles: app
                .workspace
                .order()
                .filter_map(|card| Some((card, app.workspace.card(card)?.accessible_name.clone())))
                .collect(),
            window_title: app.core.window.header_title.clone(),
            pane_inspectors: app.pane_inspectors.projection_view(),
            pull_request_inspectors: app.pull_request_inspector.projection_view(),
            transcript_probes: app.terminal_failure.projection_view(),
            automation_reads: app.automation_approval.projection_view(),
            worktree_owners: app
                .worktree_owners
                .iter()
                .map(|(card, owner)| (*card, owner.clone()))
                .collect(),
            protections: app.projected_protections.clone(),
            terminal_directories: app.terminal_working_directories.keys().copied().collect(),
            terminal_finds: app.terminal_find.keys().copied().collect(),
            source_reads: format!(
                "source {} running {}; {}; {}",
                app.refresh_ui.source_generation,
                app.refresh_ui.source_cancel.is_some(),
                app.branch_ui.projection_view(),
                app.history_ui.projection_view(),
            ),
        }
    }

    /// The parts that differ from `full`'s, each with its first differing
    /// lines.
    fn differences(&self, full: &Self) -> Vec<String> {
        let mut differences = Vec::new();
        let mut compare = |part: &str, narrow: &dyn Debug, full: &dyn Debug, equal: bool| {
            if !equal {
                differences.push(format!("{part}:\n{}", first_difference(narrow, full)));
            }
        };
        macro_rules! parts {
            ($($field:ident),* $(,)?) => {
                $(compare(
                    stringify!($field),
                    &self.$field,
                    &full.$field,
                    self.$field == full.$field,
                );)*
            };
        }
        parts!(
            sidebar_model,
            sidebar_state,
            channel_failures,
            card_chrome,
            card_indices,
            terminal_ordinals,
            terminal_next_ordinals,
            pane_titles,
            window_title,
            pane_inspectors,
            pull_request_inspectors,
            transcript_probes,
            automation_reads,
            worktree_owners,
            protections,
            terminal_directories,
            terminal_finds,
            source_reads,
        );
        differences
    }
}

/// The first few lines where two values' pretty `Debug` output differs.
fn first_difference(narrow: &dyn Debug, full: &dyn Debug) -> String {
    let narrow = format!("{narrow:#?}");
    let full = format!("{full:#?}");
    let mut narrow_lines = narrow.lines();
    let mut full_lines = full.lines();
    let mut line = 0;
    loop {
        match (narrow_lines.next(), full_lines.next()) {
            (Some(left), Some(right)) if left == right => line += 1,
            (None, None) => {
                // Equal text from unequal values: a map whose order differs.
                return "  (equal when printed)".into();
            }
            (left, right) => {
                return format!(
                    "  at line {line}\n  narrow:\n    {}\n  full:\n    {}",
                    following(left, narrow_lines),
                    following(right, full_lines)
                );
            }
        }
    }
}

/// A differing line and the few after it.
fn following<'a>(first: Option<&'a str>, lines: std::str::Lines<'a>) -> String {
    const LINES: usize = 6;
    first
        .into_iter()
        .chain(lines.take(LINES - 1))
        .collect::<Vec<_>>()
        .join("\n    ")
}

impl Cagents {
    /// Before an outermost update whose message may project less than the
    /// whole workspace, brings everything the full projection derives up to
    /// date, so the check after it judges only what that message changed.
    /// Returns the projection's tasks, and the update's [`Running`], which it
    /// holds until it returns.
    ///
    /// Tests often arrange state directly rather than through updates, and
    /// leave its projection to the next one: that is no claim of the next
    /// message's. Whatever settling finds stale is put down to such an
    /// arrangement only when the update before it projected: that update
    /// left the derived state current, so only something outside an update
    /// changed it since. When the update before it instead returned from its
    /// handler before projecting, from state that was not settled first, the
    /// stale state is reported against that update. It cannot be told apart
    /// from state a test then arranged directly without projecting it, so
    /// such a test projects what it arranged with `project_workspace`. An
    /// update that was settled first and returns before projecting is
    /// checked as it returns, by [`Self::check_skipped_projection`], and
    /// never blamed here.
    ///
    /// A message that always projects everything is not settled, so what
    /// its handler does is tested as it stands. A development build has no
    /// arrangements, so it also logs whatever it settles after an update that
    /// projected: something outside the updates changed what the projection
    /// reads.
    pub(super) fn settle_projection(&mut self, message: &Message) -> (Running, Task<Message>) {
        let outermost = DEPTH.replace(DEPTH.get() + 1) == 0;
        let running = Running(());
        if !outermost {
            return (running, Task::none());
        }
        PROJECTED.set(false);
        let skipped = SKIPPED.take();
        #[cfg(test)]
        let settling = !UNSETTLED.get();
        #[cfg(not(test))]
        let settling = true;
        let settle = settling && enabled() && self.may_project_narrowly(message);
        SETTLED.set(settle);
        if !settle {
            return (running, Task::none());
        }
        // A test's own arrangements are settled without a comparison.
        let before = (cfg!(not(test)) || skipped.is_some()).then(|| Snapshot::capture(self));
        VERIFYING.set(true);
        let task = self.project_workspace(false);
        VERIFYING.set(false);
        let Some(before) = before else {
            return (running, task);
        };
        let differences = before.differences(&Snapshot::capture(self));
        if differences.is_empty() {
            return (running, task);
        }
        let message = crate::performance::ui_trace::label(message);
        let differences = differences.join("\n");
        match skipped {
            Some(skipped) => report(&format!(
                "{skipped} returned before its projection, and derived state was stale at the \
                 next update, {message}:\n{differences}"
            )),
            #[cfg(not(test))]
            None => {
                let previous = PREVIOUS.with_borrow(|previous| previous.clone());
                report(&format!(
                    "derived state was stale before {message}, after {previous}:\n{differences}"
                ));
            }
            #[cfg(test)]
            None => {}
        }
        (running, task)
    }

    /// Whether `update` may project less than the whole workspace for
    /// `message`: the messages it, or the handler of each, can declare a
    /// narrower projection for. A narrower projection for any other message
    /// is still checked, but against state that may not have been current
    /// before it.
    fn may_project_narrowly(&self, message: &Message) -> bool {
        repaint_tick(message)
            || background_poll(message)
            || self.navigation_save_is_stale(message)
            || matches!(message, Message::ChatUi(action) if action.local_scope().is_some())
            || matches!(
                message,
                Message::ReplicaCaptureDue(_)
                    | Message::TerminalWake { .. }
                    | Message::WorkspaceModelObserved(_)
                    | Message::KeyEvent { .. }
                    | Message::ChromeUi(chrome_ui::Action::HoverTab(_))
                    | Message::Frame(_)
                    | Message::Viewport { .. }
            )
    }

    /// After an outermost update whose projection was narrower than `Full`,
    /// runs the full one and reports what it changed. Returns the full
    /// projection's tasks: in a development build what it repaired stays
    /// repaired, so one stale update is reported once rather than at every
    /// later one.
    ///
    /// An update inside another is not checked on its own: the outer one
    /// may have changed what the projection reads before dispatching it,
    /// and projects after it.
    pub(super) fn verify_projection(
        &mut self,
        message: Option<&str>,
        projection: &Projection,
        camera_only: bool,
        reconcile_row_selection: bool,
    ) -> Task<Message> {
        if DEPTH.get() == 1 {
            PROJECTED.set(true);
        }
        let Some(message) = message else {
            return Task::none();
        };
        if DEPTH.get() != 1 || (*projection == Projection::Full && !camera_only) {
            return Task::none();
        }
        let narrow = Snapshot::capture(self);
        VERIFYING.set(true);
        let task = self.project_workspace(reconcile_row_selection);
        VERIFYING.set(false);
        let differences = narrow.differences(&Snapshot::capture(self));
        if !differences.is_empty() {
            let projection = if camera_only {
                "Camera"
            } else {
                projection.name()
            };
            let unsettled = if SETTLED.get() {
                ""
            } else {
                " (its state was not brought up to date before it: see `may_project_narrowly`)"
            };
            report(&format!(
                "the {projection} projection of {message} left derived state stale{unsettled}:\n{}",
                differences.join("\n")
            ));
        }
        task
    }

    /// After an outermost update whose handler returned before projecting,
    /// as though nothing derived had changed, runs the full projection and
    /// reports what it changed, when the update's state was brought up to
    /// date before it. Otherwise leaves the update for the next settled one
    /// to judge. Returns the full projection's tasks.
    pub(super) fn check_skipped_projection(&mut self, message: Option<&str>) -> Task<Message> {
        let Some(message) = message else {
            return Task::none();
        };
        if DEPTH.get() != 1 {
            return Task::none();
        }
        #[cfg(not(test))]
        PREVIOUS.set(message.into());
        if PROJECTED.get() {
            return Task::none();
        }
        if !SETTLED.get() {
            SKIPPED.set(Some(message.into()));
            return Task::none();
        }
        let returned = Snapshot::capture(self);
        VERIFYING.set(true);
        let task = self.project_workspace(false);
        VERIFYING.set(false);
        let differences = returned.differences(&Snapshot::capture(self));
        if !differences.is_empty() {
            report(&format!(
                "{message} returned before its projection and left derived state stale:\n{}",
                differences.join("\n")
            ));
        }
        task
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::terminal_output_tests::projections;

    fn character(value: &str, modifiers: keyboard::Modifiers) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(value.into()),
            modified_key: keyboard::Key::Character(value.into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyB),
            location: keyboard::Location::Standard,
            modifiers,
            text: Some(value.into()),
            repeat: false,
        }
    }

    fn released(value: &str) -> keyboard::Event {
        keyboard::Event::KeyReleased {
            key: keyboard::Key::Character(value.into()),
            modified_key: keyboard::Key::Character(value.into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyB),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
        }
    }

    fn named(key: keyboard::key::Named, modifiers: keyboard::Modifiers, pressed: bool) -> Message {
        let physical_key = keyboard::key::Physical::Code(match key {
            keyboard::key::Named::Tab => keyboard::key::Code::Tab,
            keyboard::key::Named::Escape => keyboard::key::Code::Escape,
            _ => keyboard::key::Code::ControlLeft,
        });
        let key = keyboard::Key::Named(key);
        let event = if pressed {
            keyboard::Event::KeyPressed {
                modified_key: key.clone(),
                key,
                physical_key,
                location: keyboard::Location::Standard,
                modifiers,
                text: None,
                repeat: false,
            }
        } else {
            keyboard::Event::KeyReleased {
                modified_key: key.clone(),
                key,
                physical_key,
                location: keyboard::Location::Standard,
                modifiers,
            }
        };
        Message::KeyEvent {
            event,
            captured: false,
        }
    }

    /// Two panes in different worktrees, the first focused, with everything
    /// they derive projected.
    fn two_worktree_app() -> (Cagents, CardId, CardId, crate::model::ChannelId) {
        let (mut app, channel) = crate::app::tests::alt_tab_test_app();
        let first = app.durable.cards[0].id;
        let second = app.durable.cards[1].id;
        Arc::make_mut(&mut app.durable).focused = Some(first);
        // Named apart: unnamed, both would be the first terminal of their
        // worktree.
        app.chrome_ui.user_titles.insert(first, "Main build".into());
        app.chrome_ui
            .user_titles
            .insert(second, "Feature tests".into());
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.cards = app
            .durable
            .cards
            .iter()
            .map(|card| (card.id, CardRuntime::Loading))
            .collect();
        drop(app.project_workspace(true));
        assert_eq!(app.workspace.focused(), Some(first));
        (app, first, second, channel)
    }

    /// Ctrl+Tab moves the focus as it is pressed, and everything that shows
    /// the focused pane follows at once, not when the keys are released or
    /// a later update projects. Settling is off, so nothing but the keys'
    /// own updates projects.
    #[test]
    fn cycling_panes_by_keyboard_shows_the_new_focus_at_once() {
        use keyboard::key::Named;
        disable_settling();
        let (mut app, first, second, _) = two_worktree_app();
        let title = app.core.window.header_title.clone();
        let worktree = app.sidebar_state.active_worktree().cloned();
        assert_eq!(app.sidebar_state.focused_card(), Some(first));
        let ctrl = keyboard::Modifiers::CTRL;
        drop(app.update(Message::KeyEvent {
            event: keyboard::Event::ModifiersChanged(ctrl),
            captured: false,
        }));
        drop(app.update(named(Named::Tab, ctrl, true)));
        assert_eq!(app.workspace.focused(), Some(second));
        let shown = |app: &Cagents| {
            (
                app.core.window.header_title.clone(),
                app.sidebar_state.focused_card(),
                app.sidebar_state.active_worktree().cloned(),
            )
        };
        let after_press = shown(&app);
        assert_ne!(after_press.0, title, "the window title names the new pane");
        assert_eq!(after_press.1, Some(second), "Attention marks the new pane");
        assert_ne!(
            after_press.2, worktree,
            "source control follows its worktree"
        );
        drop(app.update(named(Named::Tab, ctrl, false)));
        drop(app.update(named(Named::Control, keyboard::Modifiers::empty(), false)));
        drop(app.update(Message::KeyEvent {
            event: keyboard::Event::ModifiersChanged(keyboard::Modifiers::empty()),
            captured: false,
        }));
        assert_eq!(shown(&app), after_press);
        assert_projected(&mut app);
    }

    /// Escape closing a dialog leaves nothing derived behind it, without a
    /// later update projecting it.
    #[test]
    fn dismissing_a_dialog_by_keyboard_projects_at_once() {
        use keyboard::key::Named;
        disable_settling();
        let (mut app, _, _, channel) = two_worktree_app();
        drop(
            app.update(Message::ChannelUi(channel_ui::Action::RequestDelete(
                channel,
            ))),
        );
        assert!(app.channel_ui.modal_open());
        drop(app.update(named(Named::Escape, keyboard::Modifiers::empty(), true)));
        assert!(!app.channel_ui.modal_open());
        assert_projected(&mut app);
    }

    /// Escape closing an inspector that covers a narrow pane updates what
    /// the inspector drew, such as the source control it bound, at once.
    #[test]
    fn closing_an_overlaid_inspector_by_keyboard_projects_at_once() {
        use keyboard::key::Named;
        disable_settling();
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        drop(app.project_workspace(true));
        drop(app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event: WorkspaceViewportEvent::Resized(500.0),
        }));
        drop(app.update(Message::TogglePaneInspector(card)));
        app.settle_pane_inspectors();
        assert_eq!(
            app.pane_inspector_layout(card),
            Some(InspectorLayout::Overlay)
        );
        drop(app.update(named(Named::Escape, keyboard::Modifiers::empty(), true)));
        assert!(!app.pane_inspectors.is_open(card));
        assert_projected(&mut app);
    }

    /// A handler that returns before projecting, from settled state, and
    /// leaves something derived stale is reported as it returns rather than
    /// repaired by the next update.
    #[test]
    fn a_handler_returning_before_its_projection_is_reported() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        // As the outermost update would be, settled and returning early.
        let (running, settled) = app.settle_projection(&Message::KeyEvent {
            event: released("a"),
            captured: false,
        });
        drop(settled);
        let state = Arc::make_mut(&mut app.durable)
            .cards
            .iter_mut()
            .find(|state| state.id == card)
            .unwrap();
        state.title = "Renamed behind the projection".into();
        let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(app.check_skipped_projection(Some("KeyEvent")));
        }));
        drop(running);
        let report = report.expect_err("the stale chrome is reported");
        let report = report
            .downcast_ref::<String>()
            .expect("the report is formatted");
        assert!(
            report.contains("KeyEvent returned before its projection"),
            "{report}"
        );
        assert!(report.contains("card_chrome:"), "{report}");
    }

    /// An update that returned before projecting from state that was not
    /// settled is named when the next settled update finds stale state.
    #[test]
    fn stale_state_after_an_unsettled_early_return_names_that_update() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        drop(app.project_workspace(false));
        SKIPPED.set(Some("Earlier".into()));
        let state = Arc::make_mut(&mut app.durable)
            .cards
            .iter_mut()
            .find(|state| state.id == card)
            .unwrap();
        state.title = "Renamed behind the projection".into();
        let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(app.update(Message::KeyEvent {
                event: released("a"),
                captured: false,
            }));
        }));
        DEPTH.set(0);
        let report = report.expect_err("the stale chrome is reported");
        let report = report
            .downcast_ref::<String>()
            .expect("the report is formatted");
        assert!(
            report.contains("Earlier returned before its projection"),
            "{report}"
        );
        assert!(report.contains("card_chrome:"), "{report}");
    }

    /// Typing into a field reaches the update as keys the field took and
    /// their releases, with modifier changes between: none of it is
    /// anything the workspace projects.
    #[test]
    fn keys_nothing_answers_skip_the_projection() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        drop(app.update(Message::KeyEvent {
            event: character("a", keyboard::Modifiers::empty()),
            captured: true,
        }));
        let before = projections();
        for value in ["t", "y", "p", "e"] {
            drop(app.update(Message::KeyEvent {
                event: character(value, keyboard::Modifiers::empty()),
                captured: true,
            }));
            drop(app.update(Message::KeyEvent {
                event: released(value),
                captured: false,
            }));
        }
        drop(app.update(Message::KeyEvent {
            event: keyboard::Event::ModifiersChanged(keyboard::Modifiers::SHIFT),
            captured: false,
        }));
        assert_eq!(app.keyboard_modifiers, keyboard::Modifiers::SHIFT);
        assert_eq!(projections(), before);
    }

    /// A key that runs a workspace command keeps the full projection, as
    /// the command may change anything.
    #[test]
    fn a_key_that_runs_a_command_projects_everything() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let collapsed = app.chrome_ui.projects_collapsed;
        let before = projections();
        drop(app.update(Message::KeyEvent {
            event: character("B", keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT),
            captured: false,
        }));
        assert_ne!(app.chrome_ui.projects_collapsed, collapsed);
        assert_eq!(projections(), before + 1);
    }

    /// The pointer crossing the tab strip changes only which tab is drawn
    /// hovered.
    #[test]
    fn hovering_tabs_skips_the_projection() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let before = projections();
        drop(app.update(Message::ChromeUi(chrome_ui::Action::HoverTab(Some(card)))));
        assert_eq!(app.chrome_ui.hovered_tab, Some(card));
        drop(app.update(Message::ChromeUi(chrome_ui::Action::HoverTab(None))));
        assert_eq!(app.chrome_ui.hovered_tab, None);
        assert_eq!(projections(), before);
    }

    /// A narrower projection that leaves a derived part stale is reported,
    /// naming the message and the part.
    #[test]
    fn a_wrong_declaration_is_reported() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        drop(app.project_workspace(false));
        let state = Arc::make_mut(&mut app.durable)
            .cards
            .iter_mut()
            .find(|state| state.id == card)
            .unwrap();
        state.title = "Renamed behind the projection".into();
        // As the outermost update would be.
        DEPTH.set(1);
        let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(app.verify_projection(Some("Test"), &Projection::Unchanged, false, false));
        }));
        DEPTH.set(0);
        let report = report.expect_err("the stale chrome is reported");
        let report = report
            .downcast_ref::<String>()
            .expect("the report is formatted");
        assert!(
            report.contains("the Unchanged projection of Test"),
            "{report}"
        );
        assert!(report.contains("card_chrome:"), "{report}");
        // The full projection the check ran repaired it.
        assert!(
            app.card_chrome[&card]
                .tooltip
                .contains("Renamed behind the projection")
        );
    }
}
