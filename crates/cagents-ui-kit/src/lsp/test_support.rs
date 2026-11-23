//! Fixtures the application's tests share with this module's.

use super::*;
use serde_json::{Value, json};
use std::sync::atomic::AtomicUsize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Generous upper bound for "the runtime is making progress"; every test
/// below reaches its condition in milliseconds on an idle machine. It is
/// this wide only so a saturated machine cannot turn a slow schedule into
/// a spurious failure.
pub const WAIT: Duration = Duration::from_secs(30);

pub fn server(id: &str, extensions: &[&str], markers: &[&str]) -> LanguageServerSettings {
    LanguageServerSettings {
        initialization_options: None,
        id: id.to_owned(),
        languages: vec![crate::settings::LanguageServerLanguage {
            language_id: id.to_owned(),
            extensions: extensions.iter().map(|value| (*value).to_owned()).collect(),
        }],
        command: vec![format!("{id}-binary")],
        root_markers: markers.iter().map(|value| (*value).to_owned()).collect(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Incoming {
    pub method: String,
    pub id: Option<i64>,
    pub params: Value,
}

/// What the fake server's writer task puts on the wire.
pub enum Outgoing {
    /// Framed as a `Content-Length` message.
    Message(Value),
    /// Written verbatim, so a test can present the framing a hostile
    /// server would.
    Raw(Vec<u8>),
}

#[derive(Clone)]
pub struct FakeHandle {
    pub log: Arc<Mutex<Vec<Incoming>>>,
    pub push: mpsc::UnboundedSender<Outgoing>,
    /// Framed messages the fake server has written, which only advances
    /// when a Content-Length frame was produced.
    pub written: Arc<AtomicUsize>,
}

impl FakeHandle {
    pub fn methods(&self) -> Vec<String> {
        self.log
            .lock()
            .expect("fake log")
            .iter()
            .map(|entry| entry.method.clone())
            .collect()
    }

    pub fn entries(&self, method: &str) -> Vec<Incoming> {
        self.log
            .lock()
            .expect("fake log")
            .iter()
            .filter(|entry| entry.method == method)
            .cloned()
            .collect()
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.push.send(Outgoing::Message(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        })));
    }

    /// Puts bytes on the wire exactly as given, framing and all.
    pub fn write_raw(&self, bytes: Vec<u8>) {
        let _ = self.push.send(Outgoing::Raw(bytes));
    }

    pub async fn wait_for(&self, method: &str) -> Incoming {
        self.wait_for_count(method, 1)
            .await
            .into_iter()
            .next_back()
            .expect("at least one entry")
    }

    pub async fn wait_for_count(&self, method: &str, count: usize) -> Vec<Incoming> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let entries = self.entries(method);
            if entries.len() >= count {
                return entries;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected {count} `{method}` message(s), saw {} of them in {:?}",
                entries.len(),
                self.methods()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

pub async fn read_framed(reader: &mut (impl AsyncBufRead + Unpin)) -> Option<Value> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            length = value.trim().parse().ok();
        }
    }
    let mut body = vec![0u8; length?];
    reader.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

pub async fn write_framed(writer: &mut (impl AsyncWrite + Unpin), value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value).expect("serialise");
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer.flush().await
}

