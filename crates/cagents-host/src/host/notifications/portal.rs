//! Linux desktop notifications through the freedesktop notification portal.
//!
//! Registration gives the portal this daemon's application ID, and clicks come
//! back either as `org.freedesktop.Application` actions, which carry the
//! compositor's activation token, or as portal signals.
#[cfg(test)]
mod registration_tests;

use std::{sync::Weak, time::Duration};

use ashpd::{
    desktop::notification::{Button, Notification, NotificationProxy, Priority},
    zbus,
    zvariant::OwnedValue,
};
use futures_util::StreamExt;
use uuid::Uuid;

use super::{MAX_ACTIVATION_TOKEN, Shared, batching};
use crate::notifications::{Notice, TestOutcome};

const PORTAL_TIMEOUT: Duration = Duration::from_secs(3);

/// The portal service's owner as the bus reported it changing; `None` means
/// it exited.
pub(super) type PortalOwner = Option<zbus::names::OwnedUniqueName>;
/// The application action a notification's default action invokes, with the
/// stable conversation-group identity as its target. The broker resolves it to
/// the current journal notice. Notification backends deliver `app.` actions
/// to the application's `org.freedesktop.Application` object together with the
/// activation token the notification server minted for the click; without that
/// token a Wayland compositor refuses to raise the window.
const OPEN_ACTION: &str = "open-notification";
/// The portal-signal action used while this daemon cannot receive application
/// actions. Backends such as xdg-desktop-portal-gtk drop the activation token on
/// this path, so the window is routed but may not be raised.
const SIGNAL_OPEN_ACTION: &str = "open";

pub(super) struct Portal {
    proxy: NotificationProxy<'static>,
    listener: tokio::task::JoinHandle<()>,
}

impl Drop for Portal {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

/// What the portal knows of this daemon's application ID. Registration binds
/// the D-Bus connection, and ashpd shares one connection for the process, so
/// the outcome outlives any single `Portal`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Registration {
    #[default]
    Untried,
    /// A `Register` call was sent, but a timeout discarded its reply.
    Unknown,
    Registered,
    /// The portal identifies this connection some other way, such as when no
    /// desktop entry exists for the ID or the portal predates registration.
    Refused,
}

impl Registration {
    fn needs_attempt(self) -> bool {
        matches!(self, Self::Untried | Self::Unknown)
    }

    /// The state after a `Register` attempt that started from `self`.
    fn settle(self, outcome: Result<(), String>) -> Self {
        match outcome {
            Ok(()) => Self::Registered,
            // The portal rejects a second registration of one connection. After
            // a lost reply that registration can only have been ours: this
            // daemon makes no other portal call before registering.
            Err(error)
                if self == Self::Unknown && error.contains("Connection already associated") =>
            {
                Self::Registered
            }
            Err(_) => Self::Refused,
        }
    }
}

/// `org.freedesktop.Application` object path for a desktop ID, per the
/// Desktop Entry specification's D-Bus activation section.
fn application_object_path(desktop_id: &str) -> String {
    format!("/{}", desktop_id.replace('.', "/").replace('-', "_"))
}

/// The activation token in desktop platform data. `desktop-startup-id` is the
/// X11-era key some backends still send alongside, or instead of, the
/// Wayland `activation-token`.
fn platform_activation_token(
    platform_data: &std::collections::HashMap<String, OwnedValue>,
) -> Option<String> {
    ["activation-token", "desktop-startup-id"]
        .into_iter()
        .find_map(|key| String::try_from(platform_data.get(key)?.try_clone().ok()?).ok())
        .filter(|token| !token.is_empty() && token.len() <= MAX_ACTIVATION_TOKEN)
}

/// The notice an `ActivateAction` call opens. Its first parameter is the
/// target the notification declared: its stable conversation-group identity.
fn activated_notice(action: &str, parameter: &[OwnedValue]) -> Option<Uuid> {
    if action != OPEN_ACTION {
        return None;
    }
    let target = match &**parameter.first()? {
        // The target is a variant inside `av`; tolerate one extra boxing.
        ashpd::zvariant::Value::Value(boxed) => &**boxed,
        target => target,
    };
    let target: &str = target.downcast_ref().ok()?;
    Uuid::parse_str(target).ok()
}

