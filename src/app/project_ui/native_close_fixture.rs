//! Fixed development-only editor scenes. The real cleanup worker and IPC finish
//! BEFORE its original App callback is held. This is not an in-census pause.
use super::*;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};

const SELECTOR: &str = "CAGENTS_DEVELOPMENT_PROJECT_CLOSE_FIXTURE";
const BASELINE: &str = "Private peer-close fixture.\n";
const EXPECTS_EDIT_RETAINED: bool = false;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scene {
    Edit,
    Unchanged,
    Reopen,
}

fn scene(value: &str) -> Result<Scene, String> {
    match value {
        "edit" => Ok(Scene::Edit),
        "unchanged" => Ok(Scene::Unchanged),
        "reopen" => Ok(Scene::Reopen),
        _ => Err("unknown fixed project-close fixture scene".into()),
    }
}

#[derive(Debug)]
pub(in crate::app) struct Context {
    root: PathBuf,
    scene: Scene,
}

impl Context {
    fn project(&self) -> PathBuf {
        self.root.join("project")
    }
}

fn private_directory(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o700
        || path.canonicalize().map_err(|e| e.to_string())? != path
    {
        return Err("fixture directories must be canonical, owned and mode0700".into());
    }
    Ok(())
}

/// Reads only the explicit selector and six prospective private directory names.
/// No caller capability is read, minted, cleared, forwarded or substituted.
pub(in crate::app) fn context() -> Result<Option<Context>, String> {
    let Some(value) = std::env::var_os(SELECTOR) else {
        return Ok(None);
    };
    let value = value
        .into_string()
        .map_err(|_| "invalid fixture selector")?;
    let selected = scene(&value)?;
    for other in [
        "CAGENTS_LATENCY_OUTPUT",
        "CAGENTS_DEVELOPMENT_ACCOUNT_FIXTURE",
        "CAGENTS_DEVELOPMENT_AUTOMATION_FIXTURE",
    ] {
        if std::env::var_os(other).is_some() {
            return Err("project-close fixture cannot share another fixture".into());
        }
    }
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("missing private fixture HOME")?);
    let root = home.parent().ok_or("missing fixture root")?.to_path_buf();
    if root.parent() != Some(Path::new("/tmp"))
        || home != root.join("home")
        || !["wide", "narrow"].into_iter().any(|layout| {
            ["before", "after"].into_iter().any(|side| {
                root.file_name()
                    == Some(std::ffi::OsStr::new(&format!(
                        "cagents-1937-native-{value}-{layout}-{side}"
                    )))
            })
        })
    {
        return Err("fixture requires its fixed /tmp/cagents-1937-native-<edit|unchanged|reopen>-<wide|narrow>-<before|after>/home".into());
    }
    private_directory(&root)?;
    for (name, leaf) in [
        ("HOME", "home"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "runtime"),
    ] {
        let actual = PathBuf::from(std::env::var_os(name).ok_or("missing private fixture root")?);
        if actual != root.join(leaf) {
            return Err("fixture HOME/XDG roots do not match the fixed namespace".into());
        }
        private_directory(&actual)?;
    }
    Ok(Some(Context {
        root,
        scene: selected,
    }))
}

fn absent(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
        Ok(_) => Err("fixture refuses a consumed namespace or persisted application state".into()),
    }
}

fn validate_contents(context: &Context) -> Result<(), String> {
    absent(&context.root.join("project-close-started"))?;
    absent(&context.project())?;
    // Build tools may have populated their private context before UI entry.
    // The supplier must exclusively create a NEW root before starting children;
    // this metadata check cannot reconstruct or prove that original creation.
    for leaf in ["data", "config", "cache", "state"] {
        for application in ["cagents-development", "cagents"] {
            absent(&context.root.join(leaf).join(application))?;
        }
    }
    for provider in [".claude", ".codex", ".omp"] {
        absent(&context.root.join("home").join(provider))?;
    }
    let mut socket_seen = false;
    for entry in std::fs::read_dir(context.root.join("runtime")).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let metadata = std::fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
        if entry.file_name() != "nested-wayland"
            || !metadata.file_type().is_socket()
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err("fixture runtime permits only the declared owned nested-wayland socket before UI entry".into());
        }
        socket_seen = true;
    }
    if !socket_seen {
        return Err("fixture needs the supplier's original nested-wayland socket".into());
    }
    Ok(())
}

