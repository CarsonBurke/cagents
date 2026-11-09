//! Private pane allocation bindings survive requests, view removal and daemon
//! replacement. Local panes need no public shared view to retain their owner.
use super::*;
use crate::{
    chat::Session,
    model::{CardId, TerminalId},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::host) struct Binding {
    pub candidate: Session,
    pub current: TerminalId,
}

pub(super) fn migrate(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(
        "CREATE TABLE pane_chat_starts(
            pane TEXT PRIMARY KEY,
            candidate TEXT NOT NULL,
            current_allocation TEXT NOT NULL
        ) STRICT;
        CREATE TABLE pane_chat_allocations(
            allocation TEXT PRIMARY KEY,
            pane TEXT NOT NULL REFERENCES pane_chat_starts(pane) ON DELETE CASCADE
        ) STRICT;",
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Registry, CardId, Session) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.sqlite3");
        let card = CardId::new();
        let source = crate::chat::Session {
            id: crate::model::TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Omp,
                session_id: None,
                recipe: crate::model::LaunchRecipe {
                    program: "omp".into(),
                    arguments: vec![],
                    working_directory: directory.path().into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let mut registry = Registry::at(&path).unwrap();
        assert!(registry.bind_pane_chat_start(card, None, &source).unwrap());
        (directory, registry, card, source)
    }

    #[test]
    fn local_binding_survives_reopen_and_refuses_a_stale_writer() {
        let (directory, registry, card, source) = fixture();
        let path = directory.path().join("model.sqlite3");
        drop(registry);
        let mut registry = Registry::at(&path).unwrap();
        assert_eq!(
            registry.pane_chat_start(card).unwrap(),
            Some(Binding {
                candidate: source.clone(),
                current: source.id
            })
        );
        assert!(
            registry.model().unwrap().views.is_empty(),
            "local ownership is not a shared view"
        );
        let mut successor = source.clone();
        successor.id = crate::model::TerminalId::new();
        assert!(
            !registry
                .bind_pane_chat_start(card, None, &successor)
                .unwrap()
        );
        assert!(
            registry
                .bind_pane_chat_start(
                    card,
                    Some(&Binding {
                        candidate: source.clone(),
                        current: source.id
                    }),
                    &successor
                )
                .unwrap()
        );
        assert!(
            !registry
                .bind_pane_chat_start(
                    card,
                    Some(&Binding {
                        candidate: source.clone(),
                        current: source.id
                    }),
                    &source
                )
                .unwrap()
        );
        assert_eq!(
            registry.pane_chat_start(card).unwrap(),
            Some(Binding {
                current: successor.id,
                candidate: successor.clone()
            })
        );
        let other_pane = CardId::new();
        assert!(
            registry
                .bind_pane_chat_start(other_pane, None, &source)
                .is_err()
        );
        assert!(
            registry.pane_chat_start(other_pane).unwrap().is_none(),
            "allocation conflicts roll back the binding too"
        );

        let typed = TerminalId::new();
        let later = TerminalId::new();
        registry
            .succeed(typed, successor.id, None, |_| false)
            .unwrap();
        registry.succeed(later, typed, None, |_| false).unwrap();
        registry
            .succeed(typed, successor.id, None, |_| {
                unreachable!("already registered")
            })
            .unwrap();
        assert_eq!(
            registry.pane_chat_allocation(source.id).unwrap(),
            Some(later)
        );
        assert_eq!(registry.pane_chat_allocation(typed).unwrap(), Some(later));
        assert!(
            !registry
                .bind_pane_chat_start(
                    card,
                    Some(&Binding {
                        candidate: successor,
                        current: typed
                    }),
                    &source
                )
                .unwrap()
        );
        drop(registry);
        assert_eq!(
            Registry::at(&path)
                .unwrap()
                .pane_chat_allocation(source.id)
                .unwrap(),
            Some(later)
        );
    }
    #[test]
    fn a_first_send_winning_during_typed_preflight_refuses_the_unbound_successor() {
        let (directory, mut registry, card, source) = fixture();
        let previous = registry.pane_chat_start(card).unwrap().unwrap();
        let mut winner = source.clone();
        winner.id = TerminalId::new();
        let mut competitor = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        let rejected = TerminalId::new();
        let mut advanced = false;
        let result = registry.succeed(rejected, source.id, None, |_| {
            if !advanced {
                assert!(
                    competitor
                        .bind_pane_chat_start(card, Some(&previous), &winner)
                        .unwrap()
                );
                advanced = true;
            }
            false
        });
        assert!(advanced);
        assert!(
            result.is_err(),
            "a lost pane comparison must not authorize a new provider"
        );
        assert!(registry.session_lineage(rejected).unwrap().is_none());
        assert!(registry.pane_chat_allocation(rejected).unwrap().is_none());
        assert_eq!(
            registry.pane_chat_allocation(source.id).unwrap(),
            Some(winner.id)
        );
    }

    #[test]
    fn allocation_conflict_rolls_back_the_typed_successor_and_keeps_both_panes() {
        let (_directory, mut registry, card, source) = fixture();
        let other_card = CardId::new();
        let mut other = source.clone();
        other.id = TerminalId::new();
        assert!(
            registry
                .bind_pane_chat_start(other_card, None, &other)
                .unwrap()
        );
        assert!(
            registry
                .succeed(other.id, source.id, None, |_| false)
                .is_err()
        );
        assert!(registry.session_lineage(other.id).unwrap().is_none());
        assert_eq!(
            registry.pane_chat_start(card).unwrap().unwrap().current,
            source.id
        );
        assert_eq!(
            registry
                .pane_chat_start(other_card)
                .unwrap()
                .unwrap()
                .current,
            other.id
        );
    }
}

