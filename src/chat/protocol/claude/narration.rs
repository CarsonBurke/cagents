//! Which of Claude's thinking blocks are narration rather than reasoning.
//!
//! Some models send the prose they write between tool calls ("Now I'll check
//! the log…") as a `thinking` block. The server records the block's kind in
//! its signature, and Claude Code reports the narration blocks of an
//! assistant frame as `narration_block_indexes` beside its `message`: indexes
//! into that frame's `message.content`. A saved transcript keeps only the
//! signature, so a block the frame doesn't list is read from its signature.
//!
//! Both fail closed, as Claude Code's own reader does: a frame's list is the
//! whole answer when it has one, and a signature that is missing, malformed
//! or of any other kind leaves the block thinking. Reasoning drawn as
//! narration would put the model's private working in the transcript's prose;
//! narration drawn as reasoning is only folded away.

use base64::Engine as _;
use serde_json::Value;

/// The block kind the server writes for narration.
const NARRATION: &[u8] = b"narration";

/// Signatures seen so far are about 1.5 KiB; anything far larger is not one.
const MAX_SIGNATURE: usize = 64 * 1024;

/// Whether `block`, at `position` in `frame`'s `message.content`, is
/// narration to show as the agent's commentary.
///
/// Only a `thinking` block with text qualifies: narration without text has
/// nothing to show, and Claude Code treats it as thinking too.
pub(super) fn is_narration(frame: &Value, position: usize, block: &Value) -> bool {
    if block["type"].as_str() != Some("thinking")
        || block["thinking"]
            .as_str()
            .is_none_or(|text| text.trim().is_empty())
    {
        return false;
    }
    match frame["narration_block_indexes"].as_array() {
        Some(indexes) => indexes
            .iter()
            .any(|index| index.as_u64() == Some(position as u64)),
        None => block["signature"].as_str().is_some_and(signed_as_narration),
    }
}

/// Whether a thinking block's signature names its kind as narration.
///
/// The signature is base64 of a protobuf message whose field 2 is an
/// envelope; the envelope's field 1 is a header whose string field 8 is the
/// block's kind, `thinking` or `narration`. Every field on that path must
/// occur exactly once and every message on it must be well formed.
fn signed_as_narration(signature: &str) -> bool {
    if signature.len() > MAX_SIGNATURE {
        return false;
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(signature) else {
        return false;
    };
    field(&bytes, 2)
        .and_then(|envelope| field(envelope, 1))
        .and_then(|header| field(header, 8))
        == Some(NARRATION)
}

/// The payload of the one length-delimited field `number` in `message`.
///
/// `None` when the field is absent, repeated, not length-delimited, or when
/// anything in the message fails to parse.
fn field(message: &[u8], number: u64) -> Option<&[u8]> {
    let mut rest = message;
    let mut found = None;
    while !rest.is_empty() {
        let tag = varint(&mut rest)?;
        if tag >> 3 == 0 {
            return None;
        }
        let payload = match tag & 7 {
            0 => {
                varint(&mut rest)?;
                None
            }
            1 => Some(take(&mut rest, 8)?),
            2 => {
                let len = usize::try_from(varint(&mut rest)?).ok()?;
                Some(take(&mut rest, len)?)
            }
            5 => Some(take(&mut rest, 4)?),
            // Groups (3 and 4) are long deprecated, and 6 and 7 don't exist.
            _ => return None,
        };
        if tag >> 3 == number {
            if found.is_some() || tag & 7 != 2 {
                return None;
            }
            found = payload;
        }
    }
    found
}

fn take<'a>(rest: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    if rest.len() < len {
        return None;
    }
    let (taken, after) = rest.split_at(len);
    *rest = after;
    Some(taken)
}

/// A base-128 varint of at most ten bytes that fits in 64 bits.
fn varint(rest: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for (index, &byte) in rest.iter().enumerate().take(10) {
        let bits = u64::from(byte & 0x7f);
        if index == 9 && bits > 1 {
            return None;
        }
        value |= bits << (7 * index);
        if byte & 0x80 == 0 {
            *rest = &rest[index + 1..];
            return Some(value);
        }
    }
    None
}
