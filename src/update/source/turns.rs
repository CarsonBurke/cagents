//! Separate a completed parent turn from background ownership in old sources.
//!
//! Their UpgradeImpact uses card activity, which also includes child agents.
//! A parent completion must leave the idle wait, but cannot authorize ending
//! those children. Present them for explicit stop review instead.
use super::{Client, UpdateError, impact, invalid};
use crate::{
    chat::{Snapshot, ToolStatus},
    model::TerminalId,
};
use std::collections::BTreeMap;

#[derive(Default)]
pub(in crate::update) struct TurnReview {
    source: Option<Client>,
    observations: BTreeMap<TerminalId, Observation>,
}

struct Observation {
    identity: String,
    activity: TurnActivity,
}

impl Observation {
    fn attach(
        client: &mut crate::control::ControlClient,
        session: crate::chat::Session,
        identity: String,
    ) -> Result<Self, UpdateError> {
        Ok(Self {
            identity,
            activity: client
                .observe_update_chat(session)
                .map_err(|error| invalid(error.to_string()))?
                .into(),
        })
    }

    fn refresh(
        &mut self,
        client: &mut crate::control::ControlClient,
        id: TerminalId,
    ) -> Result<(), UpdateError> {
        if let Some(snapshot) = client
            .read_update_chat(id, self.activity.generation)
            .map_err(|error| invalid(error.to_string()))?
        {
            if snapshot.generation <= self.activity.generation {
                return Err(invalid(
                    "The waiting conversation's publication did not advance",
                ));
            }
            self.activity = snapshot.into();
        }
        Ok(())
    }
}

/// Discard transcript payloads immediately. No update wait retains history.
struct TurnActivity {
    generation: u64,
    parent_finished: bool,
    background: Vec<String>,
    interrupted: Vec<String>,
}

impl From<Snapshot> for TurnActivity {
    fn from(snapshot: Snapshot) -> Self {
        let interrupted = snapshot
            .subagents
            .iter()
            .filter(|child| child.stopped_by_interrupt(&snapshot.capabilities))
            .map(|child| child.id.clone())
            .collect();
        Self {
            generation: snapshot.generation,
            parent_finished: snapshot.ready && !snapshot.exited && !snapshot.turn_accepted(),
            background: snapshot
                .subagents
                .into_iter()
                .filter(|child| child.background && child.status == ToolStatus::Running)
                .map(|child| child.id)
                .collect(),
            interrupted,
        }
    }
}

impl TurnActivity {
    fn review(&self, item: &mut impact::SessionImpact) {
        if item.disposition == impact::Disposition::Interrupted {
            // Codex parent interruption spares background children. Disclose
            // their exact IDs in this review before the updater can stop them
            // individually to settle the conversation for a legacy prepare.
            item.interrupted_subagents = self.background.clone();
            if !self.background.is_empty() {
                item.reason = format!(
                    "The active turn and {} running background subagent(s) will stop; background work will not resume automatically.",
                    self.background.len()
                );
            }
        } else if self.parent_finished && !self.background.is_empty() {
            item.disposition = impact::Disposition::Blocked;
            item.reason = "Its turn finished, but background agents are still running. Review stopping them to continue the update.".into();
            item.interrupted_subagents = self.background.clone();
        } else if !self.parent_finished {
            // Disclose interruption's complete scope before offering `i`.
            // These are exact observed IDs, not the source's global count.
            item.interrupted_subagents = self.interrupted.clone();
        }
    }
}

