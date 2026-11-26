//! On-demand access to content owned by a watched transcript.
use std::{io::Cursor, os::unix::fs::MetadataExt, path::Path};

use base64::{Engine, engine::general_purpose::STANDARD};
use cagents_mobile_api as api;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{chat, fs_safety, terminal::ipc::ServerResponse};

const IMAGE_PAGE_BYTES: usize = 192 * 1024;
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_UPLOAD_EDGE: u32 = 2048;
const MAX_UPLOAD_DECODE: usize = 32 * 1024 * 1024;

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
    fn parse(cursor: Option<&str>) -> Result<Option<Self>, api::Error> {
        cursor
            .map(|cursor| {
                if cursor.len() > 512 {
                    return Err(bad_cursor());
                }
                serde_json::from_str(cursor).map_err(|_| bad_cursor())
            })
            .transpose()
    }

    fn encode(&self) -> String {
        serde_json::to_string(self).expect("mobile detail cursor encodes as JSON")
    }
}

pub enum Read {
    Output {
        source: chat::TextCopySource,
        offset: u64,
    },
    Diff {
        key: chat::FileChangeKey,
        position: Option<chat::FilePatchCursor>,
        revision: u64,
    },
}

impl Read {
    pub fn command(&self) -> chat::Command {
        match self {
            Self::Output { source, offset } => chat::Command::ReadTextCopy {
                source: source.clone(),
                offset: *offset,
            },
            Self::Diff {
                key,
                position,
                revision,
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
            (Self::Output { source, offset }, ServerResponse::ChatTextCopy { page, .. })
                if page.source == source && page.offset == offset =>
            {
                let next = (!page.done).then(|| {
                    Continuation::Output {
                        source,
                        offset: offset + page.text.len() as u64,
                    }
                    .encode()
                });
                Ok(api::DetailPage {
                    next,
                    body: api::DetailBody::Output { text: page.text },
                })
            }
            (Self::Diff { key, .. }, ServerResponse::ChatFileChangePatches { page, .. })
                if page.key == key =>
            {
                if let Some(message) = page.error {
                    return Err(error(api::ErrorCode::Unavailable, &message));
                }
                let next = page.next.map(|position| {
                    Continuation::Diff {
                        position,
                        revision: page.revision,
                    }
                    .encode()
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

/// At most one encoded image is held on behalf of a conversation. Only the
/// next page keeps it alive, and replacing or ending the watch releases it.
pub struct ImageTransfer {
    id: Uuid,
    item: String,
    bytes: Vec<u8>,
    next: usize,
}

pub fn read(
    request: Request,
    snapshot: &chat::Snapshot,
    image: &mut Option<ImageTransfer>,
) -> Result<Page, api::Error> {
    let (id, cursor) = match &request {
        Request::Output { item, cursor }
        | Request::Diff { item, cursor, .. }
        | Request::Image { item, cursor } => (item, Continuation::parse(cursor.as_deref())?),
    };
    let (index, item) = snapshot
        .items
        .iter()
        .enumerate()
        .find(|(_, item)| &item.id == id)
        .ok_or_else(|| {
            error(
                api::ErrorCode::NotFound,
                "This item is no longer in the watched transcript.",
            )
        })?;
    let id = &item.id;
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
            Ok(Page::Daemon(Read::Output { source, offset }))
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
            }))
        }
        (
            Request::Image { .. },
            chat::ItemContent::Image {
                path, reference, ..
            },
        ) => {
            let mut transfer = match cursor {
                None => {
                    *image = None;
                    let bytes = image_bytes(path, reference.as_ref())
                        .map_err(|message| error(api::ErrorCode::Unavailable, &message))?;
                    ImageTransfer {
                        id: Uuid::new_v4(),
                        item: id.clone(),
                        bytes,
                        next: 0,
                    }
                }
                Some(Continuation::Image { transfer, offset })
                    if image.as_ref().is_some_and(|held| {
                        held.id == transfer && &held.item == id && held.next == offset
                    }) =>
                {
                    image.take().expect("contiguous image transfer")
                }
                _ => return Err(bad_cursor()),
            };
            let end = (transfer.next + IMAGE_PAGE_BYTES).min(transfer.bytes.len());
            let base64 = STANDARD.encode(&transfer.bytes[transfer.next..end]);
            let next = if end < transfer.bytes.len() {
                transfer.next = end;
                let next = Continuation::Image {
                    transfer: transfer.id,
                    offset: end,
                }
                .encode();
                *image = Some(transfer);
                Some(next)
            } else {
                None
            };
            Ok(Page::Local(api::DetailPage {
                next,
                body: api::DetailBody::Image { base64 },
            }))
        }
        _ => Err(error(
            api::ErrorCode::NotFound,
            "This item has no such detail.",
        )),
    }
}

/// Only managed receipts and content-addressed files in the private
/// transcript cache can be sent. A provider's ordinary pathname is not an
/// attachment receipt, even when its extension looks like an image.
pub fn cached_image_path(path: &str) -> Option<(&Path, &str)> {
    let path = Path::new(path);
    let (hash, extension) = path
        .file_name()?
        .to_str()?
        .strip_prefix("image-")?
        .split_once('.')?;
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return None;
    }
    let format = match extension {
        "png" => "png",
        "jpg" => "jpeg",
        "gif" => "gif",
        "webp" => "webp",
        _ => return None,
    };
    let directory = path
        .parent()?
        .file_name()?
        .to_str()?
        .strip_prefix("chat-")?;
    Uuid::parse_str(directory).ok()?;
    Some((path, format))
}

fn image_bytes(path: &str, reference: Option<&chat::ImageReference>) -> Result<Vec<u8>, String> {
    if let Some(reference) = reference {
        return chat::attachment_assets::read(reference, api::MAX_DOWNLOAD_IMAGE_BYTES);
    }
    let unavailable = || "This image is unavailable on the phone.".to_string();
    let (path, format) = cached_image_path(path).ok_or_else(unavailable)?;
    let root_path = crate::daemon::RuntimePaths::discover()
        .map_err(|_| unavailable())?
        .directory;
    read_cached_image(&root_path, path, format).ok_or_else(unavailable)
}

fn read_cached_image(root_path: &Path, path: &Path, format: &str) -> Option<Vec<u8>> {
    use std::path::Component;
    let relative = path.strip_prefix(root_path).ok()?;
    let mut components = relative.components();
    if !matches!(components.next(), Some(Component::Normal(_)))
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return None;
    }
    let root = fs_safety::Root::open(root_path).ok()?;
    let mut file = fs_safety::open_beneath(&root, relative).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != crate::daemon::current_uid()
        || meta.mode() & 0o077 != 0
        || meta.len() > api::MAX_DOWNLOAD_IMAGE_BYTES as u64
    {
        return None;
    }
    let bytes = fs_safety::read_bounded(&mut file, api::MAX_DOWNLOAD_IMAGE_BYTES as u64).ok()?;
    if bytes.len() as u64 != meta.len() || bytes.is_empty() {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(format!("data:image/{format};base64,"));
    // 3-byte alignment preserves the original base64, including final padding.
    for part in bytes.chunks(12 * 1024) {
        hash.update(STANDARD.encode(part));
    }
    let expected = format!("image-{:x}.", hash.finalize());
    path.file_name()?
        .to_str()?
        .starts_with(&expected)
        .then_some(bytes)
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
    use std::{fs, os::unix::fs::PermissionsExt, sync::Arc};

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
    fn cached_images_are_verified_and_cannot_escape_the_runtime_directory() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(format!("chat-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let data = b"private image data";
        let hash = Sha256::digest(format!("data:image/png;base64,{}", STANDARD.encode(data)));
        let path = directory.join(format!("image-{hash:x}.png"));
        fs::write(&path, data).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            cached_image_path(path.to_str().unwrap()),
            Some((path.as_path(), "png"))
        );
        assert_eq!(read_cached_image(root.path(), &path, "png").unwrap(), data);
        assert!(read_cached_image(&directory, &path, "png").is_none());

        fs::write(&path, b"different image").unwrap();
        assert!(read_cached_image(root.path(), &path, "png").is_none());
        fs::remove_file(&path).unwrap();
        let replacement = root.path().join("replacement");
        fs::write(&replacement, data).unwrap();
        std::os::unix::fs::symlink(replacement, &path).unwrap();
        assert!(read_cached_image(root.path(), &path, "png").is_none());
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
        let offset = 4;
        let cursor = Continuation::Output {
            source: prefix.clone(),
            offset,
        }
        .encode();
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
            read(request(), &snapshot, &mut None),
            Ok(Page::Daemon(Read::Output { source, offset: 4 })) if source == prefix
        ));
        source.revision += 1;
        snapshot.text_copy_sources.conversation = vec![Some(source)];
        assert!(read(request(), &snapshot, &mut None).is_err());
    }
}
