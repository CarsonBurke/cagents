//! What one frame of a long transcript costs the interface.
//!
//! Opt-in and non-gating: run it against an optimized crate, because an
//! unoptimized one measures the compiler rather than the surface.
//!
//! ```sh
//! cargo test --locked --no-default-features --features ui --profile dev-runtime \
//!     --config 'profile.dev-runtime.opt-level=2' --lib \
//!     chat_ui::view::frame_cost -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The base workload is the store's whole window, 128 items in sixteen
//! finished exchanges, with the newest answer streaming into it one provider
//! frame at a time. Each frame is split into the stages the runtime runs in
//! sequence: taking the snapshot, building the view, diffing and laying it
//! out, and drawing it. The other workloads vary one thing about it: a single
//! fenced block that grows to two thousand lines, a step or a reasoning block
//! the reader left open, an interaction that changes no item, and the
//! metadata a provider's snapshot carries beside its items.
//!
//! Nothing here reaches a GPU or a compositor: `draw` is the widgets
//! recording what to draw into a headless renderer.

use super::super::*;
use super::Place;
use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
use cosmic::iced::{Size, mouse};
use std::time::{Duration, Instant};

const FRAMES: usize = 240;
/// Frames a recorded workload discards while caches and allocations settle.
const WARMUP: usize = 8;
/// How many frames a workload that is not a stream of prose records.
const SAMPLES: usize = 32;
/// What one 50 ms presentation frame of a streaming answer adds.
const CHUNK: &str = "streamed words arrive in small appended pieces, ";
const WINDOW: Size = Size::new(960.0, 1_200.0);

fn answer(turn: usize) -> String {
    let mut text = format!(
        "## Summary of change {turn}\n\nThe parser now keeps its **cursor** across \
         refreshes, so a streamed body costs its new bytes rather than its whole \
         length. See `src/parser.rs` and [the design note](https://example.com/{turn}).\n\n\
         - The cache is keyed by item id.\n- A rewrite is detected by a tail digest.\n\
         - Nothing is copied out of the snapshot.\n\n"
    );
    for block in 0..2 {
        text.push_str("```rust\n");
        for line in 0..24 {
            text.push_str(&format!(
                "    let value_{block}_{line} = cache.get(&key).map(|entry| entry.len() + {line});\n"
            ));
        }
        text.push_str("```\n\n");
    }
    text.push_str(
        "That leaves the view as the remaining per-frame cost, which the next change \
         addresses by retaining finished exchanges.\n",
    );
    text
}

fn item(id: String, role: chat::Role, content: ItemContent) -> chat::Item {
    chat::Item {
        id,
        role,
        content,
        started_at: None,
        at: Some(1_700_000_000_000),
    }
}

fn prose(text: String, thinking: bool, phase: chat::Phase) -> ItemContent {
    ItemContent::Text {
        text,
        thinking,
        phase,
    }
}

/// Sixteen exchanges of eight items: a prompt, reasoning, four commands with
/// output, narration, and an answer with two fenced blocks.
pub(super) fn transcript(turns: usize) -> Vec<chat::Item> {
    let mut items = Vec::new();
    for turn in 0..turns {
        items.push(item(
            format!("{turn}-prompt"),
            chat::Role::User,
            prose(
                format!("Please make change {turn} and run the tests."),
                false,
                chat::Phase::Final,
            ),
        ));
        items.push(item(
            format!("{turn}-thinking"),
            chat::Role::Assistant,
            prose(
                "I should read the parser first, then the cache, and check how the \
                 digest is computed before I change anything. "
                    .repeat(6),
                true,
                chat::Phase::Final,
            ),
        ));
        for step in 0..4 {
            items.push(item(
                format!("{turn}-step-{step}"),
                chat::Role::Assistant,
                ItemContent::Command {
                    label: String::new(),
                    command: format!("cargo test --lib parser::case_{step}"),
                    working_directory: "/project".into(),
                    action: chat::CommandAction::Run,
                    status: chat::ToolStatus::Succeeded,
                    exit_code: Some(0),
                    duration_ms: Some(1_250),
                    output: "test parser::case ... ok\n".repeat(80),
                },
            ));
        }
        items.push(item(
            format!("{turn}-commentary"),
            chat::Role::Assistant,
            prose(
                "The tests pass; writing up the change.".into(),
                false,
                chat::Phase::Commentary,
            ),
        ));
        items.push(item(
            format!("{turn}-answer"),
            chat::Role::Assistant,
            prose(answer(turn), false, chat::Phase::Final),
        ));
    }
    items
}

