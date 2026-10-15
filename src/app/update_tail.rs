//! Conservative semantic generations for workspace projections.
//! Unknown messages invalidate all steps; output-only wakes and paint ticks
//! retain the generation. No raw runtime title or PTY output is an input.
#[derive(Clone, Copy, Debug)]
pub(super) enum Step {
    Chrome,
    Protections,
    Channels,
    WorktreePanes,
    TranscriptProbes,
}

#[derive(Default)]
pub(super) struct State {
    revisions: [u64; 5],
    applied: [Option<u64>; 5],
    #[cfg(test)]
    pub(super) runs: [u64; 5],
}
impl State {
    pub(super) fn changed(&mut self) {
        for revision in &mut self.revisions {
            *revision = revision.wrapping_add(1);
        }
    }
    pub(super) fn mark(&mut self, step: Step) {
        let revision = &mut self.revisions[step as usize];
        *revision = revision.wrapping_add(1);
    }
    pub(super) fn run(&mut self, step: Step) -> bool {
        let slot = step as usize;
        if self.applied[slot] == Some(self.revisions[slot]) {
            return false;
        }
        self.applied[slot] = Some(self.revisions[slot]);
        #[cfg(test)]
        {
            self.runs[slot] += 1;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_projection_skips_unchanged_inputs_and_reacts_to_generation() {
        let mut state = State::default();
        for step in [
            Step::Chrome,
            Step::Protections,
            Step::Channels,
            Step::WorktreePanes,
            Step::TranscriptProbes,
        ] {
            assert!(state.run(step));
            assert!(!state.run(step));
        }
        state.changed();
        for step in [
            Step::Chrome,
            Step::Protections,
            Step::Channels,
            Step::WorktreePanes,
            Step::TranscriptProbes,
        ] {
            assert!(state.run(step));
            assert!(!state.run(step));
        }
        assert_eq!(state.runs, [2; 5]);
    }
    #[test]
    fn terminal_activity_does_not_invalidate_unrelated_workspace_projections() {
        let mut state = State::default();
        for step in [
            Step::Chrome,
            Step::Protections,
            Step::Channels,
            Step::WorktreePanes,
            Step::TranscriptProbes,
        ] {
            assert!(state.run(step));
        }
        state.mark(Step::Channels);
        assert!(state.run(Step::Channels));
        for step in [
            Step::Chrome,
            Step::Protections,
            Step::WorktreePanes,
            Step::TranscriptProbes,
        ] {
            assert!(!state.run(step));
        }
        assert_eq!(state.runs, [1, 1, 2, 1, 1]);
    }

    #[test]
    fn a_noop_terminal_wake_does_not_run_any_workspace_tail_step() {
        use cosmic::Application as _;
        let mut app = super::super::tests::context_inspector_test_app(false);
        let before = app.update_tail.runs;
        drop(app.update(super::super::Message::TerminalWake {
            card: super::super::CardId::new(),
            generation: 0,
        }));
        assert_eq!(app.update_tail.runs, before);
    }

    #[test]
    fn reported_title_burst_mutates_durable_state_only_after_stability() {
        use super::super::*;
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let mut card = crate::app::tests::terminal_card_state();
        if let CardKind::Terminal(session) = &mut card.kind {
            session.launch.program = "codex".into();
        }
        let id = card.id;
        Arc::make_mut(&mut app.durable).cards.push(card);
        let original = app.durable.clone();
        for index in 0..100 {
            assert!(!app.apply_reported_title(id, format!("⠋ work {index}")));
        }
        assert!(Arc::ptr_eq(&original, &app.durable));
        assert_eq!(
            app.card_display_title(app.card_state(id).unwrap()),
            "work 99"
        );
        let changed_at = app.chrome_ui.reported_titles[&id].changed_at;
        drop(app.persist_stable_titles(
            changed_at + chrome_ui::TITLE_STABILITY - Duration::from_millis(1),
        ));
        assert!(Arc::ptr_eq(&original, &app.durable));
        drop(app.persist_stable_titles(changed_at + chrome_ui::TITLE_STABILITY));
        assert_eq!(app.card_state(id).unwrap().title, "work 99");
        let stable = app.durable.clone();
        drop(app.persist_stable_titles(changed_at + chrome_ui::TITLE_STABILITY * 2));
        assert!(Arc::ptr_eq(&stable, &app.durable));
    }
}
