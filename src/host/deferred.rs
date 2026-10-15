//! Bounded provider acknowledgements never occupy an IPC input worker. One
//! process-wide completion selector sleeps on replies or their actual deadline.
use super::{
    delivery::DeliveryQueue,
    state::{ClientId, SharedHost},
};
use crate::{
    chat::Command,
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, TerminalId},
    terminal_ipc::{
        ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope,
        ServerMessage, ServerResponse,
    },
};
use crossbeam_channel::{Receiver, Sender};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const PER_CLIENT: usize = 64;
const NORMAL_TOTAL: usize = 1024;
const INTERRUPT_TOTAL: usize = 256;
type ClientKey = (HostSessionId, BootId, HostIncarnation, ClientId);
fn client_key(proof: &HostSessionProof, client: ClientId) -> ClientKey {
    (
        proof.host_session_id.clone(),
        proof.boot_id.clone(),
        proof.host_incarnation.clone(),
        client,
    )
}
#[derive(Default)]
struct Budget {
    normal: BTreeMap<ClientKey, usize>,
    interrupts: BTreeSet<(ClientKey, TerminalId)>,
    total: usize,
}
struct Credit {
    budget: Arc<Mutex<Budget>>,
    client: ClientKey,
    chat: TerminalId,
    interrupt: bool,
}
impl Credit {
    fn acquire(
        budget: &Arc<Mutex<Budget>>,
        client: ClientKey,
        chat: TerminalId,
        interrupt: bool,
    ) -> Result<Self, String> {
        let mut state = budget.lock().unwrap_or_else(|p| p.into_inner());
        if interrupt {
            if state.interrupts.len() >= INTERRUPT_TOTAL
                || !state.interrupts.insert((client.clone(), chat))
            {
                return Err("An interrupt is already pending or interruption capacity is busy; try again after its reply".into());
            }
        } else {
            if state.total >= NORMAL_TOTAL
                || state.normal.get(&client).copied().unwrap_or(0) >= PER_CLIENT
            {
                return Err("Too many pending provider controls; wait for their replies or interrupt the session".into());
            }
            *state.normal.entry(client.clone()).or_default() += 1;
            state.total += 1;
        }
        Ok(Self {
            budget: budget.clone(),
            client,
            chat,
            interrupt,
        })
    }
}
impl Drop for Credit {
    fn drop(&mut self) {
        let mut state = self.budget.lock().unwrap_or_else(|p| p.into_inner());
        if self.interrupt {
            state.interrupts.remove(&(self.client.clone(), self.chat));
        } else {
            state.total = state.total.saturating_sub(1);
            if let Some(count) = state.normal.get_mut(&self.client) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    state.normal.remove(&self.client);
                }
            }
        }
    }
}
struct Pending {
    reply: Receiver<Result<(), String>>,
    deadline: Instant,
    complete: Box<dyn FnOnce(Result<(), String>) + Send>,
    _credit: Credit,
}
struct Broker {
    incoming: Sender<Pending>,
    cancel: Sender<ClientKey>,
    budget: Arc<Mutex<Budget>>,
}
static BROKER: OnceLock<Result<Broker, String>> = OnceLock::new();
fn broker() -> Result<&'static Broker, String> {
    BROKER
        .get_or_init(|| {
            let (incoming, receive) = crossbeam_channel::bounded(NORMAL_TOTAL + INTERRUPT_TOTAL);
            let (cancel, cancelled) = crossbeam_channel::bounded(NORMAL_TOTAL + INTERRUPT_TOTAL);
            std::thread::Builder::new()
                .name("cagents-control-replies".into())
                .spawn(move || run(receive, cancelled))
                .map_err(|error| format!("Provider reply service is unavailable: {error}"))?;
            Ok(Broker {
                incoming,
                cancel,
                budget: Arc::new(Mutex::new(Budget::default())),
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

pub(super) fn disconnected(host: &SharedHost, client: ClientId) {
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    disconnected_proof(&proof, client);
}
pub(super) fn disconnected_proof(proof: &HostSessionProof, client: ClientId) {
    // Shared IO cleanup must not acquire HostLock or initialize a service.
    if let Some(Ok(broker)) = BROKER.get() {
        let _ = broker.cancel.try_send(client_key(proof, client));
    }
}

pub(super) fn is_deferred(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::ChatCommand {
            command: Command::Interrupt
                | Command::SetModel { .. }
                | Command::SetFastMode { .. }
                | Command::SetPermissionMode { .. }
                | Command::UpdatePermissionSettings { .. }
                | Command::RefreshPermissions
                | Command::SetInterruptMode { .. },
            ..
        }
    )
}
/// None means the real response will be delivered by the completion broker.
/// A returned request follows the ordinary synchronous domain dispatch path.
pub(super) fn try_defer(
    host: &SharedHost,
    client_id: ClientId,
    request_id: u64,
    delivery: Arc<DeliveryQueue>,
    request: ClientRequest,
) -> Result<Option<ClientRequest>, ProtocolError> {
    if !is_deferred(&request) {
        return Ok(Some(request));
    }
    let ClientRequest::ChatCommand { chat_id, command } = &request else {
        unreachable!()
    };
    let refused =
        |message: String| ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false);
    let broker = broker().map_err(refused)?;
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    let credit = Credit::acquire(
        &broker.budget,
        client_key(&proof, client_id),
        *chat_id,
        matches!(command, Command::Interrupt),
    )
    .map_err(refused)?;
    let gate = super::client::control_gate(host);
    let _admission = match gate.try_read() {
        Ok(guard)=>guard,
        Err(std::sync::TryLockError::Poisoned(poisoned))=>poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock)=>return Err(ProtocolError::new(ProtocolErrorCode::QueueFull,"Daemon lifecycle control is busy; retry when it finishes",true)),
    };
    super::client::ensure_control_open(host)?;
    super::agent_execution::terminals::admit(host, client_id, &request)?;
    // This preserves the exact interactive ownership/agent gate used by the
    // synchronous ChatCommand path; only waiting for its reply is deferred.
    let reply = super::chat::enqueue_deferred(host, client_id, request)?;
    let complete = Box::new(move |result: Result<(), String>| {
        let message = match result {
            Ok(()) => ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
            Err(message) => ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                crate::chat::protocol::preview(message.as_bytes(), 1024),
                false,
            )),
        };
        let _ = delivery.push(ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request_id),
            proof,
            message,
        });
    });
    broker
        .incoming
        .try_send(Pending {
            reply,
            deadline: Instant::now() + Duration::from_secs(10),
            complete,
            _credit: credit,
        })
        .map_err(|_| {
            refused(
                "Provider control was admitted but its reply tracking failed; do not retry blindly"
                    .into(),
            )
        })?;
    Ok(None)
}

