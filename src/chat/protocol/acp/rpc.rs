//! JSON-RPC 2.0 framing for ACP's newline-delimited stdio transport.
//!
//! The agent is both a server and a client: cagents calls its methods, and it
//! calls back for permission and, were cagents to advertise them, files and
//! terminals. Every frame is therefore classified before it is interpreted.
use serde_json::{Value, json};

/// JSON-RPC's reserved code for a method the receiver does not implement.
pub(super) const METHOD_NOT_FOUND: i64 = -32601;
pub(super) const INVALID_PARAMS: i64 = -32602;
/// ACP's code for a request that ended because it was cancelled.
pub(super) const REQUEST_CANCELLED: i64 = -32800;
/// ACP's code for a request the agent refuses until the user signs in.
pub(super) const AUTH_REQUIRED: i64 = -32000;

/// What one incoming line is, by the members JSON-RPC defines.
pub(super) enum Frame<'a> {
    /// The agent calls back; it waits for an answer with this exact `id`.
    Request {
        id: &'a Value,
        method: &'a str,
        params: &'a Value,
    },
    Notification {
        method: &'a str,
        params: &'a Value,
    },
    /// The answer to one of cagents' own requests, by the number it was sent
    /// with. A reply whose identifier cagents never issued has none.
    Response {
        id: Option<u64>,
        outcome: Result<&'a Value, &'a Value>,
    },
    /// Valid JSON that is none of the above. JSON-RPC lets a receiver ignore
    /// it, and an agent's private extension frames would otherwise end the
    /// conversation.
    Other,
}

pub(super) fn classify(frame: &Value) -> Frame<'_> {
    match (frame.get("method").and_then(Value::as_str), frame.get("id")) {
        (Some(method), Some(id)) if !id.is_null() => Frame::Request {
            id,
            method,
            params: &frame["params"],
        },
        (Some(method), _) => Frame::Notification {
            method,
            params: &frame["params"],
        },
        (None, Some(id)) => match (frame.get("result"), frame.get("error")) {
            (Some(result), None) => Frame::Response {
                id: id.as_u64(),
                outcome: Ok(result),
            },
            (_, Some(error)) => Frame::Response {
                id: id.as_u64(),
                outcome: Err(error),
            },
            _ => Frame::Other,
        },
        _ => Frame::Other,
    }
}

pub(super) fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

pub(super) fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// Answers an agent's request with the identifier exactly as it was sent,
/// whether a number or a string.
pub(super) fn result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub(super) fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The agent's own words for an error reply, which is what a reader is shown.
pub(super) fn error_message(error: &Value) -> String {
    let message = error_summary(error);
    match error_detail(error) {
        Some(detail) => format!("{message}: {detail}"),
        None => message.to_owned(),
    }
}

/// An error reply's `message`, which for an agent's internal error is often
/// only the JSON-RPC code's name.
fn error_summary(error: &Value) -> &str {
    error["message"]
        .as_str()
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or("The agent reported an error without a message")
}

/// What an error reply's `data` says beyond its `message`, if anything.
fn error_detail(error: &Value) -> Option<&str> {
    let message = error_summary(error);
    error["data"]["message"]
        .as_str()
        .or_else(|| error["data"]["details"].as_str())
        .or_else(|| error["data"].as_str())
        .map(str::trim)
        .filter(|detail| !detail.is_empty() && *detail != message)
}

/// An error reply's most specific words: its detail, or its message.
pub(super) fn error_reason(error: &Value) -> &str {
    error_detail(error).unwrap_or_else(|| error_summary(error))
}

/// Whether a non-JSON stdout line contains what looks like a JSON-RPC frame,
/// which the transport must not drop silently: losing a reply would leave
/// the conversation waiting for it forever.
pub(super) fn embedded_frame(line: &[u8]) -> bool {
    // Inspect a bounded number of objects, however many braces a line has.
    const CANDIDATES: usize = 64;
    if !line.windows(9).any(|window| window == b"\"jsonrpc\"") {
        return false;
    }
    let mut start = 0;
    for _ in 0..CANDIDATES {
        let Some(offset) = line[start..].iter().position(|byte| *byte == b'{') else {
            return false;
        };
        let at = start + offset;
        let mut values = serde_json::Deserializer::from_slice(&line[at..]).into_iter::<Value>();
        match values.next() {
            Some(Ok(value)) => {
                if value.get("jsonrpc").is_some() {
                    return true;
                }
                start = at + values.byte_offset();
            }
            _ => start = at + 1,
        }
    }
    false
}
