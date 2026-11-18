//! A scripted retained helper for daemon tests. It answers the semantic
//! outcome methods (`state`, source recognition, the outbox and loss
//! acknowledgements) from a script the test edits, through the real proxy and
//! socket exchange, without a provider or a helper process.
use super::*;
use std::sync::atomic::AtomicBool;

/// What the scripted helper reports and what the daemon asked of it.
#[derive(Default)]
pub(crate) struct Script {
    pub confirmed_session: Option<String>,
    pub continuity_loss: Option<ContinuityLoss>,
    pub retained_runs: Vec<crate::model::RunId>,
    /// Unacknowledged outcomes, in sequence order.
    pub outbox: Vec<OutboxEntry>,
    pub outbox_ack: u64,
    /// Every method the daemon called, in order.
    pub calls: Vec<String>,
}

impl Script {
    pub(crate) fn calls_of(&self, method: &str) -> usize {
        self.calls.iter().filter(|call| *call == method).count()
    }
}

pub(crate) struct ScriptedHelper {
    pub script: Arc<Mutex<Script>>,
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
    _root: tempfile::TempDir,
}

impl ScriptedHelper {
    /// A proxy for `source`'s retained actor, whose helper is this script.
    pub(crate) fn start(
        source: &chat::Session,
        scope: Result<crate::account::scope::LaunchAccountScope, String>,
    ) -> (RemoteActor, Self) {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let socket = root.path().join("scripted-helper.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let actor = RemoteActor::scripted_fixture(socket.clone(), source.clone(), scope);
        let template = actor.state().as_ref().clone();
        let script = Arc::new(Mutex::new(Script::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let server = thread::spawn({
            let script = script.clone();
            let stop = stop.clone();
            move || {
                let mut changed = template.changed;
                for stream in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    let Ok(stream) = stream else { continue };
                    let Ok(request) = receive::<Invocation>(&stream, TIMEOUT) else {
                        continue;
                    };
                    changed += 1;
                    let reply = answer(&template, changed, &mut script.lock().unwrap(), request);
                    let _ = send(&stream, &reply, TIMEOUT);
                }
            }
        });
        (
            actor,
            Self {
                script,
                socket,
                stop,
                server: Some(server),
                _root: root,
            },
        )
    }
}

fn answer(
    template: &Resident,
    changed: u64,
    script: &mut Script,
    request: Invocation,
) -> Result<Value, String> {
    script.calls.push(request.method.clone());
    match request.method.as_str() {
        "state" => {
            let mut resident = template.clone();
            resident.changed = changed;
            resident.confirmed_session = script.confirmed_session.clone();
            resident.continuity_loss = script.continuity_loss.clone();
            resident.retained_runs = script.retained_runs.clone();
            resident.outbox_ack = script.outbox_ack;
            resident.outbox_latest = script
                .outbox
                .last()
                .map_or(script.outbox_ack, |entry| entry.sequence);
            Ok(serde_json::to_value(Some(resident)).unwrap())
        }
        "recognize_sources" => Ok(Value::Null),
        "outbox_unacked" => Ok(serde_json::to_value(&script.outbox).unwrap()),
        "ack_outbox" => {
            let (through,): (u64,) = serde_json::from_value(request.args).unwrap();
            script.outbox.retain(|entry| entry.sequence > through);
            script.outbox_ack = script.outbox_ack.max(through);
            Ok(Value::Null)
        }
        "ack_loss" => {
            let (id,): (uuid::Uuid,) = serde_json::from_value(request.args).unwrap();
            let loss = script
                .continuity_loss
                .as_mut()
                .filter(|loss| loss.id == id)
                .ok_or("Continuity loss acknowledgement names another incident")?;
            loss.acknowledged = true;
            Ok(Value::Null)
        }
        method => Err(format!("The scripted helper does not serve {method}")),
    }
}

impl Drop for ScriptedHelper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake the blocking accept so the server sees the stop.
        let _ = UnixStream::connect(&self.socket);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}
