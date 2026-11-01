//! Inert Accounts scenes in the real native application. Only explicit
//! development opt-in admits these synthetic collectors; no provider, home,
//! cache, login, transport, daemon or watchdog behavior is established here.
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
    "claude-catalog",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Sparse,
    Malformed,
    Delayed,
    Ticket,
    Startup,
    Catalog,
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
            format!("cagents-1938-native-{scene}-wide-after"),
            format!("cagents-1938-native-{scene}-narrow-before"),
            format!("cagents-1938-native-{scene}-narrow-after"),
        ]
        .iter()
        .any(|name| root.file_name() == Some(std::ffi::OsStr::new(name)))
        || roots
            .iter()
            .any(|path| path.parent() != Some(root.as_path()))
        || roots.iter().collect::<HashSet<_>>().len() != names.len()
    {
        return Err("fixture needs distinct direct children of its exact /tmp/cagents-1938-native-<scene>-<wide|narrow>-<before|after> namespace".into());
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
        "catalog" => Scenario::Catalog,
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
    ticket: ProbeTicket,
    observed_at: Millis,
    quota_seen: bool,
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
    catalog: Option<CatalogScene>,
}

struct StartupAdmission {
    id: AccountId,
    owner: u64,
    ticket: ProbeTicket,
    trigger: Trigger,
}

struct Startup {
    original_accounts: Vec<(AccountId, u64)>,
    original_revision: u64,
    trigger: Option<Trigger>,
    admissions: Vec<StartupAdmission>,
    sign_in_owner: Option<u64>,
}

/// Internal in-process gate notification only; no wire, commands or arbitrary
/// workload/coordinate payload. The original task owns each sender/receiver.
#[derive(Debug, Clone)]
pub enum Event {
    QuotaObserved(usize),
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
    if context.scenario == Scenario::Catalog {
        return install_catalog(app, context);
    }
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
    state.registry_ready = true;
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
        phase: "initial-collector",
        works: Vec::new(),
        error: None,
        startup: None,
        catalog: None,
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
        "transport_executed":false,"logical_expiry_only":true}),
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
        // Never mask the production readiness gate with the quota fixture's
        // phase/Manual-only filter. A rejected automatic attempt must reach
        // probe_policy_ready; an unexpected actual admission is a failure.
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
            let pending = row.pending_probe.as_ref().and_then(|ticket| {
                startup.admissions.iter().position(|admission| {
                    admission.id == row.id
                        && admission.owner == row.owner
                        && admission.ticket.same(ticket)
                })
            });
            serde_json::json!({"id":row.id,"owner":row.owner,"present":row.present,
            "in_flight":row.schedule.in_flight(app.account_ui.now_ms),
            "queued":format!("{:?}",row.queued),"pending_original_serial":pending,
            "sign_in_preparing":app.account_ui.preparing_logins.contains(&row.owner),
            "sign_in_terminal":row.sign_in_terminal.is_some(),
            "awaiting_login":row.schedule.awaiting_login(app.account_ui.now_ms)})
        })
        .collect();
    let admissions: Vec<_> = startup
        .admissions
        .iter()
        .enumerate()
        .map(|(serial, admission)| {
            serde_json::json!({"serial":serial,"id":admission.id,"owner":admission.owner,
            "trigger":format!("{:?}",admission.trigger)})
        })
        .collect();
    serde_json::json!({"registry_ready":app.account_ui.registry_ready,
        "registry_revision":app.account_ui.registry.revision(),
        "selected_account":f.system.id(),"untouched_account":f.source.id(),
        "original_registry_revision":startup.original_revision,
        "original_account_count":startup.original_accounts.len(),
        "account_count":app.account_ui.accounts.len(),"rows":rows,"admissions":admissions,
        "original_ids_and_owners_conserved":startup_accounts(app)==startup.original_accounts,
        "store_present":app.account_ui.store.is_some(),"cache_present":app.account_ui.cache.is_some(),
        "terminal_cards":app.durable.cards.iter().filter(|card| matches!(&card.kind, CardKind::Terminal(_))).count(),
        "qualified":false,"provider_outcome":null})
}

fn startup_receipt(app: &Cagents, event: &str) {
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    receipt(
        serde_json::json!({"event":event,"scene":f.context.scene,"phase":f.phase,
        "effects":startup_effects(app),"error":f.error,"boundary":"synthetic in-memory callback and actual reducer admission only",
        "input_origin_requires_native_controller_evidence":true,
        "operational_tasks_polled":false,"provider_executed":false,"qualified":false}),
    );
}

