/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! SameSite cookie enforcement (cagents patch): which restricted cookies a
//! request carries and which ones its response may set.

use cookie::{Cookie as CookiePair, SameSite};
use http::header::{self, HeaderValue};
use http::{Method, StatusCode};
use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Incoming};
use hyper::{Request as HyperRequest, Response as HyperResponse};
use net::cookie::{SameSiteContext, ServoCookie};
use net::cookie_storage::CookieStorage;
use net_traits::CookieSource;
use net_traits::request::{
    CredentialsMode, Destination, InsecureRequestsPolicy, Origin, Referrer, Request,
    RequestBuilder, RequestClient, RequestMode,
};
use servo_base::id::TEST_PIPELINE_ID;
use servo_url::{ImmutableOrigin, ServoUrl};

use crate::{fetch_with_context, make_body, make_server, mock_origin, new_fetch_context};

fn url(url: &str) -> ServoUrl {
    ServoUrl::parse(url).unwrap()
}

fn site(origin: &str) -> ImmutableOrigin {
    url(origin).origin()
}

fn client(is_nested_browsing_context: bool) -> RequestClient {
    RequestClient {
        preloaded_resources: Default::default(),
        policy_container: Default::default(),
        origin: Origin::Origin(mock_origin()),
        is_nested_browsing_context,
        insecure_requests_policy: InsecureRequestsPolicy::DoNotUpgrade,
    }
}

fn request(target: &str, site_for_cookies: Option<&str>) -> RequestBuilder {
    let builder = RequestBuilder::new(None, url(target), Referrer::NoReferrer);
    match site_for_cookies {
        Some(origin) => builder.site_for_cookies(site(origin)),
        None => builder,
    }
}

fn context(same_site: bool, top_level_navigation: bool, safe_method: bool) -> SameSiteContext {
    SameSiteContext {
        same_site,
        top_level_navigation,
        safe_method,
    }
}

#[test]
fn restricted_cookies_are_sent_only_where_their_attribute_allows() {
    let same_site = context(true, false, false);
    let safe_navigation = context(false, true, true);
    let unsafe_navigation = context(false, true, false);
    let subresource = context(false, false, true);

    for context in [same_site, safe_navigation, unsafe_navigation, subresource] {
        assert!(context.permits_retrieval(None), "{context:?}");
        assert!(context.permits_retrieval(Some(SameSite::None)), "{context:?}");
    }

    assert!(same_site.permits_retrieval(Some(SameSite::Strict)));
    assert!(same_site.permits_retrieval(Some(SameSite::Lax)));

    assert!(!safe_navigation.permits_retrieval(Some(SameSite::Strict)));
    assert!(safe_navigation.permits_retrieval(Some(SameSite::Lax)));

    // A cross-site form POST must not carry Lax cookies.
    assert!(!unsafe_navigation.permits_retrieval(Some(SameSite::Strict)));
    assert!(!unsafe_navigation.permits_retrieval(Some(SameSite::Lax)));

    assert!(!subresource.permits_retrieval(Some(SameSite::Strict)));
    assert!(!subresource.permits_retrieval(Some(SameSite::Lax)));
}

#[test]
fn restricted_cookies_are_stored_only_from_same_site_requests_and_top_level_navigations() {
    for same_site in [None, Some(SameSite::None)] {
        assert!(context(false, false, true).permits_storage(same_site));
    }
    for restricted in [SameSite::Lax, SameSite::Strict] {
        assert!(context(true, false, false).permits_storage(Some(restricted)));
        // Any method: a cross-site POST that lands on a login page may set its session.
        assert!(context(false, true, false).permits_storage(Some(restricted)));
        assert!(!context(false, false, true).permits_storage(Some(restricted)));
    }
}

