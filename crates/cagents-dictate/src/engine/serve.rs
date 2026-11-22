//! `cagents-dictate serve`: sessions over the line protocol.

use std::{
    io::{BufRead, Write},
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
                    previous.cancel();
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
                // Listen first: the model loads while the person speaks.
                let source = audio::open_session_source();
                let _ = work.send(Work::Begin {
                    session,
                    model,
                    control: control.clone(),
                    started,
                });
                let source = match source {
                    Ok(source) => source,
                    Err(error) => {
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
                let detector = match Silero::open(&directory, model) {
                    Ok(detector) => detector,
                    Err((kind, message)) => {
                        let _ = work.send(Work::End {
                            session,
                            end: End::Failed { kind, message },
                        });
                        return;
                    }
                };
                emit(Event::Listening {
                    session,
                    input: source.name(),
                });
                let mut resample = recognizer::resampler(source.sample_rate());
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
