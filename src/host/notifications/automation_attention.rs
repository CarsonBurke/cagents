//! Reconcile automation attention against committed lifecycle changes.
use std::{
    collections::BTreeSet,
    sync::Weak,
    time::{Duration, Instant},
};

use crate::notifications::{Notice, Snapshot, Target};

use super::{HostLock, Journal, MAX_PENDING, Withdrawals, withdraw_notice};
use uuid::Uuid;

#[derive(Default)]
pub(super) struct Pending {
    scan_all: bool,
    changed: BTreeSet<Uuid>,
}

impl Pending {
    pub(super) fn merge(&mut self, newer: Self) {
        if newer.scan_all {
            self.scan_all();
        } else {
            for id in newer.changed {
                self.lifecycle_changed(id);
            }
        }
    }

    pub(super) fn scan_all(&mut self) {
        self.scan_all = true;
        self.changed.clear();
    }

    pub(super) fn lifecycle_changed(&mut self, id: Uuid) {
        if !self.scan_all {
            self.changed.insert(id);
            if self.changed.len() > MAX_PENDING {
                self.scan_all();
            }
        }
    }

    pub(super) fn retry_at(&self) -> Option<Instant> {
        (self.scan_all || !self.changed.is_empty()).then(|| Instant::now() + Duration::from_secs(5))
    }

    /// Runs before publication and desktop delivery. It also checks events
    /// enqueued before a management change but recorded after it.
    pub(super) fn apply(
        &mut self,
        host: &Weak<HostLock>,
        journal: &mut Journal,
        snapshot: &mut Snapshot,
        withdrawals: &mut Withdrawals,
    ) -> Option<Instant> {
        if self.scan_all || !self.changed.is_empty() {
            match reconcile_lifecycle(self, host, journal, &mut snapshot.notices, withdrawals) {
                Ok(()) => {
                    self.scan_all = false;
                    self.changed.clear();
                }
                Err(error) => {
                    tracing::warn!(%error, "could not reconcile automation notices");
                }
            }
        }
        self.retry_at()
    }
}

pub(super) fn is_resolved(host: &Weak<HostLock>, id: Uuid, sequence: u64) -> Result<bool, String> {
    let Some(host) = host.upgrade() else {
        return Err("The daemon stopped before checking the automation notice".into());
    };
    let registry = crate::host::agent_execution::registry(&host)?;
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .automation_notice_resolved(id, sequence)
}

