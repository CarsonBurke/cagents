use super::test_support::*;
use super::*;

use std::sync::atomic::AtomicUsize;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

const CHILD_HELPER_ENV: &str = "CAGENTS_LSP_CHILD_HELPER";
const CHILD_HELPER_TEST: &str = "lsp::tests::child_helper_sleeps_until_terminated";

/// Holds the path the grandchild helper publishes its pid to, so its
/// presence is proof the helper body ran.
const GRANDCHILD_ENV: &str = "CAGENTS_LSP_GRANDCHILD_HELPER";
const GRANDCHILD_TEST: &str = "lsp::tests::child_helper_leaves_a_grandchild_behind";

/// Holds the path the keystroke probe writes once its assertions have
/// passed. A filter that matches no test makes `libtest` exit zero, so
/// this marker is what tells the parent the probe actually ran.
const KEYSTROKE_PROBE_ENV: &str = "CAGENTS_LSP_KEYSTROKE_PROBE";
const KEYSTROKE_PROBE_TEST: &str = "lsp::tests::child_helper_converts_one_keystroke";

/// This crate does not enable `tokio/macros`, so async tests drive their
/// own runtime. It is single-threaded on purpose: the whole suite is
/// cooperative async, and dozens of concurrent multi-threaded runtimes
/// starve each other badly enough to trip the deadlines below.
fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}

/// Peak resident memory of this process, in bytes.
#[cfg(target_os = "linux")]
fn peak_resident() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").expect("proc status");
    let peak = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|value| value.trim().strip_suffix(" kB"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .expect("VmHWM in kB");
    peak * 1024
}

// -----------------------------------------------------------------
// Deterministic in-process fake server
// -----------------------------------------------------------------

async fn await_ready(events: &async_channel::Receiver<LspEvent>, doc: LspDocumentId) {
    next_matching(events, |event| match event {
        LspEvent::Status {
            doc: at, status, ..
        } if *at == doc && status.is_ready() => Some(()),
        _ => None,
    })
    .await;
}

/// Multiple opens may initialise in either order. Waiting for one document
/// at a time would consume and discard another document's Ready event.
async fn await_ready_documents(
    events: &async_channel::Receiver<LspEvent>,
    documents: &[LspDocumentId],
) {
    let mut pending: HashSet<_> = documents.iter().copied().collect();
    assert!(!pending.is_empty(), "expected documents to become ready");
    next_matching(events, |event| {
        if let LspEvent::Status { doc, status, .. } = event
            && status.is_ready()
        {
            pending.remove(doc);
            if pending.is_empty() {
                return Some(());
            }
        }
        None
    })
    .await;
}

#[test]
fn concurrent_readiness_wait_retains_reverse_order_and_ignores_unrelated_events() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let source = manager.events();
        let one = LspDocumentId(1);
        let two = LspDocumentId(2);
        let three = LspDocumentId(3);
        for doc in [one, two, three] {
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "text");
        }
        let _handle = handle_of(&slot).await;
        let mut ready = HashMap::new();
        while ready.len() < 3 {
            let event = next_matching(&source, |event| match event {
                LspEvent::Status {
                    status: LspStatus::Ready,
                    ..
                } => Some(event.clone()),
                _ => None,
            })
            .await;
            ready.insert(event_doc(&event).unwrap(), event);
        }
        let (sender, events) = async_channel::bounded(8);
        for doc in [two, three, two, one] {
            sender.try_send(ready[&doc].clone()).unwrap();
        }
        // The actual producer's values are reordered, not reminted with
        // an unfenced default. The old sequential waiter still fails.
        drop(sender);
        await_ready_documents(&events, &[one, two]).await;
        assert!(events.is_empty());
        manager.begin_shutdown().await;
    });
}

fn full_sync_capabilities() -> Value {
    json!({
        "textDocumentSync": {
            "openClose": true,
            "change": 1,
            "save": { "includeText": true },
        }
    })
}

#[test]
fn queued_status_and_progress_cannot_cross_a_session_restart() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let doc = LspDocumentId(1);
        let events = manager.events();
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc");
        let old_handle = handle_of(&slot).await;
        let old_ready = next_matching(&events, |event| match event {
            LspEvent::Status {
                status: LspStatus::Ready,
                ..
            } => Some(event.clone()),
            _ => None,
        })
        .await;
        old_handle.notify(
            "$/progress",
            json!({"token":"old", "value":{
            "kind":"begin", "title":"Old indexing"}}),
        );
        let old_progress = next_matching(&events, |event| match event {
            LspEvent::Progress { .. } => Some(event.clone()),
            _ => None,
        })
        .await;
        old_handle.write_raw(b"Content-Length: invalid\r\n\r\n".to_vec());
        let old_failed = next_matching(&events, |event| match event {
            LspEvent::Status {
                status: LspStatus::Failed { .. },
                ..
            } => Some(event.clone()),
            _ => None,
        })
        .await;

        // Withhold replacement initialization: a stale Ready must not
        // masquerade as readiness while the genuine new server starts.
        let replacement = Arc::new(Mutex::new(None));
        let output = Arc::clone(&replacement);
        manager.set_transport(Arc::new(move |_| {
            let (transport, handle) = fake_transport(|incoming| {
                if incoming.method == "shutdown" {
                    vec![json!({"jsonrpc":"2.0", "id":incoming.id, "result":null})]
                } else {
                    Vec::new()
                }
            });
            *output.lock().unwrap() = Some(handle);
            Ok(transport)
        }));
        manager.restart(doc);
        let new_handle = handle_of(&replacement).await;
        let initialize = new_handle.wait_for("initialize").await;
        let mut displayed = manager.status(doc);
        assert_eq!(displayed, LspStatus::Starting);
        for event in [old_ready, old_failed, old_progress] {
            assert!(!manager.event_is_current(&event));
            if manager.event_is_current(&event)
                && let LspEvent::Status { status, .. } = event
            {
                displayed = status;
            }
            assert_eq!(displayed, LspStatus::Starting);
        }
        new_handle
            .push
            .send(Outgoing::Message(json!({"jsonrpc":"2.0",
            "id":initialize.id, "result":{"capabilities":incremental_capabilities()}})))
            .unwrap();
        let ready = next_matching(&events, |event| match event {
            LspEvent::Status {
                status: LspStatus::Ready,
                ..
            } => Some(event.clone()),
            _ => None,
        })
        .await;
        assert!(manager.event_is_current(&ready));
        manager.begin_shutdown().await;
    });
}

#[test]
fn unbacked_refusal_survives_delivery_but_not_close_or_rebinding() {
    use std::os::unix::ffi::OsStringExt;
    let mut manager = rust_manager();
    let doc = LspDocumentId(1);
    let path = PathBuf::from(std::ffi::OsString::from_vec(b"/fixture/\xff.rs".to_vec()));
    let status = manager.open_for_test(doc, &path, Path::new("/fixture"), rust_language(), 1, "");
    assert!(matches!(status, LspStatus::Unavailable { .. }));
    assert_eq!(manager.status(doc), LspStatus::Disabled);
    let event = manager.events().try_recv().unwrap();
    assert!(matches!(&event, LspEvent::Status { status: delivered, .. } if *delivered == status));
    assert!(manager.event_is_current(&event));
    manager.close(doc);
    assert!(!manager.event_is_current(&event));
    manager.open_for_test(doc, &path, Path::new("/fixture"), rust_language(), 1, "");
    let rebound = manager.events().try_recv().unwrap();
    assert!(manager.event_is_current(&rebound));
    assert!(!manager.event_is_current(&event));
}

#[test]
fn stale_producers_cannot_displace_current_staged_status_or_progress() {
    use std::os::unix::ffi::OsStringExt;
    block_on(async {
        let (manager, fixture) = progress_fixture().await;
        let mut manager = manager;
        let old_progress = fixture
            .publish(LspProgress {
                token: "old".into(),
                title: Some("Old".into()),
                message: None,
                percentage: None,
                done: false,
            })
            .await;
        let doc = fixture.doc;
        let old_ready = fixture.ready.clone();
        // A genuine URI refusal supersedes this doc without retiring the
        // shared backend. Fill the channel to exercise actual staging.
        let peer = LspDocumentId(2);
        let reference = manager.documents[&doc].clone();
        manager.open_for_test(
            peer,
            &reference.path,
            &reference.worktree,
            reference.language.clone(),
            1,
            "abc",
        );
        let backend = manager.sessions[&reference.session].clone();
        while !manager.events.tx.is_full() {
            manager.events.emit_now(LspEvent::Resources { doc });
        }
        let refused = PathBuf::from(std::ffi::OsString::from_vec(b"/fixture/\xff.rs".to_vec()));
        let current_status =
            manager.open_for_test(doc, &refused, Path::new("/fixture"), rust_language(), 1, "");
        let current = manager.events.pending.inner.lock().unwrap().replaceable
            [&StagedClass::Status(doc)]
            .clone();
        {
            let mut inner = manager.events.pending.inner.lock().unwrap();
            // Model the delayed producer already past its initial check.
            inner.stage(old_ready);
            inner.stage(old_progress.clone());
            assert_eq!(inner.replaceable[&StagedClass::Status(doc)], current);
            assert!(!inner.replaceable.contains_key(&StagedClass::Progress(doc)));
        }
        assert!(!backend.retired.load(Ordering::Acquire));
        assert!(
            manager
                .events
                .session_delivery(peer, &backend.retired)
                .is_some()
        );
        // A live new producer's progress also survives delayed old traffic.
        manager.open_for_test(
            doc,
            &reference.path,
            &reference.worktree,
            reference.language,
            1,
            "abc",
        );
        let delivery = manager
            .events
            .session_delivery(doc, &backend.retired)
            .unwrap();
        let progress = LspEvent::Progress {
            doc,
            delivery,
            progress: LspProgress {
                token: "new".into(),
                title: Some("New".into()),
                message: None,
                percentage: None,
                done: false,
            },
        };
        manager.events.emit_now(progress.clone());
        let drained = {
            let mut inner = manager.events.pending.inner.lock().unwrap();
            inner.stage(current);
            inner.stage(old_progress);
            let mut values = Vec::new();
            while let Some(event) = inner.take(false) {
                values.push(event);
            }
            values
        };
        assert!(drained.contains(&progress));
        assert!(matches!(current_status, LspStatus::Unavailable { .. }));
        while fixture.events.try_recv().is_ok() {}
        manager.begin_shutdown().await;
    });
}

#[test]
fn a_uri_with_another_workspace_owner_is_refused_before_backend_admission() {
    block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let outer = directory.path().canonicalize().unwrap();
        let package = outer.join("pkg");
        std::fs::create_dir(&package).unwrap();
        std::fs::write(outer.join("Cargo.toml"), "[workspace]\nmembers=[\"pkg\"]\n").unwrap();
        std::fs::write(package.join("Cargo.toml"), "[package]\nname=\"pkg\"\n").unwrap();
        let file = package.join("lib.rs");
        std::fs::write(&file, "fixture").unwrap();
        let mut manager = rust_manager();
        let registry =
            install_fake_registry(&mut manager, full_sync_capabilities(), |_| Vec::new());
        let one = LspDocumentId(1);
        let two = LspDocumentId(2);
        assert_eq!(
            open_discovered(&mut manager, one, &file, &outer).await,
            outer
        );
        let uri = Url::from_file_path(&file).unwrap();
        let original = manager.snapshots.latest(&uri).unwrap();
        let events = manager.events();
        let language = rust_language();
        assert_eq!(
            manager.open(two, &file, &package, language, 1, "different"),
            LspStatus::Starting
        );
        let (ticket, root) = next_matching(&events, |event| match event {
            LspEvent::RootDiscovered { doc, ticket, root } if *doc == two => {
                Some((*ticket, root.clone()))
            }
            _ => None,
        })
        .await;
        assert_eq!(root, package);
        let owner = Arc::clone(&manager.pending_opens[&two].owner);
        manager.save(two, "saved during discovery");
        let refusal = manager.finish_discovery(two, ticket, root).unwrap();
        assert!(
            matches!(&refusal, LspStatus::Unavailable { reason } if reason.contains("another language-server workspace"))
        );
        assert_eq!(manager.status(two), refusal);
        assert!(Arc::ptr_eq(&manager.pending_opens[&two].owner, &owner));
        assert_eq!(
            manager.pending_opens[&two].saved_text.as_deref(),
            Some("saved during discovery")
        );
        assert_eq!(manager.document_version(two), Some(1));
        assert_ne!(manager.pending_opens[&two].ticket, ticket);
        assert_eq!(
            manager.finish_discovery(two, ticket, package.clone()),
            None,
            "a duplicate old discovery cannot readmit the refused registration"
        );
        assert_eq!(manager.sessions.len(), 1, "no second backend was admitted");
        assert_eq!(
            registry.lock().unwrap().len(),
            1,
            "no second transport was spawned"
        );
        assert!(!manager.documents.contains_key(&two));
        let (recipients, _) = manager
            .snapshots
            .diagnostic_snapshot(&uri, &manager.documents[&one].session)
            .unwrap();
        assert_eq!(recipients, vec![one]);
        assert!(Arc::ptr_eq(
            &manager.snapshots.latest(&uri).unwrap(),
            &original
        ));
        assert_eq!(manager.document_version(one), Some(original.version));
        let event = next_matching(&events, |event| match event {
            LspEvent::Status { doc, .. } if *doc == two => Some(event.clone()),
            _ => None,
        })
        .await;
        assert!(manager.event_is_current(&event));
        assert!(matches!(event, LspEvent::Status { status, .. } if status == refusal));
        manager.close(one);
        manager.change(two, 2, &[], "saved blocked edit");
        manager.save(two, "saved blocked edit");
        manager.change(two, 3, &[], "newest unsaved blocked edit");
        manager.restart(two);
        assert_eq!(manager.status(two), LspStatus::Starting);
        let (ticket, root) = next_matching(&events, |event| match event {
            LspEvent::RootDiscovered { doc, ticket, root } if *doc == two => {
                Some((*ticket, root.clone()))
            }
            _ => None,
        })
        .await;
        assert_eq!(root, package);
        let status = manager.finish_discovery(two, ticket, root).unwrap();
        if !status.is_ready() {
            await_ready(&events, two).await;
        }
        assert_eq!(registry.lock().unwrap().len(), 2);
        assert_eq!(
            manager.documents[&two].session.root.as_ref(),
            package.as_path()
        );
        let backend = handle_for(&registry, &package).await;
        let saved = backend.wait_for("textDocument/didSave").await;
        let opened = &backend.entries("textDocument/didOpen")[0];
        assert_eq!(
            opened.params["textDocument"]["text"],
            "newest unsaved blocked edit"
        );
        assert_eq!(opened.params["textDocument"]["version"], 3);
        assert_eq!(saved.params["text"], "saved blocked edit");
        manager.begin_shutdown().await;
    });
}

async fn open_discovered(
    manager: &mut LspManager,
    doc: LspDocumentId,
    path: &Path,
    worktree: &Path,
) -> PathBuf {
    let language = manager.language_for(path).expect("configured language");
    assert_eq!(
        manager.open(doc, path, worktree, language, 1, "fixture"),
        LspStatus::Starting
    );
    let events = manager.events();
    let (ticket, root) = next_matching(&events, |event| match event {
        LspEvent::RootDiscovered {
            doc: at,
            ticket,
            root,
        } if *at == doc => Some((*ticket, root.clone())),
        _ => None,
    })
    .await;
    let status = manager.finish_discovery(doc, ticket, root.clone());
    if !status.is_some_and(|status| status.is_ready()) {
        await_ready(&events, doc).await;
    }
    root
}

#[test]
fn asynchronous_workspace_members_and_independent_islands_keep_correct_sessions_in_both_orders() {
    block_on(async {
        for island_first in [false, true] {
            for island_manifest in [
                "[package]\nname=\"island\"\n[workspace]\n",
                "[package]\nname=\"island\"\n",
            ] {
                let root = canonical_tempdir();
                let worktree = root.path();
                std::fs::write(worktree.join("Cargo.toml"), "[workspace]\nmembers=[\"crates/*\"]\nexclude=[\"tools/island\"]\n[package]\nname=\"outer\"\n").unwrap();
                for name in ["crates/a", "crates/b", "tools/island"] {
                    std::fs::create_dir_all(worktree.join(name).join("src")).unwrap();
                    std::fs::write(
                        worktree.join(name).join("Cargo.toml"),
                        if name == "tools/island" {
                            island_manifest
                        } else {
                            "[package]\nname=\"member\"\n"
                        },
                    )
                    .unwrap();
                }
                let outer = worktree.join("src/lib.rs");
                std::fs::create_dir_all(outer.parent().unwrap()).unwrap();
                let island = worktree.join("tools/island/src/lib.rs");
                let mut manager = rust_manager();
                install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
                let factory = Arc::clone(&manager.transport);
                let roots = Arc::new(Mutex::new(Vec::new()));
                let launches = Arc::clone(&roots);
                manager.set_transport(Arc::new(move |spec| {
                    launches.lock().unwrap().push(spec.root.clone());
                    factory(spec)
                }));
                let order = if island_first {
                    [
                        (&island, worktree.join("tools/island")),
                        (&outer, worktree.to_owned()),
                    ]
                } else {
                    [
                        (&outer, worktree.to_owned()),
                        (&island, worktree.join("tools/island")),
                    ]
                };
                for (index, (path, expected)) in order.iter().enumerate() {
                    assert_eq!(
                        open_discovered(
                            &mut manager,
                            LspDocumentId(index as u64 + 1),
                            path,
                            worktree
                        )
                        .await,
                        *expected
                    );
                    assert_eq!(
                        manager.documents[&LspDocumentId(index as u64 + 1)]
                            .session
                            .root
                            .as_ref(),
                        expected
                    );
                }
                for (index, name) in ["crates/a", "crates/b"].iter().enumerate() {
                    assert_eq!(
                        open_discovered(
                            &mut manager,
                            LspDocumentId(index as u64 + 3),
                            &worktree.join(name).join("src/lib.rs"),
                            worktree
                        )
                        .await,
                        worktree
                    );
                }
                assert_eq!(manager.sessions.len(), 2);
                let mut actual = roots.lock().unwrap().clone();
                actual.sort();
                let mut expected = vec![worktree.to_owned(), worktree.join("tools/island")];
                expected.sort();
                assert_eq!(
                    actual, expected,
                    "initialize must receive each discovered root exactly once"
                );
                manager.shutdown();
                manager.wait_for_shutdown().await;
            }
        }
    });
}

