//! PR1310 Before-only native History fixture on the original a322 APIs.
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
    manifest_sha256: String,
    source_sha256: [String; 3],
    appearance: std::sync::Mutex<AppearanceObserver>,
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
        manifest_sha256: format!("{:x}", Sha256::digest(&bytes)),
        // Literal source bytes included by this compilation, not a hardcoded
        // claimed commit/hash or whole-product/matched-executable certificate.
        source_sha256: [
            format!(
                "{:x}",
                Sha256::digest(include_bytes!("native_history_fixture.rs"))
            ),
            format!("{:x}", Sha256::digest(include_bytes!("../app.rs"))),
            format!(
                "{:x}",
                Sha256::digest(include_bytes!("../bin/cagents-ui.rs"))
            ),
        ],
        appearance: std::sync::Mutex::new(AppearanceObserver::new()),
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
        let window = app
            .core
            .main_window_id()
            .ok_or("History fixture has no main window target")?;
        let mut observer = self
            .appearance
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if observer.target.is_some() {
            return Err("History appearance observer already installed".into());
        }
        observer.target = Some(window);
        // Opened and native_scale remain unknown until original window events.
        drop(observer);
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
        Message::CommitUi(_)
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
            SidebarAction::LoadMoreHistory { key, .. }
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
        result: Ok(snapshot),
        ..
    } = message
        && let Some(context) = context().expect("validated History fixture context")
        && *key == context.key()
    {
        // Before returns the actual RepositorySnapshot, not After's prepared
        // model envelope. Observe its existing HeadState without fabricating
        // a SourceControlModel or changing the original completion handler.
        let actual_head = match &snapshot.status.head {
            crate::repository::HeadState::Branch { oid, .. }
            | crate::repository::HeadState::Detached { oid } => Some(oid.as_str()),
            crate::repository::HeadState::Unborn { .. } | crate::repository::HeadState::Unknown => {
                None
            }
        };
        if actual_head != context.expected_head.as_deref() {
            // A changed fixture is an admission failure, not a synthesized Git
            // error state or a page left spinning after hiding its completion.
            tracing::error!(expected = ?context.expected_head, actual = ?actual_head,
                "History fixture HEAD differs from the builder manifest; stopping fixture");
            return Some(iced::exit().map(cosmic::Action::App));
        }
    }
    match message {
        Message::StartupWindowOpened(window) => Some(initial_native_scale_task(*window)),
        Message::Sidebar(SidebarAction::ResizeFinished) => Some(Task::none()),
        Message::WindowCloseRequested(_) | Message::WindowClosed(_) => {
            Some(iced::exit().map(cosmic::Action::App))
        }
        _ => None,
    }
}

/// Request only after the observer has received the original matching Opened
/// event. The normal Opened -> geometry mapping is retained; the native open
/// task's StartupWindowOpened callback alone cannot establish that proof.
fn initial_native_scale_task(window: window::Id) -> Task<Message> {
    let Some(context) = context().expect("validated History fixture context") else {
        return Task::none();
    };
    let request = context
        .appearance
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .begin_native_scale_query(window);
    let Some(request) = request else {
        return Task::none();
    };
    window::scale_factor(window).map(move |scale| {
        context
            .appearance
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .finish_native_scale_query(request, scale);
        // An existing fixture-local message wakes a view without entering the
        // normal startup handshake. The issued-request latch prevents recursion.
        cosmic::Action::App(Message::StartupWindowOpened(window))
    })
}

pub(super) fn after_update(app: &mut Cagents, task: Task<Message>) -> Task<Message> {
    Task::batch([
        task,
        app.load_pending_history(),
        app.sync_chrome(),
        app.core
            .main_window_id()
            .map(initial_native_scale_task)
            .unwrap_or_else(Task::none),
    ])
}

