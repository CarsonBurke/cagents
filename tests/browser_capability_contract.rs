use cagents::{
    capability::{BrowserCapability, BrowserRestoreAction, browser_restore_action},
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
