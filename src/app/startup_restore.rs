//! Exact initial panes retain attachment-only intent until their first result.
//! Finishing the window's startup must not change an already-issued request.
use super::*;

#[derive(Default)]
pub(super) struct InitialPanes {
    captured: bool,
    recovery: bool,
    views_captured: bool,
    original_cards: HashSet<CardId>,
    retired: HashSet<CardId>,
    pending_views: HashSet<crate::workspace_model::views::Scope>,
    panes: HashMap<CardId, InitialPane>,
}

struct InitialPane {
    session: TerminalId,
    settled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_verdict_ends_global_admission_without_releasing_delayed_initial_panes() {
        const CHILD: &str = "CAGENTS_TEST_INITIAL_RESTORE_FINISH";
        if let Ok(case) = std::env::var(CHILD) {
            instance::initialize_window_restore_policy(instance::WindowRestorePolicy::ExistingOnly)
                .unwrap();
            let mut app = crate::app::tests::context_inspector_test_app(case == "failure");
            app.startup_gate
                .initial_panes
                .capture(instance::window_launch_restore_policy(), &app.durable.cards);
            let initial = app.durable.cards.first().and_then(|card| match &card.kind {
                CardKind::Terminal(session) => Some((card.id, session.id)),
                _ => None,
            });
            assert_eq!(
                instance::window_restore_policy(),
                instance::WindowRestorePolicy::ExistingOnly
            );
            match case.as_str() {
                "success" => {
                    let saved = SettingsSaved {
                        generation: 0,
                        path: "/fixture/settings.toml".into(),
                        settings: app.settings.clone(),
                    };
                    drop(app.startup_panes_saved(Ok(saved), Ok(())));
                    assert!(app.startup_gate.fixture_verdict().2);
                }
                "failure" => {
                    app.startup_gate = startup_gate::State::fixture_opened();
                    app.startup_gate
                        .initial_panes
                        .capture(instance::window_launch_restore_policy(), &app.durable.cards);
                    let (card, _) = initial.unwrap();
                    app.cards.insert(
                        card,
                        CardRuntime::Failed("the exact source is unavailable".into()),
                    );
                    drop(app.verify_startup_gate());
                    assert!(app.startup_gate.fixture_verdict().2);
                    assert!(
                        app.startup_gate
                            .initial_panes
                            .existing_only(card, initial.unwrap().1)
                    );
                }
                "no-handshake" => {
                    drop(app.begin_startup_gate());
                }
                _ => unreachable!(),
            }
            assert_eq!(
                instance::window_restore_policy(),
                instance::WindowRestorePolicy::Ordinary
            );
            assert_eq!(
                instance::window_launch_restore_policy(),
                instance::WindowRestorePolicy::ExistingOnly
            );
            // An empty cache did not invent a completed shared startup census.
            // The original scope's first authoritative pane can arrive later.
            let scope =
                crate::workspace_model::views::Scope::Channel(crate::model::ChannelId::new());
            let mut late = crate::app::tests::terminal_card_state();
            let terminal = match &late.kind {
                CardKind::Terminal(session) => session.id,
                _ => unreachable!(),
            };
            let mut tracker = InitialPanes::default();
            tracker.capture(instance::window_launch_restore_policy(), &[]);
            tracker.capture_views(vec![scope.clone()]);
            let mut view = crate::workspace_model::views::View::empty(scope.clone());
            view.panes
                .push(crate::workspace_model::views::Pane::from_card(&late, None).unwrap());
            let mut user_created = late.clone();
            user_created.id = CardId::new();
            view.panes
                .push(crate::workspace_model::views::Pane::from_card(&user_created, None).unwrap());
            tracker.observe_initial_view(&view, &[user_created.clone()]);
            assert!(tracker.existing_only(late.id, terminal));
            assert!(
                !tracker.existing_only(user_created.id, terminal),
                "new user panes in the initial echo remain ordinary"
            );
            // A first authoritative snapshot can replace the cached source
            // session with the update's issued successor under the same card.
            let mut cached = late.clone();
            let CardKind::Terminal(source) = &mut cached.kind else {
                unreachable!()
            };
            source.id = TerminalId::new();
            let cached_terminal = source.id;
            let mut replaced = InitialPanes::default();
            replaced.capture(instance::window_launch_restore_policy(), [&cached]);
            replaced.capture_views(vec![scope.clone()]);
            replaced.observe_initial_view(&view, [&cached]);
            assert!(replaced.existing_only(late.id, terminal));
            assert!(!replaced.existing_only(late.id, cached_terminal));
            replaced.settled(late.id, cached_terminal);
            assert!(
                replaced.existing_only(late.id, terminal),
                "an old cached attempt cannot settle the issued successor"
            );
            tracker.settled(late.id, terminal);
            tracker.capture_views(vec![scope]);
            tracker.observe_initial_view(&view, &[]);
            assert!(
                !tracker.existing_only(late.id, terminal),
                "later model echoes cannot rearm startup"
            );
            late.id = CardId::new();
            assert!(!tracker.existing_only(late.id, terminal));
            return;
        }
        let root = tempfile::tempdir().unwrap();
        for leaf in ["home", "runtime", "config", "data", "cache"] {
            std::fs::create_dir(root.path().join(leaf)).unwrap();
        }
        let name = std::thread::current().name().unwrap().to_owned();
        for case in ["success", "failure", "no-handshake"] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", &name, "--nocapture"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", root.path().join("home"))
                .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_CACHE_HOME", root.path().join("cache"))
                .env(CHILD, case);
            if let Some(authority) = std::env::var_os(crate::agent_execution::AUTHORITY_ENV) {
                command.env(crate::agent_execution::AUTHORITY_ENV, authority);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{case}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn initial_terminal_attachment_requires_exact_inventory_or_issued_lineage_without_a_spawn_recipe()
     {
        let card = crate::app::tests::terminal_card_state();
        let CardKind::Terminal(source) = card.kind else {
            unreachable!()
        };
        let proof = HostSessionProof {
            host_session_id: "host".into(),
            boot_id: "boot".into(),
            host_incarnation: "replacement".into(),
        };
        assert!(
            initial_terminal_attachment(&source, &[], &proof)
                .unwrap()
                .is_none()
        );
        let mut current = crate::terminal::ipc::TerminalDescriptor::relaunchable(
            source.id,
            source.launch.clone(),
            source.transcript_path.clone(),
        );
        current.session = source.clone();
        current.session.continuity = TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        assert_eq!(
            initial_terminal_attachment(&source, &[current.clone()], &proof).unwrap(),
            Some(current.session.clone())
        );
        current.session.id = TerminalId::new();
        assert!(
            initial_terminal_attachment(&source, &[current.clone()], &proof)
                .unwrap()
                .is_none()
        );
        current.upgrade_sources.push(source.clone());
        assert_eq!(
            initial_terminal_attachment(&source, &[current.clone()], &proof).unwrap(),
            Some(current.session.clone())
        );
        let mut foreign = source.clone();
        foreign.launch.arguments.push("foreign".into());
        assert!(
            initial_terminal_attachment(&foreign, &[current.clone()], &proof)
                .unwrap()
                .is_none()
        );
        assert!(initial_terminal_attachment(&source, &[current.clone(), current], &proof).is_err());
    }
}

impl InitialPanes {
    pub(super) fn capture<'a>(
        &mut self,
        policy: instance::WindowRestorePolicy,
        cards: impl IntoIterator<Item = &'a CardState>,
    ) {
        if std::mem::replace(&mut self.captured, true)
            || policy != instance::WindowRestorePolicy::ExistingOnly
        {
            return;
        }
        self.recovery = true;
        for card in cards {
            self.original_cards.insert(card.id);
            if !card.existence.is_open() {
                continue;
            }
            let session = match &card.kind {
                CardKind::Chat(session) if !session.is_unstarted() => session.id,
                CardKind::Terminal(session) if !terminal_ended(session) => session.id,
                _ => continue,
            };
            self.panes.insert(
                card.id,
                InitialPane {
                    session,
                    settled: false,
                },
            );
        }
    }