impl TurnReview {
    pub(in crate::update) fn apply(
        &mut self,
        mut preview: impact::Preview,
    ) -> Result<impact::Preview, UpdateError> {
        let Some(daemon) = preview.daemon.as_mut() else {
            self.clear();
            return Ok(preview);
        };
        let waiting: BTreeMap<_, _> = daemon
            .sessions
            .iter()
            .filter(|item| {
                daemon.capability.background_subagents > 0
                    && item.kind == impact::SessionKind::NativeChat
                    && matches!(
                        (daemon.policy, item.disposition),
                        (impact::Policy::WhenIdle, impact::Disposition::WaitsForIdle)
                            | (impact::Policy::Interrupt, impact::Disposition::Interrupted)
                    )
            })
            .map(|item| (item.id, item.identity.clone()))
            .collect();
        if waiting.is_empty() {
            self.clear();
            return Ok(preview);
        }
        let same_owner = self
            .source
            .as_ref()
            .is_some_and(|source| *source.proof() == daemon.capability.proof);
        let same_inventory = waiting.len() == self.observations.len()
            && waiting.iter().all(|(id, identity)| {
                self.observations
                    .get(id)
                    .is_some_and(|observed| observed.identity == *identity)
            });
        if !same_owner || !same_inventory {
            // Closing the connection removes every subscription. A changed
            // owner never inherits another chat's accepted-turn observation.
            self.clear();
            self.source = Some(super::super::source_client_for(daemon)?);
        }
        let Some(Client::Current { client, peer }) = self.source.as_mut() else {
            // Older frozen clients have no qualified resident-snapshot reader.
            return Ok(preview);
        };
        peer.verify(client.proof())?;
        if self.observations.is_empty() {
            let chats = client
                .list_chats()
                .map_err(|error| invalid(error.to_string()))?;
            for (id, identity) in &waiting {
                let mut matches = chats.iter().filter(|chat| chat.id == *id);
                let session = matches
                    .next()
                    .filter(|session| session.proof.as_ref() == Some(&daemon.capability.proof))
                    .ok_or_else(|| invalid("The waiting conversation's source owner changed"))?;
                if matches.next().is_some() {
                    return Err(invalid("The waiting conversation's source is ambiguous"));
                }
                self.observations.insert(
                    *id,
                    Observation::attach(client, session.clone(), identity.clone())?,
                );
            }
        } else {
            for (id, observed) in &mut self.observations {
                observed.refresh(client, *id)?;
            }
        }
        peer.verify(client.proof())?;
        for item in &mut daemon.sessions {
            if let Some(observed) = self.observations.get(&item.id)
                && item.kind == impact::SessionKind::NativeChat
            {
                observed.activity.review(item);
            }
        }
        Ok(preview)
    }

