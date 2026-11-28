//! The connection workers can serve reads against the current attachment.
use super::{connections::Answer, *};
use crate::terminal_ipc::ServerResponse;
use std::sync::{RwLock, TryLockError, atomic::AtomicU64};

struct Fence {
    proof: HostSessionProof,
    epoch: uuid::Uuid,
    close_only: bool,
    installed: bool,
    source: Arc<chat::Session>,
}

struct CachedPresentation {
    snapshot: Snapshot,
    source: Arc<chat::Session>,
    reset: u64,
}

pub(super) struct Reads {
    fence: Mutex<Option<Fence>>,
    startup: UnixStream,
    actor: RwLock<Option<LocalHandle>>,
    last_presentation: Mutex<Option<Arc<CachedPresentation>>>,
    refresh: AtomicBool,
    generation: Arc<AtomicU64>,
    watcher: Arc<push::Watcher>,
    publication: Arc<Mutex<journal::ReadPublication>>,
}

impl Reads {
    pub(super) fn new(server: &Server, startup: &UnixStream) -> Result<Self, String> {
        let reads = Self {
            fence: Mutex::new(None),
            startup: startup.try_clone().map_err(|error| error.to_string())?,
            actor: RwLock::new(Some(server.actor.clone())),
            last_presentation: Mutex::new(None),
            refresh: AtomicBool::new(false),
            generation: server.changed.clone(),
            watcher: server.watcher.clone(),
            publication: server
                .journal
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .read_publication(),
        };
        reads.publish(server);
        Ok(reads)
    }

    pub(super) fn publish(&self, server: &Server) {
        let mut fence = self.fence.lock().unwrap_or_else(|p| p.into_inner());
        let source = fence
            .as_ref()
            .filter(|old| *old.source == server.source)
            .map_or_else(|| Arc::new(server.source.clone()), |old| old.source.clone());
        *fence = Some(Fence {
            proof: server.proof.clone(),
            epoch: server.epoch,
            close_only: server.close_only,
            installed: server.installed,
            source,
        });
    }

    pub(super) fn stop(&self) {
        *self.fence.lock().unwrap_or_else(|p| p.into_inner()) = None;
        // Accepted but unparsed sockets may outlive the listener. Wait
        // only for active queries before releasing the actor/cache owner.
        self.actor.write().unwrap_or_else(|p| p.into_inner()).take();
    }

    fn source(&self, request: &Invocation) -> Result<Arc<chat::Session>, String> {
        let fence = self.fence.lock().unwrap_or_else(|p| p.into_inner());
        let fence = fence.as_ref().ok_or("The retained actor stopped serving")?;
        validate_attachment(request, fence.epoch, &fence.proof, fence.close_only)?;
        if !fence.installed && disconnected(&self.startup)? {
            return Err("The retained actor startup owner disconnected".into());
        }
        Ok(fence.source.clone())
    }

    pub(super) fn answer(&self, request: &Invocation, wake: impl FnOnce()) -> Option<Answer> {
        if self
            .fence
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_none()
        {
            return Some(Answer::Retry);
        }
        let actor = self.actor.read().unwrap_or_else(|p| p.into_inner());
        let actor = actor.as_ref()?;
        match request.method.as_str() {
            "file_query" => Some(Answer::Value(self.value(request, |args| {
                let (command,): (Command,) = decode(args)?;
                actor.file_query(&command)
            }))),
            "text_copy" => {
                let read = (|| {
                    self.source(request)?;
                    let (source, offset) = decode(&request.args)?;
                    let page = actor.read_text_copy(&source, offset);
                    self.source(request)?;
                    Ok(page)
                })();
                match read {
                    Ok(None) => None,
                    Ok(Some(page)) => Some(Answer::Value(page.and_then(to_value))),
                    Err(error) => Some(Answer::Value(Err(error))),
                }
            }
            "presentation_codec" => Some(Answer::Value(self.value(request, |args| {
                let (version,): (u16,) = decode(args)?;
                Ok(version == crate::terminal_ipc::PROTOCOL_VERSION)
            }))),
            "presentation" => self.presentation(actor, request, false, wake),
            "presentation_binary" => self.presentation(actor, request, true, wake),
            _ => None,
        }
    }

    fn value<T: Serialize>(
        &self,
        request: &Invocation,
        read: impl FnOnce(&Value) -> Result<T, String>,
    ) -> Result<Value, String> {
        self.source(request)?;
        let result = read(&request.args);
        self.source(request)?;
        to_value(result?)
    }

