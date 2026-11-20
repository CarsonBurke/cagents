//! Which reasoning levels each model of an ACP session offers.
//!
//! An ACP session's `thought_level` select lists the levels of the model it
//! is on, and of no other. Antigravity also reports the unstable `models`
//! state, whose identifiers join each model with each level it offers -
//! `gemini-4-neon-xhigh`, `gemini-4-argon-high`, and for a model with no
//! levels just `gpt-oss-120b-medium` - which is where the other models'
//! levels are read from. A conversation and an account probe both read a
//! session this way, so a model's picker shows the same levels either way.

/// The levels `model` offers, in the order the session's `levels` - the
/// current model's - lists them.
///
/// `combined` is the session's legacy model identifiers, empty where it
/// reported none. The current model offers the select's levels. Another
/// model offers the levels the combined identifiers name it with, which is
/// none for a model they list on its own, and an unknown - empty - set when
/// there are none to read. Only the select's levels are recognised in an
/// identifier, which keeps a model whose name extends another's, such as
/// `gpt-oss-120b-medium`, from reading as a level of a shorter one.
pub fn model_levels(
    model: &str,
    current: &str,
    levels: &[String],
    combined: &[String],
) -> Vec<String> {
    if model == current {
        return levels.to_vec();
    }
    let named = |level: &String| {
        combined.iter().any(|id| {
            id.strip_prefix(model)
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(|rest| rest == level)
        })
    };
    levels
        .iter()
        .filter(|level| named(level))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn antigravity_s_combined_identifiers_give_each_model_its_own_levels() {
        // As `agy acp` 1.5 opens a session on Gemini 4 Neon.
        let levels = strings(&["low", "medium", "high", "xhigh"]);
        let combined = strings(&[
            "gemini-4-neon-high",
            "gemini-4-neon-xhigh",
            "gemini-4-neon-medium",
            "gemini-4-neon-low",
            "gemini-4-argon-medium",
            "gemini-4-argon-high",
            "gemini-4-argon-low",
            "gemini-pro-orchestrator-neon",
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-medium",
            "gemini-3.8-flash-low",
            "gpt-oss-120b-medium",
        ]);
        let of = |model| model_levels(model, "gemini-4-neon", &levels, &combined);
        assert_eq!(of("gemini-4-neon"), ["low", "medium", "high", "xhigh"]);
        assert_eq!(of("gemini-4-argon"), ["low", "medium", "high"]);
        assert_eq!(of("gemini-3.8-flash"), ["low", "medium", "high"]);
        assert!(of("gemini-pro-orchestrator-neon").is_empty());
        // Its identifier ends like a level, but it is the model's own.
        assert!(of("gpt-oss-120b-medium").is_empty());
    }

    #[test]
    fn without_combined_identifiers_only_the_current_model_s_levels_are_known() {
        // As Muse opens a session: no legacy `models` state.
        let levels = strings(&["xhigh"]);
        assert_eq!(
            model_levels("muse-spark-1.2", "muse-spark-1.2", &levels, &[]),
            ["xhigh"]
        );
        assert!(model_levels("muse-wonder-1.0", "muse-spark-1.2", &levels, &[]).is_empty());
        // Plain identifiers say nothing of levels either, and the select
        // is the current model's whatever they say.
        let plain = strings(&["muse-spark-1.2", "muse-wonder-1.0"]);
        assert_eq!(
            model_levels("muse-spark-1.2", "muse-spark-1.2", &levels, &plain),
            ["xhigh"]
        );
        assert!(model_levels("muse-wonder-1.0", "muse-spark-1.2", &levels, &plain).is_empty());
    }
}
