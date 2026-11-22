//! A session from audio to text.
//!
//! Two threads carry a session. The capture thread reads the source,
//! resamples it to 16 kHz, meters it, and runs speech detection, which cuts
//! the audio into utterances ([`Segmenter`]). It hands utterances to the
//! recognizer thread ([`Worker`]) as jobs: a final transcription when an
//! utterance ends, and while one is still being spoken, a partial
//! transcription of it so far. Parakeet transcribes whole utterances, so a
//! partial transcribes the utterance again from its start. To keep that from
//! occupying the machine, a partial is asked for only when the recognizer is
//! idle and at most every two decode times (so it is busy at most a third
//! of the time), and detection closes an utterance after ten seconds.
//!
//! Every event that ends a session goes through the recognizer thread, after
//! the finals before it, so the client sees the finals, then the end.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, Sender},
    },
    time::{Duration, Instant},
};

use super::{
    SAMPLE_RATE,
    audio::{CaptureError, Source},
};
use crate::{
    catalog::Model,
    protocol::{ErrorKind, Event, SessionId, Stats},
};

/// Samples per detection window; Silero takes 512 at 16 kHz.
pub const WINDOW: usize = 512;

/// Audio kept from before speech is detected, so a partial includes the
/// onset that detection needed to hear before it was sure.
const PREROLL: u64 = SAMPLE_RATE as u64 / 2;

/// The least speech worth a partial.
const MIN_PARTIAL_AUDIO: u64 = SAMPLE_RATE as u64 / 2;

/// The least new audio between partials.
const MIN_PARTIAL_GAP: Duration = Duration::from_millis(400);

/// Finds utterances. A wrapper over Silero VAD in the helper; a script in
/// tests.
pub trait Detector: Send {
    fn accept(&mut self, window: &[f32]);
    /// Whether speech is under way now.
    fn speaking(&self) -> bool;
    /// The next finished utterance.
    fn pop(&mut self) -> Option<Vec<f32>>;
    /// Ends any utterance under way, so [`Detector::pop`] returns it.
    fn flush(&mut self);
}

/// Transcribes one utterance of 16 kHz mono audio.
pub trait Recognizer: Send {
    fn transcribe(&mut self, samples: &[f32]) -> String;
}

/// Something for the recognizer to transcribe.
#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    Partial { segment: u32, samples: Vec<f32> },
    /// An empty `samples` means an utterance detection dropped after a
    /// partial was sent: its final is empty.
    Final { segment: u32, samples: Vec<f32> },
}

/// Whether a partial may be asked for now, and after how much new audio.
#[derive(Clone, Copy, Debug)]
pub struct PartialGate {
    pub open: bool,
    pub gap: u64,
}

/// Cuts 16 kHz audio into utterances and asks for their transcriptions.
pub struct Segmenter {
    detector: Box<dyn Detector>,
    /// Audio from `base` on: what a partial may need.
    timeline: VecDeque<f32>,
    base: u64,
    /// Samples taken so far.
    total: u64,
    /// Audio not yet a whole window.
    pending: Vec<f32>,
    segment: u32,
    /// Where the utterance under way began.
    speech: Option<u64>,
    partial_at: u64,
    partial_sent: bool,
}

impl Segmenter {
    pub fn new(detector: Box<dyn Detector>) -> Self {
        Self {
            detector,
            timeline: VecDeque::new(),
            base: 0,
            total: 0,
            pending: Vec::with_capacity(WINDOW),
            segment: 0,
            speech: None,
            partial_at: 0,
            partial_sent: false,
        }
    }

    /// Samples taken so far.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Takes `samples` and adds what they call for to `jobs`.
    pub fn push(&mut self, samples: &[f32], gate: PartialGate, jobs: &mut Vec<Job>) {
        self.pending.extend_from_slice(samples);
        let whole = self.pending.len() / WINDOW * WINDOW;
        let pending = std::mem::take(&mut self.pending);
        for window in pending[..whole].chunks_exact(WINDOW) {
            self.window(window, jobs);
        }
        self.pending = pending;
        self.pending.drain(..whole);

        if let Some(start) = self.speech
            && gate.open
            && self.total - start >= MIN_PARTIAL_AUDIO
            && self.total - self.partial_at >= gate.gap
        {
            jobs.push(Job::Partial {
                segment: self.segment,
                samples: self.since(start),
            });
            self.partial_at = self.total;
            self.partial_sent = true;
        }
    }

