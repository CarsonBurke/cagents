//! The line protocol between the UI and `cagents-dictate serve`.
//!
//! Each message is one JSON object on its own line: commands on the helper's
//! standard input, events on its standard output. The helper writes
//! [`Event::Hello`] first; a client that finds another [`PROTOCOL`] there
//! stops rather than guess, since an installation can briefly hold a helper
//! from another build. Closing standard input ends the helper.
//!
//! A session's events come in order: [`Event::Listening`] before
//! [`Event::Ready`], each segment's partials before its final, and last one
//! end, [`Event::Stopped`], [`Event::Cancelled`] or an [`Event::Error`] that
//! names the session, with nothing of that session after it.

use serde::{Deserialize, Serialize};

/// The protocol revision both sides must agree on.
pub const PROTOCOL: u32 = 1;

/// A dictation session's number, chosen by the client. Events name it so a
/// late event from a session the client has moved past can be ignored.
pub type SessionId = u64;

/// What the client asks of the helper.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Start listening with `model`. The microphone opens at once, before
    /// the model has loaded, so the first words are not lost. A session
    /// still listening is cancelled first; one that was stopped finishes
    /// first, its last finals and [`Event::Stopped`] ahead of this one's
    /// events.
    Start { session: SessionId, model: String },
    /// Stop listening and transcribe what was heard. [`Event::Final`]s for
    /// the rest of the speech follow, then [`Event::Stopped`].
    Stop { session: SessionId },
    /// Stop listening and discard what has not been transcribed yet; the
    /// session ends with [`Event::Cancelled`], even if it was stopping.
    /// Cancelling a session that has ended does nothing.
    Cancel { session: SessionId },
}

/// What the helper reports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The first line the helper writes.
    Hello { protocol: u32, version: String },
    /// The microphone is open. `input` names it for the person dictating.
    Listening { session: SessionId, input: String },
    /// The model is loaded and transcribing; `load_ms` is how long loading
    /// took, zero when it was already loaded.
    Ready { session: SessionId, load_ms: u64 },
    /// The input level, from 0 (silence) to 1 (full scale), on a decibel
    /// scale so speech moves it visibly. About twenty a second.
    Level { session: SessionId, level: f32 },
    /// Whether the microphone is picking anything up. Sent when that
    /// changes, starting with `false` if a session is silent for a while.
    Signal { session: SessionId, present: bool },
    /// The best transcript so far of the utterance `segment`, which later
    /// events for the same segment replace.
    Partial {
        session: SessionId,
        segment: u32,
        text: String,
    },
    /// The settled transcript of `segment`. Segments arrive in order and
    /// none is skipped, though an empty one may carry no text.
    Final {
        session: SessionId,
        segment: u32,
        text: String,
    },
    /// Every final has been sent and the microphone is closed.
    Stopped { session: SessionId, stats: Stats },
    /// The session was cancelled; nothing more follows for it.
    Cancelled { session: SessionId },
    /// The session ended because of `kind`. Finals already sent stand.
    /// `session` is `None` for a problem outside any session, such as an
    /// unreadable command.
    Error {
        session: Option<SessionId>,
        kind: ErrorKind,
        message: String,
    },
}

/// Why a session ended early.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The system has no microphone.
    NoInputDevice,
    /// A microphone exists but could not be opened: busy, refused, or the
    /// sound server is not running.
    InputUnavailable,
    /// The microphone went away during the session, such as a headset that
    /// was unplugged.
    InputLost,
    /// The model is not installed.
    ModelMissing,
    /// The model's files are there but could not be loaded.
    ModelInvalid,
    /// The client sent something the helper could not understand.
    Protocol,
}

/// How a session went, for measuring.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    /// Audio heard, in milliseconds.
    pub audio_ms: u64,
    /// Time spent transcribing, partials and finals together.
    pub decode_ms: u64,
    /// How many transcriptions ran.
    pub decodes: u32,
    /// From the session's start to its first partial or final.
    pub first_text_ms: Option<u64>,
    /// How long loading the model took, if this session loaded it.
    pub load_ms: Option<u64>,
}

