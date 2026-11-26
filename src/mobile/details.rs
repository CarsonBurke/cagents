//! On-demand access to content owned by a watched transcript.
use std::{
    io::Cursor,
    sync::{Arc, Mutex},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use cagents_mobile_api as api;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{chat, terminal::ipc::ServerResponse};

const IMAGE_PAGE_BYTES: usize = 192 * 1024;
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_UPLOAD_EDGE: u32 = 2048;
const MAX_UPLOAD_DECODE: usize = 32 * 1024 * 1024;
const MAX_ITEM_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 4 * 1024;
const MAX_CURSOR_BYTES: usize = 512;

pub enum Request {
    Output {
        item: String,
        cursor: Option<String>,
    },
    Diff {
        item: String,
        path: String,
        cursor: Option<String>,
    },
    Image {
        item: String,
        cursor: Option<String>,
    },
}

impl Request {
    fn target(&self) -> (u8, &str, &str, Option<&str>) {
        match self {
            Self::Output { item, cursor } => (0, item, "", cursor.as_deref()),
            Self::Diff { item, path, cursor } => (1, item, path, cursor.as_deref()),
            Self::Image { item, cursor } => (2, item, "", cursor.as_deref()),
        }
    }

    pub fn validate(&self) -> Result<(), api::Error> {
        let (_, item, path, cursor) = self.target();
        if item.len() > MAX_ITEM_BYTES || path.len() > MAX_PATH_BYTES {
            return Err(error(
                api::ErrorCode::BadRequest,
                "This transcript detail has an invalid item or path.",
            ));
        }
        if cursor.is_some_and(|cursor| cursor.len() > MAX_CURSOR_BYTES) {
            return Err(bad_cursor());
        }
        Ok(())
    }
}

/// Each phone connection may queue one detail request and hold one image
/// between pages, even when it follows several conversations.
#[derive(Default)]
pub struct Shared {
    slot: Arc<()>,
    image: Mutex<Option<ImageTransfer>>,
}

impl Shared {
    // Only the bridge's input thread admits requests. The worker keeps the
    // returned lease until it can answer, including while the daemon reads.
    pub fn reserve(&self) -> Option<Arc<()>> {
        (Arc::strong_count(&self.slot) == 1).then(|| self.slot.clone())
    }
}

pub struct Scope {
    identity: Uuid,
    shared: Arc<Shared>,
}

impl Scope {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            identity: Uuid::new_v4(),
            shared,
        }
    }

    fn binding(&self, kind: u8, item: &str, path: &str) -> String {
        let mut hash = Sha256::new();
        hash.update(self.identity.as_bytes());
        hash.update([kind]);
        hash.update((item.len() as u64).to_le_bytes());
        hash.update(item.as_bytes());
        hash.update(path.as_bytes());
        format!("{:x}", hash.finalize())
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let mut image = self
            .shared
            .image
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if image
            .as_ref()
            .is_some_and(|image| image.watch == self.identity)
        {
            *image = None;
        }
    }
}

#[derive(Serialize, Deserialize)]
struct CursorPage {
    binding: String,
    #[serde(flatten)]
    position: Continuation,
}

/// The source and its held prefix stay fixed across output pages. Diff
/// continuations carry the cache's revision, which it validates before reading.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Continuation {
    Output {
        source: chat::TextCopySource,
        offset: u64,
    },
    Diff {
        position: chat::FilePatchCursor,
        revision: u64,
    },
    Image {
        transfer: Uuid,
        offset: usize,
    },
}

impl Continuation {
    fn parse(cursor: Option<&str>, binding: &str) -> Result<Option<Self>, api::Error> {
        cursor
            .map(|cursor| {
                let page: CursorPage = serde_json::from_str(cursor).map_err(|_| bad_cursor())?;
                if page.binding != binding {
                    return Err(bad_cursor());
                }
                Ok(page.position)
            })
            .transpose()
    }

