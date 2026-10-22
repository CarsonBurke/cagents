//! Cagents references travel as ordinary text beside real provider image
//! inputs. They are never invented provider-native IDs. Only an exact owned
//! UUID receipt matching the adjacent real image can recover this metadata.
use crate::chat::{Attachment, ImageReference};

const OPEN: &str = "\n<cagents-image-reference>";
const CLOSE: &str = "</cagents-image-reference>";
const MAX_MARKER_BYTES: usize = 4096;

pub(super) fn marker(reference: &ImageReference) -> Result<String, String> {
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

/// A reserved structured mapping claim is never permission to downgrade to
/// a legacy filesystem path when its receipt/schema cannot be verified. Plain
/// prose labels have no such claim and keep their ordinary meaning.
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

/// Literal `[Image N]` prose is never enough to look up an attachment. Even a
/// canonical full marker is only a candidate until its UUID receipt and actual
/// image path/bytes agree on this device.
pub(super) fn restore(text: &str, source: &str) -> Option<ImageReference> {
    let reference = candidate(text)?;
    crate::chat::attachment_assets::restore_reference(&reference, source)
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

const LIST_OPEN: &str = "\n<cagents-image-inputs>";
const LIST_CLOSE: &str = "</cagents-image-inputs>";
const MAX_IMAGE_LIST: usize = 128;

pub(super) fn check_image_count(count: usize) -> Result<(), String> {
    if count > MAX_IMAGE_LIST {
        Err("Too many image mappings (maximum 128 images).".into())
    } else {
        Ok(())
    }
}

/// OMP accepts a single ordinary message string and a separate ordered image
/// array. This suffix describes that actual array; it adds no native ID field.
pub(super) fn image_list(references: &[Option<ImageReference>]) -> Result<String, String> {
    check_image_count(references.len())?;
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

/// Recover only an exact canonical suffix whose complete ordered real image
/// array verifies. Otherwise preserve the provider text verbatim.
pub(super) fn restore_list<'a>(
    text: &'a str,
    sources: &[String],
) -> Option<(&'a str, Vec<Option<ImageReference>>)> {
    let (_, json) = text.rsplit_once(LIST_OPEN)?;
    if json.len() > MAX_MARKER_BYTES * MAX_IMAGE_LIST {
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
                || crate::chat::attachment_assets::restore_reference(reference, source).is_none()
            {
                return None;
            }
        }
    }
    Some((plain, references))
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
    fn canonical_image_mapping_is_not_a_display_label_or_arbitrary_json() {
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
        assert!(candidate(&"x".repeat(MAX_MARKER_BYTES + 1)).is_none());
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