impl Registry {
    pub(in crate::host) fn pane_chat_start(&self, pane: CardId) -> Result<Option<Binding>, String> {
        let raw: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT candidate,current_allocation FROM pane_chat_starts WHERE pane=?1",
                [pane.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        raw.map(|(raw, current)| {
            Ok(Binding {
                candidate: serde_json::from_str(&raw).map_err(|error| error.to_string())?,
                current: current
                    .parse()
                    .map_err(|_| "Stored pane allocation is invalid".to_owned())?,
            })
        })
        .transpose()
    }

    pub(in crate::host) fn pane_chat_allocation(
        &self,
        allocation: TerminalId,
    ) -> Result<Option<TerminalId>, String> {
        allocation_in(&self.db, allocation)
    }

    pub(in crate::host) fn pane_chat_allocation_is_current(
        &mut self,
        allocation: TerminalId,
    ) -> Result<bool, String> {
        // Coordinate with the execution registry's separate SQLite connection,
        // not just this workspace writer's mutex. Capacity is already reserved.
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let current = allocation_in(&tx, allocation)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(current.is_none_or(|current| current == allocation))
    }

    /// The caller holds the workspace writer while checking the previous
    /// allocation's ownership. An independent writer cannot replace that basis.
    pub(in crate::host) fn bind_pane_chat_start(
        &mut self,
        pane: CardId,
        previous: Option<&Binding>,
        candidate: &Session,
    ) -> Result<bool, String> {
        let current = candidate.id.to_string();
        let candidate = serde_json::to_string(candidate).map_err(|error| error.to_string())?;
        let tx = self.db.transaction().map_err(|error| error.to_string())?;
        let changed = match previous {
            Some(previous) => tx.execute(
                "UPDATE pane_chat_starts SET candidate=?1,current_allocation=?4 WHERE pane=?2 AND candidate=?3 AND current_allocation=?5",
                params![
                    candidate,
                    pane.to_string(),
                    serde_json::to_string(&previous.candidate).map_err(|error| error.to_string())?,
                    current,
                    previous.current.to_string()
                ],
            ),
            None => tx.execute(
                "INSERT OR IGNORE INTO pane_chat_starts(pane,candidate,current_allocation) VALUES(?1,?2,?3)",
                params![pane.to_string(), candidate, current],
            ),
        }
        .map_err(|error| error.to_string())?;
        if changed == 1 {
            record_allocation(&tx, &current, &pane.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())?;
        Ok(changed == 1)
    }
}

pub(super) fn allocation_in(
    db: &Connection,
    allocation: TerminalId,
) -> Result<Option<TerminalId>, String> {
    let raw: Option<String> = db
        .query_row(
            "SELECT b.current_allocation FROM pane_chat_starts b
         JOIN pane_chat_allocations a ON a.pane=b.pane WHERE a.allocation=?1",
            [allocation.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    raw.map(|raw| {
        raw.parse()
            .map_err(|_| "Stored pane allocation is invalid".to_owned())
    })
    .transpose()
}

pub(super) fn retained_owner(allocation: TerminalId) -> Result<bool, String> {
    use crate::daemon::holder_registry::{self as registry, Kind};
    for kind in [Kind::Native, Kind::Terminal] {
        let directory = registry::directory(kind)?;
        if registry::recovery_candidate(&registry::socket(&directory, allocation), kind)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Called in the same transaction as typed session succession. Replaying an old
/// transition cannot overwrite a newer owner, even when its lineage is known.
pub(super) fn advance(
    tx: &rusqlite::Transaction<'_>,
    predecessor: TerminalId,
    successor: TerminalId,
) -> Result<usize, String> {
    let mut statement = tx
        .prepare("SELECT pane FROM pane_chat_starts WHERE current_allocation=?1")
        .map_err(|error| error.to_string())?;
    let panes = statement
        .query_map([predecessor.to_string()], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    for pane in panes {
        record_allocation(tx, &successor.to_string(), &pane)?;
    }
    tx.execute(
        "UPDATE pane_chat_starts SET current_allocation=?1 WHERE current_allocation=?2",
        params![successor.to_string(), predecessor.to_string()],
    )
    .map_err(|error| error.to_string())
}

fn record_allocation(
    tx: &rusqlite::Transaction<'_>,
    allocation: &str,
    pane: &str,
) -> Result<(), String> {
    tx.execute(
        "INSERT OR IGNORE INTO pane_chat_allocations(allocation,pane) VALUES(?1,?2)",
        params![allocation, pane],
    )
    .map_err(|error| error.to_string())?;
    let owner: String = tx
        .query_row(
            "SELECT pane FROM pane_chat_allocations WHERE allocation=?1",
            [allocation],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if owner != pane {
        return Err("This conversation allocation already belongs to another pane".into());
    }
    Ok(())
}
