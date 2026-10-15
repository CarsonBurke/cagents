//! One owned CPU slot for live disk-versus-buffer comparisons. Cancellation
//! requests do not release ownership; only that exact worker receipt does.
use super::*;

#[derive(Clone, PartialEq)]
pub(super) struct Key {
    identity: AsyncIdentity,
    revision: u64,
    disk_hash: blake3::Hash,
    source: PathBuf,
    font: Font,
    theme: crate::settings::EditorSyntaxTheme,
    ignore_whitespace: bool,
}
pub(super) struct Job {
    request: uuid::Uuid,
    key: Key,
    cancellation: CancellationToken,
}
impl Drop for Job { fn drop(&mut self) { self.cancellation.cancel(); } }
impl RepositoryRuntime {
    /// Exact current external observation, independent of button availability.
    /// Ordinary captured command targets include this value, so a queued Reload,
    /// Compare or Keep mine cannot silently adopt a newer disk observation.
    pub fn disk_command_observation(&self) -> Option<RepositoryDiskObservation> {
        let DocumentState::Editable(document) = &self.document else { return None; };
        Some(RepositoryDiskObservation {
            external_hash: document.external_version.as_ref().map(|version| version.content_hash),
            external_deleted: document.external_deleted,
            baseline_hash: document.disk_hash,
            stamp: self.disk_stamp,
        })
    }

