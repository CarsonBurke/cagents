//! UI queue ownership for raw keys across native focus changes. This is not
//! a worker or PTY-consumption acknowledgement. Cleanup stays on the original
//! attachment and host proof; it is never replayed through a replacement.
use super::{KeyAction, KeyInput, KeyModifiers, ReviewInputBarrier, TerminalSendError, TerminalSurface, map_iced_modifiers};
use crate::model::{HostSessionProof, TerminalId};
use cosmic::iced::keyboard;

const MAX_KEYS: usize = 512;
#[derive(Default)]
pub(super) struct OwnedInput {
    keys: Vec<OwnedKey>,
    awaiting_up: Vec<keyboard::key::Physical>,
    native_review: Option<TerminalId>,
    modifiers: KeyModifiers,
    failures: Vec<String>,
}
struct OwnedKey {
    physical: keyboard::key::Physical,
    surface: TerminalSurface,
    proof: HostSessionProof,
    input: KeyInput,
    orphaned: bool,
    released: bool,
    failed: bool,
}
impl OwnedInput {
    fn failure(&mut self, reason: String) {
        tracing::warn!(%reason, "terminal key cleanup could not be confirmed by the UI transport");
        if self.failures.len() < 8 && !self.failures.contains(&reason) {
            self.failures.push(reason);
        }
    }
}
impl ReviewInputBarrier {
    pub(crate) fn note_review(&self, terminal: TerminalId, active: bool) {
        let mut owned = self.1.lock().unwrap_or_else(|e| e.into_inner());
        if active { owned.native_review = Some(terminal); }
        else if owned.native_review == Some(terminal) { owned.native_review = None; }
    }
    pub(crate) fn confirm_review(&self, previous: Option<TerminalId>, current: Option<TerminalId>) {
        let mut owned = self.1.lock().unwrap_or_else(|e| e.into_inner());
        if current.is_some() || owned.native_review == previous { owned.native_review = current; }
    }
    pub(super) fn press(&self, physical: keyboard::key::Physical, surface: &TerminalSurface, input: KeyInput) -> Result<(), TerminalSendError> {
        let mut owned = self.1.lock().unwrap_or_else(|e| e.into_inner());
        owned.modifiers = input.modifiers;
        // Window blur can hide the old physical up entirely. A genuine new
        // down proves that hold ended; a repeat cannot establish a new cycle.
        if input.action == KeyAction::Press {
            owned.awaiting_up.retain(|key| *key != physical);
        }
        if owned.native_review.is_some() || owned.awaiting_up.contains(&physical) { return Err(TerminalSendError::Full); }
        if let Some(key) = owned.keys.iter_mut().find(|key| key.physical == physical && !(key.failed && key.released)) {
            if key.orphaned || key.released || key.failed || !key.surface.same_attachment(surface) || surface.host_proof().as_ref() != Some(&key.proof) {
                return Err(TerminalSendError::Full);
            }
            surface.key(input.clone())?;
            key.input = input;
            return Ok(());
        }
        // An intercepted or refused down cannot be revived as a raw repeat.
        if input.action == KeyAction::Repeat { return Err(TerminalSendError::Full); }
        if owned.keys.iter().any(|key| !key.failed && (key.orphaned || key.released) && key.surface.same_attachment(surface)) {
            return Err(TerminalSendError::Full);
        }
        if owned.keys.len() + owned.awaiting_up.len() >= MAX_KEYS { return Err(TerminalSendError::Full); }
        let proof = surface.host_proof().filter(|proof| proof.is_complete()).ok_or(TerminalSendError::Closed)?;
        surface.key(input.clone())?;
        owned.keys.push(OwnedKey { physical, surface: surface.clone(), proof, input, orphaned: false, released: false, failed: false });
        Ok(())
    }
    pub(super) fn owns_raw_key(&self, physical: keyboard::key::Physical, surface: &TerminalSurface) -> bool {
        self.1.lock().unwrap_or_else(|e| e.into_inner()).keys.iter().any(|key| key.physical == physical && key.surface.same_attachment(surface))
    }
    pub(super) fn orphan_keys(&self, surface: &TerminalSurface) {
        for key in &mut self.1.lock().unwrap_or_else(|e| e.into_inner()).keys {
            if key.surface.same_attachment(surface) { key.orphaned = true; }
        }
    }
    /// Runs during native dispatch, before a new focus can consume the up.
    /// Never derive cleanup from a later queued application message.
    pub(crate) fn observe_key_event(&self, event: &keyboard::Event) {
        let mut owned = self.1.lock().unwrap_or_else(|e| e.into_inner());
        let modifiers = match event {
            keyboard::Event::ModifiersChanged(modifiers)
            | keyboard::Event::KeyPressed { modifiers, .. }
            | keyboard::Event::KeyReleased { modifiers, .. } => *modifiers,
        };
        owned.modifiers = map_iced_modifiers(modifiers);
        if let keyboard::Event::KeyReleased { physical_key, modifiers, .. } = event {
            owned.awaiting_up.retain(|key| key != physical_key);
            for key in &mut owned.keys {
                if key.physical == *physical_key {
                    key.released = true;
                    key.input.modifiers = map_iced_modifiers(*modifiers);
                }
            }
        }
    }
    /// Perform cleanup only in native dispatch/redraw, after Review state is
    /// settled for that batch. Application update acknowledgements never send
    /// input. Return true only for transport backpressure that needs a redraw.
    pub(crate) fn flush_owned_keys(&self) -> bool {
        let mut owned = self.1.lock().unwrap_or_else(|e| e.into_inner());
        if owned.native_review.is_some() { return false; }
        let mut pending = false;
        let mut index = 0;
        while index < owned.keys.len() {
            if owned.keys[index].failed || !(owned.keys[index].orphaned || owned.keys[index].released) { index += 1; continue; }
            let key = &owned.keys[index];
            if key.surface.host_proof().as_ref() != Some(&key.proof) {
                owned.keys[index].failed = true;
                owned.failure("original terminal attachment changed before key cleanup; delivery is unknown".into());
                index += 1;
                continue;
            }
            let mut input = key.input.clone();
            input.action = KeyAction::Release;
            input.text = None;
            input.modifiers = owned.modifiers;
            match key.surface.key(input) {
                Ok(()) => {
                    let key = owned.keys.swap_remove(index);
                    if !key.released { owned.awaiting_up.push(key.physical); }
                }
                Err(TerminalSendError::Full) => { pending = true; index += 1; }
                Err(error) => {
                    owned.keys[index].failed = true;
                    owned.failure(format!("original terminal key cleanup was refused: {error}; delivery is unknown"));
                    index += 1;
                }
            }
        }
        pending
    }
    pub(crate) fn take_key_cleanup_failures(&self) -> Vec<String> {
        std::mem::take(&mut self.1.lock().unwrap_or_else(|e| e.into_inner()).failures)
    }
}