/// Called from bin::run BEFORE ordinary device, daemon, lease and store effects.
/// This checks current metadata/absence, not the supplier's original creation.
pub(in crate::app) fn validate_fresh() -> Result<bool, String> {
    let Some(context) = context()? else {
        return Ok(false);
    };
    validate_contents(&context)?;
    Ok(true)
}

/// Only literal private data is seeded. Normal startup/daemon/SessionRuntime
/// initialization is retained; this is not a bootstrap or admission bypass.
pub(in crate::app) fn seed(context: &Context) -> Result<(), String> {
    use std::io::Write;
    let mut marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(context.root.join("project-close-started"))
        .map_err(|e| e.to_string())?;
    marker
        .write_all(b"fixed project-close scene; do not reuse\n")
        .map_err(|e| e.to_string())?;
    marker.sync_all().map_err(|e| e.to_string())?;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(context.project())
        .map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(context.project().join("notes.txt"))
        .map_err(|e| e.to_string())?;
    file.write_all(BASELINE.as_bytes())
        .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Project,
    Files,
    Read,
    Ready,
    Held,
    Delivering,
    Finished,
}

#[derive(Debug)]
pub(in crate::app) struct Fixture {
    context: Context,
    phase: Phase,
    project: Option<ProjectId>,
    card: Option<CardId>,
    original: Option<(ProjectId, u64)>,
    held: Option<Box<Message>>,
    expected_text: Option<String>,
    reopen_seen: bool,
    error: Option<String>,
    project_loaded: bool,
    files_loaded: bool,
    reopened: bool,
    delivering: bool,
}

fn receipt(value: serde_json::Value) {
    println!("PR1937_NATIVE {value}");
}

fn fail(app: &mut Cagents, error: &str) {
    if let Some(f) = app.project_ui.native_fixture.as_mut() {
        f.error.get_or_insert_with(|| error.into());
    }
    receipt(serde_json::json!({"event":"fixture_failed","error":error,"qualified":false}));
    app.notices.record(TransientNotice::error(format!(
        "INERT project-close fixture: {error}"
    )));
}

pub(in crate::app) fn install(app: &mut Cagents, context: Context) -> Task<Message> {
    let project = context.project();
    app.project_ui.native_fixture = Some(Fixture {
        context,
        phase: Phase::Project,
        project: None,
        card: None,
        original: None,
        held: None,
        expected_text: None,
        reopen_seen: false,
        error: None,
        project_loaded: false,
        files_loaded: false,
        reopened: false,
        delivering: false,
    });
    if crate::device::is_remote()
        || app.session.is_none()
        || !app.cards.is_empty()
        || !app.durable.cards.is_empty()
        || !app.durable.channels.is_empty()
        || !app.sidebar_model.projects.is_empty()
        || !app.workspace_model.projects.is_empty()
    {
        fail(app, "fixed scene requires its fresh local private window");
        return app.sync_chrome();
    }
    app.settings.editor_auto_save = false;
    app.settings.app_theme = AppTheme::Dark;
    app.settings.language_servers.clear();
    app.lsp = LspManager::new(Vec::new());
    app.context_inspector_open = false;
    app.notices.record(TransientNotice::warning("INERT editor scene · peer closes this private project; F8 delivers the original callback AFTER cleanup has finished · no active-census claim"));
    receipt(serde_json::json!({"event":"installed","project":project,
        "mode":"ORIGINAL_COMPLETION_DELAY","provider_started":false,"native_input_proven":false}));
    // Normal folder discovery/publication, without select_worktree's default
    // terminal creation. The normal repository factory below starts its reads.
    let load = app.repository_load_task(project, None, false);
    let theme = app.settings.app_theme.resolve(app.core.system_theme());
    Task::batch([load, app.sync_chrome(), cosmic::command::set_theme(theme)])
}

fn editor(app: &Cagents) -> Option<&RepositoryRuntime> {
    let card = app.project_ui.native_fixture.as_ref()?.card?;
    match app.cards.get(&card)? {
        CardRuntime::Repository(runtime) => Some(runtime),
        _ => None,
    }
}

