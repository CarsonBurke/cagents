//! A file URL names the machine that holds its file in its host part. This
//! machine is named by no host, by `localhost`, or by its own host name, the
//! spelling `ls --hyperlink`, ripgrep, shells reporting their directory, and
//! some desktops' file managers use. Any other host names a file this machine
//! cannot open.
//!
//! `Url` already reads an empty host and `localhost` as this machine; only the
//! machine's own name needs a lookup, which [`localize`] makes.
use url::Url;

/// This machine's name as a file URL's host spells it, which is how
/// [`localize`] compares it: lowercase, non-ASCII labels in punycode, an
/// all-numeric name as the IPv4 address `Url` reads it as. `None` when the
/// machine has no name a URL host can hold, or one, like `localhost`, that a
/// file URL reads as no host at all.
///
/// Read afresh on each call: the name can change while a long-lived process
/// runs, and the programs that put it into URLs read it when they run.
pub(crate) fn host_name() -> Option<String> {
    url_host(&system_host_name()?)
}

/// This machine's name as `gethostname` reports it.
fn system_host_name() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the pointer and length describe `buffer`, which outlives the
    // call; gethostname writes at most that many bytes into it.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return None;
    }
    // POSIX leaves a truncated name unterminated, and a truncated name is
    // not this machine's, so it matches nothing rather than a prefix.
    let name = std::ffi::CStr::from_bytes_until_nul(&buffer).ok()?;
    name.to_str().ok().map(str::to_owned)
}

/// Spells `name` as the host of a file URL naming it: parsed as `Url` parses
/// a file URL's host, which also refuses an empty name and anything a host
/// cannot hold.
fn url_host(name: &str) -> Option<String> {
    url::Host::parse(name)
        .ok()
        .map(|host| host.to_string())
        .filter(|host| host != "localhost")
}

/// Removes the host from a file URL that names this machine as `local_host`,
/// leaving the same file in the host-less spelling [`Url::to_file_path`]
/// accepts. Returns whether it did; any other URL is left as it is.
///
/// `local_host` is asked only for a file URL that names a host, so the common
/// host-less links never look this machine's name up. It must be spelled as
/// [`host_name`] spells it; pass that or a name it returned earlier. Names
/// compare without regard to ASCII case, as DNS names do.
pub(crate) fn localize<S: AsRef<str>>(
    url: &mut Url,
    local_host: impl FnOnce() -> Option<S>,
) -> bool {
    let Some(host) = url.host_str().filter(|_| url.scheme() == "file") else {
        return false;
    };
    if !local_host().is_some_and(|local| host.eq_ignore_ascii_case(local.as_ref())) {
        return false;
    }
    // Not `Url::set_host(None)`: in url 2.5 it drops the host from the text
    // but still reports an empty one, which `to_file_path` refuses. The rest
    // of the URL is already encoded, so it reads back exactly as it was.
    match Url::parse(&format!("file://{}", &url[url::Position::BeforePath..])) {
        Ok(local) => {
            *url = local;
            true
        }
        Err(_) => false,
    }
}

/// This machine's name as `gethostname` reports it, and as programs put it
/// into the links they print, when [`localize`] will recognize it as this
/// machine's: `None` exactly when [`host_name`] is. Tests that need a real
/// name skip without one.
#[cfg(test)]
pub(crate) fn test_host_name() -> Option<String> {
    system_host_name().filter(|name| url_host(name).is_some())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn localized(text: &str, local_host: Option<&str>) -> (bool, String) {
        let mut url = Url::parse(text).unwrap();
        let changed = localize(&mut url, || local_host);
        (changed, url.into())
    }

    #[test]
    fn this_machines_name_is_removed_and_the_path_kept() {
        assert_eq!(
            localized(
                "file://Workstation/tmp/a%20b/%E7%95%8C.md?q=1#L3",
                Some("workstation")
            ),
            (true, "file:///tmp/a%20b/%E7%95%8C.md?q=1#L3".into())
        );
        let mut url = Url::parse("file://workstation/tmp/a%20b").unwrap();
        assert!(localize(&mut url, || Some("WORKSTATION")));
        assert_eq!(url.to_file_path(), Ok(PathBuf::from("/tmp/a b")));
    }

    #[test]
    fn hosts_url_already_reads_as_local_are_left_alone_without_a_lookup() {
        for text in [
            "file:///tmp/a",
            "file://localhost/tmp/a",
            "file://LOCALHOST/tmp/a",
        ] {
            let mut url = Url::parse(text).unwrap();
            assert!(!localize(&mut url, || -> Option<&str> {
                panic!("{text} has no host to compare")
            }));
            assert_eq!(url.as_str(), "file:///tmp/a", "{text}");
        }
    }

    #[test]
    fn another_host_stays_unopenable() {
        let mut url = Url::parse("file://elsewhere/tmp/a").unwrap();
        assert!(!localize(&mut url, || Some("workstation")));
        assert_eq!(url.as_str(), "file://elsewhere/tmp/a");
        assert!(url.to_file_path().is_err());
        // Without a name for this machine, only the spellings every source
        // agrees on are local.
        assert_eq!(
            localized("file://workstation/tmp/a", None),
            (false, "file://workstation/tmp/a".into())
        );
    }

    #[test]
    fn only_file_urls_are_localized() {
        assert_eq!(
            localized("https://workstation/a", Some("workstation")),
            (false, "https://workstation/a".into())
        );
    }

    #[test]
    fn a_name_is_spelled_as_a_parsed_file_url_spells_its_host() {
        for (name, spelled) in [
            ("Workstation", Some("workstation")),
            ("1234", Some("0.0.4.210")),
            ("café", Some("xn--caf-dma")),
            ("LocalHost", None),
            ("", None),
            ("a/b", None),
            ("a b", None),
        ] {
            assert_eq!(url_host(name).as_deref(), spelled, "{name:?}");
        }
    }

    #[test]
    fn numeric_and_international_names_match_the_links_that_carry_them() {
        // `ls --hyperlink` writes the name as `gethostname` reports it.
        for (name, link) in [
            ("1234", "file://1234/tmp/a%20b"),
            ("café", "file://café/tmp/a%20b"),
            ("café", "file://CAF%C3%A9/tmp/a%20b"),
        ] {
            let mut url = Url::parse(link).unwrap();
            assert!(localize(&mut url, || url_host(name)), "{link}");
            assert_eq!(url.to_file_path(), Ok(PathBuf::from("/tmp/a b")), "{link}");
        }
        let mut url = Url::parse("file://1235/tmp/a").unwrap();
        assert!(!localize(&mut url, || url_host("1234")));
    }

    #[test]
    fn the_test_name_is_present_exactly_when_the_spelled_one_is() {
        assert_eq!(test_host_name().is_some(), host_name().is_some());
    }
}
