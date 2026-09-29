use std::fmt;

use url::Url;

/// Interpret omnibox input before URL parsing, which otherwise treats
/// `localhost:3000` as an opaque scheme.
pub fn navigation_url(input: &str) -> Result<Url, url::ParseError> {
    navigation_url_with_search(input, "https://duckduckgo.com/?q={query}")
}

pub fn navigation_url_with_search(input: &str, search_url: &str) -> Result<Url, url::ParseError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(url::ParseError::EmptyHost);
    }
    if input.contains("://") || input.starts_with("file:") || input.starts_with("about:") {
        return Url::parse(input);
    }
    let candidate = Url::parse(&format!("http://{input}"));
    if let Ok(mut url) = candidate {
        let local = match url.host() {
            Some(url::Host::Domain(host)) => {
                host == "localhost" || host.ends_with(".localhost") || host.ends_with(".test")
            }
            Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
            Some(url::Host::Ipv6(ip)) => {
                ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
            }
            None => false,
        };
        if local {
            return Ok(url);
        }
        if !input.chars().any(char::is_whitespace)
            && url.host_str().is_some_and(|host| host.contains('.'))
        {
            let _ = url.set_scheme("https");
            return Ok(url);
        }
    }
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", input)
        .finish();
    let encoded = &encoded[2..];
    Url::parse(&search_url.replace("{query}", &encoded))
}

/// A browser location that is safe to present outside navigation and restore
/// state.
///
/// HTTP(S) locations retain only their normalized origin and path. Every
/// other scheme is reduced to its scheme because its payload may contain
/// credentials, source code, local paths, or an embedded origin. Invalid
/// input is represented by a constant rather than echoed back to the user.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SanitizedLocation(String);

impl SanitizedLocation {
    pub const INVALID: &'static str = "Invalid browser location";

    pub fn from_url(url: &Url) -> Self {
        match url.scheme() {
            "http" | "https" if url.host().is_some() => {
                let mut sanitized = url.clone();
                // These setters cannot fail for a URL with authority. Keeping
                // this defensive instead of unwrapping preserves the total
                // presentation boundary if the url crate broadens its model.
                if sanitized.set_username("").is_err() || sanitized.set_password(None).is_err() {
                    return Self::scheme_only(url.scheme());
                }
                sanitized.set_query(None);
                sanitized.set_fragment(None);
                Self(sanitized.into())
            }
            scheme => Self::scheme_only(scheme),
        }
    }

    pub fn from_input(input: &str) -> Self {
        Url::parse(input)
            .as_ref()
            .map(Self::from_url)
            .unwrap_or_else(|_| Self::invalid())
    }

    pub fn invalid() -> Self {
        Self(Self::INVALID.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    fn scheme_only(scheme: &str) -> Self {
        Self(format!("{scheme}:"))
    }
}

impl From<&Url> for SanitizedLocation {
    fn from(url: &Url) -> Self {
        Self::from_url(url)
    }
}

impl fmt::Display for SanitizedLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omnibox_routes_local_servers_hosts_and_searches() {
        for (input, expected) in [
            ("localhost:3000", "http://localhost:3000/"),
            ("127.0.0.1:8080/a?b=2#c", "http://127.0.0.1:8080/a?b=2#c"),
            ("192.168.1.2:3000", "http://192.168.1.2:3000/"),
            ("[::1]:3000", "http://[::1]:3000/"),
            ("app.localhost:5173", "http://app.localhost:5173/"),
            ("example.com", "https://example.com/"),
        ] {
            assert_eq!(navigation_url(input).unwrap().as_str(), expected);
        }
        assert_eq!(
            navigation_url("rust iced").unwrap().query(),
            Some("q=rust+iced")
        );
        assert_eq!(navigation_url("rust").unwrap().query(), Some("q=rust"));
        assert!(navigation_url("https://bad host/").is_err());
        assert!(navigation_url("").is_err());
        assert_eq!(
            navigation_url_with_search("a&b #c", "https://example.org/search?q={query}&source=ade")
                .unwrap()
                .as_str(),
            "https://example.org/search?q=a%26b+%23c&source=ade"
        );
    }

    #[test]
    fn http_locations_keep_only_normalized_origin_and_path() {
        let fixtures = [
            (
                "https://alice:p%40ss@EXAMPLE.com:443/a%2Fb?q=oauth-code#access-token",
                "https://example.com/a%2Fb",
            ),
            (
                "http://bob:secret@Example.COM:80/path?api_key=hunter2#private",
                "http://example.com/path",
            ),
            (
                "https://user:pass@[2001:db8::1]:8443/private%20path?token=secret#fragment",
                "https://[2001:db8::1]:8443/private%20path",
            ),
            (
                "https://xn--bcher-kva.example:4443/%E2%9C%93?secret=yes#hidden",
                "https://xn--bcher-kva.example:4443/%E2%9C%93",
            ),
        ];

        for (raw, expected) in fixtures {
            let url = Url::parse(raw).unwrap();
            assert_eq!(SanitizedLocation::from(&url).as_str(), expected);
        }
    }

    #[test]
    fn unsafe_and_opaque_schemes_never_reveal_their_payload() {
        let fixtures = [
            ("file:///home/alice/private/oauth.txt", "file:"),
            ("data:text/plain,api-key=hunter2", "data:"),
            (
                "javascript:fetch('https://example.test/?token=secret')",
                "javascript:",
            ),
            (
                "blob:https://alice:pass@example.test/id?token=secret",
                "blob:",
            ),
            ("about:blank?oauth=secret#fragment", "about:"),
            ("custom:alice:password@example.test?token=secret", "custom:"),
        ];

        for (raw, expected) in fixtures {
            assert_eq!(SanitizedLocation::from_input(raw).as_str(), expected);
        }
    }

    #[test]
    fn malformed_input_is_replaced_instead_of_echoed() {
        for raw in [
            "https://alice:password@example .test/private?token=secret",
            "not a url containing api_key=hunter2",
            "http://[invalid]/?oauth=secret",
        ] {
            let presented = SanitizedLocation::from_input(raw);
            assert_eq!(presented.as_str(), SanitizedLocation::INVALID);
            assert!(!presented.as_str().contains("secret"));
            assert!(!presented.as_str().contains("hunter2"));
        }
    }
}
