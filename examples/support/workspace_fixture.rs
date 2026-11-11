//! Fixture ownership and the provider-free focused-pane transfer scenario.

use cagents::{
    control::ControlClient,
    model::{BrowserId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind, ChannelId},
    terminal::ipc::daemon_socket_path,
    workspace_model::{
        ModelEdit, WorkspaceModel,
        views::{Change, Pane, Scope, View},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    ffi::OsStr,
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn namespace(instance: Option<&OsStr>) -> Option<String> {
    match instance {
        None => Some("cagents-development".into()),
        Some(name) => {
            let name = name.to_str()?;
            if name.is_empty() {
                Some("cagents-development".into())
            } else if name.len() <= 24
                && name.as_bytes()[0].is_ascii_alphanumeric()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                Some(format!("cagents-development-{name}"))
            } else {
                None
            }
        }
    }
}

pub struct Fixture {
    pub root: PathBuf,
    pub task: PathBuf,
    pub socket: PathBuf,
    namespace: String,
    data: PathBuf,
}

impl Fixture {
    pub fn open(root: PathBuf) -> Result<Self> {
        let root = root.canonicalize()?;
        let task = root.parent().ok_or("fixture has no parent")?.to_owned();
        if !task.join(".native-evidence").is_file()
            || root.file_name().is_none_or(|name| name != "fixture")
        {
            return Err("the root must be a native-evidence task's fixture directory".into());
        }
        let socket = daemon_socket_path()?;
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR")?);
        let instance = std::env::var_os("CAGENTS_DEVELOPMENT_INSTANCE");
        let namespace =
            namespace(instance.as_deref()).ok_or("invalid development instance name")?;
        let nested = runtime.file_name().is_some_and(|part| {
            let part = part.to_string_lossy();
            part.starts_with("nh-") && part.len() == 11
        }) && socket == runtime.join(&namespace).join("cagentsd.sock");
        let data = PathBuf::from(std::env::var_os("XDG_DATA_HOME").ok_or("XDG_DATA_HOME")?);
        if !(socket.starts_with(&task) || nested) || !data.starts_with(&root) {
            return Err("the selected daemon and data directory must belong to the fixture".into());
        }
        Ok(Self {
            root,
            task,
            socket,
            namespace,
            data,
        })
    }

    fn marker(&self, path: PathBuf) -> Result<PathBuf> {
        let parent = path
            .parent()
            .ok_or("marker has no parent")?
            .canonicalize()?;
        if !parent.starts_with(&self.task) || parent.starts_with(&self.root) {
            return Err("the marker must belong to this task outside its fixture".into());
        }
        Ok(parent.join(path.file_name().ok_or("marker has no file name")?))
    }
}

/// Publish a complete marker without replacing a prior release or exposing a
/// partially serialized file to a polling consumer.
pub fn publish_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().ok_or("marker has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(temporary.as_file_mut(), value)?;
    writeln!(temporary)?;
    temporary.flush()?;
    temporary
        .persist_noclobber(path)
        .map_err(|failure| failure.error)?;
    Ok(())
}

pub fn browser_pane(id: CardId, browser: BrowserId, title: &str) -> Result<Pane> {
    Ok(Pane {
        id,
        title: title.into(),
        user_title: Some(title.into()),
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Browser(BrowserSession {
            id: browser,
            current_url: "about:blank".parse()?,
            history: vec![],
            history_index: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            fidelity: BrowserRestoreFidelity::UrlOnly,
        }),
    })
}

#[derive(Serialize)]
struct Store {
    role: &'static str,
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Store {
    fn new(role: &'static str, path: PathBuf, fixture: &Fixture) -> Result<Self> {
        let path = path.canonicalize()?;
        if !path.starts_with(&fixture.root) {
            return Err("the store must belong to this fixture".into());
        }
        let metadata = path.metadata()?;
        if !metadata.is_file() {
            return Err("the store must be a regular file".into());
        }
        Ok(Self {
            role,
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn unchanged(&self) -> Result<()> {
        let metadata = self.path.metadata()?;
        if metadata.dev() != self.device || metadata.ino() != self.inode {
            return Err("a prepared store was replaced".into());
        }
        Ok(())
    }
}

#[derive(Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Go {
    schema: u8,
    event: String,
    nonce: uuid::Uuid,
    registry: uuid::Uuid,
    source_channel: ChannelId,
    sibling: CardId,
    moved: CardId,
    destination_scope: Scope,
}

fn view<'a>(model: &'a WorkspaceModel, scope: &Scope) -> Result<&'a View> {
    let mut views = model.views.iter().filter(|view| &view.scope == scope);
    let found = views.next().ok_or("the prepared scope is absent")?;
    if views.next().is_some() {
        return Err("the prepared scope is ambiguous".into());
    }
    Ok(found)
}

/// This branch intentionally uses the carried legacy RPC. Its source is shared
/// with protocol-166 baseline examples; the timed stage-4 branch stays modern.
pub fn focused_transfer(fixture: &Fixture, worktree: PathBuf) -> Result<()> {
    if std::env::var_os("CAGENTS_AGENT_AUTHORITY").is_some() {
        return Err("the fixture needs a genuine User shell; do not remove agent authority".into());
    }
    if std::env::var_os("CAGENTS_BOUNDED").is_none() {
        return Err("run the focused transfer under scripts/bounded".into());
    }
    let marker = fixture.marker(PathBuf::from(
        std::env::var_os("CAGENTS_WORKSPACE_EDIT_READY").ok_or("a ready marker is required")?,
    ))?;
    let go_path = marker.with_extension("go");
    if marker.exists() || go_path.exists() {
        return Err("use fresh ready/go paths for the focused transfer".into());
    }
    let worktree = worktree.canonicalize()?;
    if !worktree.starts_with(&fixture.root) || !worktree.is_dir() {
        return Err("the worktree must belong to this fixture".into());
    }
    let mut client = ControlClient::connect(&fixture.socket)?;
    let initial = client.read_workspace_model()?;
    if initial.identity.is_nil() {
        return Err("the fixture registry must have a durable identity".into());
    }
    let mut destinations = initial
        .views
        .iter()
        .filter(|view| matches!(&view.scope, Scope::Worktree { path, .. } if path == &worktree));
    let mut destination = destinations
        .next()
        .ok_or("the fixture worktree must already be discovered")?
        .clone();
    if destinations.next().is_some() {
        return Err("the worktree destination is ambiguous".into());
    }
    let Scope::Worktree { repository, .. } = &destination.scope else {
        unreachable!()
    };
    let repository = repository.clone();
    if !repository.is_absolute()
        || !repository.starts_with(&fixture.root)
        || repository.canonicalize()? != repository
        || initial.project(&repository).is_none()
    {
        return Err("the destination must belong to an open fixture project".into());
    }
    let data = fixture.data.join(&fixture.namespace);
    let registry = Store::new("registry", data.join("workspace-model.sqlite3"), fixture)?;
    let stores = [
        Store::new("active_source", data.join("sessions.sqlite3"), fixture)?,
        Store::new(
            "inactive_source",
            data.join("windows/window-2/sessions.sqlite3"),
            fixture,
        )?,
    ];
    if stores[0].device == stores[1].device && stores[0].inode == stores[1].inode {
        return Err("the two windows must have distinct stores".into());
    }
    // The inactive window needs a real selected strip, not an unproved
    // window-local default. Add only a fresh fixture anchor when none of the
    // existing canonical occupants is an ordinary pane; never rewrite them.
    if destination.panes.iter().all(|pane| pane.kind.is_utility()) {
        let mut anchor = browser_pane(
            CardId::new(),
            BrowserId::new(),
            "Transfer destination anchor",
        )?;
        anchor.scope = Some(worktree.clone());
        let anchored = client.edit_workspace_model(ModelEdit::ChangeView {
            scope: destination.scope.clone(),
            expected_revision: destination.revision,
            operation: uuid::Uuid::new_v4(),
            change: Change::Insert {
                pane: anchor.clone(),
                before: None,
            },
        })?;
        let prepared = view(&anchored, &destination.scope)?.clone();
        if anchored.identity != initial.identity
            || prepared.panes.len() != destination.panes.len() + 1
            || !prepared.panes.starts_with(&destination.panes)
            || prepared.panes.last() != Some(&anchor)
            || prepared.retired != destination.retired
        {
            return Err("anchor preparation changed an existing destination occupant".into());
        }
        destination = prepared;
    }
    let source_channel = ChannelId::new();
    let source_scope = Scope::Channel(source_channel);
    let sibling = browser_pane(CardId::new(), BrowserId::new(), "Transfer sibling")?;
    let moved = browser_pane(CardId::new(), BrowserId::new(), "Transfer target")?;
    let mut model = client.edit_workspace_model(ModelEdit::CreateChannel {
        id: source_channel,
        repository: repository.clone(),
        title: "Focused transfer proof".into(),
        title_source: cagents::model::ChannelTitleSource::Manual,
    })?;
    for pane in [&sibling, &moved] {
        let revision = model
            .views
            .iter()
            .find(|view| view.scope == source_scope)
            .map_or(0, |view| view.revision);
        model = client.edit_workspace_model(ModelEdit::ChangeView {
            scope: source_scope.clone(),
            expected_revision: revision,
            operation: uuid::Uuid::new_v4(),
            change: Change::Insert {
                pane: pane.clone(),
                before: None,
            },
        })?;
    }
    let source = view(&model, &source_scope)?.clone();
    if source.panes != [sibling.clone(), moved.clone()]
        || !source.retired.is_empty()
        || model.identity != initial.identity
        || view(&model, &destination.scope)? != &destination
    {
        return Err("the prepared views changed while creating the fixture".into());
    }
    let nonce = uuid::Uuid::new_v4();
    let prepared = serde_json::json!({
        "schema":1, "event":"focused_transfer_prepared", "nonce":nonce,
        "fixture_root":fixture.root, "namespace":fixture.namespace,
        "registry":{"path":registry.path,"identity":model.identity,"device":registry.device,"inode":registry.inode},
        "stores":stores,
        "source":{"channel":source_channel,"title":"Focused transfer proof","repository":repository,
            "scope":source.scope,"revision":source.revision,"panes":source.panes,"retired":source.retired},
        "destination":{"scope":destination.scope,"revision":destination.revision,"panes":destination.panes,"retired":destination.retired},
        "sibling":sibling.id,"moved":moved.id
    });
    publish_json(&marker, &prepared)?;
    println!("{prepared}");
    let expected = Go {
        schema: 1,
        event: "focused_transfer_go".into(),
        nonce,
        registry: model.identity,
        source_channel,
        sibling: sibling.id,
        moved: moved.id,
        destination_scope: destination.scope.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    while !go_path.exists() {
        if Instant::now() >= deadline {
            return Err("no verified native selection within 120 seconds".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let go_path = go_path.canonicalize()?;
    if !go_path.starts_with(&fixture.task) || go_path.starts_with(&fixture.root) {
        return Err("the go marker must belong to this task outside its fixture".into());
    }
    if serde_json::from_slice::<Go>(&std::fs::read(go_path)?)? != expected {
        return Err("the go marker does not match the exact prepared transfer".into());
    }
    registry.unchanged()?;
    for store in &stores {
        store.unchanged()?;
    }
    let current = client.read_workspace_model()?;
    if current.identity != model.identity
        || view(&current, &source.scope)? != &source
        || view(&current, &destination.scope)? != &destination
        || current.channel(source_channel) != model.channel(source_channel)
    {
        return Err("the registry or prepared views changed before go".into());
    }
    let committed = client.edit_workspace_model(ModelEdit::ChangeView {
        scope: source.scope.clone(),
        expected_revision: source.revision,
        operation: nonce,
        change: Change::Transfer {
            id: moved.id,
            destination: destination.scope.clone(),
            before: None,
        },
    })?;
    let after_source = view(&committed, &source.scope)?;
    let after_destination = view(&committed, &destination.scope)?;
    let mut received = after_destination
        .panes
        .last()
        .ok_or("the moved pane is absent")?
        .clone();
    if received.scope.as_ref() == Some(&worktree) {
        received.scope = moved.scope.clone();
    }
    if after_source.panes != [sibling]
        || after_destination.panes.len() != destination.panes.len() + 1
        || !after_destination.panes.starts_with(&destination.panes)
        || received != moved
    {
        return Err(
            "the committed transfer did not preserve exact source/destination panes".into(),
        );
    }
    println!(
        "{}",
        serde_json::json!({"schema":1,"event":"focused_transfer_committed","nonce":nonce,
        "registry":committed.identity,"source_channel":source_channel,"sibling":expected.sibling,
        "moved":moved.id,"destination_scope":destination.scope,
        "source_revision":after_source.revision,"destination_revision":after_destination.revision})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn polling_consumers_only_see_the_complete_published_marker() {
        struct Checked<'a>(&'a Path);
        impl Serialize for Checked<'_> {
            fn serialize<S: serde::Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                assert!(
                    !self.0.exists(),
                    "a polling consumer could see an unfinished marker"
                );
                serializer.serialize_str(&"complete".repeat(4096))
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ready.json");
        publish_json(&path, &Checked(&path)).unwrap();
        assert_eq!(
            serde_json::from_slice::<String>(&std::fs::read(&path).unwrap()).unwrap(),
            "complete".repeat(4096)
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn marker_collision_keeps_original_content_and_cleans_the_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ready.json");
        std::fs::write(&path, b"original").unwrap();
        assert!(publish_json(&path, &"replacement").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_serialization_leaves_neither_marker_nor_temporary() {
        struct Rejected;
        impl Serialize for Rejected {
            fn serialize<S: serde::Serializer>(
                &self,
                _: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom(
                    "intentional serialization failure",
                ))
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ready.json");
        assert!(publish_json(&path, &Rejected).is_err());
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn default_namespaces_accept_unset_and_explicit_empty() {
        assert_eq!(namespace(None).as_deref(), Some("cagents-development"));
        assert_eq!(namespace(Some(OsStr::new(""))), namespace(None));
        assert_eq!(
            namespace(Some(OsStr::new("proof-1"))).as_deref(),
            Some("cagents-development-proof-1")
        );
    }

    #[test]
    fn namespaces_refuse_invalid_paths_names_and_non_unicode() {
        for name in [
            "../fixture",
            "has space",
            "has_underscore",
            "-bad",
            "abcdefghijklmnopqrstuvwxyz",
        ] {
            assert_eq!(namespace(Some(OsStr::new(name))), None);
        }
        assert_eq!(namespace(Some(OsStr::from_bytes(&[0xff]))), None);
    }

    #[test]
    fn measurement_browser_descriptor_retains_the_existing_payload() {
        let id = CardId::new();
        let browser = BrowserId::new();
        let pane = browser_pane(id, browser, "Edit measurement").unwrap();
        assert_eq!(pane.id, id);
        assert_eq!(pane.title, "Edit measurement");
        assert_eq!(pane.user_title.as_deref(), Some("Edit measurement"));
        assert!(!pane.pending_terminal_start);
        assert_eq!(pane.width_fraction, 0.5);
        assert_eq!(pane.scope, None);
        let CardKind::Browser(session) = pane.kind else {
            panic!("not a Browser")
        };
        assert_eq!(session.id, browser);
        assert_eq!(session.current_url.as_str(), "about:blank");
        assert!(session.history.is_empty());
        assert_eq!(session.history_index, 0);
        assert_eq!((session.scroll_x, session.scroll_y), (0.0, 0.0));
        assert_eq!(session.fidelity, BrowserRestoreFidelity::UrlOnly);
    }
}