#[test]
fn asynchronous_discovery_bounds_pending_work_and_cancels_closed_and_replaced_waiters() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        manager.discovery_slots = Arc::new(tokio::sync::Semaphore::new(0));
        manager.discovery_admissions = Arc::new(tokio::sync::Semaphore::new(4));
        let events = manager.events();
        let mut jobs = Vec::new();
        for index in 0..4 {
            assert_eq!(
                manager.open(
                    LspDocumentId(index),
                    &file,
                    &worktree,
                    rust_language(),
                    1,
                    "queued"
                ),
                LspStatus::Starting
            );
            jobs.push(
                manager.pending_opens[&LspDocumentId(index)]
                    ._discovery
                    .as_ref()
                    .unwrap()
                    .task
                    .clone(),
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "no blocking work may start before admission"
        );
        assert!(matches!(
            manager.open(
                LspDocumentId(99),
                &file,
                &worktree,
                rust_language(),
                1,
                "over limit"
            ),
            LspStatus::Unavailable { .. }
        ));
        assert_eq!(manager.pending_opens.len(), 5);
        assert_eq!(
            manager
                .pending_opens
                .values()
                .filter(|pending| pending._discovery.is_some())
                .count(),
            4
        );
        for index in 0..4 {
            manager.close(LspDocumentId(index));
        }
        tokio::time::timeout(WAIT, async {
            while jobs.iter().any(|job| !job.is_finished()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(manager.discovery_admissions.available_permits(), 4);
        // Reopening the same identity must acquire a fresh ticket and only
        // that registration may consume the subsequently released worker.
        manager.open(
            LspDocumentId(0),
            &file,
            &worktree,
            rust_language(),
            2,
            "old queued",
        );
        let old_ticket = manager.pending_opens[&LspDocumentId(0)].ticket;
        let old_job = manager.pending_opens[&LspDocumentId(0)]
            ._discovery
            .as_ref()
            .unwrap()
            .task
            .clone();
        manager.open(
            LspDocumentId(0),
            &file,
            &worktree,
            rust_language(),
            3,
            "replacement",
        );
        let new_ticket = manager.pending_opens[&LspDocumentId(0)].ticket;
        assert_ne!(old_ticket, new_ticket);
        manager.discovery_slots.add_permits(1);
        let ticket = next_matching(&events, |event| match event {
            LspEvent::RootDiscovered { ticket, .. } => Some(*ticket),
            _ => None,
        })
        .await;
        assert_eq!(ticket, new_ticket);
        tokio::time::timeout(WAIT, async {
            while !old_job.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        manager.shutdown();
        tokio::time::timeout(WAIT, async {
            while manager.discovery_admissions.available_permits() != 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(event, LspEvent::RootDiscovered { .. }));
        }
    });
}

#[test]
fn asynchronous_discovery_rejects_fifo_and_oversized_workspace_manifests() {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    struct ReleaseFifo {
        path: PathBuf,
        writer: Option<std::fs::File>,
    }
    impl Drop for ReleaseFifo {
        fn drop(&mut self) {
            // Remove the name before releasing a legacy blocking reader:
            // a late worker must see ENOENT instead of opening a new FIFO.
            let _ = std::fs::remove_file(&self.path);
            if let Some(mut writer) = self.writer.take() {
                let _ = writer.write_all(b"\n");
            }
        }
    }
    block_on(async {
        for inner in [false, true] {
            for fifo in [false, true] {
                for (name, extension, content) in [
                    ("Cargo.toml", "rs", "[workspace]\nmembers=[\"project\"]\n"),
                    ("package.json", "ts", "{\"workspaces\":[\"project\"]}"),
                    (
                        "tsconfig.json",
                        "ts",
                        "{\"references\":[{\"path\":\"project\"}]}",
                    ),
                    ("pnpm-workspace.yaml", "ts", "packages:\n - project\n"),
                ] {
                    let root = canonical_tempdir();
                    let project = root.path().join("project");
                    std::fs::create_dir_all(project.join("src")).unwrap();
                    std::fs::write(
                        project.join(if extension == "rs" {
                            "Cargo.toml"
                        } else {
                            "package.json"
                        }),
                        if extension == "rs" {
                            "[package]\nname=\"project\"\n"
                        } else {
                            "{}"
                        },
                    )
                    .unwrap();
                    if inner {
                        std::fs::write(root.path().join(name), content).unwrap();
                    }
                    let manifest = if inner {
                        project.join(name)
                    } else {
                        root.path().join(name)
                    };
                    if fifo {
                        let _ = std::fs::remove_file(&manifest);
                    }
                    let _release = if fifo {
                        let path = std::ffi::CString::new(manifest.as_os_str().as_encoded_bytes())
                            .unwrap();
                        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
                        Some(ReleaseFifo {
                            path: manifest.clone(),
                            writer: Some(
                                std::fs::OpenOptions::new()
                                    .read(true)
                                    .write(true)
                                    .custom_flags(libc::O_NONBLOCK)
                                    .open(&manifest)
                                    .unwrap(),
                            ),
                        })
                    } else {
                        let mut bytes = content.as_bytes().to_vec();
                        bytes.resize(discovery::MAX_MANIFEST_BYTES + 1, b' ');
                        std::fs::write(&manifest, bytes).unwrap();
                        None
                    };
                    let mut manager = if extension == "rs" {
                        rust_manager()
                    } else {
                        LspManager::new(vec![server("typescript", &["ts"], &["package.json"])])
                    };
                    install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
                    assert_eq!(
                        open_discovered(
                            &mut manager,
                            LspDocumentId(1),
                            &project.join(format!("src/lib.{extension}")),
                            root.path()
                        )
                        .await,
                        project,
                        "{name} descriptor (inner={inner}, fifo={fifo}) must not confer outer membership"
                    );
                    manager.shutdown();
                    manager.wait_for_shutdown().await;
                }
            }
        }
    });
}

#[test]
fn asynchronous_cargo_discovery_includes_transitive_workspace_path_dependencies() {
    block_on(async {
        let root = canonical_tempdir();
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\nmembers=[\"app\"]\n[workspace.dependencies]\nhelper={path=\"libs/helper\"}\n").unwrap();
        for (path, manifest) in [
            (
                "app",
                "[package]\nname=\"app\"\n[dependencies]\nhelper={workspace=true}\n",
            ),
            (
                "libs/helper",
                "[package]\nname=\"helper\"\n[target.'cfg(unix)'.dependencies]\nleaf={path=\"../leaf\"}\n",
            ),
            ("libs/leaf", "[package]\nname=\"leaf\"\n"),
        ] {
            std::fs::create_dir_all(root.path().join(path).join("src")).unwrap();
            std::fs::write(root.path().join(path).join("Cargo.toml"), manifest).unwrap();
        }
        let mut manager = rust_manager();
        install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        for (index, path) in ["libs/leaf", "app", "libs/helper"].iter().enumerate() {
            assert_eq!(
                open_discovered(
                    &mut manager,
                    LspDocumentId(index as u64),
                    &root.path().join(path).join("src/lib.rs"),
                    root.path()
                )
                .await,
                root.path()
            );
        }
        assert_eq!(manager.sessions.len(), 1);
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn asynchronous_discovery_uses_one_canonical_root_for_in_worktree_aliases() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        std::os::unix::fs::symlink(&worktree, worktree.join("alias")).unwrap();
        let alias = worktree.join("alias/main.rs");
        let mut manager = rust_manager();
        install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        assert_eq!(
            open_discovered(&mut manager, LspDocumentId(1), &alias, &worktree).await,
            worktree.canonicalize().unwrap()
        );
        assert_eq!(
            open_discovered(&mut manager, LspDocumentId(2), &file, &worktree).await,
            worktree.canonicalize().unwrap()
        );
        assert_eq!(manager.sessions.len(), 1);
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn asynchronous_package_workspace_link_resolves_a_member_beside_its_workspace_root() {
    block_on(async {
        let root = canonical_tempdir();
        let workspace = root.path().join("workspace");
        let member = root.path().join("member");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[workspace]\nmembers=[\"../member\"]\n",
        )
        .unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname=\"member\"\nworkspace=\"../workspace\"\n",
        )
        .unwrap();
        let mut manager = rust_manager();
        install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        assert_eq!(
            open_discovered(
                &mut manager,
                LspDocumentId(1),
                &member.join("src/lib.rs"),
                root.path()
            )
            .await,
            workspace
        );
        assert_eq!(
            manager.documents[&LspDocumentId(1)].session.root.as_ref(),
            workspace
        );
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn asynchronous_active_discovery_holds_worker_admission_until_real_worker_returns() {
    struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        manager.discovery_slots = Arc::new(tokio::sync::Semaphore::new(1));
        let release = Release(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
        let gate = Arc::clone(&release.0);
        let (entered, mut entering) = tokio::sync::mpsc::unbounded_channel();
        manager.discovery_enter = Some(Arc::new(move || {
            entered.send(()).unwrap();
            let (lock, changed) = &*gate;
            let _guard = changed
                .wait_while(lock.lock().unwrap(), |released| !*released)
                .unwrap();
        }));
        manager.open(
            LspDocumentId(1),
            &file,
            &worktree,
            rust_language(),
            1,
            "active",
        );
        tokio::time::timeout(WAIT, entering.recv())
            .await
            .unwrap()
            .unwrap();
        let task = manager.pending_opens[&LspDocumentId(1)]
            ._discovery
            .as_ref()
            .unwrap()
            .task
            .clone();
        manager.close(LspDocumentId(1));
        tokio::time::timeout(WAIT, async {
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            manager.discovery_slots.available_permits(),
            0,
            "cancelling an async waiter must not free its still-blocking worker slot"
        );
        drop(release);
        tokio::time::timeout(WAIT, async {
            while manager.discovery_slots.available_permits() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !manager
                .events()
                .try_recv()
                .is_ok_and(|event| matches!(event, LspEvent::RootDiscovered { .. }))
        );
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn asynchronous_javascript_workspace_boundaries_hold_in_both_open_orders() {
    block_on(async {
        for (descriptor, contents) in [
            (
                "package.json",
                r#"{"workspaces":["packages/*","!packages/island"]}"#,
            ),
            (
                "package.json",
                r#"{"workspaces":{"packages":["packages/a","packages/b"]}}"#,
            ),
            (
                "pnpm-workspace.yaml",
                "packages:\n - packages/*\n - '!packages/island'\n",
            ),
            (
                "pnpm-workspace.yaml",
                "packages:\n - '!packages/island'\n - packages/*\n",
            ),
            (
                "tsconfig.json",
                r#"{"references":[{"path":"packages/a/tsconfig.json"},{"path":"packages/b/tsconfig.json"}]}"#,
            ),
        ] {
            for island_first in [false, true] {
                let root = canonical_tempdir();
                for package in ["a", "b", "island"] {
                    let directory = root.path().join("packages").join(package);
                    std::fs::create_dir_all(directory.join("src")).unwrap();
                    std::fs::write(directory.join("package.json"), "{}").unwrap();
                    std::fs::write(directory.join("tsconfig.json"), r#"{"references":[{"path":"./tsconfig.app.json"},{"path":"./tsconfig.node.json"}]}"#).unwrap();
                    for config in ["tsconfig.app.json", "tsconfig.node.json"] {
                        std::fs::write(directory.join(config), "{}").unwrap();
                    }
                }
                std::fs::write(root.path().join(descriptor), contents).unwrap();
                let mut manager =
                    LspManager::new(vec![server("typescript", &["ts"], &["package.json"])]);
                install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
                let order = if island_first {
                    ["island", "a"]
                } else {
                    ["a", "island"]
                };
                for (index, package) in order.iter().enumerate() {
                    let expected = if *package == "a" {
                        root.path().to_owned()
                    } else {
                        root.path().join("packages/island")
                    };
                    assert_eq!(
                        open_discovered(
                            &mut manager,
                            LspDocumentId(index as u64),
                            &root
                                .path()
                                .join("packages")
                                .join(package)
                                .join("src/index.ts"),
                            root.path()
                        )
                        .await,
                        expected
                    );
                    assert_eq!(
                        manager.documents[&LspDocumentId(index as u64)]
                            .session
                            .root
                            .as_ref(),
                        expected
                    );
                }
                assert_eq!(
                    open_discovered(
                        &mut manager,
                        LspDocumentId(3),
                        &root.path().join("packages/b/src/index.ts"),
                        root.path()
                    )
                    .await,
                    root.path()
                );
                assert_eq!(manager.sessions.len(), 2);
                manager.shutdown();
                manager.wait_for_shutdown().await;
            }
        }
    });
}

#[test]
fn asynchronous_membership_budget_exhaustion_keeps_nearest_project() {
    block_on(async {
        let root = canonical_tempdir();
        let project = root.path().join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("Cargo.toml"), "[package]\nname=\"project\"\n").unwrap();
        let mut manifest =
            String::from("[workspace]\nmembers=[]\n[package]\nname=\"root\"\n[dependencies]\n");
        for index in 0..600 {
            manifest.push_str(&format!("dep{index}={{path=\"missing{index}\"}}\n"));
        }
        std::fs::write(root.path().join("Cargo.toml"), manifest).unwrap();
        let mut manager = rust_manager();
        install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        assert_eq!(
            open_discovered(
                &mut manager,
                LspDocumentId(1),
                &project.join("src/lib.rs"),
                root.path()
            )
            .await,
            project
        );
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn asynchronous_admission_refusal_restarts_with_the_latest_buffer_and_saved_text() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        manager.discovery_admissions = Arc::new(tokio::sync::Semaphore::new(1));
        let reservation = Arc::clone(&manager.discovery_admissions)
            .try_acquire_owned()
            .unwrap();
        let mut capabilities = incremental_capabilities();
        capabilities["textDocumentSync"]["save"] = json!({ "includeText": true });
        let fake = install_fake(&mut manager, capabilities, |_| Vec::new());
        let doc = LspDocumentId(17);
        assert!(matches!(
            manager.open(doc, &file, &worktree, rust_language(), 1, "original"),
            LspStatus::Unavailable { .. }
        ));
        assert!(matches!(manager.status(doc), LspStatus::Unavailable { .. }));
        assert!(manager.pending_opens[&doc]._discovery.is_none());
        let refused_ticket = manager.pending_opens[&doc].ticket;
        manager.change(doc, 2, &[], "saved edit");
        manager.save(doc, "saved edit");
        manager.change(doc, 3, &[], "newest unsaved edit");
        drop(reservation);
        manager.restart(doc);
        assert_eq!(manager.status(doc), LspStatus::Starting);
        let events = manager.events();
        let (ticket, root) = next_matching(&events, |event| match event {
            LspEvent::RootDiscovered {
                doc: at,
                ticket,
                root,
            } if *at == doc => Some((*ticket, root.clone())),
            _ => None,
        })
        .await;
        assert_ne!(ticket, refused_ticket);
        manager.finish_discovery(doc, ticket, root);
        await_ready(&events, doc).await;
        let fake = handle_of(&fake).await;
        fake.wait_for("textDocument/didSave").await;
        assert_eq!(
            fake.entries("textDocument/didOpen")[0].params["textDocument"]["text"],
            "newest unsaved edit"
        );
        assert_eq!(
            fake.entries("textDocument/didOpen")[0].params["textDocument"]["version"],
            3
        );
        assert_eq!(
            fake.entries("textDocument/didSave")[0].params["text"],
            "saved edit"
        );
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn asynchronous_pnpm_complexity_boundaries_keep_nearest_project() {
    block_on(async {
        let cases = [
            (
                "alias",
                String::from("shared: &members [project]\npackages: *members\n"),
            ),
            (
                "depth",
                format!(
                    "unrelated: {}0{}\npackages:\n - project\n",
                    "[".repeat(40),
                    "]".repeat(40)
                ),
            ),
            (
                "tokens",
                format!(
                    "unrelated:\n{}packages:\n - project\n",
                    " - value\n".repeat(4200)
                ),
            ),
        ];
        for (label, contents) in cases {
            let root = canonical_tempdir();
            let project = root.path().join("project");
            std::fs::create_dir_all(project.join("src")).unwrap();
            std::fs::write(project.join("package.json"), "{}").unwrap();
            std::fs::write(root.path().join("pnpm-workspace.yaml"), contents).unwrap();
            let mut manager =
                LspManager::new(vec![server("typescript", &["ts"], &["package.json"])]);
            install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            assert_eq!(
                open_discovered(
                    &mut manager,
                    LspDocumentId(1),
                    &project.join("src/index.ts"),
                    root.path()
                )
                .await,
                project,
                "{label} complexity must not confer workspace membership"
            );
            manager.shutdown();
            manager.wait_for_shutdown().await;
        }
    });
}

#[test]
fn discovery_workers_and_pending_admission_are_shared_across_managers() {
    let first = rust_manager();
    let second = rust_manager();
    assert!(Arc::ptr_eq(&first.discovery_slots, &second.discovery_slots));
    assert!(Arc::ptr_eq(
        &first.discovery_admissions,
        &second.discovery_admissions
    ));
    assert_eq!(discovery::MAX_PENDING, 256);
}

#[test]
fn workspace_discovery_prefers_topmost_cargo_workspace() {
    let root = canonical_tempdir();
    let nested = root.path().join("crates/nested");
    std::fs::create_dir_all(nested.join("src")).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/nested\"]\n",
    )
    .unwrap();
    std::fs::write(nested.join("Cargo.toml"), "[package]\nname = \"nested\"\n").unwrap();
    let markers = vec!["Cargo.toml".into()];
    assert_eq!(
        discover_root(&nested.join("src/lib.rs"), root.path(), &markers),
        root.path()
    );
    std::fs::remove_file(root.path().join("Cargo.toml")).unwrap();
    assert_eq!(
        discover_root(&nested.join("src/lib.rs"), root.path(), &markers),
        nested
    );
    std::fs::remove_file(nested.join("Cargo.toml")).unwrap();
    assert_eq!(
        discover_root(&nested.join("src/lib.rs"), root.path(), &markers),
        root.path()
    );
}

#[test]
fn workspace_discovery_recognizes_typescript_workspace_conventions() {
    for (manifest, content) in [
        ("package.json", r#"{"workspaces":["packages/*"]}"#),
        ("pnpm-workspace.yaml", "packages:\n - packages/*\n"),
        ("tsconfig.json", r#"{"references":[{"path":"packages/a"}]}"#),
    ] {
        let root = canonical_tempdir();
        let package = root.path().join("packages/a/src");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(root.path().join(manifest), content).unwrap();
        std::fs::write(package.parent().unwrap().join("package.json"), "{}").unwrap();
        std::fs::write(package.parent().unwrap().join("tsconfig.json"), "{}").unwrap();
        assert_eq!(
            discover_root(
                &package.join("index.ts"),
                root.path(),
                &["package.json".into()]
            ),
            root.path()
        );
    }
}

#[test]
fn workspace_jsonc_preserves_strings_and_accepts_comments_and_trailing_commas() {
    let value = workspace_manifest(
        br#"{
            // a normal TypeScript solution configuration
            "references": [{"path": "packages/a",},],
            "compilerOptions": {"baseUrl": "https://example.test/a//b",}, /* final comment */
        }"#
        .to_vec(),
        true,
    )
    .unwrap();
    assert_eq!(value["references"][0]["path"], "packages/a");
    assert_eq!(
        value["compilerOptions"]["baseUrl"],
        "https://example.test/a//b"
    );
}

#[test]
fn idle_state_uses_monotonic_fake_clock_and_never_evicts_open_documents() {
    let mut activity = SessionActivity {
        documents: HashSet::new(),
        last_document: None,
        last_used: Duration::from_secs(20),
    };
    let timeout = Duration::from_secs(600);
    assert!(!activity.idle_expired(Duration::from_secs(619), timeout));
    assert!(activity.idle_expired(Duration::from_secs(620), timeout));
    activity.documents.insert(LspDocumentId(1));
    assert!(!activity.idle_expired(Duration::from_secs(9999), timeout));
    activity.documents.clear();
    activity.last_used = Duration::from_secs(9999);
    assert!(!activity.idle_expired(Duration::from_secs(10000), timeout));
}

#[test]
fn session_cap_evicts_idle_lru_and_refuses_busy_eviction() {
    block_on(async {
        let (_root, worktree, _) = workspace();
        let mut manager = rust_manager().with_lifecycle(600, 2);
        manager.set_transport(Arc::new(|_| {
            Err("unused transport in admission test".into())
        }));
        let settings = manager.servers[0].clone();
        let key = |name: &str| SessionKey {
            server_id: Arc::from(settings.id.as_str()),
            root: Arc::from(worktree.join(name)),
        };
        let first = key("first");
        let second = key("second");
        let third = key("third");
        let a = manager
            .ensure_session(&first, &settings, &first.root, &worktree)
            .unwrap();
        let b = manager
            .ensure_session(&second, &settings, &second.root, &worktree)
            .unwrap();
        a.activity
            .lock()
            .unwrap()
            .documents
            .insert(LspDocumentId(1));
        b.activity
            .lock()
            .unwrap()
            .documents
            .insert(LspDocumentId(2));
        assert!(
            manager
                .ensure_session(&third, &settings, &third.root, &worktree)
                .is_err()
        );
        assert!(!a.retired.load(Ordering::Acquire));
        assert!(!b.retired.load(Ordering::Acquire));
        a.activity.lock().unwrap().documents.clear();
        manager
            .ensure_session(&third, &settings, &third.root, &worktree)
            .unwrap();
        assert!(a.retired.load(Ordering::Acquire));
        assert!(!b.retired.load(Ordering::Acquire));
        assert_eq!(manager.sessions.len(), 2);
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn cap_refused_document_keeps_latest_text_for_restart_and_later_join() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let second = worktree.join("other-worktree");
        std::fs::create_dir_all(&second).unwrap();
        let mut manager = rust_manager().with_lifecycle(600, 1);
        install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        manager.open_for_test(
            LspDocumentId(1),
            &file,
            &worktree,
            rust_language(),
            1,
            "first",
        );
        await_ready(&events, LspDocumentId(1)).await;
        let status = manager.open_for_test(
            LspDocumentId(2),
            &second.join("lib.rs"),
            &second,
            rust_language(),
            1,
            "blocked",
        );
        assert!(matches!(status, LspStatus::Unavailable { .. }));
        assert!(manager.documents.contains_key(&LspDocumentId(2)));
        manager.change(LspDocumentId(2), 2, &[], "edited while blocked");
        manager.close(LspDocumentId(1));
        manager.restart(LspDocumentId(2));
        await_ready(&events, LspDocumentId(2)).await;
        let reference = &manager.documents[&LspDocumentId(2)];
        assert_eq!(
            manager
                .snapshots
                .latest(&reference.uri)
                .unwrap()
                .full_text()
                .as_ref(),
            "edited while blocked"
        );
        assert_eq!(manager.sessions.len(), 1);
        assert!(
            manager.sessions[&reference.session]
                .activity
                .lock()
                .unwrap()
                .documents
                .contains(&LspDocumentId(2))
        );
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn later_admission_reopens_previously_refused_documents_for_that_root() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let second = worktree.join("other-worktree");
        std::fs::create_dir_all(&second).unwrap();
        let mut manager = rust_manager().with_lifecycle(600, 1);
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        manager.open_for_test(
            LspDocumentId(1),
            &file,
            &worktree,
            rust_language(),
            1,
            "first",
        );
        await_ready(&events, LspDocumentId(1)).await;
        assert!(matches!(
            manager.open_for_test(
                LspDocumentId(2),
                &second.join("lib.rs"),
                &second,
                rust_language(),
                1,
                "blocked"
            ),
            LspStatus::Unavailable { .. }
        ));
        manager.close(LspDocumentId(1));
        manager.open_for_test(
            LspDocumentId(3),
            &second.join("other.rs"),
            &second,
            rust_language(),
            1,
            "new file",
        );
        await_ready(&events, LspDocumentId(3)).await;
        let session = manager.sessions.values().next().unwrap();
        assert_eq!(session.activity.lock().unwrap().documents.len(), 2);
        let fake = handle_of(&slot).await;
        fake.wait_for_count("textDocument/didOpen", 2).await;
        let opened = fake.entries("textDocument/didOpen");
        assert!(
            opened
                .iter()
                .any(|entry| entry.params["textDocument"]["text"] == "blocked")
        );
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn cap_selects_least_recently_used_idle_session_with_fake_clock() {
    block_on(async {
        let (_root, worktree, _) = workspace();
        let mut manager = rust_manager().with_lifecycle(600, 2);
        manager.set_transport(Arc::new(|_| {
            Err("unused transport in admission test".into())
        }));
        let settings = manager.servers[0].clone();
        let key = |name: &str| SessionKey {
            server_id: Arc::from(settings.id.as_str()),
            root: Arc::from(worktree.join(name)),
        };
        let first = key("first");
        let second = key("second");
        let third = key("third");
        let a = manager
            .ensure_session(&first, &settings, &first.root, &worktree)
            .unwrap();
        let b = manager
            .ensure_session(&second, &settings, &second.root, &worktree)
            .unwrap();
        a.activity.lock().unwrap().last_used = Duration::from_secs(20);
        b.activity.lock().unwrap().last_used = Duration::from_secs(10);
        manager
            .ensure_session(&third, &settings, &third.root, &worktree)
            .unwrap();
        assert!(!a.retired.load(Ordering::Acquire));
        assert!(b.retired.load(Ordering::Acquire));
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn asynchronous_workspace_open_shares_server_and_idle_close_restarts_it() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        std::fs::write(
            worktree.join("Cargo.toml"),
            "[workspace]\nmembers=[\"crates/member\"]\n",
        )
        .unwrap();
        let member = worktree.join("crates/member");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(member.join("Cargo.toml"), "[package]\nname=\"member\"\n").unwrap();
        let nested = member.join("src/lib.rs");
        let mut manager = rust_manager().with_lifecycle(1, 2);
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        for (doc, path) in [(LspDocumentId(1), &file), (LspDocumentId(2), &nested)] {
            assert_eq!(
                manager.open(doc, path, &worktree, rust_language(), 1, "fn test() {}"),
                LspStatus::Starting
            );
            let (ticket, root) = next_matching(&events, |event| match event {
                LspEvent::RootDiscovered {
                    doc: at,
                    ticket,
                    root,
                } if *at == doc => Some((*ticket, root.clone())),
                _ => None,
            })
            .await;
            assert_eq!(root, worktree);
            let status = manager.finish_discovery(doc, ticket, root);
            if !status.is_some_and(|status| status.is_ready()) {
                await_ready(&events, doc).await;
            }
        }
        assert_eq!(manager.sessions.len(), 1);
        let handle = handle_of(&slot).await;
        assert_eq!(handle.entries("initialize").len(), 1);
        manager.close(LspDocumentId(1));
        manager.close(LspDocumentId(2));
        handle.wait_for("shutdown").await;
        handle.wait_for("exit").await;
        next_matching(&events, |event| match event {
            LspEvent::Resources { doc } if *doc == LspDocumentId(2) => Some(()),
            _ => None,
        })
        .await;
        assert!(
            manager.live_servers().is_empty(),
            "idle retirement must refresh cached metrics even after every document closed"
        );
        assert!(
            manager
                .sessions
                .values()
                .all(|session| session.retired.load(Ordering::Acquire))
        );
        let doc = LspDocumentId(3);
        manager.open(doc, &file, &worktree, rust_language(), 1, "fn test() {}");
        let (ticket, root) = next_matching(&events, |event| match event {
            LspEvent::RootDiscovered {
                doc: at,
                ticket,
                root,
            } if *at == doc => Some((*ticket, root.clone())),
            _ => None,
        })
        .await;
        let status = manager.finish_discovery(doc, ticket, root);
        if !status.is_some_and(|status| status.is_ready()) {
            await_ready(&events, doc).await;
        }
        assert!(
            manager
                .sessions
                .values()
                .all(|session| !session.retired.load(Ordering::Acquire))
        );
        manager.remove_worktree(&worktree);
        assert!(manager.sessions.is_empty());
        assert!(manager.documents.is_empty());
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn nested_worktrees_keep_separate_servers_and_removal_retires_idle_owner() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        std::fs::write(worktree.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
        let linked = worktree.join(".worktrees/feature");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
        let mut manager = rust_manager();
        install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        manager.open_for_test(
            LspDocumentId(1),
            &file,
            &worktree,
            rust_language(),
            1,
            "fn outer() {}",
        );
        await_ready(&events, LspDocumentId(1)).await;
        manager.open_for_test(
            LspDocumentId(2),
            &linked.join("lib.rs"),
            &linked,
            rust_language(),
            1,
            "fn linked() {}",
        );
        await_ready(&events, LspDocumentId(2)).await;
        assert_eq!(
            manager.sessions.len(),
            2,
            "a repository-local worktree is a separate workspace"
        );
        let outer = manager
            .sessions
            .values()
            .find(|handle| handle.worktree.as_ref() == worktree)
            .unwrap()
            .clone();
        let inner = manager
            .sessions
            .values()
            .find(|handle| handle.worktree.as_ref() == linked)
            .unwrap()
            .clone();
        manager.close(LspDocumentId(2));
        assert!(inner.activity.lock().unwrap().documents.is_empty());
        manager.remove_worktree(&linked);
        assert!(
            inner.retired.load(Ordering::Acquire),
            "idle server must retire immediately, before filesystem polling"
        );
        assert!(!outer.retired.load(Ordering::Acquire));
        assert_eq!(manager.sessions.len(), 1);
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn captured_removal_does_not_retire_restarted_same_path_session_or_reused_document() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "original");
        await_ready(&events, doc).await;
        let old = manager.sessions.values().next().unwrap().clone();
        let removal = manager.capture_worktree_removal(&worktree);
        manager.restart(doc);
        await_ready(&events, doc).await;
        let successor = manager.sessions.values().next().unwrap().clone();
        assert!(!Arc::ptr_eq(&old.retired, &successor.retired));
        assert!(manager.remove_captured_worktree(removal.clone()).is_empty());
        assert!(manager.documents.contains_key(&doc));
        assert!(!successor.retired.load(Ordering::Acquire));
        assert_eq!(manager.sessions.len(), 1);
        manager.close(doc);
        manager.open_for_test(
            doc,
            &file,
            &worktree,
            rust_language(),
            1,
            "new registration",
        );
        assert!(manager.remove_captured_worktree(removal).is_empty());
        let uri = &manager.documents[&doc].uri;
        assert_eq!(
            manager.snapshots.latest(uri).unwrap().full_text().as_ref(),
            "new registration"
        );
        assert!(!successor.retired.load(Ordering::Acquire));
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn captured_pending_removal_tracks_created_session_and_preserves_newer_joined_buffer() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let old_doc = LspDocumentId(1);
        manager.open(
            old_doc,
            &file,
            &worktree,
            rust_language(),
            1,
            "original pending",
        );
        let removal = manager.capture_worktree_removal(&worktree);
        assert!(removal.sessions.is_empty());
        let (ticket, root) = next_matching(&events, |event| match event {
            LspEvent::RootDiscovered { doc, ticket, root } if *doc == old_doc => {
                Some((*ticket, root.clone()))
            }
            _ => None,
        })
        .await;
        manager.finish_discovery(old_doc, ticket, root);
        await_ready(&events, old_doc).await;
        let original = manager.sessions.values().next().unwrap().clone();
        let new_doc = LspDocumentId(2);
        manager.open_for_test(
            new_doc,
            &worktree.join("new.rs"),
            &worktree,
            rust_language(),
            1,
            "new buffer",
        );
        manager.change(new_doc, 2, &[], "newer unsaved buffer");
        assert_eq!(
            manager.remove_captured_worktree(removal.clone()),
            vec![old_doc]
        );
        assert!(!manager.documents.contains_key(&old_doc));
        assert!(original.retired.load(Ordering::Acquire));
        await_ready(&events, new_doc).await;
        let successor = manager.sessions.values().next().unwrap().clone();
        assert!(!Arc::ptr_eq(&original.retired, &successor.retired));
        assert_eq!(
            manager
                .snapshots
                .latest(&manager.documents[&new_doc].uri)
                .unwrap()
                .full_text()
                .as_ref(),
            "newer unsaved buffer"
        );
        assert!(manager.remove_captured_worktree(removal).is_empty());
        assert!(manager.documents.contains_key(&new_doc));
        assert!(!successor.retired.load(Ordering::Acquire));
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn captured_pending_removal_cannot_retire_a_session_created_by_a_newer_registration() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let old_doc = LspDocumentId(1);
        manager.open(old_doc, &file, &worktree, rust_language(), 1, "pending A");
        let removal = manager.capture_worktree_removal(&worktree);
        let (ticket, root) = next_matching(&events, |event| match event {
            LspEvent::RootDiscovered { doc, ticket, root } if *doc == old_doc => {
                Some((*ticket, root.clone()))
            }
            _ => None,
        })
        .await;
        let new_doc = LspDocumentId(2);
        manager.open_for_test(
            new_doc,
            &worktree.join("new.rs"),
            &worktree,
            rust_language(),
            1,
            "B",
        );
        await_ready(&events, new_doc).await;
        let successor = manager.sessions.values().next().unwrap().clone();
        manager.finish_discovery(old_doc, ticket, root);
        assert_eq!(manager.remove_captured_worktree(removal), vec![old_doc]);
        assert!(!manager.documents.contains_key(&old_doc));
        assert!(manager.documents.contains_key(&new_doc));
        assert!(!successor.retired.load(Ordering::Acquire));
        assert_eq!(handle_of(&slot).await.entries("initialize").len(), 1);
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn captured_removal_refuses_another_manager_and_drops_only_original_pending_discovery() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        let doc = LspDocumentId(1);
        manager.open(doc, &file, &worktree, rust_language(), 1, "pending A");
        let old_ticket = manager.pending_opens[&doc].ticket;
        let removal = manager.capture_worktree_removal(&worktree);
        manager.close(doc);
        manager.open(doc, &file, &worktree, rust_language(), 1, "pending B");
        let new_ticket = manager.pending_opens[&doc].ticket;
        assert!(manager.remove_captured_worktree(removal.clone()).is_empty());
        assert_eq!(manager.pending_opens[&doc].ticket, new_ticket);
        assert!(
            manager
                .finish_discovery(doc, old_ticket, worktree.clone())
                .is_none()
        );
        let mut replacement = rust_manager();
        replacement.open(doc, &file, &worktree, rust_language(), 1, "other manager");
        assert!(replacement.remove_captured_worktree(removal).is_empty());
        assert_eq!(
            replacement.pending_opens[&doc].text.full_text().as_ref(),
            "other manager"
        );
        let current = manager.capture_worktree_removal(&worktree);
        assert_eq!(manager.remove_captured_worktree(current), vec![doc]);
        assert!(manager.pending_opens.is_empty());
        assert!(
            manager
                .finish_discovery(doc, new_ticket, worktree)
                .is_none()
        );
        manager.shutdown();
        replacement.shutdown();
        manager.wait_for_shutdown().await;
        replacement.wait_for_shutdown().await;
    });
}

#[test]
fn closed_workspace_session_launches_its_canonical_key_root() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        std::fs::write(
            worktree.join("Cargo.toml"),
            "[workspace]\nmembers=[\"package\"]\n[package]\nname=\"root\"\n",
        )
        .unwrap();
        let nested = worktree.join("package");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("Cargo.toml"), "[package]\n").unwrap();
        let mut manager = rust_manager();
        install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let factory = Arc::clone(&manager.transport);
        let roots = Arc::new(Mutex::new(Vec::new()));
        let launches = Arc::clone(&roots);
        manager.set_transport(Arc::new(move |spec| {
            launches.lock().unwrap().push(spec.root.clone());
            factory(spec)
        }));
        let events = manager.events();
        manager.open_for_test(
            LspDocumentId(1),
            &file,
            &worktree,
            rust_language(),
            1,
            "first",
        );
        await_ready(&events, LspDocumentId(1)).await;
        let previous = manager.sessions.values().next().unwrap().clone();
        let task = previous.task.lock().unwrap().take().unwrap();
        task.abort();
        let _ = task.await;
        assert!(previous.commands.is_closed());
        assert!(!previous.retired.load(Ordering::Acquire));
        manager.open_for_test(
            LspDocumentId(2),
            &nested.join("lib.rs"),
            &worktree,
            rust_language(),
            1,
            "nested",
        );
        await_ready(&events, LspDocumentId(2)).await;
        assert_eq!(
            *roots.lock().unwrap(),
            vec![worktree.clone(), worktree.clone()]
        );
        assert_eq!(manager.sessions.len(), 1);
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn save_during_root_discovery_is_replayed_after_open_without_losing_newer_edits() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, full_sync_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open(doc, &file, &worktree, rust_language(), 1, "original");
        manager.save(doc, "saved while discovering");
        manager.change(doc, 2, &[], "newer unsaved edit");
        let (ticket, root) = next_matching(&events, |event| match event {
            LspEvent::RootDiscovered {
                doc: at,
                ticket,
                root,
            } if *at == doc => Some((*ticket, root.clone())),
            _ => None,
        })
        .await;
        manager.finish_discovery(doc, ticket, root);
        await_ready(&events, doc).await;
        let fake = handle_of(&slot).await;
        let saved = fake.wait_for("textDocument/didSave").await;
        assert_eq!(saved.params["text"], "saved while discovering");
        assert_eq!(
            fake.entries("textDocument/didOpen")[0].params["textDocument"]["text"],
            "newer unsaved edit"
        );
        let methods = fake.methods();
        assert!(
            methods
                .iter()
                .position(|method| method == "textDocument/didOpen")
                < methods
                    .iter()
                    .position(|method| method == "textDocument/didSave")
        );
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn public_lifecycle_builder_bounds_unvalidated_limits() {
    let manager = rust_manager().with_lifecycle(u64::MAX, usize::MAX);
    assert_eq!(manager.lifecycle(), (604800, 64));
    let manager = rust_manager().with_lifecycle(0, 0);
    assert_eq!(manager.lifecycle(), (1, 1));
}

#[test]
fn shutdown_awaits_idle_retirement_already_started_by_watcher() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager().with_lifecycle(1, 1);
        let slot = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&slot);
        manager.set_transport(Arc::new(move |_| {
            let (transport, handle) = fake_transport(|message| {
                if message.method == "initialize" {
                    vec![json!({"jsonrpc":"2.0", "id":message.id,
                        "result":{"capabilities":{"textDocumentSync":2}}})]
                } else {
                    Vec::new()
                } // deliberately leave shutdown unanswered
            });
            *sink.lock().unwrap() = Some(handle);
            Ok(transport)
        }));
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "fn main() {}");
        await_ready(&events, doc).await;
        let handle = handle_of(&slot).await;
        manager.close(doc);
        handle.wait_for("shutdown").await;
        assert!(
            manager
                .sessions
                .values()
                .all(|session| session.task.lock().unwrap().is_some()),
            "watcher retirement must leave the join handle reachable by the manager"
        );
        let started = tokio::time::Instant::now();
        manager.begin_shutdown().await;
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "shutdown must await the already-retiring server's handshake"
        );
        assert!(handle.entries("exit").len() == 1);
    });
}

#[test]
fn asynchronous_discovery_ignores_closed_and_replaced_documents() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        let events = manager.events();
        let doc = LspDocumentId(11);
        assert_eq!(
            manager.open(doc, &file, &worktree, rust_language(), 1, "old"),
            LspStatus::Starting
        );
        let old_ticket = manager.pending_opens[&doc].ticket;
        manager.change(doc, 2, &[], "edited during discovery");
        assert_eq!(
            manager.pending_opens[&doc].text.full_text().as_ref(),
            "edited during discovery"
        );
        // Exercise a completion already queued before close. Closing
        // earlier cancels discovery and correctly produces no event.
        let (ticket, root) = next_matching(&events, |event| match event {
            LspEvent::RootDiscovered { ticket, root, .. } => Some((*ticket, root.clone())),
            _ => None,
        })
        .await;
        assert_eq!(ticket, old_ticket);
        manager.close(doc);
        assert!(
            manager
                .finish_discovery(doc, ticket, root.clone())
                .is_none()
        );
        assert!(manager.sessions.is_empty());
        manager.open(doc, &file, &worktree, rust_language(), 3, "new");
        assert!(manager.finish_discovery(doc, old_ticket, root).is_none());
        assert!(
            manager.sessions.is_empty(),
            "stale discovery must not reopen a replacement"
        );
        assert_eq!(manager.pending_opens[&doc].version, 3);
        manager.close(doc);
    });
}

#[test]
fn delayed_initialization_retains_only_current_changes_and_resynchronizes() {
    block_on(async {
        let (_root, worktree, first) = workspace();
        let second = worktree.join("two.rs");
        let slot = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&slot);
        let mut manager = rust_manager();
        manager.set_transport(Arc::new(move |_| {
            let (transport, handle) = fake_transport(|incoming| {
                if incoming.method == "shutdown" {
                    vec![json!({"jsonrpc":"2.0", "id":incoming.id, "result":null})]
                } else {
                    Vec::new()
                }
            });
            *sink.lock().unwrap() = Some(handle);
            Ok(transport)
        }));
        let initial = "a".repeat(64 * 1024);
        manager.open_for_test(
            LspDocumentId(1),
            &first,
            &worktree,
            rust_language(),
            1,
            &initial,
        );
        manager.open_for_test(
            LspDocumentId(2),
            &second,
            &worktree,
            rust_language(),
            1,
            &initial,
        );
        let handle = handle_of(&slot).await;
        let initialize = handle.wait_for("initialize").await;
        let reference = manager.documents.get(&LspDocumentId(1)).unwrap().clone();
        let mut old_versions = Vec::new();
        let latest = "b".repeat(initial.len());
        for version in 2..302 {
            for doc in [LspDocumentId(1), LspDocumentId(2)] {
                manager.change(doc, version, &[], &latest);
            }
            old_versions.push(Arc::downgrade(
                manager
                    .snapshots
                    .latest(&reference.uri)
                    .unwrap()
                    .full_text(),
            ));
        }
        assert!(
            old_versions[..old_versions.len() - MAX_RETAINED_SNAPSHOTS]
                .iter()
                .all(|snapshot| snapshot.upgrade().is_none()),
            "queued versions must not pin the snapshot history"
        );
        assert_eq!(
            manager.sessions[&reference.session]
                .commands
                .pending_usage(),
            (4, 4 * initial.len())
        );
        handle
            .push
            .send(Outgoing::Message(json!({
                "jsonrpc":"2.0", "id":initialize.id,
                "result":{"capabilities":full_sync_capabilities()},
            })))
            .unwrap();
        await_ready(&manager.events(), LspDocumentId(1)).await;
        let changes = handle.wait_for_count("textDocument/didChange", 2).await;
        for change in changes {
            assert_eq!(change.params["textDocument"]["version"], 301);
            assert_eq!(change.params["contentChanges"][0]["text"], latest);
        }
        manager.begin_shutdown().await;
    });
}

#[test]
fn initializing_session_overload_settles_accepted_requests_and_remains_restartable() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        manager.set_transport(Arc::new(|_| Ok(fake_transport(|_| Vec::new()).0)));
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "text");
        let mut expected = std::collections::HashSet::new();
        // The Open is already queued. Enough request barriers must stop
        // admission instead of retaining an arbitrary command history.
        for _ in 0..256 {
            expected.insert(manager.request_hover(doc, LspPosition::new(0, 0)));
        }
        let events = manager.events();
        let mut failed = std::collections::HashSet::new();
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let event = tokio::time::timeout_at(deadline, events.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                LspEvent::RequestFailed { request, .. } => {
                    failed.insert(request);
                }
                LspEvent::Status {
                    status: LspStatus::Failed { reason, .. },
                    ..
                } => {
                    assert!(reason.contains("bounded budget"));
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(failed, expected);
        assert!(manager.status(doc).can_restart());
        manager.begin_shutdown().await;
    });
}

// -----------------------------------------------------------------
// Configuration, coordinates and snippets
// -----------------------------------------------------------------

#[test]
fn language_for_uses_the_first_declaration_that_claims_an_extension() {
    let manager = LspManager::new(vec![
        server("first", &["ts", "tsx"], &[]),
        server("second", &["ts"], &[]),
    ]);
    let language = manager
        .language_for(Path::new("/tmp/app.TS"))
        .expect("extensions match case-insensitively");
    assert_eq!(language.server_id, "first");
    assert!(manager.language_for(Path::new("/tmp/app.rs")).is_none());
    assert!(manager.language_for(Path::new("/tmp/Makefile")).is_none());
}

#[test]
fn an_empty_server_list_disables_lsp_without_spawning_anything() {
    let mut manager = LspManager::new(Vec::new());
    assert_eq!(
        manager.open_for_test(
            LspDocumentId(1),
            Path::new("/tmp/a.rs"),
            Path::new("/tmp"),
            rust_language(),
            1,
            "",
        ),
        LspStatus::Disabled
    );
    assert!(!LspStatus::Disabled.can_restart());
}

/// An exit reaps the servers it knows about exactly once, and the window
/// stays live and interactive for as long as that reap takes. A document
/// opened in it - a click on a file in a card whose tabs the exit has
/// already removed - must not start a server tree behind the reaper.
#[test]
fn a_manager_that_has_shut_down_starts_no_further_server() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        manager.shutdown();
        let status = manager.open_for_test(
            LspDocumentId(1),
            &file,
            &worktree,
            rust_language(),
            1,
            "fn main() {}\n",
        );
        let LspStatus::Unavailable { reason } = &status else {
            panic!("a stopped manager must decline the document, got {status:?}");
        };
        assert!(
            reason.contains("shutting"),
            "the refusal has to be one the editor can render, got {reason:?}"
        );
        assert!(
            manager.sessions.is_empty(),
            "a stopped manager must hold no session"
        );
        assert!(
            manager.flusher.is_none(),
            "and must not restart the event flusher it just aborted"
        );
    });
}

#[test]
fn root_discovery_stops_at_the_nearest_marker_and_never_leaves_the_worktree() {
    let root = canonical_tempdir();
    let worktree = root.path();
    let inner = worktree.join("crates/inner");
    std::fs::create_dir_all(inner.join("src")).expect("dirs");
    std::fs::write(worktree.join("Cargo.toml"), "").expect("outer marker");
    std::fs::write(inner.join("Cargo.toml"), "").expect("inner marker");
    let markers = vec!["Cargo.toml".to_owned()];

    assert_eq!(
        discover_root(&inner.join("src/lib.rs"), worktree, &markers),
        inner,
        "the nearest marker wins"
    );
    assert_eq!(
        discover_root(&worktree.join("build.rs"), worktree, &markers),
        worktree
    );
    assert_eq!(
        discover_root(Path::new("/elsewhere/main.rs"), worktree, &markers),
        worktree,
        "a marker above the worktree must never be selected"
    );
    assert_eq!(
        discover_root(&inner.join("src/lib.rs"), worktree, &[]),
        worktree,
        "a server with no markers uses the worktree"
    );
}

#[test]
fn diagnostic_previews_bound_unicode_bytes_and_copy_preserves_complete_protocol() {
    let budget = DIAGNOSTIC_PREVIEW_BYTES;
    for text in ["x".repeat(budget), "λ".repeat(budget), "😀".repeat(budget)] {
        let preview = diagnostic_preview(&text);
        assert!(preview.len() <= budget);
        if text.len() <= budget {
            assert!(matches!(preview, std::borrow::Cow::Borrowed(_)));
            assert_eq!(preview, text);
        } else {
            assert!(preview.ends_with(DIAGNOSTIC_TRUNCATED));
            assert!(text.starts_with(preview.strip_suffix(DIAGNOSTIC_TRUNCATED).unwrap()));
        }
    }
    let protocol: lsp_types::Diagnostic = serde_json::from_value(json!({
        "range": {"start":{"line":0,"character":1},"end":{"line":0,"character":3}},
        "severity": 2, "message": "λ".repeat(budget) + " ORIGINAL MESSAGE END",
        "source": "s".repeat(budget + 1), "code": "κ".repeat(budget),
        "codeDescription": {"href":"https://example.invalid/code"}, "tags":[1],
        "relatedInformation":[{"location":{"uri":"file:///tmp/other.rs", "range":{
            "start":{"line":1,"character":0},"end":{"line":1,"character":1}}},"message":"related"}],
        "data": {"opaque": [7, "unchanged"]}
    }))
    .unwrap();
    let expected = serde_json::to_value(&protocol).unwrap();
    let diagnostic = convert_diagnostic(&Snapshot::new(1, Arc::from("a😀b")), protocol).unwrap();
    assert_eq!(diagnostic.range.end.character, 2);
    assert!(diagnostic.message_preview().len() <= budget);
    assert!(diagnostic.origin_preview().len() <= budget);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&diagnostic.copy_text()).unwrap(),
        expected
    );
    assert!(!format!("{diagnostic:?}").contains("ORIGINAL MESSAGE END"));
    // Both optional code representations survive explicit inspection.
    let mut numeric = diagnostic.protocol.as_ref().clone();
    numeric.code = Some(lsp_types::NumberOrString::Number(2304));
    let numeric = convert_diagnostic(&Snapshot::new(1, Arc::from("a😀b")), numeric).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&numeric.copy_text()).unwrap()["code"],
        2304
    );
}

