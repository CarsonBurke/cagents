//! The model reads one caption beside each managed image: its `[Image N]`
//! label. Claude Code and Codex already tell the model where each image is
//! stored, so the caption adds only the label the user's text refers to. It
//! carries no bookkeeping: cagents recovers the attachment from its own
//! receipt instead. The label names one receipt on this device, and the
//! adjacent image must carry that receipt's exact bytes or be its stored file.
//! Typed text therefore cannot claim an image, and another device's label
//! finds no receipt whose pixels match.
//!
//! Transcripts written before captions carry a JSON
//! `<cagents-image-reference>` marker per image, and OMP a
//! `<cagents-image-inputs>` list; [`legacy`] still reads both.
use crate::chat::{Attachment, ImageReference};
use std::path::Path;

/// What the model reads beside `reference`'s image.
pub(super) fn caption(reference: &ImageReference) -> Result<String, String> {
    reference.validate()?;
    Ok(reference.token())
}

/// The number of a label exactly as [`ImageReference::token`] writes it.
/// Only a canonical number counts, so `[Image 07]` is ordinary prose.
fn label_number(text: &str) -> Option<u64> {
    let digits = text.strip_prefix("[Image ")?.strip_suffix(']')?;
    let number: u64 = digits.parse().ok()?;
    (number != 0 && number.to_string() == digits).then_some(number)
}

/// The receipt number `text` claims for the adjacent image at `source`. A
/// bare label claims only inline pixels or a stored image's file, so a
/// message that merely says `[Image 3]` leaves an ordinary file beside it
/// alone.
fn caption_number(text: &str, source: &str) -> Option<u64> {
    label_number(text).filter(|_| {
        source.starts_with("data:image/")
            || crate::chat::attachment_assets::is_stored_image_path(Path::new(source))
    })
}

/// Whether `text`, directly before the image at `source`, is cagents'
/// caption for it (current or legacy). A claim that cannot be verified is
/// never permission to read a coincident local path in its place.
pub(super) fn claims(text: &str, source: &str) -> bool {
    caption_number(text, source).is_some() || legacy::claims_reference(text)
}

/// The label a claim names, for saying that its image is unavailable.
pub(super) fn claimed_label(text: &str, source: &str) -> String {
    caption_number(text, source)
        .map(|number| format!("Image {number}"))
        .or_else(|| legacy::candidate(text).map(|reference| reference.label()))
        .unwrap_or_else(|| "Image reference".into())
}

/// The attachment the caption `text` and its adjacent image at `source`
/// identify, once this device's receipt and the image's bytes agree.
pub(super) fn restore(text: &str, source: &str) -> Option<ImageReference> {
    match caption_number(text, source) {
        Some(number) => crate::chat::attachment_assets::restore_number(number, source),
        None => legacy::restore(text, source),
    }
}

/// Provider image order follows the first inline mention. Legacy attachments
/// without references retain their relative order at the end. Never reassign
/// a display number or manufacture an identity while sorting.
pub(super) fn ordered<'a>(
    text: &str,
    attachments: &'a [Attachment],
) -> Result<Vec<&'a Attachment>, String> {
    let mut ids = std::collections::HashSet::new();
    let mut labels = std::collections::HashSet::new();
    for attachment in attachments {
        if let Some(reference) = &attachment.reference {
            reference.validate()?;
            if !ids.insert(reference.id) || !labels.insert((reference.device, reference.number)) {
                return Err(
                    "The prompt repeats an image reference; remove the duplicate attachment".into(),
                );
            }
        }
    }
    let managed = attachments
        .iter()
        .filter_map(|attachment| attachment.reference.as_ref())
        .collect::<Vec<_>>();
    if !managed.is_empty() && !text.trim().is_empty() {
        for reference in &managed {
            if !text.contains(&reference.token()) {
                return Err(format!(
                    "{} is attached but its inline reference is missing. Insert {} or remove the attachment.",
                    reference.label(),
                    reference.token()
                ));
            }
        }
        // Other labels may refer to earlier conversation images or quoted
        // prose. Text alone neither selects an image nor grants authority to
        // resolve one; only the current attachment metadata is validated.
    }
    let mut ordered = attachments.iter().enumerate().collect::<Vec<_>>();
    ordered.sort_by_key(|(index, attachment)| {
        (
            attachment
                .reference
                .as_ref()
                .and_then(|reference| text.find(&reference.token()))
                .unwrap_or(usize::MAX),
            *index,
        )
    });
    Ok(ordered
        .into_iter()
        .map(|(_, attachment)| attachment)
        .collect())
}

