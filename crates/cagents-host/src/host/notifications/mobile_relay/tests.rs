use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    time::SystemTime,
};

use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;
use crate::{
    agent::Provider,
    mobile::pairing::{Pairing, PublicKey, Request},
    notifications::{Content, Evidence},
};

struct Fixture {
    _root: TempDir,
    journal: Journal,
    keys: AuthorizedKeys,
    settings: Settings,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let keys = AuthorizedKeys::in_home(root.path());
        let pairing = Pairing::start(
            keys.clone(),
            Path::new("/fixture/cagents"),
            root.path(),
            SystemTime::now(),
        )
        .unwrap();
        let key = PublicKey::ed25519(&[7; 32]);
        pairing
            .allow(
                &Request {
                    key: key.clone(),
                    name: "Fixture phone".into(),
                },
                "/fixture/cagents daemon bridge --mobile",
            )
            .unwrap();
        let journal = Journal::open(&root.path().join("agent-attention.sqlite3")).unwrap();
        Self {
            _root: root,
            journal,
            keys,
            settings: Settings {
                enabled: true,
                phone: Some(key.fingerprint()),
                ..Settings::default()
            },
        }
    }

    fn record(&mut self, kind: Kind) -> crate::notifications::Notice {
        let mut event = Event::session(
            TerminalId::new(),
            Provider::Claude,
            PathBuf::from("/private/workspace"),
            Evidence {
                key: "private-evidence".into(),
                kind,
            },
        );
        event.content = Some(Content {
            conversation: "Private title".into(),
            preview: "Private conversation text".into(),
            request: None,
        });
        self.journal.record(event, false).unwrap().unwrap()
    }
}

