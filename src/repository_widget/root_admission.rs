//! Two-phase original-root admission for native document loading.
//!
//! The window owns the ledger before any task is returned. Phase one may
//! connect to the captured endpoint for catalog discovery, but installs the
//! exact prepared lease in that custody before phase two can register it.
use super::*;
use crate::edited_root_admission::{PreparationWork, PreparedRoot};

#[derive(Debug, Clone)]
pub struct RootFilePreparation {
    request: uuid::Uuid,
    result: Result<LoadedFile, FileIoError>,
    indentation: crate::editor_indent::Overrides,
    root: Option<crate::editor::FileRootIdentity>,
    prepared: Option<PreparedRoot>,
}

/// A worker completion retains the same per-load nonce and original root pair.
#[derive(Debug, Clone)]
pub struct RootFileAdmission {
    request: uuid::Uuid,
    result: Result<LoadedFile, FileIoError>,
    indentation: crate::editor_indent::Overrides,
    root: Option<crate::editor::FileRootIdentity>,
}

impl RootFilePreparation {
    fn refuse(self) {
        if let Some(prepared) = self.prepared {
            prepared.ticket().request_retirement();
        }
    }
}

struct Intake {
    endpoint: crate::recovery_domain::LocalRecoveryEndpoint,
    work: PreparationWork,
}

pub(super) fn is_mutable(result: &Result<LoadedFile, FileIoError>) -> bool {
    matches!(
        result,
        Ok(LoadedFile {
            content: FileContent::Editable { .. } | FileContent::SavedAbsence,
            ..
        })
    )
}

impl RepositoryRuntime {
    pub(super) fn admitted_file_task(
        &mut self,
        identity: AsyncIdentity,
        absent: Option<(PathBuf, crate::editor::FileRootIdentity)>,
    ) -> Task<RepositoryAction> {
        // This short lock does no I/O. A sealed window cannot create a later
        // lazy task that escapes exit's retirement census.
        let request = uuid::Uuid::new_v4();
        self.editor_root_load = Some(request);
        let intake = match (&self.recovery_endpoint, &self.edited_root_admissions) {
            (Some(endpoint), Some(admissions)) => {
                let ticket = admissions.lock().unwrap_or_else(|error| error.into_inner()).reserve();
                ticket.map_err(str::to_owned).and_then(|ticket| ticket.begin_preparation().map_err(|error| error.to_string()))
                    .map(|work| Intake { endpoint: endpoint.clone(), work })
            }
            _ => Err("Original root registration is unavailable. Reopen this file after the local recovery service is available.".into()),
        };
        file_preparation_task(
            identity,
            request,
            self.document_tone,
            absent,
            self.original_editor_root.clone(),
            self.retained_editor_root_admissions(),
            intake,
        )
    }

    fn root_admissions_sealed(&self) -> bool {
        self.edited_root_admissions.as_ref().is_some_and(|ledger| {
            ledger
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_sealed()
        })
    }

    pub(super) fn editor_file_admitted(
        &mut self,
        identity: AsyncIdentity,
        admission: RootFileAdmission,
    ) -> Task<RepositoryAction> {
        if self.editor_root_load != Some(admission.request)
            || !self.accepts_selected(&identity)
            || self.workspace_edit_pending()
            || self.root_admissions_sealed()
        {
            return Task::none();
        }
        self.editor_root_load = None;
        let RootFileAdmission {
            result,
            indentation,
            root,
            ..
        } = admission;
        let result = if is_mutable(&result)
            && !root
                .as_ref()
                .is_some_and(crate::editor::FileRootIdentity::has_active_admission)
        {
            Err(FileIoError::Io(
                "Original edited-root admission closed before the file could become editable."
                    .into(),
            ))
        } else {
            result
        };
        self.finish_configured_file(identity, result, indentation, root)
    }

