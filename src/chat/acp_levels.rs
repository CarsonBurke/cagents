//! Which reasoning levels each model of an ACP session offers.
//!
//! An ACP session's `thought_level` select lists the levels of the model it
//! is on, and of no other. Antigravity also reports the unstable `models`
//! state, whose identifiers join each model with each level it offers -
//! `gemini-4-neon-xhigh`, `gemini-4-argon-high`, and for a model with no
//! levels just `gpt-oss-120b-medium` - which is where the other models'
//! levels are read from. A conversation and an account probe both read a
//! session this way, so a model's picker shows the same levels either way.

/// Levels in the order a picker lists them, from the least effort up.
const ORDER: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The levels `model` offers.
///
/// The current model offers the select's `levels`, in its order. Another
/// model offers the levels its `combined` identifiers - the session's legacy
/// model identifiers, empty where it reported none - join it with: none for
/// a model they list on its own, and an unknown, empty, set when there are
/// none to read. Those are read whatever the current model offers, since
/// models differ in their levels, and listed from the least effort up.
///
/// An identifier names a level only where what follows `model-` is one word
/// and the identifier is not itself one of the session's `models`, which
/// keeps a model whose name extends another's, such as
/// `gpt-oss-120b-medium` beside a `gpt-oss-120b`, from reading as a level of
/// the shorter one.
pub fn model_levels(
    model: &str,
    current: &str,
    levels: &[String],
    models: &[String],
    combined: &[String],
) -> Vec<String> {
    if model == current {
        return levels.to_vec();
    }
    let mut named = Vec::<String>::new();
    for id in combined {
        let Some(level) = id
            .strip_prefix(model)
            .and_then(|rest| rest.strip_prefix('-'))
            .filter(|level| !level.is_empty() && !level.contains('-'))
        else {
            continue;
        };
        if !models.contains(id) && !named.iter().any(|known| known == level) {
            named.push(level.to_owned());
        }
    }
    // Stable, so a level of no known order keeps the select's place for it,
    // then the identifiers'.
    let place = |level: &String| {
        (
            ORDER
                .iter()
                .position(|known| known == level)
                .unwrap_or(ORDER.len()),
            levels
                .iter()
                .position(|known| known == level)
                .unwrap_or(levels.len()),
        )
    };
    named.sort_by_key(place);
    named
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    /// As `agy acp` 1.5.12 lists its models and their combined identifiers.
    fn antigravity() -> (Vec<String>, Vec<String>) {
        let models = strings(&[
            "gemini-4-neon",
            "gemini-4-argon",
            "gemini-4-hydrogen",
            "gemini-pro-orchestrator-neon",
            "gemini-pro-orchestrator-g1-pro",
            "gemini-3.8-flash",
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "gpt-oss-120b-medium",
        ]);
        let combined = strings(&[
            "gemini-4-neon-high",
            "gemini-4-neon-xhigh",
            "gemini-4-neon-medium",
            "gemini-4-neon-low",
            "gemini-4-argon-medium",
            "gemini-4-argon-high",
            "gemini-4-argon-low",
            "gemini-4-hydrogen-high",
            "gemini-4-hydrogen-xhigh",
            "gemini-4-hydrogen-medium",
            "gemini-4-hydrogen-low",
            "gemini-pro-orchestrator-neon",
            "gemini-pro-orchestrator-g1-pro",
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-medium",
            "gemini-3.8-flash-low",
            "claude-opus-5-5-low",
            "claude-opus-5-5-medium",
            "claude-opus-5-5-high",
            "claude-sonnet-5-5-low",
            "claude-sonnet-5-5-medium",
            "claude-sonnet-5-5-high",
            "gpt-oss-120b-medium",
        ]);
        (models, combined)
    }

    #[test]
    fn antigravity_s_combined_identifiers_give_each_model_its_own_levels() {
        let (models, combined) = antigravity();
        let levels = strings(&["low", "medium", "high", "xhigh"]);
        let of = |model| model_levels(model, "gemini-4-neon", &levels, &models, &combined);
        assert_eq!(of("gemini-4-neon"), ["low", "medium", "high", "xhigh"]);
        assert_eq!(of("gemini-4-argon"), ["low", "medium", "high"]);
        assert_eq!(of("gemini-4-hydrogen"), ["low", "medium", "high", "xhigh"]);
        assert_eq!(of("claude-opus-5-5"), ["low", "medium", "high"]);
        assert!(of("gemini-pro-orchestrator-neon").is_empty());
        // Its identifier ends like a level, but it is the model's own.
        assert!(of("gpt-oss-120b-medium").is_empty());
    }

    /// The current model's levels do not limit another's: on Argon, Neon
    /// still offers Extra high, and on a model with no levels at all, every
    /// other model keeps its own.
    #[test]
    fn another_model_s_levels_do_not_depend_on_the_current_one_s() {
        let (models, combined) = antigravity();
        let on_argon = strings(&["low", "medium", "high"]);
        assert_eq!(
            model_levels(
                "gemini-4-neon",
                "gemini-4-argon",
                &on_argon,
                &models,
                &combined
            ),
            ["low", "medium", "high", "xhigh"]
        );
        for current in ["gemini-pro-orchestrator-neon", "gpt-oss-120b-medium"] {
            assert_eq!(
                model_levels("gemini-4-hydrogen", current, &[], &models, &combined),
                ["low", "medium", "high", "xhigh"]
            );
            assert!(model_levels(current, current, &[], &models, &combined).is_empty());
        }
        // A model named like a longer model's prefix takes none of its words.
        let models = strings(&["gpt-oss-120b", "gpt-oss-120b-medium"]);
        let combined = strings(&["gpt-oss-120b-low", "gpt-oss-120b-medium"]);
        assert_eq!(
            model_levels(
                "gpt-oss-120b",
                "gpt-oss-120b-medium",
                &[],
                &models,
                &combined
            ),
            ["low"]
        );
    }

    #[test]
    fn without_combined_identifiers_only_the_current_model_s_levels_are_known() {
        // As Muse opens a session: no legacy `models` state.
        let models = strings(&["muse-spark-1.2", "muse-wonder-1.0"]);
        let levels = strings(&["xhigh"]);
        assert_eq!(
            model_levels("muse-spark-1.2", "muse-spark-1.2", &levels, &models, &[]),
            ["xhigh"]
        );
        assert!(
            model_levels("muse-wonder-1.0", "muse-spark-1.2", &levels, &models, &[]).is_empty()
        );
        // Plain identifiers say nothing of levels either, and the select
        // is the current model's whatever they say.
        assert_eq!(
            model_levels(
                "muse-spark-1.2",
                "muse-spark-1.2",
                &levels,
                &models,
                &models
            ),
            ["xhigh"]
        );
        assert!(
            model_levels(
                "muse-wonder-1.0",
                "muse-spark-1.2",
                &levels,
                &models,
                &models
            )
            .is_empty()
        );
    }
}
