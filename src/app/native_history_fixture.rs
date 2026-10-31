//! PR1310-only native History fixture. Compiled only by the development build.
//!
//! This admits a synthetic navigation descriptor, never a Git outcome. The
//! normal snapshot/history/detail futures and their original update arms do
//! every repository read. No session, actor, daemon, account or terminal starts.

use super::*;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    sync::OnceLock,
};

const MANIFEST_ENV: &str = "CAGENTS_DEVELOPMENT_HISTORY_FIXTURE";
const DATASET_ENV: &str = "CAGENTS_DEVELOPMENT_HISTORY_DATASET";
const BEFORE: &str = "a3221af83d56663f0dcbef90f67b30ad6d9fe53d";
const AFTER: &str = "efa9288b3fd79a7214307a11b1c6d50c6cde3630";
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const PROJECT: ProjectId = ProjectId::from_uuid(uuid::Uuid::from_u128(0x13100001));
const WORKTREE: crate::model::WorktreeId =
    crate::model::WorktreeId::from_uuid(uuid::Uuid::from_u128(0x13100002));

#[derive(serde::Deserialize)]
struct Manifest {
    schema: String,
    root: PathBuf,
    status: String,
    builder_sha256: String,
    source_pair: SourcePair,
    small: Dataset,
    large: Dataset,
    unborn: Dataset,
}

#[derive(serde::Deserialize)]
struct SourcePair {
    before: String,
    after: String,
}

#[derive(serde::Deserialize)]
struct Dataset {
    root: PathBuf,
    branch: String,
    head_oid: Option<String>,
    mainline_commits: Option<u64>,
    all_commits: Option<u64>,
    commits: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    uid: u32,
    mode: u32,
}

impl Identity {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            uid: metadata.uid(),
            mode: metadata.mode(),
        }
    }
}

pub(super) struct Context {
    root: PathBuf,
    root_identity: Identity,
    repository: PathBuf,
    repository_identity: Identity,
    git_identity: Identity,
    name: &'static str,
    expected_head: Option<String>,
}

static CONTEXT: OnceLock<Result<Option<Context>, String>> = OnceLock::new();

/// Validated before the binary's operational startup. Absence reads no files;
/// malformed or partial opt-in is an error, never a natural-startup fallback.
pub(super) fn context() -> Result<Option<&'static Context>, String> {
    CONTEXT
        .get_or_init(read_context)
        .as_ref()
        .map(Option::as_ref)
        .map_err(Clone::clone)
}

pub(super) fn active() -> bool {
    context()
        .expect("native History fixture context must be validated before init")
        .is_some()
}

