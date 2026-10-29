//! Process-local observation of the real dispatch constructor. This seam never
//! changes authorization, dispatch admission, cleanup outcome or child reaping.
use super::*;
use crate::chat::runtime::startup_observation::Observation;
use std::sync::{OnceLock, mpsc};

pub(in crate::host) struct Started {
    pub source: chat::Session,
    pub context: Arc<chat::protocol::ExecutionContext>,
    pub observation: Observation,
    pub resume: mpsc::SyncSender<()>,
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
) -> Result<(), String> {
    let observer = {
        let mut slot = OBSERVER.get_or_init(|| Mutex::new(None)).lock().unwrap();
        if slot.as_ref().is_none_or(|(expected, _)| *expected != agent) {
            return Ok(());
        }
        slot.take().unwrap().1
    };
    // Existing inert ELF is only launch preflight input. The exact helper gate
    // and cancellation-before-unlock oracle require that it never executes.
    source.launch.recipe.program = "/usr/bin/false".into();
    let observation = Observation::install(source.clone(), context.clone());
    let (resume, resumed) = mpsc::sync_channel(1);
    observer
        .try_send(Started {
            source: source.clone(),
            context: context.clone(),
            observation,
            resume,
        })
        .map_err(|_| "Dispatch fixture observer disappeared before process creation".to_owned())?;
    resumed
        .recv_timeout(Duration::from_secs(8))
        .map_err(|_| "Dispatch fixture did not install its cleanup fence".to_owned())
}

pub(in crate::host) fn run(host: &SharedHost, accepted: &Accepted) -> Result<(), (State, String)> {
    let pinned = chat::protocol::ExecutionContext::capture(&accepted.agent.spec.target.directory)
        .map_err(|error| (State::Failed, error))?;
    dispatch(host, accepted, pinned.directory_descriptor())
}
