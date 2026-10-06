//! Compact, keyboard-accessible icon tabs and toolbar actions.

#[cfg(test)]
pub(crate) mod accessibility_probe;
mod icon;
mod responsive;
pub use icon::Icon;
pub use responsive::responsive;

use std::borrow::Cow;

use cosmic::{
    Element,
    iced::{self, Length},
    widget,
};

use crate::interaction::tooltip;

/// An informative glyph with the same resting ink as an inactive navigation
/// row. Use this outside buttons, where no control style supplies icon color.
pub fn passive_icon<'a, Message: 'a>(
    icon: impl Into<widget::icon::Handle>,
    size: u16,
) -> Element<'a, Message> {
    widget::container(widget::icon(icon.into()).size(size))
        .class(cosmic::theme::Container::custom(|theme| {
            iced::widget::container::Style {
                icon_color: Some(crate::design::muted_icon_color(theme)),
                ..Default::default()
            }
        }))
        .into()
}

/// Embedded application artwork does not depend on the installed icon theme.
pub fn ceditor_icon() -> widget::icon::Handle {
    static ICON: std::sync::LazyLock<widget::icon::Handle> = std::sync::LazyLock::new(|| {
        widget::icon::from_svg_bytes(
            include_bytes!("../packaging/io.github.cagents.ceditor.svg").as_slice(),
        )
        .symbolic(false)
    });
    ICON.clone()
}

/// Provider identity is shared by terminal and native-chat tab presentations.
pub fn provider_icon(provider: crate::agent::Provider) -> widget::icon::Handle {
    use std::sync::LazyLock;
    static OMP: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(include_bytes!("icons/omp.svg").as_slice()).symbolic(false)
    });
    static CLAUDE: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(include_bytes!("icons/claude.svg").as_slice()).symbolic(false)
    });
    static CODEX: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(include_bytes!("icons/codex.svg").as_slice()).symbolic(true)
    });
    match provider {
        crate::agent::Provider::Omp => OMP.clone(),
        crate::agent::Provider::Claude => CLAUDE.clone(),
        crate::agent::Provider::Codex => CODEX.clone(),
    }
}

pub struct IconControl<'a, Message> {
    button: widget::button::Button<'a, Message>,
    label: Cow<'a, str>,
    selected: bool,
    enabled: bool,
    tone: Option<crate::design::Tone>,
}

/// A centered, fixed-size icon target with a quiet surface selected state.
/// Native buttons retain keyboard focus, accessibility, and enabled cursors.
pub fn icon_control<'a, Message: Clone + 'a>(
    icon: impl Into<widget::icon::Handle>,
    label: impl Into<Cow<'a, str>>,
    selected: bool,
    enabled: bool,
    on_press: Option<Message>,
) -> IconControl<'a, Message> {
    let label = label.into();
    let content = widget::container(widget::icon(icon.into()).size(crate::design::ROW_ICON_SIZE))
        // COSMIC's transparent container resets inherited icon color. Keep the
        // button's semantic status tone through this alignment-only wrapper.
        .class(cosmic::theme::Container::custom(|_| {
            iced::widget::container::Style::default()
        }))
        .center(Length::Fill);
    IconControl {
        button: widget::button::custom(content)
            .name(label.clone())
            .selected(selected && enabled)
            .class(control_style(selected && enabled, None))
            .width(crate::design::PRIMARY_CONTROL_SIZE)
            .height(crate::design::PRIMARY_CONTROL_SIZE)
            .padding(0)
            .on_press_maybe(if enabled { on_press } else { None }),
        label,
        selected: selected && enabled,
        enabled,
        tone: None,
    }
}

/// The same target at caption scale, for controls that belong to a message
/// rather than to a toolbar.
pub fn compact_icon_control<'a, Message: Clone + 'a>(
    icon: impl Into<widget::icon::Handle>,
    label: impl Into<Cow<'a, str>>,
    on_press: Option<Message>,
) -> IconControl<'a, Message> {
    let label = label.into();
    let content =
        widget::container(widget::icon(icon.into()).size(crate::design::COMPACT_ICON_SIZE))
            .class(cosmic::theme::Container::custom(|_| {
                iced::widget::container::Style::default()
            }))
            .center(Length::Fill);
    IconControl {
        button: widget::button::custom(content)
            .name(label.clone())
            .class(control_style(false, None))
            .width(crate::design::COMPACT_CONTROL_SIZE)
            .height(crate::design::COMPACT_CONTROL_SIZE)
            .padding(0)
            .on_press_maybe(on_press),
        label,
        selected: false,
        enabled: true,
        tone: None,
    }
}

impl<'a, Message: Clone + 'a> IconControl<'a, Message> {
    pub fn id(mut self, id: widget::Id) -> Self {
        self.button = self.button.id(id);
        self
    }

    pub fn icon_color(self, color: iced::Color) -> Self {
        self.icon_tone(crate::design::Tone::Fixed(color))
    }

    /// The semantic tone variant: resolved from the drawing theme, so a row
    /// builder never locks the process-global theme to name a color.
    pub fn icon_tone(mut self, tone: crate::design::Tone) -> Self {
        self.tone = Some(tone);
        self.button = self.button.class(control_style(self.selected, self.tone));
        self
    }
}

impl<'a, Message: Clone + 'a> From<IconControl<'a, Message>> for Element<'a, Message> {
    fn from(control: IconControl<'a, Message>) -> Self {
        let IconControl {
            button,
            label,
            enabled,
            ..
        } = control;
        let button: Element<'a, Message> = if enabled {
            crate::interaction::hover(button).into()
        } else {
            button.into()
        };
        tooltip(button, label)
    }
}

