use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

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
pub struct TerminalSurface {
    handle: HostedTerminalHandle,
    /// Set while a terminal this client just started still has the size it
    /// was spawned with, which nobody chose.
    unsized_spawn: Arc<AtomicBool>,
}

impl TerminalSurface {
    /// Marks a terminal this client just started, so its first layout sizes
    /// the PTY at once rather than as a resize that waits to settle.
    pub fn spawned(self) -> Self {
        self.unsized_spawn.store(true, Ordering::Relaxed);
        self
    }

    pub fn is_unsized_spawn(&self) -> bool {
        self.unsized_spawn.load(Ordering::Relaxed)
    }

    /// Semantic surfaces must reset when a card attaches a replacement terminal.
    pub(crate) fn terminal_id(&self) -> crate::model::TerminalId {
        self.handle.terminal_id()
    }

    pub fn snapshot(&self) -> Arc<RenderSnapshot> {
        self.handle.snapshot()
    }

    /// The grid to draw; see [`HostedTerminalHandle::frame`].
    pub fn frame(&self) -> Arc<RenderSnapshot> {
        self.handle.frame()
    }

    pub fn activity(&self) -> Option<crate::model::TerminalOutputActivity> {
        self.handle.activity()
    }

    pub fn agent_activity(&self) -> Option<crate::agent::Activity> {
        self.handle.agent_activity()
    }

    #[cfg(test)]
    pub(crate) fn visibility(&self) -> crate::terminal_ipc::TerminalVisibility {
        self.handle.visibility()
    }

    pub fn generation(&self) -> u64 {
        self.handle.generation()
    }

    pub fn host_proof(&self) -> Option<HostSessionProof> {
        self.handle.proof()
    }

    pub fn worker_binding(&self) -> Option<super::HostedTerminalBinding> {
        self.handle.worker_binding()
    }

    pub fn execution_instance(&self) -> Option<uuid::Uuid> {
        self.handle.execution_instance()
    }

    pub fn acknowledge_redraw(&self, observed_generation: u64) {
        self.handle.acknowledge_redraw(observed_generation);
    }

    pub fn take_observed_output(&self) -> bool {
        self.handle.take_observed_output()
    }

    pub fn acknowledge_updates(&self) -> Result<(), TerminalSendError> {
        self.handle.acknowledge_updates()
    }

    pub fn events(&self) -> Receiver<TerminalEvent> {
        self.handle.events()
    }

    pub fn input(&self, bytes: impl Into<Vec<u8>>) -> Result<(), TerminalSendError> {
        self.handle.input(bytes)
    }

    /// Pastes text the way a terminal does: framed as a bracketed paste when
    /// the child asked for one, and otherwise typed, with as much of it acting
    /// as keys as `origin` allows; see [`PasteOrigin`].
    pub fn paste(
        &self,
        text: impl Into<String>,
        origin: PasteOrigin,
    ) -> Result<(), TerminalSendError> {
        self.handle.paste(text, origin)
    }

    pub fn key_with_receipt(
        &self,
        key: KeyInput,
    ) -> Result<super::hosted::HostedKeyReceipt, TerminalSendError> {
        self.handle.key_with_receipt(key)
    }

    pub fn key_after_receipt(
        &self,
        key: KeyInput,
        after: &super::hosted::HostedKeyReceipt,
    ) -> Result<super::hosted::HostedKeyReceipt, TerminalSendError> {
        self.handle.key_after_receipt(key, after)
    }

    pub fn key(&self, key: KeyInput) -> Result<(), TerminalSendError> {
        self.handle.key(key)
    }

    pub fn mouse(&self, mouse: MouseInput) -> Result<(), TerminalSendError> {
        self.handle.mouse(mouse)
    }

    pub fn focus(&self, focused: bool) -> Result<(), TerminalSendError> {
        self.handle.focus(focused)
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), TerminalSendError> {
        self.handle.resize(size)?;
        self.unsized_spawn.store(false, Ordering::Relaxed);
        Ok(())
    }

    pub fn scroll_lines(&self, delta: isize) -> Result<(), TerminalSendError> {
        self.handle.scroll_lines(delta)
    }

    pub fn scroll_to_top(&self) -> Result<(), TerminalSendError> {
        self.handle.scroll_to_top()
    }

    pub fn scroll_to_bottom(&self) -> Result<(), TerminalSendError> {
        self.handle.scroll_to_bottom()
    }

    pub fn scroll_to_row(&self, row: u64) -> Result<(), TerminalSendError> {
        self.handle.scroll_to_row(row)
    }
    pub fn scroll_to_match(&self, range: super::SearchMatch) -> Result<(), TerminalSendError> {
        self.handle.scroll_to_match(range)
    }

    pub fn clear_scrollback(&self) -> Result<(), TerminalSendError> {
        self.handle.clear_scrollback()
    }
    pub fn copy_selection(
        &self,
        request: uuid::Uuid,
        selection: super::SearchMatch,
    ) -> Result<(), TerminalSendError> {
        self.handle.copy_selection(request, selection)
    }

    pub fn set_theme(&self, theme: super::TerminalTheme) -> Result<(), TerminalSendError> {
        self.handle.set_theme(theme)
    }

    pub fn search(
        &self,
        request: u64,
        query: String,
        case_sensitive: bool,
    ) -> Result<(), TerminalSendError> {
        self.handle.search(request, query, case_sensitive)
    }

    pub fn hide(&self) -> Result<(), TerminalSendError> {
        self.handle.hide()
    }

    pub fn reveal(&self) -> Result<(), TerminalSendError> {
        self.handle.reveal()
    }

    pub fn hibernate(&self) -> Result<(), TerminalSendError> {
        self.handle.hibernate()
    }

    pub fn restore(&self, visible: bool) -> Result<(), TerminalSendError> {
        self.handle.restore(visible)
    }

    pub fn shutdown(&self) -> Result<(), TerminalSendError> {
        self.handle.shutdown()
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
        Self {
            handle,
            unsized_spawn: Arc::default(),
        }
    }
}
