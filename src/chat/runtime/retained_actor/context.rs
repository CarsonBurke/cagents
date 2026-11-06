//! Compatibility projection for rows reduced by an older retained decoder.
//! Execution, the helper cache, and durable final receipts stay authoritative.
use super::*;
use crate::chat::{presentation::ItemChange, protocol::claude_cached_context::CachedContext};
use crate::terminal_ipc::ServerResponse;

#[derive(Default)]
pub(super) struct ContextProjection {
    context: CachedContext,
    session: Option<String>,
    latest_generation: u64,
    // Every subscriber at or behind this helper generation needs a full resync.
    // A consumed dirty bit would repair only the first subscriber to read.
    invalidated_through: Option<u64>,
    #[cfg(test)]
    worker_waiting: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_provenance_resyncs_all_readers_and_context_appends_use_full_windows() {
        let (_root, mut remote, mut snapshot, path) = super::super::tests::legacy_context_fixture();
        let notices = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = notices.clone();
        Arc::get_mut(&mut remote.0).unwrap().notify = Arc::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
        });
        let raw = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        snapshot.generation = 7;
        let initial = remote.project_snapshot(snapshot.clone());
        assert_eq!(initial.items[0].role, chat::Role::User);
        let mut scanner = CachedContext::default();
        let mut session = None;
        remote.scan_context(&mut scanner, &mut session);
        let before = notices.load(Ordering::Relaxed);
        std::fs::write(&path, raw).unwrap();
        remote.scan_context(&mut scanner, &mut session);
        assert_eq!(
            notices.load(Ordering::Relaxed),
            before + 1,
            "idle subscribers get a correction wake"
        );
        assert!(
            remote.context_invalidates(Some(7)),
            "cover the served RPC generation, not just resident metadata"
        );
        let id = remote.0.source.id;
        let listener = UnixListener::bind(&remote.0.socket).unwrap();
        let mut full = snapshot.clone();
        full.generation = 8;
        let mut update = chat::presentation::Presentation {
            base_generation: 8,
            file_changes_unchanged: false,
            metadata: full.clone(),
            order: full.items.iter().map(|item| item.id.clone()).collect(),
            changes: vec![ItemChange::Append {
                item: Arc::unwrap_or_clone(full.items[0].clone()),
                offset: 0,
                suffix: String::new(),
            }],
            children: Vec::new(),
        };
        update.metadata.generation = 9;
        let mut last = full.clone();
        last.generation = 9;
        let expected = last.clone();
        let serving = thread::spawn(move || {
            let full_reply = |snapshot: Snapshot| {
                serde_json::to_value(Some(ServerResponse::ChatSnapshot {
                    chat_id: id,
                    snapshot: Some(snapshot),
                }))
                .unwrap()
            };
            let calls = [
                ("presentation", json!([id, null]), full_reply(full.clone())),
                ("presentation", json!([id, null]), full_reply(full)),
                (
                    "presentation",
                    json!([id, 8]),
                    serde_json::to_value(Some(ServerResponse::ChatUpdate {
                        chat_id: id,
                        update,
                    }))
                    .unwrap(),
                ),
                (
                    "snapshot",
                    json!([null]),
                    serde_json::to_value(Some(last)).unwrap(),
                ),
            ];
            for (method, args, response) in calls {
                let (stream, _) = listener.accept().unwrap();
                let request: Invocation = receive(&stream, TIMEOUT).unwrap();
                assert_eq!((request.method.as_str(), request.args), (method, args));
                send(&stream, &Ok::<_, String>(response), TIMEOUT).unwrap();
            }
        });
        for after in [7, 7, 8] {
            let Some(ServerResponse::ChatSnapshot {
                snapshot: Some(corrected),
                ..
            }) = remote.presentation(id, Some(after)).unwrap()
            else {
                panic!("requires full resync")
            };
            assert_eq!(corrected.items[0].role, chat::Role::System);
            assert_eq!(corrected.items[1], snapshot.items[1]);
            assert_eq!(corrected.text_copy_sources, expected.text_copy_sources);
        }
        serving.join().unwrap();
        assert!(
            remote.context_invalidates(Some(7)),
            "another subscriber still behind after the helper advanced"
        );
        {
            let mut state = remote.0.state.lock().unwrap();
            let state = Arc::make_mut(&mut state);
            state.confirmed_session = None;
            state.session_id = None;
        }
        // The saved launch ID remains present; it cannot resurrect proofs.
        remote.scan_context(&mut scanner, &mut session);
        assert_eq!(
            remote.project_snapshot(snapshot).items[0].role,
            chat::Role::User
        );
    }
}

impl ContextProjection {
    fn invalidates(&self, after: Option<u64>) -> bool {
        after.is_some_and(|after| self.invalidated_through.is_some_and(|last| after <= last))
    }

    fn snapshot(&mut self, snapshot: &mut Snapshot) {
        self.latest_generation = self.latest_generation.max(snapshot.generation);
        if self.session.is_some() && self.session == snapshot.session_id {
            self.context.classify_items(&mut snapshot.items);
        }
    }
}