    /// Ends the utterance under way, if any, and asks for the finals left.
    pub fn finish(&mut self, jobs: &mut Vec<Job>) {
        if !self.pending.is_empty() {
            // Silero needs whole windows; pad the tail with silence.
            let mut window = std::mem::take(&mut self.pending);
            window.resize(WINDOW, 0.0);
            self.window(&window, jobs);
        }
        self.detector.flush();
        let closed = self.pop_finals(jobs);
        if !closed && self.speech.is_some() {
            self.drop_utterance(jobs);
        }
        self.speech = None;
    }

    fn window(&mut self, window: &[f32], jobs: &mut Vec<Job>) {
        self.detector.accept(window);
        self.timeline.extend(window);
        self.total += WINDOW as u64;
        let closed = self.pop_finals(jobs);
        match (self.speech, self.detector.speaking()) {
            (None, true) => {
                // After an utterance detection cut at its length limit, the
                // next starts where it ended; otherwise include the onset.
                let back = if closed {
                    WINDOW as u64
                } else {
                    PREROLL + WINDOW as u64
                };
                let start = self.total.saturating_sub(back).max(self.base);
                self.speech = Some(start);
                self.partial_at = start;
            }
            // Speech too short for detection to keep.
            (Some(_), false) => {
                self.drop_utterance(jobs);
                self.speech = None;
            }
            _ => {}
        }
        let keep = self
            .speech
            .unwrap_or_else(|| self.total.saturating_sub(PREROLL));
        if keep > self.base {
            let drop = ((keep - self.base) as usize).min(self.timeline.len());
            self.timeline.drain(..drop);
            self.base += drop as u64;
        }
    }

    fn pop_finals(&mut self, jobs: &mut Vec<Job>) -> bool {
        let mut closed = false;
        while let Some(samples) = self.detector.pop() {
            jobs.push(Job::Final {
                segment: self.segment,
                samples,
            });
            self.segment += 1;
            self.partial_sent = false;
            self.speech = None;
            closed = true;
        }
        closed
    }

    /// Settles a dropped utterance whose partial the client already shows.
    fn drop_utterance(&mut self, jobs: &mut Vec<Job>) {
        if self.partial_sent {
            jobs.push(Job::Final {
                segment: self.segment,
                samples: Vec::new(),
            });
            self.segment += 1;
            self.partial_sent = false;
        }
    }

    fn since(&self, start: u64) -> Vec<f32> {
        let from = (start.saturating_sub(self.base) as usize).min(self.timeline.len());
        self.timeline.range(from..).copied().collect()
    }
}

/// Turns audio into [`Event::Level`] and [`Event::Signal`].
pub struct Meter {
    sum: f64,
    count: u32,
    /// Samples since anything was above the silence floor.
    quiet: u64,
    signal: bool,
}

/// Samples per level report: 50 ms.
const LEVEL_SAMPLES: u32 = SAMPLE_RATE / 20;

/// Below this, an input is dead rather than a quiet room: a muted or
/// disconnected microphone delivers digital silence, while a room's noise
/// floor sits around -60 dBFS.
const SILENCE_FLOOR_DB: f64 = -70.0;

/// How long a dead input lasts before it is reported.
const SILENCE_REPORT: u64 = 3 * SAMPLE_RATE as u64;

impl Default for Meter {
    fn default() -> Self {
        Self {
            sum: 0.0,
            count: 0,
            quiet: 0,
            signal: true,
        }
    }
}

