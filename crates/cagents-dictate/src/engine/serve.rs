//! `cagents-dictate serve`: sessions over the line protocol.

use std::{
    io::{BufRead, Write},
    path::Path,
    sync::{Arc, atomic::Ordering, mpsc},
    thread::{self, JoinHandle},
    time::Instant,
};

use super::{
    audio,
    pipeline::{self, Control, Emit, End, Work, Worker},
    recognizer::{self, Silero, Transducer},
};
use crate::{
    catalog,
    protocol::{Command, ErrorKind, Event, PROTOCOL, SessionId},
    store,
};

struct Running {
    session: SessionId,
    control: Arc<Control>,
    thread: JoinHandle<()>,
}

impl Running {
    fn cancel(self) {
        self.control.cancel.store(true, Ordering::Release);
        let _ = self.thread.join();
    }

    /// Makes way for another session. One that was told to stop keeps its
    /// speech: its capture ends promptly, its last finals are queued ahead
    /// of the next session's work, and they are all reported.
    fn supersede(self) {
        if self.control.stop.load(Ordering::Acquire) {
            let _ = self.thread.join();
        } else {
            self.cancel();
        }
    }
}

/// Serves commands from standard input until it closes.
pub fn run() -> std::process::ExitCode {
    // One line per write, whole, whichever thread writes it. A closed pipe
    // means the client is gone; the next read of standard input ends the
    // helper.
    let emit: Emit = Arc::new(|event: Event| {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{}", event.to_line()).and_then(|()| stdout.flush());
    });
    emit(Event::Hello {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    });
    let (work, receiver) = mpsc::channel::<Work>();
    let worker = {
        let emit = emit.clone();
        thread::Builder::new()
            .name("recognizer".to_owned())
            .spawn(move || {
                Worker::new(
                    Box::new(|model| {
                        Transducer::load(model)
                            .map(|recognizer| Box::new(recognizer) as Box<dyn pipeline::Recognizer>)
                    }),
                    emit,
                )
                .run(receiver)
            })
            .expect("the recognizer thread starts")
    };
    let mut running: Option<Running> = None;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let command = match serde_json::from_str::<Command>(&line) {
            Ok(command) => command,
            Err(error) => {
                emit(Event::Error {
                    session: None,
                    kind: ErrorKind::Protocol,
                    message: format!("unreadable command: {error}"),
                });
                continue;
            }
        };
        match command {
            Command::Start { session, model } => {
                if let Some(previous) = running.take() {
                    previous.supersede();
                }
                running = start(session, &model, &work, &emit);
            }
            Command::Stop { session } => {
                if let Some(current) = running.take_if(|current| current.session == session) {
                    // The thread finishes on its own: the last finals and
                    // the end follow through the recognizer.
                    current.control.stop.store(true, Ordering::Release);
                    running = Some(current);
                }
            }
            Command::Cancel { session } => {
                if let Some(current) = running.take_if(|current| current.session == session) {
                    current.cancel();
                }
            }
        }
    }
    if let Some(current) = running.take() {
        current.cancel();
    }
    drop(work);
    let _ = worker.join();
    std::process::ExitCode::SUCCESS
}

fn start(
    session: SessionId,
    model: &str,
    work: &mpsc::Sender<Work>,
    emit: &Emit,
) -> Option<Running> {
    let missing = |message: String| {
        emit(Event::Error {
            session: Some(session),
            kind: ErrorKind::ModelMissing,
            message,
        });
        None
    };
    let Some(model) = catalog::find(model) else {
        return missing(format!("no model is called {model}"));
    };
    let Some(root) = store::root() else {
        return missing("no home directory to find models in".to_owned());
    };
    if store::status(&root, model) != store::Status::Installed {
        return missing(format!("{} is not installed", model.name));
    }
    let control = Arc::new(Control::default());
    let started = Instant::now();
    let thread = {
        let control = control.clone();
        let work = work.clone();
        let emit = emit.clone();
        let directory = store::directory(&root, model);
        thread::Builder::new()
            .name("capture".to_owned())
            .spawn(move || {
                // Listen first, then load the model while the person speaks.
                let opened = open(&directory, model);
                // Cancelled while the input opened: there is nothing to load
                // a model for, and the session ends as cancelled however
                // the opening went.
                if control.cancel.load(Ordering::Acquire) {
                    drop(opened);
                    emit(Event::Cancelled { session });
                    return;
                }
                let (source, mut resample, detector) = match opened {
                    Ok(opened) => opened,
                    Err(error) => {
                        // Without loading a model for nothing: the
                        // recognizer reports the end of a session that
                        // never began after the work queued before it.
                        let _ = work.send(Work::End {
                            session,
                            end: End::Failed {
                                kind: error.kind,
                                message: error.message,
                            },
                        });
                        return;
                    }
                };
                emit(Event::Listening {
                    session,
                    input: source.name(),
                });
                let _ = work.send(Work::Begin {
                    session,
                    model,
                    control: control.clone(),
                    started,
                });
                pipeline::capture(
                    session,
                    source,
                    &mut resample,
                    Box::new(detector),
                    &control,
                    &work,
                    &*emit,
                );
            })
            .expect("the capture thread starts")
    };
    Some(Running {
        session,
        control,
        thread,
    })
}

type Resample = Box<dyn FnMut(&[f32]) -> Vec<f32> + Send>;

/// The input, its conversion to 16 kHz, and speech detection.
fn open(
    directory: &Path,
    model: &catalog::Model,
) -> Result<(Box<dyn audio::Source>, Resample, Silero), audio::CaptureError> {
    let source = audio::open_session_source()?;
    let resample = recognizer::resampler(source.sample_rate())?;
    let detector = Silero::open(directory, model)
        .map_err(|(kind, message)| audio::CaptureError { kind, message })?;
    Ok((source, resample, detector))
}
