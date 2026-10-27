//! Exact-worker reservations fence attachment while automatic cleanup runs.
use super::*;
use std::collections::HashSet;

enum RetentionVisit {
    Handoff,
    Terminal,
}

pub(in crate::host) struct Retirement {
    pub(in crate::host) handle: TerminalHandle,
    pub(in crate::host) ticket: uuid::Uuid,
    pub(in crate::host) failure: Option<String>,
    // An explicit close owns its response and final compaction. Automatic
    // maintenance must not consume that request's exact ticket underneath it.
    pub(in crate::host) explicit_close: bool,
}

impl Retirement {
    pub(in crate::host) fn submitting(handle: TerminalHandle) -> Self {
        Self {
            handle,
            ticket: uuid::Uuid::new_v4(),
            failure: None,
            explicit_close: false,
        }
    }

    pub(in crate::host) fn explicit_close(handle: TerminalHandle) -> Self {
        Self {
            explicit_close: true,
            ..Self::submitting(handle)
        }
    }
}

impl HostState {
    pub(in crate::host) fn ensure_retention_available(
        &self,
        ids: &[TerminalId],
    ) -> Result<(), ProtocolError> {
        for id in ids {
            if self.terminal_retirements.get(id).is_some_and(|entry| {
                entry.failure.is_none()
                    && self
                        .terminals
                        .get(id)
                        .is_some_and(|record| record.handle.same_worker(&entry.handle))
            }) {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::TerminalUnavailable,
                    format!("Terminal {id} is completing retirement; retry when cleanup settles"),
                    true,
                ));
            }
        }
        Ok(())
    }

    pub(in crate::host) fn ensure_view_retention_available(
        &self,
        edit: &ModelEdit,
    ) -> Result<(), ProtocolError> {
        if self.terminal_retirements.is_empty() {
            return Ok(());
        }
        let panes: &[crate::workspace_model::views::Pane] = match edit {
            ModelEdit::SeedView { panes, .. } => panes,
            ModelEdit::ChangeView { change, .. } => match change {
                crate::workspace_model::views::Change::Insert { pane, .. }
                | crate::workspace_model::views::Change::InsertIfEmpty { pane }
                | crate::workspace_model::views::Change::Update { pane, .. } => {
                    std::slice::from_ref(pane)
                }
                _ => &[],
            },
            _ => &[],
        };
        for pane in panes {
            match &pane.kind {
                crate::model::CardKind::Terminal(session) => {
                    self.ensure_retention_available(&[session.id])?
                }
                crate::model::CardKind::Chat(session) => {
                    self.ensure_retention_available(&[session.id])?;
                    if let Some(terminal) = &session.terminal {
                        self.ensure_retention_available(&[terminal.id])?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(in crate::host) fn retire_terminal_if_current(
        &mut self,
        id: TerminalId,
        expected: &TerminalHandle,
    ) -> Option<(TerminalRecord, Option<Retirement>)> {
        if self
            .terminals
            .get(&id)
            .is_some_and(|record| record.handle.same_worker(expected))
        {
            let record = self.retire_terminal(id)?;
            let reservation = if self
                .terminal_retirements
                .get(&id)
                .is_some_and(|entry| entry.handle.same_worker(expected))
            {
                self.terminal_retirements.remove(&id)
            } else {
                None
            };
            Some((record, reservation))
        } else {
            None
        }
    }

    pub(in crate::host) fn observe_completed_terminals(&mut self, now: Instant) {
        for record in self.terminals.values_mut() {
            if record.handle.is_completed() {
                record.completed_at.get_or_insert(now);
            }
        }
    }

    fn retention_candidate(
        &self,
        id: TerminalId,
        record: &TerminalRecord,
        replacements: &HashSet<TerminalId>,
    ) -> bool {
        !self.shutting_down
            && !self.shutdown_requested
            && self.upgrade.is_none()
            && record.handle.is_completed()
            // A failed startup still owns its original recovery slot. Keep the
            // record reachable until exact checked Close settles that custody.
            && !self.startup_custody.contains(&id)
            && !self.terminal_retirements.contains_key(&id)
            && self.subscriptions.clients(id).is_empty()
            && !self.chats.contains_key(&id)
            && !self.chat_handoffs.contains_key(&id)
            // Return-to-terminal receipts are keyed by the original chat,
            // while their replacement has its own terminal identity.
            && !replacements.contains(&id)
            && !self.chat_spawning.contains_key(&id)
            && !self.chat_bindings.contains_key(&id)
            && !self.native_closing.contains_key(&id)
            && !self.native_actor_startups.contains_key(&id)
            && self.native_adoption_failures.is_empty()
    }

    fn retention_candidates<'a>(
        &'a self,
        pinned: &'a BTreeSet<TerminalId>,
    ) -> impl Iterator<Item = (&'a TerminalId, &'a TerminalRecord)> + 'a {
        self.retention_candidates_observed(pinned, |_| {})
    }

    fn retention_candidates_observed<'a>(
        &'a self,
        pinned: &'a BTreeSet<TerminalId>,
        mut visit: impl FnMut(RetentionVisit) + 'a,
    ) -> impl Iterator<Item = (&'a TerminalId, &'a TerminalRecord)> + 'a {
        // Several deadline/expiry traversals run under HostState's lock. Do not
        // rescan every retained handoff for each unrelated completed terminal.
        // This membership belongs only to this borrow, so new/removed receipts
        // are observed by the next traversal without a persistent cache.
        let replacements = self
            .chat_handoffs
            .values()
            .map(|receipt| {
                visit(RetentionVisit::Handoff);
                receipt.replacement().id
            })
            .collect::<HashSet<_>>();
        self.terminals.iter().filter(move |(id, record)| {
            visit(RetentionVisit::Terminal);
            !pinned.contains(id) && self.retention_candidate(**id, record, &replacements)
        })
    }

    pub(in crate::host) fn terminal_retention_deadline(
        &self,
        retention: Duration,
        pinned: &BTreeSet<TerminalId>,
    ) -> Option<Instant> {
        self.retention_candidates(pinned)
            .filter_map(|(_, record)| record.completed_at?.checked_add(retention))
            .min()
    }

    pub(in crate::host) fn expired_terminal_handles(
        &self,
        now: Instant,
        retention: Duration,
        pinned: &BTreeSet<TerminalId>,
    ) -> Vec<(TerminalId, TerminalHandle)> {
        self.retention_candidates(pinned)
            .filter(|(_, record)| {
                record
                    .completed_at
                    .and_then(|at| at.checked_add(retention))
                    .is_some_and(|at| at <= now)
            })
            .map(|(id, record)| (*id, record.handle.clone()))
            .collect()
    }

    pub(in crate::host) fn development_idle(&self) -> bool {
        self.clients.is_empty()
            && self.terminals.is_empty()
            && self.terminal_retirements.is_empty()
            && self.spawning.is_empty()
            && self.browser_logins_spawning.is_empty()
            && self.chat_spawning.is_empty()
            && self.chat_bindings.is_empty()
            && self.execution_startups.is_empty()
            && self.native_closing.is_empty()
            && self.chat_handoffs.is_empty()
            && self.native_actor_startups.is_empty()
            && self.native_adoption_failures.is_empty()
            && self.upgrade.is_none()
            && !self.shutting_down
            && !self.shutdown_requested
            && !self
                .chats
                .values()
                .any(|record| record.handle.owns_work() || record.returning_terminal)
            && self
                .account_lifecycle
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty()
            && self
                .worktree_removals
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty()
            && !crate::daemon::child_reaper::has_outstanding_ownership()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe};

    fn observed_candidates(
        state: &HostState,
        pinned: &BTreeSet<TerminalId>,
    ) -> BTreeSet<TerminalId> {
        let mut handoffs = 0;
        let mut terminals = 0;
        let ids = state
            .retention_candidates_observed(pinned, |visit| match visit {
                RetentionVisit::Handoff => handoffs += 1,
                RetentionVisit::Terminal => terminals += 1,
            })
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(handoffs, state.chat_handoffs.len());
        assert_eq!(terminals, state.terminals.len());
        ids
    }

    #[test]
    fn retention_traversal_projects_each_handoff_once_and_preserves_original_exclusions() {
        let directory = tempfile::tempdir().unwrap();
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("retention-traversal-test"),
            boot_id: BootId::new("retention-traversal-test"),
            host_incarnation: HostIncarnation::new("retention-traversal-test"),
        });
        state.register_client(1);
        let mut sessions = Vec::new();
        let mut handles = Vec::new();
        for _ in 0..3 {
            let recipe = LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec!["-c".into(), "exit 0".into()],
                working_directory: directory.path().to_owned(),
                environment: BTreeMap::new(),
                unset_environment: Default::default(),
            };
            let session = TerminalSession {
                id: TerminalId::new(),
                launch: recipe.clone(),
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe,
                    last_exit: None,
                },
            };
            let handle =
                TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
            handle.shutdown().unwrap();
            assert!(handle.is_completed());
            state.reserve_spawn(session.id).unwrap();
            state
                .finish_spawn(1, session.clone(), handle.clone(), false)
                .unwrap();
            sessions.push(session);
            handles.push(handle);
        }
        drop(state.unregister_client(1));
        let completed = Instant::now();
        state.observe_completed_terminals(completed);
        let retention = Duration::from_secs(10);
        let deadline = completed + retention;
        let empty = BTreeSet::new();
        let all = sessions
            .iter()
            .map(|session| session.id)
            .collect::<BTreeSet<_>>();
        let handoff = |chat_id, replacement: &TerminalSession| {
            crate::host::chat::TerminalHandoff::new(
                crate::chat::Session {
                    id: chat_id,
                    launch: crate::chat::Launch {
                        provider: crate::agent::Provider::Claude,
                        recipe: replacement.launch.clone(),
                        session_id: Some("retention-policy-metadata".into()),
                    },
                    proof: None,
                    terminal: None,
                    owned_codex_listener: None,
                    native_continuity: Default::default(),
                },
                replacement.clone(),
                1,
                BTreeMap::new(),
                deadline,
            )
        };
        // These are retained transition metadata, not extra providers or PTYs.
        // Their replacement IDs are initially unrelated to all three workers.
        for _ in 0..48 {
            let chat_id = TerminalId::new();
            let mut replacement = sessions[0].clone();
            replacement.id = TerminalId::new();
            state
                .chat_handoffs
                .insert(chat_id, handoff(chat_id, &replacement));
        }
        assert_eq!(observed_candidates(&state, &empty), all);
        assert_eq!(
            state.terminal_retention_deadline(retention, &empty),
            Some(deadline)
        );
        assert!(
            state
                .expired_terminal_handles(completed, retention, &empty)
                .is_empty()
        );
        let expired = state.expired_terminal_handles(deadline, retention, &empty);
        assert_eq!(
            expired.iter().map(|(id, _)| *id).collect::<BTreeSet<_>>(),
            all
        );
        for (id, handle) in expired {
            assert!(handle.same_worker(&state.terminals[&id].handle));
        }

        let replacement = sessions[2].id;
        let mut expected = all.clone();
        expected.remove(&replacement);
        for _ in 0..2 {
            let chat_id = TerminalId::new();
            state
                .chat_handoffs
                .insert(chat_id, handoff(chat_id, &sessions[2]));
        }
        assert_eq!(observed_candidates(&state, &empty), expected);
        assert_eq!(
            state
                .expired_terminal_handles(deadline, retention, &empty)
                .iter()
                .map(|(id, _)| *id)
                .collect::<BTreeSet<_>>(),
            expected
        );
        assert_eq!(
            observed_candidates(&state, &BTreeSet::from([replacement])),
            expected
        );
        let original = sessions[1].id;
        let mut unrelated = sessions[0].clone();
        unrelated.id = TerminalId::new();
        state
            .chat_handoffs
            .insert(original, handoff(original, &unrelated));
        expected.remove(&original);
        assert_eq!(observed_candidates(&state, &empty), expected);
        let only = sessions[0].id;
        assert_eq!(
            observed_candidates(&state, &BTreeSet::from([only])),
            BTreeSet::new()
        );
        let pinned = BTreeSet::from([only]);
        assert_eq!(state.terminal_retention_deadline(retention, &pinned), None);
        assert!(
            state
                .expired_terminal_handles(deadline, retention, &pinned)
                .is_empty()
        );
        state.startup_custody.insert(only);
        assert!(observed_candidates(&state, &empty).is_empty());
        state.startup_custody.remove(&only);
        state
            .subscriptions
            .attach(2, only, TerminalVisibility::Hidden);
        assert!(observed_candidates(&state, &empty).is_empty());
        state.subscriptions.detach(2, only);
        state
            .terminal_retirements
            .insert(only, Retirement::submitting(handles[0].clone()));
        assert!(observed_candidates(&state, &empty).is_empty());
        state.terminal_retirements.remove(&only);
        assert_eq!(observed_candidates(&state, &empty), expected);
        state.shutting_down = true;
        assert!(observed_candidates(&state, &empty).is_empty());
        state.shutting_down = false;
        state.shutdown_requested = true;
        assert!(observed_candidates(&state, &empty).is_empty());
        state.shutdown_requested = false;

        // No retained cache may keep a removed receipt's replacement excluded.
        state.chat_handoffs.clear();
        assert_eq!(observed_candidates(&state, &empty), all);
        assert_eq!(
            state.terminal_retention_deadline(retention, &empty),
            Some(deadline)
        );
        assert_eq!(state.terminals.len(), 3);
        for (session, handle) in sessions.iter().zip(&handles) {
            assert!(state.terminals[&session.id].handle.same_worker(handle));
        }
    }
}
