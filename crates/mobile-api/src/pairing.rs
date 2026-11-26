//! Pairing a phone: how a phone with no key on the computer yet gets one.
//!
//! The computer shows a pairing code, a [`PAIRING_SCHEME`] URI, usually as a
//! QR code. It names the addresses, port and user to connect to, the
//! computer's host key, and a one-time Ed25519 key that the computer has
//! authorized, for a few minutes, to run only `cagents daemon bridge --pair`.
//! The phone pins the host key from the code, connects with the one-time
//! key, and sends one [`PairClientLine`] carrying its own public key. The
//! person confirms on the computer, which then authorizes the phone's key
//! for `cagents daemon bridge --mobile` alone, and the phone reconnects with
//! it.
//!
//! The exchange is JSON lines, like the rest of the API, but it has no
//! `hello`: a pairing connection does nothing else, and its messages only
//! grow, as [the crate's](crate) do.

use serde::{Deserialize, Serialize};

/// The URI scheme of a pairing code.
pub const PAIRING_SCHEME: &str = "cagents-mobile";
/// The pairing code's format version, its `v` parameter.
pub const PAIRING_CODE_VERSION: u32 = 1;
/// The longest line a phone may send while pairing, newline excluded.
pub const MAX_PAIR_LINE_BYTES: usize = 16 * 1024;
/// The longest device name the computer keeps, in characters.
pub const MAX_DEVICE_NAME_CHARS: usize = 48;

/// The phone's one line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PairClientLine {
    Pair(PairRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    /// The phone's own public key, in OpenSSH form: `ssh-ed25519 AAAA…`.
    /// Only Ed25519 keys are accepted.
    pub key: String,
    /// What the computer calls the phone, such as `Pixel 9`. The computer
    /// keeps letters, digits, spaces and common punctuation, at most
    /// [`MAX_DEVICE_NAME_CHARS`] of them.
    pub name: String,
}

/// What the computer answers, in order: [`PairServerLine::PairWaiting`] once
/// the request is shown to the person, then exactly one of the others, after
/// which the computer closes the connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PairServerLine {
    /// The person is being asked to allow the phone.
    PairWaiting {},
    /// The phone's key is authorized: connect with it and run
    /// `cagents daemon bridge --mobile`, using the host, port and user from
    /// the pairing code.
    Paired(Paired),
    /// The person chose not to allow the phone.
    PairDeclined {},
    /// Pairing didn't happen, for the reason given, written for the person.
    PairError(PairError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paired {
    /// The computer's host name, as the bridge's `welcome` reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairError {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_lines_have_their_documented_shape() {
        let line: PairClientLine = serde_json::from_str(
            r#"{"type":"pair","key":"ssh-ed25519 AAAA","name":"Pixel 9","later":1}"#,
        )
        .unwrap();
        assert_eq!(
            line,
            PairClientLine::Pair(PairRequest {
                key: "ssh-ed25519 AAAA".into(),
                name: "Pixel 9".into(),
            })
        );
        for (line, json) in [
            (PairServerLine::PairWaiting {}, r#"{"type":"pair_waiting"}"#),
            (
                PairServerLine::Paired(Paired {
                    host: Some("workstation".into()),
                }),
                r#"{"type":"paired","host":"workstation"}"#,
            ),
            (PairServerLine::PairDeclined {}, r#"{"type":"pair_declined"}"#),
            (
                PairServerLine::PairError(PairError {
                    message: "expired".into(),
                }),
                r#"{"type":"pair_error","message":"expired"}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&line).unwrap(), json);
            assert_eq!(serde_json::from_str::<PairServerLine>(json).unwrap(), line);
        }
    }
}