#[test]
fn a_request_is_same_site_only_when_every_url_shares_its_site_for_cookies() {
    let same_site = |builder: RequestBuilder| builder.build().is_same_site_for_cookies();

    assert!(same_site(request(
        "https://app.example.com/",
        Some("https://www.example.com")
    )));
    assert!(!same_site(request(
        "https://example.org/",
        Some("https://www.example.com")
    )));
    // Same-site is schemeful.
    assert!(!same_site(request(
        "http://www.example.com/",
        Some("https://www.example.com")
    )));
    // A request whose client sets no site for cookies is cross-site.
    assert!(!same_site(request("https://www.example.com/", None)));
    // So is one whose client has a cross-site ancestor.
    let mut opaque = request("https://www.example.com/", None);
    opaque.site_for_cookies = ImmutableOrigin::new_opaque();
    assert!(!same_site(opaque));

    // A redirect through another site makes the whole request cross-site.
    let mut redirected = request(
        "https://www.example.com/start",
        Some("https://www.example.com"),
    );
    redirected.url_list = vec![
        url("https://www.example.com/start"),
        url("https://tracker.example.org/bounce"),
        url("https://www.example.com/end"),
    ];
    assert!(!same_site(redirected));
    let mut redirected = request(
        "https://www.example.com/start",
        Some("https://www.example.com"),
    );
    redirected.url_list = vec![
        url("https://www.example.com/start"),
        url("https://login.example.com/end"),
    ];
    assert!(same_site(redirected));
}

#[test]
fn websocket_urls_are_compared_as_their_http_equivalents() {
    let site_for_cookies = site("https://www.example.com");
    assert!(net_traits::request::is_same_site_for_cookies(
        &site_for_cookies,
        &url("wss://chat.example.com/socket")
    ));
    assert!(!net_traits::request::is_same_site_for_cookies(
        &site_for_cookies,
        &url("ws://chat.example.com/socket")
    ));
    assert!(!net_traits::request::is_same_site_for_cookies(
        &site_for_cookies,
        &url("wss://chat.example.org/socket")
    ));
}

#[test]
fn only_a_navigation_of_a_top_level_client_is_a_top_level_navigation() {
    let navigation = |client: Option<RequestClient>, mode: RequestMode| -> Request {
        let mut builder = request("https://www.example.com/", None).mode(mode);
        builder.client = client;
        builder.build()
    };
    assert!(navigation(Some(client(false)), RequestMode::Navigate).is_top_level_navigation());
    assert!(!navigation(Some(client(true)), RequestMode::Navigate).is_top_level_navigation());
    assert!(!navigation(None, RequestMode::Navigate).is_top_level_navigation());
    assert!(!navigation(Some(client(false)), RequestMode::NoCors).is_top_level_navigation());
}

fn storage_with_every_same_site_value(origin: &ServoUrl) -> CookieStorage {
    let mut storage = CookieStorage::new(150);
    for cookie in [
        "strict=1; SameSite=Strict",
        "lax=1; SameSite=Lax",
        "none=1; SameSite=None; Secure",
        "default=1",
    ] {
        let cookie = ServoCookie::from_cookie_string(cookie, origin, CookieSource::HTTP).unwrap();
        storage.push(cookie, origin, CookieSource::HTTP);
    }
    storage
}

fn names(header: Option<String>) -> Vec<String> {
    let mut names: Vec<String> = header
        .unwrap_or_default()
        .split("; ")
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split('=').next().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn the_cookie_header_leaves_out_what_the_request_context_excludes() {
    let origin = url("https://www.example.com/");
    let mut storage = storage_with_every_same_site_value(&origin);

    assert_eq!(
        names(storage.cookies_for_request(&origin, context(true, false, true))),
        ["default", "lax", "none", "strict"]
    );
    assert_eq!(
        names(storage.cookies_for_request(&origin, context(false, true, true))),
        ["default", "lax", "none"]
    );
    assert_eq!(
        names(storage.cookies_for_request(&origin, context(false, true, false))),
        ["default", "none"]
    );
    assert_eq!(
        names(storage.cookies_for_request(&origin, context(false, false, true))),
        ["default", "none"]
    );
    // APIs without a request, such as the embedder's, see every cookie.
    assert_eq!(
        names(storage.cookies_for_url(&origin, CookieSource::HTTP)),
        ["default", "lax", "none", "strict"]
    );
}

#[test]
fn same_site_none_requires_secure() {
    let origin = url("https://www.example.com/");
    assert!(
        ServoCookie::from_cookie_string("a=b; SameSite=None", &origin, CookieSource::HTTP)
            .is_none()
    );
    assert!(
        ServoCookie::from_cookie_string("a=b; SameSite=None; Secure", &origin, CookieSource::HTTP)
            .is_some()
    );
    let mut cookie = CookiePair::new("a", "b");
    cookie.set_same_site(SameSite::None);
    assert!(ServoCookie::new_wrapped(cookie, &origin, CookieSource::NonHTTP).is_none());
}

/// Fetches `server` from a client whose site for cookies is `site_for_cookies`,
/// with `restricted=1; SameSite=Lax` and `plain=1` in the jar, and returns the
/// `Cookie` header the server received.
fn cookie_header_sent(site_for_cookies: Option<&str>, method: Method, mode: RequestMode) -> String {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let handler = move |request: HyperRequest<Incoming>,
                        response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
        let cookie = request
            .headers()
            .get(header::COOKIE)
            .map(|value| value.to_str().unwrap().to_owned())
            .unwrap_or_default();
        sender.send(cookie).unwrap();
        *response.body_mut() = make_body(b"ok".to_vec());
    };
    let (server, server_url) = make_server(handler);
    let mut context = new_fetch_context(None, None);
    {
        let mut jar = context.state.cookie_jar.write();
        for cookie in ["restricted=1; SameSite=Lax", "plain=1"] {
            let cookie =
                ServoCookie::from_cookie_string(cookie, &server_url, CookieSource::HTTP).unwrap();
            jar.push(cookie, &server_url, CookieSource::HTTP);
        }
    }
    let site_for_cookies = site_for_cookies
        .map(site)
        .unwrap_or_else(|| server_url.origin());
    let mut builder = RequestBuilder::new(None, server_url.clone(), Referrer::NoReferrer)
        .method(method)
        .destination(Destination::Document)
        .mode(mode)
        .origin(mock_origin())
        .pipeline_id(Some(TEST_PIPELINE_ID))
        .credentials_mode(CredentialsMode::Include)
        .policy_container(Default::default())
        .site_for_cookies(site_for_cookies);
    builder.client = Some(client(false));
    let response = fetch_with_context(builder.build(), &mut context);
    let _ = server.close();
    assert!(response.internal_response.unwrap().status.code().is_success());
    let mut names = names(Some(receiver.recv().unwrap()));
    names.dedup();
    names.join(",")
}

