//! Actual HOST fresh preparation, private helper bootstrap, and provider wire ID.
use super::*;
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

#[test]
fn fresh_host_launch_keeps_prepared_identity_and_rejects_foreign_provider_frames() {
    const MODE: &str = "CAGENTS_RETAINED_PREPARED_ID_TEST";
    if std::env::var_os(MODE).is_none() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = directory.path();
        for path in [
            "home/.claude",
            "run",
            "data",
            "config",
            "cache",
            "bin",
            "project",
        ] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        let test = thread::current().name().unwrap().to_owned();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env_clear()
            .env(MODE, "1")
            .env(
                "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                retained_actor_tests::retained_helper_executable(),
            )
            .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
            .env("CAGENTS_HOLDER_ROOT", root)
            .env("HOME", root.join("home"))
            .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    fn wait(mut condition: impl FnMut() -> bool, reason: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "{reason}");
            thread::sleep(Duration::from_millis(10));
        }
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    install_provider(&root.join("bin/claude"));
    let proof = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("prepared-identity-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: crate::model::HostIncarnation::new("first"),
    };
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let mut state = HostState::new(proof.clone());
    state.install_model(super::super::model::Registry::at(&root.join("registry.sqlite3")).unwrap());
    let host = Arc::new(HostLock::new(state));
    let source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    assert!(source.launch.session_id.is_none());
    let id = source.id;
    let context =
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap());
    // This is the real HOST entry, including its first protocol::create call.
    start_execution(&host, 0, source, context).unwrap();
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    let identity = handle
        .actor_identity()
        .expect("actual retained helper required");
    struct Cleanup {
        handle: Handle,
        record: crate::daemon::holder_registry::Record,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if self.handle.close().is_ok() && self.handle.finish_retained_close().is_ok() {
                return;
            }
            // Failed fixtures kill only their own exact process incarnations;
            // this is teardown, not evidence of semantic retirement.
            for process in [&self.record.root, &self.record.holder] {
                if process.is_current() {
                    unsafe {
                        libc::kill(process.pid, libc::SIGKILL);
                    }
                }
            }
        }
    }
    let _cleanup = Cleanup {
        handle: handle.clone(),
        record: identity.record.clone(),
    };
    wait(
        || handle.attachment_snapshot().ready,
        "provider did not initialize",
    );
    handle
        .command(chat::Command::Prompt {
            text: "identity probe".into(),
            attachments: Vec::new(),
            mode: chat::PromptMode::Send,
        })
        .unwrap();
    wait(
        || root.join("received.json").exists(),
        "provider did not receive the first prompt",
    );
    let received: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("received.json")).unwrap()).unwrap();
    assert_eq!(
        received["argv_id"], received["prompt_id"],
        "first prompt must use the HOST-prepared provider UUID before any provider reply"
    );
    assert_eq!(
        received["resume"], false,
        "fresh launch must not become a disk resume"
    );
    let inspection = Handle::inspect_remote(&identity.record, proof).unwrap();
    assert!(
        inspection.source.launch.session_id.is_none(),
        "provisional UUID must not rewrite the issued pre-ID source"
    );
    assert!(
        inspection.state.confirmed_session.is_none(),
        "provider has not replied yet"
    );
    fs::write(root.join("release"), b"release").unwrap();
    wait(
        || handle.attachment_snapshot().session_id.as_deref() == received["argv_id"].as_str(),
        "exact provider reply was not accepted",
    );
    fs::write(root.join("foreign"), b"foreign").unwrap();
    wait(
        || {
            handle
                .attachment_snapshot()
                .error
                .as_deref()
                .is_some_and(|error| error.contains("different session ID"))
        },
        "foreign provider identity must fail closed",
    );
    let snapshot = handle.snapshot(None).unwrap();
    assert!(!snapshot.items.iter().any(|item| matches!(&item.content,
        chat::ItemContent::Text { text, .. } if text.contains("FOREIGN_CONTENT_MUST_NOT_ENTER"))));
}

fn install_provider(path: &Path) {
    fs::write(path, r#"#!/usr/bin/python3
import json, pathlib, sys, time, uuid
root=pathlib.Path(__file__).resolve().parent.parent
session=next(arg.split('=',1)[1] for arg in sys.argv[1:] if arg.startswith('--session-id='))
def send(frame): print(json.dumps(frame),flush=True)
def wait(name):
    deadline=time.monotonic()+15
    while not (root/name).exists():
        if time.monotonic()>=deadline: raise RuntimeError('fixture release timed out')
        time.sleep(0.01)
for line in sys.stdin:
    frame=json.loads(line)
    if frame.get('type')=='control_request':
        send({'type':'control_response','response':{'subtype':'success','request_id':frame['request_id'],'response':{}}})
    elif frame.get('type')=='user':
        receipt={'argv_id':session,'prompt_id':frame.get('session_id'),'resume':any(arg=='--resume' or arg.startswith('--resume=') or arg=='-r' for arg in sys.argv[1:])}
        temporary=root/'received.tmp'
        temporary.write_text(json.dumps(receipt));temporary.replace(root/'received.json')
        wait('release')
        send({'type':'user','uuid':frame['uuid'],'session_id':session,'message':{'role':'user','content':'identity probe'}})
        send({'type':'result','uuid':'own-result','subtype':'success','session_id':session,'result':''})
        wait('foreign')
        send({'type':'assistant','uuid':'foreign-frame','session_id':str(uuid.uuid4()),'message':{'id':'foreign-message','role':'assistant','content':[{'type':'text','text':'FOREIGN_CONTENT_MUST_NOT_ENTER'}]}})
"#).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
