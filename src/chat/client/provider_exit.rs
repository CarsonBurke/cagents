//! On-demand output stays off watch publications and the UI thread.
use super::*;

pub(crate) const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_PAGE_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub(crate) struct Output {
    /// At most 24 MiB: each of the retained 8 MiB raw bytes can require
    /// one three-byte Unicode replacement character. Cleanup never expands it.
    pub text: Arc<String>,
    pub omitted: u64,
}

pub(super) struct Page {
    pub log_id: String,
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub next_offset: Option<u64>,
    pub omitted: u64,
}

pub(super) type Pending =
    Arc<Mutex<BTreeMap<u64, crossbeam_channel::Sender<Result<Page, String>>>>>;

pub(super) fn disconnected(pending: &Pending, reason: &str) {
    for (_, reply) in std::mem::take(&mut *pending.lock().unwrap_or_else(|p| p.into_inner())) {
        let _ = reply.try_send(Err(reason.to_owned()));
    }
}

fn append_page(
    output: &mut Vec<u8>,
    log: &super::super::ProviderExitLog,
    page: Page,
) -> Result<Option<u64>, String> {
    if page.log_id != log.id || page.offset != output.len() as u64 {
        return Err("The provider output changed while it was being read".into());
    }
    if page.bytes.len() > MAX_PAGE_BYTES || page.omitted != log.omitted {
        return Err("The daemon returned inconsistent provider output".into());
    }
    let length = output
        .len()
        .checked_add(page.bytes.len())
        .filter(|length| *length <= MAX_OUTPUT_BYTES && (*length as u64) <= log.bytes)
        .ok_or("Provider output exceeds its retained size")?;
    match page.next_offset {
        Some(next) if next == length as u64 && next > page.offset && next < log.bytes => {}
        None if length as u64 == log.bytes => {}
        _ => return Err("The daemon returned an invalid provider output cursor".into()),
    }
    output.extend_from_slice(&page.bytes);
    Ok(page.next_offset)
}

fn readable(bytes: Vec<u8>) -> String {
    let text = String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned());
    let mut text = super::super::terminal_output::display(text);
    text.retain(|character| !character.is_control() || matches!(character, '\n' | '\t'));
    text
}

impl ClientHandle {
    /// Read the immutable retained log over this authenticated connection.
    /// The caller runs this on its blocking pool; there is no new idle thread.
    pub(crate) fn provider_exit_output(
        &self,
        log: &super::super::ProviderExitLog,
        cancelled: &AtomicBool,
    ) -> Result<Output, String> {
        if log.bytes > MAX_OUTPUT_BYTES as u64 || log.id.is_empty() || log.id.len() > 256 {
            return Err("The retained provider output is too large or unavailable".into());
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut bytes = Vec::with_capacity(log.bytes as usize);
        let mut offset = 0;
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err("Provider output loading was cancelled".into());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("Loading provider output timed out".into());
            }
            let page =
                self.provider_exit_page(&log.id, offset, remaining.min(Duration::from_secs(5)))?;
            match append_page(&mut bytes, log, page)? {
                Some(next) => offset = next,
                None => break,
            }
        }
        if cancelled.load(Ordering::Acquire) {
            return Err("Provider output loading was cancelled".into());
        }
        Ok(Output {
            text: Arc::new(readable(bytes)),
            omitted: log.omitted,
        })
    }

    fn provider_exit_page(
        &self,
        log_id: &str,
        offset: u64,
        timeout: Duration,
    ) -> Result<Page, String> {
        if !self.inner.alive.load(Ordering::Acquire) {
            return Err("Reconnect this conversation before loading its saved output".into());
        }
        let request = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let (reply, response) = crossbeam_channel::bounded(1);
        {
            let mut pending = self
                .inner
                .diagnostic_pending
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if pending.len() >= 8 {
                return Err("Provider output requests are busy; try again shortly".into());
            }
            pending.insert(request, reply);
        }
        let result = transmit(
            &self.inner.wire,
            &self.inner.proof,
            request,
            ClientRequest::ChatProviderExitOutput {
                chat_id: self.inner.id,
                log_id: log_id.to_owned(),
                offset,
            },
        )
        .and_then(|()| {
            response
                .recv_timeout(timeout)
                .map_err(|_| "Loading provider output timed out".to_owned())?
        });
        self.inner
            .diagnostic_pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&request);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_output_invalid_utf8_has_a_bounded_readable_expansion() {
        let bytes = vec![0xff; 4096];
        let text = readable(bytes.clone());
        assert_eq!(text.len(), 3 * bytes.len());
        assert_eq!(text.chars().count(), bytes.len());
        assert!(text.chars().all(|character| character == '\u{fffd}'));
    }

    #[test]
    fn provider_output_pages_preserve_split_utf8_and_terminal_sequences() {
        let raw = b"first\n\x1b[31mred\x1b[0m \xe2\x82\xac\n";
        let log = super::super::super::ProviderExitLog {
            id: "immutable-log".into(),
            bytes: raw.len() as u64,
            omitted: 0,
        };
        for split in 1..raw.len() {
            let mut output = Vec::new();
            assert_eq!(
                append_page(
                    &mut output,
                    &log,
                    Page {
                        log_id: log.id.clone(),
                        offset: 0,
                        bytes: raw[..split].to_vec(),
                        next_offset: Some(split as u64),
                        omitted: 0,
                    }
                )
                .unwrap(),
                Some(split as u64)
            );
            assert_eq!(
                append_page(
                    &mut output,
                    &log,
                    Page {
                        log_id: log.id.clone(),
                        offset: split as u64,
                        bytes: raw[split..].to_vec(),
                        next_offset: None,
                        omitted: 0,
                    }
                )
                .unwrap(),
                None
            );
            assert_eq!(readable(output), "first\nred €\n");
        }
    }

    #[test]
    fn provider_output_pages_reject_changed_identity_and_nonprogressing_cursors() {
        let log = super::super::super::ProviderExitLog {
            id: "pinned".into(),
            bytes: 2,
            omitted: 1,
        };
        let page = || Page {
            log_id: log.id.clone(),
            offset: 0,
            bytes: vec![b'a'],
            next_offset: Some(1),
            omitted: 1,
        };
        let mut changed = page();
        changed.log_id = "different".into();
        assert!(append_page(&mut Vec::new(), &log, changed).is_err());
        let mut stalled = page();
        stalled.bytes.clear();
        stalled.next_offset = Some(0);
        assert!(append_page(&mut Vec::new(), &log, stalled).is_err());
        let mut truncated = page();
        truncated.next_offset = None;
        assert!(append_page(&mut Vec::new(), &log, truncated).is_err());
        let mut omitted = page();
        omitted.omitted = 2;
        assert!(append_page(&mut Vec::new(), &log, omitted).is_err());
    }
}