pub(super) fn subscription(app: &Cagents) -> Subscription<Message> {
    let native = event::listen_with(|event, _, event_window| {
        if !observe_native_window(event_window, &event) {
            return None;
        }
        match event {
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
        }
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

// This observer records applied shell state. It is not a compositor-present,
// renderer, authority, successful-log-write or matched-binary acknowledgement.
const APPEARANCE_RECORD_LIMIT: u64 = 256;
const APPEARANCE_RECORD_BYTES: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
struct NativeScaleRequest {
    observer_instance: uuid::Uuid,
    window: window::Id,
    opened_generation: u64,
    request: uuid::Uuid,
    native_scale_generation: u64,
}

struct AppearanceObserver {
    instance: uuid::Uuid,
    target: Option<window::Id>,
    opened: Option<window::Id>,
    opened_generation: u64,
    native_scale: Option<f32>,
    native_scale_source: &'static str,
    native_scale_generation: u64,
    native_scale_query: Option<NativeScaleRequest>,
    native_scale_query_pending: bool,
    generation: u64,
    last: Option<AppearanceKey>,
    stopped: bool,
}

impl AppearanceObserver {
    fn new() -> Self {
        Self {
            instance: uuid::Uuid::new_v4(),
            target: None,
            opened: None,
            opened_generation: 0,
            native_scale: None,
            native_scale_source: "unknown",
            native_scale_generation: 0,
            native_scale_query: None,
            native_scale_query_pending: false,
            generation: 0,
            last: None,
            stopped: false,
        }
    }

    fn begin_native_scale_query(&mut self, window: window::Id) -> Option<NativeScaleRequest> {
        if self.stopped
            || self.target != Some(window)
            || self.opened != Some(window)
            || self.native_scale.is_some()
            || self.native_scale_query.is_some()
        {
            return None;
        }
        let request = NativeScaleRequest {
            observer_instance: self.instance,
            window,
            opened_generation: self.opened_generation,
            request: uuid::Uuid::new_v4(),
            native_scale_generation: self.native_scale_generation,
        };
        self.native_scale_query = Some(request);
        self.native_scale_query_pending = true;
        Some(request)
    }

    fn finish_native_scale_query(&mut self, request: NativeScaleRequest, scale: f32) -> bool {
        if self.stopped
            || !self.native_scale_query_pending
            || self.native_scale_query != Some(request)
            || self.instance != request.observer_instance
            || self.target != Some(request.window)
            || self.opened != Some(request.window)
            || self.opened_generation != request.opened_generation
            || self.native_scale_generation != request.native_scale_generation
            || self.native_scale.is_some()
        {
            return false;
        }
        // Consume even an invalid actual reply. No retry guesses a scale or
        // treats a duplicate completion as a fresh native observation.
        self.native_scale_query_pending = false;
        if !scale.is_finite() || scale <= 0.0 {
            return false;
        }
        self.native_scale = Some(scale);
        self.native_scale_source = "window_scale_factor_query";
        self.native_scale_generation = self.native_scale_generation.saturating_add(1);
        true
    }

    fn observe_window(&mut self, event_window: window::Id, event: &iced::Event) -> bool {
        if self.target != Some(event_window) {
            return false;
        }
        match event {
            iced::Event::Window(window::Event::Opened { .. }) => {
                if self.opened != Some(event_window) {
                    self.opened = Some(event_window);
                    self.opened_generation = self.opened_generation.saturating_add(1);
                    self.native_scale_query = None;
                    self.native_scale_query_pending = false;
                }
            }
            iced::Event::Window(window::Event::Rescaled(scale))
                if scale.is_finite() && *scale > 0.0 =>
            {
                if self.native_scale.map(f32::to_bits) != Some(scale.to_bits())
                    || self.native_scale_source != "rescaled_event"
                {
                    self.native_scale = Some(*scale);
                    self.native_scale_source = "rescaled_event";
                    self.native_scale_generation = self.native_scale_generation.saturating_add(1);
                }
                // A later actual Rescaled event supersedes an in-flight query,
                // including one whose reply happens to contain the same factor.
                self.native_scale_query_pending = false;
            }
            iced::Event::Window(window::Event::Closed) => {
                self.opened = None;
                self.native_scale = None;
                self.native_scale_source = "unknown";
                self.native_scale_query_pending = false;
            }
            _ => {}
        }
        true
    }
}

#[derive(Clone, PartialEq, Eq, serde::Serialize)]
struct HistoryObservation {
    source_state: &'static str,
    head_oid: Option<String>,
    rows: usize,
    next_page: u32,
    loading: bool,
    selected_commit: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
struct AppearanceKey {
    window: window::Id,
    opened_generation: u64,
    theme_kind: &'static str,
    dark: bool,
    high_contrast: bool,
    exact_dark_class: bool,
    ui_scale_bits: Option<u32>,
    native_scale_bits: Option<u32>,
    native_scale_source: &'static str,
    native_scale_generation: u64,
    native_scale_query_request: Option<uuid::Uuid>,
    workspace_generation: u64,
    source_generation: u64,
    inspector_open: bool,
    history: Option<HistoryObservation>,
}

/// Observe the original iced event before the existing fixture mapping drops
/// its window ID. An allocated Core ID is only a target, never opened evidence.
fn observe_native_window(event_window: window::Id, event: &iced::Event) -> bool {
    let Some(context) = context().expect("validated History fixture context") else {
        return false;
    };
    let mut observer = context
        .appearance
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    observer.observe_window(event_window, event)
}

/// Read the same applied state used by Shell::theme/scale_factor, from a real
/// main-window view. No configured preference or initialized scale is promoted
/// to a native DPI observation. No timer, synthetic Frame or capture selector.
pub(super) fn observe_view(app: &Cagents) {
    let Some(context) = context().expect("validated History fixture context") else {
        return;
    };
    let Some(window) = app.core.main_window_id() else {
        return;
    };
    let theme = cosmic::theme::active();
    let theme_kind = match &theme.theme_type {
        cosmic::theme::ThemeType::Dark => "dark",
        cosmic::theme::ThemeType::Light => "light",
        cosmic::theme::ThemeType::HighContrastDark => "high_contrast_dark",
        cosmic::theme::ThemeType::HighContrastLight => "high_contrast_light",
        cosmic::theme::ThemeType::Custom(_) => "custom",
        cosmic::theme::ThemeType::System { .. } => "system",
    };
    // Shell::scale_factor returns f64::from(Core::scale_factor()). This is
    // applied application scaling; the separately observed OS factor may be None.
    let ui_scale = app.core.scale_factor();
    let ui_scale_bits = (ui_scale.is_finite() && ui_scale > 0.0).then(|| ui_scale.to_bits());
    let history = app
        .sidebar_model
        .source_control
        .as_ref()
        .filter(|source| source.key == context.key())
        .map(|source| HistoryObservation {
            source_state: match &source.data_state {
                SidebarDataState::Loading => "loading",
                SidebarDataState::Ready => "ready",
                SidebarDataState::Error(_) => "error",
            },
            head_oid: source.head_oid.as_deref().map(str::to_owned),
            rows: source.history.len(),
            next_page: source.next_history_page,
            loading: source.history_loading,
            selected_commit: source
                .commit_changes
                .as_ref()
                .map(|changes| changes.commit.oid.as_str().to_owned()),
        });
    let window_id = format!("{window:?}");
    let mut observer = context
        .appearance
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if observer.stopped || observer.target != Some(window) || observer.opened != Some(window) {
        return;
    }
    if crate::VERSION.len() > 256
        || window_id.len() > 64
        || history.as_ref().is_some_and(|history| {
            history.head_oid.as_ref().is_some_and(|oid| oid.len() > 64)
                || history
                    .selected_commit
                    .as_ref()
                    .is_some_and(|oid| oid.len() > 64)
        })
    {
        observer.stopped = true;
        drop(observer);
        tracing::warn!(target: "cagents::native_history", "appearance observer stopped: unbounded field");
        return;
    }
    let key = AppearanceKey {
        window,
        opened_generation: observer.opened_generation,
        theme_kind,
        dark: theme.theme_type.is_dark(),
        high_contrast: theme.theme_type.is_high_contrast(),
        exact_dark_class: matches!(theme.theme_type, cosmic::theme::ThemeType::Dark),
        ui_scale_bits,
        native_scale_bits: observer.native_scale.map(f32::to_bits),
        native_scale_source: observer.native_scale_source,
        native_scale_generation: observer.native_scale_generation,
        native_scale_query_request: observer
            .native_scale_query
            .filter(|_| observer.native_scale_source == "window_scale_factor_query")
            .map(|request| request.request),
        workspace_generation: app.durable.generation,
        source_generation: app.refresh_ui.source_generation,
        inspector_open: app.context_inspector_open,
        history,
    };
    if observer.last.as_ref() == Some(&key) {
        return;
    }
    if observer.generation >= APPEARANCE_RECORD_LIMIT {
        observer.stopped = true;
        drop(observer);
        tracing::warn!(target: "cagents::native_history", "appearance observer stopped: record limit");
        return;
    }
    observer.generation += 1;
    let record = serde_json::json!({
        "record_version": 1,
        "record": "history-native-appearance",
        "observer_instance": observer.instance,
        "observation_generation": observer.generation,
        "pid": std::process::id(),
        "window_id_kind": "iced_main_window_debug",
        "window_id": window_id,
        "window_open_source": "original_window_opened_event",
        "window_open_generation": key.opened_generation,
        "product_version": crate::VERSION,
        "compiled_source_sha256": {
            "src/app/native_history_fixture.rs": &context.source_sha256[0],
            "src/app.rs": &context.source_sha256[1],
            "src/bin/cagents-ui.rs": &context.source_sha256[2],
        },
        "data": { "manifest_sha256": &context.manifest_sha256, "dataset": context.name },
        "theme_kind": key.theme_kind,
        "theme_is_dark": key.dark,
        "theme_is_high_contrast": key.high_contrast,
        "theme_is_exact_dark_class": key.exact_dark_class,
        "applied_ui_scale": key.ui_scale_bits.map(|bits| f64::from(f32::from_bits(bits))),
        "applied_ui_scale_bits": key.ui_scale_bits,
        "applied_ui_scale_source": "Core::scale_factor_as_returned_by_Shell",
        "native_scale": key.native_scale_bits.map(|bits| f64::from(f32::from_bits(bits))),
        "native_scale_bits": key.native_scale_bits,
        "native_scale_source": key.native_scale_source,
        "native_scale_generation": key.native_scale_generation,
        "native_scale_query_request": key.native_scale_query_request,
        "state": {
            "workspace_generation": key.workspace_generation,
            "source_generation": key.source_generation,
            "inspector_open": key.inspector_open,
            "history": &key.history,
        },
        "observation_boundary": "view_state_readback_not_compositor_present",
    });
    let serialized = serde_json::to_string(&record);
    let Ok(serialized) = serialized else {
        observer.stopped = true;
        drop(observer);
        tracing::warn!(target: "cagents::native_history", "appearance observer stopped: serialization");
        return;
    };
    if serialized.len() > APPEARANCE_RECORD_BYTES {
        observer.stopped = true;
        drop(observer);
        tracing::warn!(target: "cagents::native_history", "appearance observer stopped: record byte limit");
        return;
    }
    observer.last = Some(key);
    drop(observer);
    // Actual recovered original UI log bytes are still required: tracing may
    // be filtered or its sink unavailable. This call does not certify delivery.
    tracing::info!(target: "cagents::native_history", appearance_json = %serialized,
        "history native appearance state");
}

#[cfg(test)]
mod native_scale_tests {
    use super::*;

    fn opened_observer(window: window::Id) -> AppearanceObserver {
        let mut observer = AppearanceObserver::new();
        observer.target = Some(window);
        assert!(observer.observe_window(
            window,
            &iced::Event::Window(window::Event::Opened {
                position: None,
                size: iced::Size::new(1440.0, 900.0),
            }),
        ));
        observer
    }

    #[test]
    fn initial_native_scale_query_requires_opened_and_accepts_one_valid_reply() {
        let window = window::Id::unique();
        let foreign_window = window::Id::unique();
        let mut observer = AppearanceObserver::new();
        observer.target = Some(window);
        assert!(observer.begin_native_scale_query(window).is_none());
        assert!(!observer.observe_window(
            foreign_window,
            &iced::Event::Window(window::Event::Opened {
                position: None,
                size: iced::Size::new(1440.0, 900.0),
            }),
        ));
        assert!(observer.begin_native_scale_query(window).is_none());
        assert!(observer.native_scale.is_none());

        let mut observer = opened_observer(window);
        let request = observer.begin_native_scale_query(window).unwrap();
        assert_eq!(request.observer_instance, observer.instance);
        assert_eq!(request.window, window);
        assert_eq!(request.opened_generation, observer.opened_generation);
        assert!(observer.begin_native_scale_query(window).is_none());
        assert!(observer.native_scale.is_none());
        assert!(observer.finish_native_scale_query(request, 1.25));
        assert_eq!(observer.native_scale, Some(1.25));
        assert_eq!(observer.native_scale_source, "window_scale_factor_query");
        assert_eq!(observer.native_scale_generation, 1);
        assert!(!observer.finish_native_scale_query(request, 2.0));
        assert_eq!(observer.native_scale, Some(1.25));
        assert!(observer.begin_native_scale_query(window).is_none());

        for invalid in [f32::NAN, f32::INFINITY, 0.0, -1.0] {
            let mut observer = opened_observer(window);
            let request = observer.begin_native_scale_query(window).unwrap();
            assert!(!observer.finish_native_scale_query(request, invalid));
            assert!(observer.native_scale.is_none());
            assert_eq!(observer.native_scale_source, "unknown");
            assert_eq!(observer.native_scale_generation, 0);
            assert!(!observer.finish_native_scale_query(request, 1.25));
            assert!(observer.begin_native_scale_query(window).is_none());
        }
    }

    #[test]
    fn initial_native_scale_query_refuses_foreign_closed_and_superseded_sources() {
        let window = window::Id::unique();
        let mut observer = opened_observer(window);
        let request = observer.begin_native_scale_query(window).unwrap();
        let mut foreign = request;
        foreign.observer_instance = uuid::Uuid::new_v4();
        assert!(!observer.finish_native_scale_query(foreign, 9.0));
        foreign = request;
        foreign.window = window::Id::unique();
        assert!(!observer.finish_native_scale_query(foreign, 9.0));
        foreign = request;
        foreign.opened_generation += 1;
        assert!(!observer.finish_native_scale_query(foreign, 9.0));
        foreign = request;
        foreign.request = uuid::Uuid::new_v4();
        assert!(!observer.finish_native_scale_query(foreign, 9.0));
        foreign = request;
        foreign.native_scale_generation += 1;
        assert!(!observer.finish_native_scale_query(foreign, 9.0));
        assert!(observer.native_scale.is_none());
        assert!(observer.native_scale_query_pending);

        assert!(
            observer.observe_window(window, &iced::Event::Window(window::Event::Rescaled(2.0)),)
        );
        assert!(!observer.finish_native_scale_query(request, 1.25));
        assert_eq!(observer.native_scale, Some(2.0));
        assert_eq!(observer.native_scale_source, "rescaled_event");
        assert!(observer.begin_native_scale_query(window).is_none());

        let mut observer = opened_observer(window);
        let request = observer.begin_native_scale_query(window).unwrap();
        assert!(observer.finish_native_scale_query(request, 1.25));
        let query_generation = observer.native_scale_generation;
        assert!(
            observer.observe_window(window, &iced::Event::Window(window::Event::Rescaled(1.25)),)
        );
        assert_eq!(observer.native_scale, Some(1.25));
        assert_eq!(observer.native_scale_source, "rescaled_event");
        assert_eq!(observer.native_scale_generation, query_generation + 1);

        let mut observer = opened_observer(window);
        let request = observer.begin_native_scale_query(window).unwrap();
        assert!(observer.observe_window(window, &iced::Event::Window(window::Event::Closed)));
        assert!(!observer.finish_native_scale_query(request, 1.25));
        assert!(observer.opened.is_none());
        assert!(observer.native_scale.is_none());
        assert!(observer.begin_native_scale_query(window).is_none());
        assert!(observer.observe_window(
            window,
            &iced::Event::Window(window::Event::Opened {
                position: None,
                size: iced::Size::new(1440.0, 900.0),
            }),
        ));
        let reopened_request = observer.begin_native_scale_query(window).unwrap();
        assert_ne!(reopened_request.request, request.request);
        assert!(reopened_request.opened_generation > request.opened_generation);
        assert!(!observer.finish_native_scale_query(request, 9.0));
        assert!(observer.finish_native_scale_query(reopened_request, 1.5));

        let mut replacement = opened_observer(window);
        let replacement_request = replacement.begin_native_scale_query(window).unwrap();
        assert!(!replacement.finish_native_scale_query(request, 9.0));
        replacement.stopped = true;
        assert!(!replacement.finish_native_scale_query(replacement_request, 1.5));
        assert!(replacement.native_scale.is_none());
        // A log limit must not swallow the unchanged native close route.
        assert!(replacement.observe_window(window, &iced::Event::Window(window::Event::Closed)));
    }
}
