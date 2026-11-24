//! Which reasoning levels the models of an ACP session offer, where the
//! agent names them.
//!
//! A session's reasoning option lists levels for the model it is on. agy
//! also reports the unstable `models` state, whose combined identifiers join
//! each model with each level it offers, such as `gemini-4-neon-xhigh`, and
//! name a model that has none on its own. A conversation and an account
//! probe both read the other models' levels from there, so a model's picker
//! offers the same levels either way.

/// `id` read as an offered model and one of `levels` joined by a hyphen.
pub fn split<'i, 'l, L: AsRef<str>>(
    id: &'i str,
    levels: &'l [L],
    offers_model: impl Fn(&str) -> bool,
) -> Option<(&'i str, &'l str)> {
    levels.iter().map(AsRef::as_ref).find_map(|level| {
        let model = id.strip_suffix(level)?.strip_suffix('-')?;
        offers_model(model).then_some((model, level))
    })
}

/// Whether `combined`, a session's `models` identifiers, name its models'
/// levels: whether any joins an offered model with one of `levels`.
pub fn names_levels<C: AsRef<str>, L: AsRef<str>>(
    combined: &[C],
    levels: &[L],
    offers_model: impl Fn(&str) -> bool,
) -> bool {
    combined
        .iter()
        .any(|id| split(id.as_ref(), levels, &offers_model).is_some())
}

/// The ones of `levels` that `combined` joins `model` with, in `levels`'
/// order: the levels `model` offers, where `combined` names them.
pub fn joined<L: AsRef<str>, C: AsRef<str>>(
    model: &str,
    levels: &[L],
    combined: &[C],
) -> Vec<String> {
    levels
        .iter()
        .map(AsRef::as_ref)
        .filter(|level| {
            combined.iter().any(|id| {
                id.as_ref()
                    .strip_suffix(*level)
                    .and_then(|rest| rest.strip_suffix('-'))
                    == Some(model)
            })
        })
        .map(str::to_owned)
        .collect()
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;

    #[test]
    fn combined_identifiers_name_each_model_s_own_levels() {
        let models = ["gemini-4-neon", "gemini-3.8-flash", "gpt-oss-120b-medium"];
        let levels = ["low", "medium", "high", "xhigh"];
        let combined = [
            "gemini-4-neon-low",
            "gemini-4-neon-high",
            "gemini-4-neon-xhigh",
            "gemini-3.8-flash-low",
            "gpt-oss-120b-medium",
        ];
        let offered = |model: &str| models.contains(&model);
        assert!(names_levels(&combined, &levels, offered));
        assert_eq!(
            split("gemini-4-neon-xhigh", &levels, offered),
            Some(("gemini-4-neon", "xhigh"))
        );
        // A model whose own identifier ends like a level is not one.
        assert_eq!(split("gpt-oss-120b-medium", &levels, offered), None);
        assert_eq!(
            joined("gemini-4-neon", &levels, &combined),
            ["low", "high", "xhigh"]
        );
        assert_eq!(joined("gemini-3.8-flash", &levels, &combined), ["low"]);
        assert!(joined("gpt-oss-120b-medium", &levels, &combined).is_empty());
        // Identifiers that are only models name no levels.
        assert!(!names_levels(&models, &levels, offered));
    }
}
