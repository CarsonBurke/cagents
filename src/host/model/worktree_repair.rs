//! The startup repair that retires worktree strips.
//!
//! Before channels carried a checkout, a worktree's panes lived in a strip of
//! their own, outside every channel. Windows now open a worktree in its
//! checkout's channel, so the daemon moves each strip that still holds panes,
//! or that an open agent is placed in, into that channel once, when it opens
//! the store. Every pane keeps its id, order, width and filing, and every agent
//! its session: the move is the same transfer a user's drag commits.
//!
//! A strip whose worktree is gone from disk is never dropped. Its channel
//! records the missing worktree as its checkout, so its panes start in the
//! project's main checkout until the worktree is recreated.
use super::{Registry, checkout_key, repository_key, take_ordinal, views};
use crate::model::{ChannelId, ChannelTitleSource};
use crate::workspace_model::views::{Change, Scope};
use crate::workspace_model::{ModelEdit, NavigationItem, canonical_checkout, checkout_branch};
use rusqlite::{OptionalExtension, Transaction, params};
use std::path::{Path, PathBuf};

/// The channel a strip's panes move to.
#[derive(Clone, Debug, PartialEq)]
struct Home {
    /// `None` for the project's main checkout. A worktree that no longer
    /// exists keeps its recorded path.
    checkout: Option<PathBuf>,
    /// The branch, else the checkout's directory name.
    title: String,
}

impl Home {
    fn of(repository: &Path, path: &Path) -> Self {
        let checkout = match canonical_checkout(repository, path) {
            Ok(checkout) => checkout,
            Err(_) if main_location(repository, path) => None,
            Err(_) => Some(path.to_path_buf()),
        };
        let title = checkout_branch(repository, path)
            .or_else(|| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| path.display().to_string());
        Self { checkout, title }
    }
}

/// Whether `path` is where the project's main checkout lives, even when it is
/// missing: the folder itself, or the directory holding a `.git` repository.
fn main_location(repository: &Path, path: &Path) -> bool {
    path == repository
        || (repository.file_name().is_some_and(|name| name == ".git")
            && repository.parent() == Some(path))
}

/// What the repair changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Adopted {
    pub(crate) strips: usize,
    pub(crate) panes: usize,
    pub(crate) created_channels: usize,
    /// Panes the transfer refused, such as an agent pane whose process changed
    /// since it was saved. They stay in their strip, which still opens.
    pub(crate) kept: usize,
}

impl Registry {
    /// Moves every worktree strip that holds panes, or that an open agent is
    /// placed in, into its checkout's channel, in one transaction. Running it
    /// again changes nothing, because a repaired strip holds nothing.
    pub(crate) fn adopt_worktree_views(&mut self) -> Result<Adopted, String> {
        let strips = strips(&self.db)?;
        if strips.is_empty() {
            return Ok(Adopted::default());
        }
        // Git's files are read before the write transaction begins.
        let homes: Vec<(Scope, Home)> = strips
            .into_iter()
            .map(|(repository, path)| {
                let home = Home::of(&repository, &path);
                (Scope::Worktree { repository, path }, home)
            })
            .collect();
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        super::schema::require_current(&transaction)?;
        let mut adopted = Adopted::default();
        for (scope, home) in &homes {
            adopt(&transaction, scope, home, &mut adopted)?;
        }
        if adopted.strips > 0 {
            transaction
                .execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(adopted)
    }
}

/// The worktree strips of open projects that hold a pane or an open agent.
fn strips(db: &rusqlite::Connection) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut statement = db
        .prepare(
            "SELECT s.repository, s.worktree FROM view_scopes s
                 JOIN projects p ON p.repository = s.repository
             WHERE s.worktree IS NOT NULL
               AND EXISTS(SELECT 1 FROM panes WHERE panes.scope = s.id)
             UNION
             SELECT json_extract(a.record, '$.placement.Worktree.repository'),
                    json_extract(a.record, '$.placement.Worktree.path')
               FROM agents a
                 JOIN projects p
                   ON p.repository = json_extract(a.record, '$.placement.Worktree.repository')
              WHERE a.closed = 0
                AND json_type(a.record, '$.placement.Worktree') = 'object'
             ORDER BY 1, 2",
        )
        .map_err(|error| error.to_string())?;
    statement
        .query_map([], |row| {
            Ok((
                PathBuf::from(row.get::<_, String>(0)?),
                PathBuf::from(row.get::<_, String>(1)?),
            ))
        })
        .and_then(Iterator::collect)
        .map_err(|error| error.to_string())
}