    pub(super) fn capture_views(&mut self, scopes: Vec<crate::workspace_model::views::Scope>) {
        if std::mem::replace(&mut self.views_captured, true) || !self.recovery {
            return;
        }
        self.pending_views.extend(scopes);
    }

    pub(super) fn observe_initial_view<'a>(
        &mut self,
        view: &crate::workspace_model::views::View,
        local: impl IntoIterator<Item = &'a CardState>,
    ) {
        if !self.pending_views.remove(&view.scope) {
            return;
        }
        // A local pane created after launch is a user action even if its
        // first registry echo arrives in this scope's initial snapshot.
        let new_local: HashSet<_> = local
            .into_iter()
            .filter(|card| !self.original_cards.contains(&card.id))
            .map(|card| card.id)
            .collect();
        for pane in &view.panes {
            if new_local.contains(&pane.id) || self.retired.contains(&pane.id) {
                continue;
            }
            let session = match &pane.kind {
                CardKind::Chat(session) if !session.is_unstarted() => session.id,
                CardKind::Terminal(session) if !terminal_ended(session) => session.id,
                _ => continue,
            };
            // The first authoritative view owns the initial descriptor. It
            // may name an issued successor to the cached session. Any attempt
            // already issued for the cache keeps its captured intent, and its
            // result cannot settle this new exact key.
            self.panes.insert(
                pane.id,
                InitialPane {
                    session,
                    settled: false,
                },
            );
        }
    }

    pub(super) fn existing_only(&self, card: CardId, session: TerminalId) -> bool {
        self.panes.get(&card).is_some_and(|initial| {
            initial.session == session
                && (!initial.settled
                    || instance::window_restore_policy()
                        == instance::WindowRestorePolicy::ExistingOnly)
        })
    }

    pub(super) fn settled(&mut self, card: CardId, session: TerminalId) {
        if let Some(initial) = self.panes.get_mut(&card)
            && initial.session == session
        {
            initial.settled = true;
            self.retired.insert(card);
        }
    }

    pub(super) fn explicit_action(&mut self, card: CardId) {
        self.panes.remove(&card);
        self.retired.insert(card);
    }

    pub(super) fn chat_intent(
        &mut self,
        card: CardId,
        session: TerminalId,
        intent: chat::Intent,
    ) -> chat::Intent {
        if matches!(
            intent,
            chat::Intent::Start | chat::Intent::Resume | chat::Intent::RetryStart
        ) {
            self.explicit_action(card);
            return intent;
        }
        let policy = if self.existing_only(card, session) {
            instance::WindowRestorePolicy::ExistingOnly
        } else {
            instance::WindowRestorePolicy::Ordinary
        };
        chat::client::restore_intent(intent, policy)
    }
}