    fn encode(self, binding: String) -> String {
        serde_json::to_string(&CursorPage {
            binding,
            position: self,
        })
        .expect("mobile detail cursor encodes as JSON")
    }
}

pub enum Read {
    Output {
        source: chat::TextCopySource,
        offset: u64,
        binding: String,
    },
    Diff {
        key: chat::FileChangeKey,
        position: Option<chat::FilePatchCursor>,
        revision: u64,
        binding: String,
    },
}

impl Read {
    pub fn command(&self) -> chat::Command {
        match self {
            Self::Output { source, offset, .. } => chat::Command::ReadTextCopy {
                source: source.clone(),
                offset: *offset,
            },
            Self::Diff {
                key,
                position,
                revision,
                ..
            } => chat::Command::LoadFileChangePatches {
                request_id: 0,
                key: key.clone(),
                cursor: position.clone(),
                revision: *revision,
            },
        }
    }

    pub fn finish(self, response: ServerResponse) -> Result<api::DetailPage, api::Error> {
        match (self, response) {
            (
                Self::Output {
                    source,
                    offset,
                    binding,
                },
                ServerResponse::ChatTextCopy { page, .. },
            ) if page.source == source && page.offset == offset => {
                let next = (!page.done).then(|| {
                    Continuation::Output {
                        source,
                        offset: offset + page.text.len() as u64,
                    }
                    .encode(binding)
                });
                Ok(api::DetailPage {
                    next,
                    body: api::DetailBody::Output { text: page.text },
                })
            }
            (
                Self::Diff { key, binding, .. },
                ServerResponse::ChatFileChangePatches { page, .. },
            ) if page.key == key => {
                if let Some(message) = page.error {
                    return Err(error(api::ErrorCode::Unavailable, &message));
                }
                let next = page.next.map(|position| {
                    Continuation::Diff {
                        position,
                        revision: page.revision,
                    }
                    .encode(binding)
                });
                let patches = page
                    .patches
                    .into_iter()
                    .map(|patch| api::DiffPart {
                        item: patch.item_id,
                        offset: patch.offset,
                        text: patch.diff,
                    })
                    .collect();
                Ok(api::DetailPage {
                    next,
                    body: api::DetailBody::Diff { patches },
                })
            }
            _ => Err(error(
                api::ErrorCode::Internal,
                "The computer returned an unexpected transcript page.",
            )),
        }
    }
}

pub enum Page {
    Daemon(Read),
    Local(api::DetailPage),
}

/// The single image held between pages for a phone connection.
struct ImageTransfer {
    watch: Uuid,
    id: Uuid,
    receipt: Uuid,
    bytes: Vec<u8>,
    next: usize,
}

pub fn read(
    request: Request,
    snapshot: &chat::Snapshot,
    scope: &Scope,
) -> Result<Page, api::Error> {
    let (kind, id, path, cursor) = request.target();
    let binding = scope.binding(kind, id, path);
    let cursor = Continuation::parse(cursor, &binding)?;
    let (index, item) = snapshot
        .items
        .iter()
        .enumerate()
        .find(|(_, item)| item.id == id)
        .ok_or_else(|| {
            error(
                api::ErrorCode::NotFound,
                "This item is no longer in the watched transcript.",
            )
        })?;
    match (request, &item.content) {
        (
            Request::Output { .. },
            chat::ItemContent::Command { .. } | chat::ItemContent::Tool { .. },
        ) => {
            let source = snapshot
                .text_copy_sources
                .conversation
                .get(index)
                .and_then(Option::as_ref)
                .ok_or_else(|| {
                    error(
                        api::ErrorCode::Unavailable,
                        "The computer no longer has the complete saved output.",
                    )
                })?;
            let (source, offset) = match cursor {
                None => (source.clone(), 0),
                Some(Continuation::Output {
                    source: prefix,
                    offset,
                }) if prefix.cache == source.cache
                    && prefix.scope == source.scope
                    && prefix.token == source.token
                    && prefix.revision == source.revision
                    && prefix.bytes <= source.bytes
                    && offset < prefix.bytes =>
                {
                    (prefix, offset)
                }
                _ => return Err(bad_cursor()),
            };
            Ok(Page::Daemon(Read::Output {
                source,
                offset,
                binding,
            }))
        }
        (Request::Diff { path, .. }, chat::ItemContent::Changes { changes, .. })
            if changes.iter().any(|change| change.path == path) =>
        {
            let (position, revision) = match cursor {
                None => (None, 0),
                Some(Continuation::Diff { position, revision }) => (Some(position), revision),
                _ => return Err(bad_cursor()),
            };
            Ok(Page::Daemon(Read::Diff {
                key: chat::FileChangeKey {
                    source: chat::FileChangeSource::Conversation,
                    path,
                },
                position,
                revision,
                binding,
            }))
        }
        (
            Request::Image { .. },
            chat::ItemContent::Image {
                reference: Some(reference),
                ..
            },
        ) => scope
            .image_page(reference, binding, cursor)
            .map(Page::Local),
        _ => Err(error(
            api::ErrorCode::NotFound,
            "This item has no such detail.",
        )),
    }
}

