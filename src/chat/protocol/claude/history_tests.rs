use super::*;
use std::io::Write;

const SESSION: &str = "00000000-0000-4000-8000-000000000001";
const OTHER_SESSION: &str = "00000000-0000-4000-8000-000000000002";

struct Fixture {
    _root: tempfile::TempDir,
    launch: Launch,
    path: PathBuf,
    bytes: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("removed-project");
        std::fs::create_dir(&cwd).unwrap();
        let home = root.path().join("claude-home");
        let project: String = cwd
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let directory = home.join("projects").join(project);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{SESSION}.jsonl"));
        let mut bytes = Vec::new();
        for index in 0..130 {
            writeln!(
                bytes,
                "{}",
                json!({"type":"user","uuid":format!("row-{index}"),
                "parentUuid":(index>0).then(||format!("row-{}",index-1)),"sessionId":SESSION,
                "message":{"role":"user","content":format!("Saved prompt {index}")}})
            )
            .unwrap();
        }
        std::fs::write(&path, &bytes).unwrap();
        std::fs::remove_dir(&cwd).unwrap();
        let launch = Launch {
            provider: Provider::Claude,
            session_id: Some(SESSION.into()),
            recipe: crate::model::LaunchRecipe {
                program: "claude".into(),
                arguments: Vec::new(),
                working_directory: cwd,
                environment: std::collections::BTreeMap::from([(
                    "CLAUDE_CONFIG_DIR".into(),
                    home.to_string_lossy().into_owned(),
                )]),
                unset_environment: Default::default(),
            },
        };
        Self {
            _root: root,
            launch,
            path,
            bytes,
        }
    }

    fn replace_session(&self) {
        let replacement = String::from_utf8(self.bytes.clone())
            .unwrap()
            .replace(SESSION, OTHER_SESSION);
        assert_eq!(replacement.len(), self.bytes.len());
        // Preserve the same inode, valid JSON and indexed offsets.
        std::fs::write(&self.path, replacement).unwrap();
    }
}

#[test]
fn initialize_refuses_history_whose_session_changed_after_open() {
    let fixture = Fixture::new();
    let (_, mut adapter) = create(&fixture.launch).unwrap();
    fixture.replace_session();
    let error = adapter
        .initialize()
        .err()
        .expect("foreign history must not initialize");
    assert!(error.contains("session identity"), "{error}");
}

#[test]
fn earlier_history_refuses_changed_session_and_can_retry_the_original_source() {
    let fixture = Fixture::new();
    let (_, mut adapter) = create(&fixture.launch).unwrap();
    let initial = adapter.initialize().unwrap();
    assert!(initial.events.iter().any(
        |event| matches!(event, Event::History { items, has_earlier: true } if items.len() == 100)
    ));
    fixture.replace_session();
    let error = adapter
        .command(Command::LoadEarlier)
        .err()
        .expect("foreign older page must be refused");
    assert!(error.contains("session identity"), "{error}");
    std::fs::write(&fixture.path, &fixture.bytes).unwrap();
    let earlier = adapter.command(Command::LoadEarlier).unwrap();
    assert!(earlier.events.iter().any(
        |event| matches!(event, Event::History { items, has_earlier: false } if items.len() == 30)
    ));
    assert!(earlier.events.iter().any(|event| matches!(event, Event::History { items, .. } if items.iter().any(|item| matches!(&item.content, ItemContent::Text { text, .. } if text == "Saved prompt 0")))));
}

#[test]
fn initialize_refuses_a_changed_off_page_tool_dependency() {
    let mut fixture = Fixture::new();
    let mut frames: Vec<Value> = String::from_utf8(fixture.bytes.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    frames[0]["type"] = json!("assistant");
    frames[0]["message"] = json!({"role":"assistant","content":[{"type":"tool_use","id":"old-call","name":"Read","input":{"file_path":"/original"}}]});
    frames[129]["message"] = json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"old-call","content":"original result"}]});
    fixture.bytes.clear();
    for frame in frames {
        writeln!(fixture.bytes, "{frame}").unwrap();
    }
    std::fs::write(&fixture.path, &fixture.bytes).unwrap();
    let (_, mut adapter) = create(&fixture.launch).unwrap();
    // The first record is outside the newest 100-row page, but its tool call
    // must be loaded to interpret the result in that page.
    let replacement =
        String::from_utf8(fixture.bytes.clone())
            .unwrap()
            .replacen(SESSION, OTHER_SESSION, 1);
    assert_eq!(replacement.len(), fixture.bytes.len());
    std::fs::write(&fixture.path, replacement).unwrap();
    let error = adapter
        .initialize()
        .err()
        .expect("foreign off-page tool dependency must be refused");
    assert!(error.contains("session identity"), "{error}");
}

