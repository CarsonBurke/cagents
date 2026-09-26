use std::sync::Arc;

use crossbeam_channel::Receiver;

use crate::model::HostSessionProof;

use super::{
    HostedTerminalHandle, KeyInput, MouseInput, PasteOrigin, RenderSnapshot, TerminalEvent,
    TerminalSendError, TerminalSize,
};

/// UI-side rendering and input surface backed by the daemon protocol.
///
/// Terminal emulation and PTY ownership live exclusively in `cagentsd`; the UI
/// never links the host implementation.
#[derive(Clone, Debug)]
pub struct TerminalSurface(HostedTerminalHandle);

impl TerminalSurface {
    pub fn snapshot(&self) -> Arc<RenderSnapshot> {
        self.0.snapshot()
    }

    pub fn activity(&self) -> Option<crate::model::TerminalOutputActivity> {
        self.0.activity()
    }

    pub fn agent_activity(&self) -> Option<crate::agent::Activity> {
        self.0.agent_activity()
    }

    #[cfg(test)]
    pub(crate) fn visibility(&self) -> crate::terminal_ipc::TerminalVisibility {
        self.0.visibility()
    }

    pub fn generation(&self) -> u64 {
        self.0.generation()
    }

    pub fn host_proof(&self) -> Option<HostSessionProof> {
        self.0.proof()
    }

    pub fn acknowledge_redraw(&self, observed_generation: u64) {
        self.0.acknowledge_redraw(observed_generation);
    }

    pub fn take_observed_output(&self) -> bool {
        self.0.take_observed_output()
    }

    pub fn acknowledge_updates(&self) -> Result<(), TerminalSendError> {
        self.0.acknowledge_updates()
    }

    pub fn events(&self) -> Receiver<TerminalEvent> {
        self.0.events()
    }

    pub fn input(&self, bytes: impl Into<Vec<u8>>) -> Result<(), TerminalSendError> {
        self.0.input(bytes)
    }

    /// Pastes text the way a terminal does: framed as a bracketed paste when
    /// the child asked for one, and otherwise typed, with as much of it acting
    /// as keys as `origin` allows; see [`PasteOrigin`].
    pub fn paste(
        &self,
        text: impl Into<String>,
        origin: PasteOrigin,
    ) -> Result<(), TerminalSendError> {
        self.0.paste(text, origin)
    }

    pub fn key(&self, key: KeyInput) -> Result<(), TerminalSendError> {
        self.0.key(key)
    }

    pub fn mouse(&self, mouse: MouseInput) -> Result<(), TerminalSendError> {
        self.0.mouse(mouse)
    }

    pub fn focus(&self, focused: bool) -> Result<(), TerminalSendError> {
        self.0.focus(focused)
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), TerminalSendError> {
        self.0.resize(size)
    }

    pub fn scroll_lines(&self, delta: isize) -> Result<(), TerminalSendError> {
        self.0.scroll_lines(delta)
    }

    pub fn scroll_to_top(&self) -> Result<(), TerminalSendError> {
        self.0.scroll_to_top()
    }

    pub fn scroll_to_bottom(&self) -> Result<(), TerminalSendError> {
        self.0.scroll_to_bottom()
    }

    pub fn scroll_to_row(&self, row: u64) -> Result<(), TerminalSendError> {
        self.0.scroll_to_row(row)
    }

    pub fn set_theme(&self, theme: super::TerminalTheme) -> Result<(), TerminalSendError> {
        self.0.set_theme(theme)
    }

    pub fn hide(&self) -> Result<(), TerminalSendError> {
        self.0.hide()
    }

    pub fn reveal(&self) -> Result<(), TerminalSendError> {
        self.0.reveal()
    }

    pub fn hibernate(&self) -> Result<(), TerminalSendError> {
        self.0.hibernate()
    }

    pub fn restore(&self, visible: bool) -> Result<(), TerminalSendError> {
        self.0.restore(visible)
    }

    pub fn shutdown(&self) -> Result<(), TerminalSendError> {
        self.0.shutdown()
    }

    /// Drop this client's attachment without stopping the daemon-owned PTY.
    ///
    /// [`shutdown`](Self::shutdown) asks the worker to kill its child. A Codex
    /// thread stays alive in that child while native chat is attached, so the
    /// UI has to leave the process hosted and only disconnect itself.
    pub fn release(self) {
        drop(self);
    }
}

impl From<HostedTerminalHandle> for TerminalSurface {
    fn from(handle: HostedTerminalHandle) -> Self {
        Self(handle)
    }
}