#[test]
fn diagnostic_publication_and_shift_share_one_original_payload_across_mirrors() {
    let snapshots = Arc::new(SnapshotStore::default());
    let uri = Arc::new(Url::parse("file:///tmp/mirrored.rs").unwrap());
    let key = SessionKey {
        server_id: Arc::from("test"),
        root: Arc::from(Path::new("/tmp")),
    };
    for doc in 1..=3 {
        snapshots
            .register(LspDocumentId(doc), &uri, &key, 1, Arc::from("a😀b"))
            .unwrap();
    }
    let version = snapshots.latest(&uri).unwrap().version;
    let (tx, rx) = async_channel::bounded(EVENT_CHANNEL_CAPACITY);
    let mut state = RouterState {
        key,
        snapshots,
        events: EventSink {
            tx,
            pending: Arc::new(PendingEvents::default()),
        },
        retired: Arc::new(AtomicBool::new(false)),
    };
    let message = "λ".repeat(DIAGNOSTIC_PREVIEW_BYTES + 1);
    let message_ptr = message.as_ptr();
    let original = lsp_types::Diagnostic {
        range: lsp_types::Range::new(
            lsp_types::Position::new(0, 1),
            lsp_types::Position::new(0, 3),
        ),
        message,
        source: Some("test".into()),
        data: Some(json!({"fix":7})),
        ..Default::default()
    };
    publish_diagnostics(
        &mut state,
        lsp_types::PublishDiagnosticsParams {
            uri: uri.as_ref().clone(),
            version: Some(version),
            diagnostics: vec![original],
        },
    );
    let mut batches = Vec::new();
    for doc in 1..=3 {
        let LspEvent::Diagnostics {
            doc: actual, items, ..
        } = rx.try_recv().unwrap()
        else {
            panic!("diagnostics event")
        };
        assert_eq!(actual, LspDocumentId(doc));
        assert_eq!(
            items[0].message.as_ptr(),
            message_ptr,
            "conversion moves the original strings"
        );
        batches.push(items);
    }
    for batch in &batches[1..] {
        assert!(Arc::ptr_eq(&batches[0][0].protocol, &batch[0].protocol));
    }
    let old_range = batches[0][0].range;
    shift_diagnostics(
        &mut batches[1],
        &[LspTextEdit {
            range: LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 0)),
            new_text: "x".into(),
        }],
    );
    assert_eq!(batches[0][0].range, old_range);
    assert_eq!(
        batches[1][0].range.start.character,
        old_range.start.character + 1
    );
    assert!(Arc::ptr_eq(
        &batches[0][0].protocol,
        &batches[1][0].protocol
    ));
    assert_eq!(
        batches[1][0].protocol.range.start.character, 1,
        "protocol coordinates remain original"
    );
}

