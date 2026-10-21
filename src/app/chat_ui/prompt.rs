//! Literal user text prepared by the existing admitted transcript worker.
use super::*;

pub(super) const QUEUE_PREVIEW_BYTES: usize = 480;
/// Called once at explicit local submission, or for an already bounded
/// provider queue source. Views receive this small immutable string.
pub(super) fn queue_preview(text: &str) -> String {
    let text = text.trim();
    text[..text.floor_char_boundary(QUEUE_PREVIEW_BYTES)].to_owned()
}
pub(super) struct QueuePreview {
    pub(super) source: Arc<LocalSource>,
    pub(super) preview: Arc<str>,
}
#[derive(Default)]
pub(super) struct ProviderQueue {
    snapshot: std::sync::Weak<Snapshot>,
    sources: HashMap<String, Arc<QueuePreview>>,
}
impl ProviderQueue {
    pub(super) fn get(&mut self, snapshot: &Arc<Snapshot>, id: &str) -> Option<Arc<QueuePreview>> {
        if self.snapshot.as_ptr() != Arc::as_ptr(snapshot) {
            if snapshot.queued_messages.len() > chat::QueuedMessage::LIMIT {
                self.clear();
                self.snapshot = Arc::downgrade(snapshot);
                return None;
            }
            self.sources.retain(|id, _| {
                snapshot
                    .queued_messages
                    .iter()
                    .take(chat::QueuedMessage::LIMIT)
                    .any(|message| &message.id == id)
            });
            let mut seen = HashSet::new();
            for message in snapshot
                .queued_messages
                .iter()
                .take(chat::QueuedMessage::LIMIT)
            {
                // The provider's public queue constructor bounds both fields.
                // Refuse malformed oversized descriptors rather than cloning
                // an arbitrary raw transcript on the UI thread.
                if message.id.len() > 256 {
                    continue;
                }
                if !seen.insert(message.id.as_str()) {
                    self.sources.remove(&message.id);
                    continue;
                }
                if message.text.len()
                    > chat::QueuedMessage::PREVIEW_BYTES + "\n[Queue preview truncated]".len()
                {
                    self.sources.remove(&message.id);
                    continue;
                }
                if self
                    .sources
                    .get(&message.id)
                    .is_some_and(|previous| previous.source.text() == message.text)
                {
                    continue;
                }
                self.sources.insert(
                    message.id.clone(),
                    Arc::new(QueuePreview {
                        source: LocalSource::new(message.text.clone()),
                        preview: queue_preview(&message.text).into(),
                    }),
                );
            }
            self.snapshot = Arc::downgrade(snapshot);
        }
        self.sources.get(id).cloned()
    }
    pub(super) fn clear(&mut self) {
        self.sources.clear();
        self.snapshot = Default::default();
    }
    pub(super) fn retained_bytes(&self) -> usize {
        self.sources
            .values()
            .map(|source| {
                source
                    .source
                    .allocation()
                    .saturating_add(source.preview.len())
            })
            .sum()
    }
}

/// Canonical local input, created once at submission and never provider history.
pub(super) struct LocalSource {
    pub(super) id: u64,
    text: String,
    pub(super) lease: Mutex<Option<Arc<preparation::Lease>>>,
    #[cfg(test)]
    pub(super) disposed: Mutex<Option<std::sync::mpsc::Sender<(std::thread::ThreadId, usize)>>>,
}
impl LocalSource {
    pub(super) fn new(text: String) -> Arc<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Arc::new(Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            text,
            lease: Mutex::default(),
            #[cfg(test)]
            disposed: Mutex::default(),
        })
    }
    pub(super) fn text(&self) -> &str {
        &self.text
    }
    pub(super) fn allocation(&self) -> usize {
        self.text
            .capacity()
            .saturating_add(std::mem::size_of::<Self>())
    }
}
impl Drop for LocalSource {
    fn drop(&mut self) {
        preparation::retire_local_text(preparation::RawLocal {
            text: std::mem::take(&mut self.text),
            lease: self
                .lease
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
            #[cfg(test)]
            disposed: self
                .disposed
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        });
    }
}

