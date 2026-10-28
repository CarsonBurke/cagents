use super::*;
use crate::performance::Generation;
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree},
};
use cosmic::iced::{Event, Rectangle, Size, Vector, mouse};
use cosmic::{Renderer, Theme};

#[cfg(test)]
mod isolation_tests;

pub(super) fn instrument(
    content: Element<'_, Message>,
    generation: Option<Generation>,
    fixture_start: bool,
) -> Element<'_, Message> {
    Element::new(RedrawGeneration {
        content,
        generation,
        fixture_start,
    })
}
struct RedrawGeneration<'a> {
    content: Element<'a, Message>,
    generation: Option<Generation>,
    fixture_start: bool,
}
impl Widget<Message, Theme, Renderer> for RedrawGeneration<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
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
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if let Event::Window(window::Event::RedrawRequested(at)) = event {
            if let Some(generation) = self.generation {
                shell.publish(Message::LatencyRedraw {
                    generation,
                    at: *at,
                });
            } else if self.fixture_start {
                shell.publish(Message::LatencyFixtureStep);
            }
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
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
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
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }
}

impl Cagents {
    /// Native fixtures mount the normal App tree, but own no service state.
    /// Keep this positive list typed: a newly added command is refused until
    /// its local effects have been assessed, rather than inheriting execution.
    pub(super) fn update_native_fixture(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::UtilityPaneUi(message) => self.update_native_fixture(*message),
            Message::StartupWindowOpened(_) => Task::none(),
            Message::WindowCloseRequested(window) if self.core.main_window_id() == Some(window) => {
                iced::window::close(window)
                    .chain(iced::exit())
                    .map(cosmic::Action::App)
            }
            Message::WindowClosed(window) if self.core.main_window_id() == Some(window) => {
                iced::exit().map(cosmic::Action::App)
            }
            Message::WindowResized(width, height) => {
                if width.is_finite() && width > 0.0 {
                    self.window_width = width;
                }
                if height.is_finite() && height > 0.0 {
                    self.window_height = height;
                }
                self.sync_context_overlay_state()
            }
            Message::WindowScaleFactorChanged(scale) => {
                if scale.is_finite() && scale > 0.0 {
                    self.window_scale_factor = scale;
                }
                Task::none()
            }
            Message::RefreshUi(refresh_ui::Action::Focused(window, focused)) => {
                if self.core.main_window_id() == Some(window) {
                    self.refresh_ui.focused = focused;
                }
                Task::none()
            }
            Message::Viewport { generation, event }
                if generation == self.workspace_view_generation =>
            {
                if matches!(event, WorkspaceViewportEvent::Reordered { .. })
                    || (self.workspace_inputs_blocked()
                        && matches!(
                            event,
                            WorkspaceViewportEvent::DragScrolled(_)
                                | WorkspaceViewportEvent::PointerFocused(_)
                                | WorkspaceViewportEvent::PointerRested(_)
                        ))
                    || (matches!(event, WorkspaceViewportEvent::PointerRested(_))
                        && !self.settings.focus_follows_mouse)
                {
                    return Task::none();
                }
                let (update, scroll) = update_workspace_viewport(&mut self.workspace, event);
                self.apply_native_fixture_viewport(update, scroll)
            }
            Message::FocusCard(card) if !self.workspace_inputs_blocked() => {
                match self.workspace.focus(card) {
                    Ok(update) => self.apply_native_fixture_viewport(update, true),
                    Err(_) => Task::none(),
                }
            }
            Message::FocusAdjacent(direction) if !self.workspace_inputs_blocked() => {
                let before = self.workspace.focused();
                match self.workspace.focus_adjacent(direction) {
                    Ok(update) => {
                        if before != self.workspace.focused() {
                            self.latency.commit(crate::performance::Action::CardFocus);
                            self.latency_focus = self.workspace.focused();
                        }
                        self.apply_native_fixture_viewport(update, true)
                    }
                    Err(_) => Task::none(),
                }
            }
            Message::ResizeCard { card, width } if !self.workspace_inputs_blocked() => {
                let viewport = self.workspace.viewport_width();
                if !width.is_finite() || viewport <= f32::EPSILON {
                    return Task::none();
                }
                let limits = self.workspace.begin_card_resize(card);
                let width = width.clamp(limits.minimum, limits.maximum);
                let fraction = self.workspace.preferred_width_for_resize(card, width) / viewport;
                match self
                    .workspace
                    .set_card_width(card, CardWidth::Fraction(fraction))
                {
                    Ok(update) => self.apply_native_fixture_viewport(update, true),
                    Err(_) => Task::none(),
                }
            }
            Message::ResizeCardFinished(card) => {
                self.workspace.finish_card_resize(card);
                Task::none()
            }
            Message::Frame(at) => {
                let inspector = self.advance_context_inspector_transition(at);
                let Some(elapsed) = self.frame_clock.elapsed(at) else {
                    return inspector;
                };
                let update = self.workspace.tick(elapsed);
                Task::batch([inspector, self.apply_native_fixture_viewport(update, true)])
            }
            Message::Sidebar(SidebarAction::Resize { width }) => {
                if width.is_finite() {
                    self.settings.sidebar_width =
                        width.clamp(sidebar::EXPANDED_WIDTH_MIN, sidebar::EXPANDED_WIDTH_MAX);
                }
                Task::none()
            }
            Message::Sidebar(SidebarAction::ResizeFinished) => Task::none(),
            Message::Sidebar(SidebarAction::ScrollProjects(offset)) => {
                iced::widget::operation::scroll_by(
                    widget::Id::new("sidebar.projects.scroll"),
                    iced::widget::operation::AbsoluteOffset { x: 0.0, y: offset },
                )
                .map(cosmic::Action::App)
            }
            Message::Sidebar(action)
                if action.fixture_local() && !matches!(action, SidebarAction::ToggleProjects) =>
            {
                self.sidebar_state.apply(&action);
                Task::none()
            }
            Message::Sidebar(SidebarAction::ToggleProjects)
            | Message::ChromeUi(chrome_ui::Action::ToggleProjects) => {
                self.chrome_ui.projects_collapsed = !self.chrome_ui.projects_collapsed;
                Task::none()
            }
            Message::ChromeUi(
                action @ (chrome_ui::Action::HoverTab(_)
                | chrome_ui::Action::TabsScrolled(_)
                | chrome_ui::Action::Close),
            ) => self.update_chrome(action),
            Message::ToggleContextInspector => {
                self.set_context_inspector_open(!self.context_inspector_open)
            }
            Message::SelectContextTab(tab) => {
                self.context_tab = tab;
                Task::none()
            }
            Message::ContextInspector(InspectorAction::ResizeFinished) => Task::none(),
            Message::ContextInspector(
                action @ (InspectorAction::Close
                | InspectorAction::ToggleDiagnostics(_)
                | InspectorAction::Resize { .. }),
            ) => self.handle_context_inspector_action(action),
            Message::ChatUi(action) if action.fixture_local() => self.handle_fixture_chat(action),
            Message::KeyEvent { event, captured } => {
                // Widgets retain native editing/selection. Global commands
                // are limited to navigation; no start, retry, file or account actions.
                if let Some(task) = self.chat_find_key(&event) {
                    return task;
                }
                if captured {
                    return Task::none();
                }
                if let keyboard::Event::KeyPressed { key, modifiers, .. } = event {
                    match key {
                        keyboard::Key::Character(ref key)
                            if modifiers.control() && key.eq_ignore_ascii_case("f") =>
                        {
                            self.open_chat_find().unwrap_or_else(Task::none)
                        }
                        keyboard::Key::Named(keyboard::key::Named::Escape) => {
                            self.chrome_ui.close();
                            self.set_context_inspector_open(false)
                        }
                        _ => Task::none(),
                    }
                } else {
                    Task::none()
                }
            }
            Message::LatencyFixtureStep => self.latency_fixture_step(),
            Message::LatencyRedraw { generation, at } => {
                if self.latency_focus == self.workspace.focused()
                    && self.latency.redraw_scheduled(generation, at)
                {
                    self.latency_fixture_step()
                } else {
                    Task::none()
                }
            }
            _ => {
                self.status = Some(TransientNotice::warning(
                    "This native fixture supports local UI interaction only",
                ));
                Task::none()
            }
        }
    }

    fn apply_native_fixture_viewport(
        &mut self,
        update: ViewportUpdate,
        scroll: bool,
    ) -> Task<Message> {
        apply_durable_viewport_update(
            Arc::make_mut(&mut self.durable),
            self.workspace.focused(),
            &update.transitions,
        );
        self.sync_native_fixture_visibility();
        self.chrome_ui.tabs_focus = self.workspace.focused();
        let focus = self
            .workspace
            .take_widget_focus_request()
            .map_or_else(Task::none, |card| {
                iced::widget::operation::focus(self.card_keyboard_focus(card))
                    .map(cosmic::Action::App)
            });
        let scroll = if scroll && update.offset_changed {
            self.synchronize_workspace_scroll()
        } else {
            Task::none()
        };
        Task::batch([
            focus,
            scroll,
            self.reveal_active_tab(),
            self.sync_context_overlay_state(),
        ])
    }

    pub(super) fn native_fixture_subscription(&self) -> Subscription<Message> {
        let input = native_window_subscription();
        let frames = self
            .core
            .main_window_id()
            .filter(|_| {
                self.workspace.is_animating() || self.context_inspector_transition.is_some()
            })
            .map_or_else(Subscription::none, |window| {
                workspace_widget::animation_frames(window, Message::Frame)
            });
        Subscription::batch([input, frames, self.fixture_chat_subscription()])
    }

    pub(super) fn latency_fixture_step(&mut self) -> Task<Message> {
        if self.latency.pending_generation().is_some() {
            return Task::none();
        }
        let Some(fixture) = &mut self.latency_fixture else {
            return Task::none();
        };
        if !fixture.automatic() {
            return Task::none();
        }
        self.latency_fixture_started = true;
        match fixture.next_direction() {
            Some(direction) => self.update(Message::FocusAdjacent(direction)),
            None => {
                if let Err(error) = fixture.finish(
                    &mut self.latency,
                    f64::from(self.window_scale_factor * self.settings.ui_scale),
                ) {
                    eprintln!("redraw scheduling fixture failed: {error}");
                }
                // Dispose the native surface and renderer before their Wayland
                // event-loop connection. Immediate global exit races teardown.
                match self.core.main_window_id() {
                    Some(window) => iced::window::close(window)
                        .chain(iced::exit())
                        .map(cosmic::Action::App),
                    None => iced::exit().map(cosmic::Action::App),
                }
            }
        }
    }
    pub(super) fn fixture_bootstrap() -> Bootstrap {
        if let Some(fixture) = crate::latency_fixture::Fixture::from_env()
            && !fixture.automatic()
        {
            let mut bootstrap = chat_ui::resident_fixture::bootstrap();
            fixture.apply_interactive_appearance(&mut bootstrap.settings);
            return bootstrap;
        }
        let mut state = WorkspaceState::default();
        for index in 0..crate::latency_fixture::CARD_COUNT {
            let launch = LaunchRecipe {
                unset_environment: Default::default(),
                program: "/bin/false".into(),
                arguments: Vec::new(),
                working_directory: PathBuf::from("/"),
                environment: Default::default(),
            };
            state.cards.push(CardState {
                id: CardId::new(),
                generation: 0,
                title: format!("Fixture card {}", index + 1),
                kind: CardKind::Terminal(TerminalSession {
                    owned_codex_listener: None,
                    recovery_origin: None,
                    predecessor: None,
                    id: TerminalId::new(),
                    launch: launch.clone(),
                    transcript_path: None,
                    resume_hint: None,
                    continuity: TerminalContinuity::RelaunchAvailable {
                        recipe: launch,
                        last_exit: None,
                    },
                }),
                existence: Existence::Open,
                exposure: Exposure::Offscreen,
                residency: Residency::Failed {
                    typed_error: RuntimeError {
                        code: "fixture".into(),
                        message: "Synthetic latency workload; no terminal process".into(),
                    },
                    retryable: false,
                },
                width_fraction: 0.7,
            });
        }
        state.focused = state.cards.first().map(|card| card.id);
        Bootstrap {
            workspace: state,
            session: None,
            projects: Vec::new(),
            worktrees: Vec::new(),
            settings: AppSettings::default(),
            settings_path: None,
            settings_write_blocked: true,
            status: std::env::var_os(crate::latency_fixture::INTERACTIVE_ENV).map(|_| {
                "Offline native UI fixture; daemon and provider actions are unavailable".into()
            }),
            save_health: None,
            restore_warnings: Vec::new(),
            settings_warnings: Vec::new(),
        }
    }
}