    fn presentation(
        &self,
        actor: &LocalHandle,
        request: &Invocation,
        binary: bool,
        wake: impl FnOnce(),
    ) -> Option<Answer> {
        let read = (|| {
            let source = self.source(request)?;
            let (id, after) = if binary {
                let (version, id, after) = decode(&request.args)?;
                validate_presentation_codec(version)?;
                (id, after)
            } else {
                decode(&request.args)?
            };
            if id != source.id {
                return Err("Retained presentation names another chat".into());
            }
            self.check_loss()?;
            let reset = actor.0.cache_reader.epoch();
            let previous = self
                .last_presentation()
                .filter(|cached| cached.reset == reset && Arc::ptr_eq(&cached.source, &source));
            let mut sampled = None;
            let store = match actor.0.store.try_lock() {
                Ok(store) => Some(store),
                Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => None,
            };
            let response = match store {
                Some(store) => actor.presentation_locked(id, after, store, |snapshot| {
                    if previous
                        .as_ref()
                        .is_none_or(|old| old.snapshot.generation != snapshot.generation)
                    {
                        let mut snapshot = snapshot.clone();
                        snapshot.sampled_at = super::super::super::observed_at();
                        sampled = Some(snapshot);
                    }
                }),
                None => {
                    let Some(previous) =
                        previous.filter(|cached| Self::matches_resident(actor, &cached.snapshot))
                    else {
                        return Ok(Some(Answer::Retry));
                    };
                    // Reannounce this revision when hydration can proceed.
                    // An unchanged answer otherwise consumes the reader's wake.
                    if !self.refresh.swap(true, Ordering::AcqRel) {
                        wake();
                    }
                    after
                        .is_none_or(|after| after < previous.snapshot.generation)
                        .then(|| ServerResponse::ChatSnapshot {
                            chat_id: id,
                            snapshot: Some(previous.snapshot.clone()),
                        })
                }
            };
            let current = self.source(request)?;
            if !Arc::ptr_eq(&source, &current) || reset != actor.0.cache_reader.epoch() {
                return Ok(Some(Answer::Retry));
            }
            let (sources, loss) = {
                let publication = self.publication.lock().unwrap_or_else(|p| p.into_inner());
                (publication.sources.clone(), publication.loss.clone())
            };
            if let Some(loss) = loss {
                return Err(loss.to_string());
            }
            let is_published = |snapshot: &Snapshot| {
                let alias = published_source(
                    &current,
                    snapshot.session_id.as_deref(),
                    &snapshot.native_continuity,
                );
                sources.contains(&alias)
            };
            // The writer validates and commits a previously unseen alias
            // before replies can carry it. No journal write runs on a worker.
            if response
                .as_ref()
                .and_then(presentation_metadata)
                .is_some_and(|snapshot| !is_published(snapshot))
            {
                return Ok(None);
            }
            if let Some(snapshot) = sampled
                && is_published(&snapshot)
            {
                self.cache_presentation(snapshot, source, reset);
            }
            let answer = if binary {
                Answer::Binary(Ok(Box::new(response.unwrap_or(
                    ServerResponse::ChatSnapshot {
                        chat_id: id,
                        snapshot: None,
                    },
                ))))
            } else {
                Answer::Value(to_value(response))
            };
            Ok(Some(answer))
        })();
        match read {
            Ok(answer) => answer,
            Err(error) if binary => Some(Answer::Binary(Err(error))),
            Err(error) => Some(Answer::Value(Err(error))),
        }
    }

    fn matches_resident(actor: &LocalHandle, snapshot: &Snapshot) -> bool {
        actor.0.state.try_lock().is_ok_and(|state| {
            state.session_id == snapshot.session_id
                && state.native_continuity == snapshot.native_continuity
        })
    }

    fn last_presentation(&self) -> Option<Arc<CachedPresentation>> {
        self.last_presentation
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub(super) fn cache_serial_snapshot(&self, snapshot: Snapshot, reset: u64) {
        let source = self
            .fence
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|fence| fence.source.clone());
        if let Some(source) = source {
            self.cache_presentation(snapshot, source, reset);
        }
    }

    fn cache_presentation(&self, snapshot: Snapshot, source: Arc<chat::Session>, reset: u64) {
        let fence = self.fence.lock().unwrap_or_else(|p| p.into_inner());
        if !fence
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(&current.source, &source))
        {
            return;
        }
        let mut last = self
            .last_presentation
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if last.as_ref().is_none_or(|old| {
            !Arc::ptr_eq(&old.source, &source)
                || (old.reset, old.snapshot.generation) <= (reset, snapshot.generation)
        }) {
            *last = Some(Arc::new(CachedPresentation {
                snapshot,
                source,
                reset,
            }));
        }
    }

    pub(super) fn needs_refresh(&self) -> bool {
        self.refresh.load(Ordering::Acquire)
    }

    pub(super) fn announce_when_readable(&self, actor: &LocalHandle) {
        if !self.needs_refresh() {
            return;
        }
        let store = match actor.0.store.try_lock() {
            Ok(store) => store,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        self.refresh.store(false, Ordering::Release);
        drop(store);
        self.generation.fetch_add(1, Ordering::Release);
        self.watcher.notify();
    }

    fn check_loss(&self) -> Result<(), String> {
        match &self
            .publication
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .loss
        {
            Some(loss) => Err(loss.to_string()),
            None => Ok(()),
        }
    }
}

fn decode<'a, T: Deserialize<'a>>(value: &'a Value) -> Result<T, String> {
    T::deserialize(value).map_err(|error| error.to_string())
}

fn to_value(value: impl Serialize) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}