#[derive(Default)]
struct Stage(Vec<Duration>);

impl Stage {
    fn median(&mut self) -> Duration {
        self.0.sort_unstable();
        self.0[(self.0.len() - 1) / 2]
    }

    fn report(&mut self, name: &str) -> Duration {
        self.0.sort_unstable();
        let at = |quantile: f64| self.0[((self.0.len() - 1) as f64 * quantile) as usize];
        let median = at(0.5);
        println!(
            "{name:>8}: median {:>8.3} ms  p95 {:>8.3} ms  max {:>8.3} ms",
            median.as_secs_f64() * 1e3,
            at(0.95).as_secs_f64() * 1e3,
            self.0.last().unwrap().as_secs_f64() * 1e3,
        );
        median
    }
}

/// The four stages of the frames a workload chose to record.
#[derive(Default)]
struct Stages {
    update: Stage,
    view: Stage,
    layout: Stage,
    draw: Stage,
}

impl Stages {
    fn record(&mut self, frame: [Duration; 4]) {
        let [update, view, layout, draw] = frame;
        self.update.0.push(update);
        self.view.0.push(view);
        self.layout.0.push(layout);
        self.draw.0.push(draw);
    }

    fn report(&mut self) {
        let total = self.update.report("update")
            + self.view.report("view")
            + self.layout.report("layout")
            + self.draw.report("draw");
        println!("   total: median sum {:>8.3} ms", total.as_secs_f64() * 1e3);
    }

    /// One line of medians, for a workload that reports several cases.
    fn row(&mut self, name: &str) {
        let stages = [
            self.update.median(),
            self.view.median(),
            self.layout.median(),
            self.draw.median(),
        ];
        let [update, view, layout, draw] = stages.map(|stage| stage.as_secs_f64() * 1e3);
        println!(
            "{name:>28}: update {update:>8.3}  view {view:>8.3}  layout {layout:>8.3}  \
             draw {draw:>8.3}  total {:>8.3} ms",
            update + view + layout + draw
        );
    }
}

#[test]
#[ignore = "a measurement, not a check; see the module documentation"]
fn a_streaming_frame_of_a_full_window() {
    // One exchange is the pane's fixed cost; sixteen are the store's whole
    // window. The difference is what the transcript's length costs a frame.
    for turns in [1, 16] {
        let mut pane = Pane::new(transcript(turns));
        let mut stages = pane.stream_prose();
        println!(
            "{} items, {} warm frames of a streaming answer:",
            pane.items.len(),
            FRAMES - WARMUP
        );
        stages.report();
    }
}

/// A live runtime showing `items`, and the handle that publishes to it.
pub(super) fn live_runtime(items: &[chat::Item]) -> (Runtime, Arc<ClientHandle>) {
    let account = Account::system(Provider::Claude);
    let session = fresh_chat_session(
        Provider::Claude,
        TerminalId::new(),
        "/project".into(),
        &account,
        false,
    )
    .expect("a session");
    let base = Snapshot {
        ready: true,
        items: items.iter().cloned().map(Arc::new).collect(),
        ..Snapshot::default()
    };
    let handle = Arc::new(ClientHandle::fixture(session.clone(), base));
    let target = Target {
        card: CardId::new(),
        generation: 1,
    };
    let runtime = Runtime::new(
        target,
        session,
        Connection::Live(handle.clone()),
        true,
        crate::settings::EditorSyntaxTheme::default(),
    );
    (runtime, handle)
}