#[test]
fn diagnostic_ranges_follow_insert_delete_and_multiline_replacements() {
    fn diagnostic(start: (u32, u32), end: (u32, u32)) -> LspDiagnostic {
        LspDiagnostic::new(
            LspRange::new(
                LspPosition::new(start.0, start.1),
                LspPosition::new(end.0, end.1),
            ),
            LspSeverity::Error,
            "error".into(),
            None,
            None,
        )
    }
    let mut items = vec![diagnostic((2, 3), (2, 8)), diagnostic((0, 0), (0, 2))];
    shift_diagnostics(
        &mut items,
        &[LspTextEdit {
            range: LspRange::new(LspPosition::new(1, 0), LspPosition::new(1, 0)),
            new_text: "α\r\nβ\n".into(),
        }],
    );
    assert_eq!(
        items[0].range,
        LspRange::new(LspPosition::new(4, 3), LspPosition::new(4, 8))
    );
    assert_eq!(
        items[1].range,
        LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 2))
    );
    shift_diagnostics(
        &mut items,
        &[LspTextEdit {
            range: LspRange::new(LspPosition::new(1, 0), LspPosition::new(3, 0)),
            new_text: String::new(),
        }],
    );
    assert_eq!(
        items[0].range,
        LspRange::new(LspPosition::new(2, 3), LspPosition::new(2, 8))
    );
    shift_diagnostics(
        &mut items,
        &[LspTextEdit {
            range: LspRange::new(LspPosition::new(2, 5), LspPosition::new(2, 7)),
            new_text: "x\ny".into(),
        }],
    );
    assert_eq!(
        items[0].range,
        LspRange::new(LspPosition::new(2, 3), LspPosition::new(3, 2))
    );
}

#[test]
fn diagnostic_insert_affinity_and_complete_deletion_remain_valid() {
    let mut item = LspDiagnostic::new(
        LspRange::new(LspPosition::new(0, 2), LspPosition::new(0, 5)),
        LspSeverity::Error,
        "error".into(),
        None,
        None,
    );
    shift_diagnostics(
        std::slice::from_mut(&mut item),
        &[LspTextEdit {
            range: LspRange::new(LspPosition::new(0, 2), LspPosition::new(0, 2)),
            new_text: "α".into(),
        }],
    );
    assert_eq!(
        item.range,
        LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 6))
    );
    shift_diagnostics(
        std::slice::from_mut(&mut item),
        &[LspTextEdit {
            range: LspRange::new(LspPosition::new(0, 6), LspPosition::new(0, 6)),
            new_text: "z".into(),
        }],
    );
    assert_eq!(item.range.end, LspPosition::new(0, 6));
    shift_diagnostics(
        std::slice::from_mut(&mut item),
        &[LspTextEdit {
            range: LspRange::new(LspPosition::new(0, 0), LspPosition::new(1, 0)),
            new_text: String::new(),
        }],
    );
    assert_eq!(item.range, LspRange::default());
}

#[test]
fn scalar_and_utf16_positions_round_trip_for_ascii_bmp_and_astral_text() {
    // "héllo" holds a two-byte BMP scalar; "𝄞" is astral and occupies two
    // UTF-16 code units.
    let snapshot = Snapshot::new(7, Arc::from("ascii\nhéllo\na𝄞b\n"));

    assert_eq!(
        snapshot
            .utf16_position(LspPosition::new(0, 5))
            .expect("ascii end"),
        lsp_types::Position::new(0, 5)
    );
    assert_eq!(
        snapshot
            .utf16_position(LspPosition::new(1, 2))
            .expect("bmp"),
        lsp_types::Position::new(1, 2)
    );
    assert_eq!(
        snapshot.byte_offset(LspPosition::new(1, 2)).expect("bytes"),
        "ascii\n".len() + 3,
        "the two-byte scalar shifts byte offsets but not UTF-16 offsets"
    );
    assert_eq!(
        snapshot
            .utf16_position(LspPosition::new(2, 2))
            .expect("astral"),
        lsp_types::Position::new(2, 3)
    );

    for (line, character) in [
        (0u32, 0u32),
        (0, 5),
        (1, 0),
        (1, 2),
        (1, 5),
        (2, 0),
        (2, 2),
        (2, 3),
    ] {
        let scalar = LspPosition::new(line, character);
        let utf16 = snapshot.utf16_position(scalar).expect("outbound");
        assert_eq!(
            snapshot.native_position(utf16).expect("inbound"),
            scalar,
            "{scalar:?} must round-trip"
        );
    }
}

#[test]
fn a_utf16_offset_inside_a_surrogate_pair_is_rejected() {
    let snapshot = Snapshot::new(1, Arc::from("a𝄞b"));
    assert_eq!(
        snapshot.native_position(lsp_types::Position::new(0, 2)),
        Err(LspError::SplitSurrogate {
            line: 0,
            character: 2
        })
    );
    assert!(matches!(
        snapshot.native_position(lsp_types::Position::new(0, 99)),
        Err(LspError::PositionOutOfRange { .. })
    ));
    assert!(matches!(
        snapshot.utf16_position(LspPosition::new(9, 0)),
        Err(LspError::PositionOutOfRange { version: 1, .. })
    ));
    assert_eq!(
        snapshot.native_range(lsp_types::Range::new(
            lsp_types::Position::new(0, 3),
            lsp_types::Position::new(0, 0),
        )),
        Err(LspError::InvertedRange)
    );
}

#[test]
fn a_crlf_terminator_is_not_part_of_its_line() {
    let snapshot = Snapshot::new(1, Arc::from("ab\r\ncd\r\n"));
    assert_eq!(
        snapshot.line_content(0),
        Some(std::borrow::Cow::Borrowed("ab"))
    );
    assert_eq!(
        snapshot.line_content(1),
        Some(std::borrow::Cow::Borrowed("cd"))
    );
    assert_eq!(
        snapshot.line_content(2),
        Some(std::borrow::Cow::Borrowed(""))
    );
    assert_eq!(
        snapshot
            .utf16_position(LspPosition::new(0, 2))
            .expect("line end"),
        lsp_types::Position::new(0, 2)
    );
    assert!(snapshot.utf16_position(LspPosition::new(0, 3)).is_err());
}

#[test]
fn incremental_conversion_measures_each_edit_against_the_text_it_applied_to() {
    let base = Snapshot::new(1, Arc::from("a𝄞b\n"));
    let edits = vec![
        // Delete the astral scalar.
        LspTextEdit {
            range: LspRange::new(LspPosition::new(0, 1), LspPosition::new(0, 2)),
            new_text: String::new(),
        },
        // Then append, at a position that only exists after the deletion.
        LspTextEdit {
            range: LspRange::new(LspPosition::new(0, 2), LspPosition::new(0, 2)),
            new_text: "!".to_owned(),
        },
    ];
    let changes = build_incremental(&base, &edits, "ab!\n").expect("convertible");
    assert_eq!(
        changes[0].range,
        Some(lsp_types::Range::new(
            lsp_types::Position::new(0, 1),
            lsp_types::Position::new(0, 3)
        )),
        "the astral scalar spans two UTF-16 units"
    );
    assert_eq!(
        changes[1].range,
        Some(lsp_types::Range::new(
            lsp_types::Position::new(0, 2),
            lsp_types::Position::new(0, 2)
        )),
        "the second edit is measured after the first applied"
    );

    let impossible = [LspTextEdit {
        range: LspRange::new(LspPosition::new(0, 99), LspPosition::new(0, 99)),
        new_text: "x".to_owned(),
    }];
    assert_eq!(
        build_incremental(&base, &impossible, "a𝄞bx\n"),
        Err(LspError::PositionOutOfRange {
            line: 0,
            character: 99,
            version: 1
        })
    );

    // An edit list that converts cleanly but does not produce the buffer
    // the editor reports is refused, because sending it would leave the
    // server permanently out of step with a text it can no longer see.
    let lying = [LspTextEdit {
        range: LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 1)),
        new_text: "z".to_owned(),
    }];
    assert_eq!(
        build_incremental(&base, &lying, "something else entirely\n"),
        Err(LspError::InconsistentEdits)
    );
    assert!(build_incremental(&base, &lying, "z𝄞b\n").is_ok());
}

/// Applies one keystroke to a large document and fails if that grew the
/// process by anything like a copy of it.
///
/// The editor pushes one edit per update and the widget calls the
/// conversion from the COSMIC update thread, so work proportional to the
/// file is felt as typing latency on every character. Peak resident
/// memory is the observable, and it is process-wide, which is why this
/// runs as a child of
/// `one_keystroke_does_not_materialise_the_document`: in the suite's own
/// process the rest of the tests would drown it.
#[test]
#[cfg(target_os = "linux")]
fn child_helper_converts_one_keystroke() {
    let Some(marker) = std::env::var_os(KEYSTROKE_PROBE_ENV) else {
        return;
    };
    // Large enough that a copy of it dwarfs whatever the process happens
    // to touch while the conversion runs.
    let text = "fn main() {}\n".repeat(2_580_000);
    let expected = format!("x{text}");
    let base = Snapshot::new(1, Arc::from(text.as_str()));
    let edits = [LspTextEdit {
        range: LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 0)),
        new_text: "x".to_owned(),
    }];
    let before = peak_resident();
    let changes = build_incremental(&base, &edits, &expected).expect("one insertion converts");
    let grown = peak_resident() - before;
    assert_eq!(changes.len(), 1);
    assert!(
        grown < text.len() / 4,
        "one edit grew the process by {grown} bytes while editing {} bytes",
        text.len()
    );
    // Written last, so its presence means the measurement above happened
    // and passed. The parent has no other way to tell this body apart from
    // a filter that selected nothing.
    std::fs::write(&marker, "converted").expect("publish the probe marker");
}

#[test]
#[cfg(target_os = "linux")]
fn one_keystroke_does_not_materialise_the_document() {
    let directory = tempfile::tempdir().expect("tempdir");
    let marker = directory.path().join("probe-ran");
    let executable = std::env::current_exe().expect("the test executable");
    let output = std::process::Command::new(executable)
        .args(["--exact", KEYSTROKE_PROBE_TEST, "--test-threads=1"])
        .env(KEYSTROKE_PROBE_ENV, &marker)
        .output()
        .expect("spawn the test executable");
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "converting one edit must not cost a copy of the document: {report}"
    );
    // `libtest` exits zero when `--exact` matches no test, so the exit
    // status alone is also what a probe that never ran would earn:
    // renaming the child test or the variable it looks for would leave
    // this test passing while measuring nothing. The marker is written by
    // the probe itself, after its assertion.
    assert!(
        marker.exists(),
        "`{KEYSTROKE_PROBE_TEST}` never ran, so nothing was measured: {report}"
    );
}

/// Pins each comparison [`splices_into`] makes, on a batch whose last edit
/// is measured against an intermediate document rather than against
/// `base`.
///
/// The conversion never builds the spliced text, so "the edits produce the
/// reported buffer" is decided by a length check plus three byte-range
/// comparisons — head, insertion, tail. Every rejection below is the same
/// length as the buffer the batch really produces except the last, so none
/// of them can be caught by the length check standing in for a comparison
/// that is missing or misaddressed.
#[test]
fn a_batch_is_accepted_only_when_its_last_edit_lands_on_the_reported_buffer() {
    let base = Snapshot::new(1, Arc::from("hello world\n"));
    let edits = [
        LspTextEdit {
            range: LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 5)),
            new_text: "goodbye".to_owned(),
        },
        // Addressed in the coordinates the first edit produced: character
        // 13 does not exist in `base` at all.
        LspTextEdit {
            range: LspRange::new(LspPosition::new(0, 8), LspPosition::new(0, 13)),
            new_text: "there".to_owned(),
        },
    ];
    assert!(build_incremental(&base, &edits, "goodbye there\n").is_ok());
    for (reported, comparison) in [
        ("goodbyE there\n", "the text before the last edit"),
        ("goodbye where\n", "the text the last edit inserts"),
        ("goodbye therex", "the text after the last edit"),
        ("goodbye there\n\n", "the length of the result"),
    ] {
        assert_eq!(
            build_incremental(&base, &edits, reported),
            Err(LspError::InconsistentEdits),
            "{reported:?} disagrees with the batch on {comparison}"
        );
    }
}

#[test]
fn snippet_tab_stops_are_expanded_in_visit_order() {
    let snippet = parse_snippet("fn ${1:name}(${2}) {\n    $0\n}");
    assert_eq!(snippet.text, "fn name() {\n    \n}");
    assert_eq!(
        snippet
            .stops
            .iter()
            .map(|stop| stop.index)
            .collect::<Vec<u32>>(),
        vec![1, 2, 0],
        "$0 is visited last"
    );
    assert_eq!(&snippet.text[snippet.stops[0].range.clone()], "name");
    assert!(snippet.stops[1].range.is_empty());

    assert_eq!(parse_snippet("cost: \\$5").text, "cost: $5");
    assert_eq!(parse_snippet("${1|first,second|}").text, "first");
    assert_eq!(parse_snippet("${TM_FILENAME}x").text, "x");
    assert_eq!(parse_snippet("${1:${2:deep}}").text, "deep");
    assert_eq!(parse_snippet("${unterminated").text, "${unterminated");
}

#[test]
fn a_document_that_was_never_opened_is_reported_rather_than_ignored() {
    let mut manager = rust_manager();
    let events = manager.events();
    let request = manager.request_hover(LspDocumentId(9), LspPosition::new(0, 0));
    let event = events.try_recv().expect("an immediate failure event");
    assert_eq!(
        event,
        LspEvent::RequestFailed {
            request,
            doc: LspDocumentId(9),
            error: LspError::UnknownDocument(9),
        }
    );
    assert_eq!(manager.status(LspDocumentId(9)), LspStatus::Disabled);
}

// -----------------------------------------------------------------
// Protocol behaviour against the fake server
// -----------------------------------------------------------------

#[test]
fn the_initialize_response_strictly_precedes_initialized() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        // This fake answers `initialize` and nothing else, so
        // `initialized` cannot appear before that response is decoded.
        let slot = install_fake(&mut manager, full_sync_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        assert_eq!(
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "fn main() {}\n"),
            LspStatus::Starting
        );

        let handle = handle_of(&slot).await;
        handle.wait_for("initialized").await;
        let methods = handle.methods();
        let initialize = methods
            .iter()
            .position(|method| method == "initialize")
            .expect("initialize was sent");
        let initialized = methods
            .iter()
            .position(|method| method == "initialized")
            .expect("initialized was sent");
        assert!(
            initialize < initialized,
            "initialize must precede initialized, saw {methods:?}"
        );
        // Both directions of Content-Length framing worked: the client
        // parsed a framed reply and the fake parsed framed requests.
        assert!(handle.written.load(Ordering::Acquire) >= 1);
        await_ready(&events, doc).await;
        assert_eq!(manager.status(doc), LspStatus::Ready);
        manager.shutdown();
    });
}

#[test]
fn shutdown_interrupts_an_unanswered_initialize() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&slot);
        manager.set_transport(Arc::new(move |_| {
            let (transport, handle) = fake_transport(|_| Vec::new());
            *sink.lock().unwrap() = Some(handle);
            Ok(transport)
        }));
        manager.open_for_test(
            LspDocumentId(1),
            &file,
            &worktree,
            rust_language(),
            1,
            "fn main() {}\n",
        );
        let handle = handle_of(&slot).await;
        handle.wait_for("initialize").await;
        tokio::time::timeout(Duration::from_secs(1), manager.begin_shutdown())
            .await
            .expect(
                "initializing servers must not hold the application open for the reap deadline",
            );
        assert!(
            !handle
                .methods()
                .iter()
                .any(|method| method == "initialized")
        );
    });
}

#[test]
fn negotiated_none_sync_suppresses_open_change_save_and_close() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        // `textDocumentSync` absent entirely: the specification default is
        // no synchronisation at all.
        let slot = install_fake(&mut manager, json!({}), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "a\n");
        let handle = handle_of(&slot).await;
        await_ready(&events, doc).await;
        manager.change(
            doc,
            2,
            &[LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 1), LspPosition::new(0, 1)),
                new_text: "b".to_owned(),
            }],
            "ab\n",
        );
        manager.save(doc, "ab\n");
        manager.close(doc);
        tokio::time::sleep(CHANGE_DEBOUNCE * 4).await;
        let methods = handle.methods();
        for suppressed in [
            "textDocument/didOpen",
            "textDocument/didChange",
            "textDocument/didSave",
            "textDocument/didClose",
        ] {
            assert!(
                !methods.iter().any(|method| method == suppressed),
                "{suppressed} must not be sent, saw {methods:?}"
            );
        }
        manager.shutdown();
    });
}

#[test]
fn negotiated_full_sync_sends_the_whole_document_and_the_saved_text() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, full_sync_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "a\n");
        let handle = handle_of(&slot).await;
        await_ready(&events, doc).await;
        let open = handle.wait_for("textDocument/didOpen").await;
        assert_eq!(
            open.params["textDocument"]["languageId"],
            json!("rust-analyzer")
        );

        manager.change(
            doc,
            2,
            &[LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 1), LspPosition::new(0, 1)),
                new_text: "b".to_owned(),
            }],
            "ab\n",
        );
        let change = handle.wait_for("textDocument/didChange").await;
        let changes = change.params["contentChanges"]
            .as_array()
            .expect("contentChanges");
        assert_eq!(changes.len(), 1);
        assert!(
            changes[0].get("range").is_none(),
            "full sync must omit ranges, saw {:?}",
            changes[0]
        );
        assert_eq!(changes[0]["text"], json!("ab\n"));

        manager.save(doc, "ab\n");
        let save = handle.wait_for("textDocument/didSave").await;
        assert_eq!(save.params["text"], json!("ab\n"));

        manager.close(doc);
        handle.wait_for("textDocument/didClose").await;
        manager.shutdown();
    });
}

