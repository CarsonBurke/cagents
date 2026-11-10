//! The same 1 MiB workloads used to size hidden-chat warmth, on main's API.
use super::*;

#[test]
#[ignore = "fw16 prepared-chat live-heap measurement; run alone"]
fn what_a_prepared_chat_holds() {
    let megabyte = |atom: &str, fence: bool| {
        let mut body = atom.repeat((1024 * 1024usize).div_ceil(atom.len()));
        body.truncate(1024 * 1024);
        if fence {
            body = format!("```text\n{body}\n```\n");
        }
        vec![
            item(
                "huge-prompt".into(),
                chat::Role::User,
                prose("Write a long answer.".into(), false, chat::Phase::Final),
            ),
            item(
                "huge-answer".into(),
                chat::Role::Assistant,
                prose(body, false, chat::Phase::Final),
            ),
        ]
    };
    for (name, items) in [
        ("window of 16 exchanges", transcript(16)),
        (
            "1 MiB answer of paragraphs",
            megabyte("A measured **plain** paragraph with `code`.\n\n", false),
        ),
        (
            "1 MiB answer in one fence",
            megabyte("let measured = fence_line(1);\n", true),
        ),
    ] {
        let mut pane = Pane::new(items);
        pane.settle_geometry();
        for _ in 0..4 {
            pane.frame();
        }
        pane.cache = Cache::new();
        preparation::retirement_barrier();
        let transcript = pane.runtime.snapshot.transcript_bytes();
        crate::test_heap::start();
        let before = crate::test_heap::live();
        pane.runtime.set_visible(false).expect("hide");
        // Explicit release keeps this workload comparable when warm retention
        // lands: both revisions release the actual prepared presentation.
        pane.runtime.release_transcript_presentation();
        preparation::retirement_barrier();
        let freed = before - crate::test_heap::live();
        crate::test_heap::stop();
        eprintln!(
            "prepared_chat_heap name={name:?} transcript_bytes={transcript} freed_bytes={freed} freed_mib={:.3}",
            freed as f64 / (1024. * 1024.)
        );
    }
}