/// Whether a portal `ActionInvoked` signal names a notice's open action.
/// Backends that forward application actions as signals keep the `app.`
/// prefix; the notification ID itself identifies the notice.
fn signal_opens_notice(action: &str) -> bool {
    action == SIGNAL_OPEN_ACTION || action.strip_prefix("app.") == Some(OPEN_ACTION)
}

/// Receives notification clicks that backends deliver as application actions.
/// This identity exists only for notification routing, so launching the
/// application or opening files through it is refused explicitly.
struct ApplicationActions(Weak<Shared>);

#[zbus::interface(name = "org.freedesktop.Application", crate = "ashpd::zbus")]
impl ApplicationActions {
    fn activate(
        &self,
        _platform_data: std::collections::HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "cagentsd only activates windows through notification actions".into(),
        ))
    }

    fn open(
        &self,
        _uris: Vec<String>,
        _platform_data: std::collections::HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "cagentsd does not open files".into(),
        ))
    }

    fn activate_action(
        &self,
        action_name: &str,
        parameter: Vec<OwnedValue>,
        platform_data: std::collections::HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        let id = activated_notice(action_name, &parameter).ok_or_else(|| {
            zbus::fdo::Error::InvalidArgs(format!("unknown notification action {action_name:?}"))
        })?;
        let shared = self
            .0
            .upgrade()
            .ok_or_else(|| zbus::fdo::Error::Failed("notifications are stopping".into()))?;
        tracing::debug!(%id, "desktop notification application action received");
        shared.activate(id, platform_activation_token(&platform_data));
        Ok(())
    }
}

/// Own the desktop ID's bus name so notification backends can deliver the
/// clicked notice and its activation token as an application action.
///
/// The name is never queued for: a queued request would leave backends calling
/// whichever process owns it now.
async fn serve_application_actions(
    shared: Weak<Shared>,
    desktop_id: &str,
) -> zbus::Result<zbus::Connection> {
    let connection = zbus::connection::Builder::session()?
        .serve_at(
            application_object_path(desktop_id),
            ApplicationActions(shared),
        )?
        .build()
        .await?;
    // Without queueing, a name another process owns is refused with a reply,
    // not an error; clicks addressed to it would reach that process.
    match connection
        .request_name_with_flags(desktop_id, zbus::fdo::RequestNameFlags::DoNotQueue.into())
        .await?
    {
        zbus::fdo::RequestNameReply::PrimaryOwner | zbus::fdo::RequestNameReply::AlreadyOwner => {
            Ok(connection)
        }
        zbus::fdo::RequestNameReply::InQueue | zbus::fdo::RequestNameReply::Exists => {
            Err(zbus::Error::NameTaken)
        }
    }
}

/// Receives desktop notification clicks for the daemon's lifetime.
///
/// The application-action endpoint is deliberately independent of `Portal`:
/// notices already on screen keep addressing it while a failed delivery
/// reconnects the portal.
#[derive(Default)]
pub(super) struct Activation {
    registration: Registration,
    /// The portal instance that decided `registration`.
    portal: Option<zbus::names::OwnedUniqueName>,
    /// The portal was reached but the bus would not name its owner, so the
    /// next owner change cannot be compared and counts as a replacement.
    portal_unknown: bool,
    actions: Option<zbus::Connection>,
}

impl Activation {
    /// Whether notices can use the application action, which carries the
    /// click's activation token, rather than the portal signal.
    pub(super) fn receives_actions(&self) -> bool {
        self.registration == Registration::Registered && self.actions.is_some()
    }

    /// Forget a registration held by a portal instance that has exited or been
    /// replaced. Returns whether the portal must be reconnected so the new
    /// instance hears `Register` before any other call.
    pub(super) fn portal_changed(&mut self, owner: PortalOwner) -> bool {
        if self.portal_unknown {
            self.portal_unknown = false;
            // The instance may still be the one we registered with, which
            // rejects a second `Register` as already associated; `Unknown`
            // reads that rejection as the registration it is.
            self.registration = match self.registration {
                Registration::Registered => Registration::Unknown,
                _ => Registration::Untried,
            };
            self.portal = None;
            return true;
        }
        if self.portal.is_none() || self.portal == owner {
            return false;
        }
        self.registration = Registration::Untried;
        self.portal = None;
        true
    }

