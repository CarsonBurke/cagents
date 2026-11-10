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

#[test]
fn trailing_record_recovers_after_more_than_256_false_openers() {
    let mut line = b"cut record ".to_vec();
    line.extend(std::iter::repeat_n(b'{', 300));
    let offset = line.len();
    line.extend_from_slice(br#"{"type":"user","uuid":"recovered","message":{"content":"kept"}}"#);
    let (found, record) =
        trailing_record(&line).expect("the complete trailing record is recoverable");
    assert_eq!(found, offset);
    assert_eq!(record["uuid"], "recovered");
}

#[test]
fn trailing_record_respects_quoted_braces_escapes_and_arbitrary_damaged_prefixes() {
    let tail = serde_json::to_vec(&json!({"type":"user","uuid":"tail","message":{"content":"braces {} and quote \\\" and \\\\"}})).unwrap();
    let alphabet = b"{}[]\\\" abcdef:012,";
    let mut seed = 37_u64;
    for length in 0..512 {
        let mut line = "damaged λ ".as_bytes().to_vec();
        for _ in 0..length % 97 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            line.push(alphabet[seed as usize % alphabet.len()]);
        }
        let offset = line.len();
        line.extend_from_slice(&tail);
        line.extend_from_slice(b" \r\n");
        let (found, record) = trailing_record(&line).expect("valid trailing record");
        assert_eq!(found, offset);
        assert_eq!(record["uuid"], "tail");
    }
}

#[test]
#[ignore = "30 MiB damaged-line workload; run explicitly on fw16 for before/after measurements"]
fn measure_trailing_record_damaged_line_recovery() {
    use std::time::Instant;
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    for valid_tail in [false, true] {
        let mut line = b"cut ".to_vec();
        for _ in 0..100 {
            line.extend_from_slice(br#"{"payload":"#);
        }
        line.push(b'"');
        line.extend(std::iter::repeat_n(b'x', MAX_PAYLOAD - 2048));
        if valid_tail {
            line.extend_from_slice(
                br#"{"type":"user","uuid":"tail","message":{"content":"kept"}}"#,
            );
        } else {
            line.push(b'}');
        }
        assert!(line.len() <= MAX_PAYLOAD);
        let mut samples = Vec::new();
        for _ in 0..3 {
            let start = Instant::now();
            let recovered = trailing_record(std::hint::black_box(&line));
            samples.push(start.elapsed().as_micros());
            assert_eq!(recovered.is_some(), valid_tail);
        }
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        eprintln!(
            "recovery-measure host={} bytes={} nested_prefixes=100 valid_tail={} samples_us={:?} median_us={}",
            host.trim(),
            line.len(),
            valid_tail,
            samples,
            sorted[1]
        );
    }
}
