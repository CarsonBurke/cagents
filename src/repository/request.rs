//! A source-control read or change as a value: what a window asks of the Git
//! that holds its worktree.
//!
//! A window on this computer answers one with its own [`RepositoryService`].
//! A device window sends it to the device, whose daemon answers it with the
//! same service, so a device's Changes panel reads and changes its worktree
//! exactly as a local one does.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepositoryRequest {
    /// [`RepositoryService::snapshot_if_changed`]. Without `files` the reply
    /// leaves out the working-file listing, which only a file browser draws
    /// and which is most of a large worktree's snapshot.
    Snapshot {
        worktree: PathBuf,
        options: SnapshotOptions,
        previous: Option<SnapshotDigest>,
        files: bool,
    },
    History(HistoryRequest),
    /// The paths one commit changed against its first parent.
    CommitFiles {
        worktree: PathBuf,
        oid: Oid,
        parent: Option<Oid>,
    },
    BranchComparison {
        worktree: PathBuf,
        base_ref: ValidatedRefName,
    },
    Diff(DiffRequest),
    Stage(StageRequest),
    /// A stage the person confirmed although its paths still hold conflict
    /// markers.
    StageWithConflictMarkers(StageRequest),
    Unstage(UnstageRequest),
    /// Stages or unstages some of one file's hunks.
    ApplyIndexPatch(IndexPatchRequest),
    Discard(DiscardRequest),
    Commit {
        request: CommitRequest,
        amend: bool,
    },
    Push(PushRequest),
    Abort {
        worktree: PathBuf,
        kind: OperationKind,
    },
    Continue {
        worktree: PathBuf,
        kind: OperationKind,
        message: Option<String>,
    },
    Skip {
        worktree: PathBuf,
        kind: OperationKind,
    },
}

/// The answer to a [`RepositoryRequest`], one variant for each kind of
/// result the service returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepositoryReply {
    Snapshot(SnapshotRead),
    History(Vec<CommitSummary>),
    Files(Vec<DiffEntry>),
    BranchComparison(BranchComparison),
    Diff(DiffContent),
    Mutation(MutationOutcome),
    Commit(CommitOutcome),
    Push(PushOutcome),
}

impl RepositoryRequest {
    /// The worktree the request reads or changes.
    pub fn worktree(&self) -> &Path {
        match self {
            Self::Snapshot { worktree, .. }
            | Self::CommitFiles { worktree, .. }
            | Self::BranchComparison { worktree, .. }
            | Self::Abort { worktree, .. }
            | Self::Continue { worktree, .. }
            | Self::Skip { worktree, .. } => worktree,
            Self::History(request) => &request.worktree,
            Self::Diff(request) => &request.worktree,
            Self::Stage(request) | Self::StageWithConflictMarkers(request) => &request.worktree,
            Self::Unstage(request) => &request.worktree,
            Self::ApplyIndexPatch(request) => &request.worktree,
            Self::Discard(request) => &request.worktree,
            Self::Commit { request, .. } => &request.worktree,
            Self::Push(request) => &request.worktree,
        }
    }

    /// Whether answering it may change the index, the worktree or a ref.
    pub fn mutates(&self) -> bool {
        !matches!(
            self,
            Self::Snapshot { .. }
                | Self::History(_)
                | Self::CommitFiles { .. }
                | Self::BranchComparison { .. }
                | Self::Diff(_)
        )
    }

