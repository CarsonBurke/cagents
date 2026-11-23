//! The desktop's text-cursor blink, read from the XDG settings portal.
//!
//! The GNOME and GTK portal backends serve GSettings, and COSMIC's portal
//! configuration falls back to GTK's for settings, so on those desktops the
//! blink arrives under `org.gnome.desktop.interface`: whether the cursor blinks
//! at all, the length of one on-and-off cycle, and how long after the last
//! input it keeps blinking. Without a portal or those keys, cursors keep the
//! shared default.
use ashpd::{desktop::settings::Settings, zvariant::Value};
use cosmic::iced::{
    advanced::text::Blink,
    futures::{SinkExt, Stream, StreamExt},
};
use std::time::Duration;

const NAMESPACE: &str = "org.gnome.desktop.interface";
const ENABLED: &str = "cursor-blink";
const CYCLE: &str = "cursor-blink-time";
const TIMEOUT: &str = "cursor-blink-timeout";

/// The blink keys the desktop has stated so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Keys {
    enabled: Option<bool>,
    cycle_ms: Option<i64>,
    timeout_s: Option<i64>,
}

impl Keys {
    /// Records one setting, returning whether it was a blink key.
    fn apply(&mut self, key: &str, value: &Value<'_>) -> bool {
        match key {
            ENABLED => self.enabled = boolean(value),
            CYCLE => self.cycle_ms = integer(value),
            TIMEOUT => self.timeout_s = integer(value),
            _ => return false,
        }
        true
    }

    /// The blink these keys describe, or `None` when the desktop stated none
    /// of them. A missing key keeps its part of [`Blink::DEFAULT`].
    fn blink(&self) -> Option<Blink> {
        if *self == Self::default() {
            return None;
        }
        if self.enabled == Some(false) {
            return Some(Blink::STEADY);
        }
        // GSettings bounds the cycle to 100-2500 ms and the timeout to at
        // least a second; a portal serving something else is held to that.
        // The cycle is one lit phase and one dark one.
        let interval = self.cycle_ms.map_or(Blink::DEFAULT.interval, |cycle| {
            Duration::from_millis(cycle.clamp(100, 2500).unsigned_abs() / 2)
        });
        let timeout = self.timeout_s.map_or(Blink::DEFAULT.timeout, |seconds| {
            Duration::from_secs(seconds.clamp(1, i64::from(i32::MAX)).unsigned_abs())
        });
        Some(Blink { interval, timeout })
    }
}

fn unwrapped<'v>(value: &'v Value<'v>) -> &'v Value<'v> {
    match value {
        Value::Value(inner) => unwrapped(inner),
        value => value,
    }
}

fn boolean(value: &Value<'_>) -> Option<bool> {
    match unwrapped(value) {
        Value::Bool(value) => Some(*value),
        _ => None,
    }
}

fn integer(value: &Value<'_>) -> Option<i64> {
    match *unwrapped(value) {
        Value::I32(value) => Some(value.into()),
        Value::U32(value) => Some(value.into()),
        Value::I64(value) => Some(value),
        Value::U64(value) => i64::try_from(value).ok(),
        Value::I16(value) => Some(value.into()),
        Value::U16(value) => Some(value.into()),
        _ => None,
    }
}

/// The desktop's blink once at start, when it states one, then again each
/// time it changes. Without a session bus it ends at once; with a bus but no
/// settings portal, the read fails and no change ever arrives, so cursors keep
/// the default.
pub fn stream() -> impl Stream<Item = Option<Blink>> {
    cosmic::iced::stream::channel(1, async |mut output| {
        let settings = match Settings::new().await {
            Ok(settings) => settings,
            Err(error) => {
                tracing::debug!(%error, "no session bus; text cursors keep the default blink");
                return;
            }
        };
        // Listen before reading, so a change between the two is not lost.
        let changes = settings.receive_setting_changed().await;
        let mut keys = Keys::default();
        match settings.read_all(&[NAMESPACE]).await {
            Ok(namespaces) => {
                for (key, value) in namespaces.get(NAMESPACE).into_iter().flatten() {
                    keys.apply(key, value);
                }
            }
            Err(error) => {
                tracing::debug!(%error, "no settings portal answered; text cursors keep the default blink");
            }
        }
        let mut sent = keys.blink();
        if sent.is_some() && output.send(sent).await.is_err() {
            return;
        }
        let Ok(mut changes) = changes else {
            return;
        };
        while let Some(setting) = changes.next().await {
            if setting.namespace() != NAMESPACE || !keys.apply(setting.key(), setting.value()) {
                continue;
            }
            let blink = keys.blink();
            if blink != sent {
                sent = blink;
                if output.send(blink).await.is_err() {
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(settings: &[(&str, Value<'static>)]) -> Keys {
        let mut keys = Keys::default();
        for (key, value) in settings {
            keys.apply(key, value);
        }
        keys
    }

    #[test]
    fn a_desktop_that_states_nothing_leaves_the_default() {
        assert_eq!(keys(&[]).blink(), None);
        assert_eq!(keys(&[("gtk-theme", Value::from("Adwaita"))]).blink(), None);
    }

    #[test]
    fn blinking_turned_off_holds_every_cursor_steady() {
        let blink = keys(&[(ENABLED, Value::from(false)), (CYCLE, Value::from(1200))]).blink();
        assert_eq!(blink, Some(Blink::STEADY));
    }

    #[test]
    fn the_cycle_and_timeout_set_the_phase_and_the_rest() {
        let blink = keys(&[
            (ENABLED, Value::from(true)),
            (CYCLE, Value::from(1200)),
            (TIMEOUT, Value::from(4)),
        ])
        .blink();
        assert_eq!(
            blink,
            Some(Blink {
                interval: Duration::from_millis(600),
                timeout: Duration::from_secs(4),
            })
        );
    }

    #[test]
    fn a_missing_key_keeps_its_default_and_strays_are_bounded() {
        let blink = keys(&[(CYCLE, Value::from(5))]).blink().unwrap();
        assert_eq!(blink.interval, Duration::from_millis(50));
        assert_eq!(blink.timeout, Blink::DEFAULT.timeout);
        let blink = keys(&[(TIMEOUT, Value::from(0))]).blink().unwrap();
        assert_eq!(blink.interval, Blink::DEFAULT.interval);
        assert_eq!(blink.timeout, Duration::from_secs(1));
    }

    #[test]
    fn values_wrapped_in_a_variant_are_read() {
        let wrapped = Value::Value(Box::new(Value::from(false)));
        assert_eq!(keys(&[(ENABLED, wrapped)]).blink(), Some(Blink::STEADY));
    }
}