/// Shared anchored popup surface. COSMIC owns placement, outside-click dismissal
/// and overlay hit testing; all application popovers share the same frame.
pub fn popover<'a, Message: Clone + 'static>(
    anchor: impl Into<Element<'a, Message>>,
    content: Option<Element<'a, Message>>,
    position: widget::popover::Position,
    on_close: Message,
) -> Element<'a, Message> {
    let mut popup = widget::popover(anchor)
        .position(position)
        .on_close(on_close);
    if let Some(content) = content {
        // iced masks the cursor beneath an overlay only when its surface
        // reports a mouse interaction. Include the frame's padding and inert
        // content, which otherwise report None and expose controls beneath.
        popup = popup.popup(crate::interaction::opaque(
            widget::container(content)
                .padding(6)
                .class(cosmic::theme::Container::custom(|theme| {
                    iced::widget::container::Style {
                        background: Some(iced::Background::Color(
                            theme.current_container().component.base.into(),
                        )),
                        border: iced::Border {
                            color: crate::design::component_rule(theme),
                            width: crate::design::BORDER_WIDTH,
                            radius: theme.cosmic().corner_radii.radius_s.into(),
                        },
                        shadow: iced::Shadow {
                            color: iced::Color::from_rgba(0.0, 0.0, 0.0, 0.24),
                            offset: iced::Vector::new(0.0, 4.0),
                            blur_radius: 16.0,
                        },
                        ..Default::default()
                    }
                })),
        ));
    }
    popup.into()
}

/// A full-width menu action with a stable icon column and a secondary description.
pub fn popover_item<'a, Message: Clone + 'a>(
    icon: impl Into<widget::icon::Handle>,
    title: &'a str,
    description: impl Into<Cow<'a, str>> + 'a,
    on_press: Message,
) -> widget::button::Button<'a, Message> {
    widget::button::custom(
        widget::row::with_children(vec![
            widget::container(widget::icon(icon.into()).size(20))
                .center_x(28)
                .center_y(36)
                .into(),
            widget::column::with_children(vec![
                crate::design::body(title).into(),
                crate::design::caption(description).into(),
            ])
            .spacing(2)
            .width(Length::Fill)
            .into(),
        ])
        .spacing(8)
        .align_y(iced::Alignment::Center),
    )
    .name(title)
    .class(control_style(false, None))
    .padding([6, 8])
    .width(Length::Fill)
    .on_press(on_press)
}

/// Independent toolbar commands, grouped by spacing without a selector surface.
pub fn toolbar_actions<'a, Message: 'a>(
    controls: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    widget::container(
        widget::row::with_children(controls)
            .spacing(0)
            .align_y(iced::Alignment::Center),
    )
    .padding(2)
    .into()
}

/// A connected mode selector using the same compact targets as toolbar actions.
/// Pass `icon_control(...).into()` children; each retains its own focus and tooltip.
pub fn segmented_controls<'a, Message: 'a>(
    controls: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    widget::container(
        widget::row::with_children(controls)
            .spacing(0)
            .align_y(iced::Alignment::Center),
    )
    .padding(2)
    .class(cosmic::theme::Container::custom(|theme| {
        iced::widget::container::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.base.into(),
            )),
            border: iced::Border {
                radius: crate::design::RADIUS_S.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }))
    .into()
}

/// One entry of a [`view_tabs`] bar.
pub struct ViewTab<'a, Message> {
    /// The short visible name.
    pub label: Cow<'a, str>,
    /// The accessible name and tooltip, which say what the label cannot fit.
    pub description: Cow<'a, str>,
    /// A quiet trailing figure, such as how many rows the view holds, and the
    /// text style it draws in, e.g. [`crate::design::muted_text`].
    pub count: Option<(
        Cow<'a, str>,
        fn(&cosmic::Theme) -> iced::widget::text::Style,
    )>,
    pub selected: bool,
    pub id: Option<widget::Id>,
    /// What choosing the tab raises. The selected tab publishes nothing:
    /// choosing the view already on screen is not an intent.
    pub on_press: Option<Message>,
}

/// A pane's single row of text tabs.
///
/// Views of one pane are peers, so they are words on one baseline rather than
/// unlabeled glyphs in a boxed selector: the selected view reads from its
/// filled surface and full-strength label, the others stay quiet until
/// hovered. Nothing else belongs in the row; a view's own options live in the
/// pane's header, beside the identity they act on.
///
/// `compact` tightens each tab's inset for a narrow pane. The row clips
/// rather than spilling past the pane if its words still do not fit.
pub fn view_tabs<'a, Message: Clone + 'a>(
    tabs: Vec<ViewTab<'a, Message>>,
    compact: bool,
) -> Element<'a, Message> {
    let row = widget::row::with_children(
        tabs.into_iter()
            .map(|tab| view_tab(tab, compact, None))
            .collect::<Vec<_>>(),
    )
    .spacing(cosmic::theme::spacing().space_xxxs)
    .align_y(iced::Alignment::Center);
    widget::container(row).width(Length::Fill).clip(true).into()
}

