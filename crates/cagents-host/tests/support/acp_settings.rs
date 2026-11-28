use serde_json::Value;

/// Check the exact settings changes and turns against one live ACP session.
pub(crate) fn assert_turns(frames: &[Value], settings: &[(&str, &str)], prompts: &[&str]) {
    let traffic: Vec<_> = frames
        .iter()
        .filter(|frame| {
            matches!(
                frame["method"].as_str(),
                Some(
                    "session/set_config_option"
                        | "session/set_mode"
                        | "session/set_model"
                        | "session/load"
                        | "session/resume"
                        | "session/prompt"
                )
            )
        })
        .collect();
    assert_eq!(traffic.len(), settings.len() + prompts.len(), "{traffic:?}");
    let (configuration, turns) = traffic.split_at(settings.len());
    let session = turns[0]["params"]["sessionId"].as_str().unwrap();
    for (frame, (option, value)) in configuration.iter().zip(settings) {
        assert_eq!(frame["method"], "session/set_config_option", "{frame}");
        assert_eq!(frame["params"]["sessionId"], session, "{frame}");
        assert_eq!(frame["params"]["configId"], *option, "{frame}");
        assert_eq!(frame["params"]["value"], *value, "{frame}");
    }
    for (frame, text) in turns.iter().zip(prompts) {
        assert_eq!(frame["method"], "session/prompt", "{frame}");
        assert_eq!(frame["params"]["sessionId"], session, "{frame}");
        let blocks = frame["params"]["prompt"].as_array().unwrap();
        assert!(
            blocks.iter().any(|block| block["text"]
                .as_str()
                .is_some_and(|body| body.contains(text))),
            "{frame}"
        );
    }
}
