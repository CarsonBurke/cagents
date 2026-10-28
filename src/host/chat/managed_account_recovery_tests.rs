//! Saved recovery alongside actual managed terminal holders. Every parent
//! uses the existing private runtime and matched helper subprocess fixture.
use super::saved_account_retained_tests::RetainedFixture;
use super::saved_account_tests::THREAD;
use super::*;
use crate::model::{LaunchRecipe, TerminalContinuity, TerminalSession};

struct Shell {
    id: TerminalId,
    handle: crate::terminal::TerminalHandle,
}

impl Shell {
    fn start(retained: &RetainedFixture) -> Self {
        let fixture = &retained.fixture;
        let id = TerminalId::new();
        let launch = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "read -r line".into()],
            working_directory: fixture.source.launch.recipe.working_directory.clone(),
            environment: BTreeMap::from([
                (
                    "HOME".into(),
                    fixture.directory.path().to_str().unwrap().into(),
                ),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ]),
            unset_environment: Default::default(),
        };
        let session = TerminalSession {
            id,
            launch: launch.clone(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        };
        super::super::state::spawn_terminal(&fixture.host, 1, session, Default::default()).unwrap();
        let handle = fixture.host.lock().unwrap().terminals[&id].handle.clone();
        assert!(
            handle.holder_identity().is_some(),
            "the real retained holder is required"
        );
        assert!(!handle.is_completed());
        Self { id, handle }
    }

    fn finish(&self) {
        self.handle
            .shutdown_until(Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert!(self.handle.is_completed());
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        let _ = self
            .handle
            .shutdown_until(Instant::now() + Duration::from_secs(5));
    }
}

fn assert_no_candidate(retained: &RetainedFixture) {
    let fixture = &retained.fixture;
    assert!(
        !fixture
            .host
            .lock()
            .unwrap()
            .chats
            .contains_key(&fixture.replacement.id)
    );
    assert!(!fixture.starts.exists());
    assert_eq!(std::fs::read(&fixture.target_transcript).unwrap(), b"{}\n");
}

fn hint(retained: &RetainedFixture, thread: &str) -> String {
    let mut launch = retained.fixture.source.launch.recipe.clone();
    launch.arguments = vec!["--resume".into(), thread.into()];
    crate::model::agent_recovery::observed_hint_from_launch(&launch).unwrap()
}

#[test]
fn saved_recovery_preserves_an_exact_managed_shell_and_rechecks_its_worker() {
    if super::retained_resolution_tests::isolated().is_none() {
        return;
    }
    let retained = RetainedFixture::new();
    let shell = Shell::start(&retained);
    let proof = retained.fixture.host.lock().unwrap().proof().clone();
    let holder = shell.handle.holder_identity().unwrap().clone();
    let receipt = AccountRecoveryManagedTerminal::capture(
        &retained.fixture.host.lock().unwrap(),
        &holder,
        &proof,
    )
    .unwrap()
    .unwrap();

    retained.assert_recovered(retained.fixture.recover().unwrap());
    assert!(
        !shell.handle.is_completed(),
        "recovery must not terminate an unrelated managed shell"
    );
    receipt
        .revalidate(&retained.fixture.host.lock().unwrap(), &proof)
        .unwrap();

    // Reusing the same terminal ID cannot borrow the inspected worker's custody.
    let replacement = Shell::start(&retained);
    {
        let mut state = retained.fixture.host.lock().unwrap();
        state.terminals.get_mut(&shell.id).unwrap().handle = replacement.handle.clone();
        assert!(receipt.revalidate(&state, &proof).is_err());
        assert!(AccountRecoveryManagedTerminal::capture(&state, &holder, &proof).is_err());
        state.terminals.get_mut(&shell.id).unwrap().handle = shell.handle.clone();
    }
    replacement.finish();
    shell.finish();
}

#[test]
fn saved_recovery_refuses_untracked_and_replaced_terminal_holders_without_effects() {
    if super::retained_resolution_tests::isolated().is_none() {
        return;
    }
    for replaced in [false, true] {
        let retained = RetainedFixture::new();
        let shell = Shell::start(&retained);
        let replacement = replaced.then(|| Shell::start(&retained));
        let removed = {
            let mut state = retained.fixture.host.lock().unwrap();
            if let Some(replacement) = &replacement {
                state.terminals.get_mut(&shell.id).unwrap().handle = replacement.handle.clone();
                None
            } else {
                state.terminals.remove(&shell.id)
            }
        };
        assert!(retained.fixture.recover().is_err());
        assert_no_candidate(&retained);
        assert!(!shell.handle.is_completed());
        if let Some(replacement) = &replacement {
            retained
                .fixture
                .host
                .lock()
                .unwrap()
                .terminals
                .get_mut(&shell.id)
                .unwrap()
                .handle = shell.handle.clone();
            replacement.finish();
        }
        drop(removed);
        shell.finish();
    }
}

#[test]
fn saved_recovery_keeps_known_launch_and_hint_conflicts_with_a_managed_shell() {
    if super::retained_resolution_tests::isolated().is_none() {
        return;
    }
    for conflicting_hint in [false, true] {
        let retained = RetainedFixture::new();
        let shell = Shell::start(&retained);
        {
            let mut state = retained.fixture.host.lock().unwrap();
            let terminal = state.terminals.get_mut(&shell.id).unwrap();
            if conflicting_hint {
                terminal.session.resume_hint = Some(hint(&retained, THREAD));
            } else {
                terminal
                    .session
                    .launch
                    .arguments
                    .extend(["--resume".into(), THREAD.into()]);
                terminal.session.resume_hint =
                    Some(hint(&retained, "98765432-1234-4234-8234-123456789abc"));
            }
            assert!(
                saved_account_admission(
                    &state,
                    &retained.fixture.source,
                    &retained.fixture.replacement,
                    &[retained.fixture.source.id],
                )
                .is_err(),
                "a different hint cannot erase a known matching launch"
            );
        }
        assert!(retained.fixture.recover().is_err());
        assert_no_candidate(&retained);
        assert!(!shell.handle.is_completed());
        shell.finish();
    }
}
