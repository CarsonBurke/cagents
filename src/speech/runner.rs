//! Fixed local recorder and verified CPU recognizer. No selected-device paths,
//! provider credentials, ambient ASR program lookup or shell interpolation.
use super::{
    Error, Signals,
    capture::{Audio, MAX_AUDIO_BYTES, MAX_SECONDS, SAMPLE_RATE},
    model::Installation,
};
use crate::process::{CommandSpec, speech_local};
use std::{sync::Arc, time::Duration};

pub(super) struct Backend {
    installation: Arc<Installation>,
    lease: Arc<super::Lease>,
}
impl Backend {
    pub(super) fn new(installation: Arc<Installation>, lease: Arc<super::Lease>) -> Self {
        Self {
            installation,
            lease,
        }
    }
    pub(super) fn capture(&self, signals: &Signals, on_pcm: impl FnMut()) -> Result<Audio, Error> {
        // Absolute OS recorder path; user/provider/environment strings can
        // never select a shell, unrelated executable, remote mic or ASR tool.
        let command = CommandSpec::new("/usr/bin/pw-record")
            .args([
                "--raw",
                "--rate",
                &SAMPLE_RATE.to_string(),
                "--channels",
                "1",
                "--format",
                "s16",
                "-",
            ])
            .env("LC_ALL", "C")
            .env_remove("LD_PRELOAD")
            .env_remove("LD_LIBRARY_PATH")
            .timeout(Duration::from_secs(u64::from(MAX_SECONDS)))
            .output_limits(MAX_AUDIO_BYTES, 16 * 1024);
        let bytes = speech_local::capture(
            &command,
            &signals.stop,
            &signals.cancel,
            self.lease.clone(),
            on_pcm,
        )
        .map_err(|failure| match failure {
            speech_local::Failure::Cancelled => Error::Cancelled,
            speech_local::Failure::CleanupPending => Error::CleanupPending,
            speech_local::Failure::Limit | speech_local::Failure::Timeout => Error::AudioLimit,
            _ => Error::Microphone,
        })?;
        Audio::new(bytes).map_err(super::audio_error)
    }
    pub(super) fn transcribe(&self, audio: Audio, signals: &Signals) -> Result<String, Error> {
        let command = self
            .installation
            .command(audio.wav().map_err(|_| Error::Inference)?)
            .map_err(|_| Error::Unavailable)?
            .timeout(Duration::from_secs(45))
            .output_limits(64 * 1024, 16 * 1024);
        let output = speech_local::transcribe(&command, &signals.cancel, self.lease.clone())
            .map_err(|failure| match failure {
                speech_local::Failure::Cancelled => Error::Cancelled,
                speech_local::Failure::CleanupPending => Error::CleanupPending,
                speech_local::Failure::Limit => Error::InvalidResult,
                _ => Error::Inference,
            })?;
        super::result(&output)
    }
}
