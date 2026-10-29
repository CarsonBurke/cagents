//! Process-local observation of the real dispatch constructor. This seam never
//! changes authorization, dispatch admission, cleanup outcome or child reaping.
use super::*;
use crate::chat::runtime::startup_observation::Observation;
use std::sync::{OnceLock, mpsc};

pub(in crate::host) struct Started {
    pub source: chat::Session,
    pub context: Arc<chat::protocol::ExecutionContext>,
    pub observation: Observation,
}

type Slot = Option<(AgentId, mpsc::SyncSender<Started>)>;
static OBSERVER: OnceLock<Mutex<Slot>> = OnceLock::new();

pub(in crate::host) fn install(agent: AgentId) -> mpsc::Receiver<Started> {
    let (send, receive) = mpsc::sync_channel(1);
    let mut observer = OBSERVER.get_or_init(|| Mutex::new(None)).lock().unwrap();
    assert!(observer.is_none());
    *observer = Some((agent, send));
    receive
}

pub(super) fn prepare(
    agent: AgentId,
    source: &mut chat::Session,
    context: &Arc<chat::protocol::ExecutionContext>,
) {
    let observer = {
        let mut slot = OBSERVER.get_or_init(|| Mutex::new(None)).lock().unwrap();
        if slot.as_ref().is_none_or(|(expected, _)| *expected != agent) {
            return;
        }
        slot.take().unwrap().1
    };
    // Existing inert ELF is only launch preflight input. The exact helper gate
    // and cancellation-before-unlock oracle require that it never executes.
    source.launch.recipe.program = "/usr/bin/false".into();
    let observation = Observation::install(source.clone(), context.clone());
    let _ = observer.try_send(Started {
        source: source.clone(),
        context: context.clone(),
        observation,
    });
}

pub(in crate::host) fn run(host: &SharedHost, accepted: &Accepted) -> Result<(), (State, String)> {
    dispatch(host, accepted, None)
}