#[test]
fn negotiated_incremental_sync_sends_converted_ranges_and_increasing_versions() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "a𝄞b\n");
        let handle = handle_of(&slot).await;
        await_ready(&events, doc).await;
        let open = handle.wait_for("textDocument/didOpen").await;
        assert_eq!(open.params["textDocument"]["version"], json!(1));

        manager.change(
            doc,
            2,
            &[LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 2), LspPosition::new(0, 3)),
                new_text: "B".to_owned(),
            }],
            "a𝄞B\n",
        );
        let first = handle.wait_for("textDocument/didChange").await;
        assert_eq!(first.params["textDocument"]["version"], json!(2));
        let changes = first.params["contentChanges"]
            .as_array()
            .expect("contentChanges");
        assert_eq!(
            changes[0]["range"],
            json!({
                "start": { "line": 0, "character": 3 },
                "end": { "line": 0, "character": 4 },
            }),
            "the astral scalar shifts UTF-16 offsets by one"
        );
        assert_eq!(changes[0]["text"], json!("B"));

        // The editor re-uses version 2; the wire version still advances.
        manager.change(doc, 2, &[], "a𝄞BC\n");
        let all = handle.wait_for_count("textDocument/didChange", 2).await;
        let versions: Vec<i64> = all
            .iter()
            .map(|entry| {
                entry.params["textDocument"]["version"]
                    .as_i64()
                    .expect("version")
            })
            .collect();
        assert!(
            versions.windows(2).all(|pair| pair[0] < pair[1]),
            "versions must strictly increase, saw {versions:?}"
        );
        // An edit batch that cannot be converted resynchronises the whole
        // document instead of sending a wrong range.
        manager.change(
            doc,
            9,
            &[LspTextEdit {
                range: LspRange::new(LspPosition::new(40, 0), LspPosition::new(40, 1)),
                new_text: "z".to_owned(),
            }],
            "resynchronised\n",
        );
        let desync = next_matching(&events, |event| match event {
            LspEvent::DocumentDesynced { doc: at, error, .. } if *at == doc => Some(error.clone()),
            _ => None,
        })
        .await;
        assert!(matches!(desync, LspError::PositionOutOfRange { .. }));
        let resend = handle.wait_for_count("textDocument/didChange", 3).await;
        let last = resend.last().expect("third change");
        let changes = last.params["contentChanges"]
            .as_array()
            .expect("contentChanges");
        assert!(
            changes[0].get("range").is_none(),
            "an unconvertible batch falls back to a whole-document change"
        );
        assert_eq!(changes[0]["text"], json!("resynchronised\n"));
        manager.shutdown();
    });
}

fn native_source(text: &str) -> TextSnapshot {
    crate::repository_widget::text_editor::Content::<cosmic::Renderer>::with_text(text)
        .text_snapshot()
}

#[test]
fn native_changes_during_root_discovery_remain_lazy_until_worker_open() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let (release, waiting) = tokio::sync::oneshot::channel::<()>();
        let mut manager = rust_manager().after_shutdown(async move {
            let _ = waiting.await;
        });
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        let initial = native_source("before");
        let changed = native_source("after");
        manager.open_snapshot(doc, &file, &worktree, rust_language(), 1, initial.clone());
        manager.change_snapshot(doc, 2, &[], changed.clone());
        assert!(!initial.is_materialized());
        assert!(!changed.is_materialized());
        let (ticket, root) = next_matching(&events, |event| match event {
            LspEvent::RootDiscovered {
                doc: at,
                ticket,
                root,
            } if *at == doc => Some((*ticket, root.clone())),
            _ => None,
        })
        .await;
        manager.finish_discovery(doc, ticket, root);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            slot.lock().unwrap().is_none(),
            "replacement must wait for the old manager to reap"
        );
        assert_eq!(initial.serialization_count(), 0);
        assert_eq!(changed.serialization_count(), 0);
        release.send(()).unwrap();
        await_ready(&events, doc).await;
        let fake = handle_of(&slot).await;
        let opened = fake.wait_for("textDocument/didOpen").await;
        assert_eq!(opened.params["textDocument"]["text"], "after");
        assert!(!initial.is_materialized());
        assert!(!changed.is_materialized());
        assert_eq!(initial.serialization_count(), 0);
        assert_eq!(changed.serialization_count(), 1);
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn native_incremental_typing_never_materializes_full_text() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_source_at_root(
            doc,
            &file,
            &worktree,
            rust_language(),
            1,
            Arc::new(Snapshot::from_native(1, native_source("a😀\nnext\n"))),
            worktree.clone(),
            Arc::default(),
        );
        await_ready(&events, doc).await;
        let source = native_source("a😀x\nnext\n");
        let edit = LspTextEdit {
            range: LspRange::new(LspPosition::new(0, 2), LspPosition::new(0, 2)),
            new_text: "x".into(),
        };
        manager.change_snapshot(doc, 2, &[edit], source.clone());
        assert!(!source.is_materialized());
        let fake = handle_of(&slot).await;
        let change = fake.wait_for("textDocument/didChange").await;
        assert_eq!(
            change.params["contentChanges"][0]["range"]["start"]["character"],
            3
        );
        assert_eq!(change.params["contentChanges"][0]["text"], "x");
        assert!(!source.is_materialized());
        let next = native_source("a😀x\nzero\nnext\n");
        manager.change_snapshot(
            doc,
            3,
            &[LspTextEdit {
                range: LspRange::new(LspPosition::new(1, 0), LspPosition::new(1, 0)),
                new_text: "zero\n".into(),
            }],
            next.clone(),
        );
        let changes = fake.wait_for_count("textDocument/didChange", 2).await;
        assert_eq!(changes[1].params["contentChanges"][0]["text"], "zero\n");
        assert!(!source.is_materialized());
        assert!(!next.is_materialized());
        assert_eq!(source.serialization_count(), 0);
        assert_eq!(next.serialization_count(), 0);
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn native_full_sync_materializes_only_the_latest_debounced_source() {
    block_on(async {
        let (_root, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, full_sync_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_source_at_root(
            doc,
            &file,
            &worktree,
            rust_language(),
            1,
            Arc::new(Snapshot::from_native(1, native_source("a"))),
            worktree.clone(),
            Arc::default(),
        );
        await_ready(&events, doc).await;
        let mut sources = Vec::new();
        for version in 2..8 {
            let source = native_source(&"a".repeat(version as usize));
            manager.change_snapshot(doc, version, &[], source.clone());
            assert!(!source.is_materialized());
            sources.push(source);
        }
        let fake = handle_of(&slot).await;
        let change = fake.wait_for("textDocument/didChange").await;
        assert_eq!(change.params["contentChanges"][0]["text"], "aaaaaaa");
        assert_eq!(fake.entries("textDocument/didChange").len(), 1);
        assert_eq!(sources.last().unwrap().serialization_count(), 1);
        assert!(sources.iter().all(|source| !source.is_materialized()));
        assert!(
            sources[..sources.len() - 1]
                .iter()
                .all(|source| source.serialization_count() == 0)
        );
        manager.shutdown();
        manager.wait_for_shutdown().await;
    });
}

#[test]
fn native_incremental_ranges_are_verified_against_the_immutable_result() {
    let base = Snapshot::from_native(1, native_source("before"));
    let expected = native_source("incorrect");
    assert!(
        native_incremental(
            &base,
            &[LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 6)),
                new_text: "after".into()
            }],
            &expected
        )
        .is_none()
    );
    assert!(!expected.is_materialized());
    assert!(!base.native.unwrap().is_materialized());
}

#[test]
fn native_group_ranges_use_persistent_intermediate_lines() {
    let base = Snapshot::from_native(1, native_source("a😀\nlast"));
    let changes = native_incremental(
        &base,
        &[
            LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 0)),
                new_text: "first\n".into(),
            },
            LspTextEdit {
                range: LspRange::new(LspPosition::new(1, 2), LspPosition::new(1, 2)),
                new_text: "x".into(),
            },
        ],
        &native_source("first\na😀x\nlast"),
    )
    .unwrap();
    assert_eq!(
        changes[1].range.unwrap().start,
        lsp_types::Position::new(1, 3)
    );
    assert!(!base.native.unwrap().is_materialized());
}

#[test]
fn debounced_edits_coalesce_into_one_notification() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "");
        let handle = handle_of(&slot).await;
        await_ready(&events, doc).await;
        handle.wait_for("textDocument/didOpen").await;

        let mut text = String::new();
        for step in 0..5u32 {
            text.push('x');
            manager.change(
                doc,
                i32::try_from(step).expect("small") + 2,
                &[LspTextEdit {
                    range: LspRange::new(LspPosition::new(0, step), LspPosition::new(0, step)),
                    new_text: "x".to_owned(),
                }],
                &text,
            );
        }
        let change = handle.wait_for("textDocument/didChange").await;
        assert_eq!(
            change.params["contentChanges"]
                .as_array()
                .expect("contentChanges")
                .len(),
            5,
            "five debounced edits arrive as one notification"
        );
        assert_eq!(change.params["textDocument"]["version"], json!(6));
        assert_eq!(
            handle.entries("textDocument/didChange").len(),
            1,
            "debouncing must not produce one notification per keystroke"
        );
        manager.shutdown();
    });
}

#[test]
fn diagnostics_round_trip_into_editor_coordinates() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "a𝄞b\n");
        let handle = handle_of(&slot).await;
        await_ready(&events, doc).await;
        let uri = Url::from_file_path(&file).expect("uri").to_string();
        handle.notify(
            "textDocument/publishDiagnostics",
            json!({
                "uri": uri,
                "version": 1,
                "diagnostics": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 3 },
                            "end": { "line": 0, "character": 4 },
                        },
                        "severity": 2,
                        "code": "E0001",
                        "source": "fake",
                        "message": "watch out",
                    },
                    {
                        "range": {
                            "start": { "line": 44, "character": 0 },
                            "end": { "line": 44, "character": 1 },
                        },
                        "message": "outside the snapshot",
                    },
                ],
            }),
        );
        let (version, items) = next_matching(&events, |event| match event {
            LspEvent::Diagnostics {
                doc: at,
                version,
                items,
            } if *at == doc => Some((*version, items.clone())),
            _ => None,
        })
        .await;
        assert_eq!(version, 1);
        assert_eq!(
            items.len(),
            1,
            "an unconvertible item is dropped, not the whole batch"
        );
        assert_eq!(
            items[0].range,
            LspRange::new(LspPosition::new(0, 2), LspPosition::new(0, 3)),
            "UTF-16 offsets convert back to scalar offsets"
        );
        assert_eq!(items[0].severity, LspSeverity::Warning);
        assert_eq!(
            items[0].code,
            Some(lsp_types::NumberOrString::String("E0001".into()))
        );
        assert_eq!(items[0].source.as_deref(), Some("fake"));
        manager.shutdown();
    });
}

#[test]
fn hover_completion_definition_and_references_round_trip() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let outside = tempfile::tempdir().expect("outside tempdir");
        let outside_file = outside.path().join("vendor.rs");
        let document_uri = Url::from_file_path(&file).expect("uri").to_string();
        let outside_uri = Url::from_file_path(&outside_file).expect("uri").to_string();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, incremental_capabilities(), move |incoming| {
            let result = match incoming.method.as_str() {
                "textDocument/hover" => json!({
                    "contents": { "kind": "markdown", "value": "**docs**" },
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 },
                    },
                }),
                "textDocument/completion" => json!([
                    {
                        "label": "edited",
                        "kind": 3,
                        "detail": "fn()",
                        "documentation": "call it",
                        "preselect": true,
                        "textEdit": {
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 1 },
                            },
                            "newText": "replacement",
                        },
                    },
                    { "label": "insert_text_item", "insertText": "from_insert_text" },
                    { "label": "label_only" },
                    {
                        "label": "snippet",
                        "insertTextFormat": 2,
                        "insertText": "wrap(${1:arg})$0",
                    },
                    {
                        "label": "insert_replace",
                        "textEdit": {
                            "newText": "both",
                            "insert": {
                                "start": { "line": 0, "character": 1 },
                                "end": { "line": 0, "character": 1 },
                            },
                            "replace": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 2 },
                            },
                        },
                    },
                ]),
                "textDocument/definition" => json!({
                    "uri": document_uri,
                    "range": {
                        "start": { "line": 0, "character": 1 },
                        "end": { "line": 0, "character": 2 },
                    },
                }),
                "textDocument/references" => json!([
                    {
                        "uri": outside_uri,
                        "range": {
                            "start": { "line": 4, "character": 0 },
                            "end": { "line": 4, "character": 3 },
                        },
                    },
                    {
                        "uri": "jdt://contents/Thing.class",
                        "range": {
                            "start": { "line": 1, "character": 0 },
                            "end": { "line": 1, "character": 1 },
                        },
                    },
                ]),
                _ => return Vec::new(),
            };
            vec![json!({ "jsonrpc": "2.0", "id": incoming.id, "result": result })]
        });
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abcdef\n");
        let _handle = handle_of(&slot).await;
        await_ready(&events, doc).await;

        let hover = manager.request_hover(doc, LspPosition::new(0, 1));
        let (markdown, range) = next_matching(&events, |event| match event {
            LspEvent::Hover {
                request,
                markdown,
                range,
                ..
            } if *request == hover => Some((markdown.clone(), *range)),
            _ => None,
        })
        .await;
        assert_eq!(markdown.as_deref(), Some("**docs**"));
        assert_eq!(
            range,
            Some(LspRange::new(
                LspPosition::new(0, 0),
                LspPosition::new(0, 1)
            ))
        );

        let completion = manager.request_completion(doc, LspPosition::new(0, 3));
        let items = next_matching(&events, |event| match event {
            LspEvent::Completion { request, items, .. } if *request == completion => {
                Some(items.clone())
            }
            _ => None,
        })
        .await;
        assert_eq!(items.len(), 5);
        // `textEdit` wins over everything else.
        assert_eq!(items[0].edit.new_text, "replacement");
        assert_eq!(
            items[0].edit.range,
            LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 1))
        );
        assert_eq!(items[0].kind, Some(LspCompletionKind::Function));
        assert!(items[0].preselect);
        assert_eq!(items[0].documentation.as_deref(), Some("call it"));
        // Then `insertText`, replacing the identifier prefix.
        assert_eq!(items[1].edit.new_text, "from_insert_text");
        assert_eq!(
            items[1].edit.range,
            LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 3)),
            "the prefix `abc` before the cursor is replaced"
        );
        // Then `label`.
        assert_eq!(items[2].edit.new_text, "label_only");
        // Snippets expand to literal text plus tab stops.
        assert_eq!(items[3].edit.new_text, "wrap(arg)");
        let snippet = items[3].edit.snippet.as_ref().expect("snippet");
        assert_eq!(snippet.text, "wrap(arg)");
        assert_eq!(
            snippet
                .stops
                .iter()
                .map(|stop| stop.index)
                .collect::<Vec<u32>>(),
            vec![1, 0]
        );
        assert!(items[0].edit.snippet.is_none(), "plain text stays plain");
        // `InsertReplaceEdit` exposes both ranges so the editor chooses.
        assert_eq!(
            items[4].edit.range,
            LspRange::new(LspPosition::new(0, 1), LspPosition::new(0, 1))
        );
        assert_eq!(
            items[4].edit.replace_range,
            Some(LspRange::new(
                LspPosition::new(0, 0),
                LspPosition::new(0, 2)
            ))
        );

        let definition = manager.request_definition(doc, LspPosition::new(0, 1));
        let items = next_matching(&events, |event| match event {
            LspEvent::Locations {
                request,
                kind,
                items,
                ..
            } if *request == definition && *kind == LocationKind::Definition => Some(items.clone()),
            _ => None,
        })
        .await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].target, LspTarget::Workspace { path: file.clone() });
        assert_eq!(items[0].coordinates, LspCoordinates::Scalar);

        let references = manager.request_references(doc, LspPosition::new(0, 1));
        let items = next_matching(&events, |event| match event {
            LspEvent::Locations {
                request,
                kind,
                items,
                ..
            } if *request == references && *kind == LocationKind::References => Some(items.clone()),
            _ => None,
        })
        .await;
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].target,
            LspTarget::External {
                path: outside_file.clone()
            },
            "a file outside the worktree opens read-only"
        );
        assert_eq!(
            items[0].coordinates,
            LspCoordinates::Utf16,
            "a target whose text is not open keeps protocol coordinates"
        );
        assert_eq!(
            items[1].target,
            LspTarget::Uri {
                uri: "jdt://contents/Thing.class".to_owned()
            },
            "a non-file URI is listed but never written"
        );
        manager.shutdown();
    });
}

#[test]
fn a_result_computed_for_an_older_version_is_rejected() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        // The fake holds the hover reply until the document has already
        // changed, which is exactly the race the version check exists for.
        let held: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
        let observer = Arc::clone(&held);
        let slot =
            install_fake(
                &mut manager,
                incremental_capabilities(),
                move |incoming| match incoming.method.as_str() {
                    "textDocument/hover" => {
                        *observer.lock().expect("held") = incoming.id;
                        Vec::new()
                    }
                    "textDocument/didChange" => match observer.lock().expect("held").take() {
                        Some(id) => vec![json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "contents": "computed for version 1" },
                        })],
                        None => Vec::new(),
                    },
                    _ => Vec::new(),
                },
            );
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
        let handle = handle_of(&slot).await;
        await_ready(&events, doc).await;

        let hover = manager.request_hover(doc, LspPosition::new(0, 1));
        handle.wait_for("textDocument/hover").await;
        manager.change(
            doc,
            2,
            &[LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 3)),
                new_text: "d".to_owned(),
            }],
            "abcd\n",
        );
        let error = next_matching(&events, |event| match event {
            LspEvent::RequestFailed { request, error, .. } if *request == hover => {
                Some(error.clone())
            }
            _ => None,
        })
        .await;
        assert_eq!(
            error,
            LspError::StaleVersion {
                result: 1,
                current: 2
            },
            "a hover computed for version 1 must not be applied to version 2"
        );
        manager.shutdown();
    });
}

#[test]
fn a_superseded_request_is_cancelled_on_the_wire() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        // Completion is never answered, so the first request stays in
        // flight until it is superseded.
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
        let handle = handle_of(&slot).await;
        await_ready(&events, doc).await;

        let first = manager.request_completion(doc, LspPosition::new(0, 1));
        let sent = handle.wait_for("textDocument/completion").await;
        let wire_id = sent.id.expect("requests carry a JSON-RPC id");
        let second = manager.request_completion(doc, LspPosition::new(0, 2));
        assert_ne!(first, second);

        let cancel = handle.wait_for("$/cancelRequest").await;
        assert_eq!(
            cancel.params["id"],
            json!(wire_id),
            "the cancellation must name the superseded JSON-RPC id"
        );
        let error = next_matching(&events, |event| match event {
            LspEvent::RequestFailed { request, error, .. } if *request == first => {
                Some(error.clone())
            }
            _ => None,
        })
        .await;
        assert_eq!(error, LspError::Cancelled);

        // An explicit cancel reaches the wire with the newer id.
        let second_sent = handle.wait_for_count("textDocument/completion", 2).await;
        let second_id = second_sent.last().expect("second request").id.expect("id");
        assert_ne!(second_id, wire_id, "each request gets a fresh id");
        manager.cancel(second);
        let cancels = handle.wait_for_count("$/cancelRequest", 2).await;
        assert_eq!(
            cancels.last().expect("second cancel").params["id"],
            json!(second_id)
        );
        manager.shutdown();
    });
}

#[test]
fn the_event_channel_stays_bounded_under_diagnostic_flooding() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
        let handle = handle_of(&slot).await;
        await_ready(&events, doc).await;
        let uri = Url::from_file_path(&file).expect("uri").to_string();

        // Comfortably past the channel bound without turning this test
        // into a load generator for the rest of the suite.
        const BATCHES: u32 = 600;
        for index in 0..BATCHES {
            handle.notify(
                "textDocument/publishDiagnostics",
                json!({
                    "uri": uri,
                    "diagnostics": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 1 },
                        },
                        "message": format!("batch {index}"),
                    }],
                }),
            );
        }
        // Nothing drains the channel while the flood is processed.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(
                events.len() <= EVENT_CHANNEL_CAPACITY,
                "the event channel grew past its bound: {}",
                events.len()
            );
            if events.len() == EVENT_CHANNEL_CAPACITY {
                break;
            }
        }
        assert_eq!(
            events.len(),
            EVENT_CHANNEL_CAPACITY,
            "the flood should have filled the channel exactly to its bound"
        );
        // Coalescing keeps the newest batch per document, so the newest
        // diagnostics still reach the application once draining resumes.
        let newest = format!("batch {}", BATCHES - 1);
        let mut seen_newest = false;
        for _ in 0..(EVENT_CHANNEL_CAPACITY * 8) {
            let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(3), events.recv()).await
            else {
                break;
            };
            if let LspEvent::Diagnostics { items, .. } = event
                && items.first().is_some_and(|item| item.message == newest)
            {
                seen_newest = true;
                break;
            }
        }
        assert!(
            seen_newest,
            "the newest diagnostics must survive coalescing"
        );
        manager.shutdown();
    });
}

