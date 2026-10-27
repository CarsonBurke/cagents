//! Fixed-format bounded audio owned only by the current dictation job.
use std::{
    fs::File,
    io::{self, Seek, SeekFrom, Write},
    sync::Arc,
    time::Duration,
};

pub const SAMPLE_RATE: u32 = 16_000;
pub const MAX_SECONDS: u32 = 60;
pub const MAX_AUDIO_BYTES: usize = SAMPLE_RATE as usize * 2 * MAX_SECONDS as usize;
const MIN_AUDIO_BYTES: usize = SAMPLE_RATE as usize / 5 * 2;

/// Raw signed16 little-endian mono samples. No API returns microphone audio in
/// Debug, diagnostics or UI status text; only the bounded recognizer receives it.
pub struct Audio(Vec<u8>);
impl Audio {
    pub(super) fn new(bytes: Vec<u8>) -> io::Result<Self> {
        if bytes.len() > MAX_AUDIO_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Recording exceeds sixty seconds",
            ));
        }
        if bytes.len() < MIN_AUDIO_BYTES || bytes.len() % 2 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "No complete microphone audio was recorded",
            ));
        }
        Ok(Self(bytes))
    }
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.0.len() as f64 / (f64::from(SAMPLE_RATE) * 2.0))
    }
    pub(super) fn wav(&self) -> io::Result<Arc<File>> {
        // tempfile() is anonymous/unlinked and owner-only. The exact descriptor
        // is inherited by the child; no ambient filename can replace the audio.
        let mut file = tempfile::tempfile()?;
        let pcm_bytes = self.0.len() as u32;
        file.write_all(b"RIFF")?;
        file.write_all(&(36 + pcm_bytes).to_le_bytes())?;
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&16u32.to_le_bytes())?;
        file.write_all(&1u16.to_le_bytes())?;
        file.write_all(&1u16.to_le_bytes())?;
        file.write_all(&SAMPLE_RATE.to_le_bytes())?;
        file.write_all(&(SAMPLE_RATE * 2).to_le_bytes())?;
        file.write_all(&2u16.to_le_bytes())?;
        file.write_all(&16u16.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&pcm_bytes.to_le_bytes())?;
        file.write_all(&self.0)?;
        file.flush()?;
        file.seek(SeekFrom::Start(0))?;
        Ok(Arc::new(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    #[test]
    fn stopped_audio_keeps_the_complete_clip_and_writes_exact_pcm_wave() {
        let bytes = vec![0x33; 128 * 1024];
        let audio = Audio::new(bytes.clone()).unwrap();
        let mut wav = audio.wav().unwrap().try_clone().unwrap();
        let mut encoded = Vec::new();
        wav.read_to_end(&mut encoded).unwrap();
        assert_eq!(&encoded[..4], b"RIFF");
        assert_eq!(&encoded[8..16], b"WAVEfmt ");
        assert_eq!(
            u32::from_le_bytes(encoded[24..28].try_into().unwrap()),
            SAMPLE_RATE
        );
        assert_eq!(
            u32::from_le_bytes(encoded[40..44].try_into().unwrap()) as usize,
            bytes.len()
        );
        assert_eq!(&encoded[44..], bytes);
        assert_eq!(audio.duration(), Duration::from_millis(4096));
    }
    #[test]
    fn malformed_empty_and_oversized_audio_cannot_reach_inference() {
        assert!(Audio::new(Vec::new()).is_err());
        assert!(Audio::new(vec![0; MIN_AUDIO_BYTES + 1]).is_err());
        assert!(Audio::new(vec![0; MAX_AUDIO_BYTES + 2]).is_err());
        assert!(Audio::new(vec![0; MAX_AUDIO_BYTES]).is_ok());
    }
}