#[test]
fn legacy_history_without_record_session_ids_remains_readable() {
    let fixture = Fixture::new();
    let mut bytes = Vec::new();
    for line in String::from_utf8(fixture.bytes.clone()).unwrap().lines() {
        let mut frame: Value = serde_json::from_str(line).unwrap();
        frame.as_object_mut().unwrap().remove("sessionId");
        writeln!(bytes, "{frame}").unwrap();
    }
    std::fs::write(&fixture.path, bytes).unwrap();
    let (_, mut adapter) = create(&fixture.launch).unwrap();
    let initial = adapter.initialize().unwrap();
    assert!(initial.events.iter().any(
        |event| matches!(event, Event::History { items, has_earlier: true } if items.len() == 100)
    ));
    let earlier = adapter.command(Command::LoadEarlier).unwrap();
    assert!(earlier.events.iter().any(
        |event| matches!(event, Event::History { items, has_earlier: false } if items.len() == 30)
    ));
}

pub(crate) struct Namespaces {
    pub(crate) root: tempfile::TempDir,
    pub(crate) session: crate::chat::Session,
    launch: Launch,
    original: PathBuf,
    replacement: PathBuf,
    transcript: PathBuf,
    decoy: PathBuf,
}

impl Namespaces {
    pub(crate) fn new(managed: bool, decoy: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        let replacement = root.path().join("replacement");
        let config = root.path().join("account");
        for path in [&original, &replacement, &config] {
            std::fs::create_dir(path).unwrap();
        }
        let original = original.canonicalize().unwrap();
        let replacement = replacement.canonicalize().unwrap();
        let config = config.canonicalize().unwrap();
        // Use the real producer and its captured account selection. SystemAt
        // is the selected daemon's system account, without changing test-wide
        // HOME or reading the developer's actual transcript store.
        let account = crate::account::Account {
            provider: Provider::Claude,
            home: if managed {
                crate::account::AccountHome::Managed {
                    key: uuid::Uuid::new_v4(),
                    path: config.clone(),
                }
            } else {
                crate::account::AccountHome::SystemAt {
                    path: config.clone(),
                }
            },
        };
        let mut session = crate::chat::fresh_session(
            Provider::Claude,
            crate::model::TerminalId::new(),
            original.clone(),
            &account,
        )
        .unwrap();
        assert_eq!(
            session.launch.recipe.environment["CLAUDE_CONFIG_DIR"],
            config.to_str().unwrap()
        );
        session.launch.session_id = Some(SESSION.into());
        let transcript = write_namespace(&config, &original, "A");
        let decoy_path = namespace_path(&config, &replacement);
        if decoy {
            assert_eq!(write_namespace(&config, &replacement, "B"), decoy_path);
        }
        std::fs::remove_dir(&original).unwrap();
        Self {
            root,
            launch: session.launch.clone(),
            session,
            original,
            replacement,
            transcript,
            decoy: decoy_path,
        }
    }

    pub(crate) fn replace_cwd(&self) {
        std::os::unix::fs::symlink(&self.replacement, &self.original).unwrap();
    }
}

fn namespace_path(config: &Path, cwd: &Path) -> PathBuf {
    let project: String = cwd
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    config
        .join("projects")
        .join(project)
        .join(format!("{SESSION}.jsonl"))
}

fn write_namespace(config: &Path, cwd: &Path, label: &str) -> PathBuf {
    let path = namespace_path(config, cwd);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = File::create(&path).unwrap();
    for index in 0..220 {
        writeln!(
            file,
            "{}",
            json!({"type":"user","uuid":format!("row-{index:03}"),
            "parentUuid":(index>0).then(||format!("row-{:03}",index-1)),
            "sessionId":SESSION,"cwd":cwd,
            "message":{"role":"user","content":format!("Source {label} row {index:03}")}})
        )
        .unwrap();
    }
    writeln!(
        file,
        "{}",
        json!({"type":"custom-title","sessionId":SESSION,"cwd":cwd,
        "customTitle":format!("Source {label} title")})
    )
    .unwrap();
    path
}

