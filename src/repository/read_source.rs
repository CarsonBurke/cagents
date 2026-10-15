//! Original reader ownership, retained across rendering and asynchronous preparation.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone, Debug)]
pub struct GitReadSource {
    binding: OperationId,
    authority: WorktreeAuthority,
    mutation_revision: Arc<AtomicU64>,
    available: Arc<AtomicBool>,
}

impl GitReadSource {
    /// The host supplies its already initialized original actor and authority.
    /// This constructor does not discover a worktree through its current path.
    pub fn new(binding: OperationId, authority: WorktreeAuthority, mutation_revision: Arc<AtomicU64>) -> Self {
        Self { binding, authority, mutation_revision, available: Arc::new(AtomicBool::new(true)) }
    }

    pub fn id(&self) -> OperationId { self.binding }
    pub fn authority(&self) -> &WorktreeAuthority { &self.authority }
    pub fn mutation_revision(&self) -> Arc<AtomicU64> { self.mutation_revision.clone() }
    pub fn is_available(&self) -> bool { self.available.load(Ordering::Acquire) }
    /// Project removal/replacement permanently retires all clones of this source.
    pub fn invalidate(&self) { self.available.store(false, Ordering::Release); }

    /// Pure comparison for native event handlers; no filesystem operations.
    pub fn same_source(&self, other: &Self) -> bool {
        self.binding == other.binding
            && self.authority.same_scope(&other.authority)
            && Arc::ptr_eq(&self.mutation_revision, &other.mutation_revision)
            && Arc::ptr_eq(&self.available, &other.available)
    }

    fn validate(&self, revision: u64) -> Result<(), RepositoryError> {
        if !self.is_available() || self.mutation_revision.load(Ordering::Acquire) != revision {
            return Err(ValidationError::Io("The original Git reader was retired or changed; reload this source before retrying".into()).into());
        }
        self.authority.validate()
    }

    /// Only the actual scoped read issues a receipt. Call on a blocking worker.
    pub fn diff(&self, request: &DiffRequest, cancellation: &CancellationToken) -> Result<(DiffContent, GitReadReceipt), RepositoryError> {
        let revision = self.mutation_revision.load(Ordering::Acquire);
        self.validate(revision)?;
        let service = RepositoryService::system("git").with_worktree_authority(self.authority.clone())?;
        let content = service.diff(request, cancellation)?;
        self.validate(revision)?;
        Ok((content, GitReadReceipt { source: self.clone(), revision }))
    }
}

/// An immutable receipt for bytes read by the original scoped Git worker.
#[derive(Clone, Debug)]
pub struct GitReadReceipt {
    source: GitReadSource,
    revision: u64,
}

impl GitReadReceipt {
    pub fn source(&self) -> &GitReadSource { &self.source }
    pub fn revision(&self) -> u64 { self.revision }
    pub fn is_current(&self) -> bool {
        self.source.is_available() && self.source.mutation_revision.load(Ordering::Acquire) == self.revision
    }
    pub fn matches_source(&self, source: &GitReadSource) -> bool {
        self.is_current() && self.source.same_source(source)
    }
    /// Worker-side physical validation, in addition to pure native comparisons.
    pub fn validate(&self) -> Result<(), RepositoryError> { self.source.validate(self.revision) }
}

