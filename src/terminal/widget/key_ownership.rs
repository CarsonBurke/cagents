//! UI queue ownership for raw keys across native focus changes. This is not
//! a worker or PTY-consumption acknowledgement. Cleanup stays on the original
//! attachment and host proof; it is never replayed through a replacement.
use super::{
    KeyAction, KeyInput, KeyModifiers, ReviewInputBarrier, TerminalSendError, TerminalSurface,
    map_iced_modifiers,
};
use crate::model::{HostSessionProof, TerminalId};
use cosmic::iced::keyboard;
use crossbeam_channel::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

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
    epoch: u64,
    press: super::super::hosted::KeyAdmission,
    cleanup: Option<(Receiver<bool>, Instant)>,
    input: KeyInput,
    orphaned: bool,
    released: bool,
    failed: bool,
}
impl OwnedInput {
    fn reap_completed(&mut self) -> bool {
        let mut pending = false;
        let mut index = 0;
        while index < self.keys.len() {
            let Some((completion, deadline)) = self.keys[index].cleanup.as_ref() else {
                index += 1;
                continue;
            };
            match completion.try_recv() {
                Ok(true) => {
                    let key = self.keys.swap_remove(index);
                    if !key.released {
                        self.awaiting_up.push(key.physical);
                    }
                }
                Ok(false) | Err(TryRecvError::Disconnected) => {
                    self.keys[index].cleanup = None;
                    self.keys[index].failed = true;
                    self.failure("original terminal key cleanup was not acknowledged on its socket attachment; delivery is unknown".into());
                    index += 1;
                }
                Err(TryRecvError::Empty) => {
                    if Instant::now() >= *deadline {
                        if !self.keys[index].failed {
                            self.keys[index].failed = true;
                            self.failure("original terminal key cleanup acknowledgement timed out; delivery is unknown".into());
                        }
                    } else {
                        pending = true;
                    }
                    index += 1;
                }
            }
        }
        pending
    }
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
        if active {
            owned.native_review = Some(terminal);
        } else if owned.native_review == Some(terminal) {
            owned.native_review = None;
        }
    }
    pub(crate) fn confirm_review(&self, previous: Option<TerminalId>, current: Option<TerminalId>) {
        let mut owned = self.1.lock().unwrap_or_else(|e| e.into_inner());
        if current.is_some() || owned.native_review == previous {
            owned.native_review = current;
        }
    }
    pub(super) fn press(
        &self,
        physical: keyboard::key::Physical,
        surface: &TerminalSurface,
        input: KeyInput,
    ) -> Result<(), TerminalSendError> {
        let mut owned = self.1.lock().unwrap_or_else(|e| e.into_inner());
        // A fresh physical cycle also proves an earlier synthetic-up hold
        // ended outside this window, including while its ACK was pending.
        if input.action == KeyAction::Press {
            for key in &mut owned.keys {
                if key.physical == physical && key.orphaned {
                    key.released = true;
                }
            }
        }
        owned.reap_completed();
        owned.modifiers = input.modifiers;
        // Window blur can hide the old physical up entirely. A genuine new
        // down proves that hold ended; a repeat cannot establish a new cycle.
        if input.action == KeyAction::Press {
            owned.awaiting_up.retain(|key| *key != physical);
        }
        if owned.native_review.is_some() || owned.awaiting_up.contains(&physical) {
            return Err(TerminalSendError::Full);
        }
        if let Some(key) = owned
            .keys
            .iter_mut()
            .find(|key| key.physical == physical && !key.released && !key.failed)
        {
            if key.orphaned
                || key.released
                || key.failed
                || !key.surface.same_attachment(surface)
                || surface.host_proof().as_ref() != Some(&key.proof)
                || surface.cleanup_epoch() != Some(key.epoch)
            {
                return Err(TerminalSendError::Full);
            }
            // Repeats may change layout text, but cleanup keeps the original
            // admitted key and unshifted identity. Current modifiers are global.
            surface.repeat_key(input, key.proof.clone(), key.epoch, key.press.clone())?;
            return Ok(());
        }
        // An intercepted or refused down cannot be revived as a raw repeat.
        if input.action == KeyAction::Repeat {
            return Err(TerminalSendError::Full);
        }
        if owned.keys.iter().any(|key| {
            !key.failed
                && key.cleanup.is_none()
                && (key.orphaned || key.released)
                && key.surface.same_attachment(surface)
        }) {
            return Err(TerminalSendError::Full);
        }
        if owned.keys.len() + owned.awaiting_up.len() >= MAX_KEYS {
            return Err(TerminalSendError::Full);
        }
        let proof = surface
            .host_proof()
            .filter(|proof| proof.is_complete())
            .ok_or(TerminalSendError::Closed)?;
        let epoch = surface.cleanup_epoch().ok_or(TerminalSendError::Closed)?;
        let completion = surface.tracked_key(input.clone(), proof.clone(), epoch)?;
        owned.keys.push(OwnedKey {
            physical,
            surface: surface.clone(),
            proof,
            epoch,
            press: completion,
            cleanup: None,
            input,
            orphaned: false,
            released: false,
            failed: false,
        });
        Ok(())
    }
    pub(super) fn owns_raw_key(
        &self,
        physical: keyboard::key::Physical,
        surface: &TerminalSurface,
    ) -> bool {
        self.1
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys
            .iter()
            .any(|key| key.physical == physical && key.surface.same_attachment(surface))
    }
    pub(super) fn orphan_keys(&self, surface: &TerminalSurface) {
        for key in &mut self.1.lock().unwrap_or_else(|e| e.into_inner()).keys {
            if key.surface.same_attachment(surface) {
                key.orphaned = true;
            }
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
        if let keyboard::Event::KeyReleased {
            physical_key,
            modifiers,
            ..
        } = event
        {
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
        let mut pending = owned.reap_completed();
        if owned.native_review.is_some() {
            return pending;
        }
        let mut index = 0;
        while index < owned.keys.len() {
            if owned.keys[index].failed
                || owned.keys[index].cleanup.is_some()
                || !(owned.keys[index].orphaned || owned.keys[index].released)
            {
                index += 1;
                continue;
            }
            let key = &owned.keys[index];
            if key.surface.host_proof().as_ref() != Some(&key.proof)
                || key.surface.cleanup_epoch() != Some(key.epoch)
            {
                owned.keys[index].failed = true;
                owned.failure(
                    "original terminal attachment changed before key cleanup; delivery is unknown"
                        .into(),
                );
                index += 1;
                continue;
            }
            let mut input = key.input.clone();
            input.action = KeyAction::Release;
            input.text = None;
            input.modifiers = owned.modifiers;
            match key
                .surface
                .cleanup_key(input, key.proof.clone(), key.epoch, key.press.clone())
            {
                Ok(completion) => {
                    // Keep the last original Surface alive through the actual
                    // daemon response. Enqueue success alone cannot retire it.
                    owned.keys[index].cleanup =
                        Some((completion, Instant::now() + Duration::from_secs(5)));
                    pending = true;
                    index += 1;
                }
                Err(TerminalSendError::Full) => {
                    pending = true;
                    index += 1;
                }
                Err(error) => {
                    owned.keys[index].failed = true;
                    owned.failure(format!(
                        "original terminal key cleanup was refused: {error}; delivery is unknown"
                    ));
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
