//! Bounded, development-only observation of native redraw-request event cadence.
//!
//! This does not measure GPU presentation, input latency, or time to first paint.
//! It never requests a redraw or publishes an application message. The fixture
//! owner supplies its busy state and explicitly flushes once on accepted exit.

#![cfg(feature = "development")]

use super::{Element, Message};
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, tree},
};
use cosmic::iced::{Event, Length, Rectangle, Size, Vector, mouse, window};
use cosmic::{Renderer, Theme};
use std::{
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const SAMPLE_CAPACITY: usize = 4_096;
static RECORDER: OnceLock<Mutex<Recorder>> = OnceLock::new();

fn recorder() -> &'static Mutex<Recorder> {
    RECORDER.get_or_init(|| Mutex::new(Recorder::default()))
}

pub(super) fn instrument(content: Element<'_, Message>, busy: bool) -> Element<'_, Message> {
    // A busy -> idle -> busy transition may have no intervening idle redraw.
    // Clear the anchor when that idle view is constructed as well as on events.
    let recorder = recorder();
    if !busy {
        recorder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reset_busy();
    }
    Element::new(Instrument { content, busy })
}

/// Prints at most one bounded, content-free summary, only when explicitly called.
pub(super) fn flush() {
    let summary = recorder()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .finish();
    let Some(summary) = summary else {
        return;
    };
    let milliseconds = |duration: Duration| duration.as_secs_f64() * 1_000.0;
    let json = serde_json::json!({
        "metric": "ui_redraw_request_event_cadence",
        "scope": "consecutive_busy_redraw_requests",
        "percentile_scope": "most_recent_bounded_busy_gaps",
        "sample_capacity": SAMPLE_CAPACITY,
        "redraw_count": summary.redraw_count,
        "busy_redraw_count": summary.busy_redraw_count,
        "idle_redraw_count": summary.idle_redraw_count,
        "duplicate_redraw_count": summary.duplicate_redraw_count,
        "out_of_order_redraw_count": summary.out_of_order_redraw_count,
        "gap_count": summary.gap_count,
        "retained_gap_count": summary.retained_gap_count,
        "discarded_gap_count": summary.discarded_gap_count,
        "p50_ms": summary.p50.map(milliseconds),
        "p95_ms": summary.p95.map(milliseconds),
        "p99_ms": summary.p99.map(milliseconds),
        "max_ms": summary.max.map(milliseconds),
        "over_200_ms": summary.over_200_ms,
        "over_500_ms": summary.over_500_ms,
    });
    eprintln!("cagents_native_fixture_frames={json}");
}

struct Recorder {
    last_redraw: Option<Instant>,
    previous_busy_redraw: Option<Instant>,
    redraw_count: u64,
    busy_redraw_count: u64,
    idle_redraw_count: u64,
    duplicate_redraw_count: u64,
    out_of_order_redraw_count: u64,
    gap_count: u64,
    discarded_gap_count: u64,
    over_200_ms: u64,
    over_500_ms: u64,
    max: Option<Duration>,
    samples: Vec<Duration>,
    next_sample: usize,
    finished: bool,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            last_redraw: None,
            previous_busy_redraw: None,
            redraw_count: 0,
            busy_redraw_count: 0,
            idle_redraw_count: 0,
            duplicate_redraw_count: 0,
            out_of_order_redraw_count: 0,
            gap_count: 0,
            discarded_gap_count: 0,
            over_200_ms: 0,
            over_500_ms: 0,
            max: None,
            samples: Vec::with_capacity(SAMPLE_CAPACITY),
            next_sample: 0,
            finished: false,
        }
    }
}

struct Summary {
    redraw_count: u64,
    busy_redraw_count: u64,
    idle_redraw_count: u64,
    duplicate_redraw_count: u64,
    out_of_order_redraw_count: u64,
    gap_count: u64,
    retained_gap_count: usize,
    discarded_gap_count: u64,
    p50: Option<Duration>,
    p95: Option<Duration>,
    p99: Option<Duration>,
    max: Option<Duration>,
    over_200_ms: u64,
    over_500_ms: u64,
}

