use super::*;
use iced::advanced::{
    Layout, layout,
    widget::{Operation, Tree},
};

#[derive(Default)]
struct Texts(Vec<String>);

impl Operation for Texts {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }

    fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
        self.0.push(text.to_owned());
    }
}

fn rendered(runtime: &Runtime, tree: &mut Option<Tree>) -> Vec<String> {
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    let mut element = runtime.view(
        None,
        false,
        true,
        &shortcuts,
        600.0,
        Place {
            name: "completion fixture".into(),
            branch: None,
            channel: None,
        },
        true,
        None,
        vec![],
    );
    let renderer = crate::app::tests::headless_renderer();
    let tree = tree.get_or_insert_with(|| Tree::new(element.as_widget()));
    tree.diff(element.as_widget_mut());
    let node = element.as_widget_mut().layout(
        tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 1000.0)),
    );
    let mut texts = Texts::default();
    element
        .as_widget_mut()
        .operate(tree, Layout::new(&node), &renderer, &mut texts);
    texts.0
}

fn install(runtime: &mut Runtime, change: impl FnOnce(&mut Snapshot)) {
    let mut snapshot = (*runtime.snapshot).clone();
    snapshot.generation += 1;
    change(&mut snapshot);
    runtime.install_snapshot(Arc::new(snapshot), None);
}

#[test]
fn completion_caption_uses_exact_turn_evidence_and_keeps_other_errors_distinct() {
    use turn_completion::{Caption, caption};

    for (detail, error, local_error, expected, icon) in [
        ("", None, false, Caption::Finished, Icon::Check),
        (
            "Turn interrupted",
            Some("Cancellation diagnostic"),
            true,
            Caption::Interrupted,
            Icon::Cancel,
        ),
        ("Turn failed", None, false, Caption::Failed, Icon::Warning),
        (
            "",
            Some("Earlier history unavailable"),
            false,
            Caption::FinishedWithError,
            Icon::Warning,
        ),
        ("", None, true, Caption::FinishedWithError, Icon::Warning),
        (
            "A tool failed; the provider turn completed",
            None,
            false,
            Caption::Finished,
            Icon::Check,
        ),
    ] {
        let snapshot = Snapshot {
            detail: detail.into(),
            error: error.map(str::to_owned),
            ..Snapshot::default()
        };
        assert_eq!(caption(&snapshot, local_error), expected);
        assert_eq!(expected.icon(), icon);
        assert_eq!(
            expected.at("10:03"),
            format!("{} at 10:03", expected.label())
        );
    }
}

#[test]
fn finished_turn_reconciles_late_outcomes_at_the_same_cached_timestamp() {
    let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
    let at = 1_700_000_000_000;
    install(&mut runtime, |snapshot| snapshot.finished_at = Some(at));
    let stamp = runtime.finished_stamp.clone().expect("recorded local time");
    assert_eq!(stamp.0, at);
    assert_eq!(
        stamp.1,
        chat::local_time((at / 1000) as i64).unwrap().clock()
    );
    let mut tree = None;
    let mut previous = None;
    for (detail, error, expected, icon) in [
        ("", None, "Finished", Icon::Check),
        ("Turn interrupted", None, "Interrupted", Icon::Cancel),
        ("Turn failed", None, "Failed", Icon::Warning),
        (
            "",
            Some("Earlier history unavailable"),
            "Finished with error",
            Icon::Warning,
        ),
        ("", None, "Finished", Icon::Check),
    ] {
        install(&mut runtime, |snapshot| {
            snapshot.detail = detail.into();
            snapshot.error = error.map(str::to_owned);
        });
        assert_eq!(runtime.finished_stamp, Some(stamp.clone()));
        assert_eq!(
            runtime.conversation_status_base(false),
            (Some(icon), expected)
        );
        let label = format!("{expected} at {}", stamp.1);
        let texts = rendered(&runtime, &mut tree);
        assert!(texts.contains(&label), "real transcript contains {label:?}");
        if let Some(previous) = &previous
            && previous != &label
        {
            assert!(!texts.contains(previous), "obsolete caption was withdrawn");
        }
        previous = Some(label);
    }
    // Local presentation errors can change without a new provider timestamp
    // or Snapshot publication, and must not become a confirmed failed turn.
    runtime.error = Some("Local attachment preview unavailable".into());
    assert_eq!(runtime.finished_stamp, Some(stamp.clone()));
    assert_eq!(
        runtime.conversation_status_base(false).1,
        "Finished with error"
    );
    assert!(rendered(&runtime, &mut tree).contains(&format!("Finished with error at {}", stamp.1)));
}

#[test]
fn finished_turn_marker_keeps_work_queue_paging_and_hidden_gates() {
    let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
    install(&mut runtime, |snapshot| {
        snapshot.finished_at = Some(1_700_000_000_000);
        snapshot.detail = "Turn interrupted".into();
    });
    let stamp = runtime.finished_stamp.clone().unwrap();
    let label = format!("Interrupted at {}", stamp.1);
    let mut tree = None;
    assert!(rendered(&runtime, &mut tree).contains(&label));
    for gate in 0..4 {
        install(&mut runtime, |snapshot| {
            snapshot.busy = gate == 0;
            snapshot.waiting = gate == 1;
            snapshot.has_later = gate == 2;
            snapshot.queued_messages = if gate == 3 {
                vec![chat::QueuedMessage {
                    id: "queued-after-completion".into(),
                    text: "A queued instruction is not completed work".into(),
                    editable: false,
                    delivery: Default::default(),
                    attachments: vec![],
                }]
            } else {
                vec![]
            };
        });
        assert!(!rendered(&runtime, &mut tree).contains(&label));
        assert_eq!(runtime.finished_stamp, Some(stamp.clone()));
    }
    install(&mut runtime, |snapshot| snapshot.queued_messages.clear());
    assert!(rendered(&runtime, &mut tree).contains(&label));
    runtime.visible.store(false, Ordering::Relaxed);
    install(&mut runtime, |_| {});
    assert!(!rendered(&runtime, &mut tree).contains(&label));
    runtime.visible.store(true, Ordering::Relaxed);
    install(&mut runtime, |_| {});
    assert!(rendered(&runtime, &mut tree).contains(&label));
    install(&mut runtime, |snapshot| snapshot.finished_at = None);
    assert!(runtime.finished_stamp.is_none());
    assert!(!rendered(&runtime, &mut tree).contains(&label));
}
