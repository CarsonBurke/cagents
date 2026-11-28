//! A best-effort wake-up from committed attention. The journal stays the
//! source of truth for unread state and is never changed by relay delivery.
use std::{
    io,
    path::{Path, PathBuf},
    sync::mpsc as oneshot,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, after, bounded, never, select_biased};
use notify::{EventKindMask, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use ureq::{
    config::Config,
    http::Uri,
    unversioned::{
        resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver},
        transport::{DefaultConnector, NextTimeout},
    },
};
use url::Url;

use super::journal::Journal;
use crate::{
    mobile::{
        notify::{Detail, Settings, public_address},
        pairing::AuthorizedKeys,
    },
    model::TerminalId,
    notifications::{Event, Kind, Target, TestOutcome},
};

const POLL: Duration = Duration::from_secs(1);
const WATCH_RETRY: Duration = Duration::from_secs(60);
const COALESCE: Duration = Duration::from_secs(2);
const MIN_INTERVAL: Duration = Duration::from_secs(5);
const MAX_AGE: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_ATTEMPTS: u32 = 3;
const GENERIC: &str = "An agent needs you";

type Test = oneshot::Sender<TestOutcome>;

#[derive(Clone)]
pub(in crate::host) struct Handle(Sender<Test>);

impl Handle {
    pub(in crate::host) fn test(&self) -> Result<oneshot::Receiver<TestOutcome>, &'static str> {
        let (answer, receiver) = oneshot::channel();
        self.0.try_send(answer).map_err(|error| match error {
            crossbeam_channel::TrySendError::Full(_) => {
                "A mobile notification test is already queued"
            }
            crossbeam_channel::TrySendError::Disconnected(_) => {
                "The mobile notification worker is unavailable"
            }
        })?;
        Ok(receiver)
    }
}

pub(super) struct Worker {
    handle: Handle,
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub(super) fn start(
        settings: PathBuf,
        journal: PathBuf,
        keys: AuthorizedKeys,
    ) -> io::Result<Self> {
        let (tests, commands) = bounded(1);
        let (stop, stopping) = bounded(1);
        let thread = thread::Builder::new()
            .name("cagentsd-mobile-relay".into())
            .spawn(move || {
                let http = Http::new();
                run(
                    &settings,
                    &journal,
                    commands,
                    stopping,
                    |settings, mut alert| {
                        if alert.title.is_some()
                            && !super::preferences().is_ok_and(|policy| policy.message_previews)
                        {
                            alert.title = None;
                        }
                        publish(&http, settings, &keys, alert)
                    },
                );
            })?;
        Ok(Self {
            handle: Handle(tests),
            stop,
            thread: Some(thread),
        })
    }

