//! `cagents-dictate transcribe FILE`: a WAV file through the same pipeline a
//! dictation session uses, with its measurements. Without `--realtime` the
//! file is read at once, which measures transcription speed; with it, the
//! file arrives at the pace of speech, as a microphone would deliver it,
//! which measures the partials and how soon text appears.

use std::{
    path::Path,
    process::ExitCode,
    sync::{Arc, mpsc},
    thread,
    time::Instant,
};

use serde::Serialize;

use super::{
    audio::{Source, WavFile},
    pipeline::{self, Control, Emit, Work, Worker},
    recognizer::{self, Silero, Transducer},
};
use crate::{
    catalog::{self, Model},
    protocol::{Event, Stats},
    store,
};

/// What `--json` prints last.
#[derive(Serialize)]
struct Summary<'a> {
    r#type: &'static str,
    model: &'a str,
    text: String,
    audio_seconds: f64,
    stats: &'a Stats,
    real_time_factor: Option<f64>,
    threads: i32,
    peak_rss_mb: Option<u64>,
}

pub fn run(file: &Path, model: &str, realtime: bool, json: bool) -> ExitCode {
    match transcribe(file, model, realtime, json) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("cagents-dictate: {message}");
            ExitCode::FAILURE
        }
    }
}

fn transcribe(file: &Path, id: &str, realtime: bool, json: bool) -> Result<(), String> {
    let model: &'static Model =
        catalog::find(id).ok_or_else(|| format!("no model is called {id}"))?;
    let root = store::root().ok_or("no home directory to find models in")?;
    if store::status(&root, model) != store::Status::Installed {
        return Err(format!(
            "{id} is not installed; run `cagents-dictate install {id}`"
        ));
    }
    let source = WavFile::open(file, realtime).map_err(|error| error.message)?;
    let audio_seconds = source.duration();
    let detector = Silero::open(&store::directory(&root, model), model).map_err(|error| error.1)?;
    let mut resample =
        recognizer::resampler(source.sample_rate()).map_err(|error| error.message)?;

    let (events, received) = mpsc::channel();
    let emit: Emit = Arc::new(move |event| {
        let _ = events.send(event);
    });
    let (work, jobs) = mpsc::channel();
    let worker = {
        let emit = emit.clone();
        thread::spawn(move || {
            Worker::new(
                Box::new(|model| {
                    Transducer::load(model)
                        .map(|recognizer| Box::new(recognizer) as Box<dyn pipeline::Recognizer>)
                }),
                emit,
            )
            .run(jobs)
        })
    };
    let control = Arc::new(Control::default());
    work.send(Work::Begin {
        session: 1,
        model,
        control: control.clone(),
        started: Instant::now(),
    })
    .map_err(|_| "the recognizer stopped")?;
    let capture = thread::spawn(move || {
        // Played at the pace of speech, the file ends in silence until its
        // last utterance closes, as when someone stops talking; then stop.
        let stopper = realtime.then(|| {
            let control = control.clone();
            let length = std::time::Duration::from_secs_f64(audio_seconds + 1.5);
            thread::spawn(move || {
                thread::sleep(length);
                control.stop.store(true, std::sync::atomic::Ordering::Release);
            })
        });
        pipeline::capture(
            1,
            Box::new(source),
            &mut resample,
            Box::new(detector),
            &control,
            &work,
            &*emit,
        );
        if let Some(stopper) = stopper {
            let _ = stopper.join();
        }
    });

    let mut texts: Vec<String> = Vec::new();
    let outcome = loop {
        let Ok(event) = received.recv() else {
            break Err("the recognizer stopped".to_owned());
        };
        if json && !matches!(event, Event::Level { .. }) {
            println!("{}", event.to_line());
        }
        match event {
            Event::Partial { text, .. } if !json => eprintln!("… {text}"),
            Event::Final { segment, text, .. } => {
                let segment = segment as usize;
                texts.resize(texts.len().max(segment + 1), String::new());
                texts[segment] = text;
            }
            Event::Stopped { stats, .. } => break Ok(stats),
            Event::Error { message, .. } => break Err(message),
            _ => {}
        }
    };
    let _ = capture.join();
    let _ = worker.join();
    let stats = outcome?;
    let text = texts
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let summary = Summary {
        r#type: "summary",
        model: model.id,
        text,
        audio_seconds,
        // Against the file rather than the session: a paced session also
        // hears the silence after it.
        real_time_factor: (audio_seconds > 0.0)
            .then(|| stats.decode_ms as f64 / 1000.0 / audio_seconds),
        stats: &stats,
        threads: super::recognizer_threads(),
        peak_rss_mb: peak_rss_mb(),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string(&summary).expect("a summary serializes")
        );
    } else {
        println!("{}", summary.text);
        eprintln!(
            "{:.1} s of audio, {} decodes in {} ms (real-time factor {}), model load {} ms, \
             first text after {} ms, {} threads, peak RSS {} MB",
            summary.audio_seconds,
            stats.decodes,
            stats.decode_ms,
            summary
                .real_time_factor
                .map_or_else(|| "-".to_owned(), |rtf| format!("{rtf:.3}")),
            stats.load_ms.unwrap_or(0),
            stats
                .first_text_ms
                .map_or_else(|| "-".to_owned(), |ms| ms.to_string()),
            summary.threads,
            summary
                .peak_rss_mb
                .map_or_else(|| "-".to_owned(), |mb| mb.to_string()),
        );
    }
    Ok(())
}

/// The most memory this process has held, from `/proc`.
fn peak_rss_mb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024)
}
