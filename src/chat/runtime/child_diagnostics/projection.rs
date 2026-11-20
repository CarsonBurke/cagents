//! Bounded resident previews of exact child provider failures.
use crate::{
    agent::Provider,
    chat::{Item, ItemContent},
};

use crate::chat::failure::PREVIEW_OMITTED as OMITTED;
// Match Failure::parse's complete classification prefix even for four-byte
// Unicode scalars, while staying below the child resident item budget.
const PREVIEW_CHARS: usize = 8192;
pub(super) const PREVIEW_BYTES: usize = PREVIEW_CHARS * 4 + OMITTED.len();

pub(crate) fn marked(item: &Item) -> bool {
    crate::chat::failure::provider_error_text(Provider::Codex, item).is_some()
}

pub(crate) fn project(item: &mut Item) {
    if !marked(item) {
        return;
    }
    if let ItemContent::Delivery { body, .. } = &mut item.content {
        if let Some((end, _)) = body.char_indices().nth(PREVIEW_CHARS) {
            let mut preview = String::with_capacity(end + OMITTED.len());
            preview.push_str(&body[..end]);
            preview.push_str(OMITTED);
            // Truncation alone retains the potentially refused 32-MiB frame
            // allocation. Replace its owner before entering resident state.
            *body = preview;
        } else if body.capacity() > PREVIEW_BYTES {
            *body = body.as_str().to_owned();
        }
    }
}