fn assert_all_history(adapter: &mut dyn Adapter, expected: &str) {
    let initial = adapter.initialize().unwrap();
    assert!(initial.writes.is_empty());
    assert!(initial.events.iter().any(|event| matches!(event,
        Event::Session { id, title: Some(title) }
        if id == SESSION && title == &format!("Source {expected} title"))));
    assert!(
        initial
            .events
            .iter()
            .any(|event| matches!(event, Event::ReadOnlyHistory { .. }))
    );
    let page = |step: Step| {
        let mut rows = Vec::new();
        let mut earlier = false;
        for event in step.events {
            if let Event::History { items, has_earlier } = event {
                earlier = has_earlier;
                rows.extend(items.into_iter().filter_map(|item| match item.content {
                    ItemContent::Text { text, .. } => Some(text),
                    _ => None,
                }));
            }
        }
        (rows, earlier)
    };
    let (mut rows, mut earlier) = page(initial);
    let mut pages = 1;
    while earlier {
        let step = adapter.command(Command::LoadEarlier).unwrap();
        assert!(step.writes.is_empty());
        let (mut older, has_earlier) = page(step);
        older.append(&mut rows);
        rows = older;
        earlier = has_earlier;
        pages += 1;
        assert!(pages <= 3);
    }
    assert_eq!(pages, 3);
    assert_eq!(
        rows,
        (0..220)
            .map(|index| format!("Source {expected} row {index:03}"))
            .collect::<Vec<_>>()
    );
    assert!(
        adapter
            .command(Command::Prompt {
                text: "must not run".into(),
                attachments: Vec::new(),
                mode: PromptMode::Send,
            })
            .unwrap_err()
            .contains("read-only")
    );
    assert!(
        adapter
            .command(Command::Interrupt)
            .unwrap_err()
            .contains("read-only")
    );
    assert!(adapter.command(Command::Refresh).unwrap().writes.is_empty());
}

#[test]
fn captured_system_and_managed_accounts_keep_deleted_cwd_history_readable() {
    for managed in [false, true] {
        let fixture = Namespaces::new(managed, true);
        let issued = fixture.launch.clone();
        let probe = history_open_probe(fixture.root.path());
        let (spec, mut adapter) = create_history(&fixture.launch).unwrap();
        assert!(matches!(
            spec.transport,
            super::super::TransportSpec::History
        ));
        assert_all_history(adapter.as_mut(), "A");
        assert_eq!(probe.paths(), vec![fixture.transcript]);
        assert_eq!(fixture.launch, issued);
    }
}

#[test]
fn fresh_system_account_keeps_implicit_home_history_readable() {
    const CHILD: &str = "CAGENTS_HISTORY_SOURCE_SYSTEM_FIXTURE";
    if std::env::var_os(CHILD).is_none() {
        // Real Account::System selection depends on process environment. An
        // isolated test process exercises the producer without changing the
        // parallel test runner's environment or selecting its real account.
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "chat::protocol::claude::history_tests::fresh_system_account_keeps_implicit_home_history_readable", "--nocapture"])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove(crate::account::SELECTION_MARKER)
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let fixture = Namespaces::new(false, false);
    let config = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".claude");
    std::fs::create_dir(&config).unwrap();
    std::fs::create_dir(&fixture.original).unwrap();
    let mut session = crate::chat::fresh_session(
        Provider::Claude,
        crate::model::TerminalId::new(),
        fixture.original.canonicalize().unwrap(),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    assert_eq!(
        session.launch.recipe.environment["CLAUDE_CONFIG_DIR"],
        config.to_str().unwrap()
    );
    session.launch.session_id = Some(SESSION.into());
    write_namespace(&config, &fixture.original, "A");
    std::fs::remove_dir(&fixture.original).unwrap();
    let issued = session.clone();
    let (_, mut adapter) = create_history(&session.launch).unwrap();
    assert_all_history(adapter.as_mut(), "A");
    assert_eq!(session, issued);
    // The process itself has readable A at HOME/.claude. If either removed
    // or explicitly unset recorded selectors fall back to that environment,
    // construction would succeed instead of returning this exact refusal.
    let probe = history_open_probe(&PathBuf::from(std::env::var_os("HOME").unwrap()));
    for unset in [false, true] {
        let mut missing = session.launch.clone();
        if unset {
            missing
                .recipe
                .unset_environment
                .extend(["CLAUDE_CONFIG_DIR".into(), "HOME".into()]);
        } else {
            missing.recipe.environment.remove("CLAUDE_CONFIG_DIR");
            missing.recipe.environment.remove("HOME");
        }
        let recorded = missing.clone();
        let error = create_history(&missing)
            .err()
            .expect("recorded selectors must not fall back to the process account");
        assert_eq!(
            error,
            "Saved Claude history has no recorded effective account directory or HOME"
        );
        assert!(probe.paths().is_empty());
        assert_eq!(missing, recorded);
    }
}

