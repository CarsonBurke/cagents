use std::sync::Arc;

use crate::model::HostSessionProof;
use crossbeam_channel::Receiver;

use super::{
    HostedTerminalHandle, KeyInput, MouseInput, RenderSnapshot, TerminalEvent, TerminalHandle,
    TerminalSendError, TerminalSize,
};

/// The rendering and input surface shared by in-process and host-owned terminals.
#[derive(Clone, Debug)]
pub enum TerminalSurface {
    Local(TerminalHandle),
    Hosted(HostedTerminalHandle),
}

impl TerminalSurface {
    pub fn snapshot(&self) -> Arc<RenderSnapshot> {
        match self {
            Self::Local(handle) => handle.snapshot(),
            Self::Hosted(handle) => handle.snapshot(),
        }
    }

    pub fn generation(&self) -> u64 {
        match self {
            Self::Local(handle) => handle.generation(),
            Self::Hosted(handle) => handle.generation(),
        }
    }
    pub fn host_proof(&self) -> Option<HostSessionProof> {
        match self {
            Self::Local(_) => None,
            Self::Hosted(handle) => handle.proof(),
        }
    }

    pub fn acknowledge_redraw(&self, observed_generation: u64) {
        match self {
            Self::Local(handle) => handle.acknowledge_redraw(observed_generation),
            Self::Hosted(handle) => handle.acknowledge_redraw(observed_generation),
        }
    }

    pub fn events(&self) -> Receiver<TerminalEvent> {
        match self {
            Self::Local(handle) => handle.events(),
            Self::Hosted(handle) => handle.events(),
        }
    }

    pub fn input(&self, bytes: impl Into<Vec<u8>>) -> Result<(), TerminalSendError> {
        let bytes = bytes.into();
        match self {
            Self::Local(handle) => handle.input(bytes),
            Self::Hosted(handle) => handle.input(bytes),
        }
    }

    pub fn key(&self, key: KeyInput) -> Result<(), TerminalSendError> {
        match self {
            Self::Local(handle) => handle.key(key),
            Self::Hosted(handle) => handle.key(key),
        }
    }

    pub fn mouse(&self, mouse: MouseInput) -> Result<(), TerminalSendError> {
        match self {
            Self::Local(handle) => handle.mouse(mouse),
            Self::Hosted(handle) => handle.mouse(mouse),
        }
    }

    pub fn focus(&self, focused: bool) -> Result<(), TerminalSendError> {
        match self {
            Self::Local(handle) => handle.focus(focused),
            Self::Hosted(handle) => handle.focus(focused),
        }
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), TerminalSendError> {
        match self {
            Self::Local(handle) => handle.resize(size),
            Self::Hosted(handle) => handle.resize(size),
        }
    }

    pub fn scroll_lines(&self, delta: isize) -> Result<(), TerminalSendError> {
        match self {
            Self::Local(handle) => handle.scroll_lines(delta),
            Self::Hosted(handle) => handle.scroll_lines(delta),
        }
    }

    pub fn scroll_to_top(&self) -> Result<(), TerminalSendError> {
        match self {
            Self::Local(handle) => handle.scroll_to_top(),
            Self::Hosted(handle) => handle.scroll_to_top(),
        }
    }

    pub fn scroll_to_bottom(&self) -> Result<(), TerminalSendError> {
        match self {
            Self::Local(handle) => handle.scroll_to_bottom(),
            Self::Hosted(handle) => handle.scroll_to_bottom(),
        }
    }

    pub fn hide(&self) -> Result<(), TerminalSendError> {
        match self {
            Self::Local(handle) => handle.hide(),
            Self::Hosted(handle) => handle.hide(),
        }
    }

    pub fn reveal(&self) -> Result<(), TerminalSendError> {
        match self {
            Self::Local(handle) => handle.reveal(),
            Self::Hosted(handle) => handle.reveal(),
        }
    }

    pub fn hibernate(&self) -> Result<(), TerminalSendError> {
        match self {
            Self::Local(handle) => handle.hibernate(),
            Self::Hosted(handle) => handle.hibernate(),
        }
    }

    pub fn restore(&self, visible: bool) -> Result<(), TerminalSendError> {
        match self {
            Self::Local(handle) => handle.restore(visible),
            Self::Hosted(handle) => handle.restore(visible),
        }
    }

    pub fn shutdown(&self) -> Result<(), TerminalSendError> {
        match self {
            Self::Local(handle) => handle.shutdown(),
            Self::Hosted(handle) => handle.shutdown(),
        }
    }
}

impl From<TerminalHandle> for TerminalSurface {
    fn from(handle: TerminalHandle) -> Self {
        Self::Local(handle)
    }
}

impl From<HostedTerminalHandle> for TerminalSurface {
    fn from(handle: HostedTerminalHandle) -> Self {
        Self::Hosted(handle)
    }
}
