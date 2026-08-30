use cagents::{
    capability::{BrowserCapability, BrowserRestoreAction, browser_restore_action},
    location::SanitizedLocation,
    model::{BrowserId, BrowserRestoreFidelity, BrowserSession},
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