fn install_startup(app: &mut Cagents, context: Context) -> Task<Message> {
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
        _ => unreachable!("only the two explicit startup scenes"),
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
        phase: "startup-unready",
        works: Vec::new(),
        error: None,
        catalog: None,
        startup: Some(Startup {
            original_accounts,
            original_revision,
            trigger: None,
            admissions: Vec::new(),
            sign_in_owner: None,
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
    // A fixture-owned failed callback carries synthetic installed/present
    // metadata. It is not a Store read or a provider discovery result. The real
    // Loaded reducer must retain unknown policy and publish its normal notice.
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
    startup_receipt(app, "startup_installed_unknown_policy");
    let theme = app.settings.app_theme.resolve(app.core.system_theme());
    Task::batch([open, app.sync_chrome(), cosmic::command::set_theme(theme)])
}

fn start_startup(
    app: &mut Cagents,
    account: Account,
    id: AccountId,
    owner: u64,
    ticket: ProbeTicket,
) -> Task<Message> {
    let current = app.account_ui.account(&id).is_some_and(|row| {
        row.account == account
            && row.owner == owner
            && row
                .pending_probe
                .as_ref()
                .is_some_and(|pending| pending.same(&ticket))
    });
    let f = app.account_ui.native_fixture.as_mut().unwrap();
    let startup = f.startup.as_mut().unwrap();
    let Some(trigger) = startup.trigger.take() else {
        fail(app, "startup admission has no original scheduling trigger");
        return Task::none();
    };
    let expected = current
        && match f.phase {
            "startup-manual-dispatch" => {
                id == f.system.id() && trigger == Trigger::Manual && startup.admissions.is_empty()
            }
            "startup-retry-loading" => {
                id == f.source.id() && trigger == Trigger::Focus && startup.admissions.len() == 1
            }
            _ => false,
        };
    // Retain the actual private ticket allocation, never a reconstructed one.
    startup.admissions.push(StartupAdmission {
        id,
        owner,
        ticket,
        trigger,
    });
    startup_receipt(app, "startup_actual_probe_admission");
    if !expected {
        fail(app, "unexpected actual startup probe admission");
    }
    // No collector, outcome, provider future or completion is manufactured.
    Task::none()
}

fn startup_action(app: &mut Cagents, action: Action) -> Task<Message> {
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    if f.error.is_some() {
        return Task::none();
    }
    match action {
        Action::Refresh(id) => {
            if f.phase != "startup-manual-ready" || id != f.system.id() {
                fail(
                    app,
                    "startup Refresh requires the selected original row and phase",
                );
                return Task::none();
            }
            app.account_ui.native_fixture.as_mut().unwrap().phase = "startup-manual-dispatch";
            drop(app.update_account_ui(Action::Refresh(id.clone())));
            let f = app.account_ui.native_fixture.as_ref().unwrap();
            let startup = f.startup.as_ref().unwrap();
            let admitted = f.error.is_none()
                && !app.account_ui.registry_ready
                && startup.admissions.len() == 1
                && app.account_ui.account(&id).is_some_and(|row| {
                    row.pending_probe
                        .as_ref()
                        .is_some_and(|ticket| startup.admissions[0].ticket.same(ticket))
                });
            if !admitted {
                fail(
                    app,
                    "manual Refresh was not admitted while startup policy stayed unknown",
                );
            } else {
                app.account_ui.native_fixture.as_mut().unwrap().phase = "startup-manual-admitted";
            }
            startup_receipt(app, "startup_native_refresh_callback");
        }
        action @ (Action::SignIn(_) | Action::SignInWithCode(_)) => {
            // Both native controls are explicit intent. This scene exercises
            // preparation admission only, not LoginReady, await-login or a
            // terminal. Keep the exact callback kind in the receipt below.
            let (id, device) = match &action {
                Action::SignIn(id) => (id.clone(), false),
                Action::SignInWithCode(id) => (id.clone(), true),
                _ => unreachable!(),
            };
            if f.phase != "startup-manual-admitted" || id != f.system.id() {
                fail(
                    app,
                    "startup Sign in requires the selected original row and phase",
                );
                return Task::none();
            }
            let owner = app.account_ui.account(&id).unwrap().owner;
            if app.account_ui.preparing_logins.contains(&owner) {
                fail(
                    app,
                    "startup Sign in must create its own preparation reservation",
                );
                return Task::none();
            }
            drop(app.update_account_ui(action));
            let admitted = app.account_ui.preparing_logins.contains(&owner)
                && !app.account_ui.registry_ready
                && app.account_ui.account(&id).is_some_and(|row| {
                    row.owner == owner
                        && row.sign_in_terminal.is_none()
                        && !row.schedule.awaiting_login(app.account_ui.now_ms)
                });
            if admitted {
                let f = app.account_ui.native_fixture.as_mut().unwrap();
                f.startup.as_mut().unwrap().sign_in_owner = Some(owner);
                f.phase = "startup-signin-preparation-admitted";
            } else {
                fail(
                    app,
                    "explicit Sign in preparation was not admitted under unknown policy",
                );
            }
            startup_receipt(
                app,
                if device {
                    "startup_native_signin_with_code_preparation_callback"
                } else {
                    "startup_native_signin_preparation_callback"
                },
            );
        }
        action @ (Action::Tick | Action::Popover(_)) => {
            drop(app.update_account_ui(action));
            startup_receipt(app, "startup_scheduling_callback");
        }
        _ => unreachable!("finite startup callbacks only"),
    }
    Task::none()
}

fn advance_startup(app: &mut Cagents) {
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    if f.error.is_some() {
        return;
    }
    match f.phase {
        "startup-unready" => {
            if !app.refresh_ui.focused {
                fail(
                    app,
                    "startup scheduling needs actual native window focus first",
                );
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
            if app.account_ui.registry_ready
                || !startup.admissions.is_empty()
                || startup_accounts(app) != startup.original_accounts
                || app
                    .account_ui
                    .accounts
                    .iter()
                    .any(|row| row.pending_probe.is_some() || row.queued.is_some())
            {
                fail(
                    app,
                    "failed registry read admitted automatic work or changed original rows",
                );
            } else {
                app.account_ui.native_fixture.as_mut().unwrap().phase = "startup-manual-ready";
            }
            startup_receipt(app, "startup_failed_registry_automatic_refused");
        }
        "startup-signin-preparation-admitted" => {
            let count = app.account_ui.accounts.len();
            let prior = app.account_ui.registry.revision();
            let revision = prior.checked_add(1).expect("finite fixture revision");
            let mut value =
                serde_json::to_value(&app.account_ui.registry).expect("typed fixture registry");
            value["revision"] = serde_json::json!(revision);
            let registry: Registry =
                serde_json::from_value(value).expect("same typed fixture policy, later revision");
            app.account_ui.native_fixture.as_mut().unwrap().phase = "startup-retry-loading";
            // Synthetic authoritative callback into the REAL reducer. No
            // actual Store read, ready assignment or registry I/O is implied.
            drop(app.update_account_ui(Action::RegistryLoaded(Ok(registry))));
            let f = app.account_ui.native_fixture.as_ref().unwrap();
            let startup = f.startup.as_ref().unwrap();
            let valid = f.error.is_none()
                && app.account_ui.registry_ready
                && app.account_ui.registry.revision() == revision
                && revision > startup.original_revision
                && prior == startup.original_revision
                && app.account_ui.accounts.len() == count
                && startup_accounts(app) == startup.original_accounts
                && startup.admissions.len() == 2
                && startup.admissions[0].id == f.system.id()
                && startup.admissions[0].trigger == Trigger::Manual
                && startup.admissions[1].id == f.source.id()
                && startup.admissions[1].trigger == Trigger::Focus
                && !startup.admissions[0]
                    .ticket
                    .same(&startup.admissions[1].ticket)
                && startup.admissions.iter().all(|admission| {
                    app.account_ui.account(&admission.id).is_some_and(|row| {
                        row.owner == admission.owner
                            && row
                                .pending_probe
                                .as_ref()
                                .is_some_and(|pending| pending.same(&admission.ticket))
                    })
                })
                && startup
                    .sign_in_owner
                    .is_some_and(|owner| app.account_ui.preparing_logins.contains(&owner));
            if valid {
                app.account_ui.native_fixture.as_mut().unwrap().phase = "startup-complete";
            } else {
                fail(
                    app,
                    "successful same-count later registry did not release the untouched account",
                );
            }
            startup_receipt(app, "startup_same_count_later_revision_admitted");
        }
        _ => startup_receipt(
            app,
            "startup_step_refused_wait_for_native_refresh_or_signin",
        ),
    }
}

fn frames(provider: Provider, sparse: bool, malformed: bool) -> probe::collector_fixture::Frames {
    use probe::collector_fixture::Frames;
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

/// Called only AFTER the actual start_probe issued this owner/id/ticket.
pub(in crate::app) fn start(
    app: &mut Cagents,
    account: Account,
    id: AccountId,
    owner: u64,
    ticket: ProbeTicket,
) -> Task<Message> {
    if app
        .account_ui
        .native_fixture
        .as_ref()
        .is_some_and(|f| f.startup.is_some())
    {
        return start_startup(app, account, id, owner, ticket);
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
    let observations = match account.provider {
        Provider::Claude => vec![quota_at - 1, quota_at, quota_at + 1],
        Provider::Codex => vec![quota_at, quota_at + 1, quota_at + 2],
        _ => unreachable!(),
    };
    let clock = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
        observations,
    )));
    let (seen, observed) = oneshot::channel();
    let (ready, wait) = oneshot::channel();
    let (close, closed) = oneshot::channel();
    let mut work = Work {
        kind,
        owner,
        ticket: ticket.clone(),
        observed_at: quota_at,
        quota_seen: false,
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
        Kind::Seed => "seeding-real-collector",
        Kind::NewB => "successor-quota-pending",
        Kind::Recovery => "recovery-quota-pending",
        _ => "quota-pending",
    };
    receipt(
        serde_json::json!({"event":"actual_admission","scene":f.context.scene,"serial":serial,
        "kind":format!("{kind:?}"),"owner":owner,"id":id,"quota_observation":quota_at,"logical_admission":f.logical_now}),
    );
    let complete = cosmic::task::future(async move {
        let outcome = probe::collector_fixture::held_completion(
            account,
            raw,
            || {
                let observed_at = clock
                    .lock()
                    .expect("fixture clock mutex")
                    .pop_front()
                    .expect("matched original observation");
                receipt(serde_json::json!({"event":"collector_observation","serial":serial,"observed_at":observed_at}));
                observed_at
            },
            seen,
            wait,
            closed,
        )
        .await;
        Message::AccountUi(Action::Probed {
            owner,
            id,
            ticket,
            outcome,
        })
    });
    let quota_seen = cosmic::task::future(async move {
        observed.await.expect("original fixture quota observer");
        Message::AccountNativeFixture(Event::QuotaObserved(serial))
    });
    Task::batch([complete, quota_seen])
}

fn effects(app: &Cagents) -> serde_json::Value {
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    let row = app.account_ui.account(&f.system.id()).unwrap();
    let pending = row
        .pending_probe
        .as_ref()
        .and_then(|ticket| f.works.iter().position(|work| work.ticket.same(ticket)));
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
        "catalog":app.account_ui.catalog(f.system.provider),"row_visible":app.account_ui.shown_in_switcher(row),
        "accounts_table_row":table::listed(row),
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
        .find(|work| work.kind == kind && work.quota_seen && !work.returned)
    else {
        fail(app, "release needs the original observed pending quota");
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
    let quota_ready = f.works.last().is_some_and(|work| work.quota_seen);
    match (phase, scenario) {
        ("quota-held", Scenario::Sparse | Scenario::Malformed) | ("recovery-held", _) => {
            let kind = f.works.last().unwrap().kind;
            app.account_ui.native_fixture.as_mut().unwrap().phase = "completing-current";
            release(app, kind);
        }
        ("failed-retained", _) => {
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
        ("obsolete-a-refused-b-still-held", _) => {
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

fn completed(
    app: &mut Cagents,
    owner: u64,
    ticket: &ProbeTicket,
    id: &AccountId,
    outcome: &Outcome,
    before: &serde_json::Value,
) {
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    let Some(serial) = f
        .works
        .iter()
        .position(|work| work.owner == owner && work.ticket.same(ticket))
    else {
        fail(app, "completion has no original fixture admission");
        return;
    };
    if *id != f.system.id() {
        fail(app, "completion row changed");
        return;
    }
    let kind = f.works[serial].kind;
    let after = effects(app);
    let row = app.account_ui.account(id).unwrap();
    let current_answer = match outcome {
        Outcome::Answered {
            identity,
            snapshot,
            catalog,
        } => {
            snapshot.status == UsageStatus::Ok
                && snapshot.source == UsageSource::Probe
                && snapshot.observed_at == f.works[serial].observed_at
                && !snapshot.windows.is_empty()
                && snapshot.plan.is_some()
                && snapshot.credits.is_some()
                && identity.is_some()
                && row.identity.as_ref() == identity.as_ref()
                && row.snapshot.as_ref() == Some(snapshot)
                && catalog.is_some()
                && app.account_ui.catalog(f.system.provider) == catalog.as_ref()
        }
        _ => false,
    };
    let valid = match kind {
        Kind::Sparse => {
            matches!(outcome, Outcome::Failed(message) if !message.is_empty())
                && before["snapshot"] == after["snapshot"]
                && before["identity"] == after["identity"]
                && before["present"] == after["present"]
                && before["catalog"] == after["catalog"]
                && before["row_visible"] == after["row_visible"]
                && after["accounts_table_row"] == true
                && (f.context.scenario != Scenario::Malformed
                    || (before["snapshot"].is_null()
                        && before["identity"].is_null()
                        && before["present"] == false
                        && after["snapshot"].is_null()
                        && after["identity"].is_null()
                        && after["present"] == false))
                && before["known_account"] == after["known_account"]
                && before["default_controls"] == after["default_controls"]
                && before["automation_choices"] == after["automation_choices"]
                && after["pending_serial"].is_null()
                && !after["failure"].is_null()
                && after["failure_count"] == 1
                && after["focus_before_backoff"] == false
                && after["focus_at_backoff"] == true
                && after["automatic_alternate"].is_null()
        }
        Kind::Delayed => {
            matches!(outcome, Outcome::Answered { snapshot, catalog: Some(catalog), .. }
                if snapshot.status == UsageStatus::Ok && snapshot.source == UsageSource::Probe
                    && snapshot.observed_at == f.works[serial].observed_at
                    && app.account_ui.catalog(f.system.provider) == Some(catalog))
                && before["catalog"] != after["catalog"]
                && before["snapshot"] == after["snapshot"]
                && before["identity"] == after["identity"]
                && app
                    .account_ui
                    .account(id)
                    .unwrap()
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| {
                        s.status == UsageStatus::Limited && s.source == UsageSource::Live
                    })
        }
        Kind::OldA => {
            before == &after
                && app
                    .account_ui
                    .account(id)
                    .unwrap()
                    .pending_probe
                    .as_ref()
                    .is_some_and(|pending| {
                        f.works
                            .iter()
                            .any(|work| work.kind == Kind::NewB && work.ticket.same(pending))
                    })
        }
        _ => {
            current_answer
                && after["present"] == true
                && after["row_visible"] == true
                && after["failure"].is_null()
                && after["pending_serial"].is_null()
                && if f.context.scenario == Scenario::Malformed {
                    // The empty scene has no synthetic exhausted source row.
                    // Recovery must not fabricate a source or an alternate.
                    after["automatic_alternate"].is_null()
                } else {
                    !after["automatic_alternate"].is_null()
                }
        }
    };
    receipt(
        serde_json::json!({"event":"actual_reducer_completion","serial":serial,"kind":format!("{kind:?}"),
        "produced_outcome":outcome,"before":before,"after":after,"source_oracle_satisfied":valid}),
    );
    if !valid {
        fail(app, "actual collector/reducer oracle failed");
        return;
    }
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
                Scenario::Startup => unreachable!("startup never constructs a collector"),
                Scenario::Catalog => unreachable!("catalog never constructs a collector"),
            });
        }
        Kind::Sparse => f.phase = "failed-retained",
        Kind::OldA => f.phase = "obsolete-a-refused-b-still-held",
        _ => f.phase = "complete",
    }
    app.account_ui.now_ms = f.logical_now.max(now_ms());
    receipt(
        serde_json::json!({"event":"scene_state","scene":f.context.scene,"phase":f.phase,"qualified":false}),
    );
}

/// Keep actual Accounts widgets/reducer and presentation input; omit every
/// operational App subscription/action. Global shortcuts are not exercised.
pub(in crate::app) fn update(app: &mut Cagents, message: Message) -> Task<Message> {
    let message = match message {
        Message::UtilityPaneUi(message) => *message,
        message => message,
    };
    let message = if app
        .account_ui
        .native_fixture
        .as_ref()
        .is_some_and(|f| f.context.scenario == Scenario::Catalog)
    {
        match message {
            Message::AccountUi(Action::Loaded(loaded)) => return catalog_loaded(app, loaded),
            Message::AccountUi(_) => return Task::none(),
            Message::ChatUi(action) => return catalog_chat(app, action),
            Message::KeyEvent {
                event:
                    keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(keyboard::key::Named::F8),
                        modifiers,
                        ..
                    },
                captured: false,
            } if modifiers.is_empty() => return queue_catalog(app),
            Message::KeyEvent {
                event:
                    keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(keyboard::key::Named::Escape),
                        modifiers,
                        ..
                    },
                captured: false,
            } if modifiers.is_empty() => {
                if let Some(view) = catalog_observation(app) {
                    if view.picker_open {
                        return catalog_chat(app, chat_ui::Action::ModelPicker(view.target, false));
                    }
                    if view.footer_open {
                        return catalog_chat(
                            app,
                            chat_ui::Action::FooterPicker(view.target, false),
                        );
                    }
                }
                return Task::none();
            }
            message => message,
        }
    } else {
        message
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
        Message::AccountUi(Action::Probed { .. } | Action::LoginReady { .. }) if startup => {
            fail(
                app,
                "startup admits no provider or login-preparation completion",
            );
            Task::none()
        }
        Message::AccountNativeFixture(Event::QuotaObserved(serial)) => {
            let f = app.account_ui.native_fixture.as_mut().unwrap();
            if let Some(work) = f.works.get_mut(serial) {
                work.quota_seen = true;
                if !work.returned && work.kind != Kind::Seed {
                    f.phase = match work.kind {
                        Kind::NewB => "successor-held",
                        Kind::Recovery => "recovery-held",
                        _ => "quota-held",
                    };
                }
                receipt(
                    serde_json::json!({"event":"matched_quota_observed","serial":serial,"phase":f.phase}),
                );
            } else {
                fail(app, "unknown quota observer");
            }
            Task::none()
        }
        Message::AccountUi(Action::Probed {
            owner,
            id,
            ticket,
            outcome,
        }) => {
            let before = effects(app);
            let produced = outcome.clone();
            let original_ticket = ticket.clone();
            let original_id = id.clone();
            let task = app.update_account_ui(Action::Probed {
                owner,
                id,
                ticket,
                outcome,
            });
            completed(
                app,
                owner,
                &original_ticket,
                &original_id,
                &produced,
                &before,
            );
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
                startup_receipt(app, "startup_window_focus");
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

// Matched Before/After data. These are logical observations, not an OS clock
// change or a cache/provider execution. Expiration requires a new reviewed seed.
const CATALOG_CURRENT_AT: Millis = 1_793_577_600_000; // 2026-11-02T00:00:00Z
const CATALOG_WINDOW_END: Millis = 1_793_664_000_000; // 2026-11-03T00:00:00Z
const CATALOG_OLD_AT: Millis = 4_102_444_800_000; // already-known pre-rollback 2100
const CATALOG_EXPECTS_CURRENT: bool = false;

struct CatalogScene {
    card: CardId,
}

fn catalog_capture_valid(now: Millis) -> bool {
    (CATALOG_CURRENT_AT..CATALOG_WINDOW_END).contains(&now)
        && CATALOG_OLD_AT > now.saturating_add(schedule::CLOCK_SLACK)
}

fn catalog_models(current: bool) -> Vec<crate::chat::Model> {
    vec![crate::chat::Model {
        id: if current {
            "fixture-current-model"
        } else {
            "fixture-old-model"
        }
        .into(),
        label: if current {
            "Current catalog model"
        } else {
            "Older cached model"
        }
        .into(),
        reasoning: Vec::new(),
    }]
}

fn catalog_payload() -> Loaded {
    Loaded {
        registry: Ok(Registry::default()),
        restored: Vec::new(),
        catalogs: BTreeMap::from([(
            Provider::Claude,
            Catalog {
                observed_at: CATALOG_CURRENT_AT,
                models: catalog_models(true),
            },
        )]),
        installed: Account::PROVIDERS
            .into_iter()
            .map(|p| (p, p == Provider::Claude))
            .collect(),
        provisional: false,
    }
}

fn catalog_observation(app: &Cagents) -> Option<chat_ui::native_catalog_fixture::Observation> {
    let card = app
        .account_ui
        .native_fixture
        .as_ref()?
        .catalog
        .as_ref()?
        .card;
    chat_ui::native_catalog_fixture::observe(app, card)
}

fn catalog_draft(app: &Cagents) -> serde_json::Value {
    catalog_observation(app).map_or(serde_json::Value::Null, |view| serde_json::json!({
        "card":view.target.card,"generation":view.target.generation,
        "models":view.models,"selected_model":view.selected,"picker_open":view.picker_open,"footer_open":view.footer_open,
        "unstarted_without_client_or_pending_command":view.unstarted,
        "widget_focus_or_pixels_proven":false,
    }))
}

fn install_catalog(app: &mut Cagents, context: Context) -> Task<Message> {
    let directory = context.root.join("public-project");
    let system = Account {
        provider: Provider::Claude,
        home: account::AccountHome::SystemAt {
            path: context.root.join("synthetic-system-never-read"),
        },
    };
    let mut state = State::default();
    state
        .remote_system_homes
        .insert(Provider::Claude, system.resolved_home().unwrap());
    state.accounts = vec![AccountState::new(system.clone())];
    state.registry_ready = true;
    state.installed = Account::PROVIDERS
        .into_iter()
        .map(|p| (p, p == Provider::Claude))
        .collect();
    state.now_ms = CATALOG_OLD_AT;
    // This row was known before the simulated rollback. Cache::catalogs would
    // filter its future stamp from a NEW load; no such producer is claimed.
    state.catalogs.insert(
        Provider::Claude,
        Catalog {
            observed_at: CATALOG_OLD_AT,
            models: catalog_models(false),
        },
    );
    state.native_fixture = Some(Fixture {
        context,
        source: system.clone(),
        system,
        baseline: CATALOG_CURRENT_AT,
        logical_now: CATALOG_OLD_AT,
        next: None,
        phase: "catalog-old-visible",
        works: Vec::new(),
        error: None,
        startup: None,
        catalog: None,
    });
    app.account_ui = state;
    app.active_workspace = Some(directory.clone());
    app.context_inspector_open = false;
    if !catalog_capture_valid(now_ms()) {
        fail(
            app,
            "fixed catalog capture window expired or not reached; use a new reviewed shared seed",
        );
        return Task::none();
    }
    let (card, presentation) = match chat_ui::native_catalog_fixture::mount(app, directory) {
        Ok(value) => value,
        Err(error) => {
            fail(app, &error);
            return Task::none();
        }
    };
    app.account_ui.native_fixture.as_mut().unwrap().catalog = Some(CatalogScene { card });
    if !catalog_observation(app).is_some_and(|v| v.unstarted && v.models == catalog_models(false)) {
        fail(
            app,
            "original mounted draft did not consume the old known catalog",
        );
        return Task::none();
    }
    app.notices.record(TransientNotice::warning("INERT catalog · open the model picker, then F8 supplies one fixed Loaded callback · no provider or cache producer"));
    app.refresh_sidebar_channels();
    app.synchronize_sidebar_selection();
    let sidebar = sidebar::effective_width(app.projects_sidebar_mode(), app.settings.sidebar_width);
    let _ = app
        .workspace
        .resize_viewport((app.content_width() - sidebar).max(0.0));
    receipt(
        serde_json::json!({"event":"catalog_installed","scene":"claude-catalog",
        "capture_window_utc":["2026-11-02T00:00:00Z","2026-11-03T00:00:00Z"],
        "old_observed_at":CATALOG_OLD_AT,"incoming_observed_at":CATALOG_CURRENT_AT,
        "stale_presentation_clock":app.account_ui.now_ms,"draft":catalog_draft(app),
        "cache_provider_or_native_input_executed":false}),
    );
    let theme = app.settings.app_theme.resolve(app.core.system_theme());
    Task::batch([
        presentation,
        app.sync_chrome(),
        cosmic::command::set_theme(theme),
    ])
}

fn queue_catalog(app: &mut Cagents) -> Task<Message> {
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    if f.error.is_some()
        || f.phase != "catalog-old-visible"
        || app.account_ui.now_ms != CATALOG_OLD_AT
        || !catalog_capture_valid(now_ms())
        || !catalog_observation(app).is_some_and(|v| {
            v.unstarted
                && v.picker_open
                && v.selected.is_none()
                && v.models == catalog_models(false)
        })
    {
        fail(
            app,
            "catalog delivery needs its original old-model picker and valid fixed capture window",
        );
        return Task::none();
    }
    app.account_ui.native_fixture.as_mut().unwrap().phase = "catalog-loaded-queued";
    receipt(
        serde_json::json!({"event":"catalog_loaded_queued","draft":catalog_draft(app),
        "input_origin_requires_native_evidence":true}),
    );
    cosmic::task::message(Message::AccountUi(Action::Loaded(catalog_payload())))
}

fn catalog_loaded(app: &mut Cagents, loaded: Loaded) -> Task<Message> {
    let f = app.account_ui.native_fixture.as_ref().unwrap();
    let fresh_event_now = now_ms();
    if f.error.is_some()
        || f.phase != "catalog-loaded-queued"
        || app.account_ui.now_ms != CATALOG_OLD_AT
        || !catalog_capture_valid(fresh_event_now)
        || loaded.catalogs != catalog_payload().catalogs
        || loaded.installed != catalog_payload().installed
        || !loaded.restored.is_empty()
        || loaded.provisional
        || loaded.registry.is_err()
        || app.account_ui.cache.is_some()
        || app.account_ui.store.is_some()
        || !app.account_ui.logins.is_empty()
    {
        fail(
            app,
            "catalog callback was not the original fixed, writer-free Loaded scene",
        );
        return Task::none();
    }
    // Invoke the real Loaded reducer and refresh_chat_drafts. Its unrelated
    // asynchronous work is not polled; this scene claims only model consumption.
    drop(app.update_account_ui(Action::Loaded(loaded)));
    let expected = catalog_models(CATALOG_EXPECTS_CURRENT);
    let expected_at = if CATALOG_EXPECTS_CURRENT {
        CATALOG_CURRENT_AT
    } else {
        CATALOG_OLD_AT
    };
    let correct = app
        .account_ui
        .catalog(Provider::Claude)
        .is_some_and(|c| c.models == expected && c.observed_at == expected_at)
        && catalog_observation(app)
            .is_some_and(|v| v.unstarted && v.picker_open && v.models == expected);
    if !correct {
        fail(
            app,
            "Loaded catalog or actual draft snapshot disagreed with this source side",
        );
        return Task::none();
    }
    app.account_ui.native_fixture.as_mut().unwrap().phase = "catalog-callback-complete";
    receipt(
        serde_json::json!({"event":"catalog_loaded_callback","source":"actual Loaded reducer",
        "payload_is_synthetic":true,"producer_cache_provider_execution":false,
        "fresh_event_now":fresh_event_now,"old_presentation_now":CATALOG_OLD_AT,
        "expected_current_catalog":CATALOG_EXPECTS_CURRENT,"catalog":app.account_ui.catalog(Provider::Claude),
        "draft":catalog_draft(app),"async_operational_tasks_polled":false}),
    );
    app.sync_chrome()
}

fn catalog_chat(app: &mut Cagents, action: chat_ui::Action) -> Task<Message> {
    let Some(view) = catalog_observation(app) else {
        return Task::none();
    };
    if !view.unstarted
        || app
            .account_ui
            .native_fixture
            .as_ref()
            .unwrap()
            .error
            .is_some()
    {
        return Task::none();
    }
    let allowed = match &action {
        chat_ui::Action::ModelPicker(target, _) | chat_ui::Action::FooterPicker(target, _) => {
            *target == view.target
        }
        chat_ui::Action::ModelQuery(target, query) => *target == view.target && query.len() <= 128,
        chat_ui::Action::DraftModel(target, provider, model) => {
            *target == view.target
                && *provider == Provider::Claude
                && model
                    .as_ref()
                    .is_none_or(|id| view.models.iter().any(|m| &m.id == id))
        }
        _ => false,
    };
    if !allowed {
        return Task::none();
    }
    // These exact same-provider picker actions return only normal widget focus
    // work. No Submit, Composer, account/provider switch or global key dispatch.
    let task = app.handle_chat_ui(action);
    receipt(
        serde_json::json!({"event":"catalog_picker_callback","draft":catalog_draft(app),
        "input_origin_requires_native_evidence":true}),
    );
    Task::batch([task, app.sync_chrome()])
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    #[test]
    fn loaded_catalog_refreshes_the_actual_unstarted_draft() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let temporary = tempfile::tempdir().unwrap();
        let models = |id: &str| {
            vec![crate::chat::Model {
                id: id.into(),
                label: id.into(),
                reasoning: Vec::new(),
            }]
        };
        let load = |models, observed_at| {
            Action::Loaded(Loaded {
                registry: Err("inert consumer test; no registry producer".into()),
                restored: Vec::new(),
                catalogs: BTreeMap::from([(
                    Provider::Claude,
                    Catalog {
                        models,
                        observed_at,
                    },
                )]),
                installed: BTreeMap::new(),
                provisional: false,
            })
        };
        drop(app.update_account_ui(load(models("fixture-old"), 1)));
        let (card, presentation) =
            chat_ui::native_catalog_fixture::mount(&mut app, temporary.path().to_path_buf())
                .unwrap();
        drop(presentation);
        assert_eq!(
            chat_ui::native_catalog_fixture::observe(&app, card)
                .unwrap()
                .models,
            models("fixture-old")
        );
        drop(app.update_account_ui(load(models("fixture-current"), 2)));
        let current = chat_ui::native_catalog_fixture::observe(&app, card).unwrap();
        assert!(current.unstarted);
        assert_eq!(current.models, models("fixture-current"));
        drop(app.handle_chat_ui(chat_ui::Action::FooterPicker(current.target, true)));
        assert!(
            chat_ui::native_catalog_fixture::observe(&app, card)
                .unwrap()
                .footer_open
        );
        drop(app.handle_chat_ui(chat_ui::Action::ModelPicker(current.target, true)));
        assert!(
            chat_ui::native_catalog_fixture::observe(&app, card)
                .unwrap()
                .picker_open
        );
        drop(app.handle_chat_ui(chat_ui::Action::ModelPicker(current.target, false)));
        assert!(
            !chat_ui::native_catalog_fixture::observe(&app, card)
                .unwrap()
                .picker_open
        );
        assert!(
            chat_ui::native_catalog_fixture::observe(&app, card)
                .unwrap()
                .unstarted
        );
    }

    #[test]
    fn fixed_catalog_data_refuses_outside_its_matched_capture_window() {
        assert!(!catalog_capture_valid(CATALOG_CURRENT_AT - 1));
        assert!(catalog_capture_valid(CATALOG_CURRENT_AT));
        assert!(catalog_capture_valid(CATALOG_WINDOW_END - 1));
        assert!(!catalog_capture_valid(CATALOG_WINDOW_END));
        assert_eq!(
            catalog_payload().catalogs[&Provider::Claude].observed_at,
            CATALOG_CURRENT_AT
        );
        assert_ne!(catalog_models(false), catalog_models(true));
        assert!(CATALOG_OLD_AT > CATALOG_WINDOW_END + schedule::CLOCK_SLACK);
    }
}
