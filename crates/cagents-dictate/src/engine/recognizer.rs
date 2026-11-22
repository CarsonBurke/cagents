//! sherpa-onnx behind the pipeline's [`Detector`] and [`Recognizer`].

use std::path::Path;

use sherpa_onnx::{
    LinearResampler, OfflineRecognizer, OfflineRecognizerConfig, VadModelConfig,
    VoiceActivityDetector,
};

use super::{
    SAMPLE_RATE,
    audio::CaptureError,
    pipeline::{Detector, Recognizer},
};
use crate::{
    catalog::{Architecture, Model},
    protocol::ErrorKind,
    store,
};

/// Silero VAD, tuned for dictation.
pub struct Silero(VoiceActivityDetector);

impl Silero {
    pub fn open(directory: &Path, model: &Model) -> Result<Self, (ErrorKind, String)> {
        let Architecture::OfflineTransducer { vad, .. } = model.architecture;
        let mut config = VadModelConfig::default();
        config.silero_vad.model = Some(path(directory, vad));
        config.silero_vad.threshold = 0.5;
        // A pause this long ends an utterance; shorter ones are breaths.
        config.silero_vad.min_silence_duration = 0.5;
        config.silero_vad.min_speech_duration = 0.25;
        // Bounds what a partial transcribes again: see the pipeline.
        config.silero_vad.max_speech_duration = 10.0;
        config.silero_vad.window_size = super::pipeline::WINDOW as i32;
        config.sample_rate = SAMPLE_RATE as i32;
        config.num_threads = 1;
        VoiceActivityDetector::create(&config, 30.0)
            .map(Self)
            .ok_or_else(|| (ErrorKind::ModelInvalid, format!("cannot load {vad}")))
    }
}

impl Detector for Silero {
    fn accept(&mut self, window: &[f32]) {
        self.0.accept_waveform(window);
    }

    fn speaking(&self) -> bool {
        self.0.detected()
    }

    fn pop(&mut self) -> Option<Vec<f32>> {
        let samples = self.0.front()?.samples().to_vec();
        self.0.pop();
        Some(samples)
    }

    fn flush(&mut self) {
        self.0.flush();
    }
}

/// A NeMo transducer, such as Parakeet TDT.
pub struct Transducer(OfflineRecognizer);

impl Transducer {
    /// Loads `model` from where it is installed.
    pub fn load(model: &'static Model) -> Result<Self, (ErrorKind, String)> {
        let root = store::root().ok_or_else(|| {
            (
                ErrorKind::ModelMissing,
                "no home directory to find models in".to_owned(),
            )
        })?;
        if store::status(&root, model) != store::Status::Installed {
            return Err((
                ErrorKind::ModelMissing,
                format!("{} is not installed", model.name),
            ));
        }
        let directory = store::directory(&root, model);
        let Architecture::OfflineTransducer {
            encoder,
            decoder,
            joiner,
            tokens,
            ..
        } = model.architecture;
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.transducer.encoder = Some(path(&directory, encoder));
        config.model_config.transducer.decoder = Some(path(&directory, decoder));
        config.model_config.transducer.joiner = Some(path(&directory, joiner));
        config.model_config.tokens = Some(path(&directory, tokens));
        config.model_config.model_type = Some("nemo_transducer".to_owned());
        config.model_config.num_threads = super::recognizer_threads();
        config.model_config.provider = Some("cpu".to_owned());
        OfflineRecognizer::create(&config).map(Self).ok_or_else(|| {
            (
                ErrorKind::ModelInvalid,
                format!("cannot load {}", model.name),
            )
        })
    }
}

impl Recognizer for Transducer {
    fn transcribe(&mut self, samples: &[f32]) -> String {
        let stream = self.0.create_stream();
        stream.accept_waveform(SAMPLE_RATE as i32, samples);
        self.0.decode(&stream);
        stream
            .get_result()
            .map(|result| result.text.trim().to_owned())
            .unwrap_or_default()
    }
}

/// Converts a source's rate to 16 kHz; a no-op at 16 kHz.
pub fn resampler(
    rate: u32,
) -> Result<Box<dyn FnMut(&[f32]) -> Vec<f32> + Send>, CaptureError> {
    if rate == SAMPLE_RATE {
        return Ok(Box::new(<[f32]>::to_vec));
    }
    let resampler = i32::try_from(rate)
        .ok()
        .and_then(|rate| LinearResampler::create(rate, SAMPLE_RATE as i32))
        .ok_or_else(|| CaptureError {
            kind: ErrorKind::InputUnavailable,
            message: format!("cannot convert {rate} Hz audio to {SAMPLE_RATE} Hz"),
        })?;
    Ok(Box::new(move |samples| resampler.resample(samples, false)))
}

fn path(directory: &Path, name: &str) -> String {
    directory.join(name).to_string_lossy().into_owned()
}
