//! Local dictation has its own process and draft ownership. It never uses a
//! provider session, sends a prompt or follows the selected workspace device.
mod capture;
pub mod model;
mod provision;
mod runner;
pub mod setup;

use crate::process::CancellationToken;
use std::{
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use uuid::Uuid;

const MAX_TRANSCRIPT_BYTES: usize = 32 * 1024;
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// The UI retains and maps the bookmark itself through ordinary composer edits.
/// This ticket only addresses that existing owner; a late result has no means
/// to select another draft, change attachment identity, or send the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket<T> {
    pub target: T,
    pub draft_revision: u64,
    pub bookmark: Uuid,
    operation: Uuid,
}
impl<T> Ticket<T> {
    pub fn new(target: T, draft_revision: u64, bookmark: Uuid) -> Self {
        Self {
            target,
            draft_revision,
            bookmark,
            operation: Uuid::new_v4(),
        }
    }
    pub fn operation(&self) -> Uuid {
        self.operation
    }
}
impl<T: PartialEq> Ticket<T> {
    pub fn belongs_to(&self, current: &Self) -> bool {
        self == current
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Preparing,
    Recording,
    Transcribing,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("Another dictation is already active")]
    Busy,
    #[error("Local dictation needs a verified speech runtime and model")]
    Unavailable,
    #[error("Dictation was cancelled")]
    Cancelled,
    #[error("Speech model download failed; check the connection and retry setup")]
    SetupDownload,
    #[error("Speech model verification failed; retry setup to fetch verified files")]
    SetupIntegrity,
    #[error("Speech model setup could not save files; check available disk space and access")]
    SetupStorage,
    #[error("The microphone could not record audio; check the desktop input device and access")]
    Microphone,
    #[error("No complete microphone audio was recorded")]
    NoAudio,
    #[error("The recording exceeded its sixty-second limit")]
    AudioLimit,
    #[error("Local transcription failed; retry after checking speech model setup")]
    Inference,
    #[error("The local recognizer returned an invalid or oversized result")]
    InvalidResult,
    #[error("No speech was detected")]
    NoSpeech,
    #[error("The microphone or recognizer is still stopping; dictation remains unavailable")]
    CleanupPending,
}

/// Result text deliberately has no Debug implementation or diagnostic path.
pub struct Transcript<T> {
    pub ticket: Ticket<T>,
    pub text: String,
}

