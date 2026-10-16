//! Real private D-Bus calls exercise registration before notification delivery.
use super::*;
use crate::process::ProcessExecutor;
use std::collections::HashMap;

#[derive(Default)]
struct Calls {
    registered: Option<String>,
    registrations: usize,
    deliveries: Vec<(String, String)>,
}

struct Registry {
    calls: Arc<Mutex<Calls>>,
    refuse: bool,
}

#[zbus::interface(name = "org.freedesktop.host.portal.Registry", crate = "ashpd::zbus")]
impl Registry {
    fn register(
        &self,
        app_id: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        assert_eq!(
            app_id,
            crate::instance::notification_desktop_id(crate::notifications::Application::Cagents)
        );
        let mut calls = self.calls.lock().unwrap();
        calls.registrations += 1;
        if self.refuse {
            return Err(zbus::fdo::Error::Failed(
                "Can't manually register a Flatpak application".into(),
            ));
        }
        assert!(calls.registered.is_none());
        calls.registered = Some(header.sender().unwrap().as_str().to_owned());
        Ok(())
    }
}

struct Notifications {
    calls: Arc<Mutex<Calls>>,
    refuse: bool,
}

#[zbus::interface(name = "org.freedesktop.portal.Notification", crate = "ashpd::zbus")]
impl Notifications {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }

    fn add_notification(
        &self,
        id: &str,
        notification: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let mut calls = self.calls.lock().unwrap();
        assert_eq!(
            calls.registrations, 1,
            "actual Register must precede AddNotification"
        );
        if !self.refuse {
            assert_eq!(
                calls.registered.as_deref(),
                Some(header.sender().unwrap().as_str()),
                "Register must bind the exact notification sender"
            );
        }
        let action = String::try_from(notification["default-action"].try_clone().unwrap()).unwrap();
        calls.deliveries.push((id.to_owned(), action));
        Ok(())
    }
}

fn isolated(name: &str) -> bool {
    let name = name
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap_or(name);
    const MARKER: &str = "CAGENTS_NOTIFICATION_REGISTRATION_TEST";
    if std::env::var_os(MARKER).as_deref() == Some(std::ffi::OsStr::new(name)) {
        return false;
    }
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    for leaf in ["home", "run", "config", "data", "cache"] {
        crate::secure_file::prepare_private_directory(&root.path().join(leaf)).unwrap();
    }
    let mut command = crate::process::CommandSpec::new("/usr/bin/dbus-run-session")
        .arg("--")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"]);
    for (key, _) in std::env::vars_os() {
        command = command.env_remove(key);
    }
    command = command
        .env(MARKER, name)
        .env("PATH", "/usr/bin:/bin")
        // This is the real ashpd shortcut that used to return a false success.
        .env("GTK_USE_PORTAL", "1")
        .env("HOME", root.path().join("home"))
        .env("XDG_RUNTIME_DIR", root.path().join("run"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CACHE_HOME", root.path().join("cache"));
    command.timeout = Some(Duration::from_secs(20));
    let output = crate::process::SystemProcessExecutor
        .execute(&command, &crate::repository::CancellationToken::new())
        .unwrap();
    assert!(
        output.success,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("test result: ok. 1 passed; 0 failed")
            && stdout.contains("notification-registration-body-complete"),
        "selected body must actually complete: {stdout}"
    );
    println!("{stdout}");
    true
}

fn exercise(refuse: bool) {
    assert_eq!(std::env::var("GTK_USE_PORTAL").unwrap(), "1");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let calls = Arc::new(Mutex::new(Calls::default()));
            let server = zbus::connection::Builder::session()
                .unwrap()
                .name("org.freedesktop.portal.Desktop")
                .unwrap()
                .serve_at(
                    "/org/freedesktop/portal/desktop",
                    Registry {
                        calls: calls.clone(),
                        refuse,
                    },
                )
                .unwrap()
                .serve_at(
                    "/org/freedesktop/portal/desktop",
                    Notifications {
                        calls: calls.clone(),
                        refuse,
                    },
                )
                .unwrap()
                .build()
                .await
                .unwrap();
            let (wake, _receiver) = mpsc::channel(1);
            let shared = Arc::new(Shared {
                pending: Mutex::default(),
                presence: Mutex::default(),
                snapshot: Mutex::default(),
                wake,
            });
            let mut activation = Activation::default();
            let portal = connect(
                Arc::downgrade(&shared),
                crate::notifications::Application::Cagents,
                &mut activation,
            )
            .await
            .unwrap();
            assert_eq!(
                activation.registration,
                if refuse {
                    Registration::Refused
                } else {
                    Registration::Registered
                }
            );
            assert_eq!(activation.receives_actions(), !refuse);
            let notice = Notice {
                id: Uuid::new_v4(),
                event: Event::session(
                    TerminalId::new(),
                    crate::agent::Provider::Codex,
                    "/private/project".into(),
                    crate::notifications::Evidence {
                        key: "completed".into(),
                        kind: crate::notifications::Kind::Succeeded,
                    },
                ),
                delivered: false,
                acknowledged: false,
                window: None,
            };
            send(&portal, &notice, activation.receives_actions())
                .await
                .unwrap();
            let group = batching::desktop_id(&notice.event.target);
            {
                let actual = calls.lock().unwrap();
                assert_eq!(actual.registrations, 1);
                assert_eq!(
                    actual.deliveries,
                    [(
                        group.to_string(),
                        if refuse {
                            SIGNAL_OPEN_ACTION.to_owned()
                        } else {
                            format!("app.{OPEN_ACTION}")
                        }
                    )]
                );
            }
            if !refuse {
                let desktop_id = crate::instance::notification_desktop_id(
                    crate::notifications::Application::Cagents,
                );
                server
                    .call_method(
                        Some(desktop_id),
                        application_object_path(desktop_id),
                        Some("org.freedesktop.Application"),
                        "ActivateAction",
                        &(
                            OPEN_ACTION,
                            vec![value(&group.to_string())],
                            HashMap::<String, OwnedValue>::new(),
                        ),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    Vec::from(shared.pending.lock().unwrap().actions.clone()),
                    [(group, None)]
                );
            }
            drop(portal);
            drop(activation);
            server.close().await.unwrap();
        });
    println!("notification-registration-body-complete");
}

fn value(text: &str) -> OwnedValue {
    ashpd::zvariant::Value::from(text).try_into().unwrap()
}

#[test]
fn gtk_portal_preference_registers_real_sender_before_delivery_and_routes_action() {
    if isolated(concat!(
        module_path!(),
        "::gtk_portal_preference_registers_real_sender_before_delivery_and_routes_action"
    )) {
        return;
    }
    exercise(false);
}

#[test]
fn refused_sandbox_registration_keeps_portal_signal_action() {
    if isolated(concat!(
        module_path!(),
        "::refused_sandbox_registration_keeps_portal_signal_action"
    )) {
        return;
    }
    exercise(true);
}