impl Recorder {
    fn reset_busy(&mut self) {
        self.previous_busy_redraw = None;
    }

    fn observe(&mut self, at: Instant, busy: bool) {
        if self.finished {
            return;
        }
        if !busy {
            self.reset_busy();
        }
        if let Some(last) = self.last_redraw {
            if at == last {
                // Iced can replay the same event through update/layout passes.
                self.duplicate_redraw_count = self.duplicate_redraw_count.saturating_add(1);
                return;
            }
            if at < last {
                self.out_of_order_redraw_count = self.out_of_order_redraw_count.saturating_add(1);
                return;
            }
        }
        self.last_redraw = Some(at);
        self.redraw_count = self.redraw_count.saturating_add(1);
        if !busy {
            self.idle_redraw_count = self.idle_redraw_count.saturating_add(1);
            return;
        }
        self.busy_redraw_count = self.busy_redraw_count.saturating_add(1);
        if let Some(previous) = self.previous_busy_redraw.replace(at) {
            self.record_gap(at.duration_since(previous));
        }
    }

    fn record_gap(&mut self, gap: Duration) {
        self.gap_count = self.gap_count.saturating_add(1);
        self.over_200_ms = self
            .over_200_ms
            .saturating_add(u64::from(gap > Duration::from_millis(200)));
        self.over_500_ms = self
            .over_500_ms
            .saturating_add(u64::from(gap > Duration::from_millis(500)));
        self.max = Some(self.max.map_or(gap, |maximum| maximum.max(gap)));
        if self.samples.len() < SAMPLE_CAPACITY {
            self.samples.push(gap);
        } else {
            self.samples[self.next_sample] = gap;
            self.next_sample = (self.next_sample + 1) % SAMPLE_CAPACITY;
            self.discarded_gap_count = self.discarded_gap_count.saturating_add(1);
        }
    }

    fn finish(&mut self) -> Option<Summary> {
        if self.finished {
            return None;
        }
        self.finished = true;
        // Sorting happens only at explicit close, never in redraw handling.
        self.samples.sort_unstable();
        let percentile = |percent: usize| {
            let rank = (self.samples.len() * percent).div_ceil(100);
            rank.checked_sub(1)
                .and_then(|index| self.samples.get(index).copied())
        };
        Some(Summary {
            redraw_count: self.redraw_count,
            busy_redraw_count: self.busy_redraw_count,
            idle_redraw_count: self.idle_redraw_count,
            duplicate_redraw_count: self.duplicate_redraw_count,
            out_of_order_redraw_count: self.out_of_order_redraw_count,
            gap_count: self.gap_count,
            retained_gap_count: self.samples.len(),
            discarded_gap_count: self.discarded_gap_count,
            p50: percentile(50),
            p95: percentile(95),
            p99: percentile(99),
            max: self.max,
            over_200_ms: self.over_200_ms,
            over_500_ms: self.over_500_ms,
        })
    }
}

struct Instrument<'a> {
    content: Element<'a, Message>,
    busy: bool,
}