#[test]
fn history_requires_lexically_normalized_recorded_namespaces() {
    let fixture = Namespaces::new(false, true);
    let config = PathBuf::from(&fixture.launch.recipe.environment["CLAUDE_CONFIG_DIR"]);
    for present in [false, true] {
        if present {
            std::fs::create_dir(&fixture.original).unwrap();
        }
        for (path, account) in [(&fixture.original, false), (&config, true)] {
            let parent = path.parent().unwrap().to_str().unwrap();
            let name = path.file_name().unwrap().to_str().unwrap();
            for raw in [
                format!("{parent}/./{name}"),
                format!("{parent}//{name}"),
                format!("{parent}/{name}/"),
            ] {
                let mut launch = fixture.launch.clone();
                if account {
                    launch
                        .recipe
                        .environment
                        .insert("CLAUDE_CONFIG_DIR".into(), raw);
                } else {
                    launch.recipe.working_directory = raw.into();
                    // This alternate raw spelling has a valid same-UUID B
                    // transcript. Neither Path equality nor components alone
                    // may turn that legacy ambiguity into a selected source.
                    write_namespace(&config, &launch.recipe.working_directory, "B");
                }
                let issued = launch.clone();
                let probe = history_open_probe(fixture.root.path());
                let error = create_history(&launch)
                    .err()
                    .expect("raw namespace aliases must refuse");
                assert!(error.contains("normalized absolute recorded"), "{error}");
                assert!(probe.paths().is_empty());
                assert_eq!(launch, issued);
            }
        }
    }
}

#[test]
fn cold_history_refuses_rebound_cwd_before_opening_same_uuid_decoy() {
    for decoy in [false, true] {
        let fixture = Namespaces::new(false, decoy);
        let issued = fixture.launch.clone();
        let (_, mut original) = create_history(&fixture.launch).unwrap();
        assert_all_history(original.as_mut(), "A");
        drop(original);
        fixture.replace_cwd();
        let probe = history_open_probe(fixture.root.path());
        let error = create_history(&fixture.launch)
            .err()
            .expect("rebound History cwd must refuse");
        assert!(
            error.contains("working directory")
                && error.contains("original history source could not be verified"),
            "{error}"
        );
        assert!(
            probe.paths().is_empty(),
            "no transcript, including B, may be opened after refusal"
        );
        assert!(fixture.transcript.is_file());
        assert_eq!(fixture.decoy.is_file(), decoy);
        assert_eq!(fixture.launch, issued);
    }
}

#[test]
fn history_refuses_a_dangling_replacement_symlink_without_opening_history() {
    let fixture = Namespaces::new(false, false);
    std::fs::remove_dir(&fixture.replacement).unwrap();
    fixture.replace_cwd();
    let probe = history_open_probe(fixture.root.path());
    let error = create_history(&fixture.launch)
        .err()
        .expect("a dangling link is not the original absent directory");
    assert!(
        error.contains("original history source could not be verified"),
        "{error}"
    );
    assert!(probe.paths().is_empty());
}

#[test]
fn resident_history_body_and_title_keep_the_original_file_after_replacement() {
    let fixture = Namespaces::new(false, true);
    let probe = history_open_probe(fixture.root.path());
    let (_, mut adapter) = create_history(&fixture.launch).unwrap();
    fixture.replace_cwd();
    // Both the namespace B decoy and a replacement at the original pathname
    // carry the same UUID. The existing body and title reader must use A's FD.
    std::fs::rename(
        &fixture.transcript,
        fixture.transcript.with_extension("original"),
    )
    .unwrap();
    std::fs::copy(&fixture.decoy, &fixture.transcript).unwrap();
    assert_all_history(adapter.as_mut(), "A");
    assert_eq!(probe.paths(), vec![fixture.transcript]);
}

