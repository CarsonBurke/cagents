//! Local blocking-worker ownership for recoverable filesystem operations.
//!
//! The supplied paths are a captured negative checkout census, not global root
//! registration. General acquisition stays outside captured roots; the Save
//! entry alone may use local custody bound to the actual edited parent. Original
//! source/parent descriptors and cleanup protection
//! remain held through the transaction and unresolved recovery. New external
//! repositories and topology changes after the last check are explicit limits.
use crate::{
    fs_safety::{
        OwnedPrivateSibling, PrivateSiblingCreateError, RecoveryParentCandidates,
        RecoveryPlacement, Root, StickyParent,
    },
    recovery_authority::{RecoveryAuthority, RecoveryContext, RecoverySlot},
    worktree_cleanup::Protection,
};
use std::{
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
};

mod save_local;
use save_local::{SaveLocalPlacement, SaveTargetAuthority};

#[derive(Debug, Clone)]
pub(crate) struct FilesystemRecoveryPlacement(Placement);
#[derive(Debug, Clone)]
enum Placement {
    Outside(Arc<RecoveryPlacement>, Option<Arc<SaveTargetAuthority>>),
    SaveLocal(Arc<SaveLocalPlacement>),
}
impl FilesystemRecoveryPlacement {
    fn target(&self) -> Option<&Arc<SaveTargetAuthority>> {
        match &self.0 {
            Placement::Outside(_, target) => target.as_ref(),
            Placement::SaveLocal(p) => Some(p.target()),
        }
    }
    pub(crate) fn directory(&self) -> &Root {
        match &self.0 {
            Placement::Outside(p, _) => p.directory(),
            Placement::SaveLocal(p) => p.directory(),
        }
    }
    pub(crate) fn namespace(&self) -> &crate::fs_safety::DirectoryNamespace {
        match &self.0 {
            Placement::Outside(p, _) => p.namespace(),
            Placement::SaveLocal(p) => p.namespace(),
        }
    }
    pub(crate) fn validate(&self) -> io::Result<()> {
        match &self.0 {
            Placement::Outside(p, target) => {
                if let Some(target) = target {
                    target.validate()?;
                }
                p.validate()
            }
            Placement::SaveLocal(p) => p.validate(),
        }
    }
    fn validate_for_source(&self, source: &Root) -> io::Result<()> {
        match &self.0 {
            Placement::Outside(p, target) => {
                if let Some(target) = target {
                    target.validate_for_source(source)?;
                }
                p.validate_for_source(source)
            }
            Placement::SaveLocal(p) => p.validate_for_source(source),
        }
    }
    fn validate_payload_root(&self, payload: &Root) -> io::Result<()> {
        match &self.0 {
            Placement::Outside(p, target) => {
                if let Some(target) = target {
                    target.validate()?;
                }
                p.validate_payload_root(payload)
            }
            Placement::SaveLocal(p) => p.validate_payload_root(payload),
        }
    }
}

enum BootstrapPlacement {
    Outside(Option<Arc<SaveTargetAuthority>>),
    SaveLocal(Arc<SaveTargetAuthority>),
}
impl BootstrapPlacement {
    fn target(&self) -> Option<&Arc<SaveTargetAuthority>> {
        match self {
            Self::Outside(target) => target.as_ref(),
            Self::SaveLocal(target) => Some(target),
        }
    }
}

#[derive(Debug)]
pub(crate) struct RecoveryAcquireError {
    pub(crate) message: String,
    pub(crate) authority: Option<RecoveryAuthority>,
}
impl From<io::Error> for RecoveryAcquireError {
    fn from(error: io::Error) -> Self {
        Self {
            message: error.to_string(),
            authority: None,
        }
    }
}
impl std::fmt::Display for RecoveryAcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RecoveryAcquireError {}