    pub(super) fn handle(&self) -> Handle {
        self.handle.clone()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn settings_watch_root(path: &Path) -> Option<&Path> {
    path.ancestors().skip(1).find(|path| path.is_dir())
}

fn watch_settings(path: &Path, wake: &Sender<()>) -> Option<RecommendedWatcher> {
    // Watch the directory because saving settings replaces the file. If the
    // first configuration has not been created, watch its nearest ancestor.
    let root = settings_watch_root(path)?;
    let file = path.to_owned();
    let changed = wake.clone();
    let mut watcher = RecommendedWatcher::new(
        move |event: notify::Result<notify::Event>| {
            if event.as_ref().map_or(true, |event| {
                event.need_rescan() || event.paths.iter().any(|changed| file.starts_with(changed))
            }) {
                let _ = changed.try_send(());
            }
        },
        notify::Config::default()
            .with_event_kinds(EventKindMask::CORE - EventKindMask::MODIFY_META),
    )
    .ok()?;
    watcher.watch(root, RecursiveMode::NonRecursive).ok()?;
    // The parent may have appeared while we were attaching to its ancestor.
    if settings_watch_root(path) != Some(root) {
        let _ = wake.try_send(());
    }
    Some(watcher)
}

fn run(
    settings_path: &Path,
    journal_path: &Path,
    commands: Receiver<Test>,
    stopping: Receiver<()>,
    send: impl Fn(&Settings, Alert) -> Result<(), Failure>,
) {
    let mut settings = None;
    let mut journal = None;
    let mut delivery = Delivery::new(Instant::now());
    let (wake, changed) = bounded(1);
    let watch = || {
        (
            watch_settings(settings_path, &wake),
            Instant::now() + WATCH_RETRY,
        )
    };
    let (mut watching, mut watch_retry) = watch();
    loop {
        let now = Instant::now();
        if watching.is_none() && now >= watch_retry {
            (watching, watch_retry) = watch();
        }
        let next = Settings::load(settings_path)
            .ok()
            .filter(|settings| settings.enabled);
        if next != settings {
            settings = next;
            journal = None;
            delivery.reset();
        }
        if let Some(settings) = &settings {
            if journal.is_none() {
                journal = Journal::open_reader(journal_path).ok();
            }
            if let Some(reader) = &mut journal
                && delivery
                    .poll(now, reader, settings, |alert| {
                        if !Settings::load(settings_path).is_ok_and(|current| &current == settings)
                        {
                            return Err(Failure {
                                message: "Mobile notification settings changed",
                                retry: None,
                            });
                        }
                        send(settings, alert)
                    })
                    .is_err()
            {
                // A read failure cannot authorize a send. Reopen on the next
                // pass. Keep the original expiry and attempt budget.
                journal = None;
            }
        }
        // A healthy, disabled relay parks until a settings change, test, or
        // shutdown. Poll only for active attention or to recover a lost watch.
        let tick = if settings.is_some() {
            after(POLL)
        } else if watching.is_some() {
            never()
        } else {
            after(watch_retry.saturating_duration_since(Instant::now()))
        };
        select_biased! {
            recv(stopping) -> _ => return,
            recv(changed) -> _ => {
                (watching, watch_retry) = watch();
            }
            recv(commands) -> command => {
                let Ok(answer) = command else { return };
                let now = Instant::now();
                let outcome = match Settings::load(settings_path) {
                    Ok(settings) if settings.enabled && now >= delivery.next_send => {
                        let result = send(&settings, Alert {
                            message: "Mobile notifications are working",
                            title: None,
                            click: None,
                        });
                        let wait = result.as_ref().err().and_then(|failure| failure.retry);
                        delivery.throttle(Instant::now(), MIN_INTERVAL.max(wait.unwrap_or_default()));
                        match result {
                            Ok(()) => TestOutcome::Sent,
                            Err(failure) => TestOutcome::Unavailable(failure.message.into()),
                        }
                    }
                    Ok(settings) if settings.enabled => TestOutcome::Unavailable(
                        "The relay is cooling down. Try again shortly.".into(),
                    ),
                    Ok(_) => TestOutcome::Unavailable(
                        "Enable mobile notifications before sending a test".into(),
                    ),
                    Err(error) => TestOutcome::Unavailable(error.into()),
                };
                let _ = answer.send(outcome);
            }
            recv(tick) -> _ => {}
        }
    }
}

struct Delivery {
    /// Notices already covered, including the startup and opt-in backlog.
    through: Option<i64>,
    batch: Option<Batch>,
    next_send: Instant,
}

#[derive(Clone, Copy)]
struct Batch {
    through: i64,
    due: Instant,
    expires: Instant,
    attempts: u32,
}

impl Delivery {
    fn new(now: Instant) -> Self {
        Self {
            through: None,
            batch: None,
            next_send: now,
        }
    }

    fn reset(&mut self) {
        self.through = None;
        self.batch = None;
    }

    fn throttle(&mut self, now: Instant, wait: Duration) {
        self.next_send = now + wait;
    }

    fn poll(
        &mut self,
        now: Instant,
        journal: &mut Journal,
        settings: &Settings,
        send: impl FnOnce(Alert) -> Result<(), Failure>,
    ) -> Result<(), diesel::result::Error> {
        let latest = journal.latest_ordinal()?;
        let Some(mut through) = self.through else {
            self.through = Some(latest);
            return Ok(());
        };
        if let Some(batch) = self.batch
            && now >= batch.expires
        {
            through = batch.through;
            self.through = Some(through);
            self.batch = None;
        }
        if latest <= through {
            self.batch = None;
            return Ok(());
        }
        let mut batch = self.batch.unwrap_or(Batch {
            through: latest,
            due: now + COALESCE,
            expires: now + MAX_AGE,
            attempts: 0,
        });
        // Collect the initial burst. Once it has been attempted, later
        // arrivals get their own window and retry budget.
        if batch.attempts == 0 {
            batch.through = latest;
        }
        let alert = journal
            .unread_events_since(through, batch.through)?
            .into_iter()
            .filter_map(|serialized| Event::from_journal(&serialized).ok())
            .find_map(|event| Alert::from_event(event, settings));
        let Some(alert) = alert else {
            self.through = Some(batch.through);
            self.batch = None;
            return Ok(());
        };
        if now < batch.due || now < self.next_send {
            self.batch = Some(batch);
            return Ok(());
        }
        batch.attempts += 1;
        let result = send(alert);
        let retry_after = result.err().and_then(|failure| failure.retry);
        let mut wait = MIN_INTERVAL.max(retry_after.unwrap_or_default());
        if retry_after.is_some() && batch.attempts < MAX_ATTEMPTS {
            self.batch = Some(batch);
            wait = wait.max(Duration::from_secs(1 << batch.attempts));
        } else {
            self.through = Some(batch.through);
            self.batch = None;
        }
        self.throttle(now, wait);
        Ok(())
    }
}

struct Alert {
    message: &'static str,
    title: Option<String>,
    click: Option<String>,
}

impl Alert {
    fn from_event(event: Event, settings: &Settings) -> Option<Self> {
        if !matches!(event.target, Target::Session(_) | Target::AgentRun(_)) {
            return None;
        }
        match event.evidence.kind {
            Kind::Waiting | Kind::Failed => {}
            Kind::Succeeded if settings.finished => {}
            _ => return None,
        }
        let session = match event.target {
            Target::Session(session) => Some(session),
            _ => event.source_session,
        };
        let title = (settings.detail == Detail::Titles)
            .then_some(event.content)
            .flatten()
            .map(|content| content.conversation)
            .filter(|title| !title.is_empty());
        Some(Self {
            message: GENERIC,
            title,
            click: session.map(|session: TerminalId| format!("cagents://conversation/{session}")),
        })
    }
}

#[derive(Debug)]
struct Failure {
    message: &'static str,
    retry: Option<Duration>,
}

fn publish(
    http: &Http,
    settings: &Settings,
    keys: &AuthorizedKeys,
    alert: Alert,
) -> Result<(), Failure> {
    let topic = settings
        .phone
        .as_deref()
        .ok_or("Choose a paired phone for mobile notifications")
        .and_then(|phone| keys.notification_topic(phone, &settings.server))
        .map_err(|message| Failure {
            message,
            retry: None,
        })?;
    http.post(&settings.server, &topic, alert)
}

/// Validate the exact addresses ureq hands to its connector. No separate
/// DNS preflight can be bypassed by a second resolution.
#[derive(Debug)]
struct PublicResolver;

impl Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let mut addresses = self.empty();
        for &address in &DefaultResolver::default().resolve(uri, config, timeout)? {
            if public_address(address.ip()) {
                addresses.push(address);
            }
        }
        if addresses.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(addresses)
    }
}