impl Scope {
    fn image_page(
        &self,
        reference: &chat::ImageReference,
        binding: String,
        cursor: Option<Continuation>,
    ) -> Result<api::DetailPage, api::Error> {
        let mut transfer = match cursor {
            None => {
                // Release the previous transfer before opening a new file.
                *self.shared.image.lock().unwrap() = None;
                let bytes = chat::attachment_assets::read(reference, api::MAX_DOWNLOAD_IMAGE_BYTES)
                    .map_err(|message| error(api::ErrorCode::Unavailable, &message))?;
                ImageTransfer {
                    watch: self.identity,
                    id: Uuid::new_v4(),
                    receipt: reference.id,
                    bytes,
                    next: 0,
                }
            }
            Some(Continuation::Image { transfer, offset }) => {
                let mut held = self.shared.image.lock().unwrap();
                if !held.as_ref().is_some_and(|image| {
                    image.watch == self.identity
                        && image.id == transfer
                        && image.receipt == reference.id
                        && image.next == offset
                }) {
                    return Err(bad_cursor());
                }
                held.take().expect("contiguous image transfer")
            }
            _ => return Err(bad_cursor()),
        };
        let end = (transfer.next + IMAGE_PAGE_BYTES).min(transfer.bytes.len());
        let base64 = STANDARD.encode(&transfer.bytes[transfer.next..end]);
        let next = (end < transfer.bytes.len()).then(|| {
            transfer.next = end;
            let cursor = Continuation::Image {
                transfer: transfer.id,
                offset: end,
            }
            .encode(binding);
            *self.shared.image.lock().unwrap() = Some(transfer);
            cursor
        });
        Ok(api::DetailPage {
            next,
            body: api::DetailBody::Image { base64 },
        })
    }
}

pub fn upload(
    snapshot: &chat::Snapshot,
    text: String,
    name: String,
    encoded: &str,
    mode: api::SendMode,
) -> Result<chat::Command, api::Error> {
    if snapshot.exited || snapshot.read_only_reason.is_some() {
        return Err(error(
            api::ErrorCode::NotAllowed,
            "This conversation is read-only.",
        ));
    }
    if !snapshot.capabilities.attachments {
        return Err(error(
            api::ErrorCode::NotAllowed,
            "This conversation cannot receive images.",
        ));
    }
    if text.len() > MAX_PROMPT_BYTES || name.len() > 256 {
        return Err(error(
            api::ErrorCode::BadRequest,
            "The message or image name is too long.",
        ));
    }
    let mode = match mode {
        api::SendMode::Send => chat::PromptMode::Send,
        api::SendMode::Steer => chat::PromptMode::Steer,
        api::SendMode::Queue => chat::PromptMode::Queue,
        api::SendMode::Unknown => {
            return Err(error(
                api::ErrorCode::BadRequest,
                "The sending mode is unsupported.",
            ));
        }
    };
    let bytes = decode_upload(encoded)?;
    let attachment = chat::attachment_assets::stage_validated(
        &bytes,
        &name,
        "png",
        api::MAX_UPLOAD_IMAGE_BYTES as u64,
    )
    .map_err(|message| error(api::ErrorCode::Unavailable, &message))?;
    Ok(chat::Command::Prompt {
        text,
        attachments: vec![attachment],
        mode,
    })
}