fn reconcile_lifecycle(
    pending: &Pending,
    host: &Weak<HostLock>,
    journal: &mut Journal,
    notices: &mut Vec<Notice>,
    withdrawals: &mut Withdrawals,
) -> Result<(), String> {
    let candidates = notices
        .iter()
        .filter_map(|notice| match notice.event.target {
            Target::Automation { id, sequence }
                if pending.scan_all || pending.changed.contains(&id) =>
            {
                Some((notice, id, sequence))
            }
            _ => None,
        });
    let mut candidates = candidates.peekable();
    if candidates.peek().is_none() {
        return Ok(());
    }
    let Some(host) = host.upgrade() else {
        return Ok(());
    };
    let registry = crate::host::agent_execution::registry(&host)?;
    let resolved = {
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let mut resolved = Vec::new();
        for (notice, id, sequence) in candidates {
            if registry.automation_notice_resolved(id, sequence)? {
                resolved.push(notice);
            }
        }
        resolved
    };
    let mut retired = BTreeSet::new();
    let result = resolved.into_iter().try_for_each(|notice| {
        journal.retire(notice.event.target())?;
        withdraw_notice(withdrawals, notice);
        retired.insert(notice.id);
        Ok(())
    });
    notices.retain(|notice| !retired.contains(&notice.id));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Authority, Spec, Surface, Target as AgentTarget, Update},
        automations::{
            Automation, Definition, DirectoryIdentity, Occurrence, Precheck, Request, Response,
            Schedule, SchedulePattern, SessionMode, Trigger,
        },
        host::{
            agent_execution::{self, SharedRegistry},
            automations::service,
            model::Registry,
            state::{HostState, SharedHost},
        },
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, ProjectKind, RunId},
        notifications::{Event, Evidence, Kind},
        workspace_model::ModelEdit,
    };
    use std::sync::Arc;

    const DIRECTORY: DirectoryIdentity = DirectoryIdentity {
        device: 1,
        inode: 1,
    };

    fn fixture(blocked: bool) -> (tempfile::TempDir, SharedHost, SharedRegistry, Automation) {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().to_owned();
        let mut registry = Registry::at(&repository.join("model.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: repository.clone(),
                name: "Automation project".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let automation = registry
            .create_automation(
                Definition {
                    name: "Fixture".into(),
                    prompt: "Check".into(),
                    agent: Spec {
                        provider: Provider::Codex,
                        account: None,
                        target: AgentTarget {
                            repository: repository.clone(),
                            directory: repository,
                            placement: None,
                        },
                        surface: Surface::Chat,
                        model: None,
                        permission_mode: None,
                        reasoning_effort: None,
                    },
                    schedule: Schedule {
                        pattern: SchedulePattern::Daily { hour: 9, minute: 0 },
                        timezone: "UTC".into(),
                    },
                    session_mode: SessionMode::Fresh,
                    precheck: blocked.then(|| Precheck {
                        command: "true".into(),
                        timeout_seconds: 1,
                    }),
                },
                1,
                DIRECTORY,
            )
            .unwrap();
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("automation-attention-fixture"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        });
        state.install_model(registry);
        let host = Arc::new(HostLock::new(state));
        let registry = agent_execution::registry(&host).unwrap();
        (directory, host, registry, automation)
    }

    fn next() -> Option<Occurrence> {
        Some(Occurrence {
            due_ms: 60_000,
            local: "next".into(),
        })
    }

    fn claim(registry: &mut Registry, automation: &Automation) -> crate::host::model::Claimed {
        registry
            .claim_automation(
                automation.id,
                automation.execution_revision,
                None,
                Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                2,
            )
            .unwrap()
    }

    fn apply(pending: &mut Pending, host: &SharedHost, journal: &mut Journal) -> Snapshot {
        let mut snapshot = Snapshot {
            notices: journal.list().unwrap(),
            ..Default::default()
        };
        assert!(
            pending
                .apply(
                    &Arc::downgrade(host),
                    journal,
                    &mut snapshot,
                    &mut Withdrawals::default(),
                )
                .is_none()
        );
        snapshot
    }

    #[test]
    fn management_retires_pause_or_block_in_source_and_journal() {
        #[derive(Clone, Copy)]
        enum Change {
            Enable,
            Edit,
            Remove,
        }
        for change in [Change::Enable, Change::Edit, Change::Remove] {
            let blocked = matches!(change, Change::Edit);
            let (directory, host, registry, automation) = fixture(blocked);
            let repository = directory.path().to_owned();
            let mut journal = Journal::open(&repository.join("attention.sqlite3")).unwrap();
            let (current, event) = {
                let mut registry = registry.lock().unwrap();
                if blocked {
                    let claim = claim(&mut registry, &automation);
                    registry
                        .interrupt_automation_precheck(
                            claim.firing.id,
                            "Cleanup uncertain".into(),
                            3,
                        )
                        .unwrap();
                } else {
                    registry
                        .set_automation_enabled(automation.id, automation.revision, next(), true, 2)
                        .unwrap();
                    registry
                        .apply(&ModelEdit::CloseProject {
                            repository: repository.clone(),
                        })
                        .unwrap();
                    registry
                        .apply(&ModelEdit::ReopenProject {
                            repository: repository.clone(),
                            name: "Automation project".into(),
                            kind: ProjectKind::Folder,
                        })
                        .unwrap();
                }
                let current = registry.automation(automation.id).unwrap();
                let entry = registry
                    .pending_automation_notifications(1)
                    .unwrap()
                    .pop()
                    .unwrap();
                let event = Event {
                    target: Target::Automation {
                        id: current.id,
                        sequence: entry.event().sequence,
                    },
                    source_session: None,
                    provider: None,
                    directory: None,
                    destination: None,
                    evidence: Evidence {
                        key: entry.evidence_key(),
                        kind: if blocked {
                            Kind::AutomationBlocked
                        } else {
                            Kind::AutomationPaused
                        },
                    },
                    content: None,
                };
                (current, event)
            };
            let Target::Automation { id, sequence } = event.target else {
                unreachable!()
            };
            let notice = journal.record(event.clone(), false).unwrap().unwrap();
            let mut unrelated = event.clone();
            let run = RunId::new();
            unrelated.target = Target::AgentRun(run);
            unrelated.evidence = Evidence {
                key: run.to_string(),
                kind: Kind::Failed,
            };
            let unrelated = journal.record(unrelated, false).unwrap().unwrap();
            let mut pending = Pending::default();
            pending.scan_all();
            assert!(
                apply(&mut pending, &host, &mut journal)
                    .notices
                    .contains(&notice)
            );

            {
                let mut registry = registry.lock().unwrap();
                match change {
                    Change::Enable => {
                        registry
                            .set_automation_enabled(id, current.revision, next(), true, 4)
                            .unwrap();
                    }
                    Change::Edit => {
                        let mut renamed = current.definition;
                        renamed.name.push_str(" renamed");
                        registry
                            .edit_automation(id, current.revision, renamed, 4, DIRECTORY)
                            .unwrap();
                    }
                    Change::Remove => registry.remove_automation(id, current.revision, 4).unwrap(),
                }
                assert!(
                    registry
                        .pending_automation_notifications(1)
                        .unwrap()
                        .is_empty()
                );
            }
            // A late publication from an already selected outbox entry is
            // obsolete even if it races the live reconciliation signal.
            assert!(is_resolved(&Arc::downgrade(&host), id, sequence).unwrap());
            pending.lifecycle_changed(id);
            assert_eq!(
                apply(&mut pending, &host, &mut journal).notices,
                vec![unrelated.clone()]
            );
            assert_eq!(journal.list().unwrap(), vec![unrelated]);

            if matches!(change, Change::Enable) {
                let mut registry = registry.lock().unwrap();
                registry
                    .apply(&ModelEdit::CloseProject { repository })
                    .unwrap();
                let newer = registry
                    .pending_automation_notifications(1)
                    .unwrap()
                    .pop()
                    .unwrap();
                assert!(newer.event().sequence > sequence);
                assert!(registry.automation_notice_resolved(id, sequence).unwrap());
                assert!(
                    !registry
                        .automation_notice_resolved(id, newer.event().sequence)
                        .unwrap()
                );
            }
        }
    }

    #[test]
    fn reading_run_detail_leaves_attention_pending() {
        let (_directory, host, registry, automation) = fixture(false);
        let (firing, waiting) = {
            let mut registry = registry.lock().unwrap();
            let claim = claim(&mut registry, &automation);
            let run = claim.accepted.unwrap().run;
            let started = registry
                .update_agent_run(
                    run.id,
                    run.revision,
                    Update::Started {
                        provider_key: "fixture".into(),
                    },
                    3,
                )
                .unwrap();
            let waiting = registry
                .update_agent_run(run.id, started.revision, Update::Waiting, 4)
                .unwrap();
            (claim.firing.id, waiting)
        };
        for request in [
            Request::ShowFiring { id: firing },
            Request::ShowFiring { id: firing },
            Request::ShowRun { run: waiting.id },
        ] {
            let response =
                service::process_under_control(&host, Authority::default(), request).unwrap();
            assert!(
                matches!(response, Response::Detail(detail) if detail.run == Some(waiting.clone()))
            );
            let pending = registry
                .lock()
                .unwrap()
                .pending_automation_attention(1)
                .unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].receipt.run, waiting.id);
            assert_eq!(pending[0].receipt.revision, waiting.revision);
        }
    }
}
