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
        if self.disk_comparison_applied.as_ref() == Some(&key) && matches!(self.disk_comparison, Some(DiffState::Ready { .. })) { return Task::none(); }
        if let Some(job) = &self.disk_comparison_job {
            if job.key != key { job.cancellation.cancel(); }
            // Keep the old slot occupied even if cancellation is slow. Its
            // receipt will admit at most one current replacement snapshot.
            self.disk_comparison = Some(DiffState::Loading { request: job.request.as_u128() as u64, cancellation: job.cancellation.clone() });
            return Task::none();
        }
        self.disk_comparison_applied = None;
        let DocumentState::Editable(document) = &self.document else { unreachable!() };
        let disk = document.external_version.as_ref().expect("key proves external bytes");
        let before: Arc<str> = Arc::from(disk.text.as_str());
        let after = document.content.text_snapshot();
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
        self.disk_comparison_applied = Some(current);
        self.diff_layout_revision = self.diff_layout_revision.wrapping_add(1);
        self.disk_comparison = Some(DiffState::ready(match model {
            Some(model) => RenderedDiff::Text { model: Arc::new(model), old_label: "Disk".into(), new_label: "Unsaved buffer".into() },
            None => RenderedDiff::Error("Could not compare the buffer and disk. Your edits have been kept.".into()),
        }));
        Task::none()
    }
}
