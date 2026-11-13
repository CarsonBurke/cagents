//! Opt-in development trace of the UI thread's work, for finding stalls.
//!
//! With `CAGENTS_UI_TRACE` set in a development build, the UI logs a summary
//! every five seconds: how long its thread spent in `update`, by message, and
//! in `view`, `subscription`, and the window content's layout, draw, event
//! handling, tree diff, operations, accessibility tree and overlay
//! collection. A [`region`] of the view breaks its share of those out beneath
//! the totals. Each window also gives the work and pacing of the frames the
//! shell presented, from the vendored runtime's frame hook. It also logs each
//! call over [`SLOW`] as it returns. Times are
//! wall-clock time on the UI thread. A nested `update` counts towards the
//! outermost one. Overlays lay out and draw outside the root, as do the
//! libcosmic chrome and the renderer, so they are not counted. Without the
//! variable, or in other builds, a scope checks one flag and records nothing.

use cosmic::iced::advanced::widget::{Id, Operation, Tree};
use cosmic::iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use cosmic::iced::{Event, Length, Rectangle, Size, Vector};
use cosmic::{Element, Renderer, Theme};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt::{self, Debug, Write as _};
use std::time::{Duration, Instant};

/// A single call this long is logged as it returns.
const SLOW: Duration = Duration::from_millis(50);
const WINDOW: Duration = Duration::from_secs(5);
/// How much of a message's `Debug` output is formatted to name it.
const LABEL_BYTES: usize = 96;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Phase {
    Update,
    View,
    Subscription,
    Layout,
    Draw,
    Event,
    /// Reconciling the new view with the retained widget tree.
    Diff,
    /// Widget operations, such as focus and scrolling, run over the tree.
    Operate,
    /// Building the accessibility tree.
    A11y,
    /// Collecting the overlays, such as menus, that widgets have open.
    Overlay,
}

pub fn enabled() -> bool {
    #[cfg(feature = "development")]
    {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ENABLED.get_or_init(|| {
            let enabled = std::env::var_os("CAGENTS_UI_TRACE").is_some();
            if enabled {
                cosmic::iced::runtime::frame::observe(frame);
            }
            enabled
        })
    }
    #[cfg(not(feature = "development"))]
    {
        false
    }
}

/// Times one call until it is dropped.
#[must_use]
pub struct Scope(Option<Entry>);

struct Entry {
    phase: Phase,
    /// Names the call beneath its phase's total, unless it is empty.
    label: Box<str>,
    /// Whether the call adds to its phase's total. A region's layout, draw
    /// and event handling are shares of the root's, so they don't.
    total: bool,
    started: Instant,
}

pub fn scope(phase: Phase) -> Scope {
    Scope(enabled().then(|| Entry::new(phase, Box::from(""), true)))
}

impl Entry {
    fn new(phase: Phase, label: Box<str>, total: bool) -> Self {
        Self {
            phase,
            label,
            total,
            started: Instant::now(),
        }
    }
}

/// Times one `update`, named by its message's variants, such as
/// `ChatUi(Stream`. A nested update is part of the outermost one.
pub fn update(message: &dyn Debug) -> Scope {
    if !enabled() {
        return Scope(None);
    }
    let depth = UPDATE_DEPTH.get();
    UPDATE_DEPTH.set(depth + 1);
    if depth > 0 {
        return Scope(None);
    }
    Scope(Some(Entry::new(Phase::Update, label(message), true)))
}

impl Drop for Scope {
    fn drop(&mut self) {
        if !enabled() {
            return;
        }
        match self.0.take() {
            Some(entry) => {
                if entry.phase == Phase::Update {
                    UPDATE_DEPTH.set(UPDATE_DEPTH.get().saturating_sub(1));
                }
                let elapsed = entry.started.elapsed();
                RECORDER.with_borrow_mut(|recorder| recorder.record(entry, elapsed));
            }
            // A nested update's scope still unwinds its depth.
            None if UPDATE_DEPTH.get() > 1 => UPDATE_DEPTH.set(UPDATE_DEPTH.get() - 1),
            None => {}
        }
    }
}

thread_local! {
    static UPDATE_DEPTH: Cell<usize> = const { Cell::new(0) };
    static RECORDER: RefCell<Recorder> = RefCell::new(Recorder::new());
}

#[derive(Default)]
struct Stat {
    count: u64,
    total: Duration,
    max: Duration,
}

impl Stat {
    fn add(&mut self, elapsed: Duration) {
        self.count += 1;
        self.total += elapsed;
        self.max = self.max.max(elapsed);
    }
}

impl fmt::Display for Stat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:.1} ms n={} max {:.1} ms",
            self.total.as_secs_f64() * 1e3,
            self.count,
            self.max.as_secs_f64() * 1e3
        )
    }
}