    async fn prepare(
        &mut self,
        shared: &Weak<Shared>,
        application: crate::notifications::Application,
        connection: &zbus::Connection,
    ) -> Result<(), String> {
        let desktop_id = crate::instance::notification_desktop_id(application);
        // A host daemon is not launched from the UI desktop file's cgroup.
        // Registration gives the portal the same application identity as UI.
        if self.registration.needs_attempt() {
            let id: ashpd::AppID = desktop_id
                .parse()
                .map_err(|error| format!("Invalid application identity: {error}"))?;
            let previous = self.registration;
            // A timeout may drop this future after the call was sent.
            self.registration = Registration::Unknown;
            // ashpd's convenience helper also treats GTK_USE_PORTAL=1 as a
            // sandbox and returns Ok without registering. A toolkit preference
            // is not evidence that this host connection has our application ID.
            // Ask the portal on the exact connection that sends notifications;
            // a real sandbox/older portal may refuse and keeps the signal path.
            let options = std::collections::HashMap::<String, OwnedValue>::new();
            let outcome = connection
                .call_method(
                    Some("org.freedesktop.portal.Desktop"),
                    "/org/freedesktop/portal/desktop",
                    Some("org.freedesktop.host.portal.Registry"),
                    "Register",
                    &(id, options),
                )
                .await
                .map(|_| ());
            if let Err(error) = &outcome {
                tracing::debug!(%error, "desktop portal did not register the application");
            }
            self.registration = previous.settle(outcome.map_err(|error| error.to_string()));
        }
        // Backends address application actions to the portal's view of the
        // application ID, which is ours only once registration succeeded.
        if self.registration == Registration::Registered && self.actions.is_none() {
            self.actions = serve_application_actions(shared.clone(), desktop_id)
                .await
                .inspect_err(|error| {
                    tracing::warn!(
                        %error,
                        "cannot receive notification actions; clicks may not raise windows"
                    );
                })
                .ok();
        }
        Ok(())
    }
}

pub(super) async fn connect(
    shared: Weak<Shared>,
    application: crate::notifications::Application,
    activation: &mut Activation,
) -> Result<Portal, String> {
    tokio::time::timeout(PORTAL_TIMEOUT, async move {
        let proxy = NotificationProxy::new()
            .await
            .map_err(|error| error.to_string())?;
        // Construction reads only the version property. GDBus dispatches
        // property getters without the portal's method authorization/identity
        // callback, so Register still precedes every identity-associating call.
        activation
            .prepare(&shared, application, proxy.connection())
            .await?;
        if activation.portal.is_none() {
            // Asks the bus, not the portal, so it cannot affect registration.
            activation.portal = zbus::fdo::DBusProxy::new(proxy.connection())
                .await
                .map_err(|error| error.to_string())?
                .get_name_owner(proxy.destination().to_owned())
                .await
                .ok();
            activation.portal_unknown = activation.portal.is_none();
        }
        let listener = NotificationProxy::new()
            .await
            .map_err(|error| error.to_string())?;
        let (ready, subscribed) = tokio::sync::oneshot::channel();
        let listener = tokio::spawn(async move {
            let subscriptions = async {
                Ok::<_, ashpd::Error>((
                    listener.receive_action_invoked().await?,
                    listener.receive_owner_changed().await?,
                ))
            };
            let (actions, owners) = match subscriptions.await {
                Ok(streams) => streams,
                Err(error) => {
                    let _ = ready.send(Err(error.to_string()));
                    return;
                }
            };
            let _ = ready.send(Ok(()));
            futures_util::pin_mut!(actions);
            futures_util::pin_mut!(owners);
            loop {
                let action = tokio::select! {
                    action = actions.next() => action,
                    owner = owners.next() => {
                        let (Some(owner), Some(shared)) = (owner, shared.upgrade()) else {
                            break;
                        };
                        shared
                            .pending
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .portal_owner = Some(owner.map(Into::into));
                        let _ = shared.wake.try_send(());
                        continue;
                    }
                };
                let Some(action) = action else {
                    break;
                };
                tracing::debug!(
                    id = action.id(),
                    action = action.name(),
                    "desktop notification action received"
                );
                let Some(shared) = shared.upgrade() else {
                    break;
                };
                if !signal_opens_notice(action.name()) {
                    continue;
                }
                let Ok(id) = Uuid::parse_str(action.id()) else {
                    continue;
                };
                // Portal v2 backends append platform data after the target.
                let token = action.parameter().iter().find_map(|parameter| {
                    platform_activation_token(&parameter.try_clone().ok()?.try_into().ok()?)
                });
                shared.activate(id, token);
            }
        });
        let portal = Portal { proxy, listener };
        // Register the action match before adding the first notification. The
        // Portal guard aborts the listener if readiness fails or times out.
        subscribed.await.map_err(|error| error.to_string())??;
        Ok(portal)
    })
    .await
    .map_err(|_| "Desktop notification portal timed out".to_owned())?
}