impl Meter {
    pub fn push(&mut self, session: SessionId, samples: &[f32], emit: &dyn Fn(Event)) {
        for &sample in samples {
            self.sum += f64::from(sample) * f64::from(sample);
            self.count += 1;
            if self.count < LEVEL_SAMPLES {
                continue;
            }
            let rms = (self.sum / f64::from(self.count)).sqrt();
            let db = 20.0 * rms.max(1e-9).log10();
            self.sum = 0.0;
            self.count = 0;
            emit(Event::Level {
                session,
                level: ((db + 60.0) / 60.0).clamp(0.0, 1.0) as f32,
            });
            if db > SILENCE_FLOOR_DB {
                self.quiet = 0;
                if !self.signal {
                    self.signal = true;
                    emit(Event::Signal {
                        session,
                        present: true,
                    });
                }
            } else {
                self.quiet += u64::from(LEVEL_SAMPLES);
                if self.signal && self.quiet >= SILENCE_REPORT {
                    self.signal = false;
                    emit(Event::Signal {
                        session,
                        present: false,
                    });
                }
            }
        }
    }
}

/// What a session's two threads share.
#[derive(Debug, Default)]
pub struct Control {
    pub stop: AtomicBool,
    pub cancel: AtomicBool,
    /// The recognizer could not load: capture should end.
    pub failed: AtomicBool,
    /// A partial is queued or being transcribed.
    partial_busy: AtomicBool,
    /// How long the last partial took, in microseconds.
    partial_micros: AtomicU64,
}

impl Control {
    fn gate(&self) -> PartialGate {
        let decode = Duration::from_micros(self.partial_micros.load(Ordering::Relaxed));
        let gap = MIN_PARTIAL_GAP.max(decode * 2);
        PartialGate {
            open: !self.partial_busy.load(Ordering::Acquire),
            gap: (gap.as_secs_f64() * f64::from(SAMPLE_RATE)) as u64,
        }
    }
}

/// How a session's capture ended.
#[derive(Clone, Debug, PartialEq)]
pub enum End {
    Stopped { audio_ms: u64 },
    Cancelled,
    Failed { kind: ErrorKind, message: String },
    /// The recognizer failed and has said so.
    Abandoned,
}

/// What the recognizer thread is given.
pub enum Work {
    /// A session starts with `model`.
    Begin {
        session: SessionId,
        model: &'static Model,
        control: Arc<Control>,
        started: Instant,
    },
    Job {
        session: SessionId,
        job: Job,
    },
    End {
        session: SessionId,
        end: End,
    },
}

pub type Emit = Arc<dyn Fn(Event) + Send + Sync>;

