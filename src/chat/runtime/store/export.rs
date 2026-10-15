//! Bounded transcript reads with a cursor independent of the UI's paging state.
use super::*;
use crate::agent_execution::{TranscriptCursor, TranscriptPage, TranscriptPart};

impl Store {
    pub(crate) fn export(&mut self, after: Option<TranscriptCursor>) -> Result<TranscriptPage, String> {
        self.flush_text()?;
        if after.as_ref().is_some_and(|cursor| cursor.offset > i64::MAX as usize) {
            return Err("Invalid transcript cursor offset".into());
        }
        // History insertion can rebalance every ordinal. The provider item
        // identity is stable across that transaction; resolve it afresh.
        let after = after.map(|mut cursor| {
            cursor.ordinal = self.ordinal(&cursor.id)?.ok_or("Transcript anchor disappeared")?;
            Ok::<_, String>(cursor)
        }).transpose()?;
        let records = {
            let mut query = self.db.prepare("SELECT id,ordinal,metadata,len0,len1,len2,spans FROM items WHERE ordinal>=?1 ORDER BY ordinal LIMIT 17").map_err(|e| e.to_string())?;
            query.query_map([after.as_ref().map_or(i64::MIN, |cursor| cursor.ordinal)], |row| Ok((
                row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, Vec<u8>>(2)?,
                row.get::<_, i64>(3)? as usize, row.get::<_, i64>(4)? as usize,
                row.get::<_, i64>(5)? as usize, row.get::<_, Vec<u8>>(6)?,
            ))).map_err(|e| e.to_string())?.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?
        };
        let mut parts = Vec::new();
        let mut next = after.clone();
        'records: for (id, ordinal, metadata, len0, len1, len2, spans) in records {
            let base: Item = postcard::from_bytes(&metadata).map_err(|e| e.to_string())?;
            let spans: Vec<usize> = postcard::from_bytes(&spans).map_err(|e| e.to_string())?;
            let total = len0.checked_add(len1).ok_or("Transcript item length overflow")?;
            let current = after.as_ref().filter(|cursor| cursor.ordinal == ordinal);
            let mut position = current.map_or(0, |cursor| cursor.offset.min(total));
            let mut empty_emitted = current.is_some_and(|cursor| cursor.emitted_empty);
            while position < total || (total == 0 && !empty_emitted) {
                if parts.len() == 16 { break 'records; }
                let start = position.min(total);
                let target = start.saturating_add(PART_BYTES).min(total);
                let end = if target == total { total } else { self.boundary(&id, len0, target)? };
                if end <= start && total > 0 { return Err("Transcript cursor did not advance".into()); }
                let (start0, end0) = (start.min(len0), end.min(len0));
                let (start1, end1) = (start.saturating_sub(len0), end.saturating_sub(len0));
                let body0 = String::from_utf8(self.field(&id, 0, start0, end0)?).map_err(|e| e.to_string())?;
                let body1 = String::from_utf8(self.field(&id, 1, start1, end1)?).map_err(|e| e.to_string())?;
                let mut item = base.clone();
                match &mut item.content {
                    ItemContent::Text { text, .. } => *text = body0,
                    ItemContent::Delivery { body, .. } => *body = body0,
                    ItemContent::Command { output, .. } => *output = body0,
                    ItemContent::Tool { input, output, .. } => { *input = body0; *output = body1; }
                    ItemContent::Changes { changes, .. } => {
                        let mut from = 0;
                        let mut lengths = spans.iter();
                        changes.retain_mut(|change| {
                            let length = lengths.next().copied().unwrap_or_default();
                            let (begin, finish) = (from, from + length); from = finish;
                            let within = if length == 0 { (start0..end0).contains(&begin) || (begin == len0 && end0 == len0) }
                                else { begin < end0 && finish > start0 };
                            if within { change.diff = body0[begin.max(start0) - start0..finish.min(end0) - start0].into(); }
                            within
                        });
                    }
                    ItemContent::Image { path, description } => {
                        *description = body0;
                        if len2 <= PART_BYTES { *path = String::from_utf8(self.field(&id, 2, 0, len2)?).map_err(|e| e.to_string())?; }
                    }
                    ItemContent::WebSearch { .. } => {}
                }
                position = end;
                empty_emitted = total == 0;
                next = Some(TranscriptCursor { id: id.clone(), ordinal, offset: position, emitted_empty: empty_emitted });
                parts.push(TranscriptPart { item, offset: start, complete: end == total,
                    attachment_omitted: len2 > PART_BYTES });
            }
        }
        let at_end = match next.as_ref() {
            Some(cursor) => !self.db.query_row("SELECT EXISTS(SELECT 1 FROM items WHERE ordinal>?1 OR (ordinal=?1 AND (len0+len1>?2 OR (len0+len1=0 AND ?3=0))))", params![cursor.ordinal, cursor.offset as i64, cursor.emitted_empty], |row| row.get::<_, bool>(0)).map_err(|e| e.to_string())?,
            None => true,
        };
        Ok(TranscriptPage { parts, next, at_end, closed: false, partial_history: self.provider_earlier })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn following_an_empty_placeholder_does_not_skip_its_first_text_byte() {
        let mut store = Store::new().unwrap();
        let item = |text: &str| Item { id: "stream".into(), role: Role::Assistant,
            content: ItemContent::Text { text: text.into(), thinking: false, phase: Phase::Final }, at: None, started_at: None };
        store.apply(Event::Item(item(""))).unwrap();
        let initial = store.export(None).unwrap();
        assert_eq!(initial.parts.len(), 1); assert!(initial.at_end);
        store.apply(Event::Item(item("Hello"))).unwrap();
        let next = store.export(initial.next).unwrap();
        assert_eq!(next.parts.len(), 1);
        assert!(matches!(&next.parts[0].item.content, ItemContent::Text { text, .. } if text == "Hello"));
    }

    #[test]
    fn following_keeps_its_stable_anchor_when_history_rebalances_ordinals() {
        let mut store = Store::new().unwrap();
        let item = |id: &str, text: &str| Item { id: id.into(), role: Role::Assistant,
            content: ItemContent::Text { text: text.into(), thinking: false, phase: Phase::Final }, at: None, started_at: None };
        store.apply(Event::Item(item("first", "one"))).unwrap();
        // Start in a sparse range so the external ordinal hint is stale after
        // rebalance and newer items receive ordinals below that old hint.
        store.db.execute("UPDATE items SET ordinal=?1", [100 * ORDINAL_STRIDE]).unwrap();
        let initial = store.export(None).unwrap();
        store.rebalance().unwrap();
        store.apply(Event::Item(item("second", "two"))).unwrap();
        let next = store.export(initial.next).unwrap();
        assert_eq!(next.parts.len(), 1);
        assert_eq!(next.parts[0].item.id, "second");
    }

    #[test]
    fn export_pages_large_unicode_items_without_changing_presentation_cursor() {
        let mut store = Store::new().unwrap();
        let text = "界".repeat(PART_BYTES);
        store.apply(Event::Item(Item { id: "large".into(), role: Role::Assistant,
            content: ItemContent::Text { text: text.clone(), thinking: false, phase: Phase::Final }, at: None, started_at: None })).unwrap();
        let before = (store.end, store.forward, store.first, store.newest, store.paging);
        let page = store.export(None).unwrap();
        let exported = page.parts.iter().map(|part| match &part.item.content { ItemContent::Text { text, .. } => text.as_str(), _ => panic!() }).collect::<String>();
        assert_eq!(exported, text); assert!(page.at_end);
        assert_eq!(before, (store.end, store.forward, store.first, store.newest, store.paging));
        store.apply(Event::Item(Item { id: "next".into(), role: Role::User,
            content: ItemContent::Text { text: "appended".into(), thinking: false, phase: Phase::Final }, at: None, started_at: None })).unwrap();
        let following = store.export(page.next).unwrap();
        assert_eq!(following.parts.len(), 1); assert_eq!(following.parts[0].item.id, "next");
    }
}