struct Http(ureq::Agent);

impl Http {
    fn configuration() -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
        ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(HTTP_TIMEOUT))
            .max_idle_connections(0)
    }

    fn new() -> Self {
        Self(ureq::Agent::with_parts(
            Self::configuration().https_only(true).build(),
            DefaultConnector::default(),
            PublicResolver,
        ))
    }

    fn post(&self, server: &Url, topic: &str, alert: Alert) -> Result<(), Failure> {
        #[derive(Serialize)]
        struct Payload<'a> {
            topic: &'a str,
            title: &'a str,
            message: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            click: Option<String>,
        }
        // ntfy accepts JSON at its origin. Keeping the topic in the body keeps
        // it out of request URLs and ordinary request/access logs.
        let json = serde_json::to_vec(&Payload {
            topic,
            // ntfy otherwise uses a title containing the subscription URL.
            title: alert.title.as_deref().unwrap_or("Cagents"),
            message: alert.message,
            click: alert.click,
        })
        .expect("the relay payload is serializable");
        // A relay can echo the secret in its response headers. Even library
        // debug diagnostics on this thread must omit request and response data.
        let response =
            tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
                self.0
                    .post(server.as_str())
                    .header("Content-Type", "application/json")
                    .send(&json)
            })
            .map_err(|_| Failure {
                message: "Could not reach the mobile notification relay",
                retry: Some(Duration::ZERO),
            })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let retry = if status.as_u16() == 429 {
            let seconds = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(30);
            Some(Duration::from_secs(seconds.min(900)))
        } else {
            status.is_server_error().then_some(Duration::ZERO)
        };
        Err(Failure {
            message: "The mobile notification relay refused the request",
            retry,
        })
    }
}

#[cfg(test)]
mod tests;
