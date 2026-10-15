//! Reviewed PR identity carried through the existing creation journal.
use super::*;
use crate::github::{GitHubRepoKey, remote_repository_key};

/// Immutable consent for a new PR checkout. It does not authorize opening an
/// existing, different local commit or changing the selected PR source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrWorktreeCreationConsent {
    head_repository: GitHubRepoKey,
    head_branch: ValidatedBranchName,
    expected_head: Oid,
    remote: String,
    track: bool,
}

impl PrWorktreeCreationConsent {
    pub fn new(
        head_repository: GitHubRepoKey,
        head_branch: ValidatedBranchName,
        expected_head: Oid,
        remote: String,
        track: bool,
    ) -> Result<Self, ValidationError> {
        let consent = Self {
            head_repository,
            head_branch,
            expected_head,
            remote,
            track,
        };
        consent.validate()?;
        Ok(consent)
    }

    pub fn head_repository(&self) -> &GitHubRepoKey {
        &self.head_repository
    }
    pub fn head_branch(&self) -> &ValidatedBranchName {
        &self.head_branch
    }
    pub fn expected_head(&self) -> &Oid {
        &self.expected_head
    }
    pub fn remote(&self) -> &str {
        &self.remote
    }
    pub fn tracks_remote(&self) -> bool {
        self.track
    }

    fn remote_ref(&self) -> String {
        format!("refs/remotes/{}/{}", self.remote, self.head_branch.as_str())
    }

    fn validate(&self) -> Result<(), ValidationError> {
        Oid::parse_object(self.expected_head.as_str())?;
        ValidatedBranchName::parse(self.head_branch.as_str())?;
        BaseRevision::remote_ref(self.remote_ref())?;
        if self.remote.starts_with('-') || self.remote.starts_with('/') || self.remote.contains(':')
        {
            return Err(ValidationError::InvalidRemote(self.remote.clone()));
        }
        GitHubRepoKey::new(
            &self.head_repository.host,
            self.head_repository.port,
            &self.head_repository.owner,
            &self.head_repository.name,
        )
        .map_err(|error| ValidationError::InvalidBase(error.to_string()))?;
        Ok(())
    }

    pub(super) fn validate_request(
        &self,
        request: &CreateWorktreeRequest,
    ) -> Result<(), ValidationError> {
        self.validate()?;
        let matches = if self.track {
            matches!(&request.base, BaseRevision::RemoteRef(reference) if reference == &self.remote_ref())
        } else {
            matches!(&request.base, BaseRevision::ObjectId(oid) if oid == self.expected_head.as_str())
        };
        if request.track != self.track || !matches {
            return Err(ValidationError::InvalidBase(
                "The selected base or tracking differs from the reviewed PR checkout".into(),
            ));
        }
        Ok(())
    }
}

impl WorktreeJournal {
    pub(super) fn valid_pr_checkout_evidence(&self) -> bool {
        self.pr_checkout.as_ref().is_none_or(|consent| {
            consent.validate_request(&self.request).is_ok()
                && self.base_oid.as_deref().is_none_or(|oid| oid == consent.expected_head.as_str())
                && (self.state != WorktreeOperationState::Created
                    || (self.base_oid.as_deref() == Some(consent.expected_head.as_str())
                        && matches!(&self.outcome, Some(WorktreeCreationOutcome::Created {worktree, ..})
                            if worktree.head_oid.as_deref() == Some(consent.expected_head.as_str()))))
        })
    }
}

impl<E: ProcessExecutor> RepositoryService<E> {
    pub(super) fn reviewed_pr_creation_endpoint(
        &self,
        request: &CreateWorktreeRequest,
        consent: &PrWorktreeCreationConsent,
        cancellation: &CancellationToken,
    ) -> Result<String, RepositoryError> {
        consent.validate_request(request)?;
        self.checked_remote(&request.repository, &consent.remote, cancellation)?;
        let values = self
            .pr_values(
                &request.repository,
                &format!("remote.{}.url", consent.remote),
                cancellation,
            )
            .map_err(ValidationError::InvalidBase)?;
        if values.len() != 1
            || !remote_repository_key(&values[0])
                .is_some_and(|source| source.same_repository(&consent.head_repository))
        {
            return Err(ValidationError::InvalidBase(
                "The PR remote no longer names its reviewed head repository".into(),
            )
            .into());
        }
        Ok(values[0].clone())
    }

