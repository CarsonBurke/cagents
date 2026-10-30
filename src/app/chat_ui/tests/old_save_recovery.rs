// Evidence for the old 582a054bc save-refusal route, not a production repair.
// Every parent runs only itself in a private exact-ELF child. No returned UI,
// Skills, probe, first-send or provider task is polled, except the update's
// settings-save / real session-flush task below. The original constructor's
// persistable environment projection is deliberately unchanged and never logged.
use super::*;
use cosmic::iced::futures::StreamExt;
use std::collections::BTreeMap;
use std::os::unix::fs::DirBuilderExt;

const CHILD_ROOT: &str = "CAGENTS_TEST_1995_OLD_RECOVERY_ROOT";
const WAIT: Duration = Duration::from_secs(10);

fn isolated_parent(name: &str) -> Option<PathBuf> {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        assert!(root.is_absolute(), "fixture root must be absolute");
        return Some(root);
    }
    let root = tempfile::tempdir().expect("private recovery fixture root");
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        &format!("{}::{name}", module_path!()),
        "--nocapture",
    ]);
    command.env(CHILD_ROOT, root.path());
    for (name, leaf) in [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "runtime"),
    ] {
        let path = root.path().join(leaf);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        command.env(name, path);
    }
    // Inherit the natural caller context. Never clear/replace authority or
    // provider variables, choose a profile, or switch the device route.
    let output = command.output().expect("own exact recovery test child");
    assert!(
        output.status.success(),
        "isolated recovery child failed (only categorical assertions are emitted): {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    None
}

#[derive(Clone, PartialEq)]
struct ChatMemory {
    card: CardId,
    session: chat::Session,
    snapshot: Arc<Snapshot>,
    text: String,
    images: Vec<Attachment>,
    history: Vec<String>,
    history_images: Vec<Vec<Attachment>>,
    history_cursor: Option<usize>,
    history_stash: Option<String>,
    image_stash: Option<Vec<Attachment>>,
    expanded: HashSet<String>,
    tool_groups_expanded: bool,
}

#[derive(Clone, PartialEq)]
struct WindowMemory {
    workspace: Arc<WorkspaceState>,
    chats: Vec<ChatMemory>,
    settings: AppSettings,
    view: serde_json::Value,
    pane_order: Vec<CardId>,
    pane_focus: Option<CardId>,
    camera: crate::workspace::Camera,
}

impl WindowMemory {
    fn capture(app: &mut Cagents) -> Self {
        let workspace = app.persistent_workspace();
        let chats = workspace
            .cards
            .iter()
            .filter_map(|card| match app.cards.get(&card.id) {
                Some(CardRuntime::Chat(runtime)) => Some(ChatMemory {
                    card: card.id,
                    session: runtime.session.clone(),
                    snapshot: Arc::clone(&runtime.snapshot),
                    text: runtime.composer.text(),
                    images: runtime.attachments.clone(),
                    history: runtime.history.clone(),
                    history_images: runtime.history_images.clone(),
                    history_cursor: runtime.history_cursor,
                    history_stash: runtime.history_stash.as_ref().map(|text| text.text()),
                    image_stash: runtime.history_attachment_stash.clone(),
                    expanded: runtime.expanded.clone(),
                    tool_groups_expanded: runtime.tool_groups_expanded,
                }),
                _ => None,
            })
            .collect();
        Self {
            workspace,
            chats,
            settings: app.settings.clone(),
            view: serde_json::to_value(app.capture_settings_save().unwrap().view).unwrap(),
            pane_order: app.workspace.order().collect(),
            pane_focus: app.workspace.focused(),
            camera: app.workspace.camera(),
        }
    }
}

struct Fixture {
    app: Cagents,
    database: PathBuf,
    boot: crate::model::BootId,
    card: CardId,
    neighbour: CardId,
    account_a: AccountId,
    account_b: AccountId,
    home_b: PathBuf,
    initial: Arc<WorkspaceState>,
    projects: Vec<crate::model::ProjectState>,
}

