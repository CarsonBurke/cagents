//! Read-only safety evidence and retained roots for the explicit control APIs.
use super::*;
use crate::{
    fs_safety,
    process::{CommandSpec, ProcessExecutor, ProcessFailure, ProcessOutput},
};
use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    sync::Arc,
};

#[derive(Clone)]
pub(super) struct Selection {
    pub(super) common: Arc<fs_safety::Root>,
    pub(super) admin: Arc<fs_safety::Root>,
    pub(super) checkout: Arc<fs_safety::Root>,
}

fn same_named(root: &fs_safety::Root) -> Result<(), String> {
    let held = root.metadata().map_err(|error| error.to_string())?;
    let named = std::fs::symlink_metadata(root.path()).map_err(|error| error.to_string())?;
    if !named.is_dir() || (held.dev(), held.ino()) != (named.dev(), named.ino()) {
        return Err(
            "The original worktree directory was replaced; refresh before another request".into(),
        );
    }
    Ok(())
}

impl Selection {
    pub(super) fn capture<E: ProcessExecutor>(
        service: &Service<E>,
        repository: &Path,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<Self, String> {
        let common = service
            .common_dir(repository, cancel)
            .map_err(|error| error.to_string())?;
        let path = path.canonicalize().map_err(|error| error.to_string())?;
        let records = service
            .list_worktrees(&common, cancel)
            .map_err(|error| error.to_string())?;
        let primary = service
            .managed_primary_checkout(&common, cancel)
            .map_err(|error| error.to_string())?;
        if !records.iter().any(|record| record.path == path) && primary.as_ref() != Some(&path) {
            return Err("The selected checkout does not belong to this repository".into());
        }
        let actual = service
            .common_dir(&path, cancel)
            .map_err(|error| error.to_string())?;
        let admin = service
            .git_dir(&path, cancel)
            .map_err(|error| error.to_string())?;
        let selection = Self {
            common: Arc::new(fs_safety::Root::open(&common).map_err(|error| error.to_string())?),
            admin: Arc::new(fs_safety::Root::open(&admin).map_err(|error| error.to_string())?),
            checkout: Arc::new(fs_safety::Root::open(&path).map_err(|error| error.to_string())?),
        };
        let expected = selection
            .common
            .metadata()
            .map_err(|error| error.to_string())?;
        let actual = std::fs::metadata(actual).map_err(|error| error.to_string())?;
        if (expected.dev(), expected.ino()) != (actual.dev(), actual.ino()) {
            return Err("The selected checkout belongs to another physical repository".into());
        }
        selection.verify(cancel)?;
        Ok(selection)
    }

    pub(super) fn check_roots(&self) -> Result<(), String> {
        same_named(&self.common)?;
        same_named(&self.admin)?;
        same_named(&self.checkout)
    }

    pub(super) fn verify(&self, cancel: &CancellationToken) -> Result<(), String> {
        self.check_roots()?;
        // These are only observations of the current names. Commands that can
        // write, or contribute safety counts, use the retained roots below.
        let service = RepositoryService::system("git");
        for (observed, retained) in [
            (
                service.common_dir(self.checkout.path(), cancel),
                &self.common,
            ),
            (service.git_dir(self.checkout.path(), cancel), &self.admin),
        ] {
            let observed = std::fs::metadata(observed.map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
            let held = retained.metadata().map_err(|error| error.to_string())?;
            if (observed.dev(), observed.ino()) != (held.dev(), held.ino()) {
                return Err(
                    "The checkout's Git registration changed; refresh before another request"
                        .into(),
                );
            }
        }
        self.check_roots()
    }

    pub(super) fn service(&self) -> Service<Self> {
        RepositoryService::new("git", Arc::new(self.clone()))
    }

    pub(super) fn policy(&self, cancel: &CancellationToken) -> Result<Vec<String>, String> {
        self.service()
            .disposable_ignored_directories_at(&self.common, &self.admin, &self.checkout, cancel)
            .map_err(|error| error.to_string())
    }

    pub(super) fn notable(
        &self,
        policy: &[String],
        cancel: &CancellationToken,
    ) -> Result<Vec<crate::repository::RepoPath>, String> {
        self.service()
            .notable_ignored_files_at(&self.common, &self.admin, &self.checkout, policy, cancel)
            .map_err(|error| error.to_string())
    }

    pub(super) fn commit_count(
        &self,
        detached: bool,
        cancel: &CancellationToken,
    ) -> Result<u64, String> {
        let mut command = RepositoryService::system("git")
            .git_base(self.checkout.path(), false)
            .args(["rev-list", "--count", "HEAD", "--not", "--remotes"])
            .timeout(std::time::Duration::from_secs(30))
            .output_limits(4096, 64 * 1024);
        if detached {
            command = command.args(["--branches", "--tags"]);
        }
        let output = self
            .execute(&command, cancel)
            .and_then(|output| output.require_success(&command.program))
            .map_err(|error| error.reported_detail())?;
        if output.stderr.iter().any(|byte| !byte.is_ascii_whitespace()) {
            return Err(
                "Git could not provide complete reachability evidence; worktree retained".into(),
            );
        }
        std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .ok_or_else(|| "Git did not return a bounded commit count".into())
    }

    pub(super) fn fingerprint(
        &self,
        inspection: &crate::worktree_content::Inspection,
        policy: &[String],
        notable: &[crate::repository::RepoPath],
        unpushed: u64,
        detached: u64,
    ) -> Result<String, String> {
        let mut hash = Sha256::new();
        hash.update(
            serde_json::to_vec(&(inspection, policy, notable, unpushed, detached))
                .map_err(|error| error.to_string())?,
        );
        for path in notable {
            hash.update(path.as_os_str().as_bytes());
            hash.update([0]);
            let metadata = fs_safety::metadata_beneath(&self.checkout, Path::new(path.as_os_str()))
                .map_err(|error| error.to_string())?;
            if metadata.is_dir() {
                return Err(
                    "Ignored inventory left an uninspected directory; worktree retained".into(),
                );
            }
            for value in [
                metadata.dev(),
                metadata.ino(),
                metadata.len(),
                metadata.mtime() as u64,
                metadata.mtime_nsec() as u64,
                metadata.ctime() as u64,
                metadata.ctime_nsec() as u64,
            ] {
                hash.update(value.to_le_bytes());
            }
        }
        Ok(format!("{:x}", hash.finalize()))
    }
}

impl ProcessExecutor for Selection {
    fn execute(
        &self,
        command: &CommandSpec,
        cancel: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        let failure = |detail: String| {
            crate::process::process_failure(
                command,
                crate::process::ProcessFailureKind::Io,
                &detail,
                false,
            )
        };
        self.check_roots().map_err(failure)?;
        let mut anchored = command.clone();
        let at = anchored.arguments.windows(2).position(|pair| {
            pair[0] == OsStr::new("-C")
                && [self.common.path(), self.admin.path(), self.checkout.path()]
                    .contains(&Path::new(&pair[1]))
        });
        if let Some(at) = at {
            let option = |prefix: &str, path: PathBuf| {
                let mut value = OsString::from(prefix);
                value.push(path);
                value
            };
            anchored.arguments.splice(
                at..at + 2,
                [
                    OsString::from("-C"),
                    self.common.process_path().into_os_string(),
                    option("--git-dir=", self.admin.process_path()),
                    option("--work-tree=", self.checkout.process_path()),
                ],
            );
            anchored = anchored.env("GIT_COMMON_DIR", self.common.process_path());
        }
        let result = SystemProcessExecutor.execute(&anchored, cancel)?;
        self.check_roots().map_err(failure)?;
        if anchored
            .arguments
            .iter()
            .any(|argument| argument == "status")
            && result.stderr.iter().any(|byte| !byte.is_ascii_whitespace())
        {
            return Err(failure(
                "Git could not inspect every file; no complete safety counts are available".into(),
            ));
        }
        Ok(result)
    }
}

pub(super) fn inspect<E: ProcessExecutor>(
    service: &Service<E>,
    repository: &Path,
    path: &Path,
    cancel: &CancellationToken,
) -> Result<Reply, String> {
    let selected = Selection::capture(service, repository, path, cancel)?;
    let policy = selected.policy(cancel)?;
    let notable = selected.notable(&policy, cancel)?;
    let inspection = crate::worktree_content::inspect(
        &crate::worktree_content::Request {
            path: selected.checkout.path().to_owned(),
            targets: Vec::new(),
            merged_head: None,
        },
        cancel,
    )
    .map_err(|error| error.to_string())?;
    let status = selected
        .service()
        .status(selected.checkout.path(), cancel)
        .map_err(|error| error.to_string())?;
    let modified = status
        .entries
        .iter()
        .filter(|entry| entry.area != crate::repository::ChangeArea::Untracked)
        .map(|entry| &entry.path)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let unpushed = selected.commit_count(false, cancel)?;
    let detached = if inspection.branch.is_none() {
        selected.commit_count(true, cancel)?
    } else {
        0
    };
    let fingerprint = selected.fingerprint(&inspection, &policy, &notable, unpushed, detached)?;
    let current = crate::worktree_content::inspect(
        &crate::worktree_content::Request {
            path: selected.checkout.path().to_owned(),
            targets: Vec::new(),
            merged_head: None,
        },
        cancel,
    )
    .map_err(|error| error.to_string())?;
    if current != inspection {
        return Err(
            "The worktree changed while it was inspected; request fresh safety evidence".into(),
        );
    }
    selected.verify(cancel)?;
    // The string keeps host-only repository types out of the isolated CLI graph.
    let snapshot = serde_json::to_string(&serde_json::json!({"fingerprint":fingerprint,"modified":modified,"untracked":inspection.untracked,"notable_ignored":notable,"unpushed_commits":unpushed,"detached_only_commits":detached,"branch":inspection.branch,"head_oid":inspection.head,"content":inspection})).map_err(|error| error.to_string())?;
    Ok(Reply::Inspected {
        snapshot,
        disposable_ignored_directories: policy,
    })
}