fn observe(
    app: &mut Cagents,
    project_loaded: bool,
    files_loaded: bool,
    reopened: bool,
) -> Task<Message> {
    let f = app.project_ui.native_fixture.as_ref().unwrap();
    let root = f.context.project();
    let phase = f.phase;
    if f.error.is_some() {
        return Task::none();
    }
    if phase == Phase::Project {
        // Discovery installs a provisional sidebar row before its worker
        // finishes. Never mount against that placeholder or a random frame.
        if !project_loaded {
            return Task::none();
        }
        let Some(row) = app
            .sidebar_model
            .projects
            .iter()
            .find(|p| p.repository == root)
        else {
            return Task::none();
        };
        if row.kind != ProjectKind::Folder || !app.cards.is_empty() {
            fail(
                app,
                "fixed folder discovery acquired unexpected cards or repository type",
            );
            return Task::none();
        }
        let project = row.id;
        app.active_workspace = Some(root.clone());
        let task = app.add_repository_card(project, root.clone(), root);
        let card = app
            .durable
            .focused
            .expect("normal repository factory focused its card");
        let f = app.project_ui.native_fixture.as_mut().unwrap();
        f.project = Some(project);
        f.card = Some(card);
        f.phase = Phase::Files;
        return task;
    }
    if phase == Phase::Files && files_loaded {
        let card = f.card.unwrap();
        app.project_ui.native_fixture.as_mut().unwrap().phase = Phase::Read;
        return cosmic::task::message(Message::Repository(RepositoryAction::OpenFile {
            card,
            path: PathBuf::from("notes.txt"),
        }));
    }
    if phase == Phase::Read
        && editor(app)
            .and_then(RepositoryRuntime::editor_text)
            .as_deref()
            == Some(BASELINE)
        && app.workspace_model.project(&root).is_some()
    {
        let f = app.project_ui.native_fixture.as_mut().unwrap();
        f.phase = Phase::Ready;
        receipt(
            serde_json::json!({"event":"ready_for_peer_close","project":root,
            "card":f.card,"editor_loaded_through_normal_tasks":true,"widget_focus_proven":false}),
        );
    }
    let f = app.project_ui.native_fixture.as_mut().unwrap();
    if f.phase == Phase::Ready && f.original.is_none() {
        if let Some(project) = f.project {
            if let Some(generation) = app.project_ui.close_generation.get(&project) {
                f.original = Some((project, *generation));
                receipt(
                    serde_json::json!({"event":"original_close_admitted","project":project,
                    "generation":generation,"task_intercepted":false}),
                );
            }
        }
    }
    if f.phase == Phase::Held
        && reopened
        && f.original
            .is_some_and(|(project, _)| !app.project_ui.close_generation.contains_key(&project))
        && app.workspace_model.project(&root).is_some()
    {
        f.reopen_seen = true;
        receipt(serde_json::json!({"event":"actual_peer_reopen_observed","original":f.original}));
    }
    Task::none()
}

/// Move the exact original global App message into custody. The normal cleanup
/// creator emits this unwrapped variant. Utility-pane wrappers pass unchanged
/// to App's own pane-origin guard; they are never normalized into this slot.
/// No result/proof is built, cloned or replaced, and close state is unchanged.
fn capture(app: &mut Cagents, message: Message) -> Result<Message, ()> {
    let Message::ProjectUi(Action::ProjectSessionsClosed(project, generation, result)) = &message
    else {
        return Ok(message);
    };
    let f = app.project_ui.native_fixture.as_ref().unwrap();
    if f.phase != Phase::Ready || f.original != Some((*project, *generation)) || f.held.is_some() {
        return Ok(message);
    }
    if result.is_err() {
        fail(
            app,
            "original cleanup failed; successful callback scene unavailable",
        );
        return Ok(message);
    }
    let original = (*project, *generation);
    let f = app.project_ui.native_fixture.as_mut().unwrap();
    f.held = Some(Box::new(message));
    f.phase = Phase::Held;
    receipt(
        serde_json::json!({"event":"original_callback_held","original":original,
        "actual_result_ok":true,"worker_and_ipc_already_completed":true,
        "active_daemon_census_paused":false,"native_input_proven":false}),
    );
    Err(())
}

