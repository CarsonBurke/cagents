//! Bounded reads of immutable operator input. Pages never infer a native
//! identity, settings, or missing text from the visible transcript.
use super::annotations::{
    ORIGINAL_PAGE_BYTES, ORIGINAL_TEXT_BYTES, OriginalError, OriginalPage, OriginalPrompt,
};

const ID_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginalText {
    pub session_id: String,
    pub original: OriginalPrompt,
    pub text: String,
}

pub(crate) fn validate_query(
    user: Option<&str>,
    session: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<(), OriginalError> {
    if limit < 4 || limit > ORIGINAL_PAGE_BYTES || offset > ORIGINAL_TEXT_BYTES {
        return Err(OriginalError::InvalidRange);
    }
    if [user, session].into_iter().flatten().any(|id| id.is_empty() || id.len() > ID_BYTES) {
        return Err(OriginalError::InvalidRange);
    }
    // `None` may resolve latest exactly once. A continuation cannot silently
    // follow a different latest user or provider conversation.
    if offset != 0 && (user.is_none() || session.is_none()) {
        return Err(OriginalError::InvalidRange);
    }
    Ok(())
}

pub(crate) fn validate_page(
    user: Option<&str>,
    session: Option<&str>,
    offset: usize,
    limit: usize,
    page: &OriginalPage,
) -> Result<(), OriginalError> {
    validate_query(user, session, offset, limit)?;
    if page.original.text_bytes > ORIGINAL_TEXT_BYTES {
        return Err(OriginalError::TooLarge);
    }
    if page.session_id.is_empty() || page.session_id.len() > ID_BYTES
        || page.original.user_id.is_empty() || page.original.user_id.len() > ID_BYTES
        || page.original.turn_id.is_empty() || page.original.turn_id.len() > ID_BYTES
        // The first lookup may use a visible row alias. The annotation store
        // resolves it to the accepted native user; later pages pin that user.
        || (offset != 0 && user.is_some_and(|id| id != page.original.user_id))
        || session.is_some_and(|id| id != page.session_id)
        || page.offset != offset || page.text.len() > limit
        || page.next_offset != offset.saturating_add(page.text.len())
        || page.next_offset > page.original.text_bytes
        || page.complete != (page.next_offset == page.original.text_bytes)
        || (!page.complete && page.next_offset == offset)
    {
        return Err(OriginalError::Changed);
    }
    if !page.original.settings.is_exact() {
        return Err(OriginalError::Unavailable("The accepted provider settings are unknown".into()));
    }
    Ok(())
}

/// Collects one original while fencing every later page to the first native
/// user, confirmed session, settings, attachments, and declared byte length.
pub(crate) struct Assembly {
    user: Option<String>,
    session: Option<String>,
    original: Option<OriginalPrompt>,
    text: String,
    complete: bool,
}

impl Assembly {
    pub(crate) fn new(user: Option<&str>, session: Option<&str>) -> Result<Self, OriginalError> {
        validate_query(user, session, 0, ORIGINAL_PAGE_BYTES)?;
        Ok(Self { user: user.map(str::to_owned), session: session.map(str::to_owned),
            original: None, text: String::new(), complete: false })
    }

    pub(crate) fn query(&self) -> (Option<&str>, Option<&str>, usize, usize) {
        (self.user.as_deref(), self.session.as_deref(), self.text.len(), ORIGINAL_PAGE_BYTES)
    }

    pub(crate) fn push(&mut self, page: OriginalPage) -> Result<bool, OriginalError> {
        if self.complete { return Err(OriginalError::Changed); }
        let (user, session, offset, limit) = self.query();
        validate_page(user, session, offset, limit, &page)?;
        if self.original.as_ref().is_some_and(|original| original != &page.original) {
            return Err(OriginalError::Changed);
        }
        if self.original.is_none() {
            self.user = Some(page.original.user_id.clone());
            self.session = Some(page.session_id.clone());
            self.original = Some(page.original);
        }
        self.text.push_str(&page.text);
        self.complete = page.complete;
        Ok(self.complete)
    }

    pub(crate) fn finish(self) -> Result<OriginalText, OriginalError> {
        if !self.complete { return Err(OriginalError::Changed); }
        Ok(OriginalText { session_id: self.session.ok_or(OriginalError::Changed)?,
            original: self.original.ok_or(OriginalError::Changed)?, text: self.text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::annotations::{OriginalSettings, Setting};

    fn page(offset: usize, text: &str, total: usize) -> OriginalPage {
        OriginalPage { session_id: "confirmed-session".into(), original: OriginalPrompt {
            user_id: "native-user".into(), turn_id: "native-turn".into(), text_bytes: total,
            attachments: vec![], settings: OriginalSettings { model: Setting::Value("native-model".into()),
                reasoning: Setting::Value("low".into()), fast: Setting::Unsupported,
                permission: Setting::Value("ask".into()) } },
            offset, next_offset: offset + text.len(), text: text.into(), complete: offset + text.len() == total }
    }

    #[test]
    fn latest_is_resolved_once_and_all_continuations_pin_native_identity() {
        let mut reader = Assembly::new(None, None).unwrap();
        assert!(!reader.push(page(0, "operator ", 13)).unwrap());
        assert_eq!(reader.query(), (Some("native-user"), Some("confirmed-session"), 9, ORIGINAL_PAGE_BYTES));
        assert!(reader.push(page(9, "🦀", 13)).unwrap());
        let original = reader.finish().unwrap();
        assert_eq!(original.text, "operator 🦀");
        assert_eq!(original.session_id, "confirmed-session");
    }

    #[test]
    fn visible_alias_resolves_once_then_continuations_pin_the_canonical_user() {
        let mut reader = Assembly::new(Some("native-user:input/0"), Some("confirmed-session")).unwrap();
        assert!(!reader.push(page(0, "first", 9)).unwrap());
        assert_eq!(reader.query(), (Some("native-user"), Some("confirmed-session"), 5, ORIGINAL_PAGE_BYTES));
        let mut wrong = page(5, "last", 9);
        wrong.original.user_id = "native-user:input/0".into();
        assert_eq!(reader.push(wrong), Err(OriginalError::Changed));
        assert!(reader.push(page(5, "last", 9)).unwrap());
        assert_eq!(reader.finish().unwrap().text, "firstlast");
    }

    #[test]
    fn changed_session_user_settings_attachment_or_range_refuses_partial_text() {
        for change in 0..7 {
            let mut reader = Assembly::new(None, None).unwrap();
            reader.push(page(0, "first", 9)).unwrap();
            let mut later = page(5, "last", 9);
            match change {
                0 => later.session_id = "replacement-session".into(),
                1 => later.original.user_id = "replacement-user".into(),
                2 => later.original.settings.model = Setting::Value("different-model".into()),
                3 => later.original.turn_id = "different-turn".into(),
                4 => later.original.attachments.push(crate::chat::annotations::OriginalAttachment {
                    original_path: "image.png".into(), path: "private-capture.png".into(), bytes: 1, sha256: [1; 32] }),
                5 => later.next_offset -= 1,
                _ => later.original.text_bytes += 1,
            }
            assert!(reader.push(later).is_err());
            assert_eq!(reader.finish(), Err(OriginalError::Changed));
        }
    }

    #[test]
    fn empty_complete_original_is_valid_but_stalled_or_unbounded_reads_are_not() {
        let mut reader = Assembly::new(None, None).unwrap();
        assert!(reader.push(page(0, "", 0)).unwrap());
        assert!(reader.finish().unwrap().text.is_empty());
        assert_eq!(validate_query(None, None, 1, 1), Err(OriginalError::InvalidRange));
        assert_eq!(validate_query(Some("u"), Some("s"), 1, 0), Err(OriginalError::InvalidRange));
        assert_eq!(validate_page(None, None, 0, 1, &page(0, "", 1)), Err(OriginalError::Changed));
        assert_eq!(validate_page(None, None, 0, 1, &page(0, "x", ORIGINAL_TEXT_BYTES + 1)), Err(OriginalError::TooLarge));
        let mut unknown = page(0, "x", 1);
        unknown.original.settings.model = Setting::Unknown;
        assert!(matches!(validate_page(None, None, 0, 1, &unknown), Err(OriginalError::Unavailable(_))));
    }

    #[test]
    fn original_requests_and_qualified_errors_round_trip_on_the_matched_wire() {
        use crate::{chat::Command, model::TerminalId, terminal_ipc::{ClientRequest, ServerResponse}};
        let chat_id = TerminalId::new();
        let request = ClientRequest::ChatCommand { chat_id, command: Command::LoadOriginal {
            user_id: Some("native-user".into()), expected_session: Some("confirmed-session".into()),
            offset: 7, limit: ORIGINAL_PAGE_BYTES,
        } };
        assert_eq!(postcard::from_bytes::<ClientRequest>(&postcard::to_stdvec(&request).unwrap()).unwrap(), request);
        for result in [Ok(page(0, "text", 4)), Err(OriginalError::Unavailable("No provider acceptance proof".into())),
            Err(OriginalError::InvalidRange), Err(OriginalError::TooLarge), Err(OriginalError::Changed),
            Err(OriginalError::Transport("The daemon changed".into()))] {
            let reply = ServerResponse::ChatOriginal { chat_id, result };
            assert_eq!(postcard::from_bytes::<ServerResponse>(&postcard::to_stdvec(&reply).unwrap()).unwrap(), reply);
        }
    }
}
