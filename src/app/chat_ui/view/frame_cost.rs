//! What one streaming frame of a long transcript costs the interface.
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
//! The workload is the store's whole window, 128 items in sixteen finished
//! exchanges, with the newest answer streaming into it one provider frame at a
//! time. Each frame is split into the stages the runtime runs in sequence:
//! taking the snapshot, building the view, diffing and laying it out, and
//! drawing it.

use super::super::*;
use super::Place;
use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
use cosmic::iced::{Size, mouse};
use std::time::{Duration, Instant};

const FRAMES: usize = 240;
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

#[test]
#[ignore = "a measurement, not a check; see the module documentation"]
fn a_streaming_frame_of_a_full_window() {
    // One exchange is the pane's fixed cost; sixteen are the store's whole
    // window. The difference is what the transcript's length costs a frame.
    for turns in [1, 16] {
        measure(turns);
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

fn measure(turns: usize) {
    let mut items = transcript(turns);
    let streaming = items.len() - 1;
    let (mut runtime, handle) = live_runtime(&items);
    let theme = cosmic::Theme::dark();
    let mut renderer = crate::app::tests::headless_renderer();
    let mut cache = Cache::new();
    let (mut update, mut view, mut layout, mut draw) = (
        Stage::default(),
        Stage::default(),
        Stage::default(),
        Stage::default(),
    );
    let mut generation = 0;
    for frame in 0..FRAMES {
        if let ItemContent::Text { text, .. } = &mut items[streaming].content {
            text.push_str(CHUNK);
            // A paragraph of about four hundred bytes, as answers are written.
            if frame % 8 == 7 {
                text.push_str("\n\n");
            }
        }
        generation += 1;
        handle.publish_snapshot_for_test(Snapshot {
            generation,
            ready: true,
            items: items.iter().cloned().map(Arc::new).collect(),
            ..Snapshot::default()
        });
        let warm = frame >= 8;
        let clock = Instant::now();
        runtime.refresh();
        let refreshed = clock.elapsed();
        let clock = Instant::now();
        let element = runtime.view(
            false,
            true,
            720.0,
            Place {
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
            std::mem::replace(&mut cache, Cache::new()),
            &mut renderer,
        );
        let laid_out = clock.elapsed();
        let clock = Instant::now();
        ui.draw(
            &mut renderer,
            &theme,
            &cosmic::iced::advanced::renderer::Style::default(),
            mouse::Cursor::Unavailable,
        );
        let drawn = clock.elapsed();
        if warm {
            update.0.push(refreshed);
            view.0.push(viewed);
            layout.0.push(laid_out);
            draw.0.push(drawn);
        }
        cache = ui.into_cache();
    }
    println!(
        "{} items, {} warm frames of a streaming answer:",
        items.len(),
        FRAMES - 8
    );
    let total = update.report("update")
        + view.report("view")
        + layout.report("layout")
        + draw.report("draw");
    println!("   total: median sum {:>8.3} ms", total.as_secs_f64() * 1e3);
}
