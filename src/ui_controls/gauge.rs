//! Indicators the shell draws itself: the activity spinner and budget gauges.

use cosmic::{
    Element,
    iced::{self, Length},
};

/// The instant every activity spinner measures its rotation from.
///
/// Phase has to come from one value shared by the whole process rather than
/// from each widget's own mount time, so two indicators that appear a second
/// apart still draw the same arc instead of chasing each other.
static SPINNER_EPOCH: std::sync::LazyLock<cosmic::iced::time::Instant> =
    std::sync::LazyLock::new(cosmic::iced::time::Instant::now);

/// How often a visible spinner asks for a frame.
///
/// Every frame repaints the whole window - update, layout and a full render
/// pass - so this is the price of every working agent on screen, paid for as
/// long as it works. Ten steps a second, thirty degrees of a
/// [`crate::design::SPINNER_PERIOD`] turn each, still reads as a turning arc
/// at indicator size, at a third of the cost of the thirty steps it replaced.
const SPINNER_FRAME: std::time::Duration = std::time::Duration::from_millis(100);

// The cadence is the whole cost of a working agent; it must not creep back
// toward display refresh.
const _: () = assert!(SPINNER_FRAME.as_millis() >= 100);

std::thread_local! {
    /// Whether spinners may ask for frames at all: true only while the
    /// window they are drawn in is focused.
    ///
    /// This is the window's state rather than any one spinner's, because a
    /// spinner mounted while the window is in the background has never seen
    /// the focus event that put it there. It is thread-local because the
    /// window's widgets and the application that reports its focus share one
    /// UI thread, which keeps tests on their own threads from seeing each
    /// other's windows.
    static ANIMATE_ACTIVITY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Tell every activity spinner whether its window is focused.
///
/// The application calls this for its main window's focus changes; spinners
/// are drawn only there. While unfocused, spinners hold a resting arc and ask
/// for no frames, so a background window with agents working costs nothing
/// to keep up to date. A window that is minimized, on another workspace or
/// behind a lock screen has lost focus too. Regaining focus resumes them on
/// the redraw that follows the application's update.
pub fn animate_activity(focused: bool) {
    ANIMATE_ACTIVITY.set(focused);
}

#[cfg(test)]
pub(crate) fn activity_animates() -> bool {
    ANIMATE_ACTIVITY.get()
}

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
/// `tone` names the arc's color and is resolved from the drawing theme, so a
/// per-row indicator never locks the process-global theme to learn it.
pub fn activity_spinner<'a, Message: Clone + 'a>(
    size: f32,
    tone: fn(&cosmic::Theme) -> iced::Color,
) -> Element<'a, Message> {
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
    tone: fn(&cosmic::Theme) -> iced::Color,
    handle: cosmic::iced::advanced::svg::Handle,
}