fn release(app: &mut Cagents) -> Task<Message> {
    let f = app.project_ui.native_fixture.as_ref().unwrap();
    if f.phase != Phase::Held || f.held.is_none() {
        return Task::none();
    }
    let current = editor(app);
    let acceptable = match f.context.scene {
        Scene::Edit => current.is_some_and(|e| {
            e.is_dirty() && !e.is_saving() && e.editor_text().is_some_and(|text| text != BASELINE)
        }),
        Scene::Unchanged => current.is_some_and(|e| {
            !e.blocks_navigation() && e.editor_text().as_deref() == Some(BASELINE)
        }),
        Scene::Reopen => {
            f.reopen_seen
                && current.is_some_and(|e| {
                    !e.blocks_navigation() && e.editor_text().as_deref() == Some(BASELINE)
                })
        }
    };
    if !acceptable && f.error.is_none() {
        receipt(
            serde_json::json!({"event":"release_refused","reason":"fixed native scene prerequisite not observed"}),
        );
        return Task::none();
    }
    let text = current.and_then(RepositoryRuntime::editor_text);
    let f = app.project_ui.native_fixture.as_mut().unwrap();
    f.expected_text = text;
    let original = *f.held.take().unwrap();
    f.phase = Phase::Delivering;
    receipt(
        serde_json::json!({"event":"original_callback_released","original":f.original,
        "same_original_message_moved":true,"delivery_count":1}),
    );
    // Normal runtime delivery, never a direct handler/reducer call.
    cosmic::task::message(original)
}

fn completed(app: &mut Cagents) {
    let f = app.project_ui.native_fixture.as_ref().unwrap();
    let project = f.project.unwrap();
    let card = f.card.unwrap();
    let present = app.cards.contains_key(&card) && app.sidebar_model.project(project).is_some();
    let retained = match f.context.scene {
        Scene::Edit => EXPECTS_EDIT_RETAINED,
        Scene::Unchanged => false,
        Scene::Reopen => true,
    };
    let text_preserved =
        !retained || editor(app).and_then(RepositoryRuntime::editor_text) == f.expected_text;
    if present != retained || !text_preserved {
        fail(
            app,
            "original callback outcome disagreed with the source side or lost current text",
        );
    }
    let f = app.project_ui.native_fixture.as_mut().unwrap();
    f.phase = Phase::Finished;
    receipt(
        serde_json::json!({"event":"original_callback_consumed","original":f.original,
        "row_and_editor_present":present,"expected_retained":retained,"current_text_preserved":text_preserved,
        "fixture_error":f.error,"native_input_or_ux_qualified":false}),
    );
}

fn allowed(app: &Cagents, message: &Message) -> bool {
    let f = app.project_ui.native_fixture.as_ref().unwrap();
    match inner(message) {
        Message::RepositoryLoaded { .. }
        | Message::WorkspaceModelObserved(_)
        | Message::ProjectUi(_)
        | Message::SessionSaveHealthChanged(_)
        | Message::StartupWindowOpened(_)
        | Message::StartupReported(_)
        | Message::VerifyStartupPanes
        | Message::StartupPanesSaved(..)
        | Message::Frame(_)
        | Message::Viewport { .. }
        | Message::WindowResized(..)
        | Message::WindowScaleFactorChanged(_)
        | Message::WindowEdgeState { .. }
        | Message::WindowFilled(..)
        | Message::WindowCloseRequested(_)
        | Message::WindowClosed(_)
        | Message::DismissNotice(_)
        | Message::DismissStatus
        | Message::DismissAllNotices => true,
        Message::Repository(action) => {
            f.card == Some(action.card())
                && matches!(
                    action,
                    RepositoryAction::FilesLoaded { .. }
                        | RepositoryAction::FileListUnchanged { .. }
                        | RepositoryAction::FileLoadedAt { .. }
                        | RepositoryAction::FileLoaded { .. }
                        | RepositoryAction::DocumentChanged { .. }
                        | RepositoryAction::DocumentReloaded { .. }
                        | RepositoryAction::OpenFile { .. }
                        | RepositoryAction::EditorAction { .. }
                        | RepositoryAction::Save { .. }
                        | RepositoryAction::RetainedSaveFinished { .. }
                        | RepositoryAction::SaveFinished { .. }
                        | RepositoryAction::Reload { .. }
                        | RepositoryAction::DiscardEdits { .. }
                        | RepositoryAction::Undo { .. }
                        | RepositoryAction::Redo { .. }
                        | RepositoryAction::Clipboard { .. }
                )
        }
        Message::FocusCard(card) => f.card == Some(*card),
        Message::ChromeUi(chrome_ui::Action::ToggleNotices | chrome_ui::Action::Close) => true,
        Message::ChromeUi(chrome_ui::Action::ShowNoticesFor(card)) => f.card == Some(*card),
        Message::KeyEvent {
            event: keyboard::Event::ModifiersChanged(_),
            ..
        } => true,
        Message::KeyEvent { event, .. } if is_save_press(event) => true,
        Message::KeyEvent {
            event: keyboard::Event::KeyPressed { key, modifiers, .. },
            ..
        } => {
            matches!(
                key,
                keyboard::Key::Named(keyboard::key::Named::Tab | keyboard::key::Named::Escape)
            ) && (modifiers.is_empty() || *modifiers == keyboard::Modifiers::SHIFT)
        }
        _ => false,
    }
}