pub(super) async fn send(
    portal: &Portal,
    notice: &Notice,
    application_action: bool,
) -> Result<(), String> {
    // Content reaching this point has already passed broker privacy policy.
    let message = crate::notifications::presentation::message(&notice.event, true);
    let id = batching::desktop_id(&notice.event.target).to_string();
    let action = if application_action {
        format!("app.{OPEN_ACTION}")
    } else {
        SIGNAL_OPEN_ACTION.to_owned()
    };
    let notification = Notification::new(&message.title)
        .body(message.body.as_str())
        .priority(if message.needs_input {
            Priority::High
        } else {
            Priority::Normal
        })
        .button(Button::new(message.action, action.as_str()).target(id.as_str()))
        .default_action(action.as_str())
        .default_action_target(id.as_str());
    tokio::time::timeout(
        PORTAL_TIMEOUT,
        portal.proxy.add_notification(&id, notification),
    )
    .await
    .map_err(|_| "Desktop notification portal timed out".to_owned())?
    .map_err(|error| format!("Desktop notifications unavailable: {error}"))
}

pub(super) async fn send_test(backend: &Portal) -> TestOutcome {
    let test = Notification::new("cagents notifications work")
        .body("Agent completion and attention alerts appear here.");
    match tokio::time::timeout(
        PORTAL_TIMEOUT,
        backend
            .proxy
            .add_notification("cagents-notification-test", test),
    )
    .await
    {
        Ok(Ok(())) => TestOutcome::Sent,
        Ok(Err(error)) => TestOutcome::Unavailable(error.to_string()),
        Err(_) => TestOutcome::TimedOut,
    }
}

