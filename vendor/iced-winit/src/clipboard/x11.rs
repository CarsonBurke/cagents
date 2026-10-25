//! Bounded raw selection reads for the X11 backend, including ICCCM INCR.
//!
//! Each read owns a fresh requestor connection: abandoned transfers and their
//! queued events cannot contaminate a later paste. Wayland still uses
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

    pub(super) fn read_text(
        &self,
        kind: Kind,
    ) -> Result<Option<String>, Error> {
        self.read_text_bounded(kind, MAX_BYTES)
    }

    fn read_text_bounded(
        &self,
        kind: Kind,
        max_bytes: usize,
    ) -> Result<Option<String>, Error> {
        let deadline = Instant::now() + TIMEOUT;
        // A fresh requestor also retires an interrupted INCR transfer. Never
        // share the legacy text reader's property or queued selection events.
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
        // Preserve the text backend's direct UTF8_STRING contract. A text
        // owner need not advertise a new MIME name or answer TARGETS first.
        transfer
            .convert(
                transfer.context.atoms.utf8_string,
                transfer.context.atoms.utf8_string,
                8,
                max_bytes,
            )?
            .map(|reply| String::from_utf8(reply.value))
            .transpose()
            .map_err(Into::into)
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
        delete: bool,
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
                delete,
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
            let reply = self.property(expected, format, limit, true)?;
            self.check_owner()?;
            return Ok(Some(reply));
        }
        let announcement =
            self.property(self.context.atoms.incr, 32, 4, false)?;
        let mut sizes = announcement
            .value32()
            .ok_or_else(|| invalid("invalid INCR size"))?;
        let size = sizes.next().ok_or_else(|| invalid("missing INCR size"))?;
        if sizes.next().is_some() || size as usize > limit {
            return Err(invalid("invalid or excessive INCR size"));
        }
        // Validate the announcement before acknowledging it: an oversized
        // declaration must not start an unnecessary payload transfer.
        connection.delete_property(
            self.context.window,
            self.context.atoms.property,
        )?;
        // Ignore queued old PropertyNotify events; the property may already
        // be absent when read.
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
            let chunk =
                self.property(expected, format, limit - data.len(), true)?;
            connection.flush()?;
            if chunk.value.is_empty() {
                self.check_owner()?;
                return Ok(Some(GetPropertyReply {
                    value_len: (data.len() / usize::from(format / 8)) as u32,
                    value: data,
                    ..chunk
                }));
            }
            let needed = data.len() + chunk.value.len();
            if needed > data.capacity() {
                // Amortize tiny INCR chunks without reserving beyond the cap
                // or trusting the owner's announced size.
                let capacity =
                    data.capacity().saturating_mul(2).max(needed).min(limit);
                data.try_reserve_exact(capacity - data.len())?;
            }
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
            Ok("timeout") => {
                let error = data.expect_err("stalled transfer was accepted");
                assert_eq!(
                    error.downcast_ref::<io::Error>().unwrap().kind(),
                    io::ErrorKind::TimedOut
                );
                assert!(start.elapsed() >= TIMEOUT);
            }
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

    // The production requestor talks to a real X server and a separate owner
    // connection. These ignored tests run only on an owned private display.
    enum TextOffer {
        Direct(Vec<u8>),
        Incremental(Vec<u8>),
        WrongFormat,
        WrongType,
        WrongProperty,
        ChangeOwner,
        Refuse,
        Stall,
    }

    struct TextOwner {
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
        destroyed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TextOwner {
        fn start(display: &str, offers: Vec<TextOffer>) -> Self {
            use std::sync::{
                Arc,
                atomic::{AtomicBool, AtomicUsize, Ordering},
            };
            use x11rb::{protocol::xproto::EventMask, wrapper::ConnectionExt};
            let context = Context::new(Some(display)).unwrap();
            for selection in [context.atoms.clipboard, context.atoms.primary] {
                context
                    .connection
                    .set_selection_owner(
                        context.window,
                        selection,
                        x11rb::CURRENT_TIME,
                    )
                    .unwrap()
                    .check()
                    .unwrap();
                assert_eq!(
                    context
                        .connection
                        .get_selection_owner(selection)
                        .unwrap()
                        .reply()
                        .unwrap()
                        .owner,
                    context.window
                );
            }
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = Arc::clone(&stop);
            let destroyed = Arc::new(AtomicUsize::new(0));
            let observed_destroy = Arc::clone(&destroyed);
            let display = display.to_owned();
            let worker = thread::spawn(move || {
                let deadline = Instant::now()
                    + Duration::from_secs(5 * (offers.len() as u64 + 1));
                let mut _replacement_owner: Option<Context> = None;
                let mut offers = std::collections::VecDeque::from(offers);
                let mut pending: Vec<(XWindow, Atom, Atom, Vec<u8>, usize)> =
                    Vec::new();
                while !stopping.load(Ordering::Acquire) {
                    assert!(
                        Instant::now() < deadline,
                        "private selection owner exceeded its bound"
                    );
                    let Some(event) =
                        context.connection.poll_for_event().unwrap()
                    else {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    };
                    match event {
                        Event::SelectionRequest(request) => {
                            // Advertise no TARGETS, including to private
                            // desktop clients. Refusal must not consume the
                            // next scripted direct UTF8_STRING offer.
                            if request.target == context.atoms.targets {
                                context
                                    .connection
                                    .send_event(
                                        false,
                                        request.requestor,
                                        EventMask::NO_EVENT,
                                        x11rb::protocol::xproto::SelectionNotifyEvent {
                                            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
                                            sequence: 0,
                                            time: request.time,
                                            requestor: request.requestor,
                                            selection: request.selection,
                                            target: request.target,
                                            property: x11rb::NONE,
                                        },
                                    )
                                    .unwrap()
                                    .check()
                                    .unwrap();
                                context.connection.flush().unwrap();
                                continue;
                            }
                            assert_eq!(
                                request.target,
                                context.atoms.utf8_string
                            );
                            context
                                .connection
                                .change_window_attributes(
                                    request.requestor,
                                    &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                                        .event_mask(
                                            EventMask::PROPERTY_CHANGE
                                                | EventMask::STRUCTURE_NOTIFY,
                                        ),
                                )
                                .unwrap()
                                .check()
                                .unwrap();
                            let offer = offers
                                .pop_front()
                                .expect("unexpected selection read");
                            if matches!(offer, TextOffer::Stall) {
                                continue;
                            }
                            let mut property = request.property;
                            match offer {
                                TextOffer::Direct(bytes) => {
                                    context
                                        .connection
                                        .change_property8(
                                            x11rb::protocol::xproto::PropMode::REPLACE,
                                            request.requestor,
                                            property,
                                            request.target,
                                            &bytes,
                                        )
                                        .unwrap()
                                        .check()
                                        .unwrap();
                                }
                                TextOffer::Incremental(bytes) => {
                                    context
                                        .connection
                                        .change_property32(
                                            x11rb::protocol::xproto::PropMode::REPLACE,
                                            request.requestor,
                                            property,
                                            context.atoms.incr,
                                            &[u32::try_from(bytes.len()).unwrap()],
                                        )
                                        .unwrap()
                                        .check()
                                        .unwrap();
                                    pending.push((
                                        request.requestor,
                                        property,
                                        request.target,
                                        bytes,
                                        0,
                                    ));
                                }
                                TextOffer::WrongFormat => {
                                    context
                                        .connection
                                        .change_property32(
                                            x11rb::protocol::xproto::PropMode::REPLACE,
                                            request.requestor,
                                            property,
                                            request.target,
                                            &[65],
                                        )
                                        .unwrap()
                                        .check()
                                        .unwrap();
                                }
                                TextOffer::WrongType => {
                                    context.connection.change_property8(
                                        x11rb::protocol::xproto::PropMode::REPLACE,
                                        request.requestor, property, context.atoms.string, b"wrong type",
                                    ).unwrap().check().unwrap();
                                }
                                TextOffer::WrongProperty => {
                                    property = context.atoms.string;
                                    context.connection.change_property8(
                                        x11rb::protocol::xproto::PropMode::REPLACE,
                                        request.requestor, property, request.target, b"wrong property bytes",
                                    ).unwrap().check().unwrap();
                                }
                                TextOffer::ChangeOwner => {
                                    let next =
                                        Context::new(Some(&display)).unwrap();
                                    next.connection
                                        .set_selection_owner(
                                            next.window,
                                            request.selection,
                                            x11rb::CURRENT_TIME,
                                        )
                                        .unwrap()
                                        .check()
                                        .unwrap();
                                    _replacement_owner = Some(next);
                                    context.connection.change_property8(
                                        x11rb::protocol::xproto::PropMode::REPLACE,
                                        request.requestor, property, request.target, b"changed owner bytes",
                                    ).unwrap().check().unwrap();
                                }
                                TextOffer::Refuse => property = x11rb::NONE,
                                TextOffer::Stall => unreachable!(),
                            }
                            context
                                .connection
                                .send_event(
                                    false,
                                    request.requestor,
                                    EventMask::NO_EVENT,
                                    x11rb::protocol::xproto::SelectionNotifyEvent {
                                        response_type:
                                            x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
                                        sequence: 0,
                                        time: request.time,
                                        requestor: request.requestor,
                                        selection: request.selection,
                                        target: request.target,
                                        property,
                                    },
                                )
                                .unwrap()
                                .check()
                                .unwrap();
                            context.connection.flush().unwrap();
                        }
                        Event::PropertyNotify(event)
                            if event.state == Property::DELETE =>
                        {
                            let Some(index) =
                                pending.iter().position(|entry| {
                                    entry.0 == event.window
                                        && entry.1 == event.atom
                                })
                            else {
                                continue;
                            };
                            let (window, property, target, bytes, offset) =
                                &mut pending[index];
                            let end = offset
                                .saturating_add(16 * 1024)
                                .min(bytes.len());
                            context
                                .connection
                                .change_property8(
                                    x11rb::protocol::xproto::PropMode::REPLACE,
                                    *window,
                                    *property,
                                    *target,
                                    &bytes[*offset..end],
                                )
                                .unwrap()
                                .check()
                                .unwrap();
                            context.connection.flush().unwrap();
                            let finished = *offset == bytes.len();
                            *offset = end;
                            if finished {
                                pending.remove(index);
                            }
                        }
                        Event::DestroyNotify(event) => {
                            pending.retain(|entry| entry.0 != event.window);
                            observed_destroy.fetch_add(1, Ordering::Release);
                        }
                        _ => {}
                    }
                }
            });
            Self {
                stop,
                thread: Some(worker),
                destroyed,
            }
        }
    }

    impl Drop for TextOwner {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
            let result = self.thread.take().unwrap().join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }

    #[test]
    #[ignore = "requires an owned private X11 display"]
    fn text_selection_preserves_standard_and_primary_direct_and_incr() {
        let display = std::env::var("DISPLAY").unwrap();
        let megabyte = vec![b'x'; 1024 * 1024];
        let _owner = TextOwner::start(
            &display,
            vec![
                TextOffer::Direct(megabyte.clone()),
                TextOffer::Incremental(megabyte.clone()),
                TextOffer::Direct(megabyte.clone()),
                TextOffer::Incremental(megabyte.clone()),
                TextOffer::Direct("π🧪".as_bytes().to_vec()),
                TextOffer::Direct(vec![0xff]),
                TextOffer::WrongFormat,
                TextOffer::WrongType,
                TextOffer::Direct(vec![b'x'; 33]),
                TextOffer::Incremental(vec![b'x'; 33]),
                TextOffer::Refuse,
                TextOffer::WrongProperty,
                TextOffer::ChangeOwner,
            ],
        );
        let reader = Reader { display };
        // Real raw MIME negotiation is refused without consuming the next
        // scripted text offer; subsequent UTF8 assertions verify the queue.
        assert!(
            reader
                .read(Kind::Standard, &["image/png".into()])
                .unwrap()
                .is_none()
        );
        for kind in [Kind::Standard, Kind::Primary] {
            for _ in 0..2 {
                assert_eq!(
                    reader.read_text(kind).unwrap().unwrap().as_bytes(),
                    megabyte
                );
            }
        }
        assert_eq!(
            reader.read_text(Kind::Standard).unwrap().as_deref(),
            Some("π🧪")
        );
        assert!(
            reader
                .read_text(Kind::Standard)
                .unwrap_err()
                .is::<std::string::FromUtf8Error>()
        );
        assert_eq!(
            reader
                .read_text(Kind::Standard)
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            reader
                .read_text(Kind::Standard)
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            reader
                .read_text_bounded(Kind::Standard, 32)
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            reader
                .read_text_bounded(Kind::Standard, 32)
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(reader.read_text(Kind::Standard).unwrap().is_none());
        let wrong_property_start = Instant::now();
        assert_eq!(
            reader
                .read_text(Kind::Standard)
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(
            wrong_property_start.elapsed() >= TIMEOUT
                && wrong_property_start.elapsed() < Duration::from_secs(5)
        );
        assert_eq!(
            reader
                .read_text(Kind::Standard)
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    #[ignore = "requires an owned private X11 display"]
    fn text_selection_timeout_retires_requestor_before_next_read() {
        let display = std::env::var("DISPLAY").unwrap();
        let owner = TextOwner::start(
            &display,
            vec![TextOffer::Stall, TextOffer::Direct(b"fresh text".to_vec())],
        );
        let reader = Reader { display };
        // Real raw MIME negotiation is refused without consuming the next
        // scripted text offer; subsequent UTF8 assertions verify the queue.
        assert!(
            reader
                .read(Kind::Standard, &["image/png".into()])
                .unwrap()
                .is_none()
        );
        let start = Instant::now();
        assert_eq!(
            reader
                .read_text(Kind::Standard)
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(
            start.elapsed() >= TIMEOUT
                && start.elapsed() < Duration::from_secs(5)
        );
        let retired = Instant::now() + Duration::from_secs(1);
        while owner.destroyed.load(std::sync::atomic::Ordering::Acquire) == 0 {
            assert!(
                Instant::now() < retired,
                "timed-out requestor window was retained"
            );
            thread::yield_now();
        }
        assert_eq!(
            reader.read_text(Kind::Standard).unwrap().as_deref(),
            Some("fresh text")
        );
    }
}