fn adopt(
    transaction: &Transaction<'_>,
    scope: &Scope,
    home: &Home,
    adopted: &mut Adopted,
) -> Result<(), String> {
    let Scope::Worktree { repository, path } = scope else {
        return Ok(());
    };
    // A strip none of whose panes or agents can move leaves no channel
    // behind, or each start would make another.
    transaction
        .execute_batch("SAVEPOINT adopt_strip")
        .map_err(|error| error.to_string())?;
    let (channel, created) = home_channel(transaction, repository, home)?;
    let destination = Scope::Channel(channel);
    let mut recorder = views::Recorder::default();
    let strip = views::read(transaction, scope)?;
    let mut moved = 0;
    for pane in &strip.panes {
        let change = Change::Transfer {
            id: pane.id,
            destination: destination.clone(),
            before: None,
        };
        if let Some(reason) = views::refusal(transaction, scope, &change)? {
            tracing::warn!(pane = %pane.id, %reason, "a worktree strip's pane was left in place");
            adopted.kept += 1;
            continue;
        }
        let edit = ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: views::read(transaction, scope)?.revision,
            operation: uuid::Uuid::new_v4(),
            change,
        };
        views::apply_recorded(transaction, &edit, &mut recorder)?;
        moved += 1;
    }
    let left: Vec<crate::model::CardId> = views::read(transaction, scope)?
        .panes
        .iter()
        .map(|pane| pane.id)
        .collect();
    let placed = place_agents(transaction, scope, &destination, &left)?;
    if moved == 0 && placed == 0 {
        transaction
            .execute_batch("ROLLBACK TO adopt_strip; RELEASE adopt_strip")
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    place_in_navigation(transaction, repository, path, channel)?;
    transaction
        .execute_batch("RELEASE adopt_strip")
        .map_err(|error| error.to_string())?;
    adopted.strips += 1;
    adopted.panes += moved;
    adopted.created_channels += usize::from(created);
    Ok(())
}

