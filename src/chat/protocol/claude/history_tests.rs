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
