// The version an executable states, shared by the root package's
// `build_version` module and `cagents-dictate`, which does not depend on the
// root package but must state the same version as the rest of the matched set.
// Each includes this file.

/// The metadata the packaging script or build gate supplied, or else the
/// checkout's. A supplied value that is not valid build metadata gives the
/// bare product version rather than the checkout's.
fn resolve(product: &str, supplied: Option<&str>, checkout: Option<&str>) -> String {
    let metadata = match supplied.map(str::trim) {
        Some(supplied) if !supplied.is_empty() => Some(supplied),
        _ => checkout.map(str::trim),
    };
    match metadata.filter(|metadata| valid_metadata(metadata)) {
        Some(metadata) => format!("{product}+{metadata}"),
        None => product.to_owned(),
    }
}

/// Semantic-versioning build metadata: dot-separated ASCII alphanumerics and
/// hyphens. Anything else would make `--version` print something the
/// packaging step's own version check rejects.
fn valid_metadata(metadata: &str) -> bool {
    !metadata.is_empty()
        && metadata.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}