#[derive(Default)]
pub(super) struct LocalPresentation {
    pub(super) prepared: Option<Box<Prepared>>,
    pub(super) flow: flow_layout::Cache,
    pub(super) revision: u64,
}
impl Drop for LocalPresentation {
    fn drop(&mut self) {
        if let Some(prepared) = self.prepared.take() {
            preparation::retire_prompt(prepared);
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum PreviewPolicy {
    User,
    Notice,
}

pub(super) struct Prepared {
    pub(super) full: markdown::Text,
    preview: markdown::Text,
    pub(super) long: bool,
    #[cfg(test)]
    pub(super) created_on: std::thread::ThreadId,
}
impl Prepared {
    pub(super) fn new(body: &str, cancelled: &AtomicBool) -> Result<Box<Self>, String> {
        Self::with_policy(body, PreviewPolicy::User, cancelled)
    }
    pub(super) fn with_policy(
        body: &str,
        policy: PreviewPolicy,
        cancelled: &AtomicBool,
    ) -> Result<Box<Self>, String> {
        let check = || {
            if cancelled.load(Ordering::Acquire) {
                Err("Message loading was cancelled.".to_owned())
            } else {
                Ok(())
            }
        };
        check()?;
        let (preview, long) = match policy {
            PreviewPolicy::User => preview(body),
            PreviewPolicy::Notice => {
                let long = body.lines().take(view::NOTICE_LINES + 1).count() > view::NOTICE_LINES;
                (
                    if long {
                        body.lines()
                            .take(view::NOTICE_LINES)
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        String::new()
                    },
                    long,
                )
            }
        };
        let full = markdown::Text::literal(body.to_owned());
        full.plain();
        check()?;
        full.plain_prefixes();
        let preview = if long {
            markdown::Text::literal(preview)
        } else {
            full.clone()
        };
        preview.plain();
        preview.plain_prefixes();
        check()?;
        Ok(Box::new(Self {
            full,
            preview,
            long,
            #[cfg(test)]
            created_on: std::thread::current().id(),
        }))
    }
    pub(super) fn shown(&self, expanded: bool) -> &markdown::Text {
        if !self.long || expanded {
            &self.full
        } else {
            &self.preview
        }
    }
    pub(super) fn retained_bytes(&self) -> usize {
        // Exposed literal raw/plain backing, not allocator or renderer totals.
        self.full.len().saturating_mul(2)
            + if self.long {
                self.preview.len().saturating_mul(2)
            } else {
                0
            }
    }
}

/// The old preview semantics with a hard bound on the bytes inspected/copied.
/// More than 2000 characters is already long. Otherwise the complete source
/// has at most 8000 UTF-8 bytes and the line count is bounded as well.
/// A collapsed first-12-line preview removes at most twelve CR characters;
/// 2024 source characters therefore contain its first 2000 output characters.
pub(super) fn preview(body: &str) -> (String, bool) {
    let long = body.chars().take(2001).count() > 2000 || body.lines().take(13).count() > 12;
    if !long {
        return (body.to_owned(), false);
    }
    let bounded: String = body.chars().take(2024).collect();
    let preview = bounded
        .lines()
        .take(12)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(2000)
        .collect();
    (preview, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_queue_previews_keep_meaningful_text_after_whitespace_and_files_only_empty() {
        let body = format!("{}meaningful next task λ\r\n", " \t\r\n".repeat(4096));
        assert_eq!(queue_preview(&body), "meaningful next task λ");
        assert_eq!(queue_preview(&" \r\n\t".repeat(4096)), "");
        assert!(queue_preview(&"λ".repeat(4096)).len() <= QUEUE_PREVIEW_BYTES);
        let message = chat::QueuedMessage::new(
            "queue-id".into(),
            format!("{}provider task λ", " ".repeat(4096)),
        );
        let mut snapshot = Arc::new(Snapshot::default());
        Arc::make_mut(&mut snapshot).queued_messages.push(message);
        let mut cache = ProviderQueue::default();
        let previous = cache.get(&snapshot, "queue-id").unwrap();
        assert_eq!(&*previous.preview, "provider task λ");
        let mut appended = (*snapshot).clone();
        appended.generation += 1;
        appended.busy = !appended.busy;
        let appended = Arc::new(appended);
        assert!(
            Arc::ptr_eq(&previous, &cache.get(&appended, "queue-id").unwrap()),
            "unrelated publications preserve exact immutable queue authority"
        );
        let mut changed = (*appended).clone();
        changed.queued_messages[0].text = "same id, rewritten source".into();
        let changed = Arc::new(changed);
        assert!(!Arc::ptr_eq(
            &previous,
            &cache.get(&changed, "queue-id").unwrap()
        ));
        let removed = Arc::new(Snapshot::default());
        assert!(cache.get(&removed, "queue-id").is_none());
    }

    #[test]
    fn provider_queue_refuses_duplicate_oversized_and_over_limit_source_authority() {
        let valid = chat::QueuedMessage::new("same".into(), "valid exact source".into());
        let mut oversized = valid.clone();
        oversized.text = "x".repeat(chat::QueuedMessage::PREVIEW_BYTES + 128);
        for messages in [
            vec![oversized.clone(), valid.clone()],
            vec![valid.clone(), oversized],
            vec![valid.clone(), valid.clone()],
        ] {
            let snapshot = Arc::new(Snapshot {
                queued_messages: messages,
                ..Snapshot::default()
            });
            assert!(
                ProviderQueue::default().get(&snapshot, "same").is_none(),
                "duplicate IDs cannot bind a record to a different raw source"
            );
        }
        let mut oversized_id = valid.clone();
        oversized_id.id = "id".repeat(100_000);
        let snapshot = Arc::new(Snapshot {
            queued_messages: vec![oversized_id],
            ..Snapshot::default()
        });
        assert!(ProviderQueue::default().get(&snapshot, "same").is_none());
        let messages = (0..=chat::QueuedMessage::LIMIT)
            .map(|index| chat::QueuedMessage::new(format!("message-{index}"), "source".into()))
            .collect();
        let snapshot = Arc::new(Snapshot {
            queued_messages: messages,
            ..Snapshot::default()
        });
        assert!(
            ProviderQueue::default()
                .get(&snapshot, "message-0")
                .is_none(),
            "malformed queue length cannot bypass aggregate source limits"
        );
    }

    #[test]
    fn bounded_preview_matches_literal_legacy_semantics_at_unicode_and_crlf_boundaries() {
        for body in [
            String::new(),
            "literal **not bold** [no link](https://invalid.test)".into(),
            "x".repeat(2000),
            "x".repeat(2001),
            "👩‍💻".repeat(1000),
            "line\r\n".repeat(12),
            "line\r\n".repeat(13),
            "\r\n".repeat(1000),
            format!("{}\r\n{}", "x".repeat(1999), "z".repeat(4096)),
            format!("{}\r\n{}", "x".repeat(2000), "z".repeat(4096)),
            format!("{}\n{}", "line\r\n".repeat(11), "x".repeat(65_536)),
            "\r".repeat(65_536),
        ] {
            let long = body.lines().count() > 12 || body.chars().count() > 2000;
            let legacy = if long {
                body.lines()
                    .take(12)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .chars()
                    .take(2000)
                    .collect()
            } else {
                body.clone()
            };
            assert_eq!(preview(&body), (legacy, long));
        }
    }
}