struct Recorder {
    started: Instant,
    phases: HashMap<Phase, Stat>,
    labels: HashMap<(Phase, Box<str>), Stat>,
    /// The shell's work for each frame it presented.
    frames: Vec<Duration>,
    /// The time between consecutive presented frames.
    intervals: Vec<Duration>,
}

/// Records a frame the shell presented.
#[cfg(feature = "development")]
fn frame(frame: cosmic::iced::runtime::frame::Frame) {
    RECORDER.with_borrow_mut(|recorder| {
        recorder.frames.push(frame.work);
        recorder.intervals.extend(frame.interval);
        recorder.roll();
    });
}

/// `values`' median, 95th, 99th percentile and maximum, nearest-rank.
fn quantiles(values: &mut [Duration]) -> String {
    values.sort_unstable();
    let at = |quantile: f64| {
        let rank = (quantile * values.len() as f64).ceil() as usize;
        values[rank.clamp(1, values.len()) - 1].as_secs_f64() * 1e3
    };
    format!(
        "p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms",
        at(0.5),
        at(0.95),
        at(0.99),
        at(1.0)
    )
}

impl Recorder {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            phases: HashMap::new(),
            labels: HashMap::new(),
            frames: Vec::new(),
            intervals: Vec::new(),
        }
    }

    fn record(&mut self, entry: Entry, elapsed: Duration) {
        let Entry {
            phase,
            label,
            total,
            ..
        } = entry;
        if elapsed >= SLOW {
            eprintln!(
                "ui-trace slow {phase:?} {label} {:.1} ms",
                elapsed.as_secs_f64() * 1e3
            );
        }
        if total {
            self.phases.entry(phase).or_default().add(elapsed);
        }
        if !label.is_empty() {
            self.labels.entry((phase, label)).or_default().add(elapsed);
        }
        self.roll();
    }

    /// Reports and starts a new window once this one is complete.
    fn roll(&mut self) {
        let window = self.started.elapsed();
        if window >= WINDOW {
            self.report(window);
            *self = Self::new();
        }
    }

    fn report(&mut self, window: Duration) {
        let busy: Duration = self.phases.values().map(|stat| stat.total).sum();
        let mut line = format!(
            "ui-trace {:.1}s: busy {:.0} ms ({:.0}%)",
            window.as_secs_f64(),
            busy.as_secs_f64() * 1e3,
            100.0 * busy.as_secs_f64() / window.as_secs_f64()
        );
        let mut phases: Vec<_> = self.phases.iter().collect();
        phases.sort_by_key(|(phase, _)| **phase);
        for (phase, stat) in phases {
            let _ = write!(line, " | {phase:?} {stat}");
        }
        eprintln!("{line}");
        if !self.frames.is_empty() {
            eprintln!(
                "ui-trace   Frames n={} work {}",
                self.frames.len(),
                quantiles(&mut self.frames)
            );
        }
        if !self.intervals.is_empty() {
            eprintln!(
                "ui-trace   Frames interval {}",
                quantiles(&mut self.intervals)
            );
        }
        let mut labels: Vec<_> = self.labels.iter().collect();
        labels.sort_by_key(|((phase, _), stat)| (*phase, std::cmp::Reverse(stat.total)));
        let mut updates = 0;
        for ((phase, label), stat) in labels {
            if *phase == Phase::Update {
                updates += 1;
                if updates > 8 {
                    continue;
                }
            }
            eprintln!("ui-trace   {phase:?} {label} {stat}");
        }
    }
}

/// The message's outer two variant names, from its bounded `Debug` output.
fn label(message: &dyn Debug) -> Box<str> {
    let mut bounded = Bounded(String::with_capacity(LABEL_BYTES));
    // Stopping early is the point; the error only reports it.
    let _ = write!(bounded, "{message:?}");
    variants(&bounded.0).into()
}

fn variants(debug: &str) -> &str {
    let name = |text: &str| {
        text.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
            .unwrap_or(text.len())
    };
    let first = name(debug);
    match debug[first..].strip_prefix('(') {
        Some(rest) => &debug[..first + 1 + name(rest)],
        None => &debug[..first],
    }
}

/// Keeps the first [`LABEL_BYTES`] of what is written to it, then fails, so
/// a large message stops formatting there.
struct Bounded(String);

impl fmt::Write for Bounded {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let room = LABEL_BYTES - self.0.len();
        if text.len() <= room {
            self.0.push_str(text);
            return Ok(());
        }
        let mut end = room;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.0.push_str(&text[..end]);
        Err(fmt::Error)
    }
}

/// Times the window's layout, draw and event handling when tracing, and is
/// the element itself otherwise.
pub fn root<'a, Message: 'a>(content: Element<'a, Message>) -> Element<'a, Message> {
    region("", content)
}

/// Times one part of the view's layout, draw and event handling, inside the
/// [`root`], when tracing, and is the element itself otherwise.
pub fn region<'a, Message: 'a>(
    label: &'static str,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    if enabled() {
        Element::new(Region { label, content })
    } else {
        content
    }
}