    pub(super) fn editor_file_prepared(
        &mut self,
        identity: AsyncIdentity,
        preparation: RootFilePreparation,
    ) -> Task<RepositoryAction> {
        if self.editor_root_load != Some(preparation.request)
            || !self.accepts_selected(&identity)
            || self.workspace_edit_pending()
            || self.root_admissions_sealed()
        {
            preparation.refuse();
            return Task::none();
        }
        let RootFilePreparation {
            request,
            result,
            indentation,
            root,
            prepared,
        } = preparation;
        let Some(prepared) = prepared else {
            return self.editor_file_admitted(
                identity,
                RootFileAdmission {
                    request,
                    result,
                    indentation,
                    root,
                },
            );
        };
        let enrollment = match prepared.begin_enrollment() {
            Ok(work) => work,
            Err(error) => {
                return self.editor_file_admitted(
                    identity,
                    RootFileAdmission {
                        request,
                        result: Err(FileIoError::Io(error.to_string())),
                        indentation,
                        root: None,
                    },
                );
            }
        };
        // EnrollmentWork is already fenced in the window's preheld ticket.
        // Dropping this lazy task or a panicking worker retains that owner.
        cosmic::task::future(async move {
            let loaded = tokio::task::spawn_blocking(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                let admission = enrollment
                    .run(&CancellationToken::new(), deadline)
                    .map_err(|error| FileIoError::Io(error.to_string()))?;
                let root = root
                    .ok_or_else(|| {
                        FileIoError::Io("Original file root was lost before enrollment.".into())
                    })?
                    .with_admission(admission, deadline)?;
                Ok::<_, FileIoError>((result, root))
            })
            .await
            .map_err(|error| FileIoError::Io(format!("Root enrollment worker failed: {error}")))
            .and_then(|result| result);
            let (result, root) = match loaded {
                Ok((result, root)) => (result, Some(root)),
                Err(error) => (Err(error), None),
            };
            RepositoryAction::EditorFileAdmitted {
                identity,
                admission: RootFileAdmission {
                    request,
                    result,
                    indentation,
                    root,
                },
            }
        })
    }
}