enum Ready {
    New(Result<Pending, crossbeam_channel::RecvError>),
    Cancel(Result<ClientKey, crossbeam_channel::RecvError>),
    Reply(
        usize,
        Result<Result<(), String>, crossbeam_channel::RecvError>,
    ),
    Deadline,
}
fn run(incoming: Receiver<Pending>, cancelled: Receiver<ClientKey>) {
    let mut pending: Vec<Pending> = Vec::new();
    let mut connected = true;
    let mut cancellations = true;
    let mut retired = BTreeSet::new();
    let mut retirement_order = VecDeque::new();
    loop {
        if !connected && pending.is_empty() {
            break;
        }
        let ready = {
            let mut select = crossbeam_channel::Select::new();
            let input_index = connected.then(|| select.recv(&incoming));
            let cancel_index = cancellations.then(|| select.recv(&cancelled));
            let mut indices = Vec::with_capacity(pending.len());
            for job in &pending {
                indices.push(select.recv(&job.reply));
            }
            let selection = if let Some(deadline) = pending.iter().map(|job| job.deadline).min() {
                select
                    .select_timeout(deadline.saturating_duration_since(Instant::now()))
                    .ok()
            } else {
                Some(select.select())
            };
            match selection {
                Some(operation) if Some(operation.index()) == input_index => {
                    Ready::New(operation.recv(&incoming))
                }
                Some(operation) if Some(operation.index()) == cancel_index => {
                    Ready::Cancel(operation.recv(&cancelled))
                }
                Some(operation) => {
                    let index = indices
                        .iter()
                        .position(|index| *index == operation.index())
                        .unwrap();
                    Ready::Reply(index, operation.recv(&pending[index].reply))
                }
                None => Ready::Deadline,
            }
        };
        match ready {
            Ready::New(Ok(job)) => {
                if !retired.contains(&job._credit.client) {
                    pending.push(job);
                }
            }
            Ready::New(Err(_)) => connected = false,
            Ready::Cancel(Ok(client)) => {
                pending.retain(|job| job._credit.client != client);
                if retired.insert(client.clone()) {
                    retirement_order.push_back(client);
                }
                // Client ids are never reused. Bound tombstones as well as
                // jobs; every pending job has a hard ten-second lifetime.
                while retirement_order.len() > 4096 {
                    if let Some(old) = retirement_order.pop_front() {
                        retired.remove(&old);
                    }
                }
            }
            Ready::Cancel(Err(_)) => cancellations = false,
            Ready::Reply(index, result) => {
                let job = pending.swap_remove(index);
                (job.complete)(result.unwrap_or_else(|_| {
                    Err("Provider exited before acknowledging the control request".into())
                }));
            }
            Ready::Deadline => {}
        }
        let now = Instant::now();
        let mut index = 0;
        while index < pending.len() {
            if pending[index].deadline <= now {
                let job = pending.swap_remove(index);
                (job.complete)(Err(
                    "Provider command acknowledgement timed out; do not retry blindly".into(),
                ));
            } else {
                index += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(client: ClientId) -> ClientKey {
        (
            HostSessionId::new("test"),
            BootId::new("test"),
            HostIncarnation::new("test"),
            client,
        )
    }
    #[test]
    fn ordinary_credit_saturation_does_not_block_an_interrupt_and_credits_return() {
        let budget = Arc::new(Mutex::new(Budget::default()));
        let chat = TerminalId::new();
        let controls = (0..PER_CLIENT)
            .map(|_| Credit::acquire(&budget, key(1), chat, false).unwrap())
            .collect::<Vec<_>>();
        assert!(Credit::acquire(&budget, key(1), chat, false).is_err());
        let stop = Credit::acquire(&budget, key(1), chat, true).unwrap();
        assert!(Credit::acquire(&budget, key(1), chat, true).is_err());
        assert!(Credit::acquire(&budget, key(2), chat, false).is_ok());
        let mut other_host = key(1);
        other_host.2 = HostIncarnation::new("replacement");
        assert!(
            Credit::acquire(&budget, other_host, chat, false).is_ok(),
            "reused client ids from another daemon have independent credits"
        );
        drop(controls);
        drop(stop);
        assert!(Credit::acquire(&budget, key(1), chat, false).is_ok());
        assert!(Credit::acquire(&budget, key(1), chat, true).is_ok());
    }
    #[test]
    fn an_unanswered_control_does_not_hold_up_a_later_interrupt_reply() {
        let (send, receive) = crossbeam_channel::bounded(4);
        let (events, observed) = crossbeam_channel::bounded(4);
        let (cancel, cancelled) = crossbeam_channel::bounded(4);
        let worker = std::thread::spawn(move || run(receive, cancelled));
        let budget = Arc::new(Mutex::new(Budget::default()));
        let chat = TerminalId::new();
        let (ignored, reply) = crossbeam_channel::bounded(1);
        let event = events.clone();
        send.send(Pending {
            reply,
            deadline: Instant::now() + Duration::from_secs(10),
            complete: Box::new(move |reply| {
                event.send(("ordinary", reply)).unwrap();
            }),
            _credit: Credit::acquire(&budget, key(1), chat, false).unwrap(),
        })
        .unwrap_or_else(|_| panic!("broker fixture stopped"));
        let (interrupt, reply) = crossbeam_channel::bounded(1);
        send.send(Pending {
            reply,
            deadline: Instant::now() + Duration::from_secs(1),
            complete: Box::new(move |reply| {
                events.send(("interrupt", reply)).unwrap();
            }),
            _credit: Credit::acquire(&budget, key(1), chat, true).unwrap(),
        })
        .unwrap_or_else(|_| panic!("broker fixture stopped"));
        interrupt.send(Ok(())).unwrap();
        let first = observed.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(first.0, "interrupt");
        assert!(first.1.is_ok());
        ignored.send(Err("control timed out".into())).unwrap();
        let second = observed.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(second.0, "ordinary");
        assert!(second.1.unwrap_err().contains("timed out"));
        drop(ignored);
        drop(send);
        drop(cancel);
        worker.join().unwrap();
        assert_eq!(budget.lock().unwrap().total, 0);
    }
    #[test]
    fn deadlines_and_disconnections_release_bounded_reply_ownership() {
        for disconnected in [false, true] {
            let (send, receive) = crossbeam_channel::bounded(4);
            let (cancel, cancelled) = crossbeam_channel::bounded(4);
            let worker = std::thread::spawn(move || run(receive, cancelled));
            let budget = Arc::new(Mutex::new(Budget::default()));
            let chat = TerminalId::new();
            let (provider, reply) = crossbeam_channel::bounded(1);
            let (event, observed) = crossbeam_channel::bounded(1);
            send.send(Pending {
                reply,
                deadline: if disconnected {
                    Instant::now() + Duration::from_secs(10)
                } else {
                    Instant::now()
                },
                complete: Box::new(move |reply| {
                    let _ = event.send(reply);
                }),
                _credit: Credit::acquire(&budget, key(1), chat, false).unwrap(),
            })
            .unwrap_or_else(|_| panic!("broker fixture stopped"));
            if disconnected {
                cancel.send(key(1)).unwrap();
            }
            drop(send);
            drop(cancel);
            worker.join().unwrap();
            if disconnected {
                assert!(observed.try_recv().is_err());
            } else {
                assert!(observed.recv().unwrap().unwrap_err().contains("timed out"));
            }
            assert_eq!(budget.lock().unwrap().total, 0);
            drop(provider);
        }
    }
}
