//! Test-only access to image custody, not an admission or provider transport.
//! The caller owns the real managed seed/Ready handshake. Images already have
//! genuine private Store receipts; only clipboard completion is synthetic here.
use super::*;

fn runtime(app: &Cagents, card: CardId) -> &Runtime {
    match app.cards.get(&card) {
        Some(CardRuntime::Chat(runtime)) => runtime,
        _ => app.chat_ui.parked.get(&card).expect("parked chat runtime"),
    }
}

pub(in crate::app) fn insert_image_only(app: &mut Cagents, card: CardId, image: &Attachment) {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::validate(image).unwrap();
    let target = {
        let runtime = runtime(app, card);
        let handle = runtime.handle().expect("actual managed wire attachment");
        assert!(handle.is_connected());
        let snapshot = handle.snapshot();
        assert!(snapshot.ready);
        assert!(snapshot.capabilities.attachments);
        assert!(runtime.snapshot.ready);
        assert!(runtime.snapshot.capabilities.attachments);
        assert!(runtime.accepts_draft_attachments());
        runtime.target
    };
    let ticket = app
        .chat_runtime_mut(target)
        .unwrap()
        .begin_attachment_insert();
    drop(app.update(Message::ChatUi(Action::ImageSaved(
        target,
        ticket,
        Ok(image.clone()),
    ))));
    assert_eq!(
        runtime(app, card).composer.text(),
        format!("{} ", image.reference.as_ref().unwrap().token())
    );
    // The real editor actions remove only the inline marker, leaving the chip.
    for action in [
        text_editor::Action::Move(text_editor::Motion::DocumentStart),
        text_editor::Action::Select(text_editor::Motion::DocumentEnd),
        text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(String::new()))),
    ] {
        drop(app.update(Message::ChatUi(Action::Composer(target, action))));
    }
}

pub(in crate::app) fn assert_runtime_only_image(
    app: &Cagents,
    card: CardId,
    image: &Attachment,
    parked: bool,
    closed_elsewhere: bool,
) -> usize {
    let runtime = runtime(app, card);
    assert_eq!(app.chat_ui.parked.contains_key(&card), parked);
    assert_eq!(app.cards.contains_key(&card), !parked);
    assert_eq!(runtime.composer.text(), "");
    assert_eq!(runtime.unsent_text(), "");
    assert!(!runtime.has_unsent_text());
    assert_eq!(runtime.attachments, [image.clone()]);
    assert_eq!(runtime.unsent_attachments(), [image.clone()]);
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Chat(runtime.session.clone())
    );
    assert_eq!(runtime.closed_elsewhere, closed_elsewhere);
    assert_eq!(runtime.transition_blocker(), None);
    assert_eq!(runtime.pastes_pending, 0);
    assert_eq!(runtime.image_pastes_saving, 0);
    assert!(runtime.attachment_jobs.is_empty());
    assert!(runtime.attachment_bookmarks.is_empty());
    assert!(!app.chat_ui.phases.contains_key(&card));
    assert!(!app.chat_ui.drafts.contains_key(&card));
    assert!(!app.chat_ui.draft_attachments.contains_key(&card));
    chat::attachment_assets::validate(image).unwrap();
    runtime as *const Runtime as usize
}

pub(in crate::app) fn park(app: &mut Cagents, card: CardId) {
    assert!(!app.chat_ui.parked.contains_key(&card));
    let CardRuntime::Chat(runtime) = app.cards.remove(&card).unwrap() else {
        panic!("mounted chat runtime");
    };
    assert!(app.chat_ui.parked.insert(card, runtime).is_none());
}