fn file_preparation_task(
    identity: AsyncIdentity,
    request: uuid::Uuid,
    tone: crate::editor::PdfTone,
    absent: Option<(PathBuf, crate::editor::FileRootIdentity)>,
    source: Option<crate::editor::FileRootIdentity>,
    existing: Vec<crate::editor::FileRootIdentity>,
    intake: Result<Intake, String>,
) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    let path = identity.path.clone();
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            let indentation = crate::editor_indent::read(&worktree, &path);
            let opened = crate::editor::worktree_root(&worktree).and_then(|root| crate::editor::FileRootIdentity::of(Arc::new(root)));
            let (result, root) = match opened {
                Ok(root) => {
                    let absent = absent.as_ref().filter(|(_, expected)| *expected == root).map(|(source, _)| source.as_path());
                    (crate::editor::load_file_with_saved_absence(&root.retained_root(), &path, tone, absent), Some(root))
                }
                Err(error) => (Err(error), None),
            };
            if !is_mutable(&result) {
                if let Ok(intake) = intake { intake.work.finish_read_only(); }
                return RootFilePreparation { request, result, indentation, root, prepared: None };
            }
            let intake = match intake {
                Ok(intake) => intake,
                Err(error) => return RootFilePreparation { request, result: Err(FileIoError::Io(error)), indentation, root, prepared: None },
            };
            let root = root.expect("a mutable read retains its opened root");
            if source.as_ref().is_some_and(|source| !source.has_active_admission() || !source.same_directory_candidate(&root)) {
                intake.work.finish_read_only();
                return RootFilePreparation { request, result: Err(FileIoError::Io("The original editor Source was moved or replaced. Reopen its checkout before editing this file.".into())), indentation, root: Some(root), prepared: None };
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            if let Some(previous) = existing.iter().find(|previous| previous.same_directory_candidate(&root)) {
                let admitted = previous.retained_admission().ok_or_else(|| FileIoError::Io("The original root admission has closed.".into()))
                    .and_then(|admission| root.with_admission(admission, deadline));
                intake.work.finish_read_only();
                return match admitted {
                    Ok(root) => RootFilePreparation { request, result, indentation, root: Some(root), prepared: None },
                    Err(error) => RootFilePreparation { request, result: Err(error), indentation, root: None, prepared: None },
                };
            }
            let owner = crate::recovery_domain::RecoveryPlacementClient::connect_local_until(intake.endpoint, deadline)
                .and_then(|client| client.prepare_edited_root_owner(root.retained_root()));
            match owner {
                Ok(owner) => RootFilePreparation { request, result, indentation, root: Some(root), prepared: Some(intake.work.prepared(owner)) },
                Err(error) => {
                    intake.work.failed(&error);
                    RootFilePreparation { request, result: Err(FileIoError::Io(error.to_string())), indentation, root: Some(root), prepared: None }
                }
            }
        }).await;
        let preparation = result.unwrap_or_else(|error| RootFilePreparation {
            request,
            result: Err(FileIoError::Io(format!(
                "File admission worker failed: {error}"
            ))),
            indentation: crate::editor_indent::Overrides::default(),
            root: None,
            prepared: None,
        });
        RepositoryAction::EditorFilePrepared {
            identity,
            preparation,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;

    async fn finish(runtime: &mut RepositoryRuntime, task: Task<RepositoryAction>) {
        let mut pending = vec![task];
        while let Some(task) = pending.pop() {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(action) = action {
                        pending.push(runtime.update(action));
                    }
                }
            }
        }
    }

    async fn receipt(task: Task<RepositoryAction>) -> RepositoryAction {
        let mut stream = iced::runtime::task::into_stream(task).expect("actual file worker task");
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(action) = action {
                return action;
            }
        }
        panic!("file worker must return its owned receipt");
    }

    #[tokio::test]
    async fn unconfigured_runtime_refuses_editable_but_preserves_binary_viewing() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("source.txt"), "keep original").unwrap();
        std::fs::write(directory.path().join("binary.bin"), [0, 255, 1]).unwrap();
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::selection_fixture(card, "source.txt");
        runtime.descriptor.worktree = directory.path().to_path_buf();
        let task = runtime.open_file(card, "source.txt".into());
        finish(&mut runtime, task).await;
        assert!(
            matches!(&runtime.document, DocumentState::Failed { message, .. } if message.contains("registration is unavailable"))
        );
        assert!(runtime.editor_text().is_none());
        assert_eq!(
            std::fs::read(directory.path().join("source.txt")).unwrap(),
            b"keep original"
        );
        let task = runtime.open_file(card, "binary.bin".into());
        finish(&mut runtime, task).await;
        assert!(matches!(
            runtime.document,
            DocumentState::ReadOnly {
                reason: ReadOnlyReason::BinaryPreview(_),
                ..
            }
        ));
    }

    #[test]
    fn raw_loaded_message_cannot_create_an_unregistered_editable_document() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "source.txt");
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity,
            result: Ok(LoadedFile {
                source: "source.txt".into(),
                content: FileContent::Editable {
                    text: "unregistered".into(),
                    content_hash: blake3::hash(b"unregistered"),
                    markdown: false,
                },
            }),
        }));
        assert!(matches!(runtime.document, DocumentState::Failed { .. }));
        assert!(runtime.editor_text().is_none());
    }

    #[tokio::test]
    async fn a_sealed_window_cannot_start_later_file_registration() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("source.txt"), "original").unwrap();
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::selection_fixture(card, "source.txt");
        runtime.descriptor.worktree = directory.path().to_path_buf();
        let ledger = Arc::new(std::sync::Mutex::new(
            crate::edited_root_admission::EditedRootAdmissions::default(),
        ));
        ledger.lock().unwrap().seal_new_admissions();
        runtime.set_edited_root_admissions(ledger.clone());
        runtime.set_recovery_endpoint(
            crate::recovery_domain::LocalRecoveryEndpoint::fixture(
                directory.path().join("never-connect.sock"),
                None,
            )
            .unwrap(),
        );
        let task = runtime.open_file(card, "source.txt".into());
        assert!(
            ledger.lock().unwrap().is_empty(),
            "no late admission was reserved before returning the task"
        );
        finish(&mut runtime, task).await;
        assert!(!matches!(runtime.document, DocumentState::Editable(_)));
        assert!(ledger.lock().unwrap().is_empty());
        assert!(!directory.path().join("never-connect.sock").exists());
    }

    #[tokio::test]
    async fn old_runtime_read_cannot_adopt_into_same_path_and_generation() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("binary.bin"), [0, 255, 1]).unwrap();
        let card = CardId::new();
        let (mut old, _) = RepositoryRuntime::selection_fixture(card, "binary.bin");
        old.descriptor.worktree = directory.path().to_path_buf();
        let task = old.open_file(card, "binary.bin".into());
        let receipt = receipt(task).await;
        let RepositoryAction::EditorFilePrepared {
            identity: old_identity,
            ..
        } = &receipt
        else {
            panic!("actual phase-one file receipt");
        };
        let (mut replacement, _) = RepositoryRuntime::selection_fixture(card, "binary.bin");
        replacement.descriptor.worktree = directory.path().to_path_buf();
        let current = replacement.open_file(card, "binary.bin".into());
        assert!(
            replacement.accepts_selected(old_identity),
            "numeric generation/path alone really collide"
        );
        let nonce = replacement.editor_root_load;
        drop(replacement.update(receipt));
        assert_eq!(replacement.editor_root_load, nonce);
        assert!(matches!(replacement.document, DocumentState::Loading));
        finish(&mut replacement, current).await;
        assert!(matches!(
            replacement.document,
            DocumentState::ReadOnly {
                reason: ReadOnlyReason::BinaryPreview(_),
                ..
            }
        ));
    }

    // This peer exercises actual client framing and original descriptor
    // transfer. Its replies are SCRIPTED, not daemon Registry19 acceptance.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn framed_original_pair_is_installed_before_enrollment_and_reused_by_undo() {
        use crate::edited_root_admission::{
            AdmissionPhase, EditedRootAdmissions,
            test_peer::{Event, ScriptedEditedRootPeer},
        };
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("source.txt"), "original").unwrap();
        std::fs::write(directory.path().join("other.txt"), "second").unwrap();
        let original = crate::editor::worktree_root(directory.path()).unwrap();
        let peer = ScriptedEditedRootPeer::start(&original);
        let ledger = Arc::new(std::sync::Mutex::new(EditedRootAdmissions::default()));
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::selection_fixture(card, "source.txt");
        runtime.descriptor.worktree = directory.path().to_path_buf();
        runtime.set_recovery_endpoint(peer.endpoint());
        runtime.set_edited_root_admissions(ledger.clone());
        let task = runtime.open_file(card, "source.txt".into());
        assert_eq!(
            ledger.lock().unwrap().len(),
            1,
            "window owns the ticket before the lazy phase-one task"
        );
        let prepared = receipt(task).await;
        assert!(matches!(
            prepared,
            RepositoryAction::EditorFilePrepared { .. }
        ));
        assert!(
            peer.events().is_empty(),
            "phase one performed no negative registration"
        );
        let ticket = ledger.lock().unwrap().tickets().next().unwrap().clone();
        assert_eq!(ticket.phase(), AdmissionPhase::Prepared);
        let task = runtime.update(prepared);
        assert!(
            ticket.has_worker(),
            "phase two is fenced before returning a lazy task"
        );
        assert!(peer.events().is_empty());
        finish(&mut runtime, task).await;
        assert_eq!(runtime.editor_text().as_deref(), Some("original"));
        let task = runtime.open_file(card, "other.txt".into());
        finish(&mut runtime, task).await;
        assert_eq!(runtime.editor_text().as_deref(), Some("second"));
        assert_eq!(
            peer.events()
                .iter()
                .filter(|event| **event == Event::Prepared)
                .count(),
            1,
            "same original checkout reuses its real lease"
        );
        let roots = runtime.retained_editor_root_admissions();
        assert!(!roots.is_empty());
        let pair = roots[0].retained_admission().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        let foreign = Arc::new(crate::editor::worktree_root(foreign.path()).unwrap());
        assert!(
            crate::editor::FileRootIdentity::admitted(
                foreign,
                pair.clone(),
                Instant::now() + Duration::from_secs(2)
            )
            .is_err(),
            "an actual successful pair cannot authorize a different original root"
        );
        drop(roots);
        drop(runtime);
        ledger.lock().unwrap().request_unowned_retirements();
        assert!(
            ledger.lock().unwrap().next_retirement().unwrap().is_none(),
            "actual queued/Undo owner still retains the raw lease"
        );
        drop(pair);
        ledger.lock().unwrap().request_unowned_retirements();
        let retirement = ledger
            .lock()
            .unwrap()
            .next_retirement()
            .unwrap()
            .expect("last actual holder drained");
        retirement
            .run(Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert_eq!(ticket.phase(), AdmissionPhase::Settled);
        drop(ticket);
        drop(ledger);
        let events = peer.finish();
        assert_eq!(
            events
                .iter()
                .filter(|event| **event == Event::Released)
                .count(),
            1
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn held_actual_enrollment_receipt_cannot_publish_in_replacement_runtime() {
        use crate::edited_root_admission::{
            EditedRootAdmissions,
            test_peer::{Event, ScriptedEditedRootPeer},
        };
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("source.txt"), "original").unwrap();
        let original = crate::editor::worktree_root(directory.path()).unwrap();
        let peer = ScriptedEditedRootPeer::start(&original);
        let ledger = Arc::new(std::sync::Mutex::new(EditedRootAdmissions::default()));
        let card = CardId::new();
        let (mut old, _) = RepositoryRuntime::selection_fixture(card, "source.txt");
        old.descriptor.worktree = directory.path().to_path_buf();
        old.set_recovery_endpoint(peer.endpoint());
        old.set_edited_root_admissions(ledger.clone());
        let task = old.open_file(card, "source.txt".into());
        let prepared = receipt(task).await;
        let task = old.update(prepared);
        let admitted = receipt(task).await;
        let RepositoryAction::EditorFileAdmitted { identity, .. } = &admitted else {
            panic!("actual successful enrollment worker receipt");
        };
        assert!(peer.events().contains(&Event::Activated));
        drop(old);
        // A new Runtime reuses the numeric path/generation, while its actual
        // worker reads a different current content classification.
        std::fs::write(directory.path().join("source.txt"), [0, 255, 1]).unwrap();
        let (mut replacement, _) = RepositoryRuntime::selection_fixture(card, "source.txt");
        replacement.descriptor.worktree = directory.path().to_path_buf();
        let current = replacement.open_file(card, "source.txt".into());
        assert!(replacement.accepts_selected(identity));
        drop(replacement.update(admitted));
        assert!(matches!(replacement.document, DocumentState::Loading));
        finish(&mut replacement, current).await;
        assert!(matches!(
            replacement.document,
            DocumentState::ReadOnly {
                reason: ReadOnlyReason::BinaryPreview(_),
                ..
            }
        ));
        drop(replacement);
        ledger.lock().unwrap().request_unowned_retirements();
        let retirement = ledger
            .lock()
            .unwrap()
            .next_retirement()
            .unwrap()
            .expect("late completion dropped only its passive original pair");
        retirement
            .run(Instant::now() + Duration::from_secs(2))
            .unwrap();
        drop(ledger);
        assert!(peer.finish().contains(&Event::Released));
    }
}
