//! Presentation is encoded by its owner; the daemon relays those bytes.
use super::*;
use crate::terminal_ipc::{PreparedResponse, ServerResponse};

impl RemoteActor {
    pub(in crate::chat::runtime) fn presentation_wire(
        &self,
        id: crate::model::TerminalId,
        after: Option<u64>,
    ) -> Result<PreparedResponse, String> {
        let binary = if let Some(binary) = self.0.binary_codec.get() {
            *binary
        } else {
            let binary = match self.call::<bool>(
                "presentation_codec",
                json!([crate::terminal_ipc::PROTOCOL_VERSION]),
            ) {
                Ok(binary) => binary,
                Err(error) if error == UNKNOWN_METHOD => false,
                Err(error) => return Err(error),
            };
            let _ = self.0.binary_codec.set(binary);
            binary
        };
        if !binary {
            return self
                .call::<Option<ServerResponse>>("presentation", json!([id, after]))
                .map(|response| {
                    PreparedResponse::Decoded(response.unwrap_or(ServerResponse::ChatSnapshot {
                        chat_id: id,
                        snapshot: None,
                    }))
                });
        }
        let deadline = Instant::now() + TIMEOUT;
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
        // Decode only the Result discriminant/error; the transcript stays encoded.
        let (result, rest) =
            postcard::take_from_bytes::<Result<(), String>>(&bytes).map_err(|e| e.to_string())?;
        result?;
        if rest.is_empty() {
            return Err("Retained presentation omitted its payload".into());
        }
        let offset = bytes.len() - rest.len();
        let mut bytes = bytes;
        bytes.drain(..offset);
        Ok(PreparedResponse::EncodedChat { chat_id: id, bytes })
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