/// The worktree's oldest channel, or a new one named after its branch. A main
/// checkout's strip gets a channel of its own rather than joining whichever
/// conversation happens to be oldest.
fn home_channel(
    transaction: &Transaction<'_>,
    repository: &Path,
    home: &Home,
) -> Result<(ChannelId, bool), String> {
    let key = repository_key(repository)?;
    if let Some(checkout) = &home.checkout {
        let existing: Option<String> = transaction
            .query_row(
                "SELECT id FROM channels WHERE repository = ?1 AND checkout = ?2
                 ORDER BY ordinal, id LIMIT 1",
                params![key, checkout_key(checkout)?],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some(id) = existing {
            return Ok((super::parse_channel_id(&id)?, false));
        }
    }
    let id = ChannelId::new();
    let ordinal = take_ordinal(transaction, "next_channel_ordinal")?;
    transaction
        .execute(
            "INSERT INTO channels(id, repository, title, ordinal, title_source, checkout)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id.to_string(),
                key,
                home.title,
                ordinal,
                ChannelTitleSource::Manual as i64,
                home.checkout.as_deref().map(checkout_key).transpose()?,
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok((id, true))
}

/// Agents still placed in the strip move with it, unless their own card stayed
/// behind. The rest of each record, including a stop's `process_end`, is kept.
fn place_agents(
    transaction: &Transaction<'_>,
    scope: &Scope,
    destination: &Scope,
    left: &[crate::model::CardId],
) -> Result<usize, String> {
    let Scope::Worktree { repository, path } = scope else {
        return Ok(0);
    };
    let (repository, path) = (repository_key(repository)?, repository_key(path)?);
    let placed: Vec<String> = transaction
        .prepare(
            "SELECT id FROM agents
              WHERE closed = 0
                AND json_extract(record, '$.placement.Worktree.repository') = ?1
                AND json_extract(record, '$.placement.Worktree.path') = ?2",
        )
        .and_then(|mut statement| {
            statement
                .query_map(params![repository, path], |row| row.get(0))?
                .collect()
        })
        .map_err(|error| error.to_string())?;
    let placement = serde_json::to_string(destination).map_err(|error| error.to_string())?;
    let mut moved = 0;
    for id in placed {
        let agent = id
            .parse::<uuid::Uuid>()
            .map_err(|error| format!("stored agent id `{id}` is unreadable: {error}"))?;
        if left.contains(&crate::model::CardId::from_uuid(agent)) {
            continue;
        }
        transaction
            .execute(
                "UPDATE agents SET record = json_set(record, '$.placement', json(?2)) WHERE id = ?1",
                params![id, placement],
            )
            .map_err(|error| error.to_string())?;
        moved += 1;
    }
    Ok(moved)
}

/// A project arranged by hand shows the channel where its worktree was.
fn place_in_navigation(
    transaction: &Transaction<'_>,
    repository: &Path,
    path: &Path,
    channel: ChannelId,
) -> Result<(), String> {
    let key = repository_key(repository)?;
    let stored: Option<String> = transaction
        .query_row(
            "SELECT items FROM navigation_orders WHERE repository = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some(stored) = stored else {
        return Ok(());
    };
    let mut items: Vec<NavigationItem> =
        serde_json::from_str(&stored).map_err(|error| error.to_string())?;
    if items.contains(&NavigationItem::Channel(channel)) {
        return Ok(());
    }
    let Some(at) = items
        .iter()
        .position(|item| matches!(item, NavigationItem::Worktree(listed) if listed == path))
    else {
        return Ok(());
    };
    items.insert(at, NavigationItem::Channel(channel));
    transaction
        .execute(
            "UPDATE navigation_orders SET items = ?2 WHERE repository = ?1",
            params![
                key,
                serde_json::to_string(&items).map_err(|error| error.to_string())?
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BrowserId, BrowserSession, CardId, CardKind, ProjectKind};
    use crate::workspace_model::CheckoutFixture;
    use crate::workspace_model::views::Pane;

    fn pane(width: f32, filing: &Path) -> Pane {
        Pane {
            id: CardId::new(),
            title: "browser".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: width,
            scope: Some(filing.to_path_buf()),
            kind: CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: "https://example.com".parse().unwrap(),
                history: vec![],
                history_index: 0,
                scroll_x: 0.,
                scroll_y: 0.,
                fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
            }),
        }
    }

    struct Store {
        registry: Registry,
        checkout: CheckoutFixture,
        _directory: tempfile::TempDir,
    }

    impl Store {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
            let checkout = CheckoutFixture::new();
            registry
                .apply(&ModelEdit::OpenProject {
                    repository: checkout.repository.clone(),
                    name: "repo".into(),
                    kind: ProjectKind::Git,
                })
                .unwrap();
            Self {
                registry,
                checkout,
                _directory: directory,
            }
        }

        fn strip(&self, path: &Path) -> Scope {
            Scope::Worktree {
                repository: self.checkout.repository.clone(),
                path: path.to_path_buf(),
            }
        }

        fn seed(&mut self, scope: &Scope, panes: Vec<Pane>) {
            self.registry
                .apply(&ModelEdit::SeedView {
                    scope: scope.clone(),
                    panes,
                    closed: vec![],
                })
                .unwrap();
        }

        fn view(&self, scope: &Scope) -> Vec<Pane> {
            self.registry
                .model()
                .unwrap()
                .views
                .into_iter()
                .find(|view| &view.scope == scope)
                .map(|view| view.panes)
                .unwrap_or_default()
        }

        fn channels(&self) -> Vec<crate::workspace_model::ModelChannel> {
            self.registry.model().unwrap().channels
        }
    }

    /// A worktree strip's panes move, in order and unchanged, into a channel
    /// named after the branch that records the worktree; a second start
    /// changes nothing.
    #[test]
    fn a_worktree_strip_moves_into_its_checkouts_channel_once() {
        let mut store = Store::new();
        let linked = store.checkout.linked.clone();
        let strip = store.strip(&linked);
        let panes = vec![pane(0.25, &linked), pane(0.75, &linked)];
        store.seed(&strip, panes.clone());

        let adopted = store.registry.adopt_worktree_views().unwrap();
        assert_eq!(
            adopted,
            Adopted {
                strips: 1,
                panes: 2,
                created_channels: 1,
                kept: 0
            }
        );
        let channel = store
            .channels()
            .into_iter()
            .find(|channel| channel.checkout.as_deref() == Some(linked.as_path()))
            .expect("the worktree's channel");
        assert_eq!(channel.title, "feature");
        assert_eq!(channel.title_source, ChannelTitleSource::Manual);
        assert_eq!(store.view(&Scope::Channel(channel.id)), panes);
        assert!(store.view(&strip).is_empty());

        let sequence = store.registry.model().unwrap().sequence;
        assert_eq!(
            store.registry.adopt_worktree_views().unwrap(),
            Adopted::default()
        );
        assert_eq!(store.registry.model().unwrap().sequence, sequence);
    }

    /// A worktree that already has a channel keeps it: the strip's panes join
    /// its oldest one after the panes it holds.
    #[test]
    fn a_strip_joins_the_oldest_channel_already_in_its_worktree() {
        let mut store = Store::new();
        let linked = store.checkout.linked.clone();
        let (older, newer) = (ChannelId::new(), ChannelId::new());
        for id in [older, newer] {
            store
                .registry
                .apply(&ModelEdit::CreateChannel {
                    id,
                    repository: store.checkout.repository.clone(),
                    title: "work".into(),
                    title_source: ChannelTitleSource::Manual,
                    checkout: Some(linked.clone()),
                })
                .unwrap();
        }
        let held = pane(0.5, &linked);
        let channel = Scope::Channel(older);
        store.seed(&channel, vec![held.clone()]);
        let strip = store.strip(&linked);
        let moved = pane(0.5, &linked);
        store.seed(&strip, vec![moved.clone()]);

        let adopted = store.registry.adopt_worktree_views().unwrap();
        assert_eq!(adopted.created_channels, 0);
        assert_eq!(store.view(&channel), vec![held, moved]);
        assert!(store.view(&Scope::Channel(newer)).is_empty());
    }

    /// A strip whose worktree was removed keeps its panes. Its channel records
    /// the missing worktree, so they start in the main checkout until it is
    /// made again, and is named after the branch Git still remembers.
    #[test]
    fn a_removed_worktrees_strip_keeps_its_panes_in_a_channel_that_records_it() {
        let mut store = Store::new();
        let linked = store.checkout.linked.clone();
        let strip = store.strip(&linked);
        let panes = vec![pane(1.0, &linked)];
        store.seed(&strip, panes.clone());
        std::fs::remove_dir_all(&linked).unwrap();

        store.registry.adopt_worktree_views().unwrap();
        let channel = store
            .channels()
            .into_iter()
            .find(|channel| channel.checkout.as_deref() == Some(linked.as_path()))
            .expect("a channel that records the missing worktree");
        assert_eq!(channel.title, "feature");
        assert_eq!(store.view(&Scope::Channel(channel.id)), panes);
    }

    /// The main checkout's strip becomes a channel of its own in the main
    /// checkout rather than joining an unrelated conversation.
    #[test]
    fn the_main_checkouts_strip_becomes_its_own_channel() {
        let mut store = Store::new();
        let existing = ChannelId::new();
        store
            .registry
            .apply(&ModelEdit::CreateChannel {
                id: existing,
                repository: store.checkout.repository.clone(),
                title: "conversation".into(),
                title_source: ChannelTitleSource::Prompt,
                checkout: None,
            })
            .unwrap();
        let main = store.checkout.main.clone();
        let strip = store.strip(&main);
        let panes = vec![pane(1.0, &main)];
        store.seed(&strip, panes.clone());

        store.registry.adopt_worktree_views().unwrap();
        let channel = store
            .channels()
            .into_iter()
            .find(|channel| channel.id != existing)
            .expect("the main checkout's channel");
        assert_eq!(channel.checkout, None);
        assert_eq!(store.view(&Scope::Channel(channel.id)), panes);
        assert!(store.view(&Scope::Channel(existing)).is_empty());
    }

    /// A project arranged by hand lists the new channel where its worktree is.
    #[test]
    fn a_hand_arranged_project_lists_the_channel_beside_its_worktree() {
        let mut store = Store::new();
        let linked = store.checkout.linked.clone();
        let main = store.checkout.main.clone();
        let items = vec![
            NavigationItem::Worktree(main.clone()),
            NavigationItem::Worktree(linked.clone()),
        ];
        store
            .registry
            .apply(&ModelEdit::ReorderNavigation {
                repository: store.checkout.repository.clone(),
                source: items[1].clone(),
                before: Some(items[0].clone()),
                fallback: items.clone(),
            })
            .unwrap();
        let strip = store.strip(&linked);
        store.seed(&strip, vec![pane(1.0, &linked)]);

        store.registry.adopt_worktree_views().unwrap();
        let channel = store.channels()[0].id;
        let order = store
            .registry
            .model()
            .unwrap()
            .navigation_orders
            .into_iter()
            .find(|order| order.repository == store.checkout.repository)
            .unwrap()
            .items;
        assert_eq!(
            order,
            vec![
                NavigationItem::Channel(channel),
                NavigationItem::Worktree(linked),
                NavigationItem::Worktree(main),
            ]
        );
    }

    /// A worktree strip takes no new pane, by insertion or by transfer.
    #[test]
    fn a_worktree_strip_refuses_new_panes() {
        use crate::workspace_model::views::Change;
        let mut store = Store::new();
        let linked = store.checkout.linked.clone();
        let strip = store.strip(&linked);
        let held = pane(1.0, &linked);
        store.seed(&strip, vec![held.clone()]);
        let revision = store
            .registry
            .model()
            .unwrap()
            .views
            .into_iter()
            .find(|view| view.scope == strip)
            .unwrap()
            .revision;
        let insert = ModelEdit::ChangeView {
            scope: strip.clone(),
            expected_revision: revision,
            operation: uuid::Uuid::new_v4(),
            change: Change::Insert {
                pane: pane(1.0, &linked),
                before: None,
            },
        };
        assert_eq!(
            store.registry.apply(&insert).unwrap_err(),
            views::WORKTREE_VIEW_CLOSED
        );

        let channel = ChannelId::new();
        store
            .registry
            .apply(&ModelEdit::CreateChannel {
                id: channel,
                repository: store.checkout.repository.clone(),
                title: "work".into(),
                title_source: ChannelTitleSource::Manual,
                checkout: None,
            })
            .unwrap();
        let elsewhere = pane(1.0, &linked);
        store.seed(&Scope::Channel(channel), vec![elsewhere.clone()]);
        let transfer = Change::Transfer {
            id: elsewhere.id,
            destination: strip.clone(),
            before: None,
        };
        assert_eq!(
            store
                .registry
                .view_change_refusal(&Scope::Channel(channel), &transfer)
                .unwrap()
                .as_deref(),
            Some(views::WORKTREE_VIEW_CLOSED)
        );
        assert_eq!(store.view(&strip), vec![held]);
    }
}
