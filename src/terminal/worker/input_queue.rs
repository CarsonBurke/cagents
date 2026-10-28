//! Bounded PTY frames retain pair ownership after command encoding.
use super::{
    KeyAction, KeyInput, MouseAction, MouseButton, MouseInput, PTY_WRITE_CAPACITY, admission,
};
use std::{collections::VecDeque, sync::Arc};

const HELD_LIMIT: usize = 512;
const FRAME_LIMIT: usize = 4096;
const RELEASE_BYTES: usize = 128;

use super::admission::Identity;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    User,
    Text,
    Reply,
    Packet,
    Paste,
    Finish,
    Press(u64),
    Repeat(u64),
    Release(u64),
}
#[derive(Debug)]
#[cfg_attr(test, derive(Clone))]
struct Frame {
    bytes: Vec<u8>,
    offset: usize,
    kind: Kind,
    paired: bool,
    // The command's aggregate credit survives transfer into its encoded bytes.
    _permit: Option<Arc<admission::Permit>>,
}
#[derive(Debug)]
#[cfg_attr(test, derive(Clone))]
struct Owner {
    token: u64,
    identity: Identity,
    escaped: bool,
    paired: bool,
    releasing: bool,
    claim: Option<Arc<admission::Cleanup>>,
}

#[derive(Debug, Default)]
#[cfg_attr(test, derive(Clone))]
pub(super) struct PendingInput {
    frames: VecDeque<Frame>,
    owners: Vec<Owner>,
    bytes: usize,
    next: u64,
    // No child-consumption acknowledgement exists. Never infer that a sent
    // paired frame is safe to flush from an empty userspace or kernel queue.
    kernel_protected: bool,
}

