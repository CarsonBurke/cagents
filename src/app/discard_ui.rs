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
    /// Intake an already-held source from its acknowledged creator. This is
    /// pure: it neither opens the path nor adopts a later root at that name.
    pub fn from_authority(
        key: ActiveWorktreeKey,
        authority: WorktreeAuthority,
    ) -> Result<Self, String> {
        if key.canonical_path != authority.path() {
            return Err("The supplied original source belongs to a different worktree".into());
        }
        Ok(Self {
            id: uuid::Uuid::new_v4(),
            key,
            authority: Arc::new(OnceLock::from(Ok(authority))),
        })
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
    pub fn anchored_read_path(&self) -> Result<PathBuf, String> {
        Ok(self
            .authority
            .get()
            .ok_or("The original source read is not ready")?
            .as_ref()
            .map_err(Clone::clone)?
            .anchored_read_path())
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
    pub fn finish_recover_original(&self, nonce: uuid::Uuid) -> Option<String> {
        let receipt = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .receipt
            .clone();
        let report = receipt
            .ok_or_else(|| "The original discard receipt is unavailable".to_owned())
            .and_then(|receipt| {
                RepositoryService::system("git")
                    .recover_original_file(&receipt, nonce, &CancellationToken::new())
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
            .err()
            .map(|e| format!("Original-file recovery remains unresolved: {e}"));
        let mut entry = self.0.lock().unwrap_or_else(|e| e.into_inner());
        entry.running = false;
        entry.error = report.clone();
        report
    }
}
#[derive(Default)]
pub(super) struct State {
    active: Option<ReadSource>,
    supplied: Option<ReadSource>,
    intent: Option<(ReadSource, DiscardRequest)>,
    admitted_event: Option<uuid::Uuid>,
    slots: Vec<Slot>,
}
impl State {
    pub fn has_inflight(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.0.lock().unwrap_or_else(|e| e.into_inner()).running)
    }
    pub fn unresolved_authorities(&self) -> Vec<crate::recovery_authority::RecoveryAuthority> {
        self.slots
            .iter()
            .filter_map(|slot| {
                slot.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .receipt
                    .as_ref()
                    .and_then(|receipt| receipt.live_recovery.authority())
            })
            .filter(|authority| !authority.is_resolved())
            .collect()
    }
    pub fn close_block_reason(&self) -> Option<&'static str> {
        if self.has_inflight() {
            Some("Wait for discard or Undo to finish before closing.")
        } else if !self.unresolved_authorities().is_empty() {
            Some("Resolve the pending discard recovery before closing.")
        } else {
            None
        }
    }
    pub fn source(&self) -> Option<&ReadSource> {
        self.active.as_ref()
    }
    /// Stage exactly one original creator source before selection/rebind.
    /// The next selected key must match; any other selection drops it.
    pub fn supply_original(
        &mut self,
        key: ActiveWorktreeKey,
        authority: WorktreeAuthority,
    ) -> Result<uuid::Uuid, String> {
        let supplied = ReadSource::from_authority(key, authority)?;
        let id = supplied.id;
        self.supplied = Some(supplied);
        Ok(id)
    }
    /// The cache-release half of a real rebind retires the old actor while
    /// preserving the one-shot source intended for that same rebind.
    pub fn retire_source(&mut self) {
        self.active = None;
        self.intent = None;
        self.admitted_event = None;
    }
    pub fn select(&mut self, key: Option<ActiveWorktreeKey>) {
        if key.is_none() {
            self.supplied = None;
            self.retire_source();
            return;
        }
        if let Some(supplied) = self.supplied.take()
            && Some(&supplied.key) == key.as_ref()
        {
            self.retire_source();
            self.active = Some(supplied);
            return;
        }
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
    pub fn begin_recover_original(&mut self, id: uuid::Uuid, nonce: uuid::Uuid) -> Option<Slot> {
        let slot = self.slot(id)?;
        {
            let mut entry = slot.0.lock().unwrap_or_else(|e| e.into_inner());
            let receipt = entry.receipt.as_ref()?;
            if entry.running
                || !receipt.live_recovery.needs_resolution()
                || !receipt
                    .live_recovery
                    .recoverable_originals()
                    .iter()
                    .any(|p| p.nonce == nonce)
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
    pub fn original_details(&self, id: uuid::Uuid, nonce: uuid::Uuid) -> Option<String> {
        let slot = self.slot(id)?;
        let receipt = slot
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .receipt
            .clone()?;
        let original = receipt
            .live_recovery
            .recoverable_originals()
            .into_iter()
            .find(|original| original.nonce == nonce)?;
        let digest: String = original
            .sha256
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let guidance = if original.published_unconfirmed {
            "The original inode has already been placed at the original path. Leave it in place. Verify recovered original or Resolve verifies that same inode and finishes durability/cleanup; neither publishes again."
        } else {
            "Preserve any replacement folder under a separate name and restore the original folder. Preserve any occupied baseline under a separate name, leaving the original file path empty. Recover original file never overwrites an occupied file. Resolve does not restore an unpublished payload."
        };
        Some(format!(
            "Original file: {}\nRetained payload (original diagnostic path only): {}\nBytes: {}\nMode: {:04o}\nSHA-256: {}\nPublication unconfirmed: {}\n\n{}\n",
            display_path(&original.original_path),
            display_path(&original.retained_path),
            original.bytes,
            original.mode,
            digest,
            original.published_unconfirmed,
            guidance,
        ))
    }
    pub fn banner(&self, disabled: bool) -> Element<'_, Message> {
        use cosmic::iced::{advanced::text::EllipsizeHeightLimit, widget::text::Ellipsize};
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
            let mut content = vec![
                widget::row::with_children(vec![
                    design::body(format!(
                        "{} · {label}",
                        display_path(&entry.source.key.canonical_path)
                    ))
                    .ellipsize(Ellipsize::Middle(EllipsizeHeightLimit::Lines(1)))
                    .width(Length::Fill)
                    .into(),
                    crate::ui_controls::button::secondary(if resolve {
                        "Resolve recovery"
                    } else {
                        "Undo discard"
                    })
                    .on_press_maybe(
                        (!entry.running && !disabled).then_some(Message::DiscardUndo {
                            id: entry.id,
                            resolve,
                        }),
                    )
                    .into(),
                ])
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
            ];
            let originals = receipt.live_recovery.recoverable_originals();
            if originals
                .iter()
                .any(|original| !original.published_unconfirmed)
            {
                content.push(design::caption("Preserve the replacement folder and baseline first. Restore the original folder and leave the file path empty; Recover never overwrites it.").into());
            }
            if originals
                .iter()
                .any(|original| original.published_unconfirmed)
            {
                content.push(design::caption("A recovered file still needs verification. Leave it in place and choose Verify or Resolve.").into());
            }
            for original in originals {
                content.push(
                    widget::row::with_children(vec![
                        design::body(display_path(original.path.as_path()).into_owned())
                            .ellipsize(Ellipsize::Middle(EllipsizeHeightLimit::Lines(1)))
                            .width(Length::Fill)
                            .into(),
                        crate::ui_controls::button::secondary(if original.published_unconfirmed {
                            "Verify recovered original"
                        } else {
                            "Recover original file"
                        })
                        .on_press_maybe((!entry.running && !disabled).then_some(
                            Message::DiscardRecoverOriginal {
                                id: entry.id,
                                nonce: original.nonce,
                            },
                        ))
                        .into(),
                        crate::ui_controls::button::quiet("Copy recovery details")
                            .on_press_maybe((!disabled).then_some(
                                Message::DiscardCopyRecoveryDetails {
                                    id: entry.id,
                                    nonce: original.nonce,
                                },
                            ))
                            .into(),
                    ])
                    .spacing(space::S)
                    .align_y(Alignment::Center)
                    .into(),
                );
            }
            if let Some(error) = &entry.error {
                content.push(design::caption(error.clone()).into());
            }
            rows.push(
                widget::container(widget::column::with_children(content).spacing(space::S))
                    .padding(space::S)
                    .into(),
            );
        }
        // One shared bounded region, rather than one growing region per file
        // or receipt. All actions remain scroll-reachable in smaller windows.
        widget::container(
            widget::scrollable(widget::column::with_children(rows).spacing(space::XS))
                .id(widget::Id::new("discard-recovery-notices"))
                .height(Length::Shrink),
        )
        .max_height(180.0)
        .width(Length::Fill)
        .into()
    }
}

#[cfg(all(test, target_os = "linux"))]
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
    fn discard_inspector_rendered_controls_carry_the_original_snapshot_actor() {
        use cosmic::iced::{
            Event, Rectangle, Size,
            advanced::{
                Layout, Shell, clipboard, layout, mouse,
                widget::{Tree, operation},
            },
            keyboard,
        };
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.sidebar_model = sidebar::tests::panel_model();
        app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
        let key = app
            .sidebar_model
            .source_control
            .as_ref()
            .unwrap()
            .key
            .clone();
        app.discard_ui.select(Some(key));
        let original = app.discard_ui.source().unwrap().id;
        app.context_tab = ContextTab::Changes;
        let renderer = crate::app::tests::headless_renderer();
        let mut element = app.context_panel(None, 380.0);
        let mut tree = Tree::new(element.as_widget());
        let viewport = Rectangle::with_size(Size::new(380.0, 4_000.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        fn identities(tree: &Tree, found: &mut Vec<widget::Id>) {
            found.extend(tree.id.clone());
            for child in &tree.children {
                identities(child, found);
            }
        }
        let mut ids = Vec::new();
        identities(&tree, &mut ids);
        let enter = Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
        let mut fired = Vec::new();
        for id in ids {
            let mut focus = operation::focusable::focus::<()>(id);
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut focus);
            element.as_widget_mut().update(
                &mut tree,
                &enter,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut fired),
                &viewport,
            );
        }
        assert!(fired.iter().any(|message| matches!(message,
            Message::SidebarSnapshot { source, action } if *source == original && matches!(&**action,
                SidebarAction::RequestDiscard { .. } | SidebarAction::ChangeGroup { command: sidebar::ChangeGroupCommand::Discard, .. }
            )
        )), "the actual inspector Changes widget must emit an original-source discard command");
        assert!(
            !fired.iter().any(|message| matches!(
                message,
                Message::Sidebar(
                    SidebarAction::RequestDiscard { .. }
                        | SidebarAction::ChangeGroup {
                            command: sidebar::ChangeGroupCommand::Discard,
                            ..
                        }
                )
            )),
            "no destructive inspector command may bypass the source envelope"
        );
    }

    #[test]
    fn discard_supplied_original_survives_release_and_never_recaptures_on_focus_return() {
        let (temp, original) = source();
        let authority = original.authority.get().unwrap().clone().unwrap();
        let mut state = State::default();
        state.active = Some(original.clone());
        let supplied = state
            .supply_original(original.key.clone(), authority.clone())
            .unwrap();
        assert_ne!(supplied, original.id);
        state.retire_source();
        assert!(state.source().is_none());
        state.select(Some(original.key.clone()));
        assert_eq!(state.source().unwrap().id, supplied);
        assert!(
            state
                .source()
                .unwrap()
                .authority
                .get()
                .unwrap()
                .as_ref()
                .unwrap()
                .same_scope(&authority)
        );
        assert!(!state.admit_event(original.id));
        state.select(Some(original.key.clone()));
        assert_eq!(
            state.source().unwrap().id,
            supplied,
            "ordinary refresh keeps the supplied actor"
        );
        std::fs::rename(temp.path().join(".git"), temp.path().join("old-git")).unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        assert!(state.source().unwrap().read_service().is_err());
        assert!(state.source().unwrap().mutation_service().is_err());
        let wrong = ActiveWorktreeKey {
            canonical_path: temp.path().join("other"),
            ..original.key.clone()
        };
        assert!(ReadSource::from_authority(wrong.clone(), authority.clone()).is_err());
        state
            .supply_original(original.key.clone(), authority)
            .unwrap();
        state.retire_source();
        state.select(Some(wrong));
        state.select(Some(original.key));
        assert_ne!(
            state.source().unwrap().id,
            supplied,
            "a different selection consumes no old intake"
        );
        assert!(state.source().unwrap().authority.get().is_none());
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
        assert!(state.close_block_reason().is_some());
        state.select(None);
        assert_eq!(
            state.slots.len(),
            1,
            "navigation cannot erase in-flight custody"
        );
        slot.cancel_before_start();
        state.prune();
        assert!(state.slots.is_empty());
        assert!(state.close_block_reason().is_none());
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

#[cfg(all(test, target_os = "linux"))]
mod actual_tests;