/// A shared view tab, with an optional leading status mark. Compact tabs use
/// caption text and the smaller inline-control height.
pub fn view_tab<'a, Message: Clone + 'a>(
    tab: ViewTab<'a, Message>,
    compact: bool,
    leading: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    let ViewTab {
        label,
        description,
        count,
        selected,
        id,
        on_press,
    } = tab;
    let mut content = widget::row::with_capacity(3)
        .spacing(spacing.space_xxs)
        .align_y(iced::Alignment::Center);
    if let Some(leading) = leading {
        content = content.push(leading);
    }
    let label = if compact {
        crate::design::caption(label)
    } else {
        crate::design::body(label)
    };
    content = content.push(label.wrapping(iced::widget::text::Wrapping::None));
    // The count is drawn, so it is spoken too.
    let name: Cow<'a, str> = match &count {
        Some((count, _)) => format!("{description}: {count}").into(),
        None => description,
    };
    if let Some((count, style)) = count {
        content = content.push(
            crate::design::caption(count)
                .wrapping(iced::widget::text::Wrapping::None)
                .class(cosmic::theme::Text::Custom(style)),
        );
    }
    let mut button = widget::button::custom(widget::container(content).center_y(Length::Fill))
        .class(view_tab_style(selected))
        .name(name.clone())
        .selected(selected)
        .padding([
            0,
            if compact {
                spacing.space_xxs
            } else {
                spacing.space_xs
            },
        ])
        .height(view_tab_height(compact))
        .on_press_maybe(if selected { None } else { on_press });
    if let Some(id) = id {
        button = button.id(id);
    }
    tooltip(crate::interaction::hover(button), name)
}

pub fn view_tab_height(compact: bool) -> f32 {
    if compact {
        crate::design::COMPACT_CONTROL_SIZE
    } else {
        crate::design::DENSE_ROW_HEIGHT
    }
}

/// [`control_style`]'s surface with a label tone: the selected tab keeps its
/// fill and full-strength label even though it publishes nothing, and the
/// rest read as quieter peers rather than as disabled controls.
fn view_tab_style(selected: bool) -> cosmic::theme::Button {
    let appearance = move |focused: bool, theme: &cosmic::Theme| {
        let component = &theme.current_container().component;
        widget::button::Style {
            background: (selected || focused)
                .then(|| iced::Background::Color(component.selected_state_color().into())),
            border_radius: theme.cosmic().corner_radii.radius_s.into(),
            border_width: 0.0,
            text_color: Some(if selected {
                component.on.into()
            } else {
                crate::design::muted_color(theme)
            }),
            ..widget::button::Style::new()
        }
    };
    cosmic::theme::Button::Custom {
        active: Box::new(appearance),
        disabled: Box::new(move |theme| appearance(false, theme)),
        hovered: Box::new(move |focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.hover.into(),
            )),
            text_color: Some(theme.current_container().component.on.into()),
            ..appearance(focused, theme)
        }),
        pressed: Box::new(move |focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.pressed.into(),
            )),
            ..appearance(focused, theme)
        }),
    }
}

/// The shell's flat control surface: no border, no fill until hovered.
///
/// Exposed so a surface that composes its own row - a transcript step, a
/// navigation entry - presses the same treatment as the icon controls beside
/// it instead of reaching for a stock button class.
pub fn control_style(selected: bool, tone: Option<crate::design::Tone>) -> cosmic::theme::Button {
    let appearance = move |focused: bool, theme: &cosmic::Theme| widget::button::Style {
        background: if selected || focused {
            Some(iced::Background::Color(
                theme
                    .current_container()
                    .component
                    .selected_state_color()
                    .into(),
            ))
        } else {
            None
        },
        border_radius: theme.cosmic().corner_radii.radius_s.into(),
        border_width: 0.0,
        border_color: theme.current_container().component.on_disabled.into(),
        icon_color: Some(tone.map_or_else(
            || {
                if selected || focused {
                    theme.current_container().component.on.into()
                } else {
                    crate::design::muted_icon_color(theme)
                }
            },
            |tone| tone.resolve(theme),
        )),
        ..widget::button::Style::new()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(appearance),
        disabled: Box::new(move |theme| widget::button::Style {
            // Symbolic SVG recoloring ignores alpha, so mix against the
            // surface instead of relying on `on_disabled` transparency.
            icon_color: Some(muted_icon_color(theme)),
            background: None,
            ..appearance(false, theme)
        }),
        hovered: Box::new(move |focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.hover.into(),
            )),
            icon_color: Some(tone.map_or_else(
                || theme.current_container().component.on.into(),
                |tone| tone.resolve(theme),
            )),
            ..appearance(focused, theme)
        }),
        pressed: Box::new(move |focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.pressed.into(),
            )),
            ..appearance(focused, theme)
        }),
    }
}

fn muted_icon_color(theme: &cosmic::Theme) -> iced::Color {
    let on: iced::Color = theme.current_container().component.on.into();
    let base: iced::Color = theme.current_container().component.base.into();
    iced::Color {
        r: on.r * 0.38 + base.r * 0.62,
        g: on.g * 0.38 + base.g * 0.62,
        b: on.b * 0.38 + base.b * 0.62,
        a: 1.0,
    }
}

/// The instant every activity spinner measures its rotation from.
///
/// Phase has to come from one value shared by the whole process rather than
/// from each widget's own mount time, so two indicators that appear a second
/// apart still draw the same arc instead of chasing each other.
static SPINNER_EPOCH: std::sync::LazyLock<cosmic::iced::time::Instant> =
    std::sync::LazyLock::new(cosmic::iced::time::Instant::now);

