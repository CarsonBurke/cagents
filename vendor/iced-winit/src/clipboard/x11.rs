//! Bounded raw selection reads for the X11 backend, including ICCCM INCR.
//!
//! Each read owns a fresh requestor connection: abandoned transfers and their
//! queued events cannot contaminate a later paste. Text and Wayland still use
//! window_clipboard's existing implementation.
use crate::core::clipboard::Kind;
use clipboard_x11::Context;
use std::{
    io, thread,
    time::{Duration, Instant},
};
use winit::{
    raw_window_handle::{HasDisplayHandle, RawDisplayHandle},
    window::Window,
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            Atom, AtomEnum, ConnectionExt, GetPropertyReply, Property,
            Window as XWindow,
        },
    },
};

type Error = Box<dyn std::error::Error + Send + Sync>;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_TARGET_BYTES: usize = 16 * 1024;
const TIMEOUT: Duration = Duration::from_secs(3);

pub(super) struct Reader {
    display: String,
}

impl Reader {
    pub(super) fn for_window(window: &dyn Window) -> Option<Self> {
        match window.display_handle().ok()?.as_raw() {
            RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_) => {
                Some(Self {
                    display: std::env::var("DISPLAY").ok()?,
                })
            }
            _ => None,
        }
    }

    pub(super) fn read(
        &self,
        kind: Kind,
        mimes: &[String],
    ) -> Result<Option<(Vec<u8>, String)>, Error> {
        self.read_bounded(kind, mimes, MAX_BYTES)
    }

    fn read_bounded(
        &self,
        kind: Kind,
        mimes: &[String],
        max_bytes: usize,
    ) -> Result<Option<(Vec<u8>, String)>, Error> {
        if mimes.is_empty() {
            return Ok(None);
        }
        let deadline = Instant::now() + TIMEOUT;
        let context = Context::new(Some(&self.display))?;
        let selection = match kind {
            Kind::Standard => context.atoms.clipboard,
            Kind::Primary => context.atoms.primary,
        };
        let owner = context
            .connection
            .get_selection_owner(selection)?
            .reply()?
            .owner;
        if owner == x11rb::NONE {
            return Ok(None);
        }
        let transfer = Transfer {
            context,
            selection,
            owner,
            deadline,
        };
        let Some(targets) = transfer.convert(
            transfer.context.atoms.targets,
            AtomEnum::ATOM.into(),
            32,
            MAX_TARGET_BYTES,
        )?
        else {
            return Ok(None);
        };
        let offered: Vec<_> = targets
            .value32()
            .ok_or_else(|| invalid("invalid TARGETS format"))?
            .collect();
        for mime in mimes {
            transfer.check_deadline()?;
            // only_if_exists avoids interning arbitrary unsupported names.
            let atom = transfer
                .context
                .connection
                .intern_atom(true, mime.as_bytes())?
                .reply()?
                .atom;
            if atom == x11rb::NONE || !offered.contains(&atom) {
                continue;
            }
            if let Some(data) = transfer.convert(atom, atom, 8, max_bytes)? {
                transfer.check_owner()?;
                return Ok(Some((data.value, mime.clone())));
            }
        }
        Ok(None)
    }
}

struct Transfer {
    context: Context,
    selection: Atom,
    owner: XWindow,
    deadline: Instant,
}

fn invalid(message: &'static str) -> Error {
    io::Error::new(io::ErrorKind::InvalidData, message).into()
}