/// A chat pane on a headless renderer, driven one frame at a time.
struct Pane {
    runtime: Runtime,
    handle: Arc<ClientHandle>,
    items: Vec<chat::Item>,
    /// Everything a published snapshot carries besides its items.
    metadata: Snapshot,
    generation: u64,
    renderer: cosmic::Renderer,
    cache: Cache,
    theme: cosmic::Theme,
    shortcuts: crate::settings::KeyboardShortcuts,
}

impl Pane {
    fn new(items: Vec<chat::Item>) -> Self {
        let (runtime, handle) = live_runtime(&items);
        Self {
            runtime,
            handle,
            items,
            metadata: Snapshot {
                ready: true,
                ..Snapshot::default()
            },
            generation: 0,
            renderer: crate::app::tests::headless_renderer(),
            cache: Cache::new(),
            theme: cosmic::Theme::dark(),
            shortcuts: crate::settings::KeyboardShortcuts::default(),
        }
    }

    /// The text of the newest item, which is the one a workload streams into.
    fn tail(&mut self) -> &mut String {
        match &mut self.items.last_mut().expect("an item").content {
            ItemContent::Text { text, .. } => text,
            _ => unreachable!("the streamed item is prose"),
        }
    }

    /// Publishes the items as a provider frame would, without taking it.
    fn publish(&mut self) {
        self.generation += 1;
        let mut snapshot = Snapshot {
            generation: self.generation,
            items: self.items.iter().cloned().map(Arc::new).collect(),
            ..self.metadata.clone()
        };
        if let Some(child) = snapshot
            .subagents
            .iter_mut()
            .find(|child| child.id == "streaming-child")
        {
            child.items = vec![snapshot.items.pop().expect("streamed child item")];
        }
        self.handle.publish_snapshot_for_test(snapshot);
    }

    /// Opens `id` the way pressing its header does.
    fn open(&mut self, id: &str) {
        let runtime = &mut self.runtime;
        runtime.expanded.insert(id.to_owned());
        let (index, item) = runtime
            .snapshot
            .items
            .iter()
            .enumerate()
            .find(|(_, item)| item.id == id)
            .expect("an item to open");
        // A step is only drawn while the strip it belongs to is open.
        if let Some(turn) = runtime
            .turns
            .iter()
            .find(|turn| turn.steps.iter().any(|placed| placed.index == index))
        {
            runtime.opened.insert(turn.key.clone(), true);
        }
        let coordination = chat::coordination::recognize(item, runtime.session.launch.provider);
        let previous = runtime.cache.remove(id);
        runtime.cache.insert(
            id.to_owned(),
            CachedItem::updated(
                &item.content,
                runtime.syntax_theme,
                previous,
                true,
                false,
                coordination,
            ),
        );
    }

    /// One frame: the refresh, the view, its diff and layout, and its draw.
    fn frame(&mut self) -> [Duration; 4] {
        let clock = Instant::now();
        self.runtime.refresh();
        let refreshed = clock.elapsed();
        let clock = Instant::now();
        let element = self.runtime.view(
            false,
            true,
            &self.shortcuts,
            720.0,
            Place {
                channel: None,
                name: "project".into(),
                branch: None,
            },
            true,
            None,
            Vec::new(),
        );
        let viewed = clock.elapsed();
        let clock = Instant::now();
        let mut ui = UserInterface::build(
            element,
            WINDOW,
            std::mem::replace(&mut self.cache, Cache::new()),
            &mut self.renderer,
        );
        let laid_out = clock.elapsed();
        let clock = Instant::now();
        ui.draw(
            &mut self.renderer,
            &self.theme,
            &cosmic::iced::advanced::renderer::Style::default(),
            mouse::Cursor::Unavailable,
        );
        let drawn = clock.elapsed();
        self.cache = ui.into_cache();
        [refreshed, viewed, laid_out, drawn]
    }

