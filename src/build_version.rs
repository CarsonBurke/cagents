//! The version an executable states with `stamp_version!`, resolved once per
//! process.

use std::sync::OnceLock;

/// The product version without build metadata, as a release reports it.
pub const PRODUCT: &str = env!("CARGO_PKG_VERSION");
const UNSTAMPED: &str = concat!(env!("CARGO_PKG_VERSION"), "+unstamped");

static VERSION: OnceLock<&'static str> = OnceLock::new();

/// Records this process's version from the metadata its executable was
/// compiled with. Stamping again with the same metadata does nothing.
///
/// # Panics
///
/// When the process already has a different version: a second executable's
/// stamp, or [`current`] having answered `+unstamped` before the stamp.
pub fn stamp(supplied: Option<&str>, checkout: Option<&str>) {
    record(&VERSION, resolve(PRODUCT, supplied, checkout));
}

fn record(cell: &OnceLock<&'static str>, version: String) {
    let recorded = *cell.get_or_init(|| Box::leak(version.clone().into_boxed_str()));
    assert_eq!(
        recorded, version,
        "this process already reported another version"
    );
}

pub(crate) fn current() -> &'static str {
    VERSION.get_or_init(|| UNSTAMPED)
}

include!("../build/stated_version.rs");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supplied_metadata_wins_over_the_checkout() {
        assert_eq!(
            resolve("0.1.3", Some("local.3d80e6d"), Some("local.1111111.dirty")),
            "0.1.3+local.3d80e6d"
        );
        assert_eq!(
            resolve("0.1.3", Some("  local.3d80e6d\n"), None),
            "0.1.3+local.3d80e6d"
        );
    }

    #[test]
    fn empty_supplied_metadata_defers_to_the_checkout() {
        for supplied in [None, Some(""), Some("  ")] {
            assert_eq!(
                resolve("0.1.3", supplied, Some("local.3d80e6d.dirty")),
                "0.1.3+local.3d80e6d.dirty"
            );
            assert_eq!(resolve("0.1.3", supplied, None), "0.1.3");
        }
    }

    #[test]
    fn invalid_metadata_gives_the_bare_product_version() {
        for invalid in ["local..3d80e6d", "local.3d80e6d+x", "local 3d80e6d", "."] {
            assert_eq!(
                resolve("0.1.3", Some(invalid), Some("local.3d80e6d")),
                "0.1.3"
            );
            assert_eq!(resolve("0.1.3", None, Some(invalid)), "0.1.3");
        }
    }

    #[test]
    fn a_process_keeps_the_version_it_first_reported() {
        let cell = OnceLock::new();
        record(&cell, "0.1.3+local.3d80e6d".into());
        record(&cell, "0.1.3+local.3d80e6d".into());
        assert_eq!(cell.get(), Some(&"0.1.3+local.3d80e6d"));
        let other = std::panic::catch_unwind(|| record(&cell, "0.1.3+local.1111111".into()));
        assert!(
            other.is_err(),
            "a second, different version must not replace the first"
        );
        assert_eq!(cell.get(), Some(&"0.1.3+local.3d80e6d"));
    }

    #[test]
    fn every_result_is_a_semantic_version() {
        for version in [
            UNSTAMPED,
            &resolve(PRODUCT, Some("local.3d80e6d.dirty"), None),
        ] {
            semver::Version::parse(version).unwrap();
        }
    }
}