/// The arc the spinner draws `elapsed` after [`SPINNER_EPOCH`], as start and
/// end angles in radians measured from twelve o'clock.
///
/// Deriving the angle from elapsed time rather than accumulating a per-frame
/// delta means a dropped frame skips ahead instead of rewinding, and two
/// spinners mounted at different moments stay in phase. The reduction to a
/// single turn happens in `f64` because a shell left running overnight
/// accumulates enough seconds that `f32` can no longer resolve a frame.
fn spinner_arc(elapsed: std::time::Duration) -> (f32, f32) {
    use std::f32::consts::TAU;

    let period = crate::design::SPINNER_PERIOD.as_secs_f64();
    let turns = (elapsed.as_secs_f64() % period) / period;
    let start = turns as f32 * TAU - std::f32::consts::FRAC_PI_2;
    (start, start + crate::design::SPINNER_ARC_SPAN * TAU)
}

/// A small activity glyph that owns its clock: there is no application
/// subscription and no view rebuild behind its motion.
///
/// `color` is the tone the arc draws in.
pub fn activity_spinner<'a, Message: Clone + 'a>(
    size: f32,
    color: iced::Color,
) -> Element<'a, Message> {
    spinner(size, crate::design::Tone::Fixed(color))
}

/// [`activity_spinner`] in a semantic tone, resolved from the drawing theme.
///
/// Per-row indicators must use this: naming the color eagerly meant
/// `cosmic::theme::active()` - a process-global theme lock plus a clone of the
/// active `Theme` - once per visible row per frame.
pub fn activity_spinner_tone<'a, Message: Clone + 'a>(
    size: f32,
    tone: fn(&cosmic::Theme) -> iced::Color,
) -> Element<'a, Message> {
    spinner(size, crate::design::Tone::Themed(tone))
}

fn spinner<'a, Message: Clone + 'a>(size: f32, tone: crate::design::Tone) -> Element<'a, Message> {
    Element::new(ActivitySpinner {
        size,
        tone,
        handle: spinner_handle(size),
    })
}

fn spinner_handle(size: f32) -> cosmic::iced::advanced::svg::Handle {
    use cosmic::iced::advanced::svg::Handle;
    use std::sync::LazyLock;

    // These are the shell's indicator sizes. Stable handles keep rasterized
    // SVGs shared between rows and frames without a growing geometry cache.
    const SIZES: [f32; 5] = [12.0, 14.0, 16.0, 20.0, 24.0];
    static HANDLES: LazyLock<[Handle; 5]> = LazyLock::new(|| SIZES.map(spinner_svg));
    SIZES
        .iter()
        .position(|candidate| *candidate == size)
        .map_or_else(|| spinner_svg(size), |index| HANDLES[index].clone())
}

fn spinner_svg(size: f32) -> cosmic::iced::advanced::svg::Handle {
    let stroke = (size / 12.0).max(2.0);
    let radius = (size - stroke) / 2.0;
    let center = size / 2.0;
    let right = center + radius;
    let top = center - radius;
    cosmic::iced::advanced::svg::Handle::from_memory(format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}"><path d="M{right} {center}A{radius} {radius} 0 1 1 {center} {top}" fill="none" stroke="black" stroke-width="{stroke}" stroke-linecap="round"/></svg>"#
    ).into_bytes())
}

/// An arc of fixed span turning at a fixed rate.
///
/// COSMIC's own indeterminate indicator grows its arc to nearly a full circle
/// and shrinks it back twice a second on top of the rotation. That pulse reads
/// as a progress report the shell has no way to make, so the shell draws its
/// own indicator instead of restyling the pinned one.
struct ActivitySpinner {
    size: f32,
    tone: crate::design::Tone,
    handle: cosmic::iced::advanced::svg::Handle,
}

#[derive(Default)]
struct SpinnerState {
    /// Time since [`SPINNER_EPOCH`] as of the last frame, kept here because
    /// `draw` is handed no clock of its own.
    elapsed: std::time::Duration,
}