fn read_context() -> Result<Option<Context>, String> {
    let manifest = std::env::var_os(MANIFEST_ENV);
    let dataset = std::env::var_os(DATASET_ENV);
    if manifest.is_none() && dataset.is_none() {
        return Ok(None);
    }
    let manifest = PathBuf::from(manifest.ok_or("History fixture manifest is required")?);
    let dataset = dataset.ok_or("History fixture dataset is required")?;
    let (key, directory, count) = match dataset.to_str() {
        Some("small") => ("small", "small401", 401),
        Some("large") => ("large", "large25000", 25_000),
        Some("unborn") => ("unborn", "unborn", 0),
        _ => return Err("History fixture dataset must be small, large or unborn".into()),
    };
    // These opt-in conflict checks add no environment scrubbing. The original
    // launcher's literal inheritance is distinct from normal pre-main account
    // selection detachment: it may remove CODEX_HOME/other selected provider
    // homes and always CAGENTS_ACCOUNT_HOMES before these checks. That existing
    // policy is unchanged; no value/account/profile is switched or variable
    // presence/absence forced for this fixture. This is not a HOME-access audit.
    if std::env::vars_os().any(|(name, _)| {
        name.to_str().is_some_and(|name| {
            name.starts_with("GIT_")
                || name.starts_with("CAGENTS_LATENCY_")
                || (name.starts_with("CAGENTS_")
                    && name.contains("FIXTURE")
                    && name != MANIFEST_ENV)
        })
    }) {
        return Err(
            "History fixture conflicts with another fixture or inherited GIT_* setting".into(),
        );
    }
    exact_absolute(&manifest)?;
    let root = manifest
        .parent()
        .ok_or("History fixture manifest has no root")?;
    if manifest.file_name().and_then(|name| name.to_str()) != Some("fixture.json")
        || !matches!(root.parent(), Some(parent) if parent == Path::new("/tmp") || parent == Path::new("/var/tmp"))
    {
        return Err(
            "History fixture must be fixture.json in its direct private temporary root".into(),
        );
    }
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("History fixture root is not UTF-8")?;
    let suffix = name
        .strip_prefix("pr1310-native-fixture-")
        .ok_or("History fixture root name is invalid")?;
    if suffix.is_empty()
        || suffix.len() > 80
        || !suffix.as_bytes()[0].is_ascii_alphanumeric()
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err("History fixture root suffix is invalid".into());
    }
    let root_metadata = owned_directory(root)?;
    if root_metadata.mode() & 0o7777 != 0o700 {
        return Err("History fixture root must be owned and mode 0700".into());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&manifest)
        .map_err(|error| format!("Cannot read History fixture manifest: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != root_metadata.uid()
        || metadata.nlink() != 1
        || metadata.mode() & 0o022 != 0
        || metadata.len() > MAX_MANIFEST_BYTES
    {
        return Err("History fixture manifest is not a bounded owned regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("History fixture manifest grew beyond the bound".into());
    }
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Invalid History fixture JSON: {error}"))?;
    if manifest.schema != "pr1310-public-git-fixture-v1"
        || manifest.status != "GENERATED_GIT_DATA_ONLY_NOT_NATIVE_QUALIFICATION"
        || manifest.root != root
        || manifest.source_pair.before != BEFORE
        || manifest.source_pair.after != AFTER
        || manifest.builder_sha256
            != format!(
                "{:x}",
                Sha256::digest(include_bytes!(
                    "../../qualification-packets/pr1310-native-fixture/build.py"
                ))
            )
    {
        return Err("History fixture schema, source pair or builder does not match".into());
    }
    let selected = match key {
        "small" => &manifest.small,
        "large" => &manifest.large,
        _ => &manifest.unborn,
    };
    if selected.root != root.join(directory)
        || selected.branch != "main"
        || (count == 0 && (selected.head_oid.is_some() || selected.commits != Some(0)))
        || (count != 0
            && (selected.mainline_commits != Some(count)
                || selected.all_commits != Some(count + 1)
                || !selected.head_oid.as_deref().is_some_and(object_id)))
    {
        return Err("History fixture selected repository declaration is incomplete".into());
    }
    let repository_identity = Identity::of(&owned_directory(&selected.root)?);
    let git_identity = Identity::of(&owned_directory(&selected.root.join(".git"))?);
    let result = Context {
        root: root.to_path_buf(),
        root_identity: Identity::of(&root_metadata),
        repository: selected.root.clone(),
        repository_identity,
        git_identity,
        name: key,
        expected_head: selected.head_oid.clone(),
    };
    result.recheck()?;
    Ok(Some(result))
}

fn object_id(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn exact_absolute(path: &Path) -> Result<(), String> {
    // Path::components normalizes '.', so compare the literal spelling too.
    let text = path.to_str().ok_or("History fixture path is not UTF-8")?;
    if !text.starts_with('/')
        || text.ends_with('/')
        || text[1..]
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("History fixture path must be an exact absolute path without aliases".into());
    }
    let mut ancestor = PathBuf::from("/");
    for component in path.components().skip(1) {
        ancestor.push(component);
        if fs::symlink_metadata(&ancestor)
            .map_err(|error| error.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("History fixture paths cannot traverse symlinks".into());
        }
    }
    if path.canonicalize().map_err(|error| error.to_string())? != path {
        return Err("History fixture path is not canonical".into());
    }
    Ok(())
}

fn owned_directory(path: &Path) -> Result<fs::Metadata, String> {
    exact_absolute(path)?;
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    // SAFETY: geteuid has no pointer arguments or side effects.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err("History fixture directory must be owned and not group/world writable".into());
    }
    Ok(metadata)
}

impl Context {
    fn key(&self) -> ActiveWorktreeKey {
        ActiveWorktreeKey {
            project_id: PROJECT,
            worktree_id: WORKTREE,
            canonical_path: self.repository.clone(),
        }
    }

    fn recheck(&self) -> Result<(), String> {
        let git = self.repository.join(".git");
        for (path, identity) in [
            (&self.root, self.root_identity),
            (&self.repository, self.repository_identity),
            (&git, self.git_identity),
        ] {
            if Identity::of(&owned_directory(path)?) != identity {
                return Err("History fixture root/repository identity changed".into());
            }
        }
        Ok(())
    }

    pub(super) fn startup(&self) -> Startup {
        let mut workspace = WorkspaceState::default();
        workspace.id = crate::model::WorkspaceId::from_uuid(uuid::Uuid::from_u128(0x13100003));
        workspace.active_worktree = Some(self.repository.clone());
        workspace.project_order = vec![PROJECT];
        let mut settings = AppSettings::default();
        settings.app_theme = AppTheme::Dark;
        settings.ui_scale = 1.0;
        settings.sidebar_width = 320.0;
        settings.inspector_width = 480.0;
        settings.language_servers.clear();
        settings.editor_auto_save = false;
        Startup {
            bootstrap: Bootstrap {
                workspace,
                session: None,
                projects: vec![ProjectState {
                    id: PROJECT,
                    generation: 0,
                    name: format!("PR1310 synthetic {}", self.name),
                    repository: self.repository.clone(),
                    kind: ProjectKind::Git,
                    existence: Existence::Open,
                }],
                worktrees: vec![WorktreeState {
                    id: WORKTREE,
                    project_id: PROJECT,
                    generation: 0,
                    path: self.repository.clone(),
                    branch: Some("main".into()),
                    existence: Existence::Open,
                }],
                settings,
                settings_path: None,
                settings_write_blocked: true,
                status: None,
                save_health: None,
                restore_warnings: Vec::new(),
                settings_warnings: Vec::new(),
            },
            window_view: None,
        }
    }

    pub(super) fn install(&self, app: &mut Cagents) -> Result<Task<Message>, String> {
        self.recheck()?;
        if app.session.is_some() || !app.durable.cards.is_empty() || !app.cards.is_empty() {
            return Err("History fixture requires its empty sessionless startup".into());
        }
        app.active_workspace = Some(self.repository.clone());
        app.context_tab = ContextTab::Changes;
        app.context_inspector_open = true;
        app.context_inspector_transition = None;
        app.sidebar_state.sync_active_worktree(
            &app.sidebar_model,
            &SidebarFocus::Repository {
                worktree: self.repository.clone(),
            },
        );
        let key = self.key();
        app.sidebar_state.apply(&SidebarAction::SetChangesTab {
            key: key.clone(),
            tab: sidebar::ChangesTab::History,
        });
        app.sidebar_model.source_control = Some(SourceControlModel::loading(key.clone()));
        // No query or completion is synthesized. The ordinary initial History
        // page is requested by load_pending_history after the real snapshot.
        Ok(Task::batch([
            app.synchronize_appearance(1.0),
            app.source_control_snapshot_task(key),
            app.sync_chrome(),
        ]))
    }
}

/// Pure routing restriction, not User/native resource admission. The admitted
/// messages continue through the unchanged production match arms; wrappers
/// that schedule unrelated work are bypassed only for this fixture.
pub(super) fn admits(app: &Cagents, message: &Message) -> bool {
    let Some(context) = context().expect("validated History fixture context") else {
        return false;
    };
    let bound = |key: &ActiveWorktreeKey| *key == context.key();
    match message {
        Message::SourceControlLoaded { key, .. } => bound(key),
        Message::SourceControlHistory { key, .. } => bound(key),
        Message::SourceControlReleased
        | Message::CommitUi(_)
        | Message::BranchUi(_)
        | Message::WindowResized(..)
        | Message::WindowScaleFactorChanged(_)
        | Message::WindowEdgeState { .. }
        | Message::WindowFilled(..)
        | Message::Frame(_)
        | Message::Viewport { .. }
        | Message::ToggleContextInspector => true,
        Message::SelectContextTab(ContextTab::Changes) => true,
        Message::Sidebar(action) => match action {
            SidebarAction::HistoryQueryChanged { key, .. }
            | SidebarAction::SearchHistory { key, .. }
            | SidebarAction::LoadMoreHistory { key, .. }
            | SidebarAction::OpenCommit { key, .. }
            | SidebarAction::CloseCommit { key }
            | SidebarAction::ShowMoreCommitFiles { key, .. }
            | SidebarAction::OpenCommitFile { key, .. }
            | SidebarAction::RefreshWorktree { key }
            | SidebarAction::SetChangesTab {
                key,
                tab: sidebar::ChangesTab::History,
            }
            | SidebarAction::ToggleSourceControlPanel { key } => bound(key),
            SidebarAction::Resize { .. } => true,
            _ => false,
        },
        Message::Repository(action) => {
            matches!(app.cards.get(&action.card()), Some(CardRuntime::Repository(runtime)) if runtime.descriptor.worktree == context.repository)
                && matches!(
                    action,
                    RepositoryAction::FilesLoaded { .. }
                        | RepositoryAction::FileListUnchanged { .. }
                        | RepositoryAction::OpenCommittedFile { .. }
                        | RepositoryAction::DiffLoaded { .. }
                        | RepositoryAction::RetryDiff { .. }
                        | RepositoryAction::DiffViewport { .. }
                        | RepositoryAction::ApplyDiffMode { .. }
                        | RepositoryAction::PreviousChange { .. }
                        | RepositoryAction::NextChange { .. }
                        | RepositoryAction::RevealDiffTop { .. }
                        | RepositoryAction::ExpandDiffFold { .. }
                )
        }
        Message::FocusCard(card) => {
            matches!(app.cards.get(card), Some(CardRuntime::Repository(runtime)) if runtime.descriptor.worktree == context.repository)
        }
        _ => false,
    }
}

/// Special fixture teardown/start observation only; no startup handshake or
/// normal close/save/actor settlement is claimed by this native workload.
pub(super) fn local_message(message: &Message) -> Option<Task<Message>> {
    if let Message::SourceControlLoaded {
        key,
        result: Ok(Some(prepared)),
        ..
    } = message
        && let Some(context) = context().expect("validated History fixture context")
        && *key == context.key()
        && prepared.1.head_oid != context.expected_head
    {
        // A changed fixture is an admission failure, not a synthesized Git
        // error state or a page left spinning after its completion was hidden.
        tracing::error!(expected = ?context.expected_head, actual = ?prepared.1.head_oid,
            "History fixture HEAD differs from the builder manifest; stopping fixture");
        return Some(iced::exit().map(cosmic::Action::App));
    }
    match message {
        Message::StartupWindowOpened(_) | Message::Sidebar(SidebarAction::ResizeFinished) => {
            Some(Task::none())
        }
        Message::WindowCloseRequested(_) | Message::WindowClosed(_) => {
            Some(iced::exit().map(cosmic::Action::App))
        }
        _ => None,
    }
}

pub(super) fn after_update(app: &mut Cagents, task: Task<Message>) -> Task<Message> {
    Task::batch([task, app.load_pending_history(), app.sync_chrome()])
}

pub(super) fn subscription(app: &Cagents) -> Subscription<Message> {
    let native = event::listen_with(|event, _, event_window| match event {
        iced::Event::Window(window::Event::Rescaled(scale)) => {
            Some(Message::WindowScaleFactorChanged(scale))
        }
        iced::Event::Window(window::Event::CloseRequested) => {
            Some(Message::WindowCloseRequested(event_window))
        }
        iced::Event::Window(window::Event::Closed) => Some(Message::WindowClosed(event_window)),
        iced::Event::Window(event) => window_geometry_message(&event),
        event @ iced::Event::PlatformSpecific(_) => window_edge_message(&event, event_window),
        _ => None,
    });
    let frames = app
        .core
        .main_window_id()
        .filter(|_| app.workspace.is_animating() || app.context_inspector_transition.is_some())
        .map(|window| workspace_widget::animation_frames(window, Message::Frame))
        .unwrap_or_else(Subscription::none);
    // Widget callbacks are delivered by iced itself. There is deliberately no
    // global KeyEvent/RefreshUi routing, watcher, tick, account or daemon stream.
    Subscription::batch([native, frames])
}
