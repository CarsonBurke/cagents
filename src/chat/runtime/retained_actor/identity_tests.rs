//! Prepared identity is an adapter selection, separate from argv parsing and
//! from provider-confirmed history identity.
use super::*;

fn source() -> (tempfile::TempDir, chat::Session) {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    std::fs::create_dir(directory.path().join("claude")).unwrap();
    let mut source = fixture_source();
    source.launch.recipe.working_directory = directory.path().into();
    source.launch.recipe.environment = BTreeMap::from([
        (
            "HOME".into(),
            directory.path().to_string_lossy().into_owned(),
        ),
        (
            "CLAUDE_CONFIG_DIR".into(),
            directory
                .path()
                .join("claude")
                .to_string_lossy()
                .into_owned(),
        ),
    ]);
    (directory, source)
}

#[test]
fn original_adapter_identity_preserves_option_values_and_fresh_mode() {
    let (_directory, mut source) = source();
    // This is a valid option value, despite looking like an identity flag.
    source.launch.recipe.arguments = vec!["--system-prompt".into(), "--session-id".into()];
    let (spec, adapter) = chat::protocol::create(&source.launch).unwrap();
    let prepared = adapter.prepared_launch_identity().unwrap();
    let selected = serde_json::to_value(&prepared).unwrap();
    assert_eq!(selected["resumed"], false);
    assert_eq!(
        spec.arguments.last().unwrap(),
        &format!("--session-id={}", selected["session_id"].as_str().unwrap())
    );
    let (rebuilt, mut adapter) =
        chat::protocol::claude::create_prepared(&source.launch, prepared).unwrap();
    assert_eq!(rebuilt.arguments, spec.arguments);
    assert!(source.launch.session_id.is_none());
    assert!(
        !adapter
            .initialize()
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(event, chat::Event::Session { .. }))
    );
}

#[test]
fn helper_refuses_prepared_identity_with_wrong_launch_mode_or_resume_id() {
    let (_directory, source) = source();
    let prepared = chat::protocol::create(&source.launch)
        .unwrap()
        .1
        .prepared_launch_identity()
        .unwrap();
    let selected = serde_json::to_value(&prepared).unwrap();
    let mut wrong_mode = selected.clone();
    wrong_mode["resumed"] = serde_json::json!(true);
    assert!(
        chat::protocol::claude::create_prepared(
            &source.launch,
            serde_json::from_value(wrong_mode).unwrap()
        )
        .is_err()
    );
    let mut resumed = source.launch.clone();
    resumed.session_id = Some(uuid::Uuid::new_v4().to_string());
    let mut wrong_resume = selected.clone();
    wrong_resume["resumed"] = serde_json::json!(true);
    assert!(
        chat::protocol::claude::create_prepared(
            &resumed,
            serde_json::from_value(wrong_resume).unwrap()
        )
        .is_err()
    );
    let mut malformed = selected;
    malformed["session_id"] = serde_json::json!("latest");
    assert!(
        chat::protocol::claude::create_prepared(
            &source.launch,
            serde_json::from_value(malformed).unwrap()
        )
        .is_err()
    );
}