impl<Message> cosmic::iced::advanced::Widget<Message, cosmic::Theme, cosmic::Renderer>
    for ActivitySpinner
{
    fn tag(&self) -> cosmic::iced::advanced::widget::tree::Tag {
        cosmic::iced::advanced::widget::tree::Tag::of::<SpinnerState>()
    }

    fn state(&self) -> cosmic::iced::advanced::widget::tree::State {
        cosmic::iced::advanced::widget::tree::State::new(SpinnerState::default())
    }

    fn size(&self) -> cosmic::iced::Size<Length> {
        cosmic::iced::Size {
            width: Length::Fixed(self.size),
            height: Length::Fixed(self.size),
        }
    }

    fn layout(
        &mut self,
        _tree: &mut cosmic::iced::advanced::widget::Tree,
        _renderer: &cosmic::Renderer,
        limits: &cosmic::iced::advanced::layout::Limits,
    ) -> cosmic::iced::advanced::layout::Node {
        cosmic::iced::advanced::layout::atomic(limits, self.size, self.size)
    }

    fn update(
        &mut self,
        tree: &mut cosmic::iced::advanced::widget::Tree,
        event: &cosmic::iced::Event,
        layout: cosmic::iced::advanced::Layout<'_>,
        _cursor: cosmic::iced::mouse::Cursor,
        _renderer: &cosmic::Renderer,
        _clipboard: &mut dyn cosmic::iced::advanced::Clipboard,
        shell: &mut cosmic::iced::advanced::Shell<'_, Message>,
        viewport: &cosmic::iced::Rectangle,
    ) {
        // A spinner nobody can see must not hold the frame clock. Indicators
        // are mounted in cards sitting in an off-screen workspace column and
        // in strips scrolled past their own viewport, and asking for the next
        // frame from each of them pins the whole window to the compositor's
        // cadence on behalf of nothing - worst exactly when many agents are
        // working at once, which is when the shell can least afford it.
        if !layout.bounds().intersects(viewport) {
            return;
        }
        if let cosmic::iced::Event::Window(cosmic::iced::window::Event::RedrawRequested(now)) =
            event
        {
            let state = tree.state.downcast_mut::<SpinnerState>();
            state.elapsed = now.saturating_duration_since(*SPINNER_EPOCH);
            shell.request_redraw();
        }
    }

    fn draw(
        &self,
        tree: &cosmic::iced::advanced::widget::Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        _style: &cosmic::iced::advanced::renderer::Style,
        layout: cosmic::iced::advanced::Layout<'_>,
        _cursor: cosmic::iced::mouse::Cursor,
        viewport: &cosmic::iced::Rectangle,
    ) {
        use cosmic::iced::advanced::Renderer as _;
        use cosmic::iced::advanced::svg::{Renderer as _, Svg};

        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        if bounds.width < 1.0 || bounds.height < 1.0 {
            return;
        }
        let state = tree.state.downcast_ref::<SpinnerState>();
        let (start, _) = spinner_arc(state.elapsed);
        let color = self.tone.resolve(theme);
        // Rotate a stable SVG through the image pipeline. Dynamic canvas
        // meshes in the pinned WGPU renderer retained arcs after their panes
        // unmounted; sharing one icon per size also avoids retessellation on
        // every animation frame. Keep the original phase, sweep and stroke.
        renderer.with_layer(clip, |renderer| {
            renderer.draw_svg(
                Svg::new(self.handle.clone())
                    .color(cosmic::iced::Color { a: 1.0, ..color })
                    .opacity(color.a)
                    .rotation(cosmic::iced::Radians(start)),
                bounds,
                clip,
            );
        });
    }
}

/// The sweep a radial gauge fills for `fraction`, as start and end angles in
/// radians measured from twelve o'clock, or `None` when nothing is used.
///
/// A fraction outside the unit range is clamped rather than wrapped: a
/// provider reporting more context than its own window says the reader is
/// full, not that the ring should start a second lap. A non-finite figure
/// fills nothing, because a ring is a claim about a proportion and there is
/// none to make.
fn radial_sweep(fraction: f32) -> Option<(f32, f32)> {
    use std::f32::consts::{FRAC_PI_2, TAU};

    let filled = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    (filled > 0.0).then(|| (-FRAC_PI_2, -FRAC_PI_2 + filled * TAU))
}

/// A ring whose filled sweep is the fraction of a budget in use.
///
/// The sweep uses the same muted neutral ink as inactive navigation icons,
/// so a healthy status line is not a row of competing colours; a caller whose
/// budget is in trouble tones it with [`budget_radial_toned`].
///
/// The linear capsule this replaces cost a status line the width of a whole
/// caption and, at the height a caption row allows, read as a crack in the
/// layout. A ring carries the same proportion in the space of a glyph, which
/// is what lets the budget stay pinned to the pane's right edge however
/// narrow the pane gets.
pub fn budget_radial<'a, Message: 'a>(fraction: f32) -> Element<'a, Message> {
    budget_radial_toned(fraction, None)
}

/// A [`budget_radial`] whose sweep takes `tone` instead of the neutral ink,
/// for a gauge whose budget is in trouble and should say so on its own.
pub fn budget_radial_toned<'a, Message: 'a>(
    fraction: f32,
    tone: Option<crate::design::Tone>,
) -> Element<'a, Message> {
    Element::new(BudgetRadial { fraction, tone })
}

struct BudgetRadial {
    fraction: f32,
    tone: Option<crate::design::Tone>,
}

impl<Message> cosmic::iced::advanced::Widget<Message, cosmic::Theme, cosmic::Renderer>
    for BudgetRadial
{
    fn size(&self) -> cosmic::iced::Size<Length> {
        cosmic::iced::Size {
            width: Length::Fixed(crate::design::RADIAL_GAUGE_SIZE),
            height: Length::Fixed(crate::design::RADIAL_GAUGE_SIZE),
        }
    }

    fn layout(
        &mut self,
        _tree: &mut cosmic::iced::advanced::widget::Tree,
        _renderer: &cosmic::Renderer,
        limits: &cosmic::iced::advanced::layout::Limits,
    ) -> cosmic::iced::advanced::layout::Node {
        cosmic::iced::advanced::layout::atomic(
            limits,
            crate::design::RADIAL_GAUGE_SIZE,
            crate::design::RADIAL_GAUGE_SIZE,
        )
    }

    fn draw(
        &self,
        _tree: &cosmic::iced::advanced::widget::Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        _style: &cosmic::iced::advanced::renderer::Style,
        layout: cosmic::iced::advanced::Layout<'_>,
        _cursor: cosmic::iced::mouse::Cursor,
        _viewport: &cosmic::iced::Rectangle,
    ) {
        use cosmic::iced::advanced::Renderer as _;
        use cosmic::iced::advanced::graphics::geometry::Renderer as _;
        use cosmic::widget::canvas;

        let bounds = layout.bounds();
        let radius = (bounds.width.min(bounds.height) - crate::design::RADIAL_GAUGE_STROKE) / 2.0;
        if radius <= 0.0 {
            return;
        }
        let stroke = canvas::Stroke::default().with_width(crate::design::RADIAL_GAUGE_STROKE);
        let mut frame = canvas::Frame::new(&*renderer, bounds.size());
        // The unfilled ring is drawn whatever the occupancy, because a gauge
        // has to read as a whole for its filled part to mean anything.
        let mut track = canvas::path::Builder::new();
        track.circle(frame.center(), radius);
        frame.stroke(
            &track.build(),
            stroke.with_color(crate::design::track(theme)),
        );
        if let Some((start, end)) = radial_sweep(self.fraction) {
            let mut used = canvas::path::Builder::new();
            used.arc(canvas::path::Arc {
                center: frame.center(),
                radius,
                start_angle: cosmic::iced::Radians(start),
                end_angle: cosmic::iced::Radians(end),
            });
            frame.stroke(
                &used.build(),
                stroke.with_color(self.tone.map_or_else(
                    || crate::design::muted_icon_color(theme),
                    |tone| tone.resolve(theme),
                )),
            );
        }
        let geometry = frame.into_geometry();

        renderer.with_translation(cosmic::iced::Vector::new(bounds.x, bounds.y), |renderer| {
            renderer.draw_geometry(geometry);
        });
    }
}