#[test]
fn lax_cookies_reach_the_network_only_from_their_site_or_a_safe_top_level_navigation() {
    let cross_site = Some("https://attacker.example.org");
    assert_eq!(
        cookie_header_sent(None, Method::GET, RequestMode::NoCors),
        "plain,restricted"
    );
    assert_eq!(
        cookie_header_sent(cross_site, Method::GET, RequestMode::NoCors),
        "plain"
    );
    assert_eq!(
        cookie_header_sent(cross_site, Method::GET, RequestMode::Navigate),
        "plain,restricted"
    );
    assert_eq!(
        cookie_header_sent(cross_site, Method::POST, RequestMode::Navigate),
        "plain"
    );
}

/// Fetches a server that sets `restricted=1; SameSite=Lax` and `plain=1`, and
/// returns the names of the cookies the jar then holds for it.
fn cookies_stored(site_for_cookies: Option<&str>, mode: RequestMode) -> Vec<String> {
    let handler = move |_: HyperRequest<Incoming>,
                        response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
        for cookie in ["restricted=1; SameSite=Lax", "plain=1"] {
            response
                .headers_mut()
                .append(header::SET_COOKIE, HeaderValue::from_static(cookie));
        }
        *response.status_mut() = StatusCode::OK;
        *response.body_mut() = make_body(b"ok".to_vec());
    };
    let (server, server_url) = make_server(handler);
    let mut context = new_fetch_context(None, None);
    let site_for_cookies = site_for_cookies
        .map(site)
        .unwrap_or_else(|| server_url.origin());
    let mut builder = RequestBuilder::new(None, server_url.clone(), Referrer::NoReferrer)
        .destination(Destination::Document)
        .mode(mode)
        .origin(mock_origin())
        .pipeline_id(Some(TEST_PIPELINE_ID))
        .credentials_mode(CredentialsMode::Include)
        .policy_container(Default::default())
        .site_for_cookies(site_for_cookies);
    builder.client = Some(client(false));
    let _ = fetch_with_context(builder.build(), &mut context);
    let _ = server.close();
    let header = context
        .state
        .cookie_jar
        .write()
        .cookies_for_url(&server_url, CookieSource::HTTP);
    names(header)
}

#[test]
fn a_cross_site_subresource_response_cannot_set_lax_cookies() {
    let cross_site = Some("https://attacker.example.org");
    assert_eq!(
        cookies_stored(None, RequestMode::NoCors),
        ["plain", "restricted"]
    );
    assert_eq!(cookies_stored(cross_site, RequestMode::NoCors), ["plain"]);
    assert_eq!(
        cookies_stored(cross_site, RequestMode::Navigate),
        ["plain", "restricted"]
    );
}
