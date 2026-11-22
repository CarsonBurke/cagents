//! The helper's implementation: audio capture, speech detection,
//! transcription and model downloads. Only `cagents-dictate` selects it.

pub mod audio;
pub mod install;
pub mod pipeline;
pub mod recognizer;
pub mod serve;
pub mod transcribe;

/// The sample rate every model here takes.
pub const SAMPLE_RATE: u32 = 16_000;

/// Selects the capture source: `wav:PATH` plays that file into the session
/// at the pace of speech, then silence, in place of the microphone. Tests
/// and native evidence on a host with no microphone dictate this way.
pub const INPUT_VARIABLE: &str = "CAGENTS_DICTATION_INPUT";

/// Overrides how many threads the recognizer uses.
pub const THREADS_VARIABLE: &str = "CAGENTS_DICTATION_THREADS";

/// How many threads the recognizer uses: half the processors, at most four.
/// Agents and builds share the machine, and a 0.6B model on four threads
/// already transcribes many times faster than speech.
pub fn recognizer_threads() -> i32 {
    if let Some(threads) = std::env::var(THREADS_VARIABLE)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|&threads| threads > 0)
    {
        return threads;
    }
    let processors = std::thread::available_parallelism().map_or(2, |count| count.get());
    (processors / 2).clamp(1, 4) as i32
}
