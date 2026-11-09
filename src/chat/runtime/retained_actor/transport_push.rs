//! Coalesced readiness, independent of transcript size or visibility.
use super::*;
use std::io::{Read, Write};

/// Writes never wait for the observer. One unread byte is already a wakeup.
#[derive(Default)]
pub(super) struct Watcher(Mutex<Option<UnixStream>>);
impl Watcher {
    pub(super) fn subscribe(&self) -> Result<UnixStream, String> {
        let (writer, reader) = UnixStream::pair().map_err(|e| e.to_string())?;
        writer.set_nonblocking(true).map_err(|e| e.to_string())?;
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(writer);
        // Registration cannot miss a publication between attach and watch.
        self.notify();
        Ok(reader)
    }
    pub(super) fn clear(&self) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
    pub(super) fn notify(&self) {
        if let Some(writer) = self.0.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            let _ = (&*writer).write(&[1]);
        }
    }
}

pub(super) struct Wake {
    reader: UnixStream,
    writer: UnixStream,
}
impl Wake {
    pub(super) fn new() -> Result<Self, String> {
        let (reader, writer) = UnixStream::pair().map_err(|e| e.to_string())?;
        for end in [&reader, &writer] {
            end.set_nonblocking(true).map_err(|e| e.to_string())?;
        }
        Ok(Self { reader, writer })
    }
    pub(super) fn notify(&self) {
        let _ = (&self.writer).write(&[1]);
    }
    pub(super) fn wait(&self, watch: Option<&UnixStream>, retry: bool) -> Result<(), String> {
        let mut events = vec![libc::pollfd {
            fd: self.reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        if let Some(watch) = watch {
            events.push(libc::pollfd {
                fd: watch.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        loop {
            let result = unsafe {
                libc::poll(
                    events.as_mut_ptr(),
                    events.len() as libc::nfds_t,
                    if retry { 50 } else { -1 },
                )
            };
            if result >= 0 {
                break;
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        if events
            .iter()
            .any(|event| event.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0)
        {
            return Err("Retained actor readiness connection closed".into());
        }
        drain(&self.reader);
        if let Some(watch) = watch {
            drain(watch);
        }
        Ok(())
    }
}
pub(super) fn drain(reader: &UnixStream) {
    let mut buffer = [0; 256];
    while (&*reader).read(&mut buffer).is_ok_and(|n| n > 0) {}
}