fn inner(message: &Message) -> &Message {
    match message {
        Message::UtilityPaneUi(inner) => inner,
        other => other,
    }
}

/// Interception happens before the ordinary dispatcher, but never returns out of
/// App::update: a development-only no-op still traverses its entire postlude.
pub(in crate::app) fn before_update(
    app: &mut Cagents,
    message: Message,
) -> (Message, Task<Message>) {
    if app.project_ui.native_fixture.is_none() {
        return (message, Task::none());
    }
    let f = app.project_ui.native_fixture.as_mut().unwrap();
    f.project_loaded = false;
    f.files_loaded = false;
    f.reopened = false;
    f.delivering = false;
    if matches!(&message, Message::KeyEvent { captured: false, event: keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(keyboard::key::Named::F8), modifiers, ..
    }} if modifiers.is_empty())
    {
        return (Message::ProjectCloseFixtureCheckpoint, release(app));
    }
    let f = app.project_ui.native_fixture.as_ref().unwrap();
    if matches!(&message, Message::WindowCloseRequested(_))
        && (f.held.is_some()
            || (f.original.is_some() && f.error.is_none() && f.phase != Phase::Finished))
    {
        app.notices.record(TransientNotice::warning(
            "Finish the original fixture callback with F8 before closing this private window.",
        ));
        return (Message::ProjectCloseFixtureCheckpoint, Task::none());
    }
    let message = match capture(app, message) {
        Ok(message) => message,
        Err(()) => return (Message::ProjectCloseFixtureCheckpoint, Task::none()),
    };
    if !allowed(app, &message) {
        return (Message::ProjectCloseFixtureCheckpoint, Task::none());
    }
    let f = app.project_ui.native_fixture.as_ref().unwrap();
    let view = inner(&message);
    let project_loaded = matches!(view, Message::RepositoryLoaded { result: Ok(loaded), .. }
        if loaded.project.repository == f.context.project());
    let files_loaded = matches!(view, Message::Repository(RepositoryAction::FilesLoaded { identity, .. })
        if f.card == Some(identity.card));
    let reopened = matches!(view, Message::WorkspaceModelObserved(workspace_model_sync::ModelUpdate::Edited {
        edit: crate::workspace_model::ModelEdit::ReopenProject { repository, .. }, ..
    }) if repository == &f.context.project());
    let delivering = f.phase == Phase::Delivering
        && matches!(view,
        Message::ProjectUi(Action::ProjectSessionsClosed(project, generation, _))
            if f.original == Some((*project, *generation)));
    let f = app.project_ui.native_fixture.as_mut().unwrap();
    f.project_loaded = project_loaded;
    f.files_loaded = files_loaded;
    f.reopened = reopened;
    f.delivering = delivering;
    (message, Task::none())
}

pub(in crate::app) fn after_update(app: &mut Cagents) -> Task<Message> {
    let Some(f) = app.project_ui.native_fixture.as_mut() else {
        return Task::none();
    };
    let delivering = std::mem::take(&mut f.delivering);
    let project_loaded = std::mem::take(&mut f.project_loaded);
    let files_loaded = std::mem::take(&mut f.files_loaded);
    let reopened = std::mem::take(&mut f.reopened);
    if delivering {
        completed(app);
    }
    observe(app, project_loaded, files_loaded, reopened)
}