/// The filled width of a linear gauge `width` wide at `fraction`, clamped the
/// same way as [`radial_sweep`]: past full is full, and a non-finite figure
/// fills nothing.
fn bar_fill(width: f32, fraction: f32) -> f32 {
    let filled = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    width * filled
}

/// A thin horizontal gauge the width of its row, for a list of budgets read
/// against each other.
///
/// The ring is right where a budget sits beside a caption; where several
/// windows are listed one under another, bars of one length let the eye
/// compare them down the column, which rings of one size cannot.
/// The highlighted fraction is the allowance still available.
pub fn budget_bar<'a, Message: 'a>(
    fraction: f32,
    tone: crate::design::Tone,
) -> Element<'a, Message> {
    Element::new(BudgetBar { fraction, tone })
}

struct BudgetBar {
    fraction: f32,
    tone: crate::design::Tone,
}

impl<Message> cosmic::iced::advanced::Widget<Message, cosmic::Theme, cosmic::Renderer>
    for BudgetBar
{
    fn size(&self) -> cosmic::iced::Size<Length> {
        cosmic::iced::Size {
            width: Length::Fill,
            height: Length::Fixed(crate::design::BUDGET_BAR_HEIGHT),
        }
    }

    fn layout(
        &mut self,
        _tree: &mut cosmic::iced::advanced::widget::Tree,
        _renderer: &cosmic::Renderer,
        limits: &cosmic::iced::advanced::layout::Limits,
    ) -> cosmic::iced::advanced::layout::Node {
        cosmic::iced::advanced::layout::atomic(
            limits,
            Length::Fill,
            crate::design::BUDGET_BAR_HEIGHT,
        )
    }

    fn draw(
        &self,
        _tree: &cosmic::iced::advanced::widget::Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        _style: &cosmic::iced::advanced::renderer::Style,
        layout: cosmic::iced::advanced::Layout<'_>,
        _cursor: cosmic::iced::mouse::Cursor,
        _viewport: &cosmic::iced::Rectangle,
    ) {
        use cosmic::iced::advanced::Renderer as _;

        let bounds = layout.bounds();
        let radius = bounds.height / 2.0;
        renderer.fill_quad(
            cosmic::iced::advanced::renderer::Quad {
                bounds,
                border: iced::Border {
                    radius: radius.into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            crate::design::track(theme),
        );
        let filled = bar_fill(bounds.width, self.fraction);
        if filled > 0.0 {
            renderer.fill_quad(
                cosmic::iced::advanced::renderer::Quad {
                    bounds: cosmic::iced::Rectangle {
                        // Never narrower than the bar is tall, so a sliver of
                        // remaining allowance reads as a rounded mark rather than a seam.
                        width: filled.max(bounds.height).min(bounds.width),
                        ..bounds
                    },
                    border: iced::Border {
                        radius: radius.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                self.tone.resolve(theme),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;
    use std::time::Duration;

    #[test]
    fn compact_status_tabs_select_only_an_inactive_view() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Point, Rectangle, Size};

        let renderer = headless_renderer();
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(400.0, 80.0));
        for selected in [false, true] {
            let mut tab = view_tab(
                ViewTab {
                    label: "Worker".into(),
                    description: "Worker · Running".into(),
                    count: None,
                    selected,
                    id: None,
                    on_press: Some(()),
                },
                true,
                Some(activity_spinner_tone(12.0, crate::design::accent_color)),
            );
            let mut tree = Tree::new(tab.as_widget());
            let node = tab.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            assert_eq!(node.size().height, crate::design::COMPACT_CONTROL_SIZE);
            let cursor = mouse::Cursor::Available(node.bounds().center());
            let mut messages = Vec::new();
            for event in [
                mouse::Event::ButtonPressed(mouse::Button::Left),
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ] {
                tab.as_widget_mut().update(
                    &mut tree,
                    &Event::Mouse(event),
                    Layout::new(&node),
                    cursor,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
            }
            assert_eq!(messages.len(), usize::from(!selected));
        }
    }

    #[test]
    fn queued_overlay_clicks_use_the_position_before_later_motion() {
        use cosmic::iced::advanced::clipboard;
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{Event, Point, Size, mouse};

        #[derive(Clone, Debug, PartialEq)]
        enum Action {
            Press,
            Close,
        }

        let mut renderer = headless_renderer();
        let inside = Point::new(60.0, 60.0);
        let outside = Point::new(180.0, 160.0);
        for (start, finish, expected_press) in [(inside, outside, false), (outside, inside, true)] {
            let base = iced::widget::mouse_area(iced::widget::Space::new().width(200).height(200))
                .on_press(Action::Press);
            let element = popover(
                base,
                Some(iced::widget::Space::new().width(100).height(60).into()),
                widget::popover::Position::Point(Point::new(40.0, 40.0)),
                Action::Close,
            );
            let mut ui = UserInterface::build(
                element,
                Size::new(200.0, 200.0),
                Cache::new(),
                &mut renderer,
            );
            let mut messages = Vec::new();
            let _ = ui.update(
                &[
                    Event::Mouse(mouse::Event::CursorMoved { position: start }),
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    Event::Mouse(mouse::Event::CursorMoved { position: finish }),
                ],
                mouse::Cursor::Available(finish),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(
                messages.contains(&Action::Press),
                expected_press,
                "{start:?} to {finish:?}"
            );
        }
    }

    #[test]
    fn popover_inert_surface_blocks_underlying_hover_and_clicks() {
        use cosmic::iced::advanced::clipboard;
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{Event, Point, Size, mouse};

        #[derive(Clone, Debug, PartialEq)]
        enum Action {
            Hover,
            Press,
            Close,
        }

        let mut renderer = headless_renderer();
        // The popup covers x=40..152, y=40..112. Probe its padding,
        // disabled control and empty area, then the exposed background.
        for (point, covered) in [
            (Point::new(42.0, 42.0), true),
            (Point::new(60.0, 55.0), true),
            (Point::new(60.0, 95.0), true),
            (Point::new(180.0, 160.0), false),
        ] {
            let base = iced::widget::mouse_area(iced::widget::Space::new().width(200).height(200))
                .on_enter(Action::Hover)
                .on_press(Action::Press);
            let content = widget::column::with_children(vec![
                widget::button::text("Disabled")
                    .width(100)
                    .height(30)
                    .into(),
                iced::widget::Space::new().width(100).height(30).into(),
            ]);
            let element = popover(
                base,
                Some(content.into()),
                widget::popover::Position::Point(Point::new(40.0, 40.0)),
                Action::Close,
            );
            let mut ui = UserInterface::build(
                element,
                Size::new(200.0, 200.0),
                Cache::new(),
                &mut renderer,
            );
            let mut messages = Vec::new();
            let _ = ui.update(
                &[
                    Event::Mouse(mouse::Event::CursorMoved { position: point }),
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                ],
                mouse::Cursor::Available(point),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(messages.contains(&Action::Hover), !covered, "{point:?}");
            assert_eq!(messages.contains(&Action::Press), !covered, "{point:?}");
            assert!(!messages.contains(&Action::Close));
        }
    }

    #[test]
    fn popover_dismisses_when_clicking_outside_its_anchor_and_surface() {
        use cosmic::iced::advanced::clipboard;
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{Event, Point, Size, mouse};

        let mut renderer = headless_renderer();
        let element = popover(
            iced::widget::Space::new().width(20).height(20),
            Some(iced::widget::Space::new().width(100).height(60).into()),
            widget::popover::Position::Bottom,
            (),
        );
        let mut ui = UserInterface::build(
            element,
            Size::new(200.0, 200.0),
            Cache::new(),
            &mut renderer,
        );
        let mut messages = Vec::new();
        let _ = ui.update(
            &[Event::Mouse(mouse::Event::ButtonPressed(
                mouse::Button::Left,
            ))],
            mouse::Cursor::Available(Point::new(180.0, 160.0)),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(messages, vec![()]);
    }

    /// The defect the spinner replaced was an arc that grew and shrank while
    /// it turned, so both halves of "constant span, constant rate" are worth
    /// holding onto.
    #[test]
    fn the_arc_keeps_its_span_and_turns_at_a_constant_rate() {
        let period = crate::design::SPINNER_PERIOD;
        let span = crate::design::SPINNER_ARC_SPAN * TAU;
        let step = period / 8;

        let mut previous: Option<f32> = None;
        for tick in 0..8 {
            let (start, end) = spinner_arc(step * tick);
            assert!(
                (end - start - span).abs() < 1e-4,
                "the arc spans {} rad at tick {tick}, not {span}",
                end - start
            );
            if let Some(previous) = previous {
                assert!(
                    (start - previous - span_per(step, period)).abs() < 1e-4,
                    "the arc advanced {} rad over one step at tick {tick}",
                    start - previous
                );
            }
            previous = Some(start);
        }
    }

    fn span_per(step: Duration, period: Duration) -> f32 {
        step.as_secs_f32() / period.as_secs_f32() * TAU
    }

    /// A spinner that has been turning all day has to draw the same arc as
    /// one mounted a moment ago, which is what keeps several of them in phase
    /// and what stops the angle losing resolution as the shell stays open.
    #[test]
    fn the_angle_depends_only_on_time_within_the_period() {
        let period = crate::design::SPINNER_PERIOD;
        for offset in [Duration::ZERO, period / 3, period / 2] {
            let (fresh, _) = spinner_arc(offset);
            let (aged, _) = spinner_arc(offset + period * 72_000);
            assert!(
                (fresh - aged).abs() < 1e-4,
                "a whole number of turns later the arc starts at {aged} rather than {fresh}"
            );
        }
    }

    /// A spinner mounted in an off-screen card or scrolled past the end of a
    /// strip must not keep asking for the next frame: several of those at
    /// once pin the whole window to the compositor's cadence for motion
    /// nobody can see.
    #[test]
    fn an_offscreen_spinner_stops_asking_for_frames() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Point, Rectangle, Size, window};

        let renderer = headless_renderer();
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(400.0, 300.0));
        // Half a period after the shared epoch, so a spinner that does read
        // the clock lands on a distinctly non-zero angle.
        let redraw = Event::Window(window::Event::RedrawRequested(
            *SPINNER_EPOCH + crate::design::SPINNER_PERIOD / 2,
        ));

        let requested = |origin: Point| {
            let mut element: Element<'_, ()> = spinner(
                24.0,
                crate::design::Tone::Themed(crate::design::accent_color),
            );
            let mut tree = Tree::new(element.as_widget());
            let node = layout::Node::new(Size::new(24.0, 24.0)).move_to(origin);
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &redraw,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut shell,
                &viewport,
            );
            (
                shell.redraw_request(),
                tree.state.downcast_ref::<SpinnerState>().elapsed,
            )
        };

        let (visible, advanced) = requested(Point::new(8.0, 8.0));
        assert_eq!(visible, window::RedrawRequest::NextFrame);
        assert_ne!(advanced, Duration::ZERO, "the visible arc did not advance");

        let (hidden, frozen) = requested(Point::new(8.0, 900.0));
        assert_eq!(hidden, window::RedrawRequest::Wait);
        assert_eq!(frozen, Duration::ZERO, "the hidden arc advanced anyway");
    }

    #[test]
    fn spinner_draws_only_inside_its_current_viewport_after_navigation() {
        use cosmic::iced::advanced::renderer::Headless as _;
        use cosmic::iced::advanced::{
            Layout, Renderer as _, layout, mouse, renderer, widget::Tree,
        };
        use cosmic::iced::{Color, Point, Rectangle, Size};

        let mut renderer = headless_renderer();
        let theme = cosmic::Theme::default();
        let extent = Rectangle::with_size(Size::new(64.0, 64.0));
        let element: Element<'_, ()> = spinner(24.0, crate::design::Tone::Fixed(Color::WHITE));
        let mut tree = Tree::new(element.as_widget());
        let node = layout::Node::new(Size::new(24.0, 24.0)).move_to(Point::new(8.0, 8.0));
        let size = Size::new(64, 64);
        let mut draw = |viewport: Rectangle, elapsed: Duration| {
            tree.state.downcast_mut::<SpinnerState>().elapsed = elapsed;
            renderer.reset(extent);
            element.as_widget().draw(
                &tree,
                &mut renderer,
                &theme,
                &renderer::Style::default(),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &viewport,
            );
            renderer.screenshot(size, 1.0, Color::BLACK)
        };
        let painted = |pixels: &[u8]| {
            pixels
                .chunks_exact(4)
                .filter(|pixel| pixel[..3] != [0, 0, 0])
                .count()
        };
        let initial = draw(extent, Duration::ZERO);
        assert!(painted(&initial) > 0, "the visible spinner must paint");
        assert_ne!(
            initial,
            draw(extent, crate::design::SPINNER_PERIOD / 2),
            "the visible spinner must rotate between frames"
        );

        // The old pane is still mounted at its previous on-screen position,
        // but its viewport no longer includes it after navigation.
        let hidden = Rectangle::new(Point::new(40.0, 0.0), Size::new(24.0, 64.0));
        assert_eq!(
            painted(&draw(hidden, Duration::ZERO)),
            0,
            "a hidden pane leaked a frozen arc"
        );
        let clipped = Rectangle::new(Point::new(20.0, 0.0), Size::new(44.0, 64.0));
        let pixels = draw(clipped, Duration::ZERO);
        assert!(painted(&pixels) > 0, "the visible portion must still paint");
        for y in 0..64 {
            for x in 0..20 {
                let index = (y * 64 + x) * 4;
                assert_eq!(
                    &pixels[index..index + 3],
                    &[0, 0, 0],
                    "arc escaped its pane at ({x}, {y})"
                );
            }
        }
    }

    /// The ring is the only thing a reader looks at before deciding to
    /// compact, so a full budget has to look full and an empty one has to
    /// draw no sweep at all rather than a stray dot at twelve o'clock.
    #[test]
    fn the_bar_fill_is_the_clamped_fraction_of_its_width() {
        assert_eq!(bar_fill(200.0, 0.25), 50.0);
        assert_eq!(bar_fill(200.0, 1.7), 200.0);
        for nothing in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(bar_fill(200.0, nothing), 0.0, "{nothing} filled the bar");
        }
    }

    #[test]
    fn the_radial_sweep_is_the_fraction_of_the_budget_in_use() {
        let quarter = radial_sweep(0.25).expect("a quarter of a budget is a sweep");
        assert!((quarter.1 - quarter.0 - TAU / 4.0).abs() < 1e-4);
        let full = radial_sweep(1.0).expect("a full budget is a whole ring");
        assert!((full.1 - full.0 - TAU).abs() < 1e-4);
        // A provider reporting past its own window still means "full".
        let over = radial_sweep(2.5).expect("an overrun budget is a whole ring");
        assert_eq!(over, full);
        for nothing in [0.0, -1.0, f32::NAN] {
            assert_eq!(
                radial_sweep(nothing),
                None,
                "{nothing} drew a sweep of its own"
            );
        }
    }

    fn headless_renderer() -> cosmic::Renderer {
        use cosmic::iced::advanced::renderer::Headless;

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(<cosmic::Renderer as Headless>::new(
                cosmic::iced::Font::DEFAULT,
                cosmic::iced::Pixels::from(16),
                Some("tiny-skia"),
            ))
            .expect("create a headless renderer")
    }
}