    /// Answers the request with `service`, against the worktree it names.
    pub fn run<E: ProcessExecutor>(
        self,
        service: &RepositoryService<E>,
        cancellation: &CancellationToken,
    ) -> Result<RepositoryReply, RepositoryError> {
        Ok(match self {
            Self::Snapshot {
                worktree,
                options,
                previous,
                files,
            } => {
                let mut read = service.snapshot_if_changed(
                    &worktree,
                    options,
                    previous.as_ref(),
                    cancellation,
                )?;
                if !files && let SnapshotRead::Changed { snapshot, .. } = &mut read {
                    snapshot.files = WorkingFiles {
                        tracked_count: snapshot.files.tracked_count,
                        ..WorkingFiles::default()
                    };
                }
                RepositoryReply::Snapshot(read)
            }
            Self::History(request) => {
                RepositoryReply::History(service.history(&request, cancellation)?)
            }
            Self::CommitFiles {
                worktree,
                oid,
                parent,
            } => RepositoryReply::Files(service.commit_diff_entries(
                &worktree,
                &oid,
                parent.as_ref(),
                cancellation,
            )?),
            Self::BranchComparison { worktree, base_ref } => RepositoryReply::BranchComparison(
                service.branch_comparison(&worktree, &base_ref, cancellation)?,
            ),
            Self::Diff(request) => RepositoryReply::Diff(service.diff(&request, cancellation)?),
            Self::Stage(request) => {
                RepositoryReply::Mutation(service.stage(&request, cancellation)?)
            }
            Self::StageWithConflictMarkers(request) => RepositoryReply::Mutation(
                service.stage_with_conflict_markers(&request, cancellation)?,
            ),
            Self::Unstage(request) => {
                RepositoryReply::Mutation(service.unstage(&request, cancellation)?)
            }
            Self::ApplyIndexPatch(request) => {
                RepositoryReply::Mutation(service.apply_index_patch(&request, cancellation)?)
            }
            Self::Discard(request) => {
                RepositoryReply::Mutation(service.discard(&request, cancellation)?)
            }
            Self::Commit { request, amend } => {
                RepositoryReply::Commit(service.commit_with_amend(&request, amend, cancellation)?)
            }
            Self::Push(request) => RepositoryReply::Push(service.push(&request, cancellation)?),
            Self::Abort { worktree, kind } => {
                RepositoryReply::Mutation(service.abort_operation(&worktree, kind, cancellation)?)
            }
            Self::Continue {
                worktree,
                kind,
                message,
            } => RepositoryReply::Mutation(service.continue_operation(
                &worktree,
                kind,
                message.as_deref(),
                cancellation,
            )?),
            Self::Skip { worktree, kind } => {
                RepositoryReply::Mutation(service.skip_operation(&worktree, kind, cancellation)?)
            }
        })
    }
}

/// What a reply was expected to be and was not.
fn unexpected(expected: &str) -> String {
    format!("the repository answered with something other than {expected}")
}

impl RepositoryReply {
    pub fn into_snapshot(self) -> Result<SnapshotRead, String> {
        match self {
            Self::Snapshot(read) => Ok(read),
            _ => Err(unexpected("a snapshot")),
        }
    }

    pub fn into_history(self) -> Result<Vec<CommitSummary>, String> {
        match self {
            Self::History(commits) => Ok(commits),
            _ => Err(unexpected("history")),
        }
    }

    pub fn into_files(self) -> Result<Vec<DiffEntry>, String> {
        match self {
            Self::Files(entries) => Ok(entries),
            _ => Err(unexpected("a commit's files")),
        }
    }

    pub fn into_branch_comparison(self) -> Result<BranchComparison, String> {
        match self {
            Self::BranchComparison(comparison) => Ok(comparison),
            _ => Err(unexpected("a branch comparison")),
        }
    }

    pub fn into_diff(self) -> Result<DiffContent, String> {
        match self {
            Self::Diff(content) => Ok(content),
            _ => Err(unexpected("a diff")),
        }
    }

    pub fn into_mutation(self) -> Result<MutationOutcome, String> {
        match self {
            Self::Mutation(outcome) => Ok(outcome),
            _ => Err(unexpected("a change's outcome")),
        }
    }

    pub fn into_commit(self) -> Result<CommitOutcome, String> {
        match self {
            Self::Commit(outcome) => Ok(outcome),
            _ => Err(unexpected("a commit's outcome")),
        }
    }

    pub fn into_push(self) -> Result<PushOutcome, String> {
        match self {
            Self::Push(outcome) => Ok(outcome),
            _ => Err(unexpected("a push's outcome")),
        }
    }

