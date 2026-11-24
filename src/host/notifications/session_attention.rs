use super::{CommitReceipt, Journal, MAX_PENDING, Notice, Snapshot, Target};
use crate::{
    agent_execution::Lineage,
    host::{agent_execution, model::Registry, state::SharedHost},
    model::TerminalId,
    notifications::Kind,
};
use std::collections::{BTreeSet, VecDeque};
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(super) enum Clear {
    View(Option<i64>),
    Turn,
    Waiting,
}

impl Clear {
    pub(super) fn matches(self, notice: &Notice, ordinal: i64) -> bool {
        let kind = notice.event.evidence.kind;
        match self {
            Self::View(cutoff) => cutoff.is_none_or(|cutoff| ordinal <= cutoff),
            Self::Turn => matches!(
                kind,
                Kind::Succeeded
                    | Kind::Failed
                    | Kind::Cancelled
                    | Kind::Waiting
                    | Kind::Interrupted
            ),
            Self::Waiting => kind == Kind::Waiting,
        }
    }
}

pub(super) enum Change {
    Clear(TerminalId, Clear, Option<(Uuid, u64)>),
    Retire(super::session_census::Census),
}

pub(super) type Entry = (u64, Change, Option<CommitReceipt>);

#[derive(Default)]
pub(super) struct Pending(VecDeque<Entry>);

impl Pending {
    pub(super) fn push(&mut self, entry: Entry) -> Result<(), String> {
        if self.0.len() >= MAX_PENDING {
            return Err("Session attention queue is full".into());
        }
        self.0.push_back(entry);
        Ok(())
    }

    pub(super) fn extend(&mut self, newer: Self) {
        let available = MAX_PENDING.saturating_sub(self.0.len());
        self.0.extend(newer.0.into_iter().take(available));
    }

    pub(super) fn deferred(mut self) -> Self {
        self.0.retain(|(_, _, receipt)| receipt.is_none());
        self
    }

    pub(super) fn entries(self) -> VecDeque<Entry> {
        self.0
    }
}

fn lineage(registry: &Registry, session: TerminalId) -> Result<Lineage, String> {
    Ok(registry
        .session_lineage(session)?
        .unwrap_or(Lineage(session)))
}

fn for_target(registry: &Registry, target: &Target) -> Result<Option<Lineage>, String> {
    match *target {
        Target::Session(session) => Ok(Some(lineage(registry, session)?)),
        Target::SessionInbox(lineage) => Ok(Some(lineage)),
        _ => Ok(None),
    }
}

pub(super) fn clear(
    journal: &mut Journal,
    host: &SharedHost,
    session: TerminalId,
    clear: Clear,
    cursor: Option<(Uuid, u64)>,
) -> Result<Vec<Notice>, String> {
    let (lineage, sessions) = match agent_execution::registry(host) {
        Ok(registry) => {
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            let lineage = lineage(&registry, session)?;
            (lineage, registry.lineage_succession(lineage)?)
        }
        Err(_) => (Lineage(session), vec![session]),
    };
    let inbox = matches!(clear, Clear::View(_)).then_some(Target::SessionInbox(lineage));
    let targets = sessions.into_iter().map(Target::Session).chain(inbox);
    journal.clear_session_notices(targets, clear, cursor)
}

pub(super) fn acknowledge(
    journal: &mut Journal,
    host: Option<&SharedHost>,
    id: Uuid,
    visible: Option<&Notice>,
) -> Result<Vec<Notice>, String> {
    let Some(visible) = visible else {
        journal.acknowledge(id)?;
        return Ok(Vec::new());
    };
    if let Some(host) = host
        && let Some(session) = match visible.event.target {
            Target::Session(session) => Some(session),
            Target::SessionInbox(lineage) => Some(lineage.0),
            _ => None,
        }
        && let Some((notice, ordinal)) = journal.notice_for_target(&visible.event.target)?
        && notice.id == id
    {
        return clear(journal, host, session, Clear::View(Some(ordinal)), None);
    }
    journal.acknowledge(id)?;
    Ok(vec![visible.clone()])
}

/// Use the newest published member for both opening and reading a notice.
/// The journal still retains the original target to deduplicate evidence and
/// withdraw its desktop notification by the ID it was published under.
pub(super) fn project(host: &SharedHost, snapshot: &mut Snapshot) -> Result<(), String> {
    if !snapshot.notices.iter().any(|notice| {
        matches!(
            notice.event.target,
            Target::Session(_) | Target::SessionInbox(_)
        )
    }) {
        return Ok(());
    }
    let current = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .notification_sessions();
    let registry = agent_execution::registry(host)?;
    let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    for notice in &mut snapshot.notices {
        project_notice(&registry, &current, notice)?;
    }
    Ok(())
}

fn project_notice(
    registry: &Registry,
    current: &BTreeSet<TerminalId>,
    notice: &mut Notice,
) -> Result<(), String> {
    let Some(lineage) = for_target(registry, &notice.event.target)? else {
        return Ok(());
    };
    let Some(session) = registry
        .lineage_succession(lineage)?
        .into_iter()
        .find(|session| current.contains(session))
    else {
        return Ok(());
    };
    notice.event.source_session = Some(session);
    Ok(())
}