fn session(id: TerminalId, directory: &Path, account_home: &Path) -> chat::Session {
    chat::Session {
        id,
        proof: None,
        terminal: None,
        native_continuity: Default::default(),
        owned_codex_listener: None,
        launch: chat::Launch {
            provider: Provider::Claude,
            session_id: None,
            recipe: LaunchRecipe {
                program: "claude".into(),
                arguments: vec!["--permission-mode".into(), "plan".into()],
                working_directory: directory.to_owned(),
                environment: BTreeMap::from([(
                    "CLAUDE_CONFIG_DIR".into(),
                    account_home.to_str().unwrap().into(),
                )]),
                unset_environment: Default::default(),
            },
        },
    }
}

fn card(kind: CardKind, title: &str, width: f32) -> CardState {
    CardState {
        id: CardId::new(),
        generation: 0,
        title: title.into(),
        width_fraction: width,
        existence: Existence::Open,
        exposure: Exposure::Visible,
        residency: Residency::Hot,
        kind,
    }
}

impl Fixture {
    fn new(root: &Path) -> Self {
        assert!(
            !crate::device::is_remote(),
            "fixture must have no remote context"
        );
        let home = root.join("home");
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.context_inspector_open = false;
        app.settings.show_status_bar = false;
        app.settings_path = Some(root.join("settings.toml"));
        app.settings_write_blocked = false;
        let mut registry = account::registry::Registry::default();
        let mut accounts = Vec::new();
        for key in [19951_u128, 19952] {
            let key = uuid::Uuid::from_u128(key);
            let path = home.join(format!("synthetic-account-{key}"));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            std::fs::write(
                path.join("settings.json"),
                br#"{"permissions":{"defaultMode":"manual"}}"#,
            )
            .unwrap();
            registry
                .register(
                    account::registry::Registered {
                        key,
                        provider: Provider::Claude,
                        linked_home: Some(path.clone()),
                        label: Some("Synthetic recovery fixture".into()),
                        email: None,
                        organization: None,
                        added_at: 0,
                    },
                    &[],
                )
                .unwrap();
            accounts.push(Account::linked(Provider::Claude, key, path));
        }
        // A synthetic registry completion changes only State::default's
        // in-memory registry (Store/Cache/Homes are None). Probe Tasks stay inert.
        drop(app.update(Message::AccountUi(
            crate::app::account_ui::Action::RegistryLoaded(Ok(registry)),
        )));
        let project = ProjectId::new();
        let channel = crate::model::ChannelId::new();
        let target = card(
            CardKind::Chat(session(TerminalId::new(), &home, &home.join("old-account"))),
            "Duplicated unstarted chat",
            0.40,
        );
        let neighbour = card(
            CardKind::Chat(session(
                TerminalId::new(),
                &home,
                &home.join("neighbour-account"),
            )),
            "Other unsaved draft",
            0.30,
        );
        let browser = card(
            CardKind::Browser(crate::model::BrowserSession {
                id: crate::model::BrowserId::new(),
                current_url: "https://example.invalid/second".parse().unwrap(),
                history: vec![
                    "https://example.invalid/first".parse().unwrap(),
                    "https://example.invalid/second".parse().unwrap(),
                ],
                history_index: 1,
                scroll_x: 12.0,
                scroll_y: 240.0,
                fidelity: crate::model::BrowserRestoreFidelity::HistoryAndScroll,
            }),
            "Retained browser descriptor",
            0.25,
        );
        let editor = card(
            CardKind::Repository(crate::model::RepositoryView {
                project,
                repository: home.clone(),
                worktree: home.clone(),
                selected_file: Some("notes.txt".into()),
            }),
            "Editor navigation only",
            0.35,
        );
        let (id, neighbour_id, editor_id) = (target.id, neighbour.id, editor.id);
        let order = vec![target.id, browser.id, neighbour.id, editor.id];
        app.durable = Arc::new(WorkspaceState {
            cards: vec![target, browser, neighbour, editor],
            focused: Some(id),
            project_order: vec![project],
            channels: vec![crate::model::ProjectChannel {
                id: channel,
                project_id: project,
                title: "Retained channel".into(),
                title_source: Default::default(),
                ordinal: 7,
                shared: false,
                cards: order,
                focused: Some(id),
            }],
            card_scopes: HashMap::from([(id, home.clone()), (neighbour_id, home.clone())]),
            repository_navigation: HashMap::from([(
                editor_id,
                crate::model::RepositoryNavigation {
                    project,
                    worktree: home.clone(),
                    selected_file: Some("notes.txt".into()),
                    editor: Some(crate::model::EditorNavigation {
                        caret: crate::model::EditorPosition { line: 8, column: 3 },
                        selection: Some(crate::model::EditorPosition { line: 6, column: 1 }),
                        scroll_line: 5,
                        scroll_vertical: 42.5,
                        scroll_horizontal: 9.0,
                    }),
                },
            )]),
            local_only_cards: HashSet::from([id, neighbour_id]),
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(1_000.0);
        app.workspace.native_scroll(61.5, 1_000.0);
        app.chrome_ui
            .user_titles
            .insert(id, "Retained custom pane title".into());
        app.workspace_history = crate::tab_history::TabHistory::from_recent([
            crate::model::WorkspaceDestination::Channel(channel),
            crate::model::WorkspaceDestination::Folder(project),
        ]);
        let image = Attachment {
            // Metadata-only synthetic saved image; no asset registry, decoder,
            // clipboard or provider is exercised by this reference.
            path: home.join("synthetic-image.png"),
            reference: Some(chat::ImageReference {
                id: uuid::Uuid::from_u128(19953),
                device: uuid::Uuid::from_u128(19954),
                number: 3,
                name: "Synthetic image metadata".into(),
                sha256: "0".repeat(64),
                extension: "png".into(),
            }),
        };
        for (number, id) in [id, neighbour_id].into_iter().enumerate() {
            let CardKind::Chat(session) = &app.card_state(id).unwrap().kind else {
                unreachable!()
            };
            let session = session.clone();
            let mut snapshot = draft_snapshot(
                Provider::Claude,
                vec![chat::Model {
                    id: "fixture-model".into(),
                    label: "Synthetic model".into(),
                    reasoning: vec!["high".into()],
                }],
                Some("fixture-model".into()),
                Some("high".into()),
            );
            snapshot.permission_mode = Some("plan".into());
            snapshot.skills = vec![chat::Skill {
                name: "Retained fixture skill".into(),
                description: "Synthetic catalog metadata".into(),
                invocation: "/fixture-skill".into(),
            }];
            let target = app.chat_ui.next_target(id);
            let mut runtime = Runtime::draft(
                target,
                session,
                snapshot,
                false,
                app.settings.editor_syntax_theme,
            );
            runtime.replace_draft(text_editor::Content::with_text(&format!(
                "Unsent draft {number} [Image 3]"
            )));
            runtime.attachments = vec![image.clone()];
            runtime.history = vec![format!("Synthetic earlier prompt {number}")];
            runtime.history_images = vec![vec![image.clone()]];
            runtime.expanded.insert("retained-disclosure".into());
            runtime.tool_groups_expanded = true;
            app.cards.insert(id, CardRuntime::Chat(Box::new(runtime)));
        }
        let database = root.join("window.sqlite3");
        let boot = crate::model::BootId::new("old-save-recovery-fixture");
        let (writer, restored) =
            crate::session::SessionRuntime::open(&database, boot.clone()).unwrap();
        assert!(restored.is_none(), "fixture database must start empty");
        app.session = Some(writer);
        app.session
            .as_ref()
            .unwrap()
            .save_project(crate::model::ProjectState {
                id: project,
                generation: 0,
                name: "Synthetic saved project".into(),
                repository: home,
                kind: crate::model::ProjectKind::Folder,
                existence: Existence::Open,
            });
        app.persist();
        assert!(
            app.session.as_ref().unwrap().flush(WAIT).is_ok(),
            "initial snapshot must save"
        );
        let initial = app.persistent_workspace();
        let projects = app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .list_projects()
            .unwrap();
        Self {
            app,
            database,
            boot,
            card: id,
            neighbour: neighbour_id,
            account_a: accounts[0].id(),
            account_b: accounts[1].id(),
            home_b: accounts[1].resolved_home().unwrap(),
            initial,
            projects,
        }
    }

    fn refuse(&mut self) -> WindowMemory {
        let target = chat_runtime(&self.app, self.card).target;
        let runtime = self.app.chat_runtime_mut(target).unwrap();
        runtime.session.launch.recipe.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            self.home_b.to_str().unwrap().into(),
        );
        let changed = runtime.session.clone();
        self.app.card_state_mut(self.card).unwrap().kind = CardKind::Chat(changed);
        self.app.card_state_mut(self.neighbour).unwrap().title = "Latest unsaved title".into();
        write(
            &mut self.app,
            self.neighbour,
            "Latest unsaved neighbour draft [Image 3]",
        );
        let durable = Arc::make_mut(&mut self.app.durable);
        durable.channels[0].title = "Latest unsaved channel title".into();
        for card in &mut durable.cards {
            if let CardKind::Browser(browser) = &mut card.kind {
                let url = "https://example.invalid/latest-unsaved".parse().unwrap();
                browser.current_url = url;
                browser.history.push(browser.current_url.clone());
                browser.history_index = 2;
                browser.scroll_y = 760.0;
            }
        }
        for navigation in durable.repository_navigation.values_mut() {
            navigation.editor.as_mut().unwrap().caret.line = 99;
        }
        let snapshot = self.app.persistent_workspace();
        let store = self.app.session.as_ref().unwrap().operation_store();
        let base = store.stored_generations(snapshot.id).unwrap();
        let refusal = store.save_workspace(
            &snapshot,
            crate::persistence::SnapshotBase {
                workspace: base.workspace,
                cards: &base.cards,
            },
        );
        assert!(
            matches!(&refusal, Err(crate::persistence::StoreError::Card { card, error })
                if *card == self.card && matches!(error.as_ref(),
                    crate::persistence::StoreError::InvalidState(reason)
                        if reason.contains("(launch recipe)"))),
            "real ordinary SQLite transaction must refuse this pane's changed recipe",
        );
        // The rejected ordinary transaction is not an accepted repair write.
        // It must roll back the newest title/browser/editor changes together.
        self.assert_stored(&self.initial);
        self.app.persist();
        assert!(
            self.app.session.as_ref().unwrap().flush(WAIT).is_err(),
            "old recipe mismatch must refuse"
        );
        let events = self.app.session.as_ref().unwrap().health_receiver();
        let mut typed_writer_refusal = false;
        while let Ok(event) = events.try_recv() {
            typed_writer_refusal |= event.quarantined.iter().any(|write| {
                write.category == crate::session::SaveFailureCategory::InvalidData
                    && matches!(write.subject,
                        crate::session::WriteSubject::Workspace { card: Some(card) }
                            if card == self.card)
            });
        }
        assert!(
            typed_writer_refusal,
            "real writer must report this exact pane as invalid data, not an unrelated I/O error",
        );
        self.assert_stored(&self.initial);
        WindowMemory::capture(&mut self.app)
    }

    fn assert_stored(&self, expected: &WorkspaceState) {
        let restored = self
            .app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .load_workspace_restored(expected.id, None)
            .unwrap()
            .unwrap();
        assert!(
            restored.warnings.is_empty(),
            "restore must not silently lose a pane"
        );
        assert_restored(&restored.state, expected);
        assert!(
            self.app
                .session
                .as_ref()
                .unwrap()
                .operation_store()
                .list_projects()
                .unwrap()
                == self.projects,
            "project catalog must remain intact",
        );
    }

    fn assert_no_operations(&self) {
        for id in [self.card, self.neighbour] {
            let runtime = chat_runtime(&self.app, id);
            assert!(runtime.choosing(), "both chats must remain eligible drafts");
            assert!(
                runtime.handle().is_none() && runtime.pending.is_none(),
                "no provider command"
            );
            assert!(!self.app.chat_start_pending(id), "no first send");
        }
    }
}

fn assert_restored(actual: &WorkspaceState, expected: &WorkspaceState) {
    // The public load API intentionally stamps storage generations and restores
    // every card Offscreen/Cold. Normalize ONLY these documented runtime fields.
    let mut projected = expected.clone();
    projected.generation = actual.generation;
    for (card, loaded) in projected.cards.iter_mut().zip(&actual.cards) {
        card.generation = loaded.generation;
        card.exposure = Exposure::Offscreen;
        card.residency = Residency::Cold;
    }
    assert!(actual == &projected, "complete restored workspace differs");
}

async fn prepare(app: &mut Cagents) -> bool {
    assert!(
        app.window_update_blocker().is_none(),
        "fixture has no dirty editor/provider operation"
    );
    let (response, receiver) = std::sync::mpsc::sync_channel(1);
    let task = app.update(Message::UiUpdate(instance::UiUpdateRequest {
        action: instance::UiUpdateAction::Prepare,
        response,
    }));
    let mut stream = iced::runtime::task::into_stream(task).expect("real update flush task");
    let mut completed = false;
    while let Some(action) = stream.next().await {
        if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action
            && matches!(message, Message::UiUpdateFlushed { .. })
        {
            drop(app.update(message));
            completed = true;
            break;
        }
    }
    assert!(completed, "real update flush must complete");
    let reply = receiver.try_recv().expect("update flush reply");
    assert!(
        !app.update_committed && app.exit.is_none(),
        "preparation cannot close the window"
    );
    if reply == "ok" {
        true
    } else {
        assert!(
            reply.contains("storage refused part of the window's own state"),
            "update must fail for the refused snapshot, not an unrelated settings/I/O error",
        );
        false
    }
}

fn executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn identical_permission_choice_and_retry_leave_old_refusal_unrepaired() {
    let Some(root) =
        isolated_parent("identical_permission_choice_and_retry_leave_old_refusal_unrepaired")
    else {
        return;
    };
    let mut fixture = Fixture::new(&root);
    let before = fixture.refuse();
    executor().block_on(async {
        assert!(
            !prepare(&mut fixture.app).await,
            "old update must veto refused state"
        );
    });
    let target = chat_runtime(&fixture.app, fixture.card).target;
    drop(fixture.app.update(Message::ChatUi(Action::PermissionMode(
        target,
        "plan".into(),
    ))));
    fixture.app.persist();
    assert!(
        fixture.app.session.as_ref().unwrap().retry_save(),
        "writer accepts retry request"
    );
    assert!(
        fixture.app.session.as_ref().unwrap().flush(WAIT).is_err(),
        "same descriptor is not repair"
    );
    assert!(
        WindowMemory::capture(&mut fixture.app) == before,
        "refused in-memory work must survive"
    );
    fixture.assert_stored(&fixture.initial);
    fixture.assert_no_operations();
}

#[test]
fn permission_round_trip_requires_full_checkpoint_and_preserves_captured_window() {
    let Some(root) = isolated_parent(
        "permission_round_trip_requires_full_checkpoint_and_preserves_captured_window",
    ) else {
        return;
    };
    let mut fixture = Fixture::new(&root);
    let before = fixture.refuse();
    executor().block_on(async {
        assert!(
            !prepare(&mut fixture.app).await,
            "same source window must first refuse its real update preparation",
        );
    });
    assert!(
        WindowMemory::capture(&mut fixture.app) == before,
        "the failed update must leave the entire captured source window available",
    );
    let target = chat_runtime(&fixture.app, fixture.card).target;
    let desired = chat_runtime(&fixture.app, fixture.card).session.clone();
    let store = fixture.app.session.as_ref().unwrap().operation_store();
    // This API has expected-ID semantics, not full original-recipe CAS.
    assert!(
        store
            .replace_chat_identity(fixture.card, TerminalId::new(), &desired)
            .is_err(),
        "wrong expected conversation must refuse"
    );
    fixture.assert_stored(&fixture.initial);
    drop(fixture.app.update(Message::ChatUi(Action::PermissionMode(
        target,
        "acceptEdits".into(),
    ))));
    drop(fixture.app.update(Message::ChatUi(Action::PermissionMode(
        target,
        "plan".into(),
    ))));
    assert!(
        WindowMemory::capture(&mut fixture.app) == before,
        "all captured memory and exact recipe must return unchanged"
    );
    assert!(
        fixture.app.session.as_ref().unwrap().flush(WAIT).is_ok(),
        "real complete writer checkpoint is required"
    );
    fixture.assert_stored(&before.workspace);
    executor().block_on(async {
        assert!(
            prepare(&mut fixture.app).await,
            "old update prepare may succeed only after durable repair"
        );
    });
    assert!(
        WindowMemory::capture(&mut fixture.app) == before,
        "prepare must preserve captured memory"
    );
    assert!(
        store
            .load_window_view()
            .unwrap()
            .map(|view| serde_json::from_str::<serde_json::Value>(&view).unwrap())
            == Some(before.view.clone()),
        "real update checkpoint must also preserve the complete window view",
    );
    assert!(
        settings::load(fixture.app.settings_path.as_ref().unwrap()).unwrap() == before.settings,
        "real update checkpoint must preserve settings",
    );
    let (response, receiver) = std::sync::mpsc::sync_channel(1);
    drop(
        fixture
            .app
            .update(Message::UiUpdate(instance::UiUpdateRequest {
                action: instance::UiUpdateAction::Cancel,
                response,
            })),
    );
    assert!(
        matches!(receiver.try_recv(), Ok(reply) if reply == "ok"),
        "fixture cancels preparation without closing"
    );
    fixture.assert_no_operations();
    let mut writer = fixture.app.session.take().unwrap();
    writer.shutdown();
    drop(store);
    drop(writer);
    let (mut reopened, restored) =
        crate::session::SessionRuntime::open(&fixture.database, fixture.boot.clone()).unwrap();
    assert_restored(
        &restored.expect("complete private store reopened"),
        &before.workspace,
    );
    assert!(
        reopened.restore_warnings().is_empty(),
        "no skipped restored panes"
    );
    assert!(
        reopened.operation_store().list_projects().unwrap() == fixture.projects,
        "reopen must retain the project catalog",
    );
    reopened.shutdown();
    // Reopening proves the OLD persistence API only. No new-version process,
    // native interaction, provider history or arbitrary memory export is claimed.
}

#[test]
fn identical_account_descriptor_does_not_repair_old_refusal() {
    let Some(root) = isolated_parent("identical_account_descriptor_does_not_repair_old_refusal")
    else {
        return;
    };
    let mut fixture = Fixture::new(&root);
    fixture.refuse();
    let target = chat_runtime(&fixture.app, fixture.card).target;
    let account = fixture.app.known_account(&fixture.account_b).unwrap();
    let current = chat_runtime(&fixture.app, fixture.card).session.clone();
    let descriptor = fresh_chat_session(
        Provider::Claude,
        current.id,
        root.join("home"),
        &account,
        false,
    )
    .unwrap();
    let runtime = fixture.app.chat_runtime_mut(target).unwrap();
    Arc::make_mut(&mut runtime.snapshot).permission_mode =
        draft_permission_mode(&descriptor.launch.recipe, Provider::Claude);
    runtime.session = descriptor.clone();
    fixture.app.card_state_mut(fixture.card).unwrap().kind = CardKind::Chat(descriptor);
    fixture.app.persist();
    assert!(
        fixture.app.session.as_ref().unwrap().flush(WAIT).is_err(),
        "ordinary constructor descriptor remains refused"
    );
    let before = WindowMemory::capture(&mut fixture.app);
    drop(
        fixture
            .app
            .update(Message::ChatUi(Action::AccountPicker(target, true))),
    );
    assert!(
        chat_runtime(&fixture.app, fixture.card).account_picker,
        "public draft picker must open"
    );
    drop(fixture.app.update(Message::ChatUi(Action::ChooseAccount(
        target,
        fixture.account_b.clone(),
    ))));
    fixture.app.persist();
    assert!(
        fixture.app.session.as_ref().unwrap().flush(WAIT).is_err(),
        "equal account descriptor is no repair"
    );
    assert!(
        WindowMemory::capture(&mut fixture.app) == before,
        "equal choice must leave captured memory alone"
    );
    fixture.assert_no_operations();
}

#[test]
fn account_round_trip_is_not_a_preserve_all_draft_repair() {
    let Some(root) = isolated_parent("account_round_trip_is_not_a_preserve_all_draft_repair")
    else {
        return;
    };
    let mut fixture = Fixture::new(&root);
    let before = fixture.refuse();
    let target = chat_runtime(&fixture.app, fixture.card).target;
    for id in [fixture.account_a.clone(), fixture.account_b.clone()] {
        drop(
            fixture
                .app
                .update(Message::ChatUi(Action::AccountPicker(target, true))),
        );
        assert!(
            chat_runtime(&fixture.app, fixture.card).account_picker,
            "public draft picker must open"
        );
        // This is the actual public reducer. Its returned Skills task is
        // dropped, never a fake successful discovery or provider completion.
        drop(
            fixture
                .app
                .update(Message::ChatUi(Action::ChooseAccount(target, id))),
        );
    }
    let after = WindowMemory::capture(&mut fixture.app);
    assert!(
        after != before,
        "strict preserve-all oracle must reject this route"
    );
    assert!(
        !before
            .chats
            .iter()
            .find(|chat| chat.card == fixture.card)
            .unwrap()
            .snapshot
            .skills
            .is_empty(),
        "negative control needs synthetic catalog metadata"
    );
    assert!(
        chat_runtime(&fixture.app, fixture.card)
            .snapshot
            .skills
            .is_empty(),
        "account choice cleared the captured catalog"
    );
    // No exception is removed from the equality oracle to turn this into a
    // recovery PASS. Permission/recipe differences may also remain. Even if a
    // complete save succeeds, that is the CHANGED state, not all original work.
    assert!(
        fixture.app.session.as_ref().unwrap().flush(WAIT).is_ok(),
        "explicit account replacement can checkpoint changed state"
    );
    fixture.assert_stored(&after.workspace);
    fixture.assert_no_operations();
}

#[test]
fn permission_round_trip_does_not_preserve_a_noncanonical_original_recipe() {
    let Some(root) =
        isolated_parent("permission_round_trip_does_not_preserve_a_noncanonical_original_recipe")
    else {
        return;
    };
    let mut fixture = Fixture::new(&root);
    fixture.refuse();
    let target = chat_runtime(&fixture.app, fixture.card).target;
    let runtime = fixture.app.chat_runtime_mut(target).unwrap();
    runtime.session.launch.recipe.arguments = vec!["--permission-mode=plan".into()];
    let descriptor = runtime.session.clone();
    fixture.app.card_state_mut(fixture.card).unwrap().kind = CardKind::Chat(descriptor);
    fixture.app.persist();
    assert!(
        fixture.app.session.as_ref().unwrap().flush(WAIT).is_err(),
        "noncanonical recipe still has the original ordinary save refusal",
    );
    let before = WindowMemory::capture(&mut fixture.app);
    for mode in ["acceptEdits", "plan"] {
        drop(
            fixture
                .app
                .update(Message::ChatUi(Action::PermissionMode(target, mode.into()))),
        );
    }
    let after = WindowMemory::capture(&mut fixture.app);
    assert!(
        after != before,
        "canonicalization must not be passed off as exact recipe preservation",
    );
    let original = &before
        .chats
        .iter()
        .find(|chat| chat.card == fixture.card)
        .unwrap()
        .session;
    let current = &chat_runtime(&fixture.app, fixture.card).session;
    assert!(
        original.launch.recipe.arguments != current.launch.recipe.arguments,
        "negative control must observe the changed argument representation",
    );
    assert!(
        original.id == current.id
            && original.launch.provider == current.launch.provider
            && original.launch.recipe.environment == current.launch.recipe.environment,
        "this counterexample keeps conversation, provider and account environment",
    );
    assert!(
        fixture.app.session.as_ref().unwrap().flush(WAIT).is_ok(),
        "an accepted changed checkpoint is not an exact preserve-all receipt",
    );
    fixture.assert_stored(&after.workspace);
    fixture.assert_no_operations();
}