fn decode_upload(encoded: &str) -> Result<Vec<u8>, api::Error> {
    let invalid = || {
        error(
            api::ErrorCode::BadRequest,
            "Choose a valid PNG no larger than 512 KiB and 2048 × 2048 pixels.",
        )
    };
    if encoded.len() > api::MAX_UPLOAD_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(invalid());
    }
    let bytes = STANDARD.decode(encoded).map_err(|_| invalid())?;
    if bytes.len() > api::MAX_UPLOAD_IMAGE_BYTES {
        return Err(invalid());
    }
    let mut decoder = png::Decoder::new(Cursor::new(&bytes));
    decoder.set_limits(png::Limits {
        bytes: MAX_UPLOAD_DECODE,
    });
    let header = decoder.read_header_info().map_err(|_| invalid())?;
    if header.width > MAX_UPLOAD_EDGE || header.height > MAX_UPLOAD_EDGE {
        return Err(invalid());
    }
    let mut reader = decoder.read_info().map_err(|_| invalid())?;
    if reader.info().animation_control.is_some() {
        return Err(invalid());
    }
    let size = reader
        .output_buffer_size()
        .filter(|size| *size <= MAX_UPLOAD_DECODE)
        .ok_or_else(invalid)?;
    reader
        .next_frame(&mut vec![0; size])
        .map_err(|_| invalid())?;
    reader.finish().map_err(|_| invalid())?;
    Ok(bytes)
}

fn bad_cursor() -> api::Error {
    error(
        api::ErrorCode::BadRequest,
        "This page is no longer available. Open the detail again.",
    )
}

