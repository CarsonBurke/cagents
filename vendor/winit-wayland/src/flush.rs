//! Outgoing socket readiness for the Winit and iced Wayland event loops.

use std::cell::Cell;
use std::io::{ErrorKind, Result};
use std::os::fd::AsFd;
use std::rc::Rc;

use calloop::generic::Generic;
use calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use wayland_client::Connection;
use wayland_client::backend::WaylandError;

/// Complements the read-only `WaylandSource` in the same calloop event loop.
/// Use each instance with a single connection and event loop.
#[derive(Debug, Default)]
pub struct WaylandFlush {
    writer: Option<RegistrationToken>,
    writable: Rc<Cell<bool>>,
}

impl WaylandFlush {
    pub fn dispatch<Data>(
        &mut self,
        connection: &Connection,
        handle: &LoopHandle<'_, Data>,
        dispatch: impl FnOnce() -> calloop::Result<()>,
    ) -> calloop::Result<()> {
        self.flush(connection, handle)?;
        if let Err(error) = dispatch() {
            // A Wayland source error can be wrapped in a generic calloop
            // error. The backend keeps the original failure if it lost the
            // connection during dispatch.
            return Err(connection
                .backend()
                .last_error()
                .map_or(error, |cause| connection_error(cause).into()));
        }

        // The compositor can drain requests without sending any new events.
        // Retry on the writable wakeup even if the caller is about to stop polling.
        if self.writable.replace(false) {
            self.flush(connection, handle)?;
        }
        Ok(())
    }

    fn flush<Data>(
        &mut self,
        connection: &Connection,
        handle: &LoopHandle<'_, Data>,
    ) -> Result<()> {
        match flush_pending(connection)? {
            false => {
                if let Some(writer) = self.writer.take() {
                    handle.remove(writer);
                }
            }
            true => {
                if self.writer.is_none() {
                    // The read source already registers the original descriptor.
                    let fd = connection.as_fd().try_clone_to_owned()?;
                    let writable = Rc::clone(&self.writable);
                    let writer = handle
                        .insert_source(
                            Generic::new(fd, Interest::WRITE, Mode::Level),
                            move |_, _, _| {
                                writable.set(true);
                                Ok(PostAction::Continue)
                            },
                        )
                        .map_err(|error| error.error)?;
                    self.writer = Some(writer);
                }
            }
        }
        Ok(())
    }
}

/// Flush requests and report whether there is still data waiting to be sent.
pub fn flush_pending(connection: &Connection) -> Result<bool> {
    match connection.flush() {
        Ok(()) => Ok(false),
        Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(connection_error(error)),
    }
}

fn connection_error(error: WaylandError) -> std::io::Error {
    match error {
        WaylandError::Io(error) => error,
        WaylandError::Protocol(error) => {
            tracing::error!("Wayland protocol error: {error}");
            std::io::Error::from_raw_os_error(libc::EPROTO)
        }
    }
}