impl RemoteActor {
    /// Scanning never holds the publication lock or runs on the receipt pump.
    /// Only a changed proof index is copied across, with no transcript bodies.
    pub(super) fn scan_context(&self, scanner: &mut CachedContext, previous: &mut Option<String>) {
        let state = self.state();
        let finalized = self
            .0
            .final_snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        // Do not resurrect the saved launch ID after a provider reset.
        // A verified final snapshot can be newer than the last resident poll.
        let session = finalized.as_ref().map_or_else(
            || {
                state
                    .confirmed_session
                    .clone()
                    .or_else(|| state.session_id.clone())
            },
            |snapshot| snapshot.session_id.clone(),
        );
        let changed = match (&session, &self.0.launch_account_scope) {
            (Some(session), Ok(scope)) => scanner.refresh(&self.0.source.launch, scope, session),
            _ => {
                *scanner = CachedContext::default();
                previous.is_some()
            }
        };
        if changed || *previous != session {
            let proof = scanner.projection();
            {
                let mut projection = self.0.context.lock().unwrap_or_else(|p| p.into_inner());
                projection.context = proof;
                projection.session = session.clone();
                projection.latest_generation = projection.latest_generation.max(
                    finalized
                        .as_ref()
                        .map_or(state.generation, |snapshot| snapshot.generation),
                );
                projection.invalidated_through = Some(projection.latest_generation);
            }
            // All subscribers need this wake, including idle readers whose
            // helper generation did not change while raw provenance arrived.
            (self.0.notify)(Update::Presentation);
        }
        *previous = session;
    }

    pub(super) fn start_context_worker(&self) -> Result<(), String> {
        let weak = Arc::downgrade(&self.0);
        let worker = thread::Builder::new()
            .name("cagents-context-proxy".into())
            .spawn(move || {
                let mut scanner = CachedContext::default();
                let mut session = None;
                loop {
                    let Some(inner) = weak.upgrade() else { break };
                    // A verified final publication can outlive the helper and
                    // its last JSONL flush. Keep correcting that read-only view
                    // until its last owner drops; never restart the receipt pump.
                    if inner.stop.load(Ordering::Acquire)
                        && inner
                            .final_snapshot
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .is_none()
                    {
                        #[cfg(test)]
                        {
                            inner.context.lock().unwrap().worker_waiting = true;
                        }
                        // An unavailable proxy can later recover verified final
                        // evidence. Wait without scanning, retaining only Weak
                        // ownership so the final view can resume this worker.
                        drop(inner);
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                    let actor = Self(inner);
                    actor.scan_context(&mut scanner, &mut session);
                    drop(actor);
                    thread::sleep(Duration::from_secs(1));
                }
            })
            .map_err(|error| error.to_string())?;
        *self
            .0
            .context_worker
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(worker);
        Ok(())
    }

    pub(super) fn context_invalidates(&self, after: Option<u64>) -> bool {
        self.0
            .context
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .invalidates(after)
    }

    pub(super) fn project_snapshot(&self, mut snapshot: Snapshot) -> Snapshot {
        self.0
            .context
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .snapshot(&mut snapshot);
        snapshot
    }

    pub(in crate::chat::runtime) fn presentation(
        &self,
        chat_id: crate::model::TerminalId,
        after: Option<u64>,
    ) -> Result<Option<ServerResponse>, String> {
        let final_snapshot = self
            .0
            .final_snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(snapshot) = final_snapshot {
            let snapshot = self.project_snapshot(snapshot.as_ref().clone());
            return Ok(Some(ServerResponse::ChatSnapshot {
                chat_id,
                snapshot: Some(snapshot),
            }));
        }
        let requested = if self.context_invalidates(after) {
            None
        } else {
            after
        };
        let mut response: Option<ServerResponse> =
            self.call("presentation", json!([chat_id, requested]))?;
        let mut projection = self.0.context.lock().unwrap_or_else(|p| p.into_inner());
        match &mut response {
            Some(ServerResponse::ChatSnapshot {
                snapshot: Some(snapshot),
                ..
            }) => {
                projection.snapshot(snapshot);
                return Ok(response);
            }
            Some(ServerResponse::ChatUpdate { update, .. }) => {
                projection.latest_generation =
                    projection.latest_generation.max(update.metadata.generation);
                // Delivery cannot be appended by the existing Text delta reducer.
                if !projection.invalidates(after)
                    && (projection.session != update.metadata.session_id
                        || !update
                            .changes
                            .iter()
                            .any(|change| projection.context.needs_full(change)))
                {
                    if projection.session.is_some()
                        && projection.session == update.metadata.session_id
                    {
                        let mut items: Vec<_> = update
                            .changes
                            .iter()
                            .filter_map(|change| match change {
                                ItemChange::Replace(item) => Some(Arc::new(item.clone())),
                                _ => None,
                            })
                            .collect();
                        projection.context.classify_items(&mut items);
                        let mut items = items.into_iter();
                        for change in &mut update.changes {
                            if let ItemChange::Replace(item) = change {
                                *item = Arc::unwrap_or_clone(items.next().unwrap());
                            }
                        }
                    }
                    return Ok(response);
                }
            }
            _ if !projection.invalidates(after) => return Ok(response),
            _ => {}
        }
        drop(projection);
        // Full same-generation snapshots are an existing resync boundary. Keep
        // the helper clock and aligned Copy authority rather than inventing one.
        let snapshot: Option<Snapshot> = self.call("snapshot", json!([null]))?;
        Ok(Some(ServerResponse::ChatSnapshot {
            chat_id,
            snapshot: snapshot.map(|snapshot| self.project_snapshot(snapshot)),
        }))
    }
}
