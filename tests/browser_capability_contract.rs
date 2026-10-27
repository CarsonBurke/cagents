#![cfg(feature = "ui")]

use std::collections::BTreeMap;

use cagents::{
    capability::{BrowserCapability, BrowserRestoreAction, browser_restore_action},
    location::SanitizedLocation,
    model::{
        BootId, BrowserId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind, CardState,
        Existence, Exposure, Residency, WorkspaceState,
    },
    persistence::{SnapshotBase, StateStore},
};
use url::Url;

fn persisted_browser() -> BrowserSession {
    let history = [
        "https://example.test/projects",
        "https://example.test/projects/cagents",
        "https://example.test/projects/cagents/activity",
    ]
    .map(|url| Url::parse(url).unwrap())
    .to_vec();
    BrowserSession {
        id: BrowserId::new(),
        current_url: history[1].clone(),
        history,
        history_index: 1,
        scroll_x: 17.25,
        scroll_y: 912.5,
        fidelity: BrowserRestoreFidelity::HistoryAndScroll,
    }
}

#[test]
fn persisted_browser_keeps_navigation_authority_but_presents_a_sanitized_location() {
    let raw = "https://alice:password@example.test:8443/private%20path?oauth=secret#token";
    let url = Url::parse(raw).unwrap();
    let session = BrowserSession {
        id: BrowserId::new(),
        current_url: url.clone(),
        history: vec![url.clone()],
        history_index: 0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        fidelity: BrowserRestoreFidelity::HistoryAndScroll,
    };

    assert_eq!(session.current_url.as_str(), raw);
    assert_eq!(session.history, vec![url]);
    assert_eq!(
        SanitizedLocation::from(&session.current_url).as_str(),
        "https://example.test:8443/private%20path"
    );

    #[cfg(not(feature = "integrated-browser"))]
    assert!(matches!(
        browser_restore_action(&session),
        BrowserRestoreAction::OfferExternal(full) if full.as_str() == raw
    ));
}

#[test]
fn persisted_browser_handling_matches_the_compiled_capability_without_mutation() {
    let session = persisted_browser();
    let before = session.clone();
    let action = browser_restore_action(&session);

    assert_eq!(session, before, "capability inspection must be lossless");

    #[cfg(feature = "integrated-browser")]
    {
        assert_eq!(BrowserCapability::current(), BrowserCapability::Integrated);
        assert_eq!(action, BrowserRestoreAction::HydrateIntegrated);
    }

    #[cfg(not(feature = "integrated-browser"))]
    {
        assert_eq!(
            BrowserCapability::current(),
            BrowserCapability::ExternalOnly
        );
        assert_eq!(
            action,
            BrowserRestoreAction::OfferExternal(&before.current_url)
        );
        assert_eq!(session.history, before.history);
        assert_eq!(session.history_index, before.history_index);
        assert_eq!((session.scroll_x, session.scroll_y), (17.25, 912.5));
        assert_eq!(session.fidelity, BrowserRestoreFidelity::HistoryAndScroll);
    }
}

#[test]
fn durable_browser_urls_keep_navigation_authority_across_store_reopen() {
    // Synthetic credentials distinguish durable navigation state from its
    // sanitized presentation without opening any URL or starting a browser.
    let history = [
        "https://fixture-user:fixture-password@example.test:8443/start?fixture=first#first",
        "https://fixture-user:fixture-password@example.test:8443/private%20path?fixture=current#current",
        "https://other-user:other-password@example.test:8443/next?fixture=last#last",
    ]
    .map(|url| Url::parse(url).unwrap())
    .to_vec();
    let expected = BrowserSession {
        id: BrowserId::new(),
        current_url: history[1].clone(),
        history,
        history_index: 1,
        scroll_x: 17.25,
        scroll_y: 912.5,
        fidelity: BrowserRestoreFidelity::HistoryAndScroll,
    };
    let card = CardId::new();
    let workspace = WorkspaceState {
        cards: vec![CardState {
            id: card,
            generation: 0,
            title: "Browser persistence fixture".into(),
            width_fraction: 1.0,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Cold,
            kind: CardKind::Browser(expected.clone()),
        }],
        focused: Some(card),
        ..WorkspaceState::default()
    };
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("session.sqlite3");
    let boot = BootId::new("browser-capability-persistence");
    {
        let store = StateStore::open(&database, boot.clone()).unwrap();
        store
            .save_workspace(
                &workspace,
                SnapshotBase {
                    workspace: 0,
                    cards: &BTreeMap::new(),
                },
            )
            .unwrap();
        store.shutdown().unwrap();
    }

    let store = StateStore::open(&database, boot).unwrap();
    let restored = store
        .load_workspace(workspace.id, None)
        .unwrap()
        .expect("the browser-only workspace must survive reopening its database");
    assert_eq!(restored.cards.len(), 1);
    assert_eq!(restored.cards[0].id, card);
    let CardKind::Browser(browser) = &restored.cards[0].kind else {
        panic!("the restored card must remain a browser");
    };
    assert_eq!(browser.id, expected.id);
    assert_eq!(browser.current_url, expected.current_url);
    assert_eq!(browser.history, expected.history);
    assert_eq!(browser.history_index, expected.history_index);
    assert_eq!((browser.scroll_x, browser.scroll_y), (17.25, 912.5));
    assert_eq!(browser.fidelity, BrowserRestoreFidelity::HistoryAndScroll);
    assert_eq!(
        SanitizedLocation::from(&browser.current_url).as_str(),
        "https://example.test:8443/private%20path"
    );

    let action = browser_restore_action(browser);
    #[cfg(feature = "integrated-browser")]
    assert_eq!(action, BrowserRestoreAction::HydrateIntegrated);
    #[cfg(not(feature = "integrated-browser"))]
    assert_eq!(
        action,
        BrowserRestoreAction::OfferExternal(&expected.current_url)
    );
    store.shutdown().unwrap();
}