struct Signals {
    stop: CancellationToken,
    cancel: CancellationToken,
}
/// Unique UI owner. Closing or retiring its card drops this control and pushes
/// cancellation, including while a worker is blocked in process I/O or inference.
pub struct Control(Arc<Signals>);
impl Control {
    /// Finish capture and transcribe its complete bounded clip. This does not
    /// interrupt the agent or cancel a transcription already in progress.
    pub fn stop_recording(&self) {
        self.0.stop.cancel();
    }
    pub fn cancel(&self) {
        self.0.cancel.cancel();
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Verification runs off the UI thread before recording is offered. There is
/// intentionally no constructor that guesses a host whisper/sherpa executable.
pub struct Prepared(Arc<model::Installation>);
impl Prepared {
    /// Inputs are separately downloaded official archives, not a marker or
    /// arbitrary pre-existing executable/model directory. No network is used.
    pub fn install(
        root: &Path,
        runtime: impl io::Read,
        model: impl io::Read,
        cancellation: &CancellationToken,
    ) -> Result<Self, Error> {
        let lease = Arc::new(Lease::acquire()?);
        let installed = provision::install(root, runtime, model, cancellation, lease.clone())
            .map_err(|_| {
                if cancellation.is_cancelled() {
                    Error::Cancelled
                } else {
                    Error::Unavailable
                }
            })?;
        model::Installation::from_extraction(installed, cancellation)
            .map(|model| Self(Arc::new(model)))
            .map_err(|_| {
                if cancellation.is_cancelled() {
                    Error::Cancelled
                } else {
                    Error::Unavailable
                }
            })
    }
    pub fn start<T>(&self, ticket: Ticket<T>) -> Result<(Session<T>, Control), Error> {
        let lease = Arc::new(Lease::acquire()?);
        let signals = Arc::new(Signals {
            stop: CancellationToken::new(),
            cancel: CancellationToken::new(),
        });
        Ok((
            Session {
                ticket,
                signals: signals.clone(),
                backend: runner::Backend::new(self.0.clone(), lease.clone()),
                _lease: lease,
            },
            Control(signals),
        ))
    }
}

pub struct Session<T> {
    ticket: Ticket<T>,
    signals: Arc<Signals>,
    backend: runner::Backend,
    _lease: Arc<Lease>,
}
impl<T> Session<T> {
    /// Run on a blocking worker. Progress emits at most three transitions and
    /// never includes audio/text. `Recording` starts only after actual PCM.
    pub fn run(self, mut progress: impl FnMut(Phase)) -> Result<Transcript<T>, Error> {
        progress(Phase::Preparing);
        if self.signals.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let audio = self
            .backend
            .capture(&self.signals, || progress(Phase::Recording))?;
        if self.signals.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        progress(Phase::Transcribing);
        let text = self.backend.transcribe(audio, &self.signals)?;
        if self.signals.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(Transcript {
            ticket: self.ticket,
            text,
        })
    }
}
struct Lease;
impl Lease {
    fn acquire() -> Result<Self, Error> {
        ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| Error::Busy)
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
    }
}

fn result(bytes: &[u8]) -> Result<String, Error> {
    #[derive(serde::Deserialize)]
    struct ResultText {
        text: String,
    }
    // Pinned sherpa-onnx v2.0.0 prints one JSON object to stdout per WAV.
    // Diagnostics go to stderr. Accept neither prose scraping nor extra objects.
    let result: ResultText = serde_json::from_slice(bytes).map_err(|_| Error::InvalidResult)?;
    let text = result.text.trim().to_owned();
    if text.len() > MAX_TRANSCRIPT_BYTES || text.contains('\0') {
        return Err(Error::InvalidResult);
    }
    if text.is_empty() {
        return Err(Error::NoSpeech);
    }
    Ok(text)
}

fn audio_error(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::InvalidData {
        Error::NoAudio
    } else {
        Error::Microphone
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognizer_result_is_structured_bounded_and_never_command_dispatch() {
        assert_eq!(
            result(br#"{"text":" /goal finish tests ","timestamps":[0.1]}"#).unwrap(),
            "/goal finish tests"
        );
        assert_eq!(result(br#"{"text":"   "}"#), Err(Error::NoSpeech));
        assert_eq!(
            result(b"human log line\n{\"text\":\"hello\"}"),
            Err(Error::InvalidResult)
        );
        assert_eq!(
            result(br#"{"text":"first"}{"text":"second"}"#),
            Err(Error::InvalidResult)
        );
        assert_eq!(
            result(
                serde_json::to_string(
                    &serde_json::json!({"text": "a".repeat(MAX_TRANSCRIPT_BYTES + 1)})
                )
                .unwrap()
                .as_bytes()
            ),
            Err(Error::InvalidResult)
        );
    }
    #[test]
    fn results_keep_exact_runtime_revision_and_bookmark_identity() {
        let ticket = Ticket::new(("card", 4), 12, Uuid::new_v4());
        assert!(ticket.belongs_to(&ticket));
        let mut changed = ticket.clone();
        changed.target.1 += 1;
        assert!(!ticket.belongs_to(&changed));
        let mut changed = ticket.clone();
        changed.draft_revision += 1;
        assert!(!ticket.belongs_to(&changed));
        assert!(!ticket.belongs_to(&Ticket::new(
            ticket.target,
            ticket.draft_revision,
            ticket.bookmark
        )));
    }
    #[test]
    fn dropping_control_pushes_cancellation_without_stopping_the_agent() {
        let signals = Arc::new(Signals {
            stop: CancellationToken::new(),
            cancel: CancellationToken::new(),
        });
        let control = Control(signals.clone());
        control.stop_recording();
        assert!(signals.stop.is_cancelled());
        assert!(!signals.cancel.is_cancelled());
        drop(control);
        assert!(signals.cancel.is_cancelled());
    }
}
