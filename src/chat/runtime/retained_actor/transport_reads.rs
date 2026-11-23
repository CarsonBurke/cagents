//! Reads the connection's own worker answers, without the serve loop.
//!
//! The serve loop answers requests one at a time. A transcript read or a
//! changed files query that changes nothing the loop orders used to wait
//! there behind input admission, state polls and other reads, and a message
//! sent meanwhile waited behind the read in turn (#3602). The worker that
//! received such a read answers it itself instead, from a [`View`] of the
//! attachment fence and issued identity that the loop publishes whenever
//! they change. Every write still goes through the loop's single-writer
//! order.
//!
//! A read is answered entirely under one published view: the loop waits for
//! reads in flight before it publishes another, so a read either finishes
//! before an attachment supersedes its own or is refused as superseded.
use super::connections::Answer;
use super::*;
use crate::terminal_ipc::ServerResponse;
use std::sync::RwLock;

/// The view workers answer reads from; `None` once the serve loop stopped.
#[derive(Default)]
pub(super) struct Reads(RwLock<Option<View>>);

/// What a read needs from the serve loop's [`Server`]: the fields it admits
/// a request by and remembers a publication under, and the actor it reads.
struct View {
    epoch: uuid::Uuid,
    proof: HostSessionProof,
    close_only: bool,
    installed: bool,
    source: chat::Session,
    lineage: Vec<chat::Session>,
    actor: LocalHandle,
    journal: Arc<Mutex<Journal>>,
}

impl Reads {
    /// Publish `server`'s fence and identity if they changed since the last
    /// publication. Only the serve loop calls this, after each request it
    /// runs and before replying to it.
    pub(super) fn publish(&self, server: &Server) {
        let published = self.0.read().unwrap_or_else(|p| p.into_inner());
        if published.as_ref().is_some_and(|view| view.shows(server)) {
            return;
        }
        drop(published);
        *self.0.write().unwrap_or_else(|p| p.into_inner()) = Some(View {
            epoch: server.epoch,
            proof: server.proof.clone(),
            close_only: server.close_only,
            installed: server.installed,
            source: server.source.clone(),
            lineage: server.lineage.clone(),
            actor: server.actor.clone(),
            journal: server.journal.clone(),
        });
    }
    /// Answer `request` on this thread if it is a read, or give it back for
    /// the serve loop. Once the loop has stopped every request goes back,
    /// to be dropped unanswered as the loop would have dropped it.
    pub(super) fn answer(&self, request: Invocation) -> Result<Answer, Invocation> {
        if !matches!(
            request.method.as_str(),
            "presentation_codec" | "presentation" | "presentation_binary" | "text_copy" | "file_query"
        ) {
            return Err(request);
        }
        let view = self.0.read().unwrap_or_else(|p| p.into_inner());
        let Some(view) = view.as_ref() else {
            return Err(request);
        };
        let admitted = admit(&request, view.epoch, &view.proof, view.close_only);
        let publisher = view.publisher();
        Ok(if request.method == "presentation_binary" {
            Answer::Binary(
                admitted
                    .and_then(|()| binary(&publisher, request.args))
                    .map(Box::new),
            )
        } else {
            Answer::Value(admitted.and_then(|()| value(&publisher, request)))
        })
    }
}

impl View {
    fn shows(&self, server: &Server) -> bool {
        self.epoch == server.epoch
            && self.proof == server.proof
            && self.close_only == server.close_only
            && self.installed == server.installed
            && self.source == server.source
            && self.lineage == server.lineage
    }
    fn publisher(&self) -> Publisher<'_> {
        Publisher {
            source: &self.source,
            lineage: &self.lineage,
            installed: self.installed,
            journal: &self.journal,
            actor: &self.actor,
        }
    }
}

/// Stops workers answering reads when dropped, however the serve loop ends.
pub(super) struct Serving(pub(super) Arc<Reads>);
impl Drop for Serving {
    fn drop(&mut self) {
        *self.0.0.write().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// Answer an admitted read whose result is JSON.
pub(super) fn value(publisher: &Publisher<'_>, request: Invocation) -> Result<Value, String> {
    fn decode<T: DeserializeOwned>(args: Value) -> Result<T, String> {
        serde_json::from_value(args).map_err(|error| error.to_string())
    }
    fn encode(value: impl Serialize) -> Result<Value, String> {
        serde_json::to_value(value).map_err(|error| error.to_string())
    }
    match request.method.as_str() {
        "presentation_codec" => {
            let (version,) = decode::<(u16,)>(request.args)?;
            encode(version == crate::terminal_ipc::PROTOCOL_VERSION)
        }
        "presentation" => {
            let (id, after) = decode::<(crate::model::TerminalId, Option<u64>)>(request.args)?;
            encode(publisher.presentation(id, after)?)
        }
        "text_copy" => {
            let (source, offset) = decode::<(chat::TextCopySource, u64)>(request.args)?;
            encode(publisher.actor.text_copy(&source, offset)?)
        }
        "file_query" => {
            let (command,) = decode::<(Command,)>(request.args)?;
            encode(publisher.actor.file_query(&command)?)
        }
        method => Err(format!("{method} is not a retained read")),
    }
}

/// Answer an admitted `presentation_binary` read, which the connection
/// encodes and the daemon relays without decoding.
pub(super) fn binary(publisher: &Publisher<'_>, args: Value) -> Result<ServerResponse, String> {
    let (version, id, after) =
        serde_json::from_value::<(u16, crate::model::TerminalId, Option<u64>)>(args)
            .map_err(|error| error.to_string())?;
    if version != crate::terminal_ipc::PROTOCOL_VERSION {
        return Err("Retained binary presentation requires a matching wire version".into());
    }
    Ok(publisher
        .presentation(id, after)?
        .unwrap_or(ServerResponse::ChatSnapshot {
            chat_id: id,
            snapshot: None,
        }))
}