pub(super) async fn remove(proxy: &Portal, id: Uuid) -> Result<(), String> {
    tokio::time::timeout(
        PORTAL_TIMEOUT,
        proxy.proxy.remove_notification(&id.to_string()),
    )
    .await
    .map_err(|_| "Desktop notification portal timed out".to_owned())?
    .map_err(|error| format!("Could not withdraw desktop notification: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(value: &str) -> OwnedValue {
        ashpd::zvariant::Value::from(value).try_into().unwrap()
    }

    #[test]
    fn application_action_target_names_the_clicked_notice() {
        let id = Uuid::new_v4();
        assert_eq!(
            activated_notice(OPEN_ACTION, &[value(&id.to_string())]),
            Some(id)
        );
        // Backends strip the `app.` prefix before calling ActivateAction.
        assert_eq!(
            activated_notice(&format!("app.{OPEN_ACTION}"), &[value(&id.to_string())]),
            None
        );
        assert_eq!(activated_notice("quit", &[value(&id.to_string())]), None);
        assert_eq!(activated_notice(OPEN_ACTION, &[]), None);
        assert_eq!(
            activated_notice(OPEN_ACTION, &[value("not-a-notice")]),
            None
        );
        let number: OwnedValue = ashpd::zvariant::Value::from(7u32).try_into().unwrap();
        assert_eq!(activated_notice(OPEN_ACTION, &[number]), None);
        let boxed: OwnedValue =
            ashpd::zvariant::Value::Value(Box::new(ashpd::zvariant::Value::from(id.to_string())))
                .try_into()
                .unwrap();
        assert_eq!(activated_notice(OPEN_ACTION, &[boxed]), Some(id));
    }

    #[test]
    fn registration_survives_a_lost_reply_but_not_another_identity() {
        let associated = || Err("Connection already associated with an application ID".to_owned());
        assert_eq!(
            Registration::Untried.settle(Ok(())),
            Registration::Registered
        );
        // Only our own earlier Register call can have associated the
        // connection after its reply was lost.
        assert_eq!(
            Registration::Unknown.settle(associated()),
            Registration::Registered
        );
        // Without that call, the portal chose the identity from elsewhere.
        assert_eq!(
            Registration::Untried.settle(associated()),
            Registration::Refused
        );
        assert_eq!(
            Registration::Untried.settle(Err("App info not found".into())),
            Registration::Refused
        );
        assert!(Registration::Unknown.needs_attempt());
        assert!(!Registration::Refused.needs_attempt());
        assert!(!Registration::Registered.needs_attempt());
        // Owning the name alone is not enough: backends address the portal's
        // application ID, so a refused registration keeps the signal path.
        let unowned = Activation {
            registration: Registration::Registered,
            ..Default::default()
        };
        assert!(!unowned.receives_actions());
    }

    #[test]
    fn a_replaced_portal_forgets_the_registration() {
        let name = |name: &str| zbus::names::OwnedUniqueName::try_from(name).unwrap();
        let registered = || Activation {
            registration: Registration::Registered,
            portal: Some(name(":1.1")),
            ..Default::default()
        };
        let mut activation = registered();
        // The instance we registered with reappearing needs nothing.
        assert!(!activation.portal_changed(Some(name(":1.1"))));
        assert_eq!(activation.registration, Registration::Registered);
        for owner in [None, Some(name(":1.9"))] {
            let mut activation = registered();
            assert!(activation.portal_changed(owner));
            assert_eq!(activation.registration, Registration::Untried);
            assert_eq!(activation.portal, None);
        }
        // A portal that was never reached has no registration to lose, and a
        // refusal is retried with a replacement that may accept it.
        assert!(!Activation::default().portal_changed(None));
        let mut refused = Activation {
            registration: Registration::Refused,
            portal: Some(name(":1.1")),
            ..Default::default()
        };
        assert!(refused.portal_changed(Some(name(":1.2"))));
        assert!(refused.registration.needs_attempt());
    }

    #[test]
    fn a_portal_of_unknown_owner_takes_its_first_change_as_a_replacement() {
        let name = |name: &str| zbus::names::OwnedUniqueName::try_from(name).unwrap();
        let mut activation = Activation {
            registration: Registration::Registered,
            portal_unknown: true,
            ..Default::default()
        };
        assert!(activation.portal_changed(Some(name(":1.9"))));
        assert!(!activation.portal_unknown);
        // Re-registering with the same instance must still read as registered.
        assert_eq!(activation.registration, Registration::Unknown);
        assert_eq!(
            activation
                .registration
                .settle(Err("Connection already associated".into())),
            Registration::Registered
        );
        // With the owner looked up again, only a real change counts.
        activation.portal = Some(name(":1.9"));
        assert!(!activation.portal_changed(Some(name(":1.9"))));
    }

    #[test]
    fn portal_signals_open_notices_through_either_action_name() {
        assert!(signal_opens_notice(SIGNAL_OPEN_ACTION));
        assert!(signal_opens_notice(&format!("app.{OPEN_ACTION}")));
        assert!(!signal_opens_notice(OPEN_ACTION));
        assert!(!signal_opens_notice("app.quit"));
    }

    #[test]
    fn platform_data_yields_a_bounded_activation_token() {
        let data = |entries: &[(&str, &str)]| {
            entries
                .iter()
                .map(|(key, token)| ((*key).to_owned(), value(token)))
                .collect::<std::collections::HashMap<_, _>>()
        };
        assert_eq!(
            platform_activation_token(&data(&[
                ("desktop-startup-id", "x11"),
                ("activation-token", "wayland"),
            ])),
            Some("wayland".into())
        );
        assert_eq!(
            platform_activation_token(&data(&[("desktop-startup-id", "x11")])),
            Some("x11".into())
        );
        assert_eq!(platform_activation_token(&data(&[])), None);
        assert_eq!(
            platform_activation_token(&data(&[("activation-token", "")])),
            None
        );
        let oversized = "t".repeat(MAX_ACTIVATION_TOKEN + 1);
        assert_eq!(
            platform_activation_token(&data(&[("activation-token", &oversized)])),
            None
        );
    }

    #[test]
    fn application_object_path_follows_the_desktop_entry_specification() {
        assert_eq!(
            application_object_path("io.github.cagents.Development"),
            "/io/github/cagents/Development"
        );
        assert_eq!(
            application_object_path("org.example.my-app"),
            "/org/example/my_app"
        );
    }
}