#[cfg(test)]
impl super::Handle {
    pub(in crate::host) fn session_read_pending_for_test(&self, session: TerminalId) -> bool {
        self.0
            .pending
            .lock()
            .unwrap()
            .session_attention
            .0
            .iter()
            .any(|(_, change, _)| matches!(change, Change::Clear(id, Clear::View(None), _) if *id == session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        host::state::{HostLock, HostState},
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
        notifications::{Event, Evidence, Presence},
    };
    use std::sync::Arc;

    fn fixture() -> (
        tempfile::TempDir,
        SharedHost,
        Journal,
        TerminalId,
        TerminalId,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("workspace.sqlite3")).unwrap();
        let original = TerminalId::new();
        let successor = TerminalId::new();
        registry
            .succeed(successor, original, None, |_| false)
            .unwrap();
        let mut host = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("attention-test"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("daemon"),
        });
        host.install_model(registry);
        host.watch_notifications(
            1,
            Presence {
                window: "main".into(),
                sessions: vec![successor],
                focused: Some(successor),
                ..Default::default()
            },
        );
        let journal = Journal::open(&directory.path().join("notices.sqlite3")).unwrap();
        (
            directory,
            Arc::new(HostLock::new(host)),
            journal,
            original,
            successor,
        )
    }

    fn event(id: TerminalId, key: &str, kind: Kind) -> Event {
        Event::session(
            id,
            Provider::Claude,
            "/fixture".into(),
            Evidence {
                key: key.into(),
                kind,
            },
        )
    }

    #[test]
    fn viewing_successor_clears_the_original_notice_without_forgetting_evidence() {
        let (_directory, host, mut journal, original, successor) = fixture();
        let event = event(original, "original", Kind::Succeeded);
        let old = journal.record(event.clone(), false).unwrap().unwrap();
        let mut snapshot = Snapshot {
            notices: journal.list().unwrap(),
            diagnostic: None,
        };
        project(&host, &mut snapshot).unwrap();
        assert_eq!(snapshot.notices[0].event.target, Target::Session(original));
        assert_eq!(snapshot.notices[0].event.source_session, Some(successor));
        let cleared = clear(&mut journal, &host, successor, Clear::View(None), None).unwrap();
        assert_eq!(
            cleared.iter().map(|notice| notice.id).collect::<Vec<_>>(),
            vec![old.id]
        );
        assert!(journal.list().unwrap()[0].acknowledged);
        assert!(journal.record(event, false).unwrap().is_none());
    }

    #[test]
    fn acknowledging_a_visible_row_does_not_consume_a_later_same_lineage_outcome() {
        let (_directory, host, mut journal, original, successor) = fixture();
        let before = journal
            .record(event(original, "old", Kind::Failed), false)
            .unwrap()
            .unwrap();
        let after = journal
            .record(event(successor, "new", Kind::Succeeded), false)
            .unwrap()
            .unwrap();
        acknowledge(&mut journal, Some(&host), before.id, Some(&before)).unwrap();
        let notices = journal.list().unwrap();
        assert_eq!((notices[0].id, notices[0].acknowledged), (after.id, false));
        assert_eq!((notices[1].id, notices[1].acknowledged), (before.id, true));
        acknowledge(&mut journal, Some(&host), after.id, Some(&after)).unwrap();
        assert!(
            journal
                .list()
                .unwrap()
                .iter()
                .all(|notice| notice.acknowledged)
        );
    }

    #[test]
    fn retained_turn_clear_replay_cannot_acknowledge_the_new_outcome() {
        let (directory, host, mut journal, original, successor) = fixture();
        let actor = Uuid::new_v4();
        let outcome = journal
            .record(event(original, "previous", Kind::Failed), false)
            .unwrap()
            .unwrap();
        clear(
            &mut journal,
            &host,
            successor,
            Clear::Waiting,
            Some((actor, 4)),
        )
        .unwrap();
        assert!(!journal.list().unwrap()[0].acknowledged);
        clear(
            &mut journal,
            &host,
            successor,
            Clear::Turn,
            Some((actor, 5)),
        )
        .unwrap();
        assert_eq!(
            (
                journal.list().unwrap()[0].id,
                journal.list().unwrap()[0].acknowledged
            ),
            (outcome.id, true)
        );
        let next = journal
            .record(event(successor, "finished", Kind::Succeeded), false)
            .unwrap()
            .unwrap();
        drop(journal);
        let mut journal = Journal::open(&directory.path().join("notices.sqlite3")).unwrap();
        assert!(
            clear(
                &mut journal,
                &host,
                successor,
                Clear::Turn,
                Some((actor, 5))
            )
            .unwrap()
            .is_empty()
        );
        let current = &journal.list().unwrap()[0];
        assert_eq!((current.id, current.acknowledged), (next.id, false));
    }

    #[test]
    fn an_orphan_witness_cannot_delete_a_newer_notice_at_the_same_target() {
        let (_directory, _host, mut journal, original, _) = fixture();
        let old = journal
            .record(event(original, "orphan", Kind::Failed), false)
            .unwrap()
            .unwrap();
        let new = journal
            .record(event(original, "recent", Kind::Succeeded), false)
            .unwrap()
            .unwrap();
        assert!(
            journal
                .retire_session_notices(vec![(old.id, old.event.target)])
                .unwrap()
                .is_empty()
        );
        assert_eq!(journal.list().unwrap(), vec![new.clone()]);
        assert_eq!(
            journal
                .retire_session_notices(vec![(new.id, new.event.target.clone())])
                .unwrap(),
            vec![new]
        );
        assert!(journal.list().unwrap().is_empty());
    }
}