    pub fn can_compare_disk(&self) -> bool {
        !self.has_editor_recovery() && !self.is_saving() && !self.is_read_only_source()
            && !matches!(self.disk_comparison, Some(DiffState::Loading { .. }))
            && matches!(&self.document, DocumentState::Editable(document) if document.external_version.is_some())
    }
    pub fn can_keep_mine(&self) -> bool {
        !self.has_editor_recovery() && !self.is_saving() && !self.is_read_only_source()
            && matches!(&self.document, DocumentState::Editable(document) if document.external_version.is_some() || document.external_deleted)
    }
    fn disk_comparison_key(&self, card: CardId) -> Option<Key> {
        if self.has_editor_recovery() || self.workspace_edit_pending() || self.is_saving() { return None; }
        let DocumentState::Editable(document) = &self.document else { return None; };
        let disk = document.external_version.as_ref()?;
        Some(Key { identity: self.current_identity(card, document.path.clone()), revision: document.revision,
            disk_hash: disk.content_hash, source: document.source.clone(), font: self.editor_font,
            theme: self.syntax_theme, ignore_whitespace: self.diff_ignore_whitespace })
    }
    pub(super) fn dismiss_disk_comparison(&mut self) {
        self.disk_comparison = None;
        self.disk_comparison_applied = None;
        if let Some(job) = &self.disk_comparison_job { job.cancellation.cancel(); }
    }
    pub(super) fn compare_disk(&mut self, card: CardId) -> Task<RepositoryAction> {
        let Some(key) = self.disk_comparison_key(card) else { return Task::none(); };
        self.source_surface = SourceSurface::Diff;
        if self.disk_comparison_applied.as_ref() == Some(&key) && matches!(self.disk_comparison, Some(DiffState::Ready(..))) { return Task::none(); }
        if let Some(job) = &self.disk_comparison_job {
            if job.key != key { job.cancellation.cancel(); }
            // Keep the old slot occupied even if cancellation is slow. Its
            // receipt will admit at most one current replacement snapshot.
            // Dropping a Loading state cancels its token. Preserve the
            // existing presentation instead of cancelling on a duplicate.
            if self.disk_comparison.is_none() {
                self.disk_comparison = Some(DiffState::Loading { request: job.request.as_u128() as u64, cancellation: job.cancellation.clone() });
            }
            return Task::none();
        }
        self.disk_comparison_applied = None;
        let DocumentState::Editable(document) = &self.document else { unreachable!() };
        let after = document.content.text_snapshot();
        if after.len() as u64 > MAX_DIFF_SIDE_BYTES {
            self.disk_comparison = Some(DiffState::ready(RenderedDiff::Error("The unsaved buffer exceeds the comparison size limit. Editing and safe saving remain available.".into())));
            return Task::none();
        }
        let disk = document.external_version.as_ref().expect("key proves external bytes");
        let before: Arc<str> = Arc::from(disk.text.as_str());
        let present = document.present;
        let cancellation = CancellationToken::new(); let request = uuid::Uuid::new_v4();
        self.disk_comparison = Some(DiffState::Loading { request: request.as_u128() as u64, cancellation: cancellation.clone() });
        self.disk_comparison_job = Some(Job { request, key: key.clone(), cancellation: cancellation.clone() });
        cosmic::task::future(async move {
            let worker_key = key.clone();
            let model = tokio::task::spawn_blocking(move || {
                if cancellation.is_cancelled() { return None; }
                // Persistent native text is materialized on the worker, once
                // per admitted revision rather than on repeated Compare input.
                let mut model = build_document_diff_sources_with_font(before, after.text(), &worker_key.source,
                    worker_key.theme, worker_key.ignore_whitespace, worker_key.font, &cancellation)?;
                model.present = [true, present];
                (!cancellation.is_cancelled()).then_some(model)
            }).await.ok().flatten();
            RepositoryAction::DiskComparisonLoaded { identity: key.identity, request, revision: key.revision, disk_hash: key.disk_hash, model }
        })
    }
    pub(super) fn finish_disk_comparison(&mut self, identity: AsyncIdentity, request: uuid::Uuid, revision: u64, disk_hash: blake3::Hash, model: Option<DiffModel>) -> Task<RepositoryAction> {
        let Some(job) = self.disk_comparison_job.as_ref().filter(|job| job.request == request
            && job.key.identity == identity && job.key.revision == revision && job.key.disk_hash == disk_hash) else { return Task::none(); };
        let completed = job.key.clone();
        let cancelled = job.cancellation.is_cancelled();
        self.disk_comparison_job = None;
        if self.disk_comparison.is_none() { return Task::none(); }
        let Some(current) = self.disk_comparison_key(identity.card) else { self.dismiss_disk_comparison(); return Task::none(); };
        if cancelled || current != completed { return self.compare_disk(identity.card); }
        self.disk_comparison_applied = model.is_some().then_some(current);
        self.diff_layout_revision = self.diff_layout_revision.wrapping_add(1);
        self.disk_comparison = Some(DiffState::ready(match model {
            Some(model) => RenderedDiff::Text { model: Arc::new(model), old_label: "Disk".into(), new_label: "Unsaved buffer".into() },
            None => RenderedDiff::Error("Could not compare the buffer and disk. Your edits have been kept.".into()),
        }));
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;

    fn fixture(card: CardId) -> RepositoryRuntime {
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "note.rs", "buffer");
        let DocumentState::Editable(document) = &mut runtime.document else { unreachable!() };
        document.external_version = Some(crate::editor_reload::Version { text: "disk".into(), content_hash: blake3::hash(b"disk"), source: document.source.clone(), markdown: false, mapping: Default::default(), restoration: None });
        document.conflict = true;
        runtime
    }
    async fn completion(task: Task<RepositoryAction>) -> RepositoryAction {
        let mut stream = iced::runtime::task::into_stream(task).expect("actual comparison worker task");
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(action @ RepositoryAction::DiskComparisonLoaded { .. }) = action { return action; }
        }
        panic!("comparison task must return its owned receipt");
    }
    #[test]
    fn captured_disk_actions_refuse_a_new_external_observation_without_buffer_changes() {
        for action in [RepositoryAction::CompareDisk { card: CardId::new() }, RepositoryAction::KeepMine { card: CardId::new() }, RepositoryAction::Reload { card: CardId::new() }] {
            let card = action.card(); let mut runtime = fixture(card);
            let target = runtime.command_target(card);
            assert!(runtime.resolve_captured_command(&target, action.clone()).is_some(), "unchanged captured disk action remains available");
            let DocumentState::Editable(document) = &mut runtime.document else { unreachable!() };
            let version = document.external_version.as_mut().unwrap();
            version.text = "newer disk".into(); version.content_hash = blake3::hash(version.text.as_bytes());
            assert!(runtime.resolve_captured_command(&target, action.clone()).is_none(), "same buffer revision must not authorize newer external bytes");
            let target = runtime.command_target(card);
            runtime.disk_stamp = Some(None);
            assert!(runtime.resolve_captured_command(&target, action.clone()).is_none(), "same external hash must not erase a newer metadata observation");
            let target = runtime.command_target(card);
            let DocumentState::Editable(document) = &mut runtime.document else { unreachable!() };
            document.external_deleted = true;
            assert!(runtime.resolve_captured_command(&target, action).is_none(), "deletion state belongs to the emitted action");
        }
    }

    #[tokio::test]
    async fn actual_compare_admissions_coalesce_edits_and_reject_duplicate_old_completion() {
        let card = CardId::new(); let mut runtime = fixture(card);
        let first = runtime.update(RepositoryAction::CompareDisk { card });
        let job = runtime.disk_comparison_job.as_ref().unwrap(); let request = job.request; let old = job.key.clone();
        for _ in 0..20 { assert!(iced::runtime::task::into_stream(runtime.update(RepositoryAction::CompareDisk { card })).is_none()); }
        assert!(!runtime.disk_comparison_job.as_ref().unwrap().cancellation.is_cancelled(), "duplicate Compare must not drop/cancel its Loading token");
        drop(runtime.update(RepositoryAction::EditorAction { card, action: EditorAction::Edit(EditorEdit::Insert('x')) }));
        assert!(iced::runtime::task::into_stream(runtime.update(RepositoryAction::CompareDisk { card })).is_none());
        assert_eq!(runtime.disk_comparison_job.as_ref().unwrap().request, request);
        assert!(runtime.disk_comparison_job.as_ref().unwrap().cancellation.is_cancelled());
        let replacement = runtime.update(completion(first).await);
        let next = runtime.disk_comparison_job.as_ref().unwrap().request; assert_ne!(next, request);
        drop(runtime.update(RepositoryAction::DiskComparisonLoaded { identity: old.identity, request, revision: old.revision, disk_hash: old.disk_hash, model: None }));
        assert_eq!(runtime.disk_comparison_job.as_ref().unwrap().request, next, "old receipt cannot release a newer worker");
        drop(runtime.update(completion(replacement).await));
        assert!(runtime.disk_comparison_job.is_none());
        let Some(DiffState::Ready(RenderedDiff::Text { model, .. }, _)) = &runtime.disk_comparison else { panic!("latest comparison ready"); };
        assert_eq!(&*model.after, runtime.editor_text().as_deref().unwrap()); assert_eq!(&*model.before, "disk");
        assert!(iced::runtime::task::into_stream(runtime.update(RepositoryAction::CompareDisk { card })).is_none(), "unchanged ready comparison needs no duplicate work");
    }
    #[tokio::test]
    async fn actual_whitespace_toggle_rebuilds_the_displayed_disk_comparison() {
        let card = CardId::new(); let mut runtime = fixture(card);
        runtime.set_editor_font(Font::with_name("Iosevka"), 16.0);
        let task = runtime.update(RepositoryAction::CompareDisk { card });
        drop(runtime.update(completion(task).await));
        let previous = runtime.disk_comparison_applied.as_ref().unwrap();
        assert!(!previous.ignore_whitespace); assert_eq!(previous.font, Font::with_name("Iosevka"));
        // Ceditor uses this shared reducer, without ADE's special settings arm.
        let task = runtime.update(RepositoryAction::ToggleDiffWhitespace { card });
        assert!(runtime.diff_ignore_whitespace);
        assert!(runtime.disk_comparison_job.as_ref().unwrap().key.ignore_whitespace);
        drop(runtime.update(completion(task).await));
        assert!(runtime.disk_comparison_applied.as_ref().unwrap().ignore_whitespace);
        assert!(matches!(runtime.disk_comparison, Some(DiffState::Ready(RenderedDiff::Text { .. }, _))));
    }

    #[tokio::test]
    async fn failed_owned_comparison_can_retry_unchanged_sources_with_a_new_worker() {
        let card = CardId::new(); let mut runtime = fixture(card);
        let task = runtime.update(RepositoryAction::CompareDisk { card });
        let previous = runtime.disk_comparison_job.as_ref().unwrap().request;
        let mut receipt = completion(task).await;
        let RepositoryAction::DiskComparisonLoaded { model, .. } = &mut receipt else { unreachable!() };
        // Inject the actual worker/join failure at its typed receipt boundary;
        // identity/request/revision/hash come from the real admitted task.
        *model = None;
        drop(runtime.update(receipt));
        assert!(matches!(runtime.disk_comparison, Some(DiffState::Ready(RenderedDiff::Error(_), _))));
        assert!(runtime.can_compare_disk()); assert!(runtime.disk_comparison_applied.is_none());
        let retry = runtime.update(RepositoryAction::CompareDisk { card });
        assert_ne!(runtime.disk_comparison_job.as_ref().unwrap().request, previous);
        drop(runtime.update(completion(retry).await));
        assert!(matches!(runtime.disk_comparison, Some(DiffState::Ready(RenderedDiff::Text { .. }, _))));
    }

    #[tokio::test]
    async fn dismissed_and_frozen_comparisons_release_only_on_their_actual_receipt() {
        for freeze in [false, true] {
            let card = CardId::new(); let mut runtime = fixture(card);
            let task = runtime.update(RepositoryAction::CompareDisk { card });
            if freeze {
                let (path, snapshot) = runtime.workspace_edit_snapshot().unwrap();
                let plan = crate::editor_workspace_edit::DocumentPlan { path: path.clone(), relative: "note.rs".into(), before: snapshot.text.clone(), after: snapshot.text.clone(), before_hash: blake3::hash(snapshot.text.as_bytes()), version: Some(snapshot.version), protocol_version: snapshot.protocol_version, open: true, open_paths: vec![path], edits: Vec::new() };
                runtime.begin_workspace_edit(77, &plan).unwrap();
            } else { runtime.dismiss_disk_comparison(); }
            assert!(runtime.disk_comparison_job.is_some());
            drop(runtime.update(completion(task).await));
            assert!(runtime.disk_comparison_job.is_none()); assert!(runtime.disk_comparison.is_none());
        }
    }
}