#[derive(Default)]
struct SpinnerState {
    /// Time since [`SPINNER_EPOCH`] as of the last frame, kept here because
    /// `draw` is handed no clock of its own. Zero is the resting arc an
    /// unfocused window shows.
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
            // An unfocused window still redraws when its content changes,
            // and a spinner that advanced on those redraws would twitch at
            // whatever rate an agent happens to stream. It rests instead,
            // the same arc for every spinner however long it was mounted.
            if !ANIMATE_ACTIVITY.get() {
                state.elapsed = std::time::Duration::ZERO;
                return;
            }
            state.elapsed = now.saturating_duration_since(*SPINNER_EPOCH);
            // A redraw repaints the whole window, so the arc is stepped at a
            // fixed cadence instead of holding the window at display refresh
            // for as long as any agent is working.
            shell.request_redraw_at(*now + SPINNER_FRAME);
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
        let color = (self.tone)(theme);
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

/// The ring's tessellated geometry and what it was built from.
///
/// A status line redraws with the rest of the window many times a second
/// while its budget changes a few times a minute, so the two strokes are
/// rebuilt only when the figure or a color actually moves.
#[derive(Default)]
struct RadialState {
    geometry: cosmic::widget::canvas::Cache,
    drawn: std::cell::Cell<Option<RadialKey>>,
}

#[derive(Clone, Copy, PartialEq)]
struct RadialKey {
    sweep: Option<(f32, f32)>,
    track: iced::Color,
    used: iced::Color,
    size: iced::Size,
}

impl<Message> cosmic::iced::advanced::Widget<Message, cosmic::Theme, cosmic::Renderer>
    for BudgetRadial
{
    fn tag(&self) -> cosmic::iced::advanced::widget::tree::Tag {
        cosmic::iced::advanced::widget::tree::Tag::of::<RadialState>()
    }

    fn state(&self) -> cosmic::iced::advanced::widget::tree::State {
        cosmic::iced::advanced::widget::tree::State::new(RadialState::default())
    }

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
        tree: &cosmic::iced::advanced::widget::Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        _style: &cosmic::iced::advanced::renderer::Style,
        layout: cosmic::iced::advanced::Layout<'_>,
        _cursor: cosmic::iced::mouse::Cursor,
        viewport: &cosmic::iced::Rectangle,
    ) {
        use cosmic::iced::advanced::Renderer as _;
        use cosmic::iced::advanced::graphics::geometry::Renderer as _;
        use cosmic::widget::canvas;

        let bounds = layout.bounds();
        let radius = (bounds.width.min(bounds.height) - crate::design::RADIAL_GAUGE_STROKE) / 2.0;
        if radius <= 0.0 || !bounds.intersects(viewport) {
            return;
        }
        let key = RadialKey {
            sweep: radial_sweep(self.fraction),
            track: crate::design::track(theme),
            used: self.tone.map_or_else(
                || crate::design::muted_icon_color(theme),
                |tone| tone.resolve(theme),
            ),
            size: bounds.size(),
        };
        let state = tree.state.downcast_ref::<RadialState>();
        if state.drawn.replace(Some(key)) != Some(key) {
            state.geometry.clear();
        }
        let geometry = state.geometry.draw(renderer, key.size, |frame| {
            let stroke = canvas::Stroke::default().with_width(crate::design::RADIAL_GAUGE_STROKE);
            // The unfilled ring is drawn whatever the occupancy, because a
            // gauge has to read as a whole for its filled part to mean
            // anything.
            let mut track = canvas::path::Builder::new();
            track.circle(frame.center(), radius);
            frame.stroke(&track.build(), stroke.with_color(key.track));
            if let Some((start, end)) = key.sweep {
                let mut used = canvas::path::Builder::new();
                used.arc(canvas::path::Arc {
                    center: frame.center(),
                    radius,
                    start_angle: cosmic::iced::Radians(start),
                    end_angle: cosmic::iced::Radians(end),
                });
                frame.stroke(&used.build(), stroke.with_color(key.used));
            }
        });

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
    use crate::ui_controls::accessibility_probe::headless_renderer;
    use std::f32::consts::TAU;
    use std::time::Duration;

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

    /// Half a period after the shared epoch, so a spinner that does read the
    /// clock lands on a distinctly non-zero angle.
    fn half_turn() -> cosmic::iced::time::Instant {
        *SPINNER_EPOCH + crate::design::SPINNER_PERIOD / 2
    }

    /// Delivers the window's redraw at `now` to a spinner laid out at
    /// `origin` in a 400 x 300 window, returning the frame it asks for next
    /// and the arc it will draw.
    fn redraw(
        element: &mut Element<'_, ()>,
        tree: &mut cosmic::iced::advanced::widget::Tree,
        origin: cosmic::iced::Point,
        now: cosmic::iced::time::Instant,
    ) -> (cosmic::iced::window::RedrawRequest, Duration) {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse};
        use cosmic::iced::{Event, Point, Rectangle, Size, window};

        let renderer = headless_renderer();
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(400.0, 300.0));
        let node = layout::Node::new(Size::new(24.0, 24.0)).move_to(origin);
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            tree,
            &Event::Window(window::Event::RedrawRequested(now)),
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
    }

    fn mounted() -> (Element<'static, ()>, cosmic::iced::advanced::widget::Tree) {
        let element: Element<'_, ()> = activity_spinner(24.0, crate::design::accent_color);
        let tree = cosmic::iced::advanced::widget::Tree::new(element.as_widget());
        (element, tree)
    }

    /// A spinner mounted in an off-screen card or scrolled past the end of a
    /// strip must not keep asking for the next frame: several of those at
    /// once pin the whole window to the compositor's cadence for motion
    /// nobody can see.
    #[test]
    fn an_offscreen_spinner_stops_asking_for_frames() {
        use cosmic::iced::{Point, window};

        animate_activity(true);
        let (mut element, mut tree) = mounted();
        let (visible, advanced) = redraw(&mut element, &mut tree, Point::new(8.0, 8.0), half_turn());
        // Stepped at the spinner's own cadence, not at display refresh.
        assert_eq!(visible, window::RedrawRequest::At(half_turn() + SPINNER_FRAME));
        assert_ne!(advanced, Duration::ZERO, "the visible arc did not advance");

        let (mut element, mut tree) = mounted();
        let (hidden, frozen) = redraw(&mut element, &mut tree, Point::new(8.0, 900.0), half_turn());
        assert_eq!(hidden, window::RedrawRequest::Wait);
        assert_eq!(frozen, Duration::ZERO, "the hidden arc advanced anyway");
    }

    /// A window in the background with agents working must cost nothing:
    /// its spinners stop asking for frames and rest, whether they were
    /// turning when focus left or were mounted after it did, and turn again
    /// on the first redraw once the window has focus back.
    #[test]
    fn an_unfocused_window_rests_its_spinners_until_focus_returns() {
        use cosmic::iced::{Point, window};

        let origin = Point::new(8.0, 8.0);
        animate_activity(true);
        let (mut turning, mut turning_tree) = mounted();
        let (request, _) = redraw(&mut turning, &mut turning_tree, origin, half_turn());
        assert_eq!(request, window::RedrawRequest::At(half_turn() + SPINNER_FRAME));

        animate_activity(false);
        // The frame the spinner had already asked for still arrives.
        let later = half_turn() + SPINNER_FRAME;
        assert_eq!(
            redraw(&mut turning, &mut turning_tree, origin, later),
            (window::RedrawRequest::Wait, Duration::ZERO),
            "a spinner kept turning in an unfocused window"
        );
        let (mut fresh, mut fresh_tree) = mounted();
        assert_eq!(
            redraw(&mut fresh, &mut fresh_tree, origin, later),
            (window::RedrawRequest::Wait, Duration::ZERO),
            "a spinner mounted in an unfocused window started turning"
        );

        animate_activity(true);
        for (element, tree) in [(&mut turning, &mut turning_tree), (&mut fresh, &mut fresh_tree)] {
            let (request, elapsed) = redraw(element, tree, origin, later);
            assert_eq!(request, window::RedrawRequest::At(later + SPINNER_FRAME));
            assert_ne!(elapsed, Duration::ZERO, "the arc did not resume");
        }
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
        let element: Element<'_, ()> = activity_spinner(24.0, |_| Color::WHITE);
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
}