    #[cfg(feature = "ui")]
    pub(super) fn fetch_reviewed_pr_creation_head(
        &self,
        request: &CreateWorktreeRequest,
        consent: &PrWorktreeCreationConsent,
        lease: &fs::File,
        cancellation: &CancellationToken,
    ) -> Result<(), RepositoryError> {
        let endpoint = self.reviewed_pr_creation_endpoint(request, consent, cancellation)?;
        let count = match std::env::var_os("GIT_CONFIG_COUNT") {
            None => 0,
            Some(value) => value
                .to_str()
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(|| {
                    ValidationError::InvalidBase(
                        "Invalid Git environment configuration count".into(),
                    )
                })?,
        };
        if count >= i32::MAX as u32 {
            return Err(ValidationError::InvalidBase(
                "Git environment configuration count is too large".into(),
            )
            .into());
        }
        let alias = format!("cagents-pr-create-{}", Uuid::new_v4().simple());
        // Carry the captured raw endpoint in environment config. Git applies
        // ordinary URL rewrites once; a delayed remote.name.url change cannot
        // redirect this fetch. Credentials never enter argv or failure logs.
        let mut command = self
            .git_mutation(
                &request.repository,
                [
                    OsString::from("fetch"),
                    "--quiet".into(),
                    "--no-tags".into(),
                    "--no-recurse-submodules".into(),
                    "--refmap=".into(),
                    "--end-of-options".into(),
                    alias.clone().into(),
                    format!(
                        "{}:{}",
                        consent.head_branch.full_ref(),
                        consent.remote_ref()
                    )
                    .into(),
                ],
            )
            .env("GIT_CONFIG_COUNT", (count + 1).to_string())
            .env(
                format!("GIT_CONFIG_KEY_{count}"),
                format!("remote.{alias}.url"),
            )
            .env(format!("GIT_CONFIG_VALUE_{count}"), endpoint)
            .env("GIT_NO_REPLACE_OBJECTS", "1");
        command.inherited_lease = Some(lease.as_raw_fd());
        let output = self.executor.execute(&command, cancellation);
        match output {
            Ok(output) if output.success => {}
            Ok(output) => return Err(ProcessFailure {
                kind: ProcessFailureKind::Exit, program: self.git.clone(),
                exit_code: output.exit_code, signal: output.signal,
                stdout: Vec::new(), stderr: Vec::new(), started: true,
                message: "Fetching the reviewed PR source failed; inspect the remote and refresh the review".into(),
            }.into()),
            Err(mut failure) => {
                failure.stdout.clear(); failure.stderr.clear();
                failure.message = "Fetching the reviewed PR source did not complete; refresh and inspect the checkout".into();
                return Err(failure.into());
            }
        }
        let fetched = self.resolve_optional_revision(
            &request.repository,
            &consent.remote_ref(),
            cancellation,
        )?;
        if fetched.as_ref() != Some(&consent.expected_head) {
            return Err(ValidationError::InvalidBase(
                "The fetched PR branch differs from the reviewed commit; refresh the review. No worktree was created".into(),
            ).into());
        }
        Ok(())
    }

    pub(super) fn guard_created_pr_checkout_report(
        &self,
        journal: &WorktreeJournal,
        mut report: WorktreeOperationReport,
    ) -> WorktreeOperationReport {
        if report.state == WorktreeOperationState::Created && journal.pr_checkout.is_some() {
            if let Err(error) = self.validate_created_pr_checkout(journal) {
                // Applied remains a description of the original creation. A
                // changed current checkout cannot authorize a new UI opening.
                report.state = WorktreeOperationState::OutcomeUnknown;
                report.outcome = None;
                report.detail = format!(
                    "The recorded PR checkout no longer matches its reviewed source or commit: {error}; inspect it before opening"
                );
            }
        }
        report
    }

    pub(super) fn validate_created_pr_checkout(
        &self,
        journal: &WorktreeJournal,
    ) -> Result<(), RepositoryError> {
        let Some(consent) = &journal.pr_checkout else {
            return Ok(());
        };
        if !journal.valid_pr_checkout_evidence() {
            return Err(ValidationError::InvalidBase(
                "Recorded PR consent and checkout disagree".into(),
            )
            .into());
        }
        let cancellation = CancellationToken::new();
        self.reviewed_pr_creation_endpoint(&journal.request, consent, &cancellation)?;
        let residual = self.reconcile_worktree(&journal.request, &cancellation)?;
        let matching = residual.registered_worktree.as_ref().is_some_and(|record| {
            record.head_oid.as_deref() == Some(consent.expected_head.as_str())
                && record.branch.as_deref() == Some(journal.request.branch.full_ref().as_str())
                && !record.bare
                && !record.detached
        });
        if !matching
            || self.common_dir(&journal.request.repository, &cancellation)? != journal.common_dir
            || journal.admin_dir.as_ref()
                != Some(&self.git_dir(&journal.request.destination, &cancellation)?)
        {
            return Err(ValidationError::InvalidBase(
                "The created PR checkout changed; refresh and inspect it".into(),
            )
            .into());
        }
        Ok(())
    }
}
