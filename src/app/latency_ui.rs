use super::*;
use crate::performance::Generation;
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree},
};
use cosmic::iced::{Event, Rectangle, Size, Vector, mouse};
use cosmic::{Renderer, Theme};

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
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            layout,
            renderer,
            destinations,
        );
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
    pub(super) fn latency_fixture_step(&mut self) -> Task<Message> {
        if self.latency.pending_generation().is_some() {
            return Task::none();
        }
        let Some(fixture) = &mut self.latency_fixture else {
            return Task::none();
        };
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
            status: None,
            save_health: None,
            restore_warnings: Vec::new(),
            settings_warnings: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_widget::forwarding_test::{leaf, renderer};
    use cosmic::iced::{Point, advanced::clipboard::DndDestinationRectangles};

    #[test]
    fn redraw_instrumentation_preserves_child_drop_targets_and_layout() {
        let renderer = renderer();
        let (content, visits) = leaf::<Message>(
            Size::new(100.0, 60.0),
            vec![(
                1,
                Rectangle::new(Point::new(5.0, 7.0), Size::new(80.0, 40.0)),
            )],
        );
        let mut widget = instrument(content, None, false);
        let mut tree = Tree::new(&widget);
        let node = widget
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(100.0, 60.0)),
            )
            .move_to(Point::new(20.0, 30.0));
        let offset = Vector::new(4.0, -20.0);
        let mut destinations = DndDestinationRectangles::new();
        widget.as_widget().drag_destinations(
            &tree,
            Layout::new(&node).with_virtual_offset(offset),
            &renderer,
            &mut destinations,
        );
        let actual: Vec<_> = destinations
            .into_rectangles()
            .into_iter()
            .map(|destination| {
                (
                    destination.id,
                    destination.rectangle.x,
                    destination.rectangle.y,
                )
            })
            .collect();
        assert_eq!(actual, vec![(1, 25.0, 37.0)]);
        assert_eq!(visits.offsets("drag"), vec![offset]);
    }
}