/// Spawns a fake server on one half of a `tokio::io::duplex` pair and
/// returns the transport for the client half. `run_session` wraps that
/// half with `tokio_util::compat` before handing it to
/// `async_lsp::MainLoop::run_buffered`, so the production framing path is
/// what these tests exercise.
pub(super) fn fake_transport(
    handler: impl Fn(&Incoming) -> Vec<Value> + Send + Sync + 'static,
) -> (Transport, FakeHandle) {
    let (client, fake) = tokio::io::duplex(1 << 20);
    let (client_read, client_write) = tokio::io::split(client);
    let (fake_read, mut fake_write) = tokio::io::split(fake);
    let log: Arc<Mutex<Vec<Incoming>>> = Arc::new(Mutex::new(Vec::new()));
    let written = Arc::new(AtomicUsize::new(0));
    let (push, mut push_rx) = mpsc::unbounded_channel::<Outgoing>();

    let write_count = Arc::clone(&written);
    tokio::spawn(async move {
        while let Some(item) = push_rx.recv().await {
            match item {
                Outgoing::Message(message) => {
                    if write_framed(&mut fake_write, &message).await.is_err() {
                        break;
                    }
                    write_count.fetch_add(1, Ordering::Release);
                }
                Outgoing::Raw(bytes) => {
                    if fake_write.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let reader_log = Arc::clone(&log);
    let reader_push = push.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(fake_read);
        while let Some(message) = read_framed(&mut reader).await {
            let incoming = Incoming {
                method: message
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                id: message.get("id").and_then(Value::as_i64),
                params: message.get("params").cloned().unwrap_or(Value::Null),
            };
            reader_log.lock().expect("fake log").push(incoming.clone());
            for reply in handler(&incoming) {
                if reader_push.send(Outgoing::Message(reply)).is_err() {
                    return;
                }
            }
        }
    });

    (
        Transport {
            reader: Box::new(client_read),
            writer: Box::new(client_write),
            stderr: None,
            child: None,
        },
        FakeHandle { log, push, written },
    )
}

/// Wires `manager` to a fake server that answers `initialize` with
/// `capabilities` and `shutdown` with success, delegating everything else
/// to `handler`.
pub fn install_fake(
    manager: &mut LspManager,
    capabilities: Value,
    handler: impl Fn(&Incoming) -> Vec<Value> + Send + Sync + 'static,
) -> Arc<Mutex<Option<FakeHandle>>> {
    install_fake_with_child(manager, capabilities, handler, Arc::new(Mutex::new(None)))
}

/// As [`install_fake`], but the first transport also carries `child`, so a
/// test can observe what the session task does to a real process.
pub(super) fn install_fake_with_child(
    manager: &mut LspManager,
    capabilities: Value,
    handler: impl Fn(&Incoming) -> Vec<Value> + Send + Sync + 'static,
    child: Arc<Mutex<Option<ChildProcess>>>,
) -> Arc<Mutex<Option<FakeHandle>>> {
    let slot: Arc<Mutex<Option<FakeHandle>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&slot);
    let handler = Arc::new(handler);
    manager.set_transport(Arc::new(move |_spec: &LaunchSpec| {
        let capabilities = capabilities.clone();
        let handler = Arc::clone(&handler);
        let (mut transport, handle) =
            fake_transport(move |incoming| match incoming.method.as_str() {
                "initialize" => vec![json!({
                    "jsonrpc": "2.0",
                    "id": incoming.id,
                    "result": { "capabilities": capabilities },
                })],
                "shutdown" => vec![json!({
                    "jsonrpc": "2.0",
                    "id": incoming.id,
                    "result": Value::Null,
                })],
                _ => handler(incoming),
            });
        transport.child = child.lock().expect("child slot").take();
        *sink.lock().expect("transport slot") = Some(handle);
        Ok(transport)
    }));
    slot
}

pub async fn handle_of(slot: &Arc<Mutex<Option<FakeHandle>>>) -> FakeHandle {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some(handle) = slot.lock().expect("transport slot").clone() {
            return handle;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the fake transport was never created"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

pub async fn next_matching<T>(
    events: &async_channel::Receiver<LspEvent>,
    mut pick: impl FnMut(&LspEvent) -> Option<T>,
) -> T {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "no matching event arrived in time");
        let Ok(Ok(event)) = tokio::time::timeout(remaining, events.recv()).await else {
            panic!("the event channel closed or timed out");
        };
        if let Some(value) = pick(&event) {
            return value;
        }
    }
}

pub fn incremental_capabilities() -> Value {
    json!({ "textDocumentSync": { "openClose": true, "change": 2, "save": true } })
}

/// Positive fixture roots must match discovery's canonical session keys.
pub fn canonical_tempdir() -> tempfile::TempDir {
    let parent = std::env::temp_dir().canonicalize().expect("temp parent");
    tempfile::tempdir_in(parent).expect("tempdir")
}

/// A worktree with a root marker and one Rust file.
pub fn workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = canonical_tempdir();
    let worktree = root.path().to_path_buf();
    std::fs::write(worktree.join("Cargo.toml"), "[package]\n").expect("marker");
    let file = worktree.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("file");
    (root, worktree, file)
}

pub fn rust_manager() -> LspManager {
    LspManager::new(vec![server("rust-analyzer", &["rs"], &["Cargo.toml"])])
}

pub fn rust_language() -> LspLanguage {
    LspLanguage {
        server_id: "rust-analyzer".to_owned(),
        language_id: "rust-analyzer".to_owned(),
    }
}

/// A real manager binding and in-process LSP wire for UI progress tests.
pub struct ProgressFixture {
    pub doc: LspDocumentId,
    pub ready: LspEvent,
    pub handle: FakeHandle,
    pub events: async_channel::Receiver<LspEvent>,
    pub _root: tempfile::TempDir,
}

pub async fn progress_fixture() -> (LspManager, ProgressFixture) {
    let (root, worktree, file) = workspace();
    let mut manager = rust_manager();
    let doc = LspDocumentId(1);
    let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
    let events = manager.events();
    manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc");
    let handle = handle_of(&slot).await;
    let ready = next_matching(&events, |event| match event {
        LspEvent::Status {
            status: LspStatus::Ready,
            ..
        } => Some(event.clone()),
        _ => None,
    })
    .await;
    (
        manager,
        ProgressFixture {
            doc,
            ready,
            handle,
            events,
            _root: root,
        },
    )
}

impl ProgressFixture {
    pub async fn publish(&self, progress: LspProgress) -> LspEvent {
        let value = if progress.done {
            json!({"kind":"end", "message":progress.message})
        } else {
            json!({"kind":"begin", "title":progress.title.unwrap_or_default(),
                "message":progress.message, "percentage":progress.percentage})
        };
        self.handle
            .notify("$/progress", json!({"token":progress.token, "value":value}));
        next_matching(&self.events, |event| match event {
            LspEvent::Progress { .. } => Some(event.clone()),
            _ => None,
        })
        .await
    }
}