    fn clear(&mut self) {
        self.source = None;
        self.observations.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::ControlClient,
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe},
        terminal_ipc::{
            ClientEnvelope, ClientRequest, ServerEnvelope, ServerMessage, ServerResponse,
            read_frame, write_frame,
        },
    };
    use std::{
        os::unix::net::UnixStream,
        thread,
        time::{Duration, Instant},
    };

    fn child(status: ToolStatus) -> crate::chat::Subagent {
        crate::chat::Subagent {
            id: "background-child".into(),
            kind: String::new(),
            description: String::new(),
            status,
            model: None,
            waiting: false,
            detail: String::new(),
            background: true,
            items: Vec::new(),
        }
    }

    fn item() -> impact::SessionImpact {
        impact::SessionImpact {
            id: TerminalId::new(),
            kind: impact::SessionKind::NativeChat,
            title: "Offline Codex".into(),
            identity: "exact-source-owner".into(),
            disposition: impact::Disposition::WaitsForIdle,
            reason: "Finishing its turn".into(),
            interrupted_subagents: Vec::new(),
        }
    }

    #[test]
    fn ended_parent_with_running_child_requires_stop_review_instead_of_waiting_forever() {
        let snapshot = Snapshot {
            ready: true,
            busy: false,
            generation: 9,
            subagents: vec![child(ToolStatus::Running)],
            ..Default::default()
        };
        assert!(
            snapshot.is_working(),
            "old sources use the whole card's activity"
        );
        let mut impact = item();
        TurnActivity::from(snapshot).review(&mut impact);
        assert_eq!(impact.disposition, impact::Disposition::Blocked);
        assert_eq!(impact.identity, "exact-source-owner");
        assert_eq!(impact.interrupted_subagents, ["background-child"]);
        assert!(impact.reason.contains("turn finished"));
    }

    #[test]
    fn working_review_discloses_exact_background_interruption_before_the_choice() {
        let mut snapshot = Snapshot {
            ready: true,
            busy: true,
            subagents: vec![child(ToolStatus::Running), child(ToolStatus::Succeeded)],
            ..Default::default()
        };
        snapshot.capabilities.interrupt_stops_background = true;
        let mut impact = item();
        TurnActivity::from(snapshot).review(&mut impact);
        assert_eq!(impact.disposition, impact::Disposition::WaitsForIdle);
        assert_eq!(impact.interrupted_subagents, ["background-child"]);
    }

    #[test]
    fn busy_questions_and_unready_or_exited_sources_never_waive_the_idle_wait() {
        for (busy, waiting, ready, exited) in [
            (true, false, true, false),
            (false, true, true, false),
            (false, false, false, false),
            (false, false, true, true),
        ] {
            let snapshot = Snapshot {
                busy,
                waiting,
                ready,
                exited,
                subagents: vec![child(ToolStatus::Running)],
                ..Default::default()
            };
            let mut impact = item();
            TurnActivity::from(snapshot).review(&mut impact);
            assert_eq!(impact.disposition, impact::Disposition::WaitsForIdle);
            assert!(impact.interrupted_subagents.is_empty());
        }
        let mut impact = item();
        TurnActivity::from(Snapshot {
            ready: true,
            requests: vec![crate::chat::Request {
                kind: crate::chat::RequestKind::Question,
                id: "unanswered".into(),
                title: "Input needed".into(),
                description: String::new(),
                fields: Vec::new(),
                actions: Vec::new(),
            }],
            subagents: vec![child(ToolStatus::Running)],
            ..Default::default()
        })
        .review(&mut impact);
        assert_eq!(impact.disposition, impact::Disposition::WaitsForIdle);
    }

    #[test]
    fn finished_children_do_not_create_a_stop_or_override_the_sources_disposition() {
        let mut impact = item();
        TurnActivity::from(Snapshot {
            ready: true,
            subagents: vec![child(ToolStatus::Succeeded)],
            ..Default::default()
        })
        .review(&mut impact);
        assert_eq!(impact.disposition, impact::Disposition::WaitsForIdle);
        assert!(impact.interrupted_subagents.is_empty());
    }

    #[test]
    fn interruption_review_discloses_codex_children_that_parent_interrupt_spares() {
        let mut impact = item();
        impact.disposition = impact::Disposition::Interrupted;
        TurnActivity::from(Snapshot {
            ready: true,
            busy: true,
            subagents: vec![child(ToolStatus::Running)],
            ..Default::default()
        })
        .review(&mut impact);
        assert_eq!(impact.disposition, impact::Disposition::Interrupted);
        assert_eq!(impact.interrupted_subagents, ["background-child"]);
        assert!(impact.reason.contains("background work will not resume"));
        assert_eq!(impact.identity, "exact-source-owner");
    }

    #[test]
    fn protocol_156_turn_observer_attaches_once_then_reads_only_new_generations() {
        let (stream, mut server_stream) = UnixStream::pair().unwrap();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("source"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("daemon"),
        };
        let session = crate::chat::Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: LaunchRecipe {
                    program: "offline-codex".into(),
                    arguments: Vec::new(),
                    working_directory: "/work".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some("parent-thread".into()),
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let source = session.clone();
        let server = thread::spawn(move || {
            let response = |stream: &mut UnixStream, request_id, response| {
                write_frame(
                    stream,
                    &ServerEnvelope {
                        version: 156,
                        request_id: Some(request_id),
                        proof: proof.clone(),
                        message: ServerMessage::Response { response },
                    },
                )
                .unwrap();
            };
            let connect: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(matches!(connect.request, ClientRequest::Connect { .. }));
            response(
                &mut server_stream,
                connect.request_id,
                ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: Vec::new(),
                },
            );
            let attach: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(
                attach.request,
                ClientRequest::ChatAttach {
                    session: source.clone()
                }
            );
            response(
                &mut server_stream,
                attach.request_id,
                ServerResponse::ChatAttached {
                    session: source.clone(),
                    snapshot: Snapshot {
                        generation: 7,
                        ready: true,
                        busy: true,
                        subagents: vec![child(ToolStatus::Running)],
                        ..Default::default()
                    },
                },
            );
            let visible: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(
                visible.request,
                ClientRequest::ChatVisibility {
                    chat_id: source.id,
                    visible: true
                }
            );
            response(
                &mut server_stream,
                visible.request_id,
                ServerResponse::Accepted,
            );
            let metadata = |generation, busy| Snapshot {
                generation,
                ready: true,
                busy,
                subagents: vec![child(ToolStatus::Running)],
                ..Default::default()
            };
            let delta = |base_generation, generation| ServerResponse::ChatUpdate {
                chat_id: source.id,
                update: crate::chat::presentation::Presentation {
                    base_generation,
                    file_changes_unchanged: true,
                    metadata: metadata(generation, false),
                    order: Vec::new(),
                    changes: Vec::new(),
                    children: Vec::new(),
                },
            };
            // A matching base receives a delta; missed publications receive a
            // full resync. Neither an unrelated delta nor a nonadvancing one
            // can establish that this observed parent finished its turn.
            for (generation, publication) in [
                (
                    7,
                    ServerResponse::ChatSnapshot {
                        chat_id: source.id,
                        snapshot: None,
                    },
                ),
                (7, delta(7, 8)),
                (
                    8,
                    ServerResponse::ChatSnapshot {
                        chat_id: source.id,
                        snapshot: Some(metadata(9, true)),
                    },
                ),
                (9, delta(7, 10)),
                (9, delta(9, 9)),
            ] {
                let read: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                assert_eq!(
                    read.request,
                    ClientRequest::ChatRead {
                        chat_id: source.id,
                        after_generation: Some(generation)
                    }
                );
                // Real sources interleave subscribed notifications with replies.
                write_frame(
                    &mut server_stream,
                    &ServerEnvelope {
                        version: 156,
                        request_id: None,
                        proof: proof.clone(),
                        message: ServerMessage::ChatChanged { chat_id: source.id },
                    },
                )
                .unwrap();
                write_frame(
                    &mut server_stream,
                    &ServerEnvelope {
                        version: 156,
                        request_id: None,
                        proof: proof.clone(),
                        message: ServerMessage::ChatAccountUsage {
                            chat_id: source.id,
                            report: crate::account::LiveReport::CodexLogin(
                                crate::account::codex::LoginUpdate {
                                    chatgpt: true,
                                    plan: None,
                                },
                            ),
                            observed_at: 1,
                        },
                    },
                )
                .unwrap();
                response(&mut server_stream, read.request_id, publication);
            }
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &ServerEnvelope {
                    version: 156,
                    request_id: None,
                    proof: proof.clone(),
                    message: ServerMessage::ChatAccountUsage {
                        chat_id: TerminalId::new(),
                        report: crate::account::LiveReport::CodexLogin(
                            crate::account::codex::LoginUpdate {
                                chatgpt: true,
                                plan: None,
                            },
                        ),
                        observed_at: 2,
                    },
                },
            )
            .unwrap();
        });
        let mut client = ControlClient::from_stream_at_source_protocol(
            stream,
            Instant::now() + Duration::from_secs(5),
            156,
        )
        .unwrap();
        let mut observed =
            Observation::attach(&mut client, session.clone(), "issued-owner".into()).unwrap();
        let mut busy = item();
        observed.activity.review(&mut busy);
        assert_eq!(busy.disposition, impact::Disposition::WaitsForIdle);
        observed.refresh(&mut client, session.id).unwrap();
        assert_eq!(observed.activity.generation, 7);
        assert!(!observed.activity.parent_finished);
        observed.refresh(&mut client, session.id).unwrap();
        assert_eq!(observed.activity.generation, 8);
        let mut ended = item();
        observed.activity.review(&mut ended);
        assert_eq!(ended.disposition, impact::Disposition::Blocked);
        assert_eq!(ended.interrupted_subagents, ["background-child"]);
        observed.refresh(&mut client, session.id).unwrap();
        assert_eq!(observed.activity.generation, 9);
        assert!(
            !observed.activity.parent_finished,
            "full resync observes a new active turn"
        );
        for _ in 0..2 {
            assert!(observed.refresh(&mut client, session.id).is_err());
            assert_eq!(observed.activity.generation, 9);
            assert!(
                !observed.activity.parent_finished,
                "invalid deltas cannot waive the idle wait"
            );
        }
        let unrelated = observed.refresh(&mut client, session.id).unwrap_err();
        assert!(unrelated.to_string().contains("chat_account_usage"));
        assert_eq!(observed.activity.generation, 9);
        drop(client);
        server.join().unwrap();
    }
}