impl PendingInput {
    pub(super) fn len(&self) -> usize {
        self.bytes
    }
    pub(super) fn is_empty(&self) -> bool {
        self.bytes == 0
    }
    pub(super) fn has_held_input(&self) -> bool {
        !self.owners.is_empty()
    }
    pub(super) fn has_key_owner(&self, key: &KeyInput) -> bool {
        self.owners
            .iter()
            .any(|owner| owner.identity == Identity::key(key) && !owner.releasing)
    }
    fn requires_release_protocol(
        &self,
        identity: Identity,
        claim: Option<&Arc<admission::Cleanup>>,
    ) -> bool {
        self.owners.iter().any(|owner| owner.identity == identity && !owner.releasing && claim.is_none_or(|expected| owner.claim.as_ref().is_some_and(|actual| Arc::ptr_eq(expected, actual))) && owner.paired && (owner.escaped || self.frames.iter().any(|frame| matches!(frame.kind, Kind::Press(id) | Kind::Repeat(id) if id == owner.token))))
    }
    pub(super) fn key_release_protocol_unknown(
        &self,
        input: &KeyInput,
        encoded: &[u8],
        claim: Option<&Arc<admission::Cleanup>>,
    ) -> bool {
        input.action == KeyAction::Release
            && encoded.is_empty()
            && self.requires_release_protocol(Identity::key(input), claim)
    }
    pub(super) fn mouse_release_protocol_unknown(
        &self,
        input: &MouseInput,
        encoded: &[u8],
        claim: Option<&Arc<admission::Cleanup>>,
    ) -> bool {
        input.action == MouseAction::Release
            && encoded.is_empty()
            && input.button.is_some_and(|button| {
                self.requires_release_protocol(Identity::Mouse(button), claim)
            })
    }
    pub(super) fn can_flush_kernel(&self) -> bool {
        !self.kernel_protected && !self.owners.iter().any(|owner| owner.escaped)
    }
    pub(super) fn front(&self) -> &[u8] {
        self.frames
            .front()
            .map_or(&[], |frame| &frame.bytes[frame.offset..])
    }
    #[cfg(test)]
    pub(super) fn clear(&mut self) {
        self.frames.clear();
        self.owners.clear();
        self.bytes = 0;
    }
    fn reservations(&self) -> usize {
        self.owners.iter().filter(|owner| !owner.releasing).count()
    }
    fn fits(&self, bytes: usize, frames: usize, reservations: usize) -> bool {
        self.bytes
            .saturating_add(bytes)
            .saturating_add(reservations.saturating_mul(RELEASE_BYTES))
            <= PTY_WRITE_CAPACITY
            && self
                .frames
                .len()
                .saturating_add(frames)
                .saturating_add(reservations)
                <= FRAME_LIMIT
    }
    fn append(
        &mut self,
        bytes: &[u8],
        kind: Kind,
        paired: bool,
        permit: Option<Arc<admission::Permit>>,
    ) {
        if bytes.is_empty() {
            return;
        }
        self.bytes += bytes.len();
        self.frames.push_back(Frame {
            bytes: bytes.to_vec(),
            offset: 0,
            kind,
            paired: paired
                || kind == Kind::Reply
                || (bytes.len() > 1
                    && matches!(kind, Kind::Press(_) | Kind::Repeat(_) | Kind::Release(_))),
            _permit: permit,
        });
    }
    pub(super) fn admit_bytes(
        &mut self,
        bytes: &[u8],
        user: bool,
        permit: Option<Arc<admission::Permit>>,
    ) -> bool {
        if bytes.is_empty() {
            return true;
        }
        // Input also carries IME commits and unbracketed paste. Validate text
        // here without treating arbitrary binary API input as UTF-8 framing.
        let kind = if !user {
            Kind::Reply
        } else if std::str::from_utf8(bytes).is_ok() {
            Kind::Text
        } else {
            Kind::User
        };
        let merge = self.frames.back().is_some_and(|frame| {
            frame.kind == kind
                // Keep a consumed text prefix until its scalar has finished;
                // compacting it could hide an escaped UTF-8 leading byte.
                && (kind != Kind::Text || frame.offset == 0)
                && match (&frame._permit, &permit) {
                    (None, None) => true,
                    (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                    _ => false,
                }
        });
        if !self.fits(bytes.len(), usize::from(!merge), self.reservations()) {
            return false;
        }
        if merge {
            let frame = self.frames.back_mut().unwrap();
            if frame.offset != 0 {
                frame.bytes.drain(..frame.offset);
                frame.offset = 0;
            }
            frame.bytes.extend_from_slice(bytes);
            self.bytes += bytes.len();
        } else {
            self.append(bytes, kind, false, permit);
        }
        true
    }
    pub(super) fn admit_paste(
        &mut self,
        bytes: &[u8],
        bracketed: bool,
        permit: Option<Arc<admission::Permit>>,
    ) -> bool {
        if !bracketed {
            return self.admit_bytes(bytes, true, permit);
        }
        debug_assert!(bytes.starts_with(b"\x1b[200~") && bytes.ends_with(b"\x1b[201~"));
        if !self.fits(bytes.len(), 1, self.reservations()) {
            return false;
        }
        self.append(bytes, Kind::Paste, true, permit);
        true
    }
    fn admit_packet(&mut self, bytes: &[u8], permit: Option<Arc<admission::Permit>>) -> bool {
        if !self.fits(
            bytes.len(),
            usize::from(!bytes.is_empty()),
            self.reservations(),
        ) {
            return false;
        }
        self.append(bytes, Kind::Packet, true, permit);
        true
    }
    pub(super) fn admit_key(
        &mut self,
        input: &KeyInput,
        bytes: &[u8],
        paired: bool,
        permit: Option<Arc<admission::Permit>>,
        claim: Option<Arc<admission::Cleanup>>,
    ) -> bool {
        self.admit_pair(
            Identity::key(input),
            input.action,
            bytes,
            paired,
            permit,
            claim,
        )
    }
    pub(super) fn admit_mouse(
        &mut self,
        input: &MouseInput,
        bytes: &[u8],
        permit: Option<Arc<admission::Permit>>,
        claim: Option<Arc<admission::Cleanup>>,
    ) -> bool {
        let Some(button) = input.button else {
            return if input.action == MouseAction::Release {
                true
            } else {
                self.admit_packet(bytes, permit)
            };
        };
        // Wheel and motion events do not create a held button pair.
        if input.action == MouseAction::Motion
            || matches!(button, MouseButton::WheelUp | MouseButton::WheelDown)
        {
            return self.admit_packet(bytes, permit);
        }
        self.admit_pair(
            Identity::Mouse(button),
            if input.action == MouseAction::Release {
                KeyAction::Release
            } else {
                KeyAction::Press
            },
            bytes,
            true,
            permit,
            claim,
        )
    }
    fn admit_pair(
        &mut self,
        identity: Identity,
        action: KeyAction,
        bytes: &[u8],
        paired: bool,
        permit: Option<Arc<admission::Permit>>,
        claim: Option<Arc<admission::Cleanup>>,
    ) -> bool {
        let found = self.owners.iter().position(|owner| {
            owner.identity == identity
                && !owner.releasing
                && (match (&claim, &owner.claim) {
                    (Some(expected), Some(actual)) => Arc::ptr_eq(expected, actual),
                    (Some(_), None) => false,
                    (None, _) => true,
                })
        });
        if action == KeyAction::Release {
            let Some(index) = found else {
                return true;
            };
            if self.owners[index].releasing {
                return true;
            }
            if bytes.is_empty() {
                if self.requires_release_protocol(identity, claim.as_ref()) {
                    return false;
                }
                let token = self.owners[index].token;
                self.owners[index].releasing = true;
                if !self.frames.iter().any(
                    |frame| matches!(frame.kind, Kind::Press(id) | Kind::Repeat(id) if id == token),
                ) {
                    let owner = self.owners.remove(index);
                    if let Some(claim) = owner.claim {
                        claim.released();
                    }
                }
                return true;
            }
            if bytes.len() > RELEASE_BYTES {
                return false;
            }
            let token = self.owners[index].token;
            // A zero-byte press never escaped and has no pending frame. It
            // cannot authorize a new protocol's nonempty release after a mode change.
            if !self.owners[index].escaped
                && !self
                    .frames
                    .iter()
                    .any(|frame| frame.kind == Kind::Press(token))
            {
                let owner = self.owners.remove(index);
                if let Some(claim) = owner.claim {
                    claim.canceled();
                }
                return true;
            }
            let reservations = self.reservations().saturating_sub(1);
            if !self.fits(bytes.len(), 1, reservations) {
                return false;
            }
            self.owners[index].releasing = true;
            self.append(bytes, Kind::Release(token), true, permit);
            return true;
        }
        if action == KeyAction::Repeat && found.is_none() {
            return false;
        }
        if let Some(index) = found {
            if self.owners[index].releasing {
                return false;
            }
            if !self.fits(
                bytes.len(),
                usize::from(!bytes.is_empty()),
                self.reservations(),
            ) {
                return false;
            }
            self.owners[index].paired |= paired && !bytes.is_empty();
            self.append(
                bytes,
                Kind::Repeat(self.owners[index].token),
                paired,
                permit,
            );
            if let Some(claim) = claim {
                claim.encoded();
            }
            return true;
        }
        if self.owners.len() >= HELD_LIMIT
            || !self.fits(
                bytes.len(),
                usize::from(!bytes.is_empty()),
                self.reservations() + 1,
            )
        {
            return false;
        }
        self.next = self
            .next
            .checked_add(1)
            .expect("terminal input ownership exhausted");
        let token = self.next;
        if let Some(claim) = &claim {
            claim.encoded();
        }
        self.owners.push(Owner {
            token,
            identity,
            escaped: false,
            paired,
            releasing: false,
            claim,
        });
        self.append(bytes, Kind::Press(token), paired, permit);
        true
    }
    pub(super) fn consume(&mut self, count: usize) {
        let frame = self.frames.front_mut().expect("written PTY frame exists");
        assert!(count <= frame.bytes.len() - frame.offset);
        if count == 0 {
            return;
        }
        let partial = count < frame.bytes.len() - frame.offset;
        let written_start = frame.offset;
        frame.offset += count;
        self.bytes -= count;
        if let Kind::Press(token) | Kind::Repeat(token) = frame.kind {
            if let Some(owner) = self.owners.iter_mut().find(|owner| owner.token == token) {
                owner.escaped = true;
            }
        }
        if frame.paired
            // Even one complete master write does not prove the reader has
            // consumed a whole scalar. A kernel flush could split its bytes.
            || frame.kind == Kind::Text
                && frame.bytes[written_start..frame.offset].iter().any(|byte| !byte.is_ascii())
            || partial
                && matches!(
                    frame.kind,
                    Kind::Press(_) | Kind::Repeat(_) | Kind::Release(_)
                )
        {
            self.kernel_protected = true;
        }
        if frame.offset == frame.bytes.len() {
            if let Kind::Release(token) = frame.kind {
                self.owners.retain(|owner| {
                    if owner.token == token {
                        if let Some(claim) = &owner.claim {
                            claim.released();
                        }
                        false
                    } else {
                        true
                    }
                });
            }
            self.frames.pop_front();
            self.retire_closed_owners();
        }
    }
    fn retire_closed_owners(&mut self) {
        self.owners.retain(|owner| {
            let keep = !owner.releasing || self.frames.iter().any(|frame| matches!(frame.kind, Kind::Press(id) | Kind::Repeat(id) | Kind::Release(id) if id == owner.token));
            if !keep { if let Some(claim) = &owner.claim { claim.released(); } }
            keep
        });
    }
    pub(super) fn cancel(&mut self) {
        // Keep only owners whose press may have reached the original child.
        // Unsent press+release pairs disappear together, never as phantom ups.
        self.owners.retain(|owner| {
            if !owner.escaped {
                if let Some(claim) = &owner.claim {
                    claim.canceled();
                }
            }
            owner.escaped
        });
        self.frames.retain_mut(|frame| match frame.kind {
            Kind::Reply | Kind::Finish => true,
            Kind::User => false,
            Kind::Text => {
                if frame.offset == 0 {
                    return false;
                }
                let mut end = frame.offset;
                while end < frame.bytes.len() && frame.bytes[end] & 0xc0 == 0x80 {
                    end += 1;
                }
                if end == frame.offset {
                    return false;
                }
                // Complete only a scalar whose prefix escaped. Later text is
                // still cancelable, and another interrupt cannot drop Finish.
                frame.bytes = frame.bytes[frame.offset..end].to_vec();
                frame.offset = 0;
                frame.kind = Kind::Finish;
                true
            }
            Kind::Packet => frame.offset > 0,
            Kind::Paste => {
                if frame.offset == 0 {
                    return false;
                }
                let closing = frame.bytes.len() - 6;
                let mut remaining = Vec::with_capacity(12);
                if frame.offset < 6 {
                    remaining.extend_from_slice(&frame.bytes[frame.offset..6]);
                }
                // If a UTF-8 scalar's prefix has escaped, finish only that
                // scalar before the closing marker; cancel the unsent body.
                let mut suffix = frame.offset.max(6);
                while suffix < closing && frame.bytes[suffix] & 0xc0 == 0x80 {
                    remaining.push(frame.bytes[suffix]);
                    suffix += 1;
                }
                remaining.extend_from_slice(&frame.bytes[frame.offset.max(closing)..]);
                frame.bytes = remaining;
                frame.offset = 0;
                frame.kind = Kind::Finish;
                true
            }
            Kind::Release(token) => self.owners.iter().any(|owner| owner.token == token),
            Kind::Press(token) | Kind::Repeat(token) => {
                frame.offset > 0 && self.owners.iter().any(|owner| owner.token == token)
            }
        });
        self.bytes = self
            .frames
            .iter()
            .map(|frame| frame.bytes.len() - frame.offset)
            .sum();
        self.retire_closed_owners();
    }
}

// Existing low-level fixtures append raw pending bytes without a command.
#[cfg(test)]
impl<'a> Extend<&'a u8> for PendingInput {
    fn extend<T: IntoIterator<Item = &'a u8>>(&mut self, bytes: T) {
        let bytes = bytes.into_iter().copied().collect::<Vec<_>>();
        assert!(self.admit_bytes(&bytes, true, None));
    }
}
#[cfg(test)]
impl PartialEq for PendingInput {
    fn eq(&self, other: &Self) -> bool {
        self.frames
            .iter()
            .flat_map(|frame| frame.bytes[frame.offset..].iter())
            .eq(other
                .frames
                .iter()
                .flat_map(|frame| frame.bytes[frame.offset..].iter()))
            && self
                .owners
                .iter()
                .map(|owner| {
                    (
                        owner.token,
                        owner.identity,
                        owner.escaped,
                        owner.paired,
                        owner.releasing,
                    )
                })
                .eq(other.owners.iter().map(|owner| {
                    (
                        owner.token,
                        owner.identity,
                        owner.escaped,
                        owner.paired,
                        owner.releasing,
                    )
                }))
            && self.kernel_protected == other.kernel_protected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{KeyModifiers, TerminalKey};

    fn key(action: KeyAction) -> KeyInput {
        KeyInput {
            action,
            key: TerminalKey::Character('a'),
            text: None,
            unshifted_codepoint: Some('a'),
            modifiers: KeyModifiers::default(),
        }
    }
    fn drain(queue: &mut PendingInput) -> Vec<u8> {
        let mut bytes = Vec::new();
        while !queue.is_empty() {
            let next = queue.front().to_vec();
            queue.consume(next.len());
            bytes.extend(next);
        }
        bytes
    }
    #[test]
    fn text_cancellation_finishes_only_started_scalars_and_keeps_kernel_proof_unknown() {
        let text = "a€é🦀tail".as_bytes();
        for written in 0..=text.len() {
            let mut queue = PendingInput::default();
            assert!(queue.admit_bytes(text, true, None));
            if written != 0 {
                queue.consume(written);
            }
            let protected = text[..written].iter().any(|byte| !byte.is_ascii());
            assert_eq!(!queue.can_flush_kernel(), protected);
            queue.cancel();
            queue.cancel();
            assert!(queue.admit_bytes(b"INT", true, None));
            let mut end = written;
            while end < text.len() && text[end] & 0xc0 == 0x80 {
                end += 1;
            }
            let mut expected = text[written..end].to_vec();
            expected.extend_from_slice(b"INT");
            assert_eq!(drain(&mut queue), expected, "split {written}");
        }
        let mut queue = PendingInput::default();
        assert!(queue.admit_bytes("€old".as_bytes(), true, None));
        queue.consume(1);
        assert!(queue.admit_bytes("énew".as_bytes(), true, None));
        queue.cancel();
        assert_eq!(
            drain(&mut queue),
            [0x82, 0xac],
            "later text must not hide the started scalar"
        );
        let mut binary = PendingInput::default();
        assert!(binary.admit_bytes(&[0xff, 0x80, b'x'], true, None));
        binary.consume(1);
        binary.cancel();
        assert!(binary.is_empty());
        assert!(
            binary.can_flush_kernel(),
            "arbitrary binary input has no invented UTF-8 framing"
        );
    }

    #[test]
    fn empty_current_mode_up_cannot_certify_an_escaped_paired_down() {
        let mut queue = PendingInput::default();
        assert!(queue.admit_key(&key(KeyAction::Press), b"DOWN", true, None, None));
        assert_eq!(drain(&mut queue), b"DOWN");
        assert!(queue.key_release_protocol_unknown(&key(KeyAction::Release), b"", None));
        assert!(!queue.admit_key(&key(KeyAction::Release), b"", false, None, None));
        assert!(queue.has_held_input());
        assert!(!queue.can_flush_kernel());
    }
    #[test]
    fn merging_a_partly_consumed_reply_compacts_backing_storage() {
        let mut queue = PendingInput::default();
        assert!(queue.admit_bytes(&[b'r'; 1024], false, None));
        for _ in 0..10000 {
            queue.consume(512);
            assert!(queue.admit_bytes(&[b'r'; 512], false, None));
            let frame = queue.frames.front().unwrap();
            assert_eq!(queue.len(), 1024);
            assert_eq!(frame.offset, 0);
            assert_eq!(frame.bytes.len(), 1024);
            assert!(frame.bytes.capacity() <= 2048);
        }
    }
    #[test]
    fn started_bracketed_paste_finishes_markers_and_only_the_split_scalar() {
        let encoded = b"\x1b[200~a\xc3\xa9body\x1b[201~";
        for written in 0..encoded.len() {
            let mut queue = PendingInput::default();
            assert!(queue.admit_paste(encoded, true, None));
            if written != 0 {
                queue.consume(written);
            }
            queue.cancel();
            queue.cancel();
            assert!(queue.admit_bytes(b"INT", true, None));
            let rest = drain(&mut queue);
            let mut expected = Vec::new();
            if written != 0 {
                if written < 6 {
                    expected.extend_from_slice(&encoded[written..6]);
                }
                if written == 8 {
                    expected.push(0xa9);
                }
                expected.extend_from_slice(&encoded[written.max(encoded.len() - 6)..]);
                assert!(!queue.can_flush_kernel());
            }
            expected.extend_from_slice(b"INT");
            assert_eq!(rest, expected, "split {written}");
        }
    }
    #[test]
    fn partial_unpaired_mouse_packet_keeps_its_suffix_before_interrupt() {
        for written in 0..8 {
            let mut queue = PendingInput::default();
            assert!(queue.admit_packet(b"\x1b[<64;1M", None));
            if written != 0 {
                queue.consume(written);
            }
            queue.cancel();
            assert!(queue.admit_bytes(b"INT", true, None));
            let mut expected = if written == 0 {
                Vec::new()
            } else {
                b"\x1b[<64;1M"[written..].to_vec()
            };
            expected.extend_from_slice(b"INT");
            assert_eq!(drain(&mut queue), expected);
        }
    }
    #[test]
    fn canceled_old_cleanup_cannot_retire_a_new_same_key_owner() {
        let (wake, _rx) = crossbeam_channel::bounded(1);
        let pool = admission::CleanupPool::new(wake);
        let (old, ticket) = pool.press(Identity::key(&key(KeyAction::Press))).unwrap();
        let mut queue = PendingInput::default();
        assert!(queue.admit_key(
            &key(KeyAction::Press),
            b"OLD",
            true,
            None,
            Some(old.clone())
        ));
        drop(ticket);
        queue.cancel();
        let (new, ticket) = pool.press(Identity::key(&key(KeyAction::Press))).unwrap();
        assert!(!Arc::ptr_eq(&old, &new));
        assert!(queue.admit_key(
            &key(KeyAction::Press),
            b"NEW",
            true,
            None,
            Some(new.clone())
        ));
        drop(ticket);
        assert_eq!(drain(&mut queue), b"NEW");
        assert!(queue.admit_key(&key(KeyAction::Release), b"STALE", true, None, Some(old)));
        assert!(queue.is_empty());
        assert!(queue.has_held_input());
        assert!(queue.admit_key(&key(KeyAction::Release), b"UP", true, None, Some(new)));
        assert_eq!(drain(&mut queue), b"UP");
        assert!(!queue.has_held_input());
    }
    #[test]
    fn cancellation_keeps_unsent_release_of_an_escaped_press_and_never_replays_a_sent_up() {
        let mut queue = PendingInput::default();
        assert!(queue.admit_key(&key(KeyAction::Press), b"DOWN", true, None, None));
        assert_eq!(drain(&mut queue), b"DOWN");
        assert!(queue.admit_bytes(b"bulk", true, None));
        assert!(queue.admit_key(&key(KeyAction::Release), b"UP", true, None, None));
        queue.cancel();
        assert_eq!(drain(&mut queue), b"UP");
        assert!(!queue.can_flush_kernel());
        assert!(queue.admit_key(&key(KeyAction::Release), b"UP", true, None, None));
        queue.cancel();
        assert!(drain(&mut queue).is_empty());
        assert!(!queue.has_held_input());
    }
    #[test]
    fn canceled_wholly_unsent_press_and_release_never_create_a_phantom_up() {
        let mut queue = PendingInput::default();
        assert!(queue.admit_key(&key(KeyAction::Press), b"DOWN", true, None, None));
        assert!(queue.admit_key(&key(KeyAction::Release), b"UP", true, None, None));
        queue.cancel();
        assert!(queue.is_empty());
        assert!(!queue.has_held_input());
        assert!(queue.can_flush_kernel());
        assert!(queue.admit_key(&key(KeyAction::Release), b"UP", true, None, None));
        assert!(queue.is_empty());
    }
    #[test]
    fn cancellation_finishes_a_partial_protocol_frame_before_release_and_interrupt() {
        let mut queue = PendingInput::default();
        assert!(queue.admit_key(&key(KeyAction::Press), b"\x1b[97;1:1u", true, None, None));
        queue.consume(1);
        assert!(queue.admit_key(&key(KeyAction::Release), b"\x1b[97;1:3u", true, None, None));
        queue.cancel();
        assert!(queue.admit_bytes(b"INT", true, None));
        assert_eq!(drain(&mut queue), b"[97;1:1u\x1b[97;1:3uINT");
        assert!(!queue.can_flush_kernel());
    }
    #[test]
    fn bulk_backpressure_cannot_consume_a_held_keys_reserved_release_capacity() {
        let mut queue = PendingInput::default();
        assert!(queue.admit_key(&key(KeyAction::Press), b"DOWN", true, None, None));
        assert_eq!(drain(&mut queue), b"DOWN");
        assert!(queue.admit_bytes(&vec![b'x'; PTY_WRITE_CAPACITY - RELEASE_BYTES], true, None));
        assert!(!queue.admit_bytes(b"x", true, None));
        assert!(queue.admit_key(
            &key(KeyAction::Release),
            &[b'U'; RELEASE_BYTES],
            true,
            None,
            None
        ));
        assert_eq!(queue.len(), PTY_WRITE_CAPACITY);
        queue.cancel();
        assert_eq!(drain(&mut queue), vec![b'U'; RELEASE_BYTES]);
    }
    #[test]
    fn bytes_retain_their_original_command_credit_until_written_or_canceled() {
        let budget = admission::Budget::new(1);
        let permit = budget.try_acquire().unwrap();
        let mut queue = PendingInput::default();
        assert!(queue.admit_bytes(b"pending", true, Some(permit.clone())));
        drop(permit);
        assert!(budget.try_acquire().is_none());
        queue.cancel();
        assert!(budget.try_acquire().is_some());
    }
    #[test]
    fn modifier_changes_do_not_change_pair_identity_and_repeats_share_one_owner() {
        let mut queue = PendingInput::default();
        assert!(queue.admit_key(&key(KeyAction::Press), b"D", true, None, None));
        assert_eq!(drain(&mut queue), b"D");
        let mut repeat = key(KeyAction::Repeat);
        repeat.modifiers.control = true;
        assert!(queue.admit_key(&repeat, b"R", true, None, None));
        assert_eq!(queue.owners.len(), 1);
        let mut release = key(KeyAction::Release);
        release.modifiers.shift = true;
        assert!(queue.admit_key(&release, b"U", true, None, None));
        assert_eq!(drain(&mut queue), b"RU");
        assert!(!queue.has_held_input());
    }
    #[test]
    fn pending_legacy_key_prefix_survives_its_zero_byte_release_and_cancellation() {
        let mut queue = PendingInput::default();
        assert!(queue.admit_key(&key(KeyAction::Press), b"\x1bX", false, None, None));
        queue.consume(1);
        assert!(queue.admit_key(&key(KeyAction::Release), b"", false, None, None));
        queue.cancel();
        assert_eq!(drain(&mut queue), b"X");
        assert!(!queue.has_held_input());
        assert!(!queue.can_flush_kernel());
    }
}