#[test]
fn the_stderr_tail_is_bounded_in_lines_and_in_line_length() {
    block_on(async {
        let (mut writer, reader) = tokio::io::duplex(1 << 16);
        let tail = Arc::new(StderrTail::default());
        let drain = tokio::spawn(drain_stderr(Box::new(reader), Arc::clone(&tail)));
        for index in 0..1_000u32 {
            writer
                .write_all(format!("line {index}\n").as_bytes())
                .await
                .expect("write");
        }
        // One enormous line with no terminator until the very end.
        writer.write_all(&vec![b'x'; 200_000]).await.expect("write");
        writer.write_all(b"\n").await.expect("write");
        drop(writer);
        let _ = tokio::time::timeout(WAIT, drain).await;

        let lines = tail.snapshot();
        assert_eq!(lines.len(), STDERR_TAIL_LINES, "only the tail is retained");
        assert!(
            lines
                .iter()
                .all(|line| line.len() <= STDERR_TAIL_LINE_BYTES),
            "an unterminated line must not grow the tail without bound"
        );
        assert!(
            lines.iter().any(|line| line == "line 999"),
            "the newest lines are the ones kept"
        );
    });
}

#[test]
fn a_crashed_server_reports_a_restartable_status_and_keeps_the_document() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        // A malformed frame after `initialized` fails the client's main
        // loop exactly the way a crashed server does.
        let slot = install_fake(&mut manager, incremental_capabilities(), |incoming| {
            if incoming.method == "initialized" {
                return vec![json!({ "jsonrpc": "2.0", "method": 42 })];
            }
            Vec::new()
        });
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
        let _handle = handle_of(&slot).await;
        let status = next_matching(&events, |event| match event {
            LspEvent::Status {
                doc: at, status, ..
            } if *at == doc => match status {
                LspStatus::Failed { .. } => Some(status.clone()),
                _ => None,
            },
            _ => None,
        })
        .await;
        assert!(status.can_restart(), "a crash must offer Restart");
        assert_eq!(manager.status(doc), status);

        // Editing keeps working: the buffer is still tracked and the
        // snapshot advances even though no server is listening.
        manager.change(
            doc,
            2,
            &[LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 3)),
                new_text: "d".to_owned(),
            }],
            "abcd\n",
        );
        let uri = Url::from_file_path(&file).expect("uri");
        assert_eq!(
            manager
                .snapshots
                .latest(&uri)
                .expect("the snapshot survives the crash")
                .full_text()
                .as_ref(),
            "abcd\n"
        );

        *slot.lock().expect("transport slot") = None;
        manager.restart(doc);
        let restarted = handle_of(&slot).await;
        restarted.wait_for("initialize").await;
        let reopened = restarted.wait_for("textDocument/didOpen").await;
        assert_eq!(
            reopened.params["textDocument"]["text"],
            json!("abcd\n"),
            "a restart re-opens the document with the current buffer"
        );
        manager.shutdown();
    });
}

/// Brings up a ready session and hands back the fake behind it, so a test
/// can put arbitrary bytes on the wire of a server the client trusts.
async fn ready_session(
    manager: &mut LspManager,
    worktree: &Path,
    file: &Path,
    doc: LspDocumentId,
) -> (FakeHandle, async_channel::Receiver<LspEvent>) {
    let slot = install_fake(manager, incremental_capabilities(), |_| Vec::new());
    let events = manager.events();
    manager.open_for_test(doc, file, worktree, rust_language(), 1, "abc\n");
    let handle = handle_of(&slot).await;
    await_ready(&events, doc).await;
    (handle, events)
}

async fn failure_reason(events: &async_channel::Receiver<LspEvent>, doc: LspDocumentId) -> String {
    next_matching(events, |event| match event {
        LspEvent::Status {
            doc: at,
            status: LspStatus::Failed { reason, .. },
            ..
        } if *at == doc => Some(reason.clone()),
        _ => None,
    })
    .await
}

#[test]
fn an_absurd_content_length_fails_the_session_instead_of_being_allocated() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let doc = LspDocumentId(1);
        let (handle, events) = ready_session(&mut manager, &worktree, &file, doc).await;
        // A terabyte, declared before a single body byte exists. The
        // framing layer allocates the declared length up front, and a
        // failed allocation aborts this process rather than unwinding.
        handle.write_raw(format!("Content-Length: {}\r\n\r\n", 1u64 << 40).into_bytes());
        let reason = failure_reason(&events, doc).await;
        assert!(
            reason.contains("1099511627776"),
            "the reported failure names the length the server declared: {reason}"
        );
        assert!(
            manager.status(doc).can_restart(),
            "a server that misframes is restartable like any other failure"
        );
    });
}

#[test]
fn a_header_line_that_never_ends_fails_the_session_instead_of_being_buffered() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let doc = LspDocumentId(1);
        let (handle, events) = ready_session(&mut manager, &worktree, &file, doc).await;
        // No newline anywhere: the header reader accumulates until one
        // arrives, so this is the other way stdout grows without bound.
        handle.write_raw(vec![b'x'; 32 * 1024]);
        let reason = failure_reason(&events, doc).await;
        assert!(
            reason.contains("header line"),
            "the reported failure says the header never ended: {reason}"
        );
    });
}

/// Feeds `bytes` through the framing guard in small reads, reporting what
/// a reader downstream would have seen and how the stream ended.
async fn through_guard(bytes: &[u8]) -> (Vec<u8>, io::Result<()>) {
    let (mut writer, reader) = tokio::io::duplex(1 << 20);
    writer.write_all(bytes).await.expect("write");
    drop(writer);
    let mut guarded = GuardedFrames::new(reader);
    let mut seen = Vec::new();
    let mut chunk = [0u8; 128];
    loop {
        match guarded.read(&mut chunk).await {
            Ok(0) => return (seen, Ok(())),
            Ok(count) => seen.extend_from_slice(&chunk[..count]),
            Err(error) => return (seen, Err(error)),
        }
    }
}

#[test]
fn the_framing_guard_judges_headers_and_never_message_bodies() {
    block_on(async {
        // A body carrying a newline and something that reads like a
        // header: anything scanning message content would desync here.
        let body = "{\"note\":\"x\nContent-Length: 1099511627776\r\n\"}";
        let mut stream = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        stream.push_str(&format!("Content-Length: {MAX_CONTENT_LENGTH}\r\n\r\n"));
        let (seen, outcome) = through_guard(stream.as_bytes()).await;
        assert!(outcome.is_ok(), "well-formed framing passes: {outcome:?}");
        assert_eq!(
            seen.as_slice(),
            stream.as_bytes(),
            "every byte reaches the reader downstream unchanged"
        );

        // Ordinary garbage stays the framing layer's to reject, so the
        // reason it reports remains the accurate one.
        let (seen, outcome) = through_guard(b"HTTP/1.1 200 OK\r\n\r\n").await;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(seen.as_slice(), &b"HTTP/1.1 200 OK\r\n\r\n"[..]);

        let (_, outcome) = through_guard(b"Content-Length: 1099511627776\r\n\r\n").await;
        let error = outcome.expect_err("a terabyte is refused");
        assert!(error.to_string().contains("1099511627776"), "{error}");

        // A digit string too large to be a length at all is the same
        // refusal, not a parse that wraps into something plausible.
        let overflowing = format!("Content-Length: {}\r\n\r\n", "9".repeat(40));
        let (_, outcome) = through_guard(overflowing.as_bytes()).await;
        assert!(outcome.is_err(), "an unrepresentable length is refused");

        // No newline anywhere: the reader downstream would accumulate
        // this for as long as the server kept writing.
        let (_, outcome) = through_guard(&vec![b'x'; 8 * MAX_HEADER_LINE_BYTES]).await;
        let error = outcome.expect_err("an endless header line is refused");
        assert!(error.to_string().contains("header line"), "{error}");
    });
}

/// A length is whatever the framing layer will read as one, which is not
/// the same set of strings as "digits".
#[test]
fn a_content_length_the_framing_layer_accepts_is_a_length_here_too() {
    block_on(async {
        // `usize::from_str` accepts a leading `+`, so `async_lsp` reads a
        // 60-byte body here. A guard that judged the value by its digits
        // would call this header none of its business, lose track of the
        // body, and never see the terabyte behind it as a header at all.
        let body = "x".repeat(60);
        let mut stream = format!("Content-Length: +{}\r\n\r\n{body}", body.len());
        stream.push_str("Content-Length: 1099511627776\r\n\r\n");
        let (_, outcome) = through_guard(stream.as_bytes()).await;
        let error = outcome.expect_err("the frame behind a signed length is still judged");
        assert!(error.to_string().contains("1099511627776"), "{error}");

        // And the signed length is itself capped, not merely tracked.
        let absurd = format!("Content-Length: +{}\r\n\r\n", 1u64 << 40);
        let (_, outcome) = through_guard(absurd.as_bytes()).await;
        assert!(outcome.is_err(), "a signed absurd length is refused too");

        // A `Content-Length` the framing layer cannot read at all is a
        // protocol error there, so passing it on buys nothing and costs
        // the cap: refusing it keeps the two in step by construction.
        let (_, outcome) = through_guard(b"Content-Length: 60 \r\n\r\n").await;
        assert!(outcome.is_err(), "a length that is not one is refused");
    });
}

#[test]
fn shutdown_requests_shutdown_before_notifying_exit() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
        let handle = handle_of(&slot).await;
        await_ready(&events, doc).await;
        manager.shutdown();
        handle.wait_for("exit").await;
        let methods = handle.methods();
        let shutdown = methods
            .iter()
            .position(|method| method == "shutdown")
            .expect("shutdown was requested");
        let exit = methods
            .iter()
            .position(|method| method == "exit")
            .expect("exit was notified");
        assert!(
            shutdown < exit,
            "shutdown must precede exit, saw {methods:?}"
        );
    });
}

#[test]
fn initialization_options_reach_the_server_before_document_open() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let options = json!({
            "checkOnSave": false,
            "cargo": {"buildScripts": {"enable": false}, "extraArgs": ["--locked", "--offline"]},
            "procMacro": {"enable": false}
        });
        manager.servers[0].initialization_options = Some(options.clone());
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "fn f() {}\n");
        await_ready(&events, doc).await;
        let fake = handle_of(&slot).await;
        fake.wait_for("textDocument/didOpen").await;
        assert_eq!(
            fake.entries("initialize")[0].params["initializationOptions"],
            options
        );
        let methods = fake.methods();
        assert!(
            methods.iter().position(|method| method == "initialize")
                < methods
                    .iter()
                    .position(|method| method == "textDocument/didOpen")
        );
        manager.begin_shutdown().await;
    });
}

#[test]
fn a_missing_executable_reports_unavailable_and_leaves_editing_working() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        // The real launcher runs here: nothing with this name can exist.
        let mut manager = LspManager::new(vec![LanguageServerSettings {
            initialization_options: None,
            id: "rust-analyzer".to_owned(),
            languages: vec![crate::settings::LanguageServerLanguage {
                language_id: "rust".to_owned(),
                extensions: vec!["rs".to_owned()],
            }],
            command: vec!["cagents-no-such-language-server".to_owned()],
            root_markers: vec!["Cargo.toml".to_owned()],
        }]);
        let events = manager.events();
        let doc = LspDocumentId(1);
        // Construction never probes PATH, so opening is what discovers it.
        assert_eq!(
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n"),
            LspStatus::Starting
        );
        let status = next_matching(&events, |event| match event {
            LspEvent::Status {
                doc: at, status, ..
            } if *at == doc => match status {
                LspStatus::Unavailable { .. } => Some(status.clone()),
                _ => None,
            },
            _ => None,
        })
        .await;
        let LspStatus::Unavailable { reason } = &status else {
            panic!("expected Unavailable, got {status:?}");
        };
        assert!(
            reason.contains("cagents-no-such-language-server"),
            "the reason must name the executable: {reason}"
        );
        assert!(status.can_restart());
        manager.change(doc, 2, &[], "abcd\n");
        assert_eq!(
            manager
                .snapshots
                .latest(&Url::from_file_path(&file).expect("uri"))
                .expect("snapshot")
                .full_text()
                .as_ref(),
            "abcd\n",
            "editing continues without a server"
        );
        manager.shutdown();
    });
}

/// Writes a file inside `directory` that this process could execute.
#[cfg(unix)]
fn executable(directory: &Path, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join(name);
    std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write program");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("mark executable");
    path
}

#[test]
#[cfg(unix)]
fn a_bare_program_never_resolves_against_the_directory_the_child_will_run_in() {
    use std::ffi::OsString;

    let home = tempfile::tempdir().expect("tempdir");
    let worktree = tempfile::tempdir().expect("tempdir");
    let decoy = tempfile::tempdir().expect("tempdir");
    // The repository ships a program under the configured name, and an
    // earlier PATH entry holds a same-named file that is not executable.
    executable(worktree.path(), "cagents-server");
    std::fs::write(decoy.path().join("cagents-server"), "not a program\n").expect("write");

    // POSIX reads a trailing empty PATH entry as "the current directory",
    // and the child's current directory is the worktree - which is exactly
    // why the lookup may not be left to the child.
    let path = OsString::from(format!("{}:", decoy.path().display()));
    assert_eq!(
        resolve_program_in("cagents-server", Some(path.as_os_str()), home.path()),
        Err("`cagents-server` was not found on PATH".to_owned()),
        "neither a non-executable match nor the worktree may satisfy the lookup"
    );

    let mine = executable(home.path(), "cagents-server");
    let resolved = resolve_program_in("cagents-server", Some(path.as_os_str()), home.path())
        .expect("resolved");
    assert_eq!(resolved, mine, "the application's own directory decides");
    assert!(
        resolved.is_absolute(),
        "an absolute program cannot be reinterpreted by the child's chdir"
    );
}

#[test]
#[cfg(unix)]
fn a_command_naming_a_path_is_anchored_to_the_application_directory() {
    let home = tempfile::tempdir().expect("tempdir");
    let relative = resolve_program_in("bin/server", None, home.path()).expect("resolved");
    assert_eq!(relative, home.path().join("bin/server"));
    assert!(relative.is_absolute());
    // An absolute command is already anchored and is left alone.
    assert_eq!(
        resolve_program_in("/usr/bin/server", None, home.path()),
        Ok(PathBuf::from("/usr/bin/server"))
    );
}

#[test]
fn every_manager_has_its_own_event_channel_generation() {
    let manager = LspManager::new(Vec::new());
    assert_eq!(manager.generation(), manager.generation());
    assert_ne!(
        manager.generation(),
        LspManager::new(Vec::new()).generation(),
        "a replacement manager carries a different channel"
    );
}

#[test]
#[cfg(unix)]
fn a_non_utf8_path_is_refused_instead_of_being_mangled() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let mut manager = rust_manager();
    let path = PathBuf::from(OsString::from_vec(vec![
        b'/', b't', b'm', b'p', b'/', 0xff, b'.', b'r', b's',
    ]));
    let status = manager.open_for_test(
        LspDocumentId(1),
        &path,
        Path::new("/tmp"),
        rust_language(),
        1,
        "",
    );
    let LspStatus::Unavailable { reason } = status else {
        panic!("a non-UTF-8 path must be refused, got {status:?}");
    };
    assert!(
        reason.contains("not valid UTF-8"),
        "the reason must say why: {reason}"
    );
}

#[test]
fn an_unknown_server_id_is_refused_without_starting_a_session() {
    let mut manager = rust_manager();
    let status = manager.open_for_test(
        LspDocumentId(1),
        Path::new("/tmp/a.rs"),
        Path::new("/tmp"),
        LspLanguage {
            server_id: "not-configured".to_owned(),
            language_id: "rust".to_owned(),
        },
        1,
        "",
    );
    let LspStatus::Unavailable { reason } = status else {
        panic!("expected Unavailable, got {status:?}");
    };
    assert!(reason.contains("not-configured"), "{reason}");
}

// -----------------------------------------------------------------
// Real child processes, reaped without needing a language server
// -----------------------------------------------------------------

/// Helper body executed only when this test binary is re-invoked as a
/// child by `a_live_child_is_group_terminated_and_reaped`. As a normal
/// test it returns immediately.
#[test]
fn child_helper_sleeps_until_terminated() {
    if std::env::var_os(CHILD_HELPER_ENV).is_none() {
        return;
    }
    std::thread::sleep(Duration::from_secs(120));
}

/// Helper body executed only when this test binary is re-invoked as a
/// child by `a_prompt_exit_still_ends_the_server_process_group`.
///
/// Starts a grandchild that outlives it, publishes that grandchild's pid,
/// and returns at once. The grandchild inherits this process's group,
/// which is the shape a language server has: `rust-analyzer` starts a
/// proc-macro server that goes on running after the server itself has
/// answered `exit` and gone.
#[test]
fn child_helper_leaves_a_grandchild_behind() {
    let Some(published) = std::env::var_os(GRANDCHILD_ENV) else {
        return;
    };
    let published = PathBuf::from(published);
    let executable = std::env::current_exe().expect("the test executable");
    #[allow(
        clippy::zombie_processes,
        reason = "the grandchild must outlive this helper; the stop under test ends its group"
    )]
    let grandchild = std::process::Command::new(executable)
        .args(["--exact", CHILD_HELPER_TEST, "--test-threads=1"])
        .env(CHILD_HELPER_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the grandchild");
    // Renamed into place, so the parent cannot read a half-written pid.
    let staged = published.with_extension("staged");
    std::fs::write(&staged, grandchild.id().to_string()).expect("stage the grandchild pid");
    std::fs::rename(&staged, &published).expect("publish the grandchild pid");
}

/// Which helper body a re-invoked test binary should run. The environment
/// variable is what the helper looks for; the test name is what selects it.
#[cfg(unix)]
enum Helper<'a> {
    /// Not a helper: the child does what its arguments say and exits.
    None,
    /// [`child_helper_sleeps_until_terminated`].
    Sleeps,
    /// [`child_helper_leaves_a_grandchild_behind`], publishing the
    /// grandchild's pid to this path.
    LeavesAGrandchild(&'a Path),
}

/// `reaper` decides whether Tokio may kill the child when the handle is
/// dropped. A test that must prove the session *itself* stopped the
/// process passes `false`, so nothing else can end it.
#[cfg(unix)]
fn spawn_test_child(arguments: &[&str], helper: Helper<'_>, reaper: bool) -> ChildProcess {
    let executable = std::env::current_exe().expect("the test executable");
    let mut command = tokio::process::Command::new(executable);
    command
        .args(arguments)
        .stdin(Stdio::null())
        // Discarded rather than piped: an unread pipe would kill libtest
        // with SIGPIPE the moment it printed, and this test is about the
        // termination sequence, not about protocol plumbing.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(reaper)
        .process_group(0);
    match helper {
        Helper::None => {}
        Helper::Sleeps => {
            command.env(CHILD_HELPER_ENV, "1");
        }
        Helper::LeavesAGrandchild(published) => {
            command.env(GRANDCHILD_ENV, published);
        }
    }
    let child = command.spawn().expect("spawn the test executable");
    ChildProcess::new(child).expect("capture freshly spawned child identity")
}

/// The cell [`ChildProcess::stop`] takes its group id out of, for a test
/// that stops a child without a session task around it.
#[cfg(unix)]
fn published_group(child: &ChildProcess) -> PublishedGroup {
    PublishedGroup(Mutex::new(GroupPublication::Live(child.process_group)))
}

// A scheduler may pause shutdown after its final wait reaps the leader.
// Only the explicit isolated namespace regression installs this one-shot
// hook; production and ordinary tests never do.
#[cfg(target_os = "linux")]
thread_local! {
    pub static LSP_AFTER_REAP: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "requires permitted user/PID namespaces; explicit kernel PID-reuse regression"]
fn lsp_stop_never_signals_a_kernel_reused_group_after_reaping() {
    use std::cell::RefCell;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::rc::Rc;

    const MARKER: &str = "CAGENTS_AUDIT_LSP_REUSE_CHILD";
    const PARENT_NAMESPACE: &str = "CAGENTS_AUDIT_LSP_PARENT_PID_NAMESPACE";
    const TEST: &str = "lsp::tests::lsp_stop_never_signals_a_kernel_reused_group_after_reaping";
    if std::env::var_os(MARKER).is_none() {
        for scenario in ["post-reap", "already-reaped"] {
            let output = std::process::Command::new("/usr/bin/unshare")
                .args([
                    "--user",
                    "--map-root-user",
                    "--pid",
                    "--fork",
                    "--mount-proc",
                ])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    TEST,
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(MARKER, scenario)
                .env(
                    PARENT_NAMESPACE,
                    std::fs::read_link("/proc/self/ns/pid").unwrap(),
                )
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stdout.contains("owned LSP PID-reuse scheduler fixture started"),
                "the exact namespace test must run: {stdout}; {stderr}"
            );
            assert!(
                output.status.success(),
                "owned LSP scheduler-window audit failed: {stdout}; {stderr}"
            );
        }
        return;
    }
    let already_reaped = std::env::var(MARKER).unwrap() == "already-reaped";
    println!("owned LSP PID-reuse scheduler fixture started");
    assert_eq!(
        std::process::id(),
        1,
        "only owned namespace init may reset PIDs"
    );

    assert_ne!(
        std::fs::read_link("/proc/self/ns/pid").unwrap().as_os_str(),
        std::env::var_os(PARENT_NAMESPACE).expect("outer namespace identity"),
        "PID allocation must be changed only inside a new owned namespace"
    );

    struct Replacement {
        child: std::process::Child,
        pidfd: OwnedFd,
    }
    impl Drop for Replacement {
        fn drop(&mut self) {
            // SAFETY: the descriptor identifies only our replacement child,
            // including after the numeric PID has become available again.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
            let _ = self.child.wait();
        }
    }
    block_on(async {
        // Runtime initialization precedes PID allocation. It cannot create
        // another worker thread while the synchronous hook runs.
        std::fs::write("/proc/sys/kernel/ns_last_pid", b"199").unwrap();
        let child = tokio::process::Command::new("/bin/true")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = child.id().unwrap();
        assert_eq!(group, 200);
        let mut child = ChildProcess::new(child).unwrap();
        let published = published_group(&child);
        let replacement = Rc::new(RefCell::new(None::<Replacement>));
        let captured = Rc::clone(&replacement);
        let replace = move || {
            assert!(
                !Path::new(&format!("/proc/{group}")).exists(),
                "the actual grace wait must have reaped the original leader"
            );
            std::fs::write("/proc/sys/kernel/ns_last_pid", (group - 1).to_string()).unwrap();
            let child = std::process::Command::new("/bin/sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .unwrap();
            assert_eq!(child.id(), group, "kernel must really reuse the PID");
            assert_eq!(unsafe { libc::getpgid(group as i32) }, group as i32);
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, group, 0) } as i32;
            assert!(raw >= 0);
            *captured.borrow_mut() = Some(Replacement {
                child,
                // SAFETY: pidfd_open returned a new owned descriptor.
                pidfd: unsafe { OwnedFd::from_raw_fd(raw) },
            });
        };
        if already_reaped {
            child.child.wait().await.unwrap();
            replace();
        } else {
            LSP_AFTER_REAP.with(|hook| *hook.borrow_mut() = Some(Box::new(replace)));
        }
        assert_eq!(
            child.stop(&published).await,
            if already_reaped {
                ChildStop::Unconfirmed
            } else {
                ChildStop::Exited
            }
        );
        assert_eq!(published.current(), 0);
        let mut replacement = replacement.borrow_mut();
        let replacement = replacement.as_mut().expect("after-reap hook ran");
        let mut poll = libc::pollfd {
            fd: replacement.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let exited = unsafe { libc::poll(&mut poll, 1, 250) };
        let status = replacement.child.try_wait().unwrap();
        println!(
            "actual stop scenario already_reaped={already_reaped}; reused PID/PGID={group}; replacement readiness={exited}; status={status:?}"
        );
        assert_eq!(
            exited, 0,
            "actual stop killed the unrelated replacement group"
        );
        assert!(status.is_none());
    });
}