    /// Streams prose into the newest item for `FRAMES` frames.
    fn stream_prose(&mut self) -> Stages {
        let mut stages = Stages::default();
        for frame in 0..FRAMES {
            let text = self.tail();
            text.push_str(CHUNK);
            // A paragraph of about four hundred bytes, as answers are written.
            if frame % 8 == 7 {
                text.push_str("\n\n");
            }
            self.publish();
            let stage = self.frame();
            if frame >= WARMUP {
                stages.record(stage);
            }
        }
        stages
    }

    /// `SAMPLES` frames that each follow `change`, after a few discarded ones.
    fn sample(&mut self, mut change: impl FnMut(&mut Self, usize)) -> Stages {
        let mut stages = Stages::default();
        for frame in 0..WARMUP + SAMPLES {
            change(self, frame);
            let stage = self.frame();
            if frame >= WARMUP {
                stages.record(stage);
            }
        }
        stages
    }
}

/// Lines of the fenced block the long-block workload streams, repeated.
const CODE: [&str; 10] = [
    "fn fold(input: &[u8]) -> u32 {\n",
    "    let mut total = 0u32;\n",
    "    for byte in input {\n",
    "        total += u32::from(*byte);\n",
    "    }\n",
    "    // Fold the carry back in.\n",
    "    total = (total >> 16) + (total & 0xffff);\n",
    "    total\n",
    "}\n",
    "\n",
];
/// The block's length at each point its frame is measured, in lines.
const CHECKPOINTS: [usize; 5] = [100, 250, 500, 1_000, 2_000];
/// What one 50 ms presentation frame adds to a streaming block.
const DELTA: usize = 48;

/// One fenced block streamed to two thousand lines, a provider frame at a
/// time, and what a frame costs at each length on the way.
///
/// Every frame is refreshed, as the runtime does. The view, layout and draw
/// are run for the frames of the last hundred or so lines before each
/// checkpoint, and the final thirty-two of those are the ones recorded. The
/// block stays under the size past which a fence is drawn without
/// highlighting, so every checkpoint measures a highlighted block.
#[test]
#[ignore = "a measurement, not a check; see the module documentation"]
fn a_frame_of_a_long_streaming_block() {
    long_streaming_block(false);
}

#[test]
#[ignore = "a measurement, not a check; see the module documentation"]
fn a_frame_of_a_long_background_streaming_block() {
    long_streaming_block(true);
}

fn long_streaming_block(background: bool) {
    let source: String = CODE
        .iter()
        .cycle()
        .take(*CHECKPOINTS.last().unwrap())
        .copied()
        .collect();
    assert!(source.len() < CachedItem::HIGHLIGHT_LIMIT);
    let mut items = transcript(1);
    items.push(item(
        "block".into(),
        chat::Role::Assistant,
        prose(
            "The whole file:\n\n```rust\n".into(),
            false,
            chat::Phase::Final,
        ),
    ));
    let mut pane = Pane::new(items);
    pane.metadata.busy = !background;
    if background {
        pane.metadata.subagents.push(chat::Subagent {
            model: None,
            waiting: false,
            detail: String::new(),
            id: "streaming-child".into(),
            kind: "task".into(),
            description: "The child continues after its parent settled".into(),
            status: chat::ToolStatus::Running,
            background: true,
            items: Vec::new(),
        });
        pane.runtime.selected_subagent = Some("streaming-child".into());
    }
    println!(
        "background child: {background}, parent busy: {}",
        pane.metadata.busy
    );
    println!(
        "one fenced block streamed {DELTA} bytes a frame, {SAMPLES} frames ending at each length:"
    );
    let (mut lines, mut refreshing) = (0, Duration::ZERO);
    let mut checkpoints = CHECKPOINTS.iter().copied().peekable();
    let mut window: Vec<[Duration; 4]> = Vec::new();
    for delta in source.as_bytes().chunks(DELTA) {
        let delta = std::str::from_utf8(delta).expect("the block is ASCII");
        pane.tail().push_str(delta);
        lines += delta.matches('\n').count();
        pane.publish();
        let Some(checkpoint) = checkpoints.peek().copied() else {
            break;
        };
        // About two lines arrive per frame, so this leaves room for the
        // discarded frames ahead of the recorded ones.
        if lines + 3 * (WARMUP + SAMPLES) >= checkpoint {
            let stage = pane.frame();
            refreshing += stage[0];
            window.push(stage);
        } else {
            let clock = Instant::now();
            pane.runtime.refresh();
            refreshing += clock.elapsed();
        }
        if lines >= checkpoint {
            let mut stages = Stages::default();
            for stage in &window[window.len().saturating_sub(SAMPLES)..] {
                stages.record(*stage);
            }
            stages.row(&format!("{checkpoint} lines"));
            window.clear();
            checkpoints.next();
        }
    }
    println!(
        "    every refresh of the stream together: {:>8.1} ms",
        refreshing.as_secs_f64() * 1e3
    );
}