const MAX_IMAGE_LIST: usize = 128;
const LIST_HEADING: &str = "\nImage inputs (in order):";
const UNLABELED: &str = "Unlabeled image";

pub(super) fn check_image_count(count: usize) -> Result<(), String> {
    if count > MAX_IMAGE_LIST {
        Err("Too many image mappings (maximum 128 images).".into())
    } else {
        Ok(())
    }
}

fn printable(path: &Path) -> Option<&str> {
    path.to_str()
        .filter(|path| !path.contains(char::is_control))
}

/// OMP accepts a single ordinary message string and a separate ordered image
/// array, and shows the model no path for those pixels. This suffix captions
/// that actual array, one line per image, with each image's file path.
pub(super) fn image_list(images: &[(Option<&ImageReference>, &Path)]) -> Result<String, String> {
    check_image_count(images.len())?;
    let mut list = LIST_HEADING.to_owned();
    for (reference, path) in images {
        list.push('\n');
        match reference {
            Some(reference) => list.push_str(&caption(reference)?),
            None => list.push_str(UNLABELED),
        }
        if let Some(path) = printable(path) {
            list.push_str(": ");
            list.push_str(path);
        }
    }
    Ok(list)
}

/// Recover only a list whose every labeled line verifies against the image
/// at its position. Otherwise preserve the provider text verbatim.
pub(super) fn restore_list<'a>(
    text: &'a str,
    sources: &[String],
) -> Option<(&'a str, Vec<Option<ImageReference>>)> {
    if let Some(restored) = legacy::restore_list(text, sources) {
        return Some(restored);
    }
    if sources.len() > MAX_IMAGE_LIST {
        return None;
    }
    let (plain, list) = text.rsplit_once(LIST_HEADING)?;
    let lines = list.strip_prefix('\n')?.split('\n').collect::<Vec<_>>();
    if lines.len() != sources.len() {
        return None;
    }
    let mut labeled = Vec::new();
    let mut references = Vec::with_capacity(lines.len());
    for (line, source) in lines.into_iter().zip(sources) {
        if line == UNLABELED || line.starts_with("Unlabeled image: ") {
            references.push(None);
            continue;
        }
        let label = line.split_once(": ").map_or(line, |(label, _)| label);
        labeled.push((label_number(label)?, source.as_str()));
        references.push(Some(labeled.len() - 1));
    }
    if labeled.is_empty() {
        return None;
    }
    let restored = crate::chat::attachment_assets::restore_numbers(labeled)?;
    let mut ids = std::collections::HashSet::new();
    if !restored.iter().all(|reference| ids.insert(reference.id)) {
        return None;
    }
    let references = references
        .into_iter()
        .map(|index| index.map(|index| restored[index].clone()))
        .collect();
    Some((plain, references))
}

/// The JSON markers cagents wrote before captions. They are read, never
/// written: their receipt UUID still has to match the adjacent real image.
pub(super) mod legacy {
    use crate::chat::ImageReference;

    const OPEN: &str = "\n<cagents-image-reference>";
    const CLOSE: &str = "</cagents-image-reference>";
    const MAX_MARKER_BYTES: usize = 4096;
    const LIST_OPEN: &str = "\n<cagents-image-inputs>";
    const LIST_CLOSE: &str = "</cagents-image-inputs>";