#[test]
fn retired_lsp_authority_refuses_late_publication_and_restart_uses_a_new_slot() {
    let retired = PublishedGroup::default();
    retired.kill();
    assert!(!retired.publish(123));
    let replacement = PublishedGroup::default();
    assert!(replacement.publish(456));
    assert!(!replacement.publish(789));
    assert_eq!(replacement.claim(), Some(456));
    assert!(!replacement.publish(789));
}

#[test]
#[cfg(target_os = "linux")]
fn dropping_an_lsp_child_disarms_its_published_group_before_tokio_reaps() {
    block_on(async {
        let mut child = spawn_test_child(
            &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
            Helper::Sleeps,
            true,
        );
        let group = child.process_group;
        let published = Arc::new(PublishedGroup::default());
        assert!(child.publish(Arc::clone(&published)));
        drop(child);
        assert_eq!(published.current(), 0);
        assert!(!published.publish(group));
        assert!(wait_until_gone(group).await);
    });
}

#[test]
#[cfg(target_os = "linux")]
fn losing_lsp_shutdown_claim_waits_for_the_fallback_signal_before_reaping() {
    block_on(async {
        let mut child = spawn_test_child(&["--list"], Helper::None, true);
        assert!(child.settled(WAIT).await.unwrap());
        let group = child.process_group;
        let published = Arc::new(PublishedGroup::default());
        assert!(child.publish(Arc::clone(&published)));
        let completed = Arc::new(AtomicBool::new(false));
        let check = Arc::clone(&completed);
        LSP_AFTER_REAP.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                assert!(
                    check.load(Ordering::Acquire),
                    "fallback signal must precede final reap"
                );
            }));
        });
        let fallback = Arc::clone(&published);
        let (claimed_tx, claimed_rx) = std::sync::mpsc::channel();
        let (starting_tx, starting_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            // Model preemption between the fallback's claim and syscall,
            // retaining the exact production authority mutex throughout.
            let mut state = fallback.0.lock().unwrap();
            assert!(
                matches!(std::mem::replace(&mut *state, GroupPublication::Closed), GroupPublication::Live(id) if id == group)
            );
            claimed_tx.send(()).unwrap();
            starting_rx.recv_timeout(WAIT).unwrap();
            signal_process_group(group, SIGKILL);
            completed.store(true, Ordering::Release);
        });
        claimed_rx.recv_timeout(WAIT).unwrap();
        assert!(
            published.0.try_lock().is_err(),
            "pending claimed signal owns the authority lock"
        );
        starting_tx.send(()).unwrap();
        assert_eq!(child.stop(&published).await, ChildStop::Exited);
        thread.join().unwrap();
        assert!(wait_until_gone(group).await);
    });
}

#[test]
#[cfg(target_os = "linux")]
fn observing_lsp_exit_preserves_the_original_leader_until_group_cleanup() {
    block_on(async {
        let child = spawn_test_child(&["--list"], Helper::None, true);
        let group = child.process_group;
        assert!(child.settled(WAIT).await.unwrap());
        assert!(child.observed_exit().unwrap());
        assert_eq!(child.child.id(), Some(group));
        let stat = std::fs::read_to_string(format!("/proc/{group}/stat")).unwrap();
        assert_eq!(
            stat.rsplit_once(") ").unwrap().1.split_whitespace().next(),
            Some("Z")
        );
        let published = published_group(&child);
        assert_eq!(child.stop(&published).await, ChildStop::Exited);
        assert!(
            wait_until_gone(group).await,
            "final shutdown must reap the leader"
        );
    });
}

#[test]
#[cfg(target_os = "linux")]
fn reaped_lsp_identity_refuses_group_shutdown() {
    block_on(async {
        let mut child = spawn_test_child(&["--list"], Helper::None, true);
        let published = published_group(&child);
        child.child.wait().await.unwrap();
        assert_eq!(
            child.observed_exit().unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert_eq!(child.stop(&published).await, ChildStop::Unconfirmed);
        assert_eq!(published.current(), 0);
    });
}

#[test]
#[cfg(target_os = "linux")]
fn cancelling_lsp_shutdown_preserves_tokio_drop_cleanup() {
    block_on(async {
        let child = spawn_test_child(
            &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
            Helper::Sleeps,
            true,
        );
        let group = child.process_group;
        let published = published_group(&child);
        assert!(
            tokio::time::timeout(TERMINATION_GRACE / 2, child.stop(&published))
                .await
                .is_err()
        );
        assert_eq!(published.current(), 0);
        assert!(
            wait_until_gone(group).await,
            "Tokio must still kill and reap a cancelled shutdown child"
        );
    });
}

#[test]
#[cfg(unix)]
fn a_child_that_exits_on_its_own_is_reaped_without_being_terminated() {
    block_on(async {
        // `--list` makes libtest print its test names and exit at once.
        let child = spawn_test_child(&["--list"], Helper::None, true);
        let published = published_group(&child);
        assert_eq!(child.stop(&published).await, ChildStop::Exited);
    });
}

#[test]
#[cfg(unix)]
fn a_live_child_is_group_terminated_and_reaped() {
    block_on(async {
        let child = spawn_test_child(
            &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
            Helper::Sleeps,
            true,
        );
        let published = published_group(&child);
        assert_eq!(
            child.stop(&published).await,
            ChildStop::Terminated,
            "a server that ignores `exit` is SIGTERMed by process group and reaped"
        );
    });
}

#[test]
#[cfg(target_os = "linux")]
fn the_published_group_is_withdrawn_before_its_leader_can_be_reaped() {
    block_on(async {
        let child = spawn_test_child(
            &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
            Helper::Sleeps,
            true,
        );
        let group = child.process_group;
        let published = PublishedGroup(Mutex::new(GroupPublication::Live(group)));
        let mut stop = std::pin::pin!(child.stop(&published));
        // This child ignores everything, so `stop` is still inside its
        // first grace period here — and the id must already be gone from
        // the cell, because the moment it is reaped is the moment the
        // kernel may hand the id to something else. A reader that finds
        // the id after that has no way to tell.
        let observed = tokio::select! {
            _ = &mut stop => panic!("a sleeping child cannot be stopped within the grace period"),
            _ = tokio::time::sleep(TERMINATION_GRACE / 2) => published.current(),
        };
        assert!(
            PathBuf::from(format!("/proc/{group}")).exists(),
            "the leader is still running, so its id is still its own"
        );
        assert_eq!(
            observed, 0,
            "the id must be withdrawn before the reap, not after it"
        );
        stop.await;
    });
}

/// Waits for a helper child to publish its grandchild's pid. Its absence
/// means the helper body never ran, which a `libtest` filter that selects
/// nothing would otherwise make indistinguishable from success.
#[cfg(target_os = "linux")]
async fn published_pid(path: &Path) -> u32 {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            return text.trim().parse().expect("a published pid");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no grandchild pid was published, so the helper never ran"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A leader that exits promptly is the common case — `rust-analyzer` quits
/// on `exit` — and its group is exactly what must still be ended: nothing
/// else can reach the children it left behind.
#[test]
#[cfg(target_os = "linux")]
fn a_prompt_exit_still_ends_the_server_process_group() {
    block_on(async {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("grandchild-pid");
        let child = spawn_test_child(
            &["--exact", GRANDCHILD_TEST, "--test-threads=1"],
            Helper::LeavesAGrandchild(&path),
            true,
        );
        let group = child.process_group;
        let grandchild = published_pid(&path).await;
        assert_ne!(
            grandchild, group,
            "the grandchild is a process of its own, in the leader's group"
        );

        // Nothing has reaped the leader, so it is now a zombie in the
        // group it still leads, and the grace period below sees it exit
        // at once: the case this test is about, rather than a race with
        // libtest's own shutdown.
        assert!(
            wait_until_not_running(group).await,
            "the helper publishes its grandchild's pid and returns"
        );
        let published = published_group(&child);
        assert_eq!(
            child.stop(&published).await,
            ChildStop::Exited,
            "the helper exits on its own, so no signal is needed to end it"
        );
        let gone = wait_until_not_running(grandchild).await;
        if !gone {
            // Only reached when the group was never ended, in which case
            // it still holds this process and its id is therefore still
            // reserved: this is the signal `stop` owed it.
            signal_process_group(group, SIGKILL);
        }
        assert!(
            gone,
            "a server's descendants must go with the process group it led"
        );
    });
}

/// Whether the process is gone entirely. A terminated but unreaped child
/// keeps its `/proc` entry as a zombie, so this also proves reaping.
#[cfg(target_os = "linux")]
async fn wait_until_gone(pid: u32) -> bool {
    let entry = PathBuf::from(format!("/proc/{pid}"));
    let deadline = tokio::time::Instant::now() + WAIT;
    while tokio::time::Instant::now() < deadline {
        if !entry.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

#[test]
#[cfg(target_os = "linux")]
fn a_graceful_shutdown_terminates_and_reaps_the_server_process() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        // No `kill_on_drop`: this child outlives every handle unless the
        // session task terminates its process group on purpose, so a
        // forgotten teardown path fails this test instead of hiding.
        let child = spawn_test_child(
            &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
            Helper::Sleeps,
            false,
        );
        let pid = child.process_group;
        let mut manager = rust_manager();
        let slot = install_fake_with_child(
            &mut manager,
            incremental_capabilities(),
            |_| Vec::new(),
            Arc::new(Mutex::new(Some(child))),
        );
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
        let handle = handle_of(&slot).await;
        await_ready(&events, doc).await;
        assert!(
            PathBuf::from(format!("/proc/{pid}")).exists(),
            "the child must still be running before shutdown"
        );

        manager.shutdown();
        handle.wait_for("exit").await;
        assert!(
            wait_until_gone(pid).await,
            "the server process must be group-terminated and reaped by the session"
        );
    });
}

/// Installs a fake server that keeps one handle per discovered root, so a
/// test can address each session separately.
fn install_fake_registry(
    manager: &mut LspManager,
    capabilities: Value,
    handler: impl Fn(&Incoming) -> Vec<Value> + Send + Sync + 'static,
) -> Arc<Mutex<Vec<(PathBuf, FakeHandle)>>> {
    let registry: Arc<Mutex<Vec<(PathBuf, FakeHandle)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&registry);
    let handler = Arc::new(handler);
    manager.set_transport(Arc::new(move |spec: &LaunchSpec| {
        let capabilities = capabilities.clone();
        let handler = Arc::clone(&handler);
        let (transport, handle) = fake_transport(move |incoming| match incoming.method.as_str() {
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
        sink.lock()
            .expect("registry")
            .push((spec.root.clone(), handle));
        Ok(transport)
    }));
    registry
}

async fn handle_for(registry: &Arc<Mutex<Vec<(PathBuf, FakeHandle)>>>, root: &Path) -> FakeHandle {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some((_, handle)) = registry
            .lock()
            .expect("registry")
            .iter()
            .find(|(at, _)| at == root)
        {
            return handle.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no transport was created for {}",
            root.display()
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[test]
fn two_files_in_one_root_share_one_session_and_debounce_independently() {
    block_on(async {
        let root = canonical_tempdir();
        let worktree = root.path().to_path_buf();
        std::fs::write(worktree.join("Cargo.toml"), "").expect("marker");
        let first = worktree.join("first.rs");
        let second = worktree.join("second.rs");
        std::fs::write(&first, "a\n").expect("write");
        std::fs::write(&second, "b\n").expect("write");

        let mut manager = rust_manager();
        let registry =
            install_fake_registry(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let one = LspDocumentId(1);
        let two = LspDocumentId(2);
        manager.open_for_test(one, &first, &worktree, rust_language(), 1, "a\n");
        manager.open_for_test(two, &second, &worktree, rust_language(), 1, "b\n");
        let handle = handle_for(&registry, &worktree).await;
        await_ready_documents(&events, &[one, two]).await;

        assert_eq!(
            registry.lock().expect("registry").len(),
            1,
            "two files under one root must share one process"
        );
        assert_eq!(manager.sessions.len(), 1);
        handle.wait_for_count("textDocument/didOpen", 2).await;

        // One edit to `first`, then a stream of edits to `second`. A
        // session-wide debounce timer would let `second` postpone
        // `first`'s notification indefinitely.
        manager.change(
            one,
            2,
            &[LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 1), LspPosition::new(0, 1)),
                new_text: "x".to_owned(),
            }],
            "ax\n",
        );
        let flood = tokio::time::Instant::now() + CHANGE_DEBOUNCE * 6;
        let mut text = String::from("b\n");
        let mut version = 2;
        while tokio::time::Instant::now() < flood {
            text.insert(1, 'y');
            version += 1;
            manager.change(two, version, &[], &text);
            tokio::time::sleep(CHANGE_DEBOUNCE / 3).await;
        }

        let first_uri = Url::from_file_path(&first).expect("uri").to_string();
        let changes = handle.entries("textDocument/didChange");
        assert!(
            changes
                .iter()
                .any(|entry| entry.params["textDocument"]["uri"] == json!(first_uri)),
            "the quiet file's change must be flushed on its own deadline, saw {:?}",
            changes
                .iter()
                .map(|entry| entry.params["textDocument"]["uri"].clone())
                .collect::<Vec<Value>>()
        );
        manager.shutdown();
    });
}

#[test]
fn diagnostics_reach_only_the_documents_of_the_emitting_session() {
    block_on(async {
        let first_root = canonical_tempdir();
        let second_root = canonical_tempdir();
        let first = first_root.path().join("one.rs");
        let second = second_root.path().join("two.rs");
        std::fs::write(&first, "a\n").expect("write");
        std::fs::write(&second, "b\n").expect("write");

        let mut manager = rust_manager();
        let registry =
            install_fake_registry(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let one = LspDocumentId(1);
        let two = LspDocumentId(2);
        manager.open_for_test(one, &first, first_root.path(), rust_language(), 1, "a\n");
        manager.open_for_test(two, &second, second_root.path(), rust_language(), 1, "b\n");
        let first_handle = handle_for(&registry, first_root.path()).await;
        let second_handle = handle_for(&registry, second_root.path()).await;
        await_ready_documents(&events, &[one, two]).await;
        assert_eq!(manager.sessions.len(), 2);

        let first_uri = Url::from_file_path(&first).expect("uri").to_string();
        let second_uri = Url::from_file_path(&second).expect("uri").to_string();
        for version in [Some(1), None] {
            first_handle.notify(
                "textDocument/publishDiagnostics",
                json!({
                    "uri": second_uri,
                    "version": version,
                    "diagnostics": [{
                        "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                        "message": "another live server does not own this file"
                    }]
                }),
            );
            // This follows the foreign publication on the SAME server's
            // ordered wire. Its real event is the barrier: a leaked batch
            // must fail before it, without a sleep or an absent-event guess.
            first_handle.notify(
                "textDocument/publishDiagnostics",
                json!({
                    "uri": first_uri,
                    "version": version,
                    "diagnostics": [{
                        "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                        "message": "first server's own diagnostic"
                    }]
                }),
            );
            let (doc, message) = next_matching(&events, |event| match event {
                LspEvent::Diagnostics { doc, items, .. } => {
                    Some((*doc, items.first().map(|item| item.message.to_owned())))
                }
                _ => None,
            })
            .await;
            assert_eq!(doc, one, "a live server cannot publish into another root");
            assert_eq!(message.as_deref(), Some("first server's own diagnostic"));

            second_handle.notify(
                "textDocument/publishDiagnostics",
                json!({
                    "uri": second_uri,
                    "version": version,
                    "diagnostics": [{
                        "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                        "message": "second server's own diagnostic"
                    }]
                }),
            );
            let (doc, message) = next_matching(&events, |event| match event {
                LspEvent::Diagnostics { doc, items, .. } => {
                    Some((*doc, items.first().map(|item| item.message.to_owned())))
                }
                _ => None,
            })
            .await;
            assert_eq!(doc, two);
            assert_eq!(message.as_deref(), Some("second server's own diagnostic"));
        }
        manager.shutdown();
    });
}

#[test]
fn progress_reaches_only_the_documents_of_the_emitting_session() {
    block_on(async {
        let first_root = canonical_tempdir();
        let second_root = canonical_tempdir();
        let first = first_root.path().join("one.rs");
        let second = second_root.path().join("two.rs");
        std::fs::write(&first, "a\n").expect("write");
        std::fs::write(&second, "b\n").expect("write");

        let mut manager = rust_manager();
        let registry =
            install_fake_registry(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let one = LspDocumentId(1);
        let two = LspDocumentId(2);
        manager.open_for_test(one, &first, first_root.path(), rust_language(), 1, "a\n");
        manager.open_for_test(two, &second, second_root.path(), rust_language(), 1, "b\n");
        let first_handle = handle_for(&registry, first_root.path()).await;
        let _second_handle = handle_for(&registry, second_root.path()).await;
        await_ready_documents(&events, &[one, two]).await;
        assert_eq!(
            manager.sessions.len(),
            2,
            "distinct roots must not share a session"
        );

        first_handle.notify(
            "$/progress",
            json!({
                "token": "indexing",
                "value": { "kind": "begin", "title": "Indexing", "percentage": 10 },
            }),
        );
        let doc = next_matching(&events, |event| match event {
            LspEvent::Progress { doc, progress, .. } if progress.token == "indexing" => Some(*doc),
            _ => None,
        })
        .await;
        assert_eq!(doc, one, "progress belongs to its own session's documents");
        // Nothing else may arrive for the unrelated root.
        tokio::time::sleep(Duration::from_millis(50)).await;
        while let Ok(event) = events.try_recv() {
            if let LspEvent::Progress { doc, .. } = event {
                assert_ne!(doc, two, "an unrelated server's progress leaked");
            }
        }
        manager.shutdown();
    });
}

#[test]
fn a_second_binding_on_one_uri_resynchronises_and_survives_the_first_close() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let one = LspDocumentId(1);
        let two = LspDocumentId(2);
        manager.open_for_test(one, &file, &worktree, rust_language(), 1, "first\n");
        let handle = handle_of(&slot).await;
        await_ready(&events, one).await;
        handle.wait_for("textDocument/didOpen").await;

        // A second buffer on the same file with different text: the server
        // holds the first text, so it must be resynchronised.
        manager.open_for_test(two, &file, &worktree, rust_language(), 1, "second\n");
        let resync = handle.wait_for("textDocument/didChange").await;
        assert_eq!(
            resync.params["contentChanges"][0]["text"],
            json!("second\n"),
            "a repeated open must resend the authoritative text"
        );
        assert_eq!(
            handle.entries("textDocument/didOpen").len(),
            1,
            "one URI produces exactly one didOpen"
        );

        // A pending edit from the surviving binding must not be discarded
        // by the other binding closing.
        manager.change(
            one,
            5,
            &[LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 6), LspPosition::new(0, 6)),
                new_text: "!".to_owned(),
            }],
            "second!\n",
        );
        manager.close(two);
        let changes = handle.wait_for_count("textDocument/didChange", 2).await;
        assert_eq!(
            changes.last().expect("second change").params["textDocument"]["version"],
            json!(5)
        );
        assert!(
            handle.entries("textDocument/didClose").is_empty(),
            "the file is still open in another tab"
        );

        manager.close(one);
        handle.wait_for("textDocument/didClose").await;
        manager.shutdown();
    });
}

#[test]
fn a_diagnostic_batch_for_an_older_version_is_ignored() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
        let handle = handle_of(&slot).await;
        await_ready(&events, doc).await;
        manager.change(doc, 2, &[], "abcd\n");
        let uri = Url::from_file_path(&file).expect("uri").to_string();

        let stale_range = json!({
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 1 },
        });
        handle.notify(
            "textDocument/publishDiagnostics",
            json!({
                "uri": uri,
                "version": 1,
                "diagnostics": [{ "range": stale_range, "message": "computed for version 1" }],
            }),
        );
        handle.notify(
            "textDocument/publishDiagnostics",
            json!({
                "uri": uri,
                "version": 2,
                "diagnostics": [{ "range": stale_range, "message": "computed for version 2" }],
            }),
        );
        let items = next_matching(&events, |event| match event {
            LspEvent::Diagnostics { doc: at, items, .. } if *at == doc => Some(items.clone()),
            _ => None,
        })
        .await;
        assert_eq!(
            items.first().map(|item| item.message.as_str()),
            Some("computed for version 2"),
            "a batch computed for a superseded version must not be published"
        );
        manager.shutdown();
    });
}

