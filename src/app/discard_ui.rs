//! Original snapshot sources and local discard receipts. UI methods are pure;
//! authority capture, Git, Trash and recovery retries run on blocking workers.
use super::*;
use crate::process::SystemProcessExecutor;
use crate::repository::{DiscardRecovery, DiscardRequest, RecoverableDiscard, WorktreeAuthority};
use std::{
    fmt,
    sync::{Arc, Mutex, OnceLock},
};

#[derive(Clone, Debug)]
pub(super) struct ReadSource {
    pub id: uuid::Uuid,
    pub key: ActiveWorktreeKey,
    authority: Arc<OnceLock<Result<WorktreeAuthority, String>>>,
}
impl ReadSource {
    fn new(key: ActiveWorktreeKey) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            key,
            authority: Arc::new(OnceLock::new()),
        }
    }
    /// Called only by the actual snapshot worker BEFORE its first Git read.
    pub fn read_service(
        &self,
    ) -> Result<
        crate::repository::RepositoryService<
            crate::repository::WorktreeExecutor<SystemProcessExecutor>,
        >,
        String,
    > {
        let authority = self
            .authority
            .get_or_init(|| {
                WorktreeAuthority::capture(&self.key.canonical_path).map_err(|e| e.to_string())
            })
            .clone()?;
        RepositoryService::system("git")
            .with_worktree_authority(authority)
            .map_err(|e| e.to_string())
    }
    pub fn validate_original(&self) -> Result<(), String> {
        self.authority
            .get()
            .ok_or("The original source snapshot is unavailable")?
            .as_ref()
            .map_err(Clone::clone)?
            .validate()
            .map_err(|e| e.to_string())
    }
    fn mutation_service(
        &self,
    ) -> Result<
        crate::repository::RepositoryService<
            crate::repository::WorktreeExecutor<SystemProcessExecutor>,
        >,
        String,
    > {
        let authority = self
            .authority
            .get()
            .ok_or("The original source snapshot is not ready")?
            .clone()?;
        RepositoryService::system("git")
            .with_worktree_authority(authority)
            .map_err(|e| e.to_string())
    }
}
#[derive(Clone)]
pub(super) struct Slot(Arc<Mutex<Entry>>);
struct Entry {
    id: uuid::Uuid,
    source: ReadSource,
    receipt: Option<DiscardRecovery>,
    running: bool,
    error: Option<String>,
}
impl fmt::Debug for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscardSlot")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}
impl Slot {
    pub fn id(&self) -> uuid::Uuid {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).id
    }
    pub fn source_id(&self) -> uuid::Uuid {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).source.id
    }
    pub fn cancel_before_start(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).running = false;
    }
    pub fn same_physical_source(&self, active: &ReadSource) -> bool {
        let original = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .source
            .clone();
        matches!((original.authority.get(),active.authority.get()),(Some(Ok(a)),Some(Ok(b))) if a.same_scope(b))
    }
    pub fn key(&self) -> ActiveWorktreeKey {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .source
            .key
            .clone()
    }
    pub fn finish_discard(&self, request: DiscardRequest, known: Vec<PathBuf>) -> Option<String> {
        let source = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .source
            .clone();
        let result = source.mutation_service().and_then(|service| {
            service
                .discard_recoverable(&request, &CancellationToken::new(), &known)
                .map_err(|e| e.to_string())
        });
        let (receipt, report) = match result {
            Ok(RecoverableDiscard { outcome, recovery }) => {
                (Some(recovery), mutation_report("Discard", Ok(outcome)))
            }
            Err(error) => (None, Some(format!("Discard could not run: {error}"))),
        };
        // Store BEFORE completion publication. Navigation, stale completion or
        // task dropping cannot erase an already-produced original receipt.
        let mut entry = self.0.lock().unwrap_or_else(|e| e.into_inner());
        entry.receipt = receipt;
        entry.running = false;
        entry.error = report.clone();
        report
    }
    pub fn finish_undo(&self, resolve: bool) -> Option<String> {
        let receipt = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .receipt
            .clone();
        let report = match receipt {
            None => Some("The original discard receipt is unavailable".into()),
            Some(receipt) => {
                let service = RepositoryService::system("git");
                if resolve {
                    service
                        .resolve_discard_recovery(&receipt, &CancellationToken::new())
                        .err()
                        .map(|e| format!("Recovery remains unresolved: {e}"))
                } else {
                    mutation_report(
                        "Undo discard",
                        service.undo_discard(&receipt, &CancellationToken::new()),
                    )
                }
            }
        };
        let mut entry = self.0.lock().unwrap_or_else(|e| e.into_inner());
        entry.running = false;
        entry.error = report.clone();
        report
    }
}
#[derive(Default)]
pub(super) struct State {
    active: Option<ReadSource>,
    intent: Option<(ReadSource, DiscardRequest)>,
    admitted_event: Option<uuid::Uuid>,
    slots: Vec<Slot>,
}
impl State {
    pub fn source(&self) -> Option<&ReadSource> {
        self.active.as_ref()
    }
    pub fn select(&mut self, key: Option<ActiveWorktreeKey>) {
        if self.active.as_ref().map(|s| &s.key) == key.as_ref() {
            return;
        }
        self.active = key.map(ReadSource::new);
        self.intent = None;
    }
    pub fn admits(&self, id: uuid::Uuid) -> bool {
        self.active.as_ref().is_some_and(|s| s.id == id)
    }
    pub fn admit_event(&mut self, id: uuid::Uuid) -> bool {
        if !self.admits(id) {
            return false;
        }
        self.admitted_event = Some(id);
        true
    }
    pub fn take_event(&mut self) -> Option<uuid::Uuid> {
        self.admitted_event.take()
    }
    pub fn confirm_intent(&mut self, id: uuid::Uuid, request: &DiscardRequest) -> bool {
        let Some(source) = self
            .active
            .as_ref()
            .filter(|s| s.id == id && s.key.canonical_path == request.worktree)
        else {
            return false;
        };
        if source.authority.get().is_none_or(|r| r.is_err()) {
            return false;
        }
        self.intent = Some((source.clone(), request.clone()));
        true
    }
    pub fn intent_id(&self) -> Option<uuid::Uuid> {
        self.intent.as_ref().map(|(s, _)| s.id)
    }
    pub fn cancel_intent(&mut self) {
        self.intent = None;
    }
    pub fn reserve_confirmed(
        &mut self,
        id: uuid::Uuid,
        request: &DiscardRequest,
    ) -> Result<Slot, String> {
        self.prune();
        if self.slots.len() >= 8 {
            return Err(
                "Resolve or finish existing discard recovery before another discard".into(),
            );
        }
        let (source, pending) = self
            .intent
            .as_ref()
            .ok_or("The original discard confirmation expired")?;
        let mut confirmed = pending.clone();
        confirmed.confirmed = true;
        if source.id != id || !self.admits(id) || &confirmed != request {
            return Err(
                "The discard confirmation no longer belongs to this original source".into(),
            );
        }
        let slot = Slot(Arc::new(Mutex::new(Entry {
            id: uuid::Uuid::new_v4(),
            source: source.clone(),
            receipt: None,
            running: true,
            error: None,
        })));
        self.slots.push(slot.clone());
        self.intent = None;
        Ok(slot)
    }
    pub fn slot(&self, id: uuid::Uuid) -> Option<Slot> {
        self.slots.iter().find(|s| s.id() == id).cloned()
    }
    pub fn begin_undo(&mut self, id: uuid::Uuid, resolve: bool) -> Option<Slot> {
        let slot = self.slots.iter().find(|s| s.id() == id)?.clone();
        {
            let mut entry = slot.0.lock().unwrap_or_else(|e| e.into_inner());
            let receipt = entry.receipt.as_ref()?;
            if entry.running
                || receipt.live_recovery.needs_resolution() != resolve
                || (!resolve && !receipt.can_undo())
            {
                return None;
            }
            entry.running = true;
        }
        Some(slot)
    }
    pub fn prune(&mut self) {
        self.slots.retain(|slot| {
            let entry = slot.0.lock().unwrap_or_else(|e| e.into_inner());
            entry.running
                || entry
                    .receipt
                    .as_ref()
                    .is_some_and(DiscardRecovery::can_undo)
        });
    }
    pub fn banner(&self, disabled: bool) -> Element<'_, Message> {
        let mut rows = Vec::new();
        for slot in &self.slots {
            let entry = slot.0.lock().unwrap_or_else(|e| e.into_inner());
            let Some(receipt) = &entry.receipt else {
                continue;
            };
            if !receipt.can_undo() {
                continue;
            }
            let resolve = receipt.live_recovery.needs_resolution();
            let label = if entry.running {
                "Restoring discarded work…"
            } else if resolve {
                "Discard recovery needs attention"
            } else {
                "Discarded work can be restored"
            };
            let button = crate::ui_controls::button::secondary(if resolve {
                "Resolve recovery"
            } else {
                "Undo discard"
            })
            .on_press_maybe((!entry.running && !disabled).then_some(
                Message::DiscardUndo {
                    id: entry.id,
                    resolve,
                },
            ));
            let mut content = vec![
                design::body(format!(
                    "{} · {label}",
                    display_path(&entry.source.key.canonical_path)
                ))
                .into(),
                button.into(),
            ];
            if let Some(error) = &entry.error {
                content.push(design::caption(error.clone()).into());
            }
            rows.push(
                widget::container(
                    widget::row::with_children(content)
                        .spacing(space::S)
                        .align_y(Alignment::Center),
                )
                .padding(space::S)
                .into(),
            );
        }
        widget::column::with_children(rows)
            .width(Length::Fill)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> (tempfile::TempDir, ReadSource) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let key = ActiveWorktreeKey {
            project_id: crate::model::ProjectId::new(),
            worktree_id: crate::model::WorktreeId::new(),
            canonical_path: temp.path().to_owned(),
        };
        let source = ReadSource::new(key);
        source.read_service().unwrap();
        (temp, source)
    }
    fn request(source: &ReadSource) -> DiscardRequest {
        DiscardRequest {
            worktree: source.key.canonical_path.clone(),
            tracked: vec![crate::repository::PathSelection::new(
                RepoPath::parse("file").unwrap(),
            )],
            untracked: Vec::new(),
            confirmed: false,
        }
    }
    #[test]
    fn discard_old_rendered_nonce_cannot_bind_same_path_new_actor() {
        let (_temp, old) = source();
        let mut state = State::default();
        state.active = Some(old.clone());
        let request = request(&old);
        assert!(state.confirm_intent(old.id, &request));
        state.select(None);
        state.select(Some(old.key.clone()));
        assert!(!state.admit_event(old.id));
        assert!(!state.confirm_intent(old.id, &request));
        assert!(
            state
                .reserve_confirmed(
                    old.id,
                    &DiscardRequest {
                        confirmed: true,
                        ..request
                    }
                )
                .is_err()
        );
    }
    #[test]
    fn discard_confirmation_preserves_exact_source_and_selected_paths() {
        let (_temp, source) = source();
        let mut state = State::default();
        state.active = Some(source.clone());
        let request = request(&source);
        assert!(state.confirm_intent(source.id, &request));
        let mut wrong = DiscardRequest {
            confirmed: true,
            ..request.clone()
        };
        wrong.tracked.clear();
        assert!(state.reserve_confirmed(source.id, &wrong).is_err());
        let slot = state
            .reserve_confirmed(
                source.id,
                &DiscardRequest {
                    confirmed: true,
                    ..request
                },
            )
            .unwrap();
        assert_eq!(slot.source_id(), source.id);
        assert_eq!(state.slots.len(), 1);
        state.select(None);
        assert_eq!(
            state.slots.len(),
            1,
            "navigation cannot erase in-flight custody"
        );
        slot.cancel_before_start();
        state.prune();
        assert!(state.slots.is_empty());
    }
    #[test]
    fn discard_original_snapshot_source_never_recaptures_replacement_on_refresh() {
        let (temp, source) = source();
        let original = temp.path().join("original-git");
        std::fs::rename(temp.path().join(".git"), &original).unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        assert!(source.read_service().is_err());
        assert!(source.mutation_service().is_err());
        let replacement = ReadSource::new(source.key.clone());
        assert!(replacement.read_service().is_ok());
        assert_ne!(source.id, replacement.id);
    }
}