/// An isolated HTTP peer for the production POST codec. Only tests can use
/// this transport, whose origin is always the listener's loopback address.
fn endpoint(status: &'static str, headers: &'static str) -> (Url, JoinHandle<Value>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let response = thread::spawn(move || {
        let stop = Instant::now() + HTTP_TIMEOUT;
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(peer) => break peer,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < stop =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fixture did not receive a POST: {error}"),
            }
        };
        stream.set_read_timeout(Some(HTTP_TIMEOUT)).unwrap();
        let mut input = BufReader::new(&mut stream);
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        assert_eq!(line, "POST / HTTP/1.1\r\n");
        let mut size = None;
        loop {
            line.clear();
            assert_ne!(input.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                size = Some(value.trim().parse().unwrap());
            }
        }
        let mut bytes = vec![0; size.unwrap()];
        input.read_exact(&mut bytes).unwrap();
        write!(
            stream,
            "HTTP/1.1 {status}\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    });
    (url, response)
}

fn local_http() -> Http {
    Http(ureq::Agent::new_with_config(Http::configuration().build()))
}

#[test]
fn delivers_one_minimal_alert_for_the_latest_unread_conversation() {
    let mut fixture = Fixture::new();
    fixture.record(Kind::Waiting); // startup backlog
    let now = Instant::now();
    let mut delivery = Delivery::new(now);
    let never = |_| panic!("unexpected delivery");
    let settings = &fixture.settings.clone();
    delivery
        .poll(now, &mut fixture.journal, settings, never)
        .unwrap();

    fixture.record(Kind::Waiting);
    delivery
        .poll(now, &mut fixture.journal, settings, never)
        .unwrap();
    let latest = fixture.record(Kind::Failed);
    let dismissed = fixture.record(Kind::Waiting);
    fixture.journal.acknowledge(dismissed.id).unwrap();
    fixture.record(Kind::Succeeded); // completion is another opt-in
    fixture.record(Kind::Interrupted);
    let (server, response) = endpoint("200 OK", "");
    fixture.settings.server = server;
    let topic = fixture
        .keys
        .notification_topic(
            fixture.settings.phone.as_deref().unwrap(),
            &fixture.settings.server,
        )
        .unwrap();
    delivery
        .poll(
            now + COALESCE,
            &mut fixture.journal,
            &fixture.settings,
            |alert| publish(&local_http(), &fixture.settings, &fixture.keys, alert),
        )
        .unwrap();
    assert_eq!(
        response.join().unwrap(),
        json!({
            "topic": topic,
            "title": "Cagents",
            "message": GENERIC,
            "click": format!("cagents://conversation/{}", latest.event.source_session.unwrap()),
        })
    );
    delivery
        .poll(
            now + COALESCE + MIN_INTERVAL,
            &mut fixture.journal,
            settings,
            never,
        )
        .unwrap();
    assert!(
        fixture
            .journal
            .list()
            .unwrap()
            .iter()
            .any(|note| note.id == latest.id && !note.acknowledged)
    );

    fixture.settings.detail = Detail::Titles;
    fixture.settings.finished = true;
    let alert =
        Alert::from_event(fixture.record(Kind::Succeeded).event, &fixture.settings).unwrap();
    let (server, response) = endpoint("200 OK", "");
    fixture.settings.server = server;
    publish(&local_http(), &fixture.settings, &fixture.keys, alert).unwrap();
    let payload = response.join().unwrap();
    assert_eq!(payload["title"], "Private title");
    assert_eq!(payload["message"], GENERIC);
    assert!(!payload.to_string().contains("Private conversation text"));
}

#[test]
fn bounds_retries_rechecks_acknowledgement_and_expires_a_rate_limited_batch() {
    let mut fixture = Fixture::new();
    let now = Instant::now();
    let mut delivery = Delivery::new(now);
    let never = |_| panic!("unexpected delivery");
    let settings = fixture.settings.clone();
    delivery
        .poll(now, &mut fixture.journal, &settings, never)
        .unwrap();
    fixture.record(Kind::Waiting);
    delivery
        .poll(now, &mut fixture.journal, &settings, never)
        .unwrap();
    let http = local_http();
    let mut at = now + COALESCE;
    for _ in 0..MAX_ATTEMPTS {
        let (server, response) = endpoint("503 Service Unavailable", "");
        delivery
            .poll(at, &mut fixture.journal, &settings, |alert| {
                http.post(&server, "fixture", alert)
            })
            .unwrap();
        response.join().unwrap();
        delivery
            .poll(
                at + Duration::from_secs(1),
                &mut fixture.journal,
                &settings,
                never,
            )
            .unwrap();
        at = delivery.next_send;
    }
    delivery
        .poll(at, &mut fixture.journal, &settings, never)
        .unwrap();

    let note = fixture.record(Kind::Failed);
    delivery
        .poll(at, &mut fixture.journal, &settings, never)
        .unwrap();
    let (server, response) = endpoint("429 Too Many Requests", "Retry-After: 45\r\n");
    delivery
        .poll(at + COALESCE, &mut fixture.journal, &settings, |alert| {
            http.post(&server, "fixture", alert)
        })
        .unwrap();
    response.join().unwrap();
    assert_eq!(delivery.next_send, at + COALESCE + Duration::from_secs(45));
    let new_notice = fixture.record(Kind::Failed);
    delivery
        .poll(at + MAX_AGE, &mut fixture.journal, &settings, never)
        .unwrap();
    let (server, response) = endpoint("200 OK", "");
    delivery
        .poll(
            at + COALESCE + Duration::from_secs(45),
            &mut fixture.journal,
            &settings,
            |alert| http.post(&server, "fixture", alert),
        )
        .unwrap();
    assert_eq!(
        response.join().unwrap()["click"],
        format!(
            "cagents://conversation/{}",
            new_notice.event.source_session.unwrap(),
        )
    );
    assert!(
        fixture
            .journal
            .list()
            .unwrap()
            .iter()
            .any(|current| current.id == note.id && !current.acknowledged)
    );

    let at = at + Duration::from_secs(60);
    let note = fixture.record(Kind::Failed);
    delivery
        .poll(at, &mut fixture.journal, &settings, never)
        .unwrap();
    let (server, response) = endpoint("503 Service Unavailable", "");
    delivery
        .poll(at + COALESCE, &mut fixture.journal, &settings, |alert| {
            http.post(&server, "fixture", alert)
        })
        .unwrap();
    response.join().unwrap();
    fixture.journal.acknowledge(note.id).unwrap();
    delivery
        .poll(
            at + COALESCE + MIN_INTERVAL,
            &mut fixture.journal,
            &settings,
            never,
        )
        .unwrap();
}

#[test]
fn worker_starts_off_and_rechecks_opt_out_before_a_test() {
    let mut fixture = Fixture::new();
    let root = fixture._root.path();
    let settings = root.join("settings.json");
    let journal = root.join("agent-attention.sqlite3");
    let (tests, commands) = bounded(1);
    let (stop, stopping) = bounded(1);
    let handle = Handle(tests);
    let worker_settings = settings.clone();
    let (server, response) = endpoint("200 OK", "");
    let keys = fixture.keys.clone();
    let worker = thread::spawn(move || {
        let http = local_http();
        run(
            &worker_settings,
            &journal,
            commands,
            stopping,
            |settings, alert| {
                let mut fixture = settings.clone();
                fixture.server = server.clone();
                publish(&http, &fixture, &keys, alert)
            },
        );
    });
    let test = || handle.test().unwrap().recv_timeout(HTTP_TIMEOUT).unwrap();
    assert!(matches!(test(), TestOutcome::Unavailable(_)));
    let write = |settings_value: &Settings| {
        let mut file = tempfile::NamedTempFile::new_in(root).unwrap();
        serde_json::to_writer(&mut file, settings_value).unwrap();
        file.persist(&settings).unwrap();
    };
    write(&fixture.settings);
    assert_eq!(test(), TestOutcome::Sent);
    let payload = response.join().unwrap();
    assert_eq!(payload["title"], "Cagents");
    assert_eq!(payload["message"], "Mobile notifications are working");
    assert!(payload.get("click").is_none());
    fixture.settings.enabled = false;
    write(&fixture.settings);
    assert!(matches!(test(), TestOutcome::Unavailable(message) if message.contains("Enable")));
    stop.send(()).unwrap();
    worker.join().unwrap();
}

#[test]
fn refuses_redirects_without_resending_the_topic() {
    let (server, response) = endpoint(
        "307 Temporary Redirect",
        "Location: http://127.0.0.1:1/refused\r\n",
    );
    let result = local_http().post(
        &server,
        "fixture",
        Alert {
            message: GENERIC,
            title: None,
            click: None,
        },
    );
    assert!(matches!(result, Err(Failure { retry: None, .. })));
    response.join().unwrap();
}

#[test]
fn resolver_refuses_a_connection_to_a_local_address() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = Url::parse(&format!("https://{}", listener.local_addr().unwrap())).unwrap();
    let result = Http::new().post(
        &server,
        "fixture",
        Alert {
            message: GENERIC,
            title: None,
            click: None,
        },
    );
    assert!(result.is_err());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn an_opt_out_and_reenable_discards_a_pending_retry() {
    let mut fixture = Fixture::new();
    let root = fixture._root.path();
    let config = root.join("settings.json");
    let journal = root.join("agent-attention.sqlite3");
    Settings::update(&config, |settings| {
        *settings = fixture.settings.clone();
        Ok(())
    })
    .unwrap();
    let (tests, commands) = bounded(1);
    let (stop, stopping) = bounded(1);
    let (sent, received) = bounded(4);
    let worker_config = config.clone();
    let worker = thread::spawn(move || {
        run(&worker_config, &journal, commands, stopping, |_, alert| {
            sent.try_send(alert.message).unwrap();
            if alert.message == GENERIC {
                return Err(Failure {
                    message: "fixture outage",
                    retry: Some(Duration::ZERO),
                });
            }
            Ok(())
        });
    });
    // The explicit test answers after the daemon has read its initial journal
    // checkpoint. The following notice is fresh for this worker.
    let reply = Handle(tests.clone()).test().unwrap();
    assert_eq!(
        reply.recv_timeout(Duration::from_secs(20)).unwrap(),
        TestOutcome::Sent
    );
    assert_eq!(
        received.try_recv().unwrap(),
        "Mobile notifications are working"
    );
    fixture.record(Kind::Waiting);
    assert_eq!(
        received.recv_timeout(Duration::from_secs(20)).unwrap(),
        GENERIC
    );
    for enabled in [false, true] {
        Settings::update(&config, |settings| {
            settings.enabled = enabled;
            Ok(())
        })
        .unwrap();
    }
    assert_eq!(
        received.recv_timeout(MIN_INTERVAL + POLL * 2),
        Err(crossbeam_channel::RecvTimeoutError::Timeout)
    );
    stop.send(()).unwrap();
    worker.join().unwrap();
}
