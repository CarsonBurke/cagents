//! Reads for the person looking at the chat: the transcript, which its owner
//! encodes and the daemon relays as bytes, and the changed files panel.
use super::*;
use crate::chat::runtime::ReadFailure;
use crate::terminal_ipc::{PreparedResponse, ServerResponse};

impl RemoteActor {
    /// The conversation as the reader should show it. A read changes nothing
    /// in the helper, so a connection that fails while the helper is alive is
    /// tried again within [`READ_WAIT`]. A read that still fails while
    /// the helper is alive asks the reader to read again shortly, so its
    /// failure clears once the helper answers.
    pub(in crate::chat::runtime) fn presentation_wire(
        &self,
        id: crate::model::TerminalId,
        after: Option<u64>,
    ) -> Result<PreparedResponse, ReadFailure> {
        self.presentation_by(id, after, Instant::now() + READ_WAIT)
    }
    pub(super) fn presentation_by(
        &self,
        id: crate::model::TerminalId,
        after: Option<u64>,
        deadline: Instant,
    ) -> Result<PreparedResponse, ReadFailure> {
        self.read_by("presentation", deadline, |deadline| {
            self.presentation_until(id, after, deadline)
        })
        .inspect_err(|failure| {
            if failure.transient {
                self.reread_after(PRESENTATION_RETRY);
            }
        })
    }
    /// The changed files the panel asked for, from the helper's cached edit
    /// metadata. Like the transcript, the query changes nothing in the
    /// helper, so it is tried again within [`READ_WAIT`] while the helper is
    /// alive; the panel shows a failure that outlasts that.
    pub(in crate::chat::runtime) fn file_query(
        &self,
        command: &crate::chat::Command,
    ) -> Result<Option<crate::chat::FileChangeReply>, ReadFailure> {
        self.file_query_by(command, Instant::now() + READ_WAIT)
    }
    pub(super) fn file_query_by(
        &self,
        command: &crate::chat::Command,
        deadline: Instant,
    ) -> Result<Option<crate::chat::FileChangeReply>, ReadFailure> {
        self.read_by("file_query", deadline, |deadline| {
            self.answer_until("file_query", json!([command]), deadline)
        })
    }
    /// Make a read-only request until it is answered or `deadline` passes.
    /// `attempt` keeps the helper's refusal, which is returned at once, apart
    /// from a failed exchange, which is tried again while the helper is alive.
    fn read_by<T>(
        &self,
        what: &str,
        deadline: Instant,
        mut attempt: impl FnMut(Instant) -> Result<Result<T, String>, String>,
    ) -> Result<T, ReadFailure> {
        let mut pause = RETRANSMIT_DELAY;
        loop {
            let error = match attempt(deadline) {
                Ok(answer) => return answer.map_err(ReadFailure::from),
                Err(error) => error,
            };
            let alive =
                !self.0.stop.load(Ordering::Acquire) && self.0.identity.record.holder.is_current();
            if alive && deadline.saturating_duration_since(Instant::now()) > pause {
                tracing::debug!(%error, what, "a retained read failed; reading again");
                thread::sleep(pause);
                pause = pause.saturating_mul(2).min(Duration::from_secs(1));
                continue;
            }
            if alive {
                tracing::warn!(%error, what, "a retained read failed while its helper is alive");
            }
            return Err(ReadFailure {
                message: error,
                transient: alive,
            });
        }
    }
    /// One read within `deadline`. The outer error means the exchange
    /// failed; the inner one is the helper's refusal.
    fn presentation_until(
        &self,
        id: crate::model::TerminalId,
        after: Option<u64>,
        deadline: Instant,
    ) -> Result<Result<PreparedResponse, String>, String> {
        let binary = if let Some(binary) = self.0.binary_codec.get() {
            *binary
        } else {
            let binary = match self.answer_until::<bool>(
                "presentation_codec",
                json!([crate::terminal_ipc::PROTOCOL_VERSION]),
                deadline,
            )? {
                Ok(binary) => binary,
                Err(error) if error == UNKNOWN_METHOD => false,
                Err(error) => return Ok(Err(error)),
            };
            let _ = self.0.binary_codec.set(binary);
            binary
        };
        if !binary {
            return Ok(self
                .answer_until::<Option<ServerResponse>>(
                    "presentation",
                    json!([id, after]),
                    deadline,
                )?
                .map(|response| {
                    PreparedResponse::Decoded(response.unwrap_or(ServerResponse::ChatSnapshot {
                        chat_id: id,
                        snapshot: None,
                    }))
                }));
        }
        let bytes = (|| {
            #[cfg(debug_assertions)]
            super::super::fixture_refusal::hold_read(deadline)?;
            let control =
                registry::connect_until(&self.0.socket, deadline).map_err(|e| e.to_string())?;
            verify_peer(&control, &self.0.identity.record)?;
            deadline_io::send_until(
                &control,
                &Invocation {
                    proof: self.0.proof.clone(),
                    epoch: Some(self.0.epoch),
                    method: "presentation_binary".into(),
                    args: json!([crate::terminal_ipc::PROTOCOL_VERSION, id, after]),
                },
                deadline,
            )?;
            let bytes = deadline_io::receive_bytes_until(&control, deadline)?;
            verify_peer(&control, &self.0.identity.record)?;
            Ok(bytes)
        })()
        .map_err(|error| interrupted_request("presentation_binary", error))?;
        // Decode only the Result discriminant/error; the transcript stays encoded.
        let (result, rest) =
            postcard::take_from_bytes::<Result<(), String>>(&bytes).map_err(|e| e.to_string())?;
        if let Err(refusal) = result {
            return Ok(Err(refusal));
        }
        if rest.is_empty() {
            return Ok(Err("Retained presentation omitted its payload".into()));
        }
        let offset = bytes.len() - rest.len();
        let mut bytes = bytes;
        bytes.drain(..offset);
        Ok(Ok(PreparedResponse::EncodedChat { chat_id: id, bytes }))
    }
    /// Ask the reader to read the conversation again after `delay`, unless
    /// it is already due to sooner.
    fn reread_after(&self, delay: Duration) {
        let at = Instant::now() + delay;
        let mut reread = self.0.reread_at.lock().unwrap_or_else(|p| p.into_inner());
        if reread.is_none_or(|due| due > at) {
            *reread = Some(at);
        }
        drop(reread);
        self.0.wake.notify();
    }
    /// Tell the reader the conversation changed once a read asked for after a
    /// failure is due, so it reads again.
    pub(super) fn ask_for_due_reread(&self) {
        let due = {
            let mut reread = self.0.reread_at.lock().unwrap_or_else(|p| p.into_inner());
            reread.take_if(|at| *at <= Instant::now()).is_some()
        };
        if due && !self.0.stop.load(Ordering::Acquire) {
            let _publication = self.0.publication.lock().unwrap_or_else(|p| p.into_inner());
            (self.0.notify)(Update::Presentation);
        }
    }
}
impl Server {
    pub(super) fn binary_presentation(
        &mut self,
        request: Invocation,
    ) -> Result<ServerResponse, String> {
        // Reuse the same epoch/proof/close-only admission as every invocation.
        let (version, id, after) =
            serde_json::from_value::<(u16, crate::model::TerminalId, Option<u64>)>(
                request.args.clone(),
            )
            .map_err(|e| e.to_string())?;
        self.invoke(Invocation {
            method: "presentation_codec".into(),
            args: json!([version]),
            ..request
        })?;
        if version != crate::terminal_ipc::PROTOCOL_VERSION {
            return Err("Retained binary presentation requires a matching wire version".into());
        }
        Ok(self
            .presentation_response(id, after)?
            .unwrap_or(ServerResponse::ChatSnapshot {
                chat_id: id,
                snapshot: None,
            }))
    }
}
