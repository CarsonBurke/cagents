//! Where a session's audio comes from: the microphone, or a WAV file.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use cpal::{
    FromSample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

use crate::protocol::ErrorKind;

/// Mono audio at the source's own rate.
pub trait Source {
    /// Samples per second of what [`Source::read`] returns.
    fn sample_rate(&self) -> u32;

    /// What to call the input when telling the person dictating.
    fn name(&self) -> String;

    /// Appends what has arrived since the last read to `out`, waiting
    /// briefly when nothing has. `Ok(false)` means the source has ended.
    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, CaptureError>;
}

/// Why capture failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureError {
    pub kind: ErrorKind,
    pub message: String,
}

impl CaptureError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// How long a read waits for audio before returning what there is.
const POLL: Duration = Duration::from_millis(10);

/// The default input of the system's sound server: PulseAudio, or PipeWire
/// through its pulse server, else ALSA. Capture stops when this is dropped,
/// which releases the microphone.
pub struct Microphone {
    // Dropping the stream stops the callback before the buffer goes.
    _stream: cpal::Stream,
    samples: rtrb::Consumer<f32>,
    failure: Arc<Mutex<Option<cpal::Error>>>,
    sample_rate: u32,
    name: String,
}

/// Seconds of audio the callback can get ahead of the session thread.
const BUFFERED_SECONDS: usize = 10;

impl Microphone {
    pub fn open() -> Result<Self, CaptureError> {
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or_else(|| {
            CaptureError::new(ErrorKind::NoInputDevice, "no microphone is connected")
        })?;
        let name = device.to_string();
        let supported = device
            .default_input_config()
            .map_err(|error| unavailable(&error))?;
        let config = supported.config();
        let channels = usize::from(config.channels).max(1);
        let sample_rate = config.sample_rate;
        let (producer, samples) =
            rtrb::RingBuffer::new(sample_rate as usize * BUFFERED_SECONDS);
        let failure = Arc::new(Mutex::new(None));
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => {
                build::<f32>(&device, config, channels, producer, failure.clone())
            }
            cpal::SampleFormat::I16 => {
                build::<i16>(&device, config, channels, producer, failure.clone())
            }
            cpal::SampleFormat::I32 => {
                build::<i32>(&device, config, channels, producer, failure.clone())
            }
            cpal::SampleFormat::U16 => {
                build::<u16>(&device, config, channels, producer, failure.clone())
            }
            cpal::SampleFormat::U8 => {
                build::<u8>(&device, config, channels, producer, failure.clone())
            }
            format => {
                return Err(CaptureError::new(
                    ErrorKind::InputUnavailable,
                    format!("the microphone offers only {format:?} samples"),
                ));
            }
        }
        .map_err(|error| unavailable(&error))?;
        stream.play().map_err(|error| unavailable(&error))?;
        Ok(Self {
            _stream: stream,
            samples,
            failure,
            sample_rate,
            name,
        })
    }
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    mut producer: rtrb::Producer<f32>,
    failure: Arc<Mutex<Option<cpal::Error>>>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            // Runs on the audio thread: no locks and no allocation. A
            // session thread that falls ten seconds behind loses audio
            // rather than stall the sound server.
            for frame in data.chunks(channels) {
                let sum: f32 = frame.iter().map(|&sample| sample.to_sample::<f32>()).sum();
                let _ = producer.push(sum / frame.len() as f32);
            }
        },
        move |error| {
            if let Ok(mut failure) = failure.lock() {
                failure.get_or_insert(error);
            }
        },
        None,
    )
}

fn unavailable(error: &cpal::Error) -> CaptureError {
    let kind = match error.kind() {
        cpal::ErrorKind::DeviceNotAvailable => ErrorKind::NoInputDevice,
        _ => ErrorKind::InputUnavailable,
    };
    CaptureError::new(kind, error.to_string())
}

impl Source for Microphone {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, CaptureError> {
        if let Some(error) = self.failure.lock().ok().and_then(|mut failure| failure.take()) {
            match error.kind() {
                // The stream followed the system to another default input,
                // or briefly ran behind: capture goes on.
                cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::Xrun => {}
                cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated => {
                    return Err(CaptureError::new(ErrorKind::InputLost, error.to_string()));
                }
                _ => {
                    return Err(CaptureError::new(
                        ErrorKind::InputUnavailable,
                        error.to_string(),
                    ));
                }
            }
        }
        if self.samples.is_empty() {
            thread::sleep(POLL);
        }
        let available = self.samples.slots();
        if let Ok(chunk) = self.samples.read_chunk(available) {
            out.extend(chunk);
        }
        Ok(true)
    }
}