struct Region<'a, Message> {
    /// Empty for the root, whose calls are the phases' totals.
    label: &'static str,
    content: Element<'a, Message>,
}

impl<Message> Region<'_, Message> {
    fn scope(&self, phase: Phase) -> Scope {
        Scope(Some(Entry::new(
            phase,
            Box::from(self.label),
            self.label.is_empty(),
        )))
    }

    /// Event handling, named by its region and the event's kind, such as
    /// `sidebar Window(RedrawRequested`.
    fn event_scope(&self, event: &Event) -> Scope {
        let region = if self.label.is_empty() {
            "window"
        } else {
            self.label
        };
        Scope(Some(Entry::new(
            Phase::Event,
            format!("{region} {}", label(event)).into(),
            self.label.is_empty(),
        )))
    }
}

impl<Message> Widget<Message, Theme, Renderer> for Region<'_, Message> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn id(&self) -> Option<Id> {
        self.content.as_widget().id()
    }

    fn set_id(&mut self, id: Id) {
        self.content.as_widget_mut().set_id(id);
    }

    fn diff(&mut self, tree: &mut Tree) {
        let _scope = self.scope(Phase::Diff);
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let _scope = self.scope(Phase::Layout);
        let node = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        layout::Node::with_children(node.size(), vec![node])
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
        let _scope = self.scope(Phase::Draw);
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            child(layout),
            cursor,
            viewport,
        );
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let _scope = self.scope(Phase::Operate);
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            child(layout),
            renderer,
            operation,
        );
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
        let _scope = self.event_scope(event);
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            child(layout),
            cursor,
            renderer,
            clipboard,
            shell,
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
            child(layout),
            cursor,
            viewport,
            renderer,
        )
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let _scope = self.scope(Phase::Overlay);
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            child(layout),
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
        let _scope = self.scope(Phase::A11y);
        self.content
            .as_widget()
            .a11y_nodes(child(layout), &tree.children[0], cursor)
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
            child(layout),
            renderer,
            destinations,
        );
    }
}

fn child(layout: Layout<'_>) -> Layout<'_> {
    layout
        .children()
        .next()
        .expect("the root lays out its content as its only child")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_quantiles_are_nearest_rank() {
        let mut frames: Vec<_> = (1..=200).rev().map(Duration::from_millis).collect();
        assert_eq!(
            quantiles(&mut frames),
            "p50 100.00 p95 190.00 p99 198.00 max 200.00 ms"
        );
        let mut one = vec![Duration::from_micros(2_500)];
        assert_eq!(
            quantiles(&mut one),
            "p50 2.50 p95 2.50 p99 2.50 max 2.50 ms"
        );
    }

    #[test]
    fn a_frame_counts_event_handling_but_not_waiting() {
        use cosmic::iced::runtime::frame::{Clock, Frame};
        thread_local! {
            static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
        }
        let clock =
            Clock::reporting_to(|frame| FRAMES.with_borrow_mut(|frames| frames.push(frame)));
        let busy = Duration::from_millis(20);
        let wait = Duration::from_millis(200);
        drop(clock.handling());
        std::thread::sleep(wait);
        {
            let _handling = clock.handling();
            std::thread::sleep(busy);
            clock.presented();
            // The rest of this event belongs to the next frame.
            std::thread::sleep(busy);
        }
        std::thread::sleep(wait);
        let _handling = clock.handling();
        clock.presented();
        let frames = FRAMES.with_borrow(Clone::clone);
        assert_eq!(frames.len(), 2);
        // Each frame took one busy stretch; neither counts the waits.
        for frame in &frames {
            assert!(frame.work >= busy && frame.work < wait, "{frames:?}");
        }
        assert!(frames[0].interval.is_none());
        assert!(
            frames[1]
                .interval
                .is_some_and(|interval| interval >= busy + wait)
        );
    }

    #[test]
    fn a_message_is_named_by_its_outer_two_variants() {
        assert_eq!(variants("ChatUi(Stream { id: 4 })"), "ChatUi(Stream");
        assert_eq!(variants("Tick"), "Tick");
        assert_eq!(variants("KeyEvent { event: X }"), "KeyEvent");
        assert_eq!(variants("Sidebar(Toggle(ProjectId(7)))"), "Sidebar(Toggle");
    }

    #[test]
    fn a_large_message_stops_formatting_at_the_bound() {
        struct Huge;
        impl Debug for Huge {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("Workspace(Observed(")?;
                for _ in 0..1_000_000 {
                    f.write_str("é, ")?;
                }
                f.write_str("))")
            }
        }
        assert_eq!(&*label(&Huge), "Workspace(Observed");
    }
}
