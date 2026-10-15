//! Local append provenance, fenced by each successfully installed window.
//! Replacements invalidate the proof even when followed by an append before
//! the next publication. No transcript bodies are retained here.
use super::*;
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct Changes(HashMap<String, [Option<usize>; 2]>);
impl Changes {
    pub fn clear(&mut self) { self.0.clear(); }
    pub fn remove(&mut self, id: &str) -> Option<[Option<usize>; 2]> { self.0.remove(id) }
    fn record(&mut self, id: &str, field: usize, offset: usize, previous: Option<[Option<usize>; 2]>) {
        let mut fields = previous.unwrap_or_default();
        fields[field] = Some(fields[field].map_or(offset, |old| old.min(offset)));
        self.0.insert(id.to_owned(), fields);
    }
}

fn body(content: &ItemContent, field: usize) -> Option<&str> {
    match (content, field) {
        (ItemContent::Text { text, .. }, 0) => Some(text),
        (ItemContent::Command { output, .. }, 0) => Some(output),
        (ItemContent::Tool { input, .. }, 0) => Some(input),
        (ItemContent::Tool { output, .. }, 1) => Some(output),
        _ => None,
    }
}

impl Store {
    pub(super) fn append_changed(&mut self, id: &str, field: usize, offset: usize) {
        let first_change = self.changed.as_ref().is_some_and(|changed| !changed.contains(id));
        let previous = self.appended.remove(id);
        self.changed(id);
        if self.changed.is_some() && (first_change || previous.is_some()) {
            self.appended.record(id, field, offset, previous);
        }
    }

    /// A validated append permits restoring a resident prefix from memory and
    /// asking SQLite only for its suffix. Canonical IDs still fence part ranges;
    /// missing/changed IDs and untrusted replacements use the full read path.
    pub(super) fn field_reusing(&self, id: &str, field: usize, start: usize, end: usize,
        previous: Option<&Item>) -> Result<(String, bool), String> {
        if let Some(proof) = self.appended.0.get(id)
            && let Some(old) = previous.and_then(|item| body(&item.content, field)) {
            let old_end = start.checked_add(old.len()).ok_or("Transcript prefix range overflow")?;
            let stable = match proof[field] {
                Some(offset) => old_end <= offset,
                None => old_end == end,
            };
            if stable && old_end <= end {
                let suffix = self.field(id, field as i64, old_end, end)?;
                let mut text = String::with_capacity(end - start);
                text.push_str(old);
                text.push_str(std::str::from_utf8(&suffix).map_err(|error| error.to_string())?);
                return Ok((text, true));
            }
        }
        String::from_utf8(self.field(id, field as i64, start, end)?)
            .map(|body| (body, false)).map_err(|error| error.to_string())
    }
}
