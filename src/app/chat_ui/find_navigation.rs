//! One bounded Find intent, from rendered source matching to mounted geometry.
use super::*;
use std::sync::Weak;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IntentEpoch(u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DocumentEpoch(u64);
#[derive(Clone)]
pub(super) struct Intent {
    clock: Arc<AtomicU64>,
    epoch: IntentEpoch,
    document: Arc<AtomicU64>,
}
impl Intent {
    pub(super) fn current(&self) -> bool {
        self.clock.load(Ordering::Acquire) == self.epoch.0
    }
    pub(super) fn document_current(&self, epoch: DocumentEpoch) -> bool {
        self.current() && self.document.load(Ordering::Acquire) == epoch.0
    }
    pub(super) fn cancel(&self) {
        let _ = self.clock.compare_exchange(
            self.epoch.0,
            self.epoch.0.wrapping_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}
#[derive(Default)]
pub(super) struct State {
    clock: Arc<AtomicU64>,
    document: Arc<AtomicU64>,
    pending: Option<Pending>,
}
impl State {
    #[cfg(test)]
    pub(super) fn pending(&self) -> bool {
        self.pending.is_some()
    }
    #[cfg(test)]
    pub(super) fn correction_count(&self) -> Option<u8> {
        self.pending.as_ref().map(|pending| pending.passes)
    }
    #[cfg(test)]
    pub(super) fn current_hit(&self) -> Option<(&str, u64, usize)> {
        let pending = self.pending.as_ref()?;
        if pending.document != Some(self.document_epoch()) {
            return None;
        }
        pending
            .hit
            .as_ref()
            .map(|hit| (hit.item.as_str(), hit.identity, hit.byte))
    }
    #[cfg(test)]
    pub(super) fn diagnostic(&self) -> String {
        self.pending.as_ref().map_or_else(
            || "complete".into(),
            |pending| {
                format!(
                    "key={:?}, hit={:?}, in_flight={}, wait_for_shape={}, passes={}, previous={:?}",
                    pending.key,
                    pending
                        .hit
                        .as_ref()
                        .map(|hit| (&hit.item, hit.identity, hit.byte)),
                    pending.in_flight,
                    pending.wait_for_shape,
                    pending.passes,
                    pending.previous,
                )
            },
        )
    }
    pub(super) fn cancel(&mut self) {
        self.clock.fetch_add(1, Ordering::AcqRel);
        self.pending = None;
    }
    pub(super) fn document_epoch(&self) -> DocumentEpoch {
        DocumentEpoch(self.document.load(Ordering::Acquire))
    }
    pub(super) fn changed_document(&self) {
        self.document.fetch_add(1, Ordering::AcqRel);
    }
    pub(super) fn guard(&self) -> Intent {
        Intent {
            clock: self.clock.clone(),
            document: self.document.clone(),
            epoch: IntentEpoch(self.clock.load(Ordering::Acquire)),
        }
    }
}
impl Drop for State {
    fn drop(&mut self) {
        self.cancel();
    }
}
struct Pending {
    intent: Intent,
    context: Option<String>,
    query: String,
    key: Option<String>,
    sources: Vec<(String, Weak<chat::Item>)>,
    document: Option<DocumentEpoch>,
    hit: Option<Hit>,
    in_flight: bool,
    wait_for_shape: bool,
    passes: u8,
    previous: Option<(u8, f32)>,
    realized_pending: Option<(flow_layout::PendingGeometry, Hit)>,
    _deadline: iced::task::Handle,
}
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Hit {
    item: String,
    identity: u64,
    byte: usize,
}
fn items(runtime: &Runtime, key: &str) -> Vec<Arc<chat::Item>> {
    if runtime.selected_subagent.is_some() {
        return runtime.active_item(key).cloned().into_iter().collect();
    }
    runtime
        .turns
        .iter()
        .find(|turn| turn.key == key)
        .into_iter()
        .flat_map(|turn| {
            turn.prompt.iter().copied().chain(
                turn.steps
                    .iter()
                    .chain(&turn.prose)
                    .map(|placed| placed.index),
            )
        })
        .filter_map(|index| runtime.snapshot.items.get(index).cloned())
        .collect()
}
fn sources_match(runtime: &Runtime, pending: &Pending) -> bool {
    pending.sources.iter().all(|(id, source)| {
        runtime
            .active_item(id)
            .is_some_and(|item| source.as_ptr() == Arc::as_ptr(item))
    })
}
fn document(runtime: &Runtime) -> (DocumentEpoch, Arc<selection::Catalog>) {
    let (catalog, _) = runtime.selection_document();
    let epoch = runtime.find_navigation.document_epoch();
    (epoch, catalog)
}
fn later(target: Target, intent: Intent, geometry_changed: bool) -> Task<Message> {
    cosmic::task::future(async move {
        tokio::time::sleep(Duration::from_millis(16)).await;
        action(Action::FindResume(target, intent, geometry_changed))
    })
}
impl Cagents {
    pub(super) fn begin_find_navigation(
        &mut self,
        target: Target,
        key: Option<String>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        runtime.find_navigation.cancel();
        let intent = runtime.find_navigation.guard();
        let expiry = intent.clone();
        let (deadline, handle) = cosmic::task::future(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            action(Action::FindExpired(target, expiry))
        })
        .abortable();
        let anchor = key.as_ref().map(|key| runtime.find_anchor(key));
        runtime.find_navigation.pending = Some(Pending {
            intent: intent.clone(),
            context: runtime.selected_subagent.clone(),
            query: runtime.find.clone().unwrap_or_default(),
            key,
            sources: Vec::new(),
            document: None,
            hit: None,
            in_flight: false,
            wait_for_shape: false,
            passes: 0,
            previous: None,
            realized_pending: None,
            _deadline: handle.abort_on_drop(),
        });
        Task::batch([
            anchor.map_or_else(Task::none, |anchor| {
                scroll::find_anchor(target, anchor, intent.clone()).map(cosmic::Action::App)
            }),
            later(target, intent, false),
            deadline,
        ])
    }
    pub(super) fn resume_find_navigation(
        &mut self,
        target: Target,
        intent: Intent,
        geometry_changed: bool,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(mut pending) = runtime.find_navigation.pending.take() else {
            return Task::none();
        };
        if pending.intent.epoch != intent.epoch {
            runtime.find_navigation.pending = Some(pending);
            return Task::none();
        }
        if !intent.current()
            || !runtime.visible.load(Ordering::Relaxed)
            || runtime.selected_subagent != pending.context
            || runtime.find.as_ref() != Some(&pending.query)
        {
            runtime.find_navigation.cancel();
            return Task::none();
        }
        let Some(key) = &pending.key else {
            if runtime.find_matches.is_empty() {
                runtime.find_navigation.pending = Some(pending);
                return Task::none();
            }
            // A query entered before authoritative body hydration gets the
            // same disclosure/first-group behavior once that body arrives.
            runtime.find_index = runtime.find_matches.len().saturating_sub(1);
            runtime.find_navigation.pending = Some(pending);
            return self.chat_find_next(target, true);
        };
        let current = items(runtime, key);
        if current.is_empty() {
            runtime.find_navigation.cancel();
            return Task::none();
        }
        if pending.sources.is_empty() || !sources_match(runtime, &pending) {
            pending.sources = current
                .iter()
                .map(|item| (item.id.clone(), Arc::downgrade(item)))
                .collect();
            pending.hit = None;
            pending.document = None;
            pending.in_flight = false;
            pending.wait_for_shape = false;
        }
        if pending.in_flight || (pending.wait_for_shape && !geometry_changed) {
            runtime.find_navigation.pending = Some(pending);
            return Task::none();
        }
        // Same-generation hydration and delayed parser completions are real
        // source changes. Wait for the exact current immutable item, not a
        // useful but older streaming prefix or a Loading caption.
        if current.iter().any(|item| {
            item.role != chat::Role::User
                && runtime
                    .presentation_cache()
                    .get(&item.id)
                    .is_none_or(|cached| {
                        cached.placeholder
                            || cached
                                .item
                                .as_ref()
                                .is_none_or(|shown| !Arc::ptr_eq(shown, item))
                    })
        }) {
            runtime.find_navigation.pending = Some(pending);
            return Task::none();
        }
        let (epoch, catalog) = document(runtime);
        if pending.document != Some(epoch) {
            pending.hit = None;
            pending.document = Some(epoch);
            pending.previous = None;
        }
        pending.wait_for_shape = false;
        let task = if let Some(hit) = &pending.hit {
            pending.in_flight = true;
            let intent = intent.clone();
            scroll::locate_logical(target, hit.item.clone(), hit.identity, hit.byte).map(
                move |location| {
                    cosmic::Action::App(action(Action::FindLocated(
                        target,
                        intent.clone(),
                        epoch,
                        location,
                    )))
                },
            )
        } else {
            // No waiting worker queue and no source capture before admission:
            // rapid queries cannot accumulate whole-context Arc catalogs.
            static WORKER: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> =
                std::sync::OnceLock::new();
            let Ok(permit) = WORKER
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
                .clone()
                .try_acquire_owned()
            else {
                runtime.find_navigation.pending = Some(pending);
                return later(target, intent, false);
            };
            let ids = current.iter().map(|item| item.id.clone()).collect();
            let blocks = catalog.owned_search_blocks(&ids);
            let query = pending.query.clone();
            let proof = intent.clone();
            pending.in_flight = true;
            cosmic::task::future(async move {
                let hit = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    blocks.into_iter().find_map(|(item, identity, text)| {
                        if !proof.current() {
                            return None;
                        }
                        find::first(&text, &query).map(|range| Hit {
                            item: item.to_string(),
                            identity,
                            byte: range.start,
                        })
                    })
                })
                .await
                .ok()
                .flatten();
                action(Action::FindIndexed(target, intent, epoch, hit))
            })
        };
        runtime.find_navigation.pending = Some(pending);
        task
    }
    pub(super) fn resume_current_find(
        &mut self,
        target: Target,
        geometry_changed: bool,
    ) -> Task<Message> {
        let intent = self.chat_runtime_mut(target).and_then(|runtime| {
            runtime
                .find_navigation
                .pending
                .as_ref()
                .map(|pending| pending.intent.clone())
        });
        intent.map_or_else(Task::none, |intent| {
            if geometry_changed {
                later(target, intent, true)
            } else {
                self.resume_find_navigation(target, intent, false)
            }
        })
    }
    pub(super) fn find_indexed(
        &mut self,
        target: Target,
        intent: Intent,
        epoch: DocumentEpoch,
        hit: Option<Hit>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let current = runtime
            .find_navigation
            .pending
            .as_ref()
            .is_some_and(|pending| pending.intent.epoch == intent.epoch);
        if !current {
            return self.resume_current_find(target, true);
        }
        if !intent.current() || !runtime.visible.load(Ordering::Relaxed) {
            runtime.find_navigation.cancel();
            return Task::none();
        }
        let (now, _) = document(runtime);
        let Some(pending) = runtime.find_navigation.pending.as_mut() else {
            return Task::none();
        };
        pending.in_flight = false;
        if epoch == now && intent.current() {
            pending.hit = hit;
            if pending.hit.is_none() {
                // Matching metadata has no rendered plaintext byte. Keep the
                // honest coarse anchor behavior without inventing glyph proof.
                runtime.find_navigation.pending = None;
                return Task::none();
            }
        } else {
            pending.hit = None;
            pending.document = None;
        }
        self.resume_find_navigation(target, intent, true)
    }
    pub(super) fn find_located(
        &mut self,
        target: Target,
        intent: Intent,
        epoch: DocumentEpoch,
        location: Option<scroll::LogicalLocation>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(mut pending) = runtime.find_navigation.pending.take() else {
            return Task::none();
        };
        if pending.intent.epoch != intent.epoch {
            runtime.find_navigation.pending = Some(pending);
            return Task::none();
        }
        if !intent.current()
            || !runtime.visible.load(Ordering::Relaxed)
            || runtime.selected_subagent != pending.context
            || runtime.find.as_ref() != Some(&pending.query)
        {
            runtime.find_navigation.cancel();
            return Task::none();
        }
        let (now, _) = document(runtime);
        pending.in_flight = false;
        if epoch != now || !sources_match(runtime, &pending) {
            pending.hit = None;
            pending.document = None;
            pending.wait_for_shape = false;
            runtime.find_navigation.pending = Some(pending);
            return self.resume_find_navigation(target, intent, true);
        }
        let Some(location) = location else {
            runtime.find_navigation.pending = Some(pending);
            return later(target, intent, false);
        };
        let same = pending.previous.is_some_and(|(stage, offset)| {
            stage == location.stage && (offset - location.offset).abs() <= 1.0
        });
        pending.previous = Some((location.stage, location.offset));
        // A single large paragraph has no logical row estimate until its
        // admitted full-context geometry arrives. Repeating that unchanged
        // coarse correction must not exhaust Find while the real source is
        // still shaping, even if unrelated/stale completions wake this card.
        if let Some(geometry) = location.shape_pending
            && pending
                .realized_pending
                .as_ref()
                .is_some_and(|(owner, hit)| *owner == geometry && Some(hit) == pending.hit.as_ref())
        {
            pending.wait_for_shape = true;
            runtime.find_navigation.pending = Some(pending);
            return Task::none();
        }
        if pending.passes >= 16 {
            runtime.find_navigation.cancel();
            return Task::none();
        }
        pending.passes += 1;
        if let Some(geometry) = location.shape_pending {
            // Verified appends retain the body-free geometry owner. Its one
            // initial realization survives document publications while current
            // shaping is pending; rewritten sources get another owner.
            pending.realized_pending = pending.hit.clone().map(|hit| (geometry, hit));
        }
        let scroll =
            scroll::apply_find_location(target, location.clone(), intent.clone(), Some(epoch))
                .map(cosmic::Action::App);
        if location.exact {
            return scroll;
        } // Guard remains valid until any user navigation.
        pending.wait_for_shape = location.shape_pending.is_some() || (location.stage >= 2 && same);
        let retry = if pending.wait_for_shape {
            Task::none()
        } else {
            later(target, intent, false)
        };
        runtime.find_navigation.pending = Some(pending);
        Task::batch([scroll, retry])
    }
    pub(super) fn expire_find_navigation(&mut self, target: Target, intent: Intent) {
        if let Some(runtime) = self.chat_runtime_mut(target)
            && runtime
                .find_navigation
                .pending
                .as_ref()
                .is_some_and(|pending| pending.intent.epoch == intent.epoch)
        {
            runtime.find_navigation.cancel();
        }
    }
}