/// A WAV file in place of the microphone.
pub struct WavFile {
    samples: Vec<f32>,
    sample_rate: u32,
    name: String,
    /// When paced, reads return the file at the pace it was recorded, then
    /// silence until the session stops, as a microphone would. Otherwise a
    /// read returns everything and the source then ends.
    paced: Option<Instant>,
    position: usize,
}

impl WavFile {
    pub fn open(path: &Path, paced: bool) -> Result<Self, CaptureError> {
        let unreadable = |error: hound::Error| {
            CaptureError::new(
                ErrorKind::InputUnavailable,
                format!("cannot read {}: {error}", path.display()),
            )
        };
        let mut reader = hound::WavReader::open(path).map_err(unreadable)?;
        let spec = reader.spec();
        let channels = usize::from(spec.channels).max(1);
        let interleaved: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader
                .samples::<f32>()
                .collect::<Result<_, _>>()
                .map_err(unreadable)?,
            hound::SampleFormat::Int => {
                let scale = (1_i64 << (spec.bits_per_sample.clamp(1, 32) - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|sample| sample.map(|sample| sample as f32 / scale))
                    .collect::<Result<_, _>>()
                    .map_err(unreadable)?
            }
        };
        let samples = interleaved
            .chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
            .collect();
        Ok(Self {
            samples,
            sample_rate: spec.sample_rate,
            name: path
                .file_name()
                .map_or_else(|| "WAV file".to_owned(), |name| name.to_string_lossy().into()),
            paced: paced.then(Instant::now),
            position: 0,
        })
    }

    /// The file's length in seconds.
    pub fn duration(&self) -> f64 {
        self.samples.len() as f64 / f64::from(self.sample_rate)
    }
}

impl Source for WavFile {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, CaptureError> {
        let Some(started) = self.paced else {
            if self.position == self.samples.len() {
                return Ok(false);
            }
            out.extend_from_slice(&self.samples[self.position..]);
            self.position = self.samples.len();
            return Ok(true);
        };
        thread::sleep(POLL);
        let due = (started.elapsed().as_secs_f64() * f64::from(self.sample_rate)) as usize;
        if due > self.position {
            let end = due.min(self.samples.len());
            out.extend_from_slice(&self.samples[self.position.min(end)..end]);
            // Past the end of the file the room is silent.
            out.resize(out.len() + (due - self.position.max(end)), 0.0);
            self.position = due;
        }
        Ok(true)
    }
}

/// The source a session listens to: the file `CAGENTS_DICTATION_INPUT`
/// names, else the microphone.
pub fn open_session_source() -> Result<Box<dyn Source>, CaptureError> {
    match std::env::var_os(super::INPUT_VARIABLE) {
        Some(input) => {
            let input = input.to_string_lossy();
            let path = input.strip_prefix("wav:").ok_or_else(|| {
                CaptureError::new(
                    ErrorKind::InputUnavailable,
                    format!(
                        "{} must be wav:PATH, not {input}",
                        super::INPUT_VARIABLE
                    ),
                )
            })?;
            Ok(Box::new(WavFile::open(Path::new(path), true)?))
        }
        None => Ok(Box::new(Microphone::open()?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &Path, channels: u16, rate: u32, frames: &[[i16; 2]]) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for frame in frames {
            for &sample in &frame[..usize::from(channels)] {
                writer.write_sample(sample).unwrap();
            }
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn a_wav_file_is_mixed_to_mono_and_ends_when_unpaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stereo.wav");
        write_wav(&path, 2, 8000, &[[16384, 0], [-16384, -16384]]);
        let mut source = WavFile::open(&path, false).unwrap();
        assert_eq!(source.sample_rate(), 8000);
        let mut out = Vec::new();
        assert!(source.read(&mut out).unwrap());
        assert_eq!(out, vec![0.25, -0.5]);
        assert!(!source.read(&mut out).unwrap());
    }

    #[test]
    fn a_paced_wav_file_arrives_in_real_time_then_falls_silent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("short.wav");
        // 10 ms at 1 kHz.
        write_wav(&path, 1, 1000, &[[1000, 0]; 10]);
        let mut source = WavFile::open(&path, true).unwrap();
        let mut out = Vec::new();
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(60) {
            assert!(source.read(&mut out).unwrap());
        }
        // Never ahead of the clock, and silence after the file.
        let elapsed = started.elapsed().as_millis() as usize;
        assert!(out.len() <= elapsed + 5, "{} samples in {elapsed} ms", out.len());
        assert!(out.len() >= 50);
        assert!(out[..10].iter().all(|&sample| sample > 0.0));
        assert!(out[10..].iter().all(|&sample| sample == 0.0));
    }
}