// Delegate the entire widget contract and the original Tree. The observer adds
// neither layout/state nodes nor a second copy of the child's widget ID.
impl Widget<Message, Theme, Renderer> for Instrument<'_> {
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&mut self, tree: &mut Tree) {
        self.content.as_widget_mut().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn id(&self) -> Option<Id> {
        self.content.as_widget().id()
    }

    fn set_id(&mut self, id: Id) {
        self.content.as_widget_mut().set_id(id);
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
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
            .operate(tree, layout, renderer, operation);
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
        if let Event::Window(window::Event::RedrawRequested(at)) = event {
            recorder()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .observe(*at, self.busy);
        }
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
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
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content.as_widget().a11y_nodes(layout, tree, cursor)
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content
            .as_widget()
            .drag_destinations(tree, layout, renderer, destinations);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_cadence_deduplicates_replays_and_excludes_idle_without_a_redraw() {
        let mut recorder = Recorder::default();
        let start = Instant::now();
        recorder.observe(start, true);
        recorder.observe(start, true);
        recorder.observe(start + Duration::from_millis(16), true);
        recorder.observe(start + Duration::from_millis(8), true);
        assert_eq!(
            recorder.previous_busy_redraw,
            Some(start + Duration::from_millis(16))
        );
        recorder.reset_busy(); // An idle view, with no idle redraw before resuming.
        recorder.observe(start + Duration::from_secs(10), true);
        recorder.observe(start + Duration::from_millis(10_250), true);
        recorder.observe(start + Duration::from_secs(11), false);
        recorder.observe(start + Duration::from_secs(20), true);
        recorder.observe(start + Duration::from_millis(20_600), true);
        let summary = recorder.finish().unwrap();
        assert_eq!(summary.redraw_count, 7);
        assert_eq!(summary.busy_redraw_count, 6);
        assert_eq!(summary.idle_redraw_count, 1);
        assert_eq!(summary.duplicate_redraw_count, 1);
        assert_eq!(summary.out_of_order_redraw_count, 1);
        assert_eq!(summary.gap_count, 3);
        assert_eq!(summary.p50, Some(Duration::from_millis(250)));
        assert_eq!(summary.p95, Some(Duration::from_millis(600)));
        assert_eq!(summary.p99, summary.p95);
        assert_eq!(summary.max, summary.p95);
        assert_eq!(summary.over_200_ms, 2);
        assert_eq!(summary.over_500_ms, 1);
        assert!(recorder.finish().is_none());
        recorder.observe(start + Duration::from_secs(30), true);
        assert_eq!(recorder.gap_count, 3);
    }

    #[test]
    fn samples_are_bounded_and_all_time_counters_saturate() {
        let mut recorder = Recorder::default();
        recorder.record_gap(Duration::from_secs(2));
        for _ in 0..SAMPLE_CAPACITY {
            recorder.record_gap(Duration::from_millis(16));
        }
        let summary = recorder.finish().unwrap();
        assert_eq!(summary.retained_gap_count, SAMPLE_CAPACITY);
        assert_eq!(summary.gap_count, SAMPLE_CAPACITY as u64 + 1);
        assert_eq!(summary.discarded_gap_count, 1);
        assert_eq!(summary.p99, Some(Duration::from_millis(16)));
        assert_eq!(summary.max, Some(Duration::from_secs(2)));
        assert_eq!(summary.over_200_ms, 1);
        assert_eq!(summary.over_500_ms, 1);

        let mut saturated = Recorder::default();
        saturated.redraw_count = u64::MAX;
        saturated.busy_redraw_count = u64::MAX;
        saturated.duplicate_redraw_count = u64::MAX;
        saturated.gap_count = u64::MAX;
        saturated.over_200_ms = u64::MAX;
        saturated.over_500_ms = u64::MAX;
        let start = Instant::now();
        saturated.observe(start, true);
        saturated.observe(start, true);
        saturated.observe(start + Duration::from_secs(1), true);
        assert_eq!(saturated.redraw_count, u64::MAX);
        assert_eq!(saturated.busy_redraw_count, u64::MAX);
        assert_eq!(saturated.duplicate_redraw_count, u64::MAX);
        assert_eq!(saturated.gap_count, u64::MAX);
        assert_eq!(saturated.over_200_ms, u64::MAX);
        assert_eq!(saturated.over_500_ms, u64::MAX);
    }

    #[test]
    fn empty_intervals_and_threshold_boundaries_are_explicit() {
        let mut empty = Recorder::default();
        empty.observe(Instant::now(), true);
        let summary = empty.finish().unwrap();
        assert_eq!(summary.gap_count, 0);
        assert_eq!(summary.p50, None);
        assert_eq!(summary.p95, None);
        assert_eq!(summary.p99, None);
        assert_eq!(summary.max, None);

        let mut recorder = Recorder::default();
        recorder.record_gap(Duration::from_millis(200));
        recorder.record_gap(Duration::from_millis(500));
        recorder.record_gap(Duration::from_millis(501));
        let summary = recorder.finish().unwrap();
        assert_eq!(summary.over_200_ms, 2);
        assert_eq!(summary.over_500_ms, 1);
    }
}
