//! A file URL names the machine that holds its file in its host part. This
//! machine is named by no host, by `localhost`, or by its own host name, the
//! spelling `ls --hyperlink`, ripgrep, shells reporting their directory, and
//! some desktops' file managers use. Any other host names a file this machine
//! cannot open.
//!
//! `Url` already reads an empty host and `localhost` as this machine; only the
//! machine's own name needs a lookup, which [`localize`] makes.
use url::Url;

/// This machine's host name, as `gethostname` reports it.
///
/// Read afresh on each call: the name can change while a long-lived process
/// runs, and the programs that put it into URLs read it when they run.
pub(crate) fn host_name() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the pointer and length describe `buffer`, which outlives the
    // call; gethostname writes at most that many bytes into it.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return None;
    }
    // POSIX leaves a truncated name unterminated, and a truncated name is
    // not this machine's, so it matches nothing rather than a prefix.
    let name = std::ffi::CStr::from_bytes_until_nul(&buffer).ok()?;
    name.to_str()
        .ok()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// Removes the host from a file URL that names this machine as `local_host`,
/// leaving the same file in the host-less spelling [`Url::to_file_path`]
/// accepts. Returns whether it did; any other URL is left as it is.
///
/// Host names compare without regard to ASCII case, as DNS names do. `Url`
/// lowercases the host it parses, while `gethostname` may not.
pub(crate) fn localize(url: &mut Url, local_host: Option<&str>) -> bool {
    if url.scheme() != "file"
        || !url
            .host_str()
            .zip(local_host)
            .is_some_and(|(host, local)| host.eq_ignore_ascii_case(local))
    {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn localized(text: &str, local_host: Option<&str>) -> (bool, String) {
        let mut url = Url::parse(text).unwrap();
        let changed = localize(&mut url, local_host);
        (changed, url.into())
    }

    #[test]
    fn this_machines_name_is_removed_and_the_path_kept() {
        assert_eq!(
            localized(
                "file://Workstation/tmp/a%20b/%E7%95%8C.md#L3",
                Some("workstation")
            ),
            (true, "file:///tmp/a%20b/%E7%95%8C.md#L3".into())
        );
        let mut url = Url::parse("file://workstation/tmp/a%20b").unwrap();
        assert!(localize(&mut url, Some("WORKSTATION")));
        assert_eq!(url.to_file_path(), Ok(PathBuf::from("/tmp/a b")));
    }

    #[test]
    fn hosts_url_already_reads_as_local_are_left_alone() {
        for text in [
            "file:///tmp/a",
            "file://localhost/tmp/a",
            "file://LOCALHOST/tmp/a",
        ] {
            assert_eq!(
                localized(text, Some("workstation")),
                (false, "file:///tmp/a".into()),
                "{text}"
            );
        }
    }

    #[test]
    fn another_host_stays_unopenable() {
        let mut url = Url::parse("file://elsewhere/tmp/a").unwrap();
        assert!(!localize(&mut url, Some("workstation")));
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
    fn this_machine_has_a_name() {
        assert!(host_name().is_some_and(|name| !name.contains('\0')));
    }
}