impl Transfer {
    fn check_deadline(&self) -> Result<(), Error> {
        if Instant::now() >= self.deadline {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "X11 clipboard transfer timed out",
            )
            .into())
        } else {
            Ok(())
        }
    }

    fn check_owner(&self) -> Result<(), Error> {
        if self
            .context
            .connection
            .get_selection_owner(self.selection)?
            .reply()?
            .owner
            != self.owner
        {
            Err(invalid("clipboard owner changed during transfer"))
        } else {
            Ok(())
        }
    }

    fn event(&self) -> Result<Event, Error> {
        loop {
            self.check_deadline()?;
            if let Some(event) = self.context.connection.poll_for_event()? {
                return Ok(event);
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    // Probe length before fetching bytes; never request an unbounded property.
    fn property(
        &self,
        expected: Atom,
        format: u8,
        remaining: usize,
    ) -> Result<GetPropertyReply, Error> {
        self.check_deadline()?;
        let connection = &self.context.connection;
        let header = connection
            .get_property(
                false,
                self.context.window,
                self.context.atoms.property,
                AtomEnum::ANY,
                0,
                0,
            )?
            .reply()?;
        if header.type_ != expected || header.format != format {
            return Err(invalid(
                "unexpected X11 clipboard property type or format",
            ));
        }
        let length = header.bytes_after as usize;
        if length > remaining {
            return Err(invalid("X11 clipboard data exceeds size limit"));
        }
        let reply = connection
            .get_property(
                true,
                self.context.window,
                self.context.atoms.property,
                expected,
                0,
                length.div_ceil(4) as u32,
            )?
            .reply()?;
        if reply.type_ != expected
            || reply.format != format
            || reply.bytes_after != 0
            || reply.value.len() > remaining
        {
            return Err(invalid("X11 clipboard property changed during read"));
        }
        Ok(reply)
    }

    fn convert(
        &self,
        target: Atom,
        expected: Atom,
        format: u8,
        limit: usize,
    ) -> Result<Option<GetPropertyReply>, Error> {
        self.check_owner()?;
        self.check_deadline()?;
        let connection = &self.context.connection;
        connection.delete_property(
            self.context.window,
            self.context.atoms.property,
        )?;
        connection.convert_selection(
            self.context.window,
            self.selection,
            target,
            self.context.atoms.property,
            x11rb::CURRENT_TIME,
        )?;
        connection.flush()?;
        loop {
            match self.event()? {
                Event::SelectionNotify(event)
                    if event.requestor == self.context.window
                        && event.selection == self.selection
                        && event.target == target =>
                {
                    if event.property == x11rb::NONE {
                        return Ok(None);
                    }
                    if event.property == self.context.atoms.property {
                        break;
                    }
                }
                _ => {}
            }
        }
        let header = connection
            .get_property(
                false,
                self.context.window,
                self.context.atoms.property,
                AtomEnum::ANY,
                0,
                0,
            )?
            .reply()?;
        if header.type_ != self.context.atoms.incr {
            let reply = self.property(expected, format, limit)?;
            self.check_owner()?;
            return Ok(Some(reply));
        }
        let announcement = self.property(self.context.atoms.incr, 32, 4)?;
        let mut sizes = announcement
            .value32()
            .ok_or_else(|| invalid("invalid INCR size"))?;
        let size = sizes.next().ok_or_else(|| invalid("missing INCR size"))?;
        if sizes.next().is_some() || size as usize > limit {
            return Err(invalid("invalid or excessive INCR size"));
        }
        // GetProperty(delete=true) acknowledges INCR. Ignore queued old
        // PropertyNotify events; the property may already be absent when read.
        connection.flush()?;
        let mut data = Vec::new();
        loop {
            match self.event()? {
                Event::PropertyNotify(event)
                    if event.window == self.context.window
                        && event.atom == self.context.atoms.property
                        && event.state == Property::NEW_VALUE => {}
                _ => continue,
            }
            self.check_owner()?;
            let header = connection
                .get_property(
                    false,
                    self.context.window,
                    self.context.atoms.property,
                    AtomEnum::ANY,
                    0,
                    0,
                )?
                .reply()?;
            if header.type_ == x11rb::NONE {
                continue;
            }
            let chunk = self.property(expected, format, limit - data.len())?;
            connection.flush()?;
            if chunk.value.is_empty() {
                self.check_owner()?;
                return Ok(Some(GetPropertyReply {
                    value_len: (data.len() / usize::from(format / 8)) as u32,
                    value: data,
                    ..chunk
                }));
            }
            data.try_reserve_exact(chunk.value.len())?;
            data.extend_from_slice(&chunk.value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Paired with an actual X11 selection owner (direct, INCR and malformed
    // modes), rather than mocking the event or property parsing implementation.
    #[test]
    #[ignore = "requires a private X11 clipboard fixture owner"]
    fn raw_selection_fixture() {
        let reader = Reader {
            display: std::env::var("DISPLAY").unwrap(),
        };
        let start = Instant::now();
        let limit = std::env::var("X11_FIXTURE_LIMIT")
            .ok()
            .map(|s| s.parse().unwrap())
            .unwrap_or(MAX_BYTES);
        let data = reader.read_bounded(
            Kind::Standard,
            &["image/png".into(), "image/jpeg".into()],
            limit,
        );
        match std::env::var("X11_FIXTURE_EXPECT").as_deref() {
            Ok("error") => {
                assert!(data.is_err(), "malformed transfer was accepted")
            }
            Ok("none") => assert!(data.unwrap().is_none()),
            _ => {
                let (bytes, mime) =
                    data.unwrap().expect("clipboard image missing");
                assert_eq!(mime, "image/png");
                assert_eq!(
                    bytes,
                    std::fs::read(std::env::var("X11_FIXTURE_FILE").unwrap())
                        .unwrap()
                );
            }
        }
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