/// A streaming frame of the full window while the reader has left one thing
/// open in the oldest exchange: nothing, a command with its output, or its
/// reasoning.
#[test]
#[ignore = "a measurement, not a check; see the module documentation"]
fn a_streaming_frame_beside_an_open_item() {
    println!(
        "128 items, {} warm frames of a streaming answer, one item open:",
        FRAMES - WARMUP
    );
    for (name, open) in [
        ("nothing open", None),
        ("a command open", Some("0-step-0")),
        ("reasoning open", Some("0-thinking")),
    ] {
        let mut pane = Pane::new(transcript(16));
        if let Some(id) = open {
            pane.open(id);
        }
        pane.stream_prose().row(name);
    }
}

/// A frame of the full window after something that changes no item: nothing
/// at all, a message's copy control pressed and released, a keystroke in the
/// find field, and the turn ending.
///
/// The frame with no change is what a second update costs when a first one
/// only reported the transcript's new height.
#[test]
#[ignore = "a measurement, not a check; see the module documentation"]
fn a_frame_after_an_interaction_on_a_full_window() {
    println!("128 items, {SAMPLES} frames after each interaction:");
    let mut pane = Pane::new(transcript(16));
    pane.sample(|_, _| {}).row("no change");
    pane.sample(|pane, frame| {
        pane.runtime.copied = (frame % 2 == 0).then(|| ("7-answer".to_owned(), Instant::now()));
    })
    .row("copy pressed or cleared");
    // Both spellings match the seventh exchange alone, and both spellings of
    // the other match every exchange.
    for (name, query) in [
        ("find, one exchange matching", "change 7 and"),
        ("find, every exchange matching", "parser"),
    ] {
        pane.sample(|pane, frame| {
            let typed = &query[..query.len() - frame % 2];
            pane.runtime.find = Some(typed.to_owned());
            pane.runtime.refresh_find();
        })
        .row(name);
    }
    pane.runtime.find = None;
    pane.runtime.refresh_find();
    pane.sample(|pane, frame| {
        pane.metadata.busy = frame % 2 == 0;
        pane.publish();
    })
    .row("busy started or stopped");
}

