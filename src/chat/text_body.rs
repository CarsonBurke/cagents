//! Shared prose bytes with the same serialization and mutation semantics as String.
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    ops::{Deref, DerefMut},
    sync::Arc,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TextBody(Arc<String>);

impl TextBody {
    pub fn into_owned(self) -> String {
        Arc::unwrap_or_clone(self.0)
    }

    pub(crate) fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl From<String> for TextBody {
    fn from(text: String) -> Self {
        Self(Arc::new(text))
    }
}

impl From<&str> for TextBody {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

impl From<&String> for TextBody {
    fn from(text: &String) -> Self {
        text.clone().into()
    }
}

impl Deref for TextBody {
    type Target = String;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for TextBody {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.0)
    }
}

impl AsRef<str> for TextBody {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<T: AsRef<str> + ?Sized> PartialEq<T> for TextBody {
    fn eq(&self, other: &T) -> bool {
        self.as_str() == other.as_ref()
    }
}

impl Eq for TextBody {}

impl PartialEq<TextBody> for String {
    fn eq(&self, other: &TextBody) -> bool {
        self.as_str() == other.as_str()
    }
}
impl PartialEq<TextBody> for str {
    fn eq(&self, other: &TextBody) -> bool {
        self == other.as_str()
    }
}

impl fmt::Display for TextBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_shares_bytes_until_an_owned_mutation() {
        let original = TextBody::from("immutable λ");
        let mut next = original.clone();
        assert!(original.shares_storage_with(&next));
        next.push_str(" tail");
        assert!(!original.shares_storage_with(&next));
        assert_eq!(original, "immutable λ");
        assert_eq!(next, "immutable λ tail");
        assert_eq!(next.into_owned(), "immutable λ tail");
    }

    #[test]
    fn shared_prose_preserves_current_string_json_and_postcard_bytes() {
        for text in [
            String::new(),
            "λ\n\"quoted\"".into(),
            "body".repeat(64 * 1024),
        ] {
            let shared = TextBody::from(text.clone());
            assert_eq!(
                serde_json::to_vec(&shared).unwrap(),
                serde_json::to_vec(&text).unwrap()
            );
            assert_eq!(
                postcard::to_allocvec(&shared).unwrap(),
                postcard::to_allocvec(&text).unwrap()
            );
            let from_current: TextBody =
                postcard::from_bytes(&postcard::to_allocvec(&text).unwrap()).unwrap();
            let to_current: String =
                postcard::from_bytes(&postcard::to_allocvec(&shared).unwrap()).unwrap();
            assert_eq!(from_current, text);
            assert_eq!(to_current, text);
        }
    }
    #[test]
    fn shared_text_item_preserves_current_item_encoding_and_decoding() {
        use crate::chat::{Item, ItemContent, Phase, Role};
        #[derive(Serialize, Deserialize)]
        enum ExistingContent {
            Text {
                text: String,
                thinking: bool,
                phase: Phase,
            },
        }
        #[derive(Serialize, Deserialize)]
        struct ExistingItem {
            started_at: Option<u64>,
            id: String,
            role: Role,
            content: ExistingContent,
            at: Option<u64>,
        }
        let before = ExistingItem {
            started_at: Some(3),
            id: "opaque:native-part:0".into(),
            role: Role::Assistant,
            content: ExistingContent::Text {
                text: "λ".repeat(128 * 1024),
                thinking: true,
                phase: Phase::Commentary,
            },
            at: Some(7),
        };
        let current = Item {
            started_at: before.started_at,
            id: before.id.clone(),
            role: before.role,
            content: ItemContent::Text {
                text: "λ".repeat(128 * 1024).into(),
                thinking: true,
                phase: Phase::Commentary,
            },
            at: before.at,
        };
        let old_postcard = postcard::to_stdvec(&before).unwrap();
        let current_postcard = postcard::to_stdvec(&current).unwrap();
        assert_eq!(current_postcard, old_postcard);
        assert_eq!(
            serde_json::to_vec(&current).unwrap(),
            serde_json::to_vec(&before).unwrap()
        );
        let decoded: Item = postcard::from_bytes(&old_postcard).unwrap();
        assert_eq!(decoded, current);
        let existing: ExistingItem = postcard::from_bytes(&current_postcard).unwrap();
        assert_eq!(existing.at, before.at);
        assert_eq!(existing.started_at, before.started_at);
        assert!(
            matches!(existing.content, ExistingContent::Text { text, thinking: true, phase: Phase::Commentary } if text == "λ".repeat(128 * 1024))
        );
    }
}
