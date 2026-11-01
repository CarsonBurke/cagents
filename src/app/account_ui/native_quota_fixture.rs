//! BEFORE-only inert Accounts scenes using a labeled callback adapter.
//! Fixed frames use the original pure parsers, then the actual old reducer.
//! No collector, provider, transport or observation-clock behavior is executed
//! or qualified by this synthetic post-auth/close completion mapping.
use super::*;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use tokio::sync::oneshot;

const SELECTOR: &str = "CAGENTS_DEVELOPMENT_ACCOUNT_FIXTURE";
const SCENES: &[&str] = &[
    "claude-sparse",
    "codex-sparse",
    "claude-malformed",
    "codex-malformed",
    "claude-delayed",
    "codex-delayed",
    "claude-ticket",
    "codex-ticket",
    "claude-startup",
    "codex-startup",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Sparse,
    Malformed,
    Delayed,
    Ticket,
    Startup,
}

pub(in crate::app) struct Context {
    root: PathBuf,
    scene: String,
    provider: Provider,
    scenario: Scenario,
}

/// Recognition happens before device/window preparation as well as before
/// bootstrap. Only the five explicitly selected fixture XDG paths are read.
pub(in crate::app) fn context() -> Result<Option<Context>, String> {
    let Some(scene) = std::env::var_os(SELECTOR) else {
        return Ok(None);
    };
    let scene = scene
        .into_string()
        .map_err(|_| "invalid Accounts fixture selector")?;
    if !SCENES.contains(&scene.as_str()) {
        return Err("unknown inert Accounts scene".into());
    }
    if std::env::var_os("CAGENTS_LATENCY_OUTPUT").is_some()
        || std::env::var_os("CAGENTS_DEVELOPMENT_AUTOMATION_FIXTURE").is_some()
    {
        return Err("Accounts fixture cannot share another fixture".into());
    }
    let names = [
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ];
    let roots: Vec<_> = names
        .iter()
        .map(|name| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .ok_or("fixture needs all five private XDG roots")
        })
        .collect::<Result<_, _>>()?;
    let root = roots[0]
        .parent()
        .ok_or("missing fixture namespace")?
        .to_path_buf();
    if roots.iter().any(|path| {
        !path.is_absolute()
            || path.components().count() != 4
            || path.components().any(|c| {
                !matches!(
                    c,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
    }) || root.parent() != Some(Path::new("/tmp"))
        || ![
            format!("cagents-1938-native-{scene}-wide-before"),
            format!("cagents-1938-native-{scene}-narrow-before"),
        ]
        .iter()
        .any(|name| root.file_name() == Some(std::ffi::OsStr::new(name)))
        || roots
            .iter()
            .any(|path| path.parent() != Some(root.as_path()))
        || roots.iter().collect::<HashSet<_>>().len() != names.len()
    {
        return Err("fixture needs distinct direct children of its exact /tmp/cagents-1938-native-<scene>-<wide|narrow>-before namespace".into());
    }
    private_directory(&root)?;
    for path in &roots {
        private_directory(path)?;
    }
    let provider = if scene.starts_with("claude-") {
        Provider::Claude
    } else {
        Provider::Codex
    };
    let scenario = match scene.split_once('-').unwrap().1 {
        "sparse" => Scenario::Sparse,
        "malformed" => Scenario::Malformed,
        "delayed" => Scenario::Delayed,
        "ticket" => Scenario::Ticket,
        "startup" => Scenario::Startup,
        _ => unreachable!(),
    };
    Ok(Some(Context {
        root,
        scene,
        provider,
        scenario,
    }))
}

fn private_directory(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o700
        || path.canonicalize().map_err(|e| e.to_string())? != path
    {
        return Err("fixture directory must be canonical, owned and mode 0700".into());
    }
    Ok(())
}

pub(in crate::app) fn bootstrap(context: &Context) -> Result<Bootstrap, String> {
    use std::io::Write;
    let mut marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(context.root.join("accounts-native-started"))
        .map_err(|e| format!("fixture namespace already used or unavailable: {e}"))?;
    writeln!(marker, "INERT Accounts: {}", context.scene).map_err(|e| e.to_string())?;
    let directory = context.root.join("public-project");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .map_err(|e| e.to_string())?;
    let project = ProjectId::from_uuid(uuid::Uuid::from_u128(193_801));
    let channel = ChannelId::from_uuid(uuid::Uuid::from_u128(193_802));
    let mut workspace = WorkspaceState::default();
    workspace.active_channel = Some(channel);
    workspace.channels.push(crate::model::ProjectChannel {
        id: channel,
        project_id: project,
        title: "INERT Accounts · synthetic data".into(),
        title_source: Default::default(),
        ordinal: 0,
        shared: false,
        cards: Vec::new(),
        focused: None,
    });
    let mut settings = AppSettings::default();
    settings.app_theme = AppTheme::Dark;
    Ok(Bootstrap {
        workspace,
        session: None,
        projects: vec![ProjectState {
            id: project,
            generation: 0,
            name: "INERT Accounts · no provider".into(),
            repository: directory,
            kind: ProjectKind::Folder,
            existence: Existence::Open,
        }],
        worktrees: Vec::new(),
        settings,
        settings_path: None,
        settings_write_blocked: true,
        status: None,
        save_health: None,
        restore_warnings: Vec::new(),
        settings_warnings: Vec::new(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Seed,
    Sparse,
    Delayed,
    OldA,
    NewB,
    Recovery,
}

struct Work {
    kind: Kind,
    owner: u64,
    id: AccountId,
    observed_at: Millis,
    input_held: bool,
    returned: bool,
    ready: Option<oneshot::Sender<()>>,
    close: Option<oneshot::Sender<()>>,
}

pub(in crate::app) struct Fixture {
    context: Context,
    system: Account,
    source: Account,
    baseline: Millis,
    logical_now: Millis,
    next: Option<Kind>,
    phase: &'static str,
    works: Vec<Work>,
    error: Option<String>,
    startup: Option<Startup>,
}

struct StartupAdmission {
    id: AccountId,
    owner: u64,
    trigger: Trigger,
}

struct Startup {
    original_accounts: Vec<(AccountId, u64)>,
    original_revision: u64,
    trigger: Option<Trigger>,
    admissions: Vec<StartupAdmission>,
}

/// Internal in-process gate notification only; no wire, commands or arbitrary
/// workload/coordinate payload. The original task owns each sender/receiver.
#[derive(Debug, Clone)]
pub enum Event {
    InputHeld(usize),
    Completion {
        serial: usize,
        owner: u64,
        id: AccountId,
        result: Result<Outcome, String>,
    },
}

fn receipt(value: serde_json::Value) {
    println!("ACCOUNT_NATIVE {value}");
}

fn fail(app: &mut Cagents, message: &str) {
    let f = app.account_ui.native_fixture.as_mut().unwrap();
    f.next = None;
    f.phase = "stopped-unqualified";
    f.error.get_or_insert_with(|| message.to_owned());
    receipt(
        serde_json::json!({"event":"fixture_error","scene":f.context.scene,"error":f.error,"qualified":false}),
    );
}

pub(in crate::app) fn install(app: &mut Cagents, context: Context) -> Task<Message> {
    if context.scenario == Scenario::Startup {
        return install_startup(app, context);
    }
    let system = Account {
        provider: context.provider,
        home: account::AccountHome::SystemAt {
            path: context.root.join("synthetic-system-never-read"),
        },
    };
    let key = uuid::Uuid::from_u128(193_803);
    let homes = Homes::at(context.root.join("synthetic-homes-never-read"));
    let source = Account::managed(context.provider, key, homes.path(context.provider, key));
    let mut state = State::default();
    state.homes = Some(homes);
    state
        .remote_system_homes
        .insert(context.provider, system.resolved_home().unwrap());
    state.accounts = vec![
        AccountState::new(system.clone()),
        AccountState::new(source.clone()),
    ];
    state
        .registry
        .register(
            Registered {
                key,
                provider: context.provider,
                linked_home: None,
                label: Some("Exhausted synthetic alternate".into()),
                email: Some("alternate@fixture.invalid".into()),
                organization: None,
                added_at: 1,
            },
            &[],
        )
        .expect("unique synthetic registry entry");
    state.registry.set_automatic(context.provider);
    for provider in Account::PROVIDERS {
        state
            .installed
            .insert(provider, provider == context.provider);
    }
    let now = now_ms();
    let exhausted = state.account_mut(&source.id()).unwrap();
    exhausted.present = true;
    exhausted.snapshot = Some(match context.provider {
        Provider::Claude => account::claude::parse_usage(
            &serde_json::json!({
            "rate_limits":{"limits":[{"kind":"session","percent":100.0}]}}),
            now,
        ),
        Provider::Codex => account::codex::parse_rate_limits(
            &serde_json::json!({
            "ordinaryUsageAllowed":false,"rateLimits":{"limitId":"codex","primary":{
                "usedPercent":100.0,"windowDurationMins":300}}}),
            now,
        ),
        _ => unreachable!(),
    });
    let empty = context.scenario == Scenario::Malformed;
    if empty {
        // Preserve the actual empty SYSTEM visibility rule. In particular,
        // do not add a row just to manufacture a Refresh target.
        state.accounts.truncate(1);
        state.registry = Registry::default();
    }
    state.native_fixture = Some(Fixture {
        context,
        system: system.clone(),
        source,
        baseline: now,
        logical_now: now.saturating_sub(schedule::STALE_AFTER + schedule::MANUAL_DEBOUNCE + 2_000),
        next: Some(if empty { Kind::Sparse } else { Kind::Seed }),
        phase: "initial-synthetic-old-callback",
        works: Vec::new(),
        error: None,
        startup: None,
    });
    app.account_ui = state;
    app.active_workspace = Some(
        app.account_ui
            .native_fixture
            .as_ref()
            .unwrap()
            .context
            .root
            .join("public-project"),
    );
    app.refresh_ui.focused = true;
    app.context_inspector_open = false;
    app.notices.record(TransientNotice::warning("INERT Accounts fixture · synthetic data · F8 advances held response/logical time · no provider"));
    let open = app.open_utility_card(CardKind::Accounts);
    let seed = app
        .probe_account(&system.id(), Trigger::Focus, now)
        .unwrap_or_else(Task::none);
    app.refresh_sidebar_channels();
    app.synchronize_sidebar_selection();
    let sidebar = sidebar::effective_width(app.projects_sidebar_mode(), app.settings.sidebar_width);
    let _ = app
        .workspace
        .resize_viewport((app.content_width() - sidebar).max(0.0));
    receipt(
        serde_json::json!({"event":"installed","scene":app.account_ui.native_fixture.as_ref().unwrap().context.scene,
        "transport_executed":false,"logical_expiry_only":true,
        "variant":"before","callback_adapter":"original pure parsers and old Action::Probed; synthetic post-auth/close timestamp, not a collector"}),
    );
    let theme = app.settings.app_theme.resolve(app.core.system_theme());
    Task::batch([
        open,
        seed,
        app.sync_chrome(),
        cosmic::command::set_theme(theme),
    ])
}

/// Operational subscriptions are absent; only an explicit fixture seed or the
/// real visible Refresh control may enter the unchanged production gates.
pub(in crate::app) fn admission_time(
    app: &mut Cagents,
    trigger: Trigger,
    now: Millis,
) -> Option<Millis> {
    let Some(f) = app.account_ui.native_fixture.as_mut() else {
        return Some(now);
    };
    if let Some(startup) = f.startup.as_mut() {
        // Forward every real old scheduling trigger. There is deliberately no
        // readiness gate or quota-scene Manual-only filter in this adapter.
        startup.trigger = Some(trigger);
        return f.error.is_none().then_some(now);
    }
    let initial = f.works.is_empty();
    if f.error.is_some()
        || f.next.is_none()
        || (initial && trigger != Trigger::Focus)
        || (!initial && trigger != Trigger::Manual)
    {
        return None;
    }
    let time = if f.next == Some(Kind::Seed) {
        f.logical_now
    } else {
        f.logical_now.max(now)
    };
    f.logical_now = time;
    app.account_ui.now_ms = time;
    Some(time)
}

fn startup_accounts(app: &Cagents) -> Vec<(AccountId, u64)> {
    app.account_ui
        .accounts
        .iter()
        .map(|row| (row.id.clone(), row.owner))
        .collect()
}

fn startup_effects(app: &Cagents) -> serde_json::Value {
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    let startup = f.startup.as_ref().unwrap();
    let rows: Vec<_> = app
        .account_ui
        .accounts
        .iter()
        .map(|row| {
            serde_json::json!({"id":row.id,"owner":row.owner,"present":row.present,
            "in_flight":row.schedule.in_flight(app.account_ui.now_ms),
            "manual_ready":row.schedule.manual_ready(app.account_ui.now_ms),
            "queued":format!("{:?}",row.queued),
            "sign_in_preparing":app.account_ui.preparing_logins.contains(&row.owner),
            "sign_in_terminal":row.sign_in_terminal.is_some()})
        })
        .collect();
    let admissions: Vec<_> = startup
        .admissions
        .iter()
        .enumerate()
        .map(|(serial, admission)| {
            serde_json::json!({"diagnostic_serial":serial,"id":admission.id,"owner":admission.owner,
            "trigger":format!("{:?}",admission.trigger),"production_ticket_available":false})
        })
        .collect();
    serde_json::json!({"variant":"before","readiness_gate_available":false,"registry_ready":null,
        "production_ticket_available":false,"production_ticket_status":"production-ticket-unavailable",
        "serial_namespace":"fixture-diagnostic-only; no production admission identity",
        "selected_account":f.system.id(),"untouched_account":f.source.id(),
        "registry_revision":app.account_ui.registry.revision(),
        "original_registry_revision":startup.original_revision,
        "account_count":app.account_ui.accounts.len(),"original_account_count":startup.original_accounts.len(),
        "original_ids_and_owners_conserved":startup_accounts(app)==startup.original_accounts,
        "rows":rows,"admissions":admissions,
        "store_present":app.account_ui.store.is_some(),"cache_present":app.account_ui.cache.is_some(),
        "terminal_cards":app.durable.cards.iter().filter(|card| matches!(&card.kind, CardKind::Terminal(_))).count(),
        "retry_isolated_eligibility_proof":false,"provider_outcome":null,"qualified":false})
}

fn startup_receipt(app: &Cagents, event: &str) {
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    receipt(
        serde_json::json!({"event":event,"scene":f.context.scene,"phase":f.phase,"error":f.error,
        "effects":startup_effects(app),"boundary":"synthetic callbacks and actual OLD reducer scheduling only",
        "input_origin_requires_native_controller_evidence":true,
        "operational_tasks_polled":false,"provider_executed":false,"qualified":false}),
    );
}

fn install_startup(app: &mut Cagents, context: Context) -> Task<Message> {
    // Match After's exact synthetic metadata, not the real installed accounts.
    let mut state = State::default();
    for provider in Account::PROVIDERS {
        state.remote_system_homes.insert(
            provider,
            context
                .root
                .join(format!("synthetic-{}-never-read", provider.executable())),
        );
    }
    state.sync_accounts();
    let system = state
        .account(&Account::system(context.provider).id())
        .unwrap()
        .account
        .clone();
    let other = match context.provider {
        Provider::Claude => Provider::Codex,
        Provider::Codex => Provider::Claude,
        _ => unreachable!("two explicit startup scenes"),
    };
    let source = state
        .account(&Account::system(other).id())
        .unwrap()
        .account
        .clone();
    let original_accounts = state
        .accounts
        .iter()
        .map(|row| (row.id.clone(), row.owner))
        .collect();
    let original_revision = state.registry.revision();
    let restored = state
        .accounts
        .iter()
        .map(|row| Restored {
            id: row.id.clone(),
            present: true,
            entry: None,
        })
        .collect();
    let now = now_ms();
    state.native_fixture = Some(Fixture {
        context,
        system,
        source,
        baseline: now,
        logical_now: now,
        next: None,
        phase: "startup-before-installed",
        works: Vec::new(),
        error: None,
        startup: Some(Startup {
            original_accounts,
            original_revision,
            trigger: None,
            admissions: Vec::new(),
        }),
    });
    app.account_ui = state;
    app.active_workspace = Some(
        app.account_ui
            .native_fixture
            .as_ref()
            .unwrap()
            .context
            .root
            .join("public-project"),
    );
    app.refresh_ui.focused = false;
    app.settings.show_status_bar = false;
    app.context_inspector_open = false;
    drop(app.update_account_ui(Action::Loaded(Loaded {
        registry: Err("INERT startup registry unavailable".into()),
        restored,
        catalogs: BTreeMap::new(),
        installed: Account::PROVIDERS.into_iter().map(|p| (p, true)).collect(),
        provisional: false,
    })));
    if let Some(notice) = app.status.take() {
        app.notices.record(notice);
    }
    app.notices.record(TransientNotice::warning("INERT startup policy · synthetic metadata · F8 stages failed read/retry · no provider or sign-in execution"));
    let open = app.open_utility_card(CardKind::Accounts);
    app.refresh_sidebar_channels();
    app.synchronize_sidebar_selection();
    let sidebar = sidebar::effective_width(app.projects_sidebar_mode(), app.settings.sidebar_width);
    let _ = app
        .workspace
        .resize_viewport((app.content_width() - sidebar).max(0.0));
    startup_receipt(app, "startup_before_installed");
    let theme = app.settings.app_theme.resolve(app.core.system_theme());
    Task::batch([open, app.sync_chrome(), cosmic::command::set_theme(theme)])
}

fn start_startup(app: &mut Cagents, account: Account, id: AccountId, owner: u64) -> Task<Message> {
    let current = app.account_ui.account(&id).is_some_and(|row| {
        row.account == account
            && row.owner == owner
            && row.schedule.in_flight(app.account_ui.now_ms)
    });
    let f = app.account_ui.native_fixture.as_mut().unwrap();
    let startup = f.startup.as_mut().unwrap();
    let Some(trigger) = startup.trigger.take() else {
        fail(
            app,
            "Before startup admission lacks its actual old scheduling trigger",
        );
        return Task::none();
    };
    if !current
        || startup.admissions.len() >= 8
        || !startup
            .original_accounts
            .iter()
            .any(|(row, original_owner)| *row == id && *original_owner == owner)
    {
        fail(
            app,
            "Before startup admission escaped the bounded original rows",
        );
        return Task::none();
    }
    // These are diagnostic list positions, not fabricated production tickets.
    startup
        .admissions
        .push(StartupAdmission { id, owner, trigger });
    startup_receipt(app, "startup_before_actual_old_schedule_admission");
    // Do not construct or poll the old callback adapter, provider or collector.
    Task::none()
}

fn startup_action(app: &mut Cagents, action: Action) -> Task<Message> {
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    if f.error.is_some() {
        return Task::none();
    }
    let event = match &action {
        Action::Refresh(id) | Action::SignIn(id) | Action::SignInWithCode(id) => {
            if *id != f.system.id() {
                fail(
                    app,
                    "Before startup native action targeted the wrong original row",
                );
                return Task::none();
            }
            match &action {
                Action::Refresh(_) => "startup_before_refresh_callback_if_actually_delivered",
                Action::SignIn(_) => "startup_before_signin_preparation_callback",
                _ => "startup_before_signin_with_code_preparation_callback",
            }
        }
        Action::Tick | Action::Popover(_) => "startup_before_scheduling_callback",
        _ => unreachable!("finite old startup callbacks only"),
    };
    // Preserve the exact native callback variant. A disabled Check now has no
    // callback; never synthesize a successful Refresh or LoginReady instead.
    drop(app.update_account_ui(action));
    startup_receipt(app, event);
    Task::none()
}

fn advance_startup(app: &mut Cagents) {
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    if f.error.is_some() {
        return;
    }
    match f.phase {
        "startup-before-installed" => {
            if !app.refresh_ui.focused {
                fail(app, "Before startup needs actual native window focus first");
                return;
            }
            let id = f.system.id();
            drop(app.update_account_ui(Action::RegistryLoaded(Err(
                "INERT registry retry unavailable".into(),
            ))));
            drop(app.accounts_focus_changed(true, true));
            drop(app.update_account_ui(Action::Tick));
            drop(app.update_account_ui(Action::Popover(Some(id))));
            drop(app.update_account_ui(Action::Popover(None)));
            let f = app.account_ui.native_fixture.as_ref().unwrap();
            let startup = f.startup.as_ref().unwrap();
            let observed = startup_accounts(app) == startup.original_accounts
                && startup.admissions.len() == startup.original_accounts.len()
                && startup
                    .admissions
                    .iter()
                    .all(|admission| admission.trigger == Trigger::Focus)
                && startup.original_accounts.iter().all(|(id, owner)| {
                    startup
                        .admissions
                        .iter()
                        .filter(|admission| admission.id == *id && admission.owner == *owner)
                        .count()
                        == 1
                })
                && app.account_ui.accounts.iter().all(|row| {
                    row.schedule.in_flight(app.account_ui.now_ms)
                        && !row.schedule.manual_ready(app.account_ui.now_ms)
                });
            if observed {
                app.account_ui.native_fixture.as_mut().unwrap().phase =
                    "startup-before-existing-inflight";
            } else {
                fail(
                    app,
                    "Before startup no longer matches bounded automatic in-flight consequence",
                );
            }
            startup_receipt(
                app,
                "startup_before_failed_read_existing_inflight_check_disabled",
            );
        }
        "startup-before-existing-inflight" => {
            let count = app.account_ui.accounts.len();
            let admissions = f.startup.as_ref().unwrap().admissions.len();
            let revision = app
                .account_ui
                .registry
                .revision()
                .checked_add(1)
                .expect("finite fixture revision");
            let mut value =
                serde_json::to_value(&app.account_ui.registry).expect("typed old fixture registry");
            value["revision"] = serde_json::json!(revision);
            let registry: Registry =
                serde_json::from_value(value).expect("same old policy, later revision");
            drop(app.update_account_ui(Action::RegistryLoaded(Ok(registry))));
            let f = app.account_ui.native_fixture.as_ref().unwrap();
            let startup = f.startup.as_ref().unwrap();
            if app.account_ui.accounts.len() == count
                && startup_accounts(app) == startup.original_accounts
                && app.account_ui.registry.revision() == revision
                && startup.admissions.len() == admissions
            {
                app.account_ui.native_fixture.as_mut().unwrap().phase =
                    "startup-before-complete-existing-inflight";
            } else {
                fail(
                    app,
                    "Before same-count callback changed the bounded old row/admission facts",
                );
            }
            // Both rows were already in flight. This cannot independently
            // prove or refute eligibility-release behavior after a retry.
            startup_receipt(
                app,
                "startup_before_same_count_retry_existing_inflight_not_isolated_release_proof",
            );
        }
        _ => startup_receipt(app, "startup_before_step_refused"),
    }
}

enum Frames {
    Claude {
        initialize: serde_json::Value,
        quota: serde_json::Value,
        auth: serde_json::Value,
    },
    Codex {
        quota: serde_json::Value,
        account: serde_json::Value,
        models: Option<serde_json::Value>,
    },
}

fn frames(provider: Provider, sparse: bool, malformed: bool) -> Frames {
    let quota = if sparse {
        if malformed {
            match provider {
                Provider::Claude => {
                    serde_json::json!({"rate_limits":{"limits":[{"kind":"session","percent":"not a number"}]}})
                }
                Provider::Codex => {
                    serde_json::json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":"not a number"}}})
                }
                _ => unreachable!(),
            }
        } else {
            serde_json::json!({})
        }
    } else {
        match provider {
            Provider::Claude => {
                serde_json::json!({"subscription_type":"max","rate_limits_available":true,"rate_limits":{
                "limits":[{"kind":"session","percent":20.0}],"spend":{"enabled":true,"percent":25.0,
                "used":{"amount_minor":250,"exponent":2,"currency":"USD"},"limit":{"amount_minor":1000,"exponent":2,"currency":"USD"}}}})
            }
            Provider::Codex => {
                serde_json::json!({"ordinaryUsageAllowed":true,"rateLimits":{"limitId":"codex","planType":"pro",
                "primary":{"usedPercent":20.0,"windowDurationMins":300},"credits":{"hasCredits":true,"unlimited":false,"balance":"$12.34"}}})
            }
            _ => unreachable!(),
        }
    };
    match provider {
        Provider::Claude => Frames::Claude {
            initialize: serde_json::json!({"type":"control_response","response":{"subtype":"success",
                "request_id":"cagents-usage-initialize","response":{"models":[{"value":"opus"}]}}}),
            quota: serde_json::json!({"type":"control_response","response":{"subtype":"success","request_id":"cagents-usage","response":quota}}),
            auth: serde_json::json!({"loggedIn":true,"email":"native-system@fixture.invalid","subscriptionType":"max"}),
        },
        Provider::Codex => Frames::Codex {
            quota: serde_json::json!({"id":3,"result":quota}),
            account: serde_json::json!({"id":2,"result":{"account":{"type":"chatgpt","email":"native-system@fixture.invalid","planType":"pro"},"requiresOpenaiAuth":true}}),
            models: Some(serde_json::json!({"id":4,"result":{"data":[{"model":"gpt-5"}]}})),
        },
        _ => unreachable!(),
    }
}

/// A narrow finite adapter, not an old production collector. These exact
/// successful fixture frames use the original public usage/auth/model parsers.
/// The timestamp models the old post-auth/close placement (probe.rs), and the
/// catalog deliberately shares the snapshot stamp as old run() did.
fn original_callback(raw: Frames, observed_at: Millis) -> Result<Outcome, String> {
    let (identity, snapshot, models) = match raw {
        Frames::Claude {
            initialize,
            quota,
            auth,
        } => {
            let initialized =
                probe::match_control_response(&initialize, "cagents-usage-initialize")
                    .ok_or("fixed initialize frame did not match")??;
            let quota = probe::match_control_response(&quota, "cagents-usage")
                .ok_or("fixed quota frame did not match")??;
            let identity = account::claude::parse_auth_status(&auth)
                .map_err(|state| format!("fixed signed-in Claude input was {state:?}"))?;
            let models = initialized["models"]
                .as_array()
                .ok_or("fixed Claude models missing")?
                .iter()
                .filter_map(crate::chat::Model::from_claude)
                .collect::<Vec<_>>();
            (
                identity,
                account::claude::parse_usage(&quota, observed_at),
                models,
            )
        }
        Frames::Codex {
            quota,
            account: auth,
            models,
        } => {
            if quota["id"] != 3 || auth["id"] != 2 {
                return Err("fixed Codex frame did not match".into());
            }
            let identity = account::codex::parse_account(&auth["result"])
                .map_err(|state| format!("fixed signed-in Codex input was {state:?}"))?;
            let models = models.ok_or("fixed Codex model frame missing")?;
            if models["id"] != 4 {
                return Err("fixed Codex models did not match".into());
            }
            let models = models["result"]["data"]
                .as_array()
                .ok_or("fixed Codex models missing")?
                .iter()
                .filter_map(crate::chat::Model::from_codex)
                .collect::<Vec<_>>();
            (
                identity,
                account::codex::parse_rate_limits(&quota["result"], observed_at),
                models,
            )
        }
    };
    if models.is_empty() {
        return Err("fixed model parser returned no model".into());
    }
    Ok(Outcome::Answered {
        identity: Some(identity),
        catalog: Some(Catalog {
            models,
            observed_at: snapshot.observed_at,
        }),
        snapshot,
    })
}

/// Called only AFTER the original schedule.start and owner/id capture.
/// Serial labels the original fixture future; it is never a production ticket.
pub(in crate::app) fn start(
    app: &mut Cagents,
    account: Account,
    id: AccountId,
    owner: u64,
) -> Task<Message> {
    if app
        .account_ui
        .native_fixture
        .as_ref()
        .is_some_and(|f| f.startup.is_some())
    {
        return start_startup(app, account, id, owner);
    }
    let f = app.account_ui.native_fixture.as_mut().unwrap();
    if account != f.system || f.works.len() >= 3 {
        fail(
            app,
            "unexpected account or too many admitted fixture probes",
        );
        return Task::none();
    }
    let Some(kind) = f.next.take() else {
        fail(app, "no next fixture probe");
        return Task::none();
    };
    let serial = f.works.len();
    let quota_at = if kind == Kind::Seed {
        f.baseline.saturating_sub(schedule::STALE_AFTER + 1_000)
    } else {
        now_ms().saturating_sub(200)
    };
    let (seen, observed) = oneshot::channel();
    let (ready, wait) = oneshot::channel();
    let (close, closed) = oneshot::channel();
    let mut work = Work {
        kind,
        owner,
        id: id.clone(),
        observed_at: quota_at,
        input_held: false,
        returned: false,
        ready: Some(ready),
        close: Some(close),
    };
    if kind == Kind::Seed {
        work.ready
            .take()
            .unwrap()
            .send(())
            .expect("new gate receiver");
        work.close
            .take()
            .unwrap()
            .send(())
            .expect("new close receiver");
    }
    let raw = frames(
        account.provider,
        kind == Kind::Sparse,
        f.context.scenario == Scenario::Malformed,
    );
    f.works.push(work);
    f.phase = match kind {
        Kind::Seed => "seeding-synthetic-old-callback",
        Kind::NewB => "successor-quota-pending",
        Kind::Recovery => "recovery-quota-pending",
        _ => "quota-pending",
    };
    receipt(
        serde_json::json!({"event":"actual_admission","scene":f.context.scene,
        "serial":serial,"kind":format!("{kind:?}"),"owner":owner,"id":id,
        "synthetic_quota_input_time":quota_at,"logical_admission":f.logical_now,
        "production_ticket_available":false,"serial_is_fixture_task_label_only":true}),
    );
    let complete = cosmic::task::future(async move {
        let result = async {
            seen.send(())
                .map_err(|_| "original fixture observer dropped")?;
            wait.await
                .map_err(|_| "original fixture auth/model gate dropped")?;
            closed
                .await
                .map_err(|_| "original fixture close gate dropped")?;
            // Finite synthetic callback timing: the seed has a stale matching
            // baseline; later callbacks model the old stamp AFTER both gates.
            // No real auth, provider close or observation clock was exercised.
            let at = if kind == Kind::Seed {
                quota_at
            } else {
                now_ms()
            };
            receipt(
                serde_json::json!({"event":"synthetic_old_post_close_callback",
                "serial":serial,"observed_at":at,"not_provider_clock_evidence":true}),
            );
            original_callback(raw, at)
        }
        .await;
        Message::AccountNativeFixture(Event::Completion {
            serial,
            owner,
            id,
            result,
        })
    });
    let input_held = cosmic::task::future(async move {
        observed.await.expect("original fixture input observer");
        Message::AccountNativeFixture(Event::InputHeld(serial))
    });
    Task::batch([complete, input_held])
}

fn effects(app: &Cagents) -> serde_json::Value {
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    let row = app.account_ui.account(&f.system.id()).unwrap();
    let pending: Option<usize> = None; // Old production has no per-admission ticket.
    let outstanding: Vec<_> = f
        .works
        .iter()
        .enumerate()
        .filter(|(_, work)| !work.returned)
        .map(|(serial, _)| serial)
        .collect();
    let defaults: Vec<_> = app
        .default_account_choices()
        .into_iter()
        .filter(|(provider, _)| *provider == f.system.provider)
        .flat_map(|(_, choices)| {
            choices
                .into_iter()
                .map(|choice| serde_json::json!({"id":choice.id,"selected":choice.selected}))
        })
        .collect();
    let automation: Vec<_> = app
        .automation_account_choices()
        .into_iter()
        .map(|choice| choice.id)
        .collect();
    serde_json::json!({"snapshot":row.snapshot,"identity":row.identity,"present":row.present,
        "failure":row.failure,"schedule":format!("{:?}",row.schedule),"pending_serial":pending,
        "production_ticket_available":false,"fixture_unreturned_serials":outstanding,
        "schedule_in_flight":row.schedule.in_flight(app.account_ui.now_ms),
        "catalog":app.account_ui.catalog(f.system.provider),"row_visible":app.account_ui.shown_in_switcher(row),
        "accounts_table_row":row.present || !row.account.is_system(),
        "provider_chip":app.provider_chip(f.system.provider, app.account_ui.now_ms, false).is_some(),
        "failure_count":row.schedule.failures(),"manual_ready":row.schedule.manual_ready(app.account_ui.now_ms),
        "focus_before_backoff":row.schedule.should_probe(Trigger::Focus, app.account_ui.now_ms.saturating_add(schedule::BACKOFF_BASE - 1), true),
        "focus_at_backoff":row.schedule.should_probe(Trigger::Focus, app.account_ui.now_ms.saturating_add(schedule::BACKOFF_BASE), true),
        "known_account":app.known_account(&f.system.id()),"explicit_default":app.default_account(f.system.provider),
        "default_controls":defaults,"automation_choices":automation,
        "automatic_alternate":app.automatic_account(&f.source),"now":app.account_ui.now_ms})
}

fn release(app: &mut Cagents, kind: Kind) {
    let f = app.account_ui.native_fixture.as_mut().unwrap();
    let Some(work) = f
        .works
        .iter_mut()
        .find(|work| work.kind == kind && work.input_held && !work.returned)
    else {
        fail(app, "release needs the original held fixture input");
        return;
    };
    let ready = work.ready.take();
    let close = work.close.take();
    let released = match (ready, close) {
        (Some(ready), Some(close)) => {
            let ready = ready.send(()).is_ok();
            let close = close.send(()).is_ok();
            ready && close
        }
        _ => false,
    };
    if !released {
        fail(app, "original gate was absent or dropped");
    }
}

fn advance(app: &mut Cagents) {
    if app
        .account_ui
        .native_fixture
        .as_ref()
        .is_some_and(|f| f.startup.is_some())
    {
        advance_startup(app);
        return;
    }
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    if f.error.is_some() {
        return;
    }
    let phase = f.phase;
    let scenario = f.context.scenario;
    let quota_ready = f.works.last().is_some_and(|work| work.input_held);
    match (phase, scenario) {
        ("quota-held", Scenario::Sparse | Scenario::Malformed) | ("recovery-held", _) => {
            let kind = f.works.last().unwrap().kind;
            app.account_ui.native_fixture.as_mut().unwrap().phase = "completing-current";
            release(app, kind);
        }
        ("before-sparse-reading-replaced", _)
        | ("before-empty-answer-published-without-usable-quota", _)
            if effects(app)["accounts_table_row"] == true =>
        {
            let f = app.account_ui.native_fixture.as_mut().unwrap();
            f.logical_now = now_ms()
                .max(f.logical_now)
                .saturating_add(schedule::MANUAL_DEBOUNCE + 1);
            f.next = Some(Kind::Recovery);
            f.phase = "recovery-ready";
            app.account_ui.now_ms = f.logical_now;
        }
        ("quota-held", Scenario::Delayed) => {
            let system = f.system.clone();
            let mut recipe = LaunchRecipe {
                program: system.provider.executable().into(),
                arguments: Vec::new(),
                working_directory: f.context.root.join("public-project"),
                environment: Default::default(),
                unset_environment: Default::default(),
            };
            if let Some((name, value)) = system.probe_environment() {
                recipe
                    .environment
                    .insert(name.into(), value.display().to_string());
            }
            let report = match system.provider {
                Provider::Claude => LiveReport::Claude(account::claude::parse_rate_limit_event(&serde_json::json!({
                    "status":"rejected","unifiedWindows":{"five_hour":{"utilization":1.0}}})).unwrap()),
                Provider::Codex => LiveReport::CodexLimits(account::codex::parse_update(&serde_json::json!({"rateLimits":{
                    "limitId":"codex","rateLimitReachedType":"fixture-exhausted","primary":{"usedPercent":100.0,"windowDurationMins":300}}})).unwrap()),
                _ => unreachable!(),
            };
            drop(app.account_reported(system.provider, &recipe, vec![(report, now_ms())]));
            app.account_ui.native_fixture.as_mut().unwrap().phase = "newer-live-limited-held";
        }
        ("newer-live-limited-held", _) => {
            app.account_ui.native_fixture.as_mut().unwrap().phase = "completing-delayed";
            release(app, Kind::Delayed);
        }
        ("quota-held", Scenario::Ticket) if quota_ready => {
            let f = app.account_ui.native_fixture.as_mut().unwrap();
            f.logical_now = now_ms()
                .max(f.logical_now)
                .saturating_add(schedule::IN_FLIGHT_LIMIT + 1);
            f.next = Some(Kind::NewB);
            f.phase = "logical-expiry-successor-ready";
            app.account_ui.now_ms = f.logical_now;
        }
        ("successor-held", _) => {
            app.account_ui.native_fixture.as_mut().unwrap().phase = "delivering-obsolete-a";
            release(app, Kind::OldA);
        }
        ("before-obsolete-a-settled-successor", _) => {
            app.account_ui.native_fixture.as_mut().unwrap().phase = "delivering-current-b";
            release(app, Kind::NewB);
        }
        _ => {
            receipt(
                serde_json::json!({"event":"step_refused","phase":phase,"reason":"no matching observed gate/transition"}),
            );
            return;
        }
    }
    receipt(
        serde_json::json!({"event":"fixture_step","phase":app.account_ui.native_fixture.as_ref().unwrap().phase,
        "logical_clock_staging":true,"effects":effects(app)}),
    );
}

fn completed(app: &mut Cagents, serial: usize, outcome: &Outcome, before: &serde_json::Value) {
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    let kind = f.works[serial].kind;
    let after = effects(app);
    let row = app.account_ui.account(&f.system.id()).unwrap();
    let accepted_answer = matches!(outcome, Outcome::Answered { identity, snapshot, catalog }
        if row.snapshot.as_ref() == Some(snapshot) && row.identity.as_ref() == identity.as_ref()
            && catalog.as_ref() == app.account_ui.catalog(f.system.provider));
    let expected = match kind {
        Kind::Sparse => {
            accepted_answer
                && after["failure"].is_null()
                && before["snapshot"] != after["snapshot"]
                && row
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.windows.is_empty())
        }
        Kind::Delayed => {
            accepted_answer
                && before["snapshot"] != after["snapshot"]
                && before["snapshot"]["status"] == serde_json::json!(UsageStatus::Limited)
                && row.snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot.source == UsageSource::Probe && snapshot.status == UsageStatus::Ok
                })
        }
        Kind::OldA => {
            accepted_answer
                && after["schedule_in_flight"] == false
                && f.works
                    .iter()
                    .any(|work| work.kind == Kind::NewB && !work.returned)
        }
        _ => {
            accepted_answer
                && row.present
                && row.failure.is_none()
                && row.snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot.status == UsageStatus::Ok && !snapshot.windows.is_empty()
                })
        }
    };
    receipt(
        serde_json::json!({"event":"actual_reducer_completion","variant":"before",
        "serial":serial,"kind":format!("{kind:?}"),
        "fixture_quota_input_time":f.works[serial].observed_at,"produced_outcome":outcome,
        "before":before,"after":after,"expected_before_consequence_observed":expected,
        "synthetic_callback_adapter":true,"qualified":false}),
    );
    if !expected {
        fail(
            app,
            "old parser/callback/reducer consequence differed from source expectation",
        );
        return;
    }
    let no_row = after["accounts_table_row"] == false;
    let f = app.account_ui.native_fixture.as_mut().unwrap();
    f.works[serial].returned = true;
    match kind {
        Kind::Seed => {
            f.phase = "ready-refresh";
            f.logical_now = now_ms();
            f.next = Some(match f.context.scenario {
                Scenario::Sparse | Scenario::Malformed => Kind::Sparse,
                Scenario::Delayed => Kind::Delayed,
                Scenario::Ticket => Kind::OldA,
                Scenario::Startup => unreachable!("startup constructs no completion adapter"),
            });
        }
        Kind::Sparse => {
            f.phase = if f.context.scenario == Scenario::Malformed {
                "before-empty-answer-published-without-usable-quota"
            } else {
                "before-sparse-reading-replaced"
            }
        }
        Kind::Delayed => f.phase = "before-delayed-answer-replaced-live-rejection",
        Kind::OldA => f.phase = "before-obsolete-a-settled-successor",
        _ => f.phase = "complete",
    }
    app.account_ui.now_ms = f.logical_now.max(now_ms());
    receipt(serde_json::json!({"event":"scene_state","variant":"before",
        "scene":f.context.scene,"phase":f.phase,"qualified":false,
        "native_control_gap":no_row.then_some(
            "Original System row is hidden; no programmatic Refresh can count as user recovery")}));
}