#[test]
fn a_dead_session_is_replaced_and_every_open_document_is_reopened() {
    block_on(async {
        let root = canonical_tempdir();
        let worktree = root.path().to_path_buf();
        std::fs::write(worktree.join("Cargo.toml"), "").expect("marker");
        let first = worktree.join("first.rs");
        let second = worktree.join("second.rs");
        std::fs::write(&first, "a\n").expect("write");
        std::fs::write(&second, "b\n").expect("write");

        let mut manager = rust_manager();
        // The first process dies right after `initialized`; the second
        // behaves normally.
        let launches = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&launches);
        let registry =
            install_fake_registry(&mut manager, incremental_capabilities(), move |incoming| {
                if incoming.method == "initialized" && counter.fetch_add(1, Ordering::AcqRel) == 0 {
                    return vec![json!({ "jsonrpc": "2.0", "method": 42 })];
                }
                Vec::new()
            });
        let events = manager.events();
        let one = LspDocumentId(1);
        manager.open_for_test(one, &first, &worktree, rust_language(), 1, "a\n");
        let _ = handle_for(&registry, &worktree).await;
        let status = next_matching(&events, |event| match event {
            LspEvent::Status { doc, status, .. } if *doc == one => match status {
                LspStatus::Failed { .. } => Some(status.clone()),
                _ => None,
            },
            _ => None,
        })
        .await;
        assert!(status.can_restart());

        // Opening an unrelated file in the same root replaces the dead
        // session; the already-open document must be reopened on it, or
        // the new server would know nothing about it.
        let two = LspDocumentId(2);
        manager.open_for_test(two, &second, &worktree, rust_language(), 1, "b\n");
        let replacement = {
            let deadline = tokio::time::Instant::now() + WAIT;
            loop {
                let entries = registry.lock().expect("registry").len();
                if entries >= 2 {
                    break registry.lock().expect("registry")[1].1.clone();
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the dead session was never replaced"
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        };
        let opens = replacement.wait_for_count("textDocument/didOpen", 2).await;
        let uris: Vec<String> = opens
            .iter()
            .map(|entry| {
                entry.params["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        for path in [&first, &second] {
            let expected = Url::from_file_path(path).expect("uri").to_string();
            assert!(
                uris.contains(&expected),
                "{expected} must be opened on the replacement, saw {uris:?}"
            );
        }
        manager.shutdown();
    });
}

#[test]
fn wait_for_shutdown_reaps_every_session_process() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let child = spawn_test_child(
            &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
            Helper::Sleeps,
            false,
        );
        let pid = child.process_group;
        let mut manager = rust_manager();
        let slot = install_fake_with_child(
            &mut manager,
            incremental_capabilities(),
            |_| Vec::new(),
            Arc::new(Mutex::new(Some(child))),
        );
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
        let _handle = handle_of(&slot).await;
        await_ready(&events, doc).await;

        manager.shutdown();
        manager.wait_for_shutdown().await;
        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "wait_for_shutdown must return only after the process is reaped"
        );
    });
}

/// Setup shared by the tests that stop a live server: a session serving
/// one document, with a real process behind it that nothing but the
/// session's own teardown can end.
#[cfg(target_os = "linux")]
async fn session_over_a_live_process(
    manager: &mut LspManager,
    worktree: &Path,
    file: &Path,
    doc: LspDocumentId,
) -> u32 {
    let child = spawn_test_child(
        &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
        Helper::Sleeps,
        false,
    );
    let pid = child.process_group;
    let slot = install_fake_with_child(
        manager,
        incremental_capabilities(),
        |_| Vec::new(),
        Arc::new(Mutex::new(Some(child))),
    );
    let events = manager.events();
    manager.open_for_test(doc, file, worktree, rust_language(), 1, "abc\n");
    let _handle = handle_of(&slot).await;
    await_ready(&events, doc).await;
    assert!(
        PathBuf::from(format!("/proc/{pid}")).exists(),
        "the server must be running before anything stops it"
    );
    pid
}

#[test]
#[cfg(target_os = "linux")]
fn wait_for_shutdown_leaves_a_session_nobody_retired_alone() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let doc = LspDocumentId(1);
        let pid = session_over_a_live_process(&mut manager, &worktree, &file, doc).await;

        // Nothing has been retired, so there is nothing to wait for. A
        // wait that drained the installed session instead would spend
        // `SESSION_REAP_TIMEOUT` on a server it never sent `shutdown` to,
        // and then kill the process group of a healthy one.
        let started = std::time::Instant::now();
        manager.wait_for_shutdown().await;
        assert!(
            started.elapsed() < SESSION_REAP_TIMEOUT,
            "waiting for nothing took {:?}",
            started.elapsed()
        );
        assert!(
            PathBuf::from(format!("/proc/{pid}")).exists(),
            "a server that was never told to stop must still be running"
        );
        assert_eq!(manager.status(doc), LspStatus::Ready);

        // And the session is still the manager's to stop, rather than one
        // whose task slot was emptied and can never be awaited again.
        manager.shutdown();
        manager.wait_for_shutdown().await;
        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "the session must still be waitable after the wait that skipped it"
        );
    });
}

#[test]
#[cfg(target_os = "linux")]
fn begin_shutdown_reaps_the_server_it_stops() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let doc = LspDocumentId(1);
        let pid = session_over_a_live_process(&mut manager, &worktree, &file, doc).await;

        manager.begin_shutdown().await;
        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "the future begin_shutdown returns must not finish before the reap"
        );
    });
}

#[test]
#[cfg(target_os = "linux")]
fn begin_shutdown_reaps_the_server_with_no_runtime_in_scope() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (_guard, worktree, file) = workspace();
    let mut manager = rust_manager();
    let doc = LspDocumentId(1);
    let pid = runtime.block_on(session_over_a_live_process(
        &mut manager,
        &worktree,
        &file,
        doc,
    ));

    // The application's exit path calls this from its update thread, which
    // has no runtime entered. The future handed back is then the only
    // thing that can reap, so it must be the thing that does.
    let wait = manager.begin_shutdown();
    runtime.block_on(wait);
    assert!(
        !PathBuf::from(format!("/proc/{pid}")).exists(),
        "the wait must reap even when it could not be spawned"
    );
}

/// Whether the process has stopped executing, whether or not anything has
/// collected it yet. A test that deliberately keeps Tokio from reaping
/// its child can only observe the zombie it leaves behind.
#[cfg(target_os = "linux")]
async fn wait_until_not_running(pid: u32) -> bool {
    let deadline = tokio::time::Instant::now() + WAIT;
    while tokio::time::Instant::now() < deadline {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return true;
        };
        // "pid (comm) state ...", and `comm` may itself contain
        // parentheses, so the state is what follows the last one.
        if stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

#[test]
#[cfg(target_os = "linux")]
fn a_session_that_cannot_finish_is_still_reaped_within_the_wait() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        // No `kill_on_drop`: nothing except this session's own teardown
        // can end this process, so a wait that gives up without killing
        // the group leaves the server running and fails here.
        let child = spawn_test_child(
            &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
            Helper::Sleeps,
            false,
        );
        let pid = child.process_group;
        let mut manager = rust_manager();
        let slot = install_fake_with_child(
            &mut manager,
            incremental_capabilities(),
            |_| Vec::new(),
            Arc::new(Mutex::new(Some(child))),
        );
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
        let _handle = handle_of(&slot).await;
        await_ready(&events, doc).await;

        // An application on its way out stops draining events, and a
        // session task that is publishing one then never reaches the
        // shutdown it was sent. Superseding a request makes it publish
        // the cancellation, against a channel nobody is emptying.
        while manager
            .events
            .tx
            .try_send(LspEvent::RequestFailed {
                request: LspRequestId(0),
                doc,
                error: LspError::Cancelled,
            })
            .is_ok()
        {}
        manager.request_hover(doc, LspPosition::new(0, 0));
        manager.request_hover(doc, LspPosition::new(0, 1));

        manager.shutdown();
        assert!(
            tokio::time::timeout(WAIT, manager.wait_for_shutdown())
                .await
                .is_ok(),
            "an exit path cannot wait on a session task that will never finish"
        );
        assert!(
            wait_until_not_running(pid).await,
            "giving up on the session must still kill the server's process group"
        );
    });
}

/// A worktree, a sibling directory outside it that also carries a root
/// marker, and a symlink inside the worktree pointing at that sibling.
fn escaping_symlink() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = canonical_tempdir();
    let worktree = root.path().join("worktree");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(worktree.join("src")).expect("worktree");
    std::fs::create_dir_all(outside.join("src")).expect("outside");
    std::fs::write(worktree.join("Cargo.toml"), "").expect("worktree marker");
    std::fs::write(outside.join("Cargo.toml"), "").expect("outside marker");
    std::fs::write(outside.join("src/lib.rs"), "fn f() {}\n").expect("outside file");
    std::os::unix::fs::symlink(&outside, worktree.join("link")).expect("symlink");
    (root, worktree, outside)
}

#[test]
fn a_symlinked_directory_that_leaves_the_worktree_is_never_a_server_root() {
    let (_root, worktree, outside) = escaping_symlink();
    let markers = vec!["Cargo.toml".to_owned()];
    // Lexically `<worktree>/link/src` is inside the worktree and its parent
    // holds a marker, but it resolves to a directory the user never opened.
    assert_eq!(
        discover_root(&worktree.join("link/src/lib.rs"), &worktree, &markers),
        worktree,
        "a root reached through a symlink out of the worktree must be refused"
    );
    assert_ne!(
        discover_root(&worktree.join("link/src/lib.rs"), &worktree, &markers),
        outside,
        "the escaping directory must never be selected"
    );
    // The genuine nested case still finds the nearest marker.
    std::fs::create_dir_all(worktree.join("inner/src")).expect("inner");
    std::fs::write(worktree.join("inner/Cargo.toml"), "").expect("inner marker");
    assert_eq!(
        discover_root(&worktree.join("inner/src/lib.rs"), &worktree, &markers),
        worktree.join("inner"),
        "a real nested marker still wins"
    );
}

fn location_request(worktree: &Path, uri: &Arc<Url>) -> PendingRequest {
    PendingRequest {
        id: LspRequestId(1),
        doc: LspDocumentId(1),
        uri: Arc::clone(uri),
        worktree: Arc::from(worktree.to_path_buf()),
        kind: RequestKind::Definition,
        position: lsp_types::Position::new(0, 0),
        version: 1,
    }
}

fn classify(worktree: &Path, target_path: &Path) -> LspTarget {
    let snapshot = Snapshot::new(1, Arc::from("fn main() {}\n"));
    let request_uri = Arc::new(Url::from_file_path(worktree.join("main.rs")).expect("uri"));
    let request = location_request(worktree, &request_uri);
    let location = lsp_types::Location {
        uri: Url::from_file_path(target_path).expect("target uri"),
        range: lsp_types::Range::new(
            lsp_types::Position::new(0, 0),
            lsp_types::Position::new(0, 1),
        ),
    };
    let converted = convert_locations(&snapshot, &request, &[location]);
    assert_eq!(converted.len(), 1, "a file location is always reported");
    converted.into_iter().next().expect("one location").target
}

#[test]
fn a_symlinked_path_resolving_outside_the_worktree_classifies_as_external() {
    let (_root, worktree, _outside) = escaping_symlink();
    assert!(
        matches!(
            classify(&worktree, &worktree.join("link/src/lib.rs")),
            LspTarget::External { .. }
        ),
        "a path that resolves outside the worktree must open read-only"
    );
}

#[test]
fn a_nested_worktree_file_is_workspace_and_an_unresolvable_one_is_external() {
    let (_root, worktree, _outside) = escaping_symlink();
    let nested = worktree.join("src/deep.rs");
    std::fs::write(&nested, "fn deep() {}\n").expect("nested file");
    assert!(
        matches!(classify(&worktree, &nested), LspTarget::Workspace { .. }),
        "an ordinary nested file stays editable"
    );
    // Canonicalisation cannot resolve a path that does not exist, so
    // containment is unproven and the conservative answer is read-only.
    assert!(
        matches!(
            classify(&worktree, &worktree.join("src/vanished.rs")),
            LspTarget::External { .. }
        ),
        "an unresolvable path must never be reported as writable workspace"
    );
}

#[test]
fn an_escaped_comma_stays_inside_a_snippet_choice() {
    let snippet = parse_snippet(r"m(${1|a\,b,c|})");
    assert_eq!(
        snippet.text, "m(a,b)",
        "the first choice ends at the first unescaped comma"
    );
    assert_eq!(snippet.stops.len(), 1);
    assert_eq!(
        snippet.stops[0].range,
        2..5,
        "the tab stop spans the inserted choice"
    );
}

#[test]
fn an_escaped_dollar_is_literal_text_and_not_a_tab_stop() {
    let snippet = parse_snippet(r#"println!("\${}", $1)"#);
    assert_eq!(
        snippet.text, "println!(\"${}\", )",
        "an escaped dollar contributes text, not grammar"
    );
    assert_eq!(snippet.stops.len(), 1, "only the real `$1` is a tab stop");
    // A choice list whose bar is escaped is not a choice list at all.
    assert_eq!(parse_snippet(r"${1:a\|b}").text, r"a\|b");
}

#[test]
fn a_nested_placeholder_expands_and_keeps_both_tab_stops() {
    let snippet = parse_snippet("for ${1:item} in ${2:${3:iter}} {\n\t$0\n}");
    assert_eq!(snippet.text, "for item in iter {\n\t\n}");
    let indexes: Vec<u32> = snippet.stops.iter().map(|stop| stop.index).collect();
    assert_eq!(
        indexes,
        vec![1, 2, 3, 0],
        "visit order is ascending with `$0` last, and nesting is preserved"
    );
    let stop_two = &snippet.stops[1];
    let stop_three = &snippet.stops[2];
    assert_eq!(
        snippet.text[stop_two.range.clone()].to_owned(),
        "iter",
        "the outer placeholder spans its expanded child"
    );
    assert_eq!(stop_two.range, stop_three.range);
}

#[test]
fn a_completion_with_an_unconvertible_additional_edit_is_refused_whole() {
    let text: Arc<str> = Arc::from("fn main() {}\n");
    let snapshot = Snapshot::new(1, Arc::clone(&text));
    let good_range = lsp_types::Range::new(
        lsp_types::Position::new(0, 3),
        lsp_types::Position::new(0, 7),
    );
    let item = |label: &str, additional: Option<lsp_types::Range>| lsp_types::CompletionItem {
        label: label.to_owned(),
        text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
            range: good_range,
            new_text: label.to_owned(),
        })),
        additional_text_edits: additional.map(|range| {
            vec![lsp_types::TextEdit {
                range,
                new_text: "use std::io;\n".to_owned(),
            }]
        }),
        ..lsp_types::CompletionItem::default()
    };
    // The import edit points at a line the document does not have.
    let broken_range = lsp_types::Range::new(
        lsp_types::Position::new(99, 0),
        lsp_types::Position::new(99, 0),
    );
    let error = convert_completion(
        &snapshot,
        lsp_types::Position::new(0, 3),
        &item("broken", Some(broken_range)),
    )
    .expect_err("an unconvertible import edit must refuse the item");
    assert!(
        matches!(error, LspError::PositionOutOfRange { line: 99, .. }),
        "the reason is reported, not swallowed: {error:?}"
    );
    assert!(
        convert_completion(
            &snapshot,
            lsp_types::Position::new(0, 3),
            &item("fine", Some(good_range))
        )
        .expect("a convertible item survives")
        .additional_edits
        .len()
            == 1,
        "a convertible additional edit is still carried"
    );

    // End to end: the refused item is absent from the event while its
    // healthy sibling is offered.
    let store = SnapshotStore::default();
    let uri = Arc::new(Url::parse("file:///tmp/main.rs").expect("uri"));
    let key = SessionKey {
        server_id: Arc::from("rust-analyzer"),
        root: Arc::from(PathBuf::from("/tmp")),
    };
    store
        .register(LspDocumentId(1), &uri, &key, 1, text)
        .expect("register");
    let request = PendingRequest {
        id: LspRequestId(7),
        doc: LspDocumentId(1),
        uri: Arc::clone(&uri),
        worktree: Arc::from(PathBuf::from("/tmp")),
        kind: RequestKind::Completion,
        position: lsp_types::Position::new(0, 3),
        version: 1,
    };
    let event = convert_result(
        &store,
        &request,
        RawResult::Completion(Some(lsp_types::CompletionResponse::Array(vec![
            item("broken", Some(broken_range)),
            item("fine", None),
        ]))),
    );
    let LspEvent::Completion { items, .. } = event else {
        panic!("expected a completion event, got {event:?}");
    };
    let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["fine"],
        "the refused item is omitted and the rest are still offered"
    );
}

#[test]
fn reconfiguration_waits_for_prior_reap_even_across_an_empty_intermediate_manager() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let (release, waiting) = tokio::sync::oneshot::channel::<()>();
        let mut intermediate = rust_manager().after_shutdown(async move {
            let _ = waiting.await;
        });
        // A second settings change before root discovery has opened any
        // document must preserve the first manager's outstanding reap.
        let predecessor = intermediate.begin_shutdown();
        let mut manager = rust_manager()
            .with_lifecycle(1, 1)
            .after_shutdown(predecessor);
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "fn f() {}\n");
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            slot.lock().unwrap().is_none(),
            "replacement process started before old reap completed"
        );
        release.send(()).unwrap();
        let _handle = handle_of(&slot).await;
        await_ready(&events, doc).await;
        manager.begin_shutdown().await;
    });
}

#[test]
fn cancelling_one_launch_waiter_does_not_release_or_cancel_other_waiters() {
    block_on(async {
        let (release, waiting) = tokio::sync::oneshot::channel::<()>();
        let barrier = Arc::new(LaunchBarrier {
            predecessor: Mutex::new(Some(Box::pin(async move {
                let _ = waiting.await;
            }))),
            ready: AtomicBool::new(false),
            changed: Notify::new(),
        });
        let first = tokio::spawn(Arc::clone(&barrier).wait());
        tokio::task::yield_now().await;
        first.abort();
        let second = tokio::spawn(Arc::clone(&barrier).wait());
        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        release.send(()).unwrap();
        tokio::time::timeout(WAIT, second).await.unwrap().unwrap();
        assert!(barrier.ready.load(Ordering::Acquire));
    });
}

#[test]
fn externally_removed_root_retires_and_refreshes_last_server_metrics() {
    block_on(async {
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager().with_lifecycle(1, 1);
        let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "fn f() {}\n");
        let handle = handle_of(&slot).await;
        await_ready(&events, doc).await;
        assert_eq!(manager.live_servers().len(), 1);
        std::fs::remove_dir_all(&worktree).unwrap();
        next_matching(&events, |event| match event {
            LspEvent::Resources { doc: at } if *at == doc && manager.live_servers().is_empty() => {
                Some(())
            }
            _ => None,
        })
        .await;
        handle.wait_for("shutdown").await;
        handle.wait_for("exit").await;
        assert!(manager.live_servers().is_empty());
        assert!(handle.methods().iter().any(|method| method == "shutdown"));
        assert!(handle.methods().iter().any(|method| method == "exit"));
        manager.begin_shutdown().await;
    });
}

#[test]
fn replaced_root_reaps_original_server_before_reopening_the_same_path() {
    block_on(async {
        let parent = tempfile::tempdir().unwrap();
        let worktree = parent.path().join("workspace");
        std::fs::create_dir(&worktree).unwrap();
        std::fs::write(worktree.join("Cargo.toml"), "[package]\n").unwrap();
        let file = worktree.join("main.rs");
        std::fs::write(&file, "fn old() {}\n").unwrap();
        let child = spawn_test_child(
            &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
            Helper::Sleeps,
            true,
        );
        let pid = child.process_group;
        let mut manager = rust_manager().with_lifecycle(1, 1);
        let slot = install_fake_with_child(
            &mut manager,
            incremental_capabilities(),
            |_| Vec::new(),
            Arc::new(Mutex::new(Some(child))),
        );
        let events = manager.events();
        let doc = LspDocumentId(1);
        manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "fn old() {}\n");
        let old = handle_of(&slot).await;
        await_ready(&events, doc).await;

        // There is never a watchdog observation where this spelling is
        // missing. The old open document also prevents idle expiration.
        std::fs::rename(&worktree, parent.path().join("previous-workspace")).unwrap();
        std::fs::create_dir(&worktree).unwrap();
        std::fs::write(worktree.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(&file, "fn replacement() {}\n").unwrap();
        next_matching(&events, |event| match event {
            LspEvent::Resources { doc: at } if *at == doc && manager.live_servers().is_empty() => {
                Some(())
            }
            _ => None,
        })
        .await;
        assert!(
            wait_until_gone(pid).await,
            "the old server must be reaped before replacement admission"
        );
        assert!(manager.live_servers().is_empty());
        assert!(old.methods().iter().any(|method| method == "shutdown"));
        assert!(old.methods().iter().any(|method| method == "exit"));
        assert!(matches!(manager.status(doc), LspStatus::Disabled));

        manager.open_for_test(
            LspDocumentId(2),
            &file,
            &worktree,
            rust_language(),
            1,
            "fn replacement() {}\n",
        );
        await_ready(&events, LspDocumentId(2)).await;
        let replacement = handle_of(&slot).await;
        assert_eq!(replacement.entries("initialize").len(), 1);
        assert!(replacement.entries("shutdown").is_empty());
        manager.begin_shutdown().await;
    });
}

#[test]
fn root_pin_retains_original_directory_identity_after_path_replacement() {
    block_on(async {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let (pin, identity) = pin_session_root(&root).await.unwrap();
        std::fs::rename(&root, parent.path().join("original")).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(identity == RootIdentity::from_metadata(&pin.metadata().await.unwrap()));
        assert!(identity != RootIdentity::from_metadata(&std::fs::metadata(&root).unwrap()));
    });
}