    /// The reply with each diff side over `max_side` bytes reported as
    /// oversize instead of carried, so it fits in one message between
    /// computers.
    pub fn bounded(self, max_side: u64) -> Self {
        let bound = |side: DiffSide| match side {
            DiffSide::Text { bytes, .. } if bytes > max_side => DiffSide::Oversize { bytes },
            side => side,
        };
        match self {
            Self::Diff(DiffContent::Comparison {
                old,
                new,
                old_label,
                new_label,
            }) => Self::Diff(DiffContent::Comparison {
                old: bound(old),
                new: bound(new),
                old_label,
                new_label,
            }),
            Self::Diff(DiffContent::Unmerged {
                base,
                ours,
                theirs,
                worktree,
            }) => Self::Diff(DiffContent::Unmerged {
                base: bound(base),
                ours: bound(ours),
                theirs: bound(theirs),
                worktree: bound(worktree),
            }),
            reply => reply,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(bytes: usize) -> DiffSide {
        DiffSide::Text {
            text: "x".repeat(bytes),
            bytes: bytes as u64,
        }
    }

    #[test]
    fn a_bounded_diff_reports_a_side_too_large_to_carry_as_oversize() {
        let reply = RepositoryReply::Diff(DiffContent::Comparison {
            old: text(4),
            new: text(9),
            old_label: "Index".into(),
            new_label: "Working tree".into(),
        })
        .bounded(8);
        assert_eq!(
            reply,
            RepositoryReply::Diff(DiffContent::Comparison {
                old: text(4),
                new: DiffSide::Oversize { bytes: 9 },
                old_label: "Index".into(),
                new_label: "Working tree".into(),
            })
        );
    }

    /// Every reply a device sends is read back as it was written.
    #[test]
    fn requests_and_replies_round_trip_as_json() {
        let process = MutationFailure::Process(ProcessFailure {
            kind: crate::process::ProcessFailureKind::Exit,
            program: "git".into(),
            exit_code: Some(1),
            signal: None,
            stdout: Vec::new(),
            stderr: b"hook refused".to_vec(),
            message: "exited with 1".into(),
            started: true,
        });
        let path = RepoPath::parse("src/main.rs").unwrap();
        let oid = Oid::parse("0123456789abcdef0123456789abcdef01234567").unwrap();
        let branch = ValidatedBranchName::parse("main").unwrap();
        let replies = [
            RepositoryReply::Mutation(MutationOutcome::PartiallyApplied {
                paths: vec![path.clone()],
                failures: vec![
                    process.clone(),
                    MutationFailure::Path {
                        path: path.clone(),
                        reason: "busy".into(),
                    },
                    MutationFailure::ConflictMarkers {
                        paths: vec![path.clone()],
                    },
                ],
            }),
            RepositoryReply::Mutation(MutationOutcome::OutcomeUnknown {
                failure: MutationFailure::Reconciliation {
                    reason: "unclear".into(),
                },
            }),
            RepositoryReply::Commit(CommitOutcome::Created {
                oid: oid.clone(),
                warnings: vec![process.clone()],
            }),
            RepositoryReply::Commit(CommitOutcome::NotCreated {
                failure: process.clone(),
            }),
            RepositoryReply::Push(PushOutcome::Pushed {
                remote: "origin".into(),
                remote_ref: RemoteSideRef::branch(&branch),
                oid: oid.clone(),
            }),
            RepositoryReply::Push(PushOutcome::NotPushed { failure: process }),
            RepositoryReply::Diff(DiffContent::Unmerged {
                base: DiffSide::Absent,
                ours: text(3),
                theirs: DiffSide::Binary { bytes: 9 },
                worktree: DiffSide::Oversize { bytes: 1 << 30 },
            }),
        ];
        for reply in replies {
            let json = serde_json::to_string(&reply).unwrap();
            assert_eq!(
                serde_json::from_str::<RepositoryReply>(&json).unwrap(),
                reply,
                "{json}"
            );
        }
        let requests = [
            RepositoryRequest::Push(PushRequest {
                worktree: "/srv/demo".into(),
                branch: branch.clone(),
                target: PushTarget::SetUpstream {
                    remote: "origin".into(),
                    remote_branch: branch.clone(),
                },
            }),
            RepositoryRequest::Push(PushRequest {
                worktree: "/srv/demo".into(),
                branch,
                target: PushTarget::Upstream {
                    remote: "origin".into(),
                    remote_ref: ValidatedRefName::parse("refs/remotes/origin/main").unwrap(),
                },
            }),
            RepositoryRequest::Diff(DiffRequest {
                worktree: "/srv/demo".into(),
                basis: DiffBasis::CommitToParent { oid, parent: None },
                path: path.clone(),
                old_path: Some(path),
            }),
            RepositoryRequest::Continue {
                worktree: "/srv/demo".into(),
                kind: OperationKind::Rebase,
                message: Some("resolved".into()),
            },
        ];
        for request in requests {
            let json = serde_json::to_string(&request).unwrap();
            assert_eq!(
                serde_json::from_str::<RepositoryRequest>(&json).unwrap(),
                request,
                "{json}"
            );
        }
    }
}
