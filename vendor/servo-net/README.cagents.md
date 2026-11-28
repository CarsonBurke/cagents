# SameSite cookie enforcement

Source: `components/net`, `components/shared/net` and `components/script` from [servo/servo](https://github.com/servo/servo/tree/5c14ac9428d8d95c2f6c2f0300fe248d3929f282), revision `5c14ac9428d8d95c2f6c2f0300fe248d3929f282`, the revision the application pins. They are vendored as `vendor/servo-net`, `vendor/servo-net-traits` and `vendor/servo-script`. The upstream MPL-2.0 license is included in each. The standalone manifests keep the pinned dependency graph: Servo's own crates resolve to the same Git revision, and `Cargo.lock` changes only in the three packages' `source`. `lib.rs` (net, net-traits) and `build.rs` (script) carry the shared-checkout anchor.

The pinned Servo, like upstream `main` at the time of writing (servo/servo#25600), parses and stores a cookie's `SameSite` attribute but never applies it, so every cookie behaves as `SameSite=None`. This patch enforces `Lax` and `Strict` as RFC 6265bis describes:

- **Site for cookies.** Each request carries `site_for_cookies` (net-traits `Request` and `RequestBuilder`).
  - A document's is the top-level document's origin when the document and all its ancestors are same-site, and opaque otherwise (`Document::site_for_cookies`). A sandboxed document counts as the site of its URL. Servo runs each site in its own event loop, so an ancestor that is missing from the document's event loop counts as cross-site.
  - A dedicated worker keeps its owner's. Service workers, worklets and anything else that doesn't set one get a fresh opaque origin, which is same-site with nothing.
  - Script sets the value where requests leave it: `GlobalScope::fetch_with_network_listener`, `Document::fetch` and `fetch_background`, the module loader, `load_whole_resource`, `with_global_scope` (WebSockets) and the prefetcher.
  - A top-level navigation uses its initiator's origin, or its own URL when the user or the embedder started it. A nested navigation uses its parent's value, and is opaque when its parent is on another site or another site started it.
  - A request that sets nothing (font loads in the `fonts` crate, for example) is treated as cross-site, so a missed path withholds restricted cookies rather than leaking them.
- **Sending** (`SameSiteContext::permits_retrieval`). A request is same-site when every URL in its URL list is same-site, schemefully, with its site for cookies. A host with a trailing dot (`example.com.`) is its own site, as the URL Standard's public suffix algorithm says; upstream counted every such host under one public suffix as a single site, and accepted `Domain=com.`. A redirect through another site therefore makes it cross-site. WebSocket URLs are compared as their HTTP equivalents. `Strict` cookies go only on same-site requests. `Lax` cookies also go on top-level navigations with a safe method, but not on a cross-site form `POST`.
- **Storing** (`permits_storage`). A `Lax` or `Strict` cookie in the response to a cross-site request is ignored unless the request is a top-level navigation. `SameSite=None` without `Secure` is rejected (step 19).
- **`document.cookie`** sends the document's site for cookies with `GetCookieStringForUrl` and `SetCookiesForUrl`. A document whose site for cookies is cross-site, such as a frame under another site, neither sees nor sets `Lax` and `Strict` cookies, as in Chromium.
- **Unchanged.** Cookies without a `SameSite` attribute stay unrestricted (Servo isn't lax-by-default). The embedder's cookie APIs see every cookie. So does the Cookie Store API, which Servo disables by default (`dom_cookiestore_enabled`).

Known gaps where the patch is no stricter than before:

- The HTTP cache is keyed by URL alone, so a cross-site subresource can be served a response that a same-site request fetched with restricted cookies, unless it varies on `Cookie`. Chromium and Firefox partition the cache by top-level site.
- The application's own reloads (history, restore) are browser-initiated, so they count as same-site with their URL, as in Chromium's session restore.

Known gaps where the patch is stricter than other browsers:

- Requests that set no site for cookies are cross-site. Font loads from the `fonts` crate, service workers and worklets therefore don't carry `Lax` or `Strict` cookies.
- A sandboxed iframe runs in its own event loop, so its parent counts as missing and the frame as cross-site.
- A browser-initiated navigation that redirects to another site loses its `Strict` cookies there; `Lax` cookies are still sent.

The regressions run as a standalone package:

```sh
scripts/cargo-gate test --manifest-path vendor/servo-net/Cargo.toml --features test-util --test main same_site
scripts/cargo-gate test --manifest-path vendor/servo-net-traits/Cargo.toml --test pub_domains
```

The standalone manifest patches `servo-net-traits` to `../servo-net-traits`, so the tests build against the patched request type.

Remove this patch when the pinned Servo enforces SameSite.
