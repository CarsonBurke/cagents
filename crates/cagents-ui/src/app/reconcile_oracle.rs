//! A check that an edge which skipped the complete reconciliation leaves the
//! window as that reconciliation would.
//!
//! [`Cagents::apply_changed_edge`] skips
//! [`Cagents::reconcile_workspace_model`], and the projection after it, for
//! an edge that changed nothing it reads. That claims the window is already
//! what the reconciliation makes it. In unit tests built with debug
//! assertions, each such skip captures what the reconciliation writes, runs
//! it, and fails on any difference, including any edit it would publish or
//! task it would start. The reconciliation's result stays, so a test goes on
//! from the state a correct skip leaves. The projection the skip declares is
//! the projection oracle's to check.

use super::*;
use crate::workspace_model::WorkspaceModel;

thread_local! {
    /// Shared model edits queued on this thread.
    static PUBLISHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static VERIFYING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the check's own reconciliation is running, which the count of
/// complete reconciliations leaves out, and which leaves out the naming stage
/// that the skipped edge runs itself.
pub(super) fn verifying() -> bool {
    VERIFYING.get()
}

/// Counts an edit queued for the registry.
pub(super) fn published() {
    PUBLISHED.set(PUBLISHED.get() + 1);
}

/// Everything a complete reconciliation writes, as comparable values.
#[derive(Debug, PartialEq)]
struct Snapshot {
    durable: WorkspaceState,
    workspace_model: WorkspaceModel,
    views: String,
    opens: Vec<PathBuf>,
    published: usize,
    /// Requests waiting in the publisher's queue, which other paths fill too.
    queued: usize,
    derived: super::projection_oracle::Snapshot,
}

impl Snapshot {
    fn capture(app: &Cagents) -> Self {
        let mut opens = app
            .workspace_model_opens
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        opens.sort_unstable();
        Self {
            durable: (*app.durable).clone(),
            workspace_model: app.workspace_model.clone(),
            views: app.shared_views.reconciled_view(),
            opens,
            published: PUBLISHED.get(),
            queued: app.workspace_model_tx.len(),
            derived: super::projection_oracle::Snapshot::capture(app),
        }
    }
}

/// Fails unless a complete reconciliation, apart from its naming stage,
/// changes nothing that the skipped edge, `kind`, leaves. Neither capture projects: whether the projection the
/// skip declares leaves the derived state current is the projection oracle's
/// check when the update ends, which projecting here would repair first.
pub(super) fn check_skipped(app: &mut Cagents, kind: &str) {
    if !super::projection_oracle::enabled() {
        return;
    }
    VERIFYING.set(true);
    let skipped = Snapshot::capture(app);
    // Work the reconciliation would start without changing state, such as
    // loading a repository, shows only as its task.
    let started = app.reconcile_workspace_model().units();
    VERIFYING.set(false);
    let reconciled = Snapshot::capture(app);
    if skipped != reconciled || started != 0 {
        let mut differences = Vec::new();
        if started != 0 {
            differences.push(format!("started {started} tasks"));
        }
        if skipped.durable != reconciled.durable {
            differences.push(format!(
                "durable: {:#?}\n  became {:#?}",
                skipped.durable, reconciled.durable
            ));
        }
        if skipped.workspace_model != reconciled.workspace_model {
            differences.push("workspace model".to_owned());
        }
        if skipped.views != reconciled.views {
            differences.push(format!(
                "views: {}\n  became {}",
                skipped.views, reconciled.views
            ));
        }
        if skipped.opens != reconciled.opens {
            differences.push(format!(
                "opens: {:?} became {:?}",
                skipped.opens, reconciled.opens
            ));
        }
        if (skipped.published, skipped.queued) != (reconciled.published, reconciled.queued) {
            differences.push(format!(
                "published {} more edits ({} more queued)",
                reconciled.published - skipped.published,
                reconciled.queued.saturating_sub(skipped.queued)
            ));
        }
        differences.extend(skipped.derived.differences(&reconciled.derived));
        panic!(
            "a {kind} edge skipped a reconciliation that changes:\n{}",
            differences.join("\n")
        );
    }
}
