use super::*;
use serde_json::Value;

fn install_fixture_provider(root: &Path, program: &str, shape: &str) -> std::path::PathBuf {
    let state = root.join("acp").join(shape);
    std::fs::create_dir_all(&state).unwrap();
    let executable = root.join("bin").join(program);
    std::fs::write(
        &executable,
        format!(
            r#"#!/bin/sh
export FAKE_ACP_SHAPE={shape}
export FAKE_ACP_STATE="$HOME/../acp/{shape}"
export FAKE_ACP_LOG="$FAKE_ACP_STATE/frames.log"
exec /usr/bin/python3 "$HOME/../fake-acp-agent.py" "$@"
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    state
}

fn frames(state: &Path) -> Vec<Value> {
    std::fs::read_to_string(state.join("frames.log"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn delegated_acp_provider_confirms_its_mode_before_receiving_a_prompt() {
    if isolated("host::agent_execution::tests::acp_mode::delegated_acp_provider_confirms_its_mode_before_receiving_a_prompt").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    let root = path.parent().unwrap();
    std::fs::write(root.join(".cagents-fixture-root"), "").unwrap();
    std::fs::create_dir(root.join("scripts")).unwrap();
    std::fs::write(
        root.join("scripts/fixture_home.py"),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/fixture_home.py"
        )),
    )
    .unwrap();
    std::fs::write(
        root.join("fake-acp-agent.py"),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/fake-acp-agent.py"
        )),
    )
    .unwrap();

    let agy = install_fixture_provider(root, "agy", "agy");
    let muse = install_fixture_provider(root, "muse", "muse");
    std::fs::write(agy.join("initial-mode"), "auto").unwrap();
    let person = Authority::default();
    let spawn = |authority: &Authority, key: &str, provider, mode: &str, settings| {
        let mut request = request.clone();
        let Destination::New(spec) = &mut request.destination else {
            unreachable!()
        };
        spec.provider = provider;
        spec.permission_mode = Some(mode.into());
        if settings {
            spec.model = Some("gemini-3.8-flash".into());
            spec.reasoning_effort = Some("medium".into());
        }
        request.idempotency_key = key.into();
        submit(&host, authority, request).unwrap()
    };
    let parent = spawn(&person, "parent", Provider::Claude, "default", false);
    assert_eq!(settled(&host, parent.run.id).state, State::Succeeded);
    let caller = agent_authority(&host, parent.agent.id);

    for (provider, mode, state, settings, expected) in [
        (
            Provider::Antigravity,
            "default",
            &agy,
            true,
            &[
                ("model", "gemini-3.8-flash"),
                ("thought_level", "medium"),
                ("mode", "default"),
            ][..],
        ),
        (
            Provider::Muse,
            "manual",
            &muse,
            false,
            &[("mode", "promptUnmatched")][..],
        ),
    ] {
        let child = spawn(&caller, mode, provider, mode, settings);
        let run = settled(&host, child.run.id);
        assert_eq!(run.state, State::Succeeded, "{run:?}");
        let frames = frames(state);
        let prompt = frames
            .iter()
            .position(|frame| frame["method"] == "session/prompt")
            .unwrap();
        let set = frames[..prompt]
            .iter()
            .filter(|frame| frame["method"] == "session/set_config_option")
            .map(|frame| {
                let params = &frame["params"];
                (
                    params["configId"].as_str().unwrap(),
                    params["value"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(set, expected);
        assert!(close(&host, &caller, child.agent.id).unwrap().closed);
    }

    for (marker, contents, error, requested) in [
        ("unavailable-mode", "plan", "does not offer the mode", false),
        ("reject-mode", "", "Mode change refused by fixture", true),
        ("ignore-mode", "", "did not confirm permission mode", true),
    ] {
        std::fs::write(agy.join(marker), contents).unwrap();
        let first = frames(&agy).len();
        let child = spawn(&caller, marker, Provider::Antigravity, "plan", false);
        let run = settled(&host, child.run.id);
        assert_eq!(run.state, State::Failed, "{run:?}");
        assert!(
            run.result
                .diagnostic
                .as_deref()
                .is_some_and(|text| text.contains(error)),
            "{run:?}"
        );
        assert!(run.provider_key.is_none(), "{run:?}");
        let received = frames(&agy);
        assert!(
            !received[first..]
                .iter()
                .any(|frame| frame["method"] == "session/prompt")
        );
        assert_eq!(
            received[first..].iter().any(|frame| {
                frame["method"] == "session/set_config_option" && frame["params"]["value"] == "plan"
            }),
            requested
        );
        assert!(close(&host, &caller, child.agent.id).unwrap().closed);
        std::fs::remove_file(agy.join(marker)).unwrap();
    }
    assert!(close(&host, &person, parent.agent.id).unwrap().closed);
}