/// The snapshot a provider with a long catalog publishes: its models,
/// commands, skills and plan, about as large as the store lets them be.
fn metadata() -> Snapshot {
    Snapshot {
        ready: true,
        busy: true,
        models: (0..48)
            .map(|index| chat::Model {
                id: format!("provider/model-{index}-20260101"),
                label: format!("Model {index}"),
                reasoning: ["low", "medium", "high", "max"].map(String::from).into(),
            })
            .collect(),
        selected_model: Some("provider/model-7-20260101".into()),
        commands: (0..64)
            .map(|index| chat::SlashCommand {
                name: format!("command-{index}"),
                description: "Runs the project's command of this name with its arguments.".into(),
            })
            .collect(),
        skills: (0..160)
            .map(|index| chat::Skill {
                name: format!("skill-{index}"),
                description: "Use when the task needs this skill; it explains when to load \
                              it, what it reads, and what it leaves for the caller to decide."
                    .into(),
                invocation: format!("/skill-{index}"),
            })
            .collect(),
        plan: (0..8)
            .map(|index| chat::PlanStep {
                text: format!("Step {index} of the plan the conversation is working to."),
                status: chat::PlanState::default(),
            })
            .collect(),
        title: Some("Keep the parser's cursor across refreshes".into()),
        session_id: Some("0f2c1d7e-5a53-4d51-9a6b-2d1c3f6f1b0a".into()),
        ..Snapshot::default()
    }
}

/// What a streamed update of the full window costs before any of it is
/// drawn, when the snapshot carries a provider's catalog as well.
///
/// `apply` is the reader thread installing a one-item patch while the surface
/// still holds the previous snapshot, `refresh` is the surface taking it, and
/// the last two are parts of that refresh timed on their own.
#[test]
#[ignore = "a measurement, not a check; see the module documentation"]
fn a_refresh_of_a_full_window_with_metadata() {
    let mut pane = Pane::new(transcript(16));
    pane.metadata = metadata();
    println!(
        "128 items and {} bytes of metadata, {} warm frames of a streaming answer:",
        postcard::to_stdvec(&pane.metadata)
            .expect("a snapshot")
            .len(),
        FRAMES - WARMUP
    );
    pane.stream_prose().update.report("refresh");

    let mut installed = pane.runtime.snapshot.clone();
    let mut apply = Stage::default();
    for frame in 0..FRAMES {
        // The surface holds the snapshot it last drew.
        let held = installed.clone();
        let previous = installed.items.last().expect("an item").clone();
        let mut next = (*previous).clone();
        if let ItemContent::Text { text, .. } = &mut next.content {
            text.push_str(CHUNK);
        }
        let update = chat::presentation::Presentation {
            base_generation: installed.generation,
            metadata: Snapshot {
                generation: installed.generation + 1,
                ..pane.metadata.clone()
            },
            order: installed.items.iter().map(|item| item.id.clone()).collect(),
            changes: vec![chat::presentation::ItemChange::between(
                Some(&previous),
                &next,
            )],
            children: Vec::new(),
        };
        drop(previous);
        let clock = Instant::now();
        installed = install(update, installed);
        let elapsed = clock.elapsed();
        if frame >= WARMUP {
            apply.0.push(elapsed);
        }
        drop(held);
    }
    apply.report("apply");

    let (mut group, mut history) = (Stage::default(), Stage::default());
    for _ in 0..FRAMES {
        let clock = Instant::now();
        let cache = &pane.runtime.cache;
        let turns = turn::group(
            &pane.runtime.snapshot.items,
            &pane.runtime.asked_after,
            |item| cache.get(&item.id)?.coordination.as_deref(),
        );
        group.0.push(clock.elapsed());
        std::hint::black_box(turns);
        let clock = Instant::now();
        let prompts: Vec<String> = pane
            .runtime
            .snapshot
            .items
            .iter()
            .filter_map(|item| match (&item.role, &item.content) {
                (chat::Role::User, ItemContent::Text { text, .. }) => Some(text.clone()),
                _ => None,
            })
            .collect();
        history.0.push(clock.elapsed());
        std::hint::black_box(prompts);
    }
    group.report("group");
    history.report("history");
}

/// Installs `update` over `installed` as the reader thread does.
fn install(update: chat::presentation::Presentation, installed: Arc<Snapshot>) -> Arc<Snapshot> {
    Arc::new(update.apply(&installed).expect("a patch that applies"))
}