fn error(code: api::ErrorCode, message: &str) -> api::Error {
    api::Error {
        code,
        message: message.into(),
    }
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn upload_verifies_png_bytes_and_refuses_dimensions_before_decoding_pixels() {
        let png = |width, height| {
            let mut bytes = Vec::new();
            let mut writer = png::Encoder::new(&mut bytes, width, height)
                .write_header()
                .unwrap();
            writer
                .write_image_data(&vec![0; width as usize * height as usize])
                .unwrap();
            writer.finish().unwrap();
            bytes
        };
        let valid = png(1, 1);
        assert_eq!(decode_upload(&STANDARD.encode(&valid)).unwrap(), valid);
        assert!(decode_upload(&STANDARD.encode(png(MAX_UPLOAD_EDGE + 1, 1))).is_err());
        assert!(decode_upload(&STANDARD.encode(&valid[..valid.len() / 2])).is_err());
        assert!(decode_upload(&STANDARD.encode(vec![0; api::MAX_UPLOAD_IMAGE_BYTES + 1])).is_err());
    }

    #[test]
    fn output_continuations_read_the_original_prefix_and_refuse_replacements() {
        let mut source = chat::TextCopySource {
            cache: Uuid::new_v4(),
            scope: chat::TextCopyScope::Conversation,
            token: 9,
            revision: 13,
            bytes: 20,
        };
        let prefix = source.clone();
        let scope = Scope::new(Arc::default());
        let offset = 4;
        let cursor = Continuation::Output {
            source: prefix.clone(),
            offset,
        }
        .encode(scope.binding(0, "tool", ""));
        source.bytes = 25;
        let mut snapshot = chat::Snapshot {
            items: vec![Arc::new(chat::Item {
                id: "tool".into(),
                role: chat::Role::Assistant,
                started_at: None,
                at: None,
                content: chat::ItemContent::Tool {
                    name: "test".into(),
                    input: String::new(),
                    output: String::new(),
                    status: chat::ToolStatus::Running,
                },
            })],
            ..Default::default()
        };
        snapshot.text_copy_sources.conversation = vec![Some(source.clone())];
        let request = || Request::Output {
            item: "tool".into(),
            cursor: Some(cursor.clone()),
        };
        assert!(matches!(
            read(request(), &snapshot, &scope),
            Ok(Page::Daemon(Read::Output { source, offset: 4, .. })) if source == prefix
        ));
        assert!(read(request(), &snapshot, &Scope::new(Arc::default())).is_err());
        source.revision += 1;
        snapshot.text_copy_sources.conversation = vec![Some(source)];
        assert!(read(request(), &snapshot, &scope).is_err());
    }

    #[test]
    fn diff_cursors_belong_to_the_original_item_path_and_watch() {
        let shared = Arc::new(Shared::default());
        let scope = Scope::new(shared.clone());
        let replacement = Scope::new(shared);
        let first = "src/first.rs";
        let second = "src/second.rs";
        let item = |id: &str| {
            Arc::new(chat::Item {
                id: id.into(),
                role: chat::Role::Assistant,
                at: None,
                started_at: None,
                content: chat::ItemContent::Changes {
                    status: chat::ToolStatus::Succeeded,
                    changes: [first, second]
                        .into_iter()
                        .map(|path| chat::FileChange {
                            path: path.into(),
                            kind: chat::ChangeKind::Updated,
                            diff: String::new(),
                            lines: chat::LineOrigin::File,
                        })
                        .collect(),
                },
            })
        };
        let snapshot = chat::Snapshot {
            items: vec![item("first"), item("second")],
            ..Default::default()
        };
        let cursor = Continuation::Diff {
            position: chat::FilePatchCursor {
                ordinal: 5,
                file_index: 1,
                offset: 10,
            },
            revision: 3,
        }
        .encode(scope.binding(1, "first", first));
        let request = |item: &str, path: &str| Request::Diff {
            item: item.into(),
            path: path.into(),
            cursor: Some(cursor.clone()),
        };

        assert!(matches!(
            read(request("first", first), &snapshot, &scope),
            Ok(Page::Daemon(Read::Diff { .. }))
        ));
        assert!(read(request("first", second), &snapshot, &scope).is_err());
        assert!(read(request("second", first), &snapshot, &scope).is_err());
        assert!(read(request("first", first), &snapshot, &replacement).is_err());
    }

    #[test]
    fn validates_detail_sizes_before_queueing() {
        assert!(
            Request::Output {
                item: "a".repeat(MAX_ITEM_BYTES + 1),
                cursor: None,
            }
            .validate()
            .is_err()
        );
        assert!(
            Request::Diff {
                item: "item".into(),
                path: "a".repeat(MAX_PATH_BYTES + 1),
                cursor: None,
            }
            .validate()
            .is_err()
        );
        assert!(
            Request::Image {
                item: "item".into(),
                cursor: Some("a".repeat(MAX_CURSOR_BYTES + 1)),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn images_without_owned_receipts_are_only_described() {
        let snapshot = chat::Snapshot {
            items: vec![Arc::new(chat::Item {
                id: "image".into(),
                role: chat::Role::Assistant,
                at: None,
                started_at: None,
                content: chat::ItemContent::Image {
                    path: "chat-1/image.png".into(),
                    description: "An image".into(),
                    reference: None,
                },
            })],
            ..Default::default()
        };
        let scope = Scope::new(Arc::default());
        assert!(matches!(
            read(
                Request::Image {
                    item: "image".into(),
                    cursor: None
                },
                &snapshot,
                &scope
            ),
            Err(api::Error {
                code: api::ErrorCode::NotFound,
                ..
            }),
        ));
    }
}