impl Stats {
    /// Decode time per second of audio: below 1 is faster than speech.
    pub fn real_time_factor(&self) -> Option<f64> {
        (self.audio_ms > 0).then(|| self.decode_ms as f64 / self.audio_ms as f64)
    }
}

/// What `cagents-dictate install --json` reports, one per line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InstallEvent {
    /// `done` of `total` bytes are on disk, verified or still downloading.
    Progress { done: u64, total: u64 },
    /// Every file is downloaded and verified.
    Installed { model: String },
    /// The install stopped. What was downloaded is kept, so running it
    /// again resumes.
    Failed { kind: InstallError, message: String },
}

/// Why an install stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallError {
    /// No such model in the catalog.
    UnknownModel,
    /// Another process is installing or removing the same model.
    Busy,
    /// Not enough free disk space.
    NoSpace,
    /// The download failed or was cut off.
    Network,
    /// A file did not match its published checksum.
    Corrupt,
    /// The model directory could not be written.
    Storage,
}

impl Command {
    pub fn to_line(&self) -> String {
        line(self)
    }
}

impl Event {
    pub fn to_line(&self) -> String {
        line(self)
    }

    /// The session an event belongs to, if any.
    pub fn session(&self) -> Option<SessionId> {
        match self {
            Self::Hello { .. } => None,
            Self::Listening { session, .. }
            | Self::Ready { session, .. }
            | Self::Level { session, .. }
            | Self::Signal { session, .. }
            | Self::Partial { session, .. }
            | Self::Final { session, .. }
            | Self::Stopped { session, .. }
            | Self::Cancelled { session } => Some(*session),
            Self::Error { session, .. } => *session,
        }
    }
}

impl InstallEvent {
    pub fn to_line(&self) -> String {
        line(self)
    }
}

fn line(value: &impl Serialize) -> String {
    // These types hold only strings, numbers and enums, which always
    // serialize.
    serde_json::to_string(value).expect("protocol messages serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_and_events_round_trip_as_single_lines() {
        let commands = [
            Command::Start {
                session: 7,
                model: "parakeet-tdt-0.6b-v3".to_owned(),
            },
            Command::Stop { session: 7 },
            Command::Cancel { session: 7 },
        ];
        for command in commands {
            let line = command.to_line();
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<Command>(&line).unwrap(), command);
        }
        let events = [
            Event::Hello {
                protocol: PROTOCOL,
                version: "0.1.3".to_owned(),
            },
            Event::Partial {
                session: 7,
                segment: 2,
                text: "line one\nline two".to_owned(),
            },
            Event::Stopped {
                session: 7,
                stats: Stats {
                    audio_ms: 2000,
                    decode_ms: 100,
                    decodes: 3,
                    first_text_ms: Some(600),
                    load_ms: None,
                },
            },
            Event::Error {
                session: None,
                kind: ErrorKind::Protocol,
                message: "unreadable command".to_owned(),
            },
        ];
        for event in events {
            let line = event.to_line();
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), event);
        }
    }

    #[test]
    fn the_wire_names_are_stable() {
        assert_eq!(
            Command::Stop { session: 1 }.to_line(),
            r#"{"type":"stop","session":1}"#
        );
        assert_eq!(
            Event::Error {
                session: Some(1),
                kind: ErrorKind::NoInputDevice,
                message: String::new(),
            }
            .to_line(),
            r#"{"type":"error","session":1,"kind":"no_input_device","message":""}"#
        );
        assert_eq!(
            InstallEvent::Progress { done: 1, total: 2 }.to_line(),
            r#"{"type":"progress","done":1,"total":2}"#
        );
    }

    #[test]
    fn real_time_factor_is_decode_time_per_audio_time() {
        let stats = Stats {
            audio_ms: 4000,
            decode_ms: 200,
            ..Stats::default()
        };
        assert_eq!(stats.real_time_factor(), Some(0.05));
        assert_eq!(Stats::default().real_time_factor(), None);
    }
}