/// Sparse child metadata must refresh a kept launch card even when neither
/// its tool content nor the child's running status changed.
#[test]
fn retained_coordination_cards_refresh_child_model_and_waiting_metadata() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    #[derive(Default)]
    struct ReadLabels(Vec<String>);
    impl Operation for ReadLabels {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            bounds: iced::Rectangle,
            text: &str,
        ) {
            if bounds.width > 0.0 && bounds.height > 0.0 {
                self.0.push(text.to_owned());
            }
        }
    }
    let items = vec![
        item(
            "first-prompt".into(),
            chat::Role::User,
            prose("Inspect parser".into(), false, chat::Phase::Final),
        ),
        item(
            "native-child".into(),
            chat::Role::Assistant,
            ItemContent::Tool {
                name: "Agent".into(),
                input: serde_json::json!({"prompt":"Inspect parser", "run_in_background":true})
                    .to_string(),
                output: String::new(),
                status: chat::ToolStatus::Succeeded,
            },
        ),
        item(
            "first-answer".into(),
            chat::Role::Assistant,
            prose("The agent is checking.".into(), false, chat::Phase::Final),
        ),
        item(
            "second-prompt".into(),
            chat::Role::User,
            prose("Another question".into(), false, chat::Phase::Final),
        ),
        item(
            "second-answer".into(),
            chat::Role::Assistant,
            prose("Another answer".into(), false, chat::Phase::Final),
        ),
    ];
    let (mut runtime, _handle) = live_runtime(&items);
    Arc::make_mut(&mut runtime.snapshot)
        .subagents
        .push(chat::Subagent {
            id: "native-child".into(),
            kind: "Scout".into(),
            description: "Inspect parser".into(),
            status: chat::ToolStatus::Running,
            model: None,
            waiting: false,
            detail: String::new(),
            background: true,
            items: Vec::new(),
        });
    assert_eq!(runtime.turns.len(), 2);
    assert!(runtime.retainable(&runtime.turns[0]));
    let dark = cosmic::theme::is_dark();
    let settings =
        crate::design::markdown_settings(&cosmic::theme::active(), crate::design::TEXT_PROSE);
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(
        runtime
            .turn_view(&runtime.turns[0], settings, dark)
            .as_widget(),
    );
    let labels = |runtime: &Runtime, tree: &mut Tree| {
        let mut element = runtime.turn_view(&runtime.turns[0], settings, dark);
        tree.diff(element.as_widget_mut());
        let node = element.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(640.0, 2400.0)),
        );
        let mut labels = ReadLabels::default();
        element
            .as_widget_mut()
            .operate(tree, Layout::new(&node), &renderer, &mut labels);
        labels.0
    };
    let initial = labels(&runtime, &mut tree);
    assert!(
        initial
            .iter()
            .any(|label| label.contains("Model not reported")),
        "{initial:?}"
    );
    assert!(
        initial
            .iter()
            .any(|label| label.contains("Working in the background")),
        "{initial:?}"
    );
    let unrelated = runtime.turn_fingerprint(&runtime.turns[1], dark);
    Arc::make_mut(&mut runtime.snapshot).subagents[0].model = Some("gpt-child-reported".into());
    let hydrated = labels(&runtime, &mut tree);
    assert!(
        hydrated
            .iter()
            .any(|label| label.contains("gpt-child-reported")),
        "{hydrated:?}"
    );
    assert!(
        !hydrated
            .iter()
            .any(|label| label.contains("Model not reported")),
        "{hydrated:?}"
    );
    assert_eq!(runtime.turn_fingerprint(&runtime.turns[1], dark), unrelated);
    Arc::make_mut(&mut runtime.snapshot).subagents[0].waiting = true;
    let waiting = labels(&runtime, &mut tree);
    assert!(
        waiting
            .iter()
            .any(|label| label.contains("Waiting for you")),
        "{waiting:?}"
    );
    assert!(
        !waiting
            .iter()
            .any(|label| label.contains("Working in the background")),
        "{waiting:?}"
    );
    assert_eq!(runtime.turn_fingerprint(&runtime.turns[1], dark), unrelated);
}