/// Loads a model's recognizer, or says why it can't.
pub type Loader =
    Box<dyn FnMut(&'static Model) -> Result<Box<dyn Recognizer>, (ErrorKind, String)> + Send>;

/// Runs a session's capture until it is stopped, cancelled, or its source
/// fails or ends, then tells the recognizer thread how it ended.
pub fn capture(
    session: SessionId,
    mut source: Box<dyn Source>,
    resample: &mut dyn FnMut(&[f32]) -> Vec<f32>,
    detector: Box<dyn Detector>,
    control: &Control,
    work: &Sender<Work>,
    emit: &dyn Fn(Event),
) {
    let mut segmenter = Segmenter::new(detector);
    let mut meter = Meter::default();
    let mut raw = Vec::new();
    let mut jobs = Vec::new();
    let end = loop {
        if control.cancel.load(Ordering::Acquire) {
            break End::Cancelled;
        }
        if control.failed.load(Ordering::Acquire) {
            break End::Abandoned;
        }
        if control.stop.load(Ordering::Acquire) {
            break End::Stopped { audio_ms: 0 };
        }
        raw.clear();
        let more = match source.read(&mut raw) {
            Ok(more) => more,
            Err(CaptureError { kind, message }) => break End::Failed { kind, message },
        };
        let samples = resample(&raw);
        meter.push(session, &samples, emit);
        segmenter.push(&samples, control.gate(), &mut jobs);
        send(session, &mut jobs, control, work);
        if !more {
            break End::Stopped { audio_ms: 0 };
        }
    };
    // Close the microphone before the last transcriptions run.
    drop(source);
    let end = match end {
        End::Stopped { .. } | End::Failed { .. } => {
            segmenter.finish(&mut jobs);
            send(session, &mut jobs, control, work);
            match end {
                End::Stopped { .. } => End::Stopped {
                    audio_ms: segmenter.total() * 1000 / u64::from(SAMPLE_RATE),
                },
                failed => failed,
            }
        }
        other => other,
    };
    let _ = work.send(Work::End { session, end });
}

fn send(session: SessionId, jobs: &mut Vec<Job>, control: &Control, work: &Sender<Work>) {
    for job in jobs.drain(..) {
        if matches!(job, Job::Partial { .. }) {
            control.partial_busy.store(true, Ordering::Release);
        }
        let _ = work.send(Work::Job { session, job });
    }
}

struct Current {
    session: SessionId,
    control: Arc<Control>,
    started: Instant,
    stats: Stats,
    /// An error ended the session; what follows is not reported.
    failed: bool,
}

/// The recognizer thread: loads models and transcribes jobs in order.
pub struct Worker {
    loader: Loader,
    loaded: Option<(&'static str, Box<dyn Recognizer>)>,
    emit: Emit,
}

impl Worker {
    pub fn new(loader: Loader, emit: Emit) -> Self {
        Self {
            loader,
            loaded: None,
            emit,
        }
    }

    /// Serves `work` until every sender is gone.
    pub fn run(mut self, work: Receiver<Work>) {
        let mut current: Option<Current> = None;
        for work in work {
            match work {
                Work::Begin {
                    session,
                    model,
                    control,
                    started,
                } => {
                    let mut state = Current {
                        session,
                        control,
                        started,
                        stats: Stats::default(),
                        failed: false,
                    };
                    match self.load(model) {
                        Ok(load_ms) => {
                            if let Some(load_ms) = load_ms {
                                state.stats.load_ms = Some(load_ms);
                            }
                            (self.emit)(Event::Ready {
                                session,
                                load_ms: load_ms.unwrap_or(0),
                            });
                        }
                        Err((kind, message)) => {
                            state.failed = true;
                            state.control.failed.store(true, Ordering::Release);
                            (self.emit)(Event::Error {
                                session: Some(session),
                                kind,
                                message,
                            });
                        }
                    }
                    current = Some(state);
                }
                Work::Job { session, job } => {
                    let Some(state) = current.as_mut().filter(|state| state.session == session)
                    else {
                        continue;
                    };
                    self.job(state, job);
                }
                Work::End { session, end } => {
                    let Some(state) = current.take_if(|state| state.session == session) else {
                        continue;
                    };
                    let event = match end {
                        End::Cancelled => Some(Event::Cancelled { session }),
                        _ if state.failed => None,
                        End::Abandoned => None,
                        End::Stopped { audio_ms } => Some(Event::Stopped {
                            session,
                            stats: Stats {
                                audio_ms,
                                ..state.stats
                            },
                        }),
                        End::Failed { kind, message } => Some(Event::Error {
                            session: Some(session),
                            kind,
                            message,
                        }),
                    };
                    if let Some(event) = event {
                        (self.emit)(event);
                    }
                }
            }
        }
    }

    /// Loads `model` unless it is loaded: the milliseconds it took, or
    /// `None` when it was already there.
    fn load(&mut self, model: &'static Model) -> Result<Option<u64>, (ErrorKind, String)> {
        if self.loaded.as_ref().is_some_and(|(id, _)| *id == model.id) {
            return Ok(None);
        }
        // Free the old model before loading another: two would double the
        // helper's memory.
        self.loaded = None;
        let started = Instant::now();
        let recognizer = (self.loader)(model)?;
        self.loaded = Some((model.id, recognizer));
        Ok(Some(started.elapsed().as_millis() as u64))
    }

    fn job(&mut self, state: &mut Current, job: Job) {
        let partial = matches!(job, Job::Partial { .. });
        let skip = state.failed || state.control.cancel.load(Ordering::Acquire);
        let (segment, samples) = match job {
            Job::Partial { segment, samples } | Job::Final { segment, samples } => {
                (segment, samples)
            }
        };
        let text = match &mut self.loaded {
            Some((_, recognizer)) if !skip && !samples.is_empty() => {
                let started = Instant::now();
                let text = recognizer.transcribe(&samples);
                let took = started.elapsed();
                state.stats.decode_ms += took.as_millis() as u64;
                state.stats.decodes += 1;
                if partial {
                    state
                        .control
                        .partial_micros
                        .store(took.as_micros() as u64, Ordering::Relaxed);
                }
                Some(text)
            }
            _ if skip => None,
            _ => Some(String::new()),
        };
        if partial {
            state.control.partial_busy.store(false, Ordering::Release);
        }
        let Some(text) = text else {
            return;
        };
        if !text.is_empty() && state.stats.first_text_ms.is_none() {
            state.stats.first_text_ms = Some(state.started.elapsed().as_millis() as u64);
        }
        let session = state.session;
        (self.emit)(if partial {
            Event::Partial {
                session,
                segment,
                text,
            }
        } else {
            Event::Final {
                session,
                segment,
                text,
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, mpsc};

    use super::*;
    use crate::catalog;

    /// Speech wherever the samples are loud, and an utterance once two
    /// windows of quiet follow it.
    #[derive(Default)]
    struct LoudnessDetector {
        speech: Vec<f32>,
        quiet: usize,
        done: VecDeque<Vec<f32>>,
    }

    impl Detector for LoudnessDetector {
        fn accept(&mut self, window: &[f32]) {
            let loud = window.iter().any(|sample| sample.abs() > 0.1);
            if loud {
                self.quiet = 0;
                self.speech.extend_from_slice(window);
            } else if !self.speech.is_empty() {
                self.quiet += 1;
                if self.quiet == 2 {
                    self.done.push_back(std::mem::take(&mut self.speech));
                }
            }
        }

        fn speaking(&self) -> bool {
            !self.speech.is_empty()
        }

        fn pop(&mut self) -> Option<Vec<f32>> {
            self.done.pop_front()
        }

        fn flush(&mut self) {
            if !self.speech.is_empty() {
                self.done.push_back(std::mem::take(&mut self.speech));
            }
        }
    }

    const OPEN: PartialGate = PartialGate {
        open: true,
        gap: SAMPLE_RATE as u64 / 4,
    };

    fn audio(loud: bool, seconds: f32) -> Vec<f32> {
        vec![if loud { 0.5 } else { 0.0 }; (seconds * SAMPLE_RATE as f32) as usize]
    }

    fn kinds(jobs: &[Job]) -> Vec<(char, u32, usize)> {
        jobs.iter()
            .map(|job| match job {
                Job::Partial { segment, samples } => ('p', *segment, samples.len()),
                Job::Final { segment, samples } => ('f', *segment, samples.len()),
            })
            .collect()
    }

    #[test]
    fn partials_grow_with_the_utterance_and_its_final_follows() {
        let mut segmenter = Segmenter::new(Box::new(LoudnessDetector::default()));
        let mut jobs = Vec::new();
        segmenter.push(&audio(false, 1.0), OPEN, &mut jobs);
        assert!(jobs.is_empty());
        for _ in 0..4 {
            segmenter.push(&audio(true, 0.25), OPEN, &mut jobs);
        }
        segmenter.push(&audio(false, 0.25), OPEN, &mut jobs);
        let kinds = kinds(&jobs);
        let partials: Vec<_> = kinds.iter().filter(|kind| kind.0 == 'p').collect();
        assert!(partials.len() >= 2, "{kinds:?}");
        assert!(partials.windows(2).all(|pair| pair[0].2 < pair[1].2));
        // A partial includes the onset before detection was sure.
        assert!(partials[0].2 > SAMPLE_RATE as usize / 2);
        assert_eq!(kinds.last().unwrap().0, 'f');
        assert!(kinds.iter().all(|kind| kind.1 == 0));
    }

    #[test]
    fn a_closed_gate_or_short_gap_holds_partials_back() {
        let mut segmenter = Segmenter::new(Box::new(LoudnessDetector::default()));
        let mut jobs = Vec::new();
        let closed = PartialGate {
            open: false,
            gap: 0,
        };
        segmenter.push(&audio(true, 2.0), closed, &mut jobs);
        assert!(jobs.is_empty());
        let slow = PartialGate {
            open: true,
            gap: 10 * SAMPLE_RATE as u64,
        };
        segmenter.push(&audio(true, 0.1), slow, &mut jobs);
        assert!(jobs.is_empty(), "the gap counts from where speech began");
    }

    #[test]
    fn utterances_are_numbered_in_order_and_finish_flushes_the_last() {
        let mut segmenter = Segmenter::new(Box::new(LoudnessDetector::default()));
        let mut jobs = Vec::new();
        let closed = PartialGate {
            open: false,
            gap: 0,
        };
        for _ in 0..2 {
            segmenter.push(&audio(true, 0.5), closed, &mut jobs);
            segmenter.push(&audio(false, 0.5), closed, &mut jobs);
        }
        segmenter.push(&audio(true, 0.3), closed, &mut jobs);
        segmenter.finish(&mut jobs);
        let finals: Vec<_> = kinds(&jobs).into_iter().map(|kind| (kind.0, kind.1)).collect();
        assert_eq!(finals, vec![('f', 0), ('f', 1), ('f', 2)]);
    }

    /// Hears speech at once but never keeps it, like Silero with speech
    /// shorter than its minimum.
    struct Fickle(bool);

    impl Detector for Fickle {
        fn accept(&mut self, window: &[f32]) {
            self.0 = window.iter().any(|sample| sample.abs() > 0.1);
        }
        fn speaking(&self) -> bool {
            self.0
        }
        fn pop(&mut self) -> Option<Vec<f32>> {
            None
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn a_dropped_utterance_settles_its_partial_with_an_empty_final() {
        let mut segmenter = Segmenter::new(Box::new(Fickle(false)));
        let mut jobs = Vec::new();
        segmenter.push(&audio(true, 1.0), OPEN, &mut jobs);
        segmenter.push(&audio(false, 0.1), OPEN, &mut jobs);
        let kinds = kinds(&jobs);
        assert_eq!(kinds.first().unwrap().0, 'p');
        assert_eq!(*kinds.last().unwrap(), ('f', 0, 0));
        // Without a partial there is nothing to settle.
        let mut segmenter = Segmenter::new(Box::new(Fickle(false)));
        let mut jobs = Vec::new();
        segmenter.push(&audio(true, 0.1), OPEN, &mut jobs);
        segmenter.push(&audio(false, 0.1), OPEN, &mut jobs);
        assert!(jobs.is_empty());
    }

    #[test]
    fn the_meter_reports_levels_and_a_dead_input() {
        let events = Mutex::new(Vec::new());
        let emit = |event| events.lock().unwrap().push(event);
        let mut meter = Meter::default();
        meter.push(1, &audio(true, 0.1), &emit);
        meter.push(1, &audio(false, 3.0), &emit);
        meter.push(1, &audio(true, 0.05), &emit);
        let events = events.into_inner().unwrap();
        let levels: Vec<f32> = events
            .iter()
            .filter_map(|event| match event {
                Event::Level { level, .. } => Some(*level),
                _ => None,
            })
            .collect();
        assert_eq!(levels.len(), 63);
        assert!(levels[0] > 0.8);
        assert_eq!(levels[10], 0.0);
        let signals: Vec<bool> = events
            .iter()
            .filter_map(|event| match event {
                Event::Signal { present, .. } => Some(*present),
                _ => None,
            })
            .collect();
        assert_eq!(signals, vec![false, true]);
    }

    struct Counting(Arc<Mutex<Vec<usize>>>);

    impl Recognizer for Counting {
        fn transcribe(&mut self, samples: &[f32]) -> String {
            self.0.lock().unwrap().push(samples.len());
            format!("{} samples", samples.len())
        }
    }

    fn worker(
        heard: Arc<Mutex<Vec<usize>>>,
        fail: bool,
    ) -> (Sender<Work>, Arc<Mutex<Vec<Event>>>, std::thread::JoinHandle<()>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let emit: Emit = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let loader: Loader = Box::new(move |_| {
            if fail {
                Err((ErrorKind::ModelInvalid, "bad model".to_owned()))
            } else {
                Ok(Box::new(Counting(heard.clone())) as Box<dyn Recognizer>)
            }
        });
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::spawn(move || Worker::new(loader, emit).run(receiver));
        (sender, events, thread)
    }

    fn begin(session: SessionId, control: &Arc<Control>) -> Work {
        Work::Begin {
            session,
            model: catalog::find(catalog::DEFAULT).unwrap(),
            control: control.clone(),
            started: Instant::now(),
        }
    }

    #[test]
    fn the_worker_reports_texts_in_order_then_the_end() {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let (work, events, thread) = worker(heard.clone(), false);
        let control = Arc::new(Control::default());
        control.partial_busy.store(true, Ordering::Release);
        work.send(begin(5, &control)).unwrap();
        for job in [
            Job::Partial {
                segment: 0,
                samples: vec![0.0; 3],
            },
            Job::Final {
                segment: 0,
                samples: vec![0.0; 4],
            },
            Job::Final {
                segment: 1,
                samples: Vec::new(),
            },
        ] {
            work.send(Work::Job { session: 5, job }).unwrap();
        }
        // Another session's late work is ignored.
        work.send(Work::Job {
            session: 4,
            job: Job::Final {
                segment: 9,
                samples: vec![0.0],
            },
        })
        .unwrap();
        work.send(Work::End {
            session: 5,
            end: End::Stopped { audio_ms: 1000 },
        })
        .unwrap();
        drop(work);
        thread.join().unwrap();
        assert!(!control.partial_busy.load(Ordering::Acquire));
        // An empty final is reported without transcribing nothing.
        assert_eq!(*heard.lock().unwrap(), vec![3, 4]);
        let events = events.lock().unwrap();
        assert!(matches!(events[0], Event::Ready { session: 5, .. }));
        assert_eq!(
            events[1..4],
            [
                Event::Partial {
                    session: 5,
                    segment: 0,
                    text: "3 samples".to_owned()
                },
                Event::Final {
                    session: 5,
                    segment: 0,
                    text: "4 samples".to_owned()
                },
                Event::Final {
                    session: 5,
                    segment: 1,
                    text: String::new()
                },
            ]
        );
        let Event::Stopped { session: 5, stats } = &events[4] else {
            panic!("{:?}", events[4]);
        };
        assert_eq!(stats.audio_ms, 1000);
        assert_eq!(stats.decodes, 2);
        assert!(stats.load_ms.is_some() && stats.first_text_ms.is_some());
        assert_eq!(events.len(), 5);
    }

    #[test]
    fn a_cancelled_session_transcribes_nothing_more() {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let (work, events, thread) = worker(heard.clone(), false);
        let control = Arc::new(Control::default());
        work.send(begin(1, &control)).unwrap();
        control.cancel.store(true, Ordering::Release);
        work.send(Work::Job {
            session: 1,
            job: Job::Final {
                segment: 0,
                samples: vec![0.0; 4],
            },
        })
        .unwrap();
        work.send(Work::End {
            session: 1,
            end: End::Cancelled,
        })
        .unwrap();
        drop(work);
        thread.join().unwrap();
        assert!(heard.lock().unwrap().is_empty());
        let events = events.lock().unwrap();
        assert_eq!(events.last(), Some(&Event::Cancelled { session: 1 }));
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn a_model_that_fails_to_load_ends_the_session_once() {
        let (work, events, thread) = worker(Arc::default(), true);
        let control = Arc::new(Control::default());
        work.send(begin(1, &control)).unwrap();
        work.send(Work::End {
            session: 1,
            end: End::Abandoned,
        })
        .unwrap();
        drop(work);
        thread.join().unwrap();
        assert!(control.failed.load(Ordering::Acquire));
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            Event::Error {
                session: Some(1),
                kind: ErrorKind::ModelInvalid,
                ..
            }
        ));
    }
}
