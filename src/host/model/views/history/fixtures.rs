use super::*;
use crate::{
    host::model::Registry,
    model::{BrowserId, BrowserSession, CardKind, ChannelId},
    workspace_model::{ModelEdit, views::Pane},
};

pub(super) fn fixture() -> (Registry, tempfile::TempDir, [Scope; 2]) {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = Registry::at(&dir.path().join("model.sqlite3")).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: dir.path().into(),
            name: "History fixture".into(),
            kind: Default::default(),
        })
        .unwrap();
    let scopes = [
        Scope::Channel(ChannelId::new()),
        Scope::Channel(ChannelId::new()),
    ];
    for scope in &scopes {
        let Scope::Channel(id) = scope else {
            unreachable!()
        };
        registry
            .apply(&ModelEdit::CreateChannel {
                id: *id,
                repository: dir.path().into(),
                title: "History fixture".into(),
                title_source: Default::default(),
            })
            .unwrap();
        registry
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![],
                closed: vec![],
            })
            .unwrap();
    }
    (registry, dir, scopes)
}

// Capture all published fixture rows, including CoreHome/placement/execution
// namespaces. These are synthetic rows in this test's own private Registry.
pub(super) fn rows(db: &Connection, skip: &[&str]) -> Vec<(String, Vec<Vec<String>>)> {
    let names = db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*' ORDER BY name")
        .unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap()
        .collect::<rusqlite::Result<Vec<_>>>().unwrap();
    names
        .into_iter()
        .filter(|name| !skip.contains(&name.as_str()))
        .map(|name| {
            let select = format!("SELECT * FROM \"{}\"", name.replace('"', "\"\""));
            let columns = db.prepare(&select).unwrap().column_count();
            let order = (1..=columns)
                .map(|column| column.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut stmt = db.prepare(&format!("{select} ORDER BY {order}")).unwrap();
            let values = stmt
                .query_map([], |row| {
                    (0..columns)
                        .map(|column| row.get_ref(column).map(|value| format!("{value:?}")))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            (name, values)
        })
        .collect()
}

pub(super) fn pane() -> Pane {
    Pane {
        id: CardId::new(),
        title: "Original".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: "https://example.com/".parse().unwrap(),
            history: vec![],
            history_index: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
        }),
    }
}
