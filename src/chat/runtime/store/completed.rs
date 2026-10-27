//! Share only proven completed prose bytes; each Item still has current metadata.
use super::*;
use std::sync::Arc;

impl Store {
    pub(super) fn completed_text_body(
        &self,
        root: &str,
        ranges: [[usize; 2]; 2],
        previous: &Arc<Item>,
        current: &Item,
    ) -> Option<crate::chat::TextBody> {
        #[cfg(test)]
        if self.publication_work.disable_reuse.get() {
            return None;
        }
        let (ItemContent::Text { text, .. }, ItemContent::Text { .. }) =
            (&previous.content, &current.content)
        else {
            return None;
        };
        let [[start, end], [start1, end1]] = ranges;
        if start1 != 0 || end1 != 0 || end.checked_sub(start)? != text.len() {
            return None;
        }
        (self
            .appended
            .owns(self.copy_cache, root, 0, start, previous)
            && self
                .appended
                .owns(self.copy_cache, root, 1, start1, previous)
            && self.appended.preserves(root, 0, end)
            && self.appended.preserves(root, 1, end1))
        .then(|| text.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prose(id: &str, text: String) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: Phase::Final,
            },
        }
    }

    fn body(item: &Item) -> &crate::chat::TextBody {
        let ItemContent::Text { text, .. } = &item.content else {
            panic!("prose item")
        };
        text
    }

    fn append(store: &mut Store, tail: &str) {
        store
            .apply(Event::TextDelta {
                id: "large".into(),
                role: Role::Assistant,
                thinking: false,
                delta: tail.into(),
            })
            .unwrap();
    }

    #[test]
    fn completed_payloads_share_with_fresh_clocks_when_the_root_is_paged_out() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::at(directory.path().into()).unwrap();
        let original = "x".repeat(PART_BYTES * 32 + 2000);
        // Imported history has unknown clocks; the first live append must
        // publish BOTH new authoritative clocks, even with part zero absent.
        store.put(prose("large", original.clone()), true).unwrap();
        let mut previous = store.window().unwrap().items;
        assert!(!previous.iter().any(|item| item.id == "large"));
        assert!(
            previous
                .iter()
                .all(|item| item.at.is_none() && item.started_at.is_none())
        );
        let retained = previous.clone();
        let old_copy = store.copy_projection[0].clone().unwrap();
        store.field_bytes.set(0);
        append(&mut store, "尾");
        let next = store.window_reusing(&mut previous).unwrap();
        let canonical: Item = postcard::from_bytes(
            &store
                .db
                .query_row("SELECT metadata FROM items WHERE id='large'", [], |row| {
                    row.get::<_, Vec<u8>>(0)
                })
                .unwrap(),
        )
        .unwrap();
        assert!(canonical.at.is_some() && canonical.started_at.is_some());
        assert!(!next.items.iter().any(|item| item.id == "large"));
        let mut shared = 0;
        for current in &next.items {
            assert_eq!(current.at, canonical.at);
            assert_eq!(current.started_at, canonical.started_at);
            assert_eq!(current.role, canonical.role);
            assert!(matches!(
                &current.content,
                ItemContent::Text {
                    thinking: false,
                    phase: Phase::Final,
                    ..
                }
            ));
            if let Some(old) = retained.iter().find(|old| old.id == current.id)
                && body(old).len() == PART_BYTES
            {
                assert!(
                    !Arc::ptr_eq(old, current),
                    "fresh metadata needs a fresh outer Item"
                );
                assert!(
                    body(old).shares_storage_with(body(current)),
                    "completed payload must not copy"
                );
                assert!(old.at.is_none() && old.started_at.is_none());
                shared += 1;
            }
        }
        assert!(
            shared > 8,
            "exercise real multi-megabyte completed coverage"
        );
        assert!(
            store.field_bytes.get() < PART_BYTES,
            "only live tail and boundary probes read SQL bodies"
        );
        assert!(next.changes.iter().any(|change| matches!(change,
            crate::chat::presentation::ItemChange::Append { suffix, item, .. }
            if suffix.is_empty() && item.at == canonical.at && item.started_at == canonical.started_at)));
        for source in store.copy_projection.iter().flatten() {
            assert_eq!(source.cache, old_copy.cache);
            assert_eq!(source.token, old_copy.token);
            assert_eq!(source.revision, old_copy.revision);
            assert_eq!(source.bytes, (original.len() + "尾".len()) as u64);
        }
        let page = store
            .peer_page(&mut peer_query::Reader::default(), None)
            .unwrap();
        assert!(!page.parts.is_empty());
        assert_eq!(page.parts[0].at, canonical.at);
        assert_eq!(page.parts[0].started_at, canonical.started_at);
        assert_eq!(
            page.parts[0].kind,
            peer_query::Kind::AssistantText {
                phase: Phase::Final
            }
        );
    }

    #[test]
    fn completed_payload_sharing_refuses_foreign_residents_and_rewrites() {
        for foreign in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut store = Store::at(directory.path().into()).unwrap();
            let original = "x".repeat(PART_BYTES * 3 + 1000);
            store.put(prose("large", original.clone()), true).unwrap();
            let mut previous = store.window().unwrap().items;
            let retained = previous.clone();
            if foreign {
                previous = previous
                    .iter()
                    .map(|item| Arc::new((**item).clone()))
                    .collect();
            } else {
                store
                    .apply(Event::Item(prose("large", "z".repeat(original.len()))))
                    .unwrap();
            }
            append(&mut store, " tail");
            let next = store.window_reusing(&mut previous).unwrap();
            for item in &next.items {
                if let Some(old) = retained.iter().find(|old| old.id == item.id) {
                    assert!(
                        !body(old).shares_storage_with(body(item)),
                        "unowned/replaced bytes cannot gain sharing authority"
                    );
                }
            }
            let actual: String = next.items.iter().map(|item| body(item).as_str()).collect();
            assert_eq!(
                actual,
                format!(
                    "{} tail",
                    if foreign {
                        original
                    } else {
                        "z".repeat(original.len())
                    }
                )
            );
            assert!(
                retained
                    .iter()
                    .all(|item| body(item).bytes().all(|byte| byte == b'x'))
            );
        }
    }
    #[test]
    fn paged_out_metadata_replacement_revokes_sharing_before_new_valid_appends() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::at(directory.path().into()).unwrap();
        let original = "x".repeat(PART_BYTES * 32 + 2000);
        store.put(prose("large", original.clone()), true).unwrap();
        let mut previous = store.window().unwrap().items;
        let old = previous.clone();
        let original_copy = store.copy_projection[0].clone().unwrap();
        assert!(!previous.iter().any(|item| item.id == "large"));
        // Use the actual authoritative replacement API, never raw SQL or a
        // forged append proof. Identical body bytes do not retain Item ownership.
        let mut replacement = prose("large", original.clone());
        replacement.role = Role::User;
        let ItemContent::Text { phase, .. } = &mut replacement.content else {
            unreachable!()
        };
        *phase = Phase::Commentary;
        store.apply(Event::Item(replacement)).unwrap();
        store
            .apply(Event::TextDelta {
                id: "large".into(),
                role: Role::User,
                thinking: false,
                delta: " tail".into(),
            })
            .unwrap();
        let replacement = store.window_reusing(&mut previous).unwrap();
        assert!(!replacement.items.iter().any(|item| item.id == "large"));
        for current in &replacement.items {
            assert_eq!(current.role, Role::User);
            assert!(matches!(
                &current.content,
                ItemContent::Text {
                    thinking: false,
                    phase: Phase::Commentary,
                    ..
                }
            ));
            if let Some(before) = old.iter().find(|before| before.id == current.id) {
                assert!(
                    !body(before).shares_storage_with(body(current)),
                    "metadata replacement must revoke old outer-Item append ownership"
                );
            }
        }
        let source = store.copy_projection[0].clone().unwrap();
        assert_eq!(source.cache, original_copy.cache);
        assert_eq!(source.token, original_copy.token);
        assert_eq!(
            source.revision, original_copy.revision,
            "identical canonical text is an existing prefix, not a fabricated copy rewrite"
        );
        assert_eq!(source.bytes, (original.len() + " tail".len()) as u64);
        previous = replacement.items;
        let installed = previous.clone();
        store
            .apply(Event::TextDelta {
                id: "large".into(),
                role: Role::User,
                thinking: false,
                delta: " next".into(),
            })
            .unwrap();
        let next = store.window_reusing(&mut previous).unwrap();
        let canonical: Item = postcard::from_bytes(
            &store
                .db
                .query_row("SELECT metadata FROM items WHERE id='large'", [], |row| {
                    row.get::<_, Vec<u8>>(0)
                })
                .unwrap(),
        )
        .unwrap();
        let mut shared = 0;
        for current in &next.items {
            assert_eq!(current.role, Role::User);
            assert_eq!(current.at, canonical.at);
            assert_eq!(current.started_at, canonical.started_at);
            assert!(matches!(
                &current.content,
                ItemContent::Text {
                    thinking: false,
                    phase: Phase::Commentary,
                    ..
                }
            ));
            if let Some(before) = installed.iter().find(|before| before.id == current.id)
                && body(before).len() == PART_BYTES
            {
                assert!(body(before).shares_storage_with(body(current)));
                shared += 1;
            }
        }
        assert!(shared > 8);
        let page = store
            .peer_page(&mut peer_query::Reader::default(), None)
            .unwrap();
        assert!(!page.parts.is_empty());
        assert_eq!(page.parts[0].kind, peer_query::Kind::UserText);
        assert_eq!(page.parts[0].at, canonical.at);
        assert_eq!(page.parts[0].started_at, canonical.started_at);
        for source in store.copy_projection.iter().flatten() {
            assert_eq!(source.bytes, (original.len() + " tail next".len()) as u64);
        }
        // A later authoritative thinking replacement must preserve the
        // existing peer allowlist, even though the resident bytes remain prose.
        previous = next.items;
        let mut thinking = prose("large", format!("{original} tail next"));
        let ItemContent::Text {
            thinking: flag,
            phase,
            ..
        } = &mut thinking.content
        else {
            unreachable!()
        };
        *flag = true;
        *phase = Phase::Commentary;
        store.apply(Event::Item(thinking)).unwrap();
        let current = store.window_reusing(&mut previous).unwrap();
        assert!(current.items.iter().all(|item| item.role == Role::Assistant
            && matches!(
                &item.content,
                ItemContent::Text {
                    thinking: true,
                    phase: Phase::Commentary,
                    ..
                }
            )));
        let page = store
            .peer_page(&mut peer_query::Reader::default(), None)
            .unwrap();
        assert!(
            page.parts.is_empty(),
            "thinking payload must remain outside peer inspection"
        );
    }
}