/// Keep actual Accounts widgets/reducer and presentation input; omit every
/// operational App subscription/action. Global shortcuts are not exercised.
pub(in crate::app) fn update(app: &mut Cagents, message: Message) -> Task<Message> {
    let message = match message {
        Message::UtilityPaneUi(message) => *message,
        message => message,
    };
    let startup = app
        .account_ui
        .native_fixture
        .as_ref()
        .is_some_and(|f| f.startup.is_some());
    if let Message::AccountUi(Action::Refresh(id)) = &message {
        receipt(serde_json::json!({"event":"refresh_callback","id":id,
            "phase":app.account_ui.native_fixture.as_ref().unwrap().phase,
            "input_origin_requires_native_controller_evidence":true}));
    }
    let task = match message {
        Message::AccountUi(
            action @ (Action::Refresh(_)
            | Action::SignIn(_)
            | Action::SignInWithCode(_)
            | Action::Popover(_)
            | Action::Tick),
        ) if startup => startup_action(app, action),
        Message::AccountUi(Action::Probed { .. } | Action::LoginReady { .. })
        | Message::AccountNativeFixture(_)
            if startup =>
        {
            fail(
                app,
                "Before startup constructs no provider/login/completion outcome",
            );
            Task::none()
        }
        Message::AccountNativeFixture(Event::InputHeld(serial)) => {
            let f = app.account_ui.native_fixture.as_mut().unwrap();
            if let Some(work) = f.works.get_mut(serial) {
                work.input_held = true;
                if !work.returned && work.kind != Kind::Seed {
                    f.phase = match work.kind {
                        Kind::NewB => "successor-held",
                        Kind::Recovery => "recovery-held",
                        _ => "quota-held",
                    };
                }
                receipt(
                    serde_json::json!({"event":"fixture_input_held","serial":serial,"phase":f.phase,
                        "synthetic_callback_input_only":true,"not_matched_collector_observation":true}),
                );
            } else {
                fail(app, "unknown fixture input observer");
            }
            Task::none()
        }
        Message::AccountNativeFixture(Event::Completion {
            serial,
            owner,
            id,
            result,
        }) => {
            let original = app
                .account_ui
                .native_fixture
                .as_ref()
                .unwrap()
                .works
                .get(serial)
                .is_some_and(|work| work.owner == owner && work.id == id && !work.returned);
            if !original {
                fail(app, "callback did not belong to its original fixture task");
                return Task::none();
            }
            let outcome = match result {
                Ok(outcome) => outcome,
                Err(error) => {
                    fail(app, &error);
                    return Task::none();
                }
            };
            let before = effects(app);
            let produced = outcome.clone();
            // Exact old production completion, no added ticket or gate.
            let task = app.update_account_ui(Action::Probed { owner, id, outcome });
            completed(app, serial, &produced, &before);
            task
        }
        Message::AccountUi(
            action @ (Action::Refresh(_)
            | Action::ManualReady
            | Action::Popover(_)
            | Action::OpenPane
            | Action::Menu(_)
            | Action::AddMenu(_)
            | Action::Sort(_)
            | Action::PreviewRow { .. }),
        ) => app.update_account_ui(action),
        Message::KeyEvent { event, captured } => {
            if matches!(
                &event,
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::F8),
                    ..
                }
            ) && !captured
            {
                advance(app);
                Task::none()
            } else if let Some(task) = app.account_key_event(&event) {
                task
            } else if let Some(task) = app.chrome_key_event(&event) {
                task
            } else {
                if let keyboard::Event::ModifiersChanged(modifiers) = event {
                    app.keyboard_modifiers = modifiers;
                }
                Task::none()
            }
        }
        Message::ChromeUi(
            action @ (chrome_ui::Action::ToggleNotices
            | chrome_ui::Action::ShowNoticesFor(_)
            | chrome_ui::Action::Close
            | chrome_ui::Action::HoverTab(_)
            | chrome_ui::Action::TabsScrolled(_)),
        ) => app.update_chrome(action),
        Message::FocusCard(card) if app.card_state(card).is_some_and(|c| c.existence.is_open()) => {
            app.focus_workspace_card(card)
        }
        Message::Viewport { generation, event } if generation == app.workspace_view_generation => {
            if matches!(event, WorkspaceViewportEvent::Reordered { .. }) {
                Task::none()
            } else {
                let (update, synchronize) = update_workspace_viewport(&mut app.workspace, event);
                app.apply_viewport(update, synchronize, false)
            }
        }
        Message::Frame(at) => match app.frame_clock.elapsed(at) {
            Some(elapsed) => {
                let update = app.workspace.tick(elapsed);
                app.apply_viewport(update, true, false)
            }
            None => Task::none(),
        },
        Message::WindowResized(width, height) => {
            if width.is_finite() && width > 0.0 {
                app.window_width = width;
            }
            if height.is_finite() && height > 0.0 {
                app.window_height = height;
            }
            app.content_width_changed()
        }
        Message::WindowScaleFactorChanged(scale) => {
            if scale.is_finite() && scale > 0.0 {
                app.window_scale_factor = scale;
            }
            Task::none()
        }
        Message::WindowEdgeState {
            window,
            filled,
            right_edge_tiled,
        } if app.core.main_window_id() == Some(window) => {
            app.window_state_observed = true;
            app.window_filled = filled;
            app.right_edge_tiled = right_edge_tiled;
            app.content_width_changed()
        }
        Message::WindowFilled(window, filled) if app.core.main_window_id() == Some(window) => {
            app.window_filled = filled;
            app.content_width_changed()
        }
        Message::RefreshUi(refresh_ui::Action::Focused(window, focused))
            if app.core.main_window_id() == Some(window) =>
        {
            let regained = focused && !app.refresh_ui.focused;
            app.refresh_ui.focused = focused;
            if startup {
                drop(app.accounts_focus_changed(focused, regained));
                startup_receipt(app, "startup_before_window_focus");
            }
            Task::none()
        }
        Message::WindowCloseRequested(window) if app.core.main_window_id() == Some(window) => {
            iced::window::close(window).map(cosmic::Action::App)
        }
        Message::WindowClosed(window) if app.core.main_window_id() == Some(window) => iced::exit(),
        Message::DismissNotice(id) => {
            app.notices.dismiss(id);
            Task::none()
        }
        Message::DismissStatus | Message::DismissAllNotices => {
            app.status = None;
            app.notices.clear();
            Task::none()
        }
        _ => Task::none(),
    };
    if let Some(notice) = app.status.take() {
        app.notices.record(notice);
    }
    Task::batch([task, app.sync_chrome()])
}

pub(in crate::app) fn subscription(app: &Cagents) -> Subscription<Message> {
    let mut subscriptions = vec![event::listen_with(|event, status, window| match event {
        iced::Event::Keyboard(event) => Some(Message::KeyEvent {
            event,
            captured: matches!(status, event::Status::Captured),
        }),
        iced::Event::Window(window::Event::CloseRequested) => {
            Some(Message::WindowCloseRequested(window))
        }
        iced::Event::Window(window::Event::Closed) => Some(Message::WindowClosed(window)),
        iced::Event::Window(window::Event::Focused) => Some(Message::RefreshUi(
            refresh_ui::Action::Focused(window, true),
        )),
        iced::Event::Window(window::Event::Unfocused) => Some(Message::RefreshUi(
            refresh_ui::Action::Focused(window, false),
        )),
        iced::Event::Window(window::Event::Rescaled(scale)) => {
            Some(Message::WindowScaleFactorChanged(scale))
        }
        iced::Event::Window(event) => window_geometry_message(&event),
        _ => None,
    })];
    if let Some(window) = app.core.main_window_id()
        && app.workspace.is_animating()
    {
        subscriptions.push(workspace_widget::animation_frames(window, Message::Frame));
    }
    Subscription::batch(subscriptions)
}