pub(in crate::app) fn subscription(app: &Cagents) -> Subscription<Message> {
    let mut subscriptions = vec![
        Subscription::run_with(
            WorkspaceModelSource(app.workspace_model_rx.clone()),
            workspace_model_stream,
        )
        .map(Message::WorkspaceModelObserved),
        event::listen_with(|event, status, id| match event {
            iced::Event::Keyboard(event) => Some(Message::KeyEvent {
                event,
                captured: matches!(status, event::Status::Captured),
            }),
            iced::Event::Window(window::Event::CloseRequested) => {
                Some(Message::WindowCloseRequested(id))
            }
            iced::Event::Window(window::Event::Closed) => Some(Message::WindowClosed(id)),
            iced::Event::Window(window::Event::Rescaled(scale)) => {
                Some(Message::WindowScaleFactorChanged(scale))
            }
            iced::Event::Window(event) => window_geometry_message(&event),
            _ => None,
        }),
    ];
    if let Some(session) = &app.session {
        subscriptions.push(
            Subscription::run_with(
                SessionHealthSource(session.health_receiver()),
                session_health_stream,
            )
            .map(Message::SessionSaveHealthChanged),
        );
    }
    if let Some(window) = app.core.main_window_id()
        && app.workspace.is_animating()
    {
        subscriptions.push(workspace_widget::animation_frames(window, Message::Frame));
    }
    Subscription::batch(subscriptions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;

    fn gate_app(project: ProjectId) -> Cagents {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        // In-memory gate inputs only. These are not daemon/caller/proof or
        // successful production cleanup fixtures, and perform no actor work.
        app.project_ui.native_fixture = Some(Fixture {
            context: Context {
                root: PathBuf::from("/unused-gate-unit-data"),
                scene: Scene::Edit,
            },
            phase: Phase::Ready,
            project: Some(project),
            card: None,
            original: Some((project, 7)),
            held: None,
            expected_text: None,
            reopen_seen: false,
            error: None,
            project_loaded: false,
            files_loaded: false,
            reopened: false,
            delivering: false,
        });
        app
    }

    #[test]
    fn fixed_scene_names_reject_other_cases_and_paths() {
        assert_eq!(scene("edit").unwrap(), Scene::Edit);
        assert_eq!(scene("unchanged").unwrap(), Scene::Unchanged);
        assert_eq!(scene("reopen").unwrap(), Scene::Reopen);
        for invalid in [
            "",
            "save",
            "recovery",
            "edit/../reopen",
            "/tmp/edit",
            "Edit",
        ] {
            assert!(scene(invalid).is_err());
        }
    }

    #[test]
    fn preflight_allows_build_context_and_the_declared_socket_but_not_app_state() {
        use std::os::unix::net::UnixListener;
        let directory = tempfile::tempdir().unwrap();
        let context = Context {
            root: directory.path().to_owned(),
            scene: Scene::Edit,
        };
        for leaf in ["home", "data", "config", "cache", "state", "runtime"] {
            std::fs::create_dir(context.root.join(leaf)).unwrap();
        }
        std::fs::create_dir_all(context.root.join("home/.cargo")).unwrap();
        std::fs::write(
            context.root.join("home/.cargo/config.toml"),
            "# owned build context\n",
        )
        .unwrap();
        std::fs::create_dir(context.root.join("cache/build-context")).unwrap();
        assert!(validate_contents(&context).is_err());
        let _socket = UnixListener::bind(context.root.join("runtime/nested-wayland")).unwrap();
        validate_contents(&context).unwrap();
        let stale = context.root.join("data/cagents-development");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("sessions.sqlite3"), "inert stale state").unwrap();
        assert!(validate_contents(&context).is_err());
        assert_eq!(
            std::fs::read_to_string(stale.join("sessions.sqlite3")).unwrap(),
            "inert stale state"
        );
    }

    #[test]
    fn wrapped_callback_preserves_the_original_pane_origin_and_allocation() {
        let project = ProjectId::new();
        let mut app = gate_app(project);
        let wrapped = Box::new(Message::ProjectUi(Action::ProjectSessionsClosed(
            project,
            7,
            Err("pane-origin control, not an original cleanup".into()),
        )));
        let pointer = wrapped.as_ref() as *const Message;
        let (returned, task) = before_update(&mut app, Message::UtilityPaneUi(wrapped));
        drop(task);
        assert!(matches!(returned, Message::UtilityPaneUi(original)
            if std::ptr::eq(original.as_ref(), pointer)));
        let f = app.project_ui.native_fixture.as_ref().unwrap();
        assert!(f.held.is_none());
        assert!(f.error.is_none());
    }

    #[test]
    fn seed_preserves_existing_files_and_consumed_namespaces() {
        let directory = tempfile::tempdir().unwrap();
        let context = Context {
            root: directory.path().to_owned(),
            scene: Scene::Edit,
        };
        seed(&context).unwrap();
        let notes = context.project().join("notes.txt");
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), BASELINE);
        std::fs::write(&notes, "existing fixture edit").unwrap();
        assert!(seed(&context).is_err());
        assert_eq!(
            std::fs::read_to_string(&notes).unwrap(),
            "existing fixture edit"
        );

        let other = tempfile::tempdir().unwrap();
        let context = Context {
            root: other.path().to_owned(),
            scene: Scene::Edit,
        };
        std::fs::create_dir(context.project()).unwrap();
        std::fs::write(context.project().join("keep.txt"), "keep").unwrap();
        assert!(seed(&context).is_err());
        assert_eq!(
            std::fs::read_to_string(context.project().join("keep.txt")).unwrap(),
            "keep"
        );
        assert!(context.root.join("project-close-started").exists());
    }

    #[test]
    fn callback_gate_leaves_other_generations_projects_and_errors_on_the_normal_path() {
        let project = ProjectId::new();
        let mut app = gate_app(project);
        app.project_ui.native_fixture.as_mut().unwrap().held = Some(Box::new(Message::ProjectUi(
            Action::ProjectSessionsClosed(project, 7, Err("occupied".into())),
        )));
        assert!(
            capture(
                &mut app,
                Message::ProjectUi(Action::ProjectSessionsClosed(
                    project,
                    7,
                    Err("second callback".into()),
                ))
            )
            .is_ok()
        );
        let f = app.project_ui.native_fixture.as_mut().unwrap();
        assert!(
            matches!(f.held.as_deref(), Some(Message::ProjectUi(Action::ProjectSessionsClosed(
            _, 7, Err(error)))) if error == "occupied")
        );
        f.held.take();
        for (id, generation) in [(ProjectId::new(), 7), (project, 6), (project, 8)] {
            let message = Message::ProjectUi(Action::ProjectSessionsClosed(
                id,
                generation,
                Err("original negative payload".into()),
            ));
            let returned = capture(&mut app, message).unwrap();
            assert!(
                matches!(returned, Message::ProjectUi(Action::ProjectSessionsClosed(
                got, number, Err(error))) if got == id && number == generation && error == "original negative payload")
            );
            assert!(
                app.project_ui
                    .native_fixture
                    .as_ref()
                    .unwrap()
                    .held
                    .is_none()
            );
        }
        assert!(
            capture(
                &mut app,
                Message::ProjectUi(Action::ProjectSessionsClosed(
                    project,
                    7,
                    Err("real cleanup was unavailable".into()),
                ))
            )
            .is_ok()
        );
        let f = app.project_ui.native_fixture.as_ref().unwrap();
        assert!(f.held.is_none());
        assert!(f.error.is_some());
    }

    #[tokio::test]
    async fn release_moves_the_original_payload_once_and_does_not_recapture_it() {
        let project = ProjectId::new();
        let mut app = gate_app(project);
        // This negative typed plumbing payload deliberately has no closure or
        // host proof. It is not admitted by capture as a native successful case.
        let payload = String::from("original allocation retained");
        let pointer = payload.as_ptr();
        let f = app.project_ui.native_fixture.as_mut().unwrap();
        f.phase = Phase::Held;
        f.error = Some("gate-plumbing-only control".into());
        f.held = Some(Box::new(Message::ProjectUi(Action::ProjectSessionsClosed(
            project,
            7,
            Err(payload),
        ))));
        let task = release(&mut app);
        let f = app.project_ui.native_fixture.as_ref().unwrap();
        assert!(f.held.is_none());
        assert_eq!(f.phase, Phase::Delivering);
        assert!(iced::runtime::task::into_stream(release(&mut app)).is_none());
        let mut outputs = iced::runtime::task::into_stream(task).unwrap();
        let original = loop {
            let next = outputs
                .next()
                .await
                .expect("original task must emit its payload");
            if let iced::runtime::Action::Output(cosmic::Action::App(message)) = next {
                break message;
            }
        };
        let returned = capture(&mut app, original).unwrap();
        assert!(
            matches!(returned, Message::ProjectUi(Action::ProjectSessionsClosed(
            got, 7, Err(error))) if got == project && error.as_ptr() == pointer && error == "original allocation retained")
        );
        assert!(
            app.project_ui
                .native_fixture
                .as_ref()
                .unwrap()
                .held
                .is_none()
        );
    }
}
