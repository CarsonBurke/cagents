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
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Sparse,
    Malformed,
    Delayed,
    Ticket,
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
    if let Message::AccountUi(Action::Refresh(id)) = &message {
        receipt(serde_json::json!({"event":"refresh_callback","id":id,
            "phase":app.account_ui.native_fixture.as_ref().unwrap().phase,
            "input_origin_requires_native_controller_evidence":true}));
    }
    let task = match message {
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
            app.refresh_ui.focused = focused;
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