impl PartialEq for GitReadReceipt {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision && self.source.same_source(&other.source)
    }
}
impl Eq for GitReadReceipt {}

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn git(root: &Path, args: &[&str]) {
        let mut command = CommandSpec::new("git")
            .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-c", "init.templateDir="])
            .arg("-C").arg(root).args(args)
            .timeout(Duration::from_secs(5)).output_limits(16 * 1024, 16 * 1024);
        for (name, _) in std::env::vars_os().filter(|(name, _)| name.as_encoded_bytes().starts_with(b"GIT_")) {
            command = command.env_remove(name);
        }
        command = command.env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null");
        let output = SystemProcessExecutor.execute(&command, &CancellationToken::default()).unwrap();
        assert!(output.success, "{}", String::from_utf8_lossy(&output.stderr));
    }

    fn read(root: &Path, source: &GitReadSource) -> GitReadReceipt {
        let (content, receipt) = source.diff(&DiffRequest {
            worktree: root.to_owned(), basis: DiffBasis::IndexToWorktree,
            path: RepoPath::parse("file").unwrap(), old_path: None,
        }, &CancellationToken::default()).unwrap();
        assert!(matches!(content, DiffContent::Comparison { new: DiffSide::Text { text, .. }, .. } if text == "original work\n"));
        receipt
    }

    #[test]
    fn actual_read_receipt_retains_actor_revision_and_permanent_revocation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        git(root, &["init", "-b", "main"]);
        fs::write(root.join("file"), "original work\n").unwrap();
        let authority = WorktreeAuthority::capture(root).unwrap();
        assert!(authority.same_scope(&WorktreeAuthority::capture(root).unwrap()));
        let revision = Arc::new(AtomicU64::new(0));
        let source = GitReadSource::new(OperationId::new(), authority.clone(), revision.clone());
        let receipt = read(root, &source);
        assert!(receipt.matches_source(&source.clone()));
        assert!(receipt.validate().is_ok());
        let other_actor = GitReadSource::new(OperationId::new(), authority, revision.clone());
        assert!(!receipt.matches_source(&other_actor));
        revision.fetch_add(1, Ordering::AcqRel);
        assert!(!receipt.is_current());
        let current = read(root, &source);
        source.invalidate();
        assert!(!current.is_current());
        assert!(current.validate().is_err());
        assert!(source.diff(&DiffRequest { worktree: root.to_owned(), basis: DiffBasis::IndexToWorktree,
            path: RepoPath::parse("file").unwrap(), old_path: None }, &CancellationToken::default()).is_err());
    }

    #[test]
    fn actual_read_receipt_refuses_root_and_git_metadata_replacement() {
        for replace_git in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("root");
            fs::create_dir(&root).unwrap();
            git(&root, &["init", "-b", "main"]);
            fs::write(root.join("file"), "original work\n").unwrap();
            let source = GitReadSource::new(OperationId::new(), WorktreeAuthority::capture(&root).unwrap(), Arc::new(AtomicU64::new(0)));
            let receipt = read(&root, &source);
            let moved = if replace_git { root.join(".git") } else { root.clone() };
            fs::rename(&moved, directory.path().join("original")).unwrap();
            fs::create_dir(&moved).unwrap();
            git(&root, &["init", "-b", "main"]);
            let replacement = WorktreeAuthority::capture(&root).unwrap();
            assert!(!source.authority().same_scope(&replacement));
            let newer = GitReadSource::new(OperationId::new(), replacement, Arc::new(AtomicU64::new(0)));
            assert!(!receipt.matches_source(&newer));
            assert!(receipt.validate().is_err());
        }
    }
    #[test]
    fn actual_linked_read_receipt_refuses_separate_common_directory_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let primary = directory.path().join("primary");
        let linked = directory.path().join("linked");
        fs::create_dir(&primary).unwrap();
        git(&primary, &["init", "-b", "main"]);
        fs::write(primary.join("file"), "base\n").unwrap();
        git(&primary, &["add", "file"]);
        git(&primary, &["-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "fixture"]);
        git(&primary, &["worktree", "add", "-b", "linked", linked.to_str().unwrap()]);
        fs::write(linked.join("file"), "original work\n").unwrap();
        let source = GitReadSource::new(OperationId::new(), WorktreeAuthority::capture(&linked).unwrap(), Arc::new(AtomicU64::new(0)));
        let receipt = read(&linked, &source);
        let original_common = primary.join(".git");
        let parked = directory.path().join("original-common");
        fs::rename(&original_common, &parked).unwrap();
        // Keep the original private admin at its original name, but replace
        // just the common directory containing refs/objects.
        fs::create_dir(&original_common).unwrap();
        fs::rename(parked.join("worktrees"), original_common.join("worktrees")).unwrap();
        assert!(receipt.validate().is_err());
        let replacement = WorktreeAuthority::capture(&linked).unwrap();
        assert!(!source.authority().same_scope(&replacement));
    }

}