#[test]
fn history_requires_recorded_effective_absolute_account_selectors() {
    let fixture = Namespaces::new(true, true);
    for (config, home, unset_config, unset_home) in [
        (Some("relative-account"), None, false, false),
        (None, Some("relative-home"), false, false),
        (None, None, false, false),
        (Some("relative-account"), Some("relative-home"), true, true),
    ] {
        let mut launch = fixture.launch.clone();
        launch.recipe.environment.remove("CLAUDE_CONFIG_DIR");
        launch.recipe.environment.remove("HOME");
        if let Some(config) = config {
            launch
                .recipe
                .environment
                .insert("CLAUDE_CONFIG_DIR".into(), config.into());
        }
        if let Some(home) = home {
            launch.recipe.environment.insert("HOME".into(), home.into());
        }
        if unset_config {
            launch
                .recipe
                .unset_environment
                .insert("CLAUDE_CONFIG_DIR".into());
        }
        if unset_home {
            launch.recipe.unset_environment.insert("HOME".into());
        }
        let issued = launch.clone();
        let probe = history_open_probe(fixture.root.path());
        assert!(create_history(&launch).is_err());
        assert!(probe.paths().is_empty());
        assert_eq!(launch, issued);
    }
    let home = fixture.root.path().join("recorded-home");
    let config = home.join(".claude");
    let fallback = write_namespace(&config, &fixture.original, "A");
    let mut launch = fixture.launch.clone();
    launch
        .recipe
        .environment
        .insert("HOME".into(), home.to_str().unwrap().into());
    // An explicitly unset selector cannot override the recorded HOME, even
    // when its stale value is relative or points at another absolute account.
    for unset in [
        "relative-account".to_owned(),
        fixture.launch.recipe.environment["CLAUDE_CONFIG_DIR"].clone(),
    ] {
        launch
            .recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), unset);
        launch
            .recipe
            .unset_environment
            .insert("CLAUDE_CONFIG_DIR".into());
        let issued = launch.clone();
        let probe = history_open_probe(fixture.root.path());
        let (_, mut adapter) = create_history(&launch).unwrap();
        assert_all_history(adapter.as_mut(), "A");
        assert_eq!(probe.paths(), vec![fallback.clone()]);
        assert_eq!(launch, issued);
    }
}

#[test]
fn history_refuses_rebound_recorded_account_before_opening_a_transcript() {
    let fixture = Namespaces::new(false, true);
    let config = PathBuf::from(&fixture.launch.recipe.environment["CLAUDE_CONFIG_DIR"]);
    let replacement = fixture.root.path().join("other-account");
    write_namespace(&replacement, &fixture.original, "B");
    std::fs::rename(&config, fixture.root.path().join("original-account")).unwrap();
    std::os::unix::fs::symlink(&replacement, &config).unwrap();
    let probe = history_open_probe(fixture.root.path());
    let error = create_history(&fixture.launch)
        .err()
        .expect("rebound account must refuse");
    assert!(
        error.contains("account directory")
            && error.contains("original history source could not be verified"),
        "{error}"
    );
    assert!(probe.paths().is_empty());
}

#[test]
fn ordinary_resume_preserves_live_symlink_cwd_and_relative_account_resolution() {
    let fixture = Namespaces::new(false, true);
    fixture.replace_cwd();
    let config = fixture.replacement.join("relative-account");
    let transcript = write_namespace(&config, &fixture.replacement, "B");
    let mut launch = fixture.launch.clone();
    launch
        .recipe
        .environment
        .insert("CLAUDE_CONFIG_DIR".into(), "relative-account".into());
    let probe = history_open_probe(fixture.root.path());
    let (spec, mut adapter) = create(&launch).unwrap();
    assert!(matches!(
        spec.transport,
        super::super::TransportSpec::JsonLines
    ));
    let initial = adapter.initialize().unwrap();
    assert!(!initial.writes.is_empty());
    assert!(initial.events.iter().any(|event| matches!(event, Event::History { items, .. }
        if items.iter().any(|item| matches!(&item.content, ItemContent::Text { text, .. } if text == "Source B row 219")))));
    assert_eq!(probe.paths(), vec![transcript]);
}
