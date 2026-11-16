//! The UI edits its durable state while the session writer retains a snapshot.
use super::*;
use std::{sync::Arc, time::Instant};

fn workspace(count: usize) -> WorkspaceState {
    let mut workspace = WorkspaceState::default();
    workspace.cards = (0..count)
        .map(|index| CardState {
            id: CardId::new(),
            generation: 0,
            title: format!("Investigate retained conversation {index}"),
            width_fraction: 0.5,
            existence: Existence::Open,
            kind: CardKind::Chat(crate::chat::Session {
                id: TerminalId::new(),
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Claude,
                    recipe: LaunchRecipe {
                        program: "claude".into(),
                        arguments: vec!["--model".into(), "haiku".into()],
                        working_directory: format!("/work/project/worktree-{index}").into(),
                        environment: (0..16)
                            .map(|key| {
                                (
                                    format!("RUNTIME_{key}"),
                                    format!("/home/user/.local/runtime/{index}/{key}"),
                                )
                            })
                            .collect(),
                        unset_environment: Default::default(),
                    },
                    session_id: Some(uuid::Uuid::new_v4().to_string()),
                },
                native_continuity: Default::default(),
                owned_codex_listener: None,
                proof: None,
                terminal: None,
            }),
        })
        .collect();
    workspace
}

#[test]
fn an_edit_during_a_save_preserves_unedited_card_backing() {
    let saved = Arc::new(workspace(4));
    let mut current = saved.clone();
    Arc::make_mut(&mut current).focused = Some(saved.cards[0].id);
    assert_eq!(
        current.cards[1].title.as_ptr(),
        saved.cards[1].title.as_ptr(),
        "an unrelated durable edit deep-copied a card the writer still holds"
    );
    Arc::make_mut(&mut current).cards[0]
        .title
        .push_str(" updated");
    assert_ne!(current.cards[0].title, saved.cards[0].title);
    assert_eq!(
        current.cards[1].title.as_ptr(),
        saved.cards[1].title.as_ptr()
    );
    assert_eq!(saved.focused, None);
}

#[test]
#[ignore = "fw16 retained-save copy-on-write timing workload"]
fn retained_save_edit_cost() {
    const CARDS: usize = 1760;
    const SAMPLES: usize = 50;
    let mut current = Arc::new(workspace(CARDS));
    for card_edit in [false, true] {
        let mut samples = Vec::with_capacity(SAMPLES);
        for sample in 0..SAMPLES {
            let saved = current.clone();
            let started = Instant::now();
            let edited = Arc::make_mut(&mut current);
            if card_edit {
                edited.cards[sample % CARDS].width_fraction = 0.4 + sample as f32 / 1000.;
            } else {
                edited.focused = Some(edited.cards[sample % CARDS].id);
            }
            samples.push(started.elapsed().as_secs_f64() * 1000.);
            std::hint::black_box(&current);
            drop(saved);
        }
        samples.sort_by(f64::total_cmp);
        eprintln!(
            "retained_save_edit cards={CARDS} card_edit={card_edit} samples={SAMPLES} median_ms={:.3} p95_ms={:.3} max_ms={:.3}",
            samples[SAMPLES / 2],
            samples[SAMPLES * 95 / 100],
            samples[SAMPLES - 1]
        );
    }
}