#[derive(Default)]
struct BootstrapOwner {
    sibling: Option<OwnedPrivateSibling>,
    save_live: bool,
    directory_warnings: Vec<String>,
}
impl BootstrapOwner {
    fn cleanup(&mut self) -> Result<(), String> {
        if let Some(sibling) = &mut self.sibling {
            if self.save_live {
                match sibling
                    .cleanup_save_empty()
                    .map_err(|error| error.to_string())?
                {
                    crate::fs_safety::SaveSiblingDurability::Confirmed => {}
                    crate::fs_safety::SaveSiblingDurability::Unconfirmed(error) => {
                        tracing::warn!(%error, "Save private sibling is removed but directory durability is unconfirmed");
                        self.directory_warnings.push(format!(
                            "removed private sibling durability unconfirmed: {error}"
                        ))
                    }
                }
            } else {
                sibling.cleanup_empty().map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }
}
struct BootstrapCleanup {
    owner: Arc<Mutex<BootstrapOwner>>,
    slot: Option<RecoverySlot>,
}

pub(crate) struct FilesystemRecoveryReservation {
    context: FilesystemRecoveryContext,
    slot: RecoverySlot,
}
impl FilesystemRecoveryReservation {
    pub(crate) fn into_parts(self) -> (FilesystemRecoveryContext, RecoverySlot) {
        (self.context, self.slot)
    }
}

#[derive(Clone)]
pub(crate) struct FilesystemRecoveryContext {
    placement: FilesystemRecoveryPlacement,
    context: RecoveryContext,
    bootstrap: Option<Arc<Mutex<BootstrapCleanup>>>,
}
impl std::fmt::Debug for FilesystemRecoveryContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemRecoveryContext")
            .field("placement", &self.placement)
            .finish_non_exhaustive()
    }
}
impl FilesystemRecoveryContext {
    /// No recovery directory is created until a protected, same-filesystem
    /// parent outside the source and captured known checkout roots is held.
    fn acquire_outside(
        source: &Root,
        known: &[PathBuf],
        target: Option<Arc<SaveTargetAuthority>>,
    ) -> Result<Self, RecoveryAcquireError> {
        if let Some(target) = &target {
            target.validate_for_source(source)?;
        }
        if known.len() > 256 {
            return Err(io::Error::other(
                "The known checkout census exceeds the bounded recovery inspection; no file was changed",
            ).into());
        }
        let protection = crate::worktree_cleanup::protect_root_operation(source)?;
        let source = match &target {
            Some(target) => target.source().clone(),
            None => Arc::new(source.try_clone()?),
        };
        let mut excluded = Vec::with_capacity(known.len());
        for path in known {
            match Root::open(path) {
                Ok(root) => excluded.push(Arc::new(root)),
                // An absent negative-census spelling cannot contain the
                // current holding. The actual source is separately pinned.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let excluded: Arc<[Arc<Root>]> = excluded.into();
        let mut last_error = None;
        let mut sticky_candidates = Vec::new();
        for parent in RecoveryParentCandidates::new(source.clone())? {
            let parent = parent?;
            match RecoveryPlacement::acquire(parent.clone(), source.clone(), excluded.clone()) {
                Ok(placement) => {
                    return Self::from_placement(
                        &source,
                        protection,
                        placement,
                        (),
                        target.clone(),
                    )
                    .map_err(Into::into);
                }
                Err(error) => last_error = Some(error),
            }
            if let Ok(sticky) = StickyParent::acquire(parent.clone()) {
                sticky_candidates.push((parent, sticky));
            }
        }
        // Preserve ordinary protected-parent preference. A usable higher
        // parent needs no new bootstrap and must not be hidden by a lower
        // sticky candidate. Both candidate sets and the walk are bounded.
        let mut selected_sticky = None;
        for (parent, sticky) in sticky_candidates {
            match crate::fs_safety::validate_recovery_parent(&parent, &source, &excluded) {
                Ok(()) => {
                    selected_sticky = Some(sticky);
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        // Release every unselected candidate before reserving the measured
        // bootstrap capacity and performing its first filesystem effect.
        if let Some(sticky) = selected_sticky {
            return Self::bootstrap(
                source,
                excluded,
                protection,
                sticky,
                BootstrapPlacement::Outside(target),
            );
        }
        Err(last_error.unwrap_or_else(|| io::Error::other(
            "No protected recovery parent outside the captured checkout roots exists on the source filesystem; no file was changed",
        )).into())
    }

    /// Bind custody to the actual retained edited parent, while original
    /// checkout identity/path and both cleanup leases remain authoritative.
    pub(crate) fn acquire_for_save(
        source: &Root,
        relative: &std::path::Path,
        known: &[PathBuf],
    ) -> Result<Self, RecoveryAcquireError> {
        if known.len() > 256 {
            return Err(
                io::Error::other("Save recovery exceeds the bounded checkout census").into(),
            );
        }
        let target = SaveTargetAuthority::acquire(Arc::new(source.try_clone()?), relative)?;
        let root_stamp = crate::fs_safety::recovery_directory_stamp(source)?;
        let parent_stamp = crate::fs_safety::recovery_directory_stamp(target.parent())?;
        if root_stamp.device == parent_stamp.device && root_stamp.mount_id == parent_stamp.mount_id
        {
            match Self::acquire_outside(source, known, Some(target.clone())) {
                Ok(context) => return Ok(context),
                Err(error) if error.authority.is_some() => return Err(error),
                Err(_) => {} // No unresolved effect: qualified local custody may work.
            }
        }
        target.validate()?;
        let protection = crate::worktree_cleanup::protect_root_operation(source)?;
        match SaveLocalPlacement::acquire(target.clone()) {
            Ok(local) => {
                let descriptors = target
                    .retained_descriptor_count()
                    .checked_add(local.retained_descriptor_count())
                    .and_then(|count| count.checked_add(protection.retained_descriptor_count()))
                    .ok_or_else(|| {
                        io::Error::other("Save-local descriptor accounting overflowed")
                    })?;
                let context =
                    RecoveryContext::with_guard((protection, target, local.clone()), descriptors)
                        .map_err(|message| RecoveryAcquireError {
                        message,
                        authority: None,
                    })?;
                Ok(Self {
                    placement: FilesystemRecoveryPlacement(Placement::SaveLocal(local)),
                    context,
                    bootstrap: None,
                })
            }
            Err(ref _local_error) => {
                #[cfg(test)]
                {
                    use std::io::Write as _;
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "retained-save diagnostic: stage=save-local-placement; kind={:?}; \
                         raw_errno={:?}; error={}",
                        _local_error.kind(),
                        _local_error.raw_os_error(),
                        _local_error
                    );
                }
                let sticky = StickyParent::acquire(target.parent().clone());
                #[cfg(test)]
                if let Err(error) = &sticky {
                    use std::io::Write as _;
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "retained-save diagnostic: stage=save-local-sticky-fallback; kind={:?}; \
                         raw_errno={:?}; error={}",
                        error.kind(),
                        error.raw_os_error(),
                        error
                    );
                }
                let sticky = sticky?;
                Self::bootstrap(
                    target.source().clone(),
                    Arc::from([]),
                    protection,
                    sticky,
                    BootstrapPlacement::SaveLocal(target),
                )
            }
        }
    }

    fn bootstrap(
        source: Arc<Root>,
        excluded: Arc<[Arc<Root>]>,
        protection: Protection,
        sticky: Arc<StickyParent>,
        policy: BootstrapPlacement,
    ) -> Result<Self, RecoveryAcquireError> {
        let target = policy.target().cloned();
        if let Some(target) = &target {
            target.validate()?;
        }
        let save_live = policy.target().is_some();
        let owner = Arc::new(Mutex::new(BootstrapOwner {
            save_live,
            ..BootstrapOwner::default()
        }));
        let mut descriptors = std::collections::HashSet::from([source.descriptor()]);
        descriptors.extend(excluded.iter().map(|root| root.descriptor()));
        // Reserve capacity BEFORE mkdir: one sticky-parent handle and at most
        // five additional handles cover child, placement namespace, and the
        // bounded transient ancestry/cleanup walk. This conservative local
        // reservation remains charged through unresolved cleanup.
        let capacity = descriptors.len()
            + protection.retained_descriptor_count()
            + sticky.retained_descriptor_count()
            + target.as_ref().map_or(0, |target| {
                target.retained_descriptor_count().saturating_sub(1)
            })
            + 5;
        let context = RecoveryContext::with_guard(
            (
                source.clone(),
                excluded.clone(),
                protection,
                sticky.clone(),
                owner.clone(),
                target,
            ),
            capacity,
        )
        .map_err(|message| RecoveryAcquireError {
            message,
            authority: None,
        })?;
        let slot = context.reserve().map_err(|message| RecoveryAcquireError {
            message,
            authority: None,
        })?;
        let creation = if save_live {
            sticky.clone().create_save_private_sibling()
        } else {
            sticky
                .clone()
                .create_private_sibling()
                .map(|sibling| (sibling, crate::fs_safety::SaveSiblingDurability::Confirmed))
        };
        let (sibling, durability) = match creation {
            Ok(created) => created,
            Err(PrivateSiblingCreateError::Rejected(error)) => return Err(error.into()),
            Err(PrivateSiblingCreateError::Retained { cause, sibling }) => {
                owner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .sibling = Some(sibling);
                let retry_owner = owner.clone();
                let authority = slot.retain(
                    "Private recovery parent preparation needs cleanup".into(),
                    move || {
                        retry_owner
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .cleanup()
                    },
                );
                return Err(RecoveryAcquireError {
                    message: cause.to_string(),
                    authority: Some(authority),
                });
            }
        };
        if let crate::fs_safety::SaveSiblingDurability::Unconfirmed(error) = durability {
            owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .directory_warnings
                .push(format!(
                    "private sibling preparation durability unconfirmed: {error}"
                ));
        }
        let placement = match policy {
            BootstrapPlacement::Outside(target) => sibling
                .directory_arc()
                .and_then(|directory| RecoveryPlacement::acquire(directory, source, excluded))
                .map(|placement| {
                    FilesystemRecoveryPlacement(Placement::Outside(placement, target))
                }),
            BootstrapPlacement::SaveLocal(target) => {
                SaveLocalPlacement::from_sibling(target, &sibling, sticky)
                    .map(|placement| FilesystemRecoveryPlacement(Placement::SaveLocal(placement)))
            }
        };
        owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sibling = Some(sibling);
        let cleanup = Arc::new(Mutex::new(BootstrapCleanup {
            owner,
            slot: Some(slot),
        }));
        match placement {
            Ok(placement) => Ok(Self {
                placement,
                context,
                bootstrap: Some(cleanup),
            }),
            Err(error) => {
                let authority = Self::finish_bootstrap(&cleanup, None);
                Err(RecoveryAcquireError {
                    message: error.to_string(),
                    authority,
                })
            }
        }
    }

    /// Resolve the owned sibling only after all leaf payloads have resolved.
    /// A reserved retry owns both original guards and the exact creation
    /// receipt; Drop never mutates names and never recursively removes data.
    pub(crate) fn finish(&self, leaf: Option<RecoveryAuthority>) -> Option<RecoveryAuthority> {
        self.bootstrap
            .as_ref()
            .and_then(|cleanup| Self::finish_bootstrap(cleanup, leaf))
    }
    pub(crate) fn take_directory_warnings(&self) -> Vec<String> {
        let Some(cleanup) = &self.bootstrap else {
            return Vec::new();
        };
        let owner = cleanup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .owner
            .clone();
        let mut owner = owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut owner.directory_warnings)
    }
    fn finish_bootstrap(
        cleanup: &Arc<Mutex<BootstrapCleanup>>,
        leaf: Option<RecoveryAuthority>,
    ) -> Option<RecoveryAuthority> {
        let mut cleanup = cleanup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = cleanup.slot.take()?;
        let owner = cleanup.owner.clone();
        let pending_leaf = leaf.filter(|leaf| !leaf.is_resolved());
        if pending_leaf.is_none()
            && owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .cleanup()
                .is_ok()
        {
            return None;
        }
        Some(slot.retain(
            "Private recovery parent cleanup remains pending".into(),
            move || {
                if let Some(leaf) = &pending_leaf {
                    leaf.retry()?;
                }
                owner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .cleanup()
            },
        ))
    }

    fn from_placement(
        source: &Root,
        protection: Protection,
        placement: Arc<RecoveryPlacement>,
        owner: impl Send + Sync + 'static,
        target: Option<Arc<SaveTargetAuthority>>,
    ) -> io::Result<Self> {
        placement.validate_for_source(source)?;
        if !protection.protects_root(source)? {
            return Err(io::Error::other(
                "Recovery cleanup protection belongs to another source root",
            ));
        }
        let descriptors = placement
            .retained_descriptor_count()
            .checked_add(protection.retained_descriptor_count())
            .and_then(|count| {
                count.checked_add(target.as_ref().map_or(0, |target| {
                    target.retained_descriptor_count().saturating_sub(1)
                }))
            })
            .ok_or_else(|| io::Error::other("Recovery descriptor accounting overflowed"))?;
        // The SAME context owns these actual native handles. Its charge stays
        // until the guard tuple has dropped, including unresolved leaf retries.
        let context = RecoveryContext::with_guard(
            (owner, protection, placement.clone(), target.clone()),
            descriptors,
        )
        .map_err(io::Error::other)?;
        Ok(Self {
            placement: FilesystemRecoveryPlacement(Placement::Outside(placement, target)),
            context,
            bootstrap: None,
        })
    }

    pub(crate) fn validate_save_parent(
        &self,
        parent: &Root,
        relative: &std::path::Path,
        name: &std::ffi::OsStr,
    ) -> io::Result<()> {
        if let Some(target) = self.placement.target() {
            target.validate()?;
            if relative != target.relative_path()
                || name != target.name()
                || crate::fs_safety::recovery_directory_stamp(parent)?
                    != crate::fs_safety::recovery_directory_stamp(target.parent())?
            {
                return Err(io::Error::other(
                    "Save parent no longer matches its captured original target",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn validate_for_source(&self, source: &Root) -> io::Result<()> {
        self.placement.validate_for_source(source)
    }
    pub(crate) fn placement(&self) -> &FilesystemRecoveryPlacement {
        &self.placement
    }
    pub(crate) fn validate_payload_root(&self, payload: &Root) -> io::Result<()> {
        self.placement.validate_payload_root(payload)
    }
    pub(crate) fn reserve_directory(&self) -> Result<FilesystemRecoveryReservation, String> {
        let slot = self.reserve()?;
        Ok(FilesystemRecoveryReservation {
            context: self.clone(),
            slot,
        })
    }
    pub(crate) fn reserve(&self) -> Result<RecoverySlot, String> {
        self.placement
            .validate()
            .map_err(|error| error.to_string())?;
        self.context.reserve()
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod fixtures {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn fixture_context(source: &Root) -> io::Result<FilesystemRecoveryContext> {
        fixture_context_with_guard(source, ())
    }
    pub(crate) fn fixture_context_with_guard(
        source: &Root,
        guard: impl Send + Sync + 'static,
    ) -> io::Result<FilesystemRecoveryContext> {
        let runtime = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let outside = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let protection =
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false)?;
        let source = Arc::new(source.try_clone()?);
        // Keep the original owners alive while reporting either setup error,
        // including when from_placement drops its supplied owner on failure.
        let owner = Arc::new((guard, runtime, outside));
        let report_setup_error = |stage, error: &io::Error| {
            use std::io::Write as _;
            // These lexical spellings are diagnostics, never placement authority.
            // A failed stderr write must not replace the original setup error.
            let _ = writeln!(
                io::stderr().lock(),
                "recovery fixture setup: stage={stage} error_kind={:?} source_reporting_spelling={:?} source_is_absolute={} runtime_reporting_spelling={:?} runtime_is_absolute={} outside_reporting_spelling={:?} outside_is_absolute={}",
                error.kind(),
                source.path(),
                source.path().is_absolute(),
                owner.1.path(),
                owner.1.path().is_absolute(),
                owner.2.path(),
                owner.2.path().is_absolute(),
            );
        };
        let placement = RecoveryPlacement::acquire(
            Arc::new(Root::open(owner.2.path())?),
            source.clone(),
            Arc::from([source.clone()]),
        )
        .inspect_err(|error| report_setup_error("acquire_outside", error))?;
        FilesystemRecoveryContext::from_placement(
            &source,
            protection,
            placement,
            owner.clone(),
            None,
        )
        .inspect_err(|error| report_setup_error("from_placement", error))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Charge(Arc<AtomicBool>);
    impl Drop for Charge {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    // Exercise a real sticky parent without assuming that a shared system
    // temporary directory has no enclosing checkout marker. The owning test
    // keeps both private directories alive through every cleanup/retry oracle.
    fn sticky_source_fixture() -> (tempfile::TempDir, tempfile::TempDir, Arc<StickyParent>) {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o1777)).unwrap();
        let directory = tempfile::tempdir_in(parent.path()).unwrap();
        let sticky = StickyParent::acquire(Arc::new(Root::open(parent.path()).unwrap())).unwrap();
        (parent, directory, sticky)
    }

    #[test]
    fn sticky_bootstrap_cleanup_removes_only_its_owned_empty_sibling() {
        let (_parent, directory, sticky) = sticky_source_fixture();
        let runtime = tempfile::tempdir().unwrap();
        let source = Arc::new(Root::open(directory.path()).unwrap());
        let protection =
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false)
                .unwrap();
        let context = FilesystemRecoveryContext::bootstrap(
            source.clone(),
            Arc::from([source]),
            protection,
            sticky,
            BootstrapPlacement::Outside(None),
        )
        .unwrap();
        let owned = context.placement().directory().path().to_owned();
        assert!(owned.is_dir());
        assert!(context.finish(None).is_none());
        assert!(!owned.exists());
        assert!(context.finish(None).is_none());
    }

    #[test]
    fn sticky_bootstrap_retry_resolves_leaf_before_parent_cleanup() {
        let (_parent, directory, sticky) = sticky_source_fixture();
        let runtime = tempfile::tempdir().unwrap();
        let source = Arc::new(Root::open(directory.path()).unwrap());
        let protection =
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false)
                .unwrap();
        let context = FilesystemRecoveryContext::bootstrap(
            source.clone(),
            Arc::from([source]),
            protection,
            sticky,
            BootstrapPlacement::Outside(None),
        )
        .unwrap();
        let owned = context.placement().directory().path().to_owned();
        let payload = owned.join("retained-leaf");
        std::fs::write(&payload, b"preserve until leaf retry").unwrap();
        let leaf = RecoverySlot::reserve()
            .unwrap()
            .retain("owning leaf fixture".into(), move || {
                std::fs::remove_file(&payload).map_err(|error| error.to_string())
            });
        let authority = context
            .finish(Some(leaf))
            .expect("pending leaf retains parent");
        drop(context);
        assert!(owned.join("retained-leaf").is_file());
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(!owned.exists());
    }

    #[test]
    fn sticky_bootstrap_nonempty_cleanup_retains_original_for_retry() {
        let (_parent, directory, sticky) = sticky_source_fixture();
        let runtime = tempfile::tempdir().unwrap();
        let source = Arc::new(Root::open(directory.path()).unwrap());
        let protection =
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false)
                .unwrap();
        let context = FilesystemRecoveryContext::bootstrap(
            source.clone(),
            Arc::from([source]),
            protection,
            sticky,
            BootstrapPlacement::Outside(None),
        )
        .unwrap();
        let owned = context.placement().directory().path().to_owned();
        std::fs::write(owned.join("preserve"), b"newer").unwrap();
        let authority = context
            .finish(None)
            .expect("nonempty parent retains cleanup");
        drop(context);
        assert_eq!(std::fs::read(owned.join("preserve")).unwrap(), b"newer");
        assert!(authority.retry().is_err());
        std::fs::remove_file(owned.join("preserve")).unwrap();
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(!owned.exists());
    }

    #[test]
    fn actual_preparation_failure_retains_original_file_cleanup_lease_and_charge() {
        let source = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let source_root = Arc::new(Root::open(source.path()).unwrap());
        let placement = RecoveryPlacement::acquire(
            Arc::new(Root::open(outside.path()).unwrap()),
            source_root.clone(),
            Arc::from([source_root.clone()]),
        )
        .unwrap();
        let protection =
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false)
                .unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let context = FilesystemRecoveryContext::from_placement(
            &source_root,
            protection,
            placement,
            Charge(released.clone()),
            None,
        )
        .unwrap();
        let original_file = Arc::new(tempfile::tempfile().unwrap());
        let witness = Arc::downgrade(&original_file);
        let error = crate::fs_safety::fixture_recovery_directory_preparation_failure(
            context.reserve_directory().unwrap(),
            original_file,
        );
        let authority = match error {
            crate::fs_safety::RecoveryDirectoryError::Retained { recovery, .. } => recovery,
            other => panic!("expected actual retained preparation, got {other:?}"),
        };
        drop(context);
        assert!(witness.upgrade().is_some());
        assert!(!released.load(Ordering::SeqCst));
        assert!(
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), true)
                .is_err()
        );
        assert!(authority.retry().is_err());
        assert!(witness.upgrade().is_some());
        let artifact = std::fs::read_dir(outside.path())
            .unwrap()
            .map(|entry| entry.unwrap().path().join("directory"))
            .find(|path| path.is_file())
            .expect("real producer's private partial artifact");
        assert_eq!(
            std::fs::read(&artifact).unwrap(),
            b"retained fixture artifact"
        );
        std::fs::remove_file(artifact).unwrap();
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(
            witness.upgrade().is_none(),
            "original file closes before its charge releases"
        );
        assert!(released.load(Ordering::SeqCst));
        assert!(
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), true)
                .is_ok()
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