    pub(in crate::chat::protocol) fn marker(reference: &ImageReference) -> Result<String, String> {
        reference.validate()?;
        let marker = format!(
            "{}{OPEN}{}{CLOSE}",
            reference.token(),
            serde_json::to_string(reference).map_err(|e| e.to_string())?
        );
        if marker.len() > MAX_MARKER_BYTES {
            return Err("The image reference exceeds its mapping limit".into());
        }
        Ok(marker)
    }

    /// A reserved structured mapping claim is never permission to downgrade
    /// to a legacy filesystem path when its receipt/schema cannot be
    /// verified. Plain prose labels have no such claim.
    pub(super) fn claims_reference(text: &str) -> bool {
        let Some((label, _)) = text.split_once(OPEN) else {
            return false;
        };
        let Some(number) = label
            .strip_prefix("[Image ")
            .and_then(|s| s.strip_suffix(']'))
        else {
            return false;
        };
        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) && text.ends_with(CLOSE)
    }

    pub(super) fn candidate(text: &str) -> Option<ImageReference> {
        if text.len() > MAX_MARKER_BYTES {
            return None;
        }
        let (_, json) = text.split_once(OPEN)?;
        let reference: ImageReference = serde_json::from_str(json.strip_suffix(CLOSE)?).ok()?;
        (marker(&reference).ok()?.as_str() == text).then_some(reference)
    }

    pub(super) fn restore(text: &str, source: &str) -> Option<ImageReference> {
        let reference = candidate(text)?;
        crate::chat::attachment_assets::restore_reference(&reference, source)
    }

    pub(in crate::chat::protocol) fn image_list(
        references: &[Option<ImageReference>],
    ) -> Result<String, String> {
        super::check_image_count(references.len())?;
        for reference in references.iter().flatten() {
            reference.validate()?;
        }
        let labels = references
            .iter()
            .map(|reference| {
                reference
                    .as_ref()
                    .map(|r| r.token())
                    .unwrap_or_else(|| "legacy image".into())
            })
            .collect::<Vec<_>>()
            .join(", ");
        Ok(format!(
            "\nImage inputs (in order): {labels}{LIST_OPEN}{}{LIST_CLOSE}",
            serde_json::to_string(references).map_err(|e| e.to_string())?
        ))
    }

    pub(super) fn restore_list<'a>(
        text: &'a str,
        sources: &[String],
    ) -> Option<(&'a str, Vec<Option<ImageReference>>)> {
        let (_, json) = text.rsplit_once(LIST_OPEN)?;
        if json.len() > MAX_MARKER_BYTES * super::MAX_IMAGE_LIST {
            return None;
        }
        let references: Vec<Option<ImageReference>> =
            serde_json::from_str(json.strip_suffix(LIST_CLOSE)?).ok()?;
        if references.len() != sources.len() || !references.iter().any(Option::is_some) {
            return None;
        }
        let suffix = image_list(&references).ok()?;
        let plain = text.strip_suffix(&suffix)?;
        let mut ids = std::collections::HashSet::new();
        for (reference, source) in references.iter().zip(sources) {
            if let Some(reference) = reference {
                if !ids.insert(reference.id)
                    || crate::chat::attachment_assets::restore_reference(reference, source)
                        .is_none()
                {
                    return None;
                }
            }
        }
        Some((plain, references))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(number: u64) -> ImageReference {
        ImageReference {
            id: uuid::Uuid::new_v4(),
            number,
            device: uuid::Uuid::from_u128(7),
            name: "literal <file> image.png".into(),
            extension: "png".into(),
            sha256: "a".repeat(64),
        }
    }

    #[test]
    fn legacy_mapping_is_not_a_display_label_or_arbitrary_json() {
        use legacy::*;
        let reference = reference(13);
        let marker = marker(&reference).unwrap();
        assert_eq!(candidate(&marker), Some(reference.clone()));
        assert!(candidate(&reference.token()).is_none());
        assert!(!claims_reference(&reference.token()));
        assert!(claims_reference(&marker));
        assert!(claims_reference(
            &marker.replace("{\"id\"", "{\"future\":true,\"id\"")
        ));
        assert!(candidate(&marker.replace("{\"id\"", "{\"future\":true,\"id\"")).is_none());
        assert!(candidate(&marker.replace("[Image 13]", "[Image 12]")).is_none());
        assert!(candidate(&marker.replace("{\"id\"", "{ \"id\"")).is_none());
        assert!(candidate(&format!("prefix {marker}")).is_none());
        assert!(candidate(&"x".repeat(4097)).is_none());
    }

    #[test]
    fn a_caption_is_the_label_alone() {
        let reference = reference(13);
        assert_eq!(caption(&reference).unwrap(), "[Image 13]");
        assert_eq!(label_number("[Image 13]"), Some(13));
        for prose in [
            "[Image 013]",
            "[Image 0]",
            "[Image 13] ",
            "see [Image 13]",
            "[Image 13]: /path.png",
            "[Image -1]",
            "[Image ]",
        ] {
            assert_eq!(label_number(prose), None, "{prose:?}");
            assert!(!claims(prose, "data:image/png;base64,"), "{prose:?}");
        }
        let stored = format!("/data/chat-image-assets/{}.png", reference.id);
        assert!(claims("[Image 13]", "data:image/png;base64,"));
        assert!(claims("[Image 13]", &stored));
        assert!(
            !claims("[Image 13]", "/home/me/shot.png"),
            "a message saying [Image 13] beside an ordinary file is prose"
        );
        assert!(!claims(
            "[Image 13]",
            &format!("/elsewhere/{}.png", reference.id)
        ));
        assert!(claims(
            &legacy::marker(&reference).unwrap(),
            "/home/me/shot.png"
        ));
        assert_eq!(claimed_label("[Image 13]", &stored), "Image 13");
    }

    #[test]
    fn an_image_list_names_each_input_once() {
        let first = reference(3);
        let second = reference(7);
        let list = image_list(&[
            (Some(&first), Path::new("/store/a.png")),
            (None, Path::new("/home/me/shot.png")),
            (Some(&second), Path::new("/store/b.png")),
        ])
        .unwrap();
        assert_eq!(
            list,
            "\nImage inputs (in order):\n[Image 3]: /store/a.png\nUnlabeled image: /home/me/shot.png\n[Image 7]: /store/b.png"
        );
        assert!(!list.contains(&first.id.to_string()) && !list.contains(&first.sha256));
    }

    #[test]
    fn caret_order_does_not_renumber_or_swap_attachment_identity() {
        let first = Attachment {
            path: "/first.png".into(),
            reference: Some(reference(13)),
        };
        let second = Attachment {
            path: "/second.png".into(),
            reference: Some(reference(29)),
        };
        let attachments = vec![
            first.clone(),
            Attachment::file("/legacy.png"),
            second.clone(),
        ];
        let order = ordered("Compare [Image 29] with [Image 13]", &attachments).unwrap();
        assert_eq!(order, [&second, &first, &attachments[1]]);
        assert_eq!(attachments[0].reference.as_ref().unwrap().number, 13);
        assert!(ordered("[Image 13]", &[first.clone(), first.clone()]).is_err());
        assert!(
            ordered("missing reference", &[first.clone()])
                .unwrap_err()
                .contains("inline reference is missing")
        );
        let current = [second.clone()];
        let mixed = ordered(
            "Earlier [Image 13] was dark; compare new [Image 29]",
            &current,
        )
        .unwrap();
        assert_eq!(
            mixed,
            [&second],
            "a history label never adds a current attachment"
        );
        assert!(ordered("[Image 13] then literal [Image 999]", &[first.clone()]).is_ok());
        assert!(ordered("", &[first]).is_ok());
        assert!(ordered("literal [Image 999]", &[Attachment::file("/legacy.png")]).is_ok());
    }
}
