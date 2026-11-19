//! A cagents prompt as the content blocks of `session/prompt`.
//!
//! Text always goes as text. An image goes inline, when the agent's
//! `promptCapabilities` say it reads them, after the text, which then ends
//! with the same ordered image list OMP is given, so a replayed history can
//! tell which receipt each image was. Any other file goes as a
//! `resource_link`, which every ACP agent must accept; the agent reads it
//! with its own tools.
use super::super::image_references;
use crate::chat::{Attachment, Item, ItemContent, Phase, Role};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::io::Read;

pub(super) struct Prompt {
    pub blocks: Vec<Value>,
    /// The user's own message as the transcript shows it.
    pub items: Vec<Item>,
}

pub(super) fn build(
    text: &str,
    attachments: &[Attachment],
    images_accepted: bool,
    max_image_bytes: u64,
    label: &str,
    next_id: &mut dyn FnMut() -> String,
) -> Result<Prompt, String> {
    let ordered = image_references::ordered(text, attachments)?;
    image_references::check_image_count(
        ordered
            .iter()
            .filter(|attachment| attachment.reference.is_some())
            .count(),
    )?;
    let mut message = text.to_owned();
    let mut shown = text.to_owned();
    let mut images = Vec::new();
    let mut image_items = Vec::new();
    let mut listed = Vec::new();
    let mut links = Vec::new();
    for attachment in ordered {
        let owned = crate::chat::attachment_assets::resolve(attachment)?;
        let path = owned
            .path
            .canonicalize()
            .map_err(|error| format!("Cannot attach {}: {error}", owned.path.display()))?;
        let printable = path
            .to_str()
            .ok_or_else(|| format!("Cannot attach {}: its path is not text", path.display()))?
            .to_owned();
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mime = match extension.as_str() {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            _ => None,
        };
        match mime {
            Some(mime) => {
                if !images_accepted {
                    return Err(format!(
                        "{label} does not accept images in a prompt; remove {}.",
                        attachment.label()
                    ));
                }
                let mut bytes = Vec::new();
                std::fs::File::open(&path)
                    .map_err(|error| format!("Cannot attach {}: {error}", path.display()))?
                    .take(max_image_bytes.saturating_add(1))
                    .read_to_end(&mut bytes)
                    .map_err(|error| format!("Cannot attach {}: {error}", path.display()))?;
                if bytes.len() as u64 > max_image_bytes {
                    return Err(format!(
                        "{} is larger than {label} accepts.",
                        attachment.label()
                    ));
                }
                images.push(
                    json!({"type": "image", "mimeType": mime, "data": STANDARD.encode(&bytes)}),
                );
                image_items.push(Item {
                    started_at: None,
                    id: next_id(),
                    role: Role::User,
                    content: ItemContent::Image {
                        path: printable.clone(),
                        description: attachment.label(),
                        reference: attachment.reference.clone(),
                    },
                    at: None,
                });
                listed.push((attachment.reference.as_ref(), path));
            }
            None => {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(&printable)
                    .to_owned();
                shown.push_str("\nAttached file: ");
                shown.push_str(&printable);
                links.push(json!({
                    "type": "resource_link",
                    "uri": file_uri(&printable),
                    "name": name,
                }));
            }
        }
    }
    if !listed.is_empty() {
        let listed = listed
            .iter()
            .map(|(reference, path)| (*reference, path.as_path()))
            .collect::<Vec<_>>();
        message.push_str(&image_references::image_list(&listed)?);
    }
    let mut blocks = Vec::with_capacity(1 + images.len() + links.len());
    if !message.is_empty() {
        blocks.push(json!({"type": "text", "text": message}));
    }
    blocks.extend(images);
    blocks.extend(links);
    if blocks.is_empty() {
        return Err("Write a message or attach something to send.".into());
    }
    for attachment in attachments {
        crate::chat::attachment_assets::retain(attachment)?;
    }
    let mut items = Vec::with_capacity(1 + image_items.len());
    if !shown.is_empty() {
        items.push(Item {
            started_at: None,
            id: next_id(),
            role: Role::User,
            content: ItemContent::Text {
                text: shown,
                thinking: false,
                phase: Phase::Final,
            },
            at: None,
        });
    }
    items.extend(image_items);
    Ok(Prompt { blocks, items })
}

/// A `file://` URI for an absolute path, with the characters a URI reserves
/// escaped.
fn file_uri(path: &str) -> String {
    let mut uri = String::from("file://");
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// The path a `file://` URI names, for showing a replayed attachment.
pub(super) fn uri_path(uri: &str) -> Option<String> {
    let encoded = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut rest = encoded.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%'
            && let Some(hex) = tail.get(..2)
            && let Ok(value) = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16)
        {
            bytes.push(value);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_file_uri_round_trips_reserved_characters() {
        let path = "/tmp/a dir/100% done#1.txt";
        let uri = super::file_uri(path);
        assert_eq!(uri, "file:///tmp/a%20dir/100%25%20done%231.txt");
        assert_eq!(super::uri_path(&uri).as_deref(), Some(path));
    }
}
