//! Public ChatAttach reaches the real lease-authenticated retained helper.
//! The only provider is a private executable that accepts initialization, never
//! a prompt. The existing outer harness pins its original child and bounds the
//! entire isolated case to forty seconds with capped regular-file diagnostics.
use super::*;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

#[test]
fn fresh_pre_id_public_attach_reuses_exact_prepared_provider_on_retry() {
    let exact = "host::chat::retained_actor_tests::restart_identity::fresh_pre_id_public_attach_reuses_exact_prepared_provider_on_retry";
    if chat::runtime::isolated_retained_actor_contract(exact) {
        return;
    }
    exercise(false);
}

#[test]
fn fresh_pre_id_public_attach_refuses_a_different_actual_system_identity() {
    let exact = "host::chat::retained_actor_tests::restart_identity::fresh_pre_id_public_attach_refuses_a_different_actual_system_identity";
    if chat::runtime::isolated_retained_actor_contract(exact) {
        return;
    }
    exercise(true);
}

struct Cleanup(Vec<FixtureProcessPin>);
impl Cleanup {
    fn pin(&mut self, handle: &Handle) {
        if let Some(process) = handle
            .provider_process()
            .filter(|process| process.is_current())
        {
            match FixtureProcessPin::capture(&process) {
                Ok(pin) => self.0.push(pin),
                // A rejected init may already have retired the provider. No
                // late numeric signal is authorized by its cached identity.
                Err(_) if !process.is_current() => {}
                Err(error) => panic!("cannot pin live fixture provider: {error}"),
            }
        }
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        for pin in &self.0 {
            let _ = pin.signal(libc::SIGKILL);
        }
    }
}

fn exercise(mismatch: bool) {
    // This is the sole selected test in a separate process with isolated HOME,
    // XDG paths and logs. No environment is changed in the shared test process.
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let root = home.parent().unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir(root.join("project")).unwrap();
    let program = root.join("claude-peer");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&program)
        .unwrap();
    file.write_all(PROVIDER.as_bytes()).unwrap();
    drop(file);
    if mismatch {
        fs::write(root.join("mismatch"), b"1").unwrap();
    }
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = HostSessionProof {
        host_session_id: "pre-id-public-restart".into(),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: "owning-fixture".into(),
    };
    let lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let host = Arc::new(HostLock::new(HostState::new(proof)));
    host.lock().unwrap().register_client(1);
    let mut launch = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    launch.launch.recipe.program = program.to_string_lossy().into_owned();
    let id = launch.id;
    start_execution(
        &host,
        1,
        launch,
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
    )
    .unwrap();
    let actor = host.lock().unwrap().chats[&id].handle.clone();
    let identity = actor
        .actor_identity()
        .expect("genuine execed retained helper");
    let mut cleanup = Cleanup(vec![
        FixtureProcessPin::capture(&identity.record.root).unwrap(),
        FixtureProcessPin::capture(&identity.record.holder).unwrap(),
    ]);
    wait_until(
        || actor.attachment_snapshot().ready,
        "initial pre-ID provider did not become ready",
    );
    cleanup.pin(&actor);
    assert!(actor.confirmed_session_id().is_none());
    let ServerResponse::ChatAttached {
        session: source, ..
    } = session_snapshot(&host.lock().unwrap().chats[&id])
    else {
        unreachable!()
    };
    assert!(source.launch.session_id.is_none());
    fs::write(root.join("complete-original"), b"1").unwrap();
    wait_until(
        || actor.is_completed(),
        "original pre-ID provider did not complete",
    );
    assert!(actor.confirmed_session_id().is_none());
    assert_eq!(spawns(root).len(), 1);

    let mut forged = source.clone();
    forged
        .launch
        .recipe
        .environment
        .insert("DIFFERENT_ACCOUNT".into(), "not-issued".into());
    assert!(
        super::super::process(&host, 1, ClientRequest::ChatAttach { session: forged }).is_err()
    );
    assert_eq!(
        spawns(root).len(),
        1,
        "invalid source must not start a provider"
    );
    let response = super::super::process(
        &host,
        1,
        ClientRequest::ChatAttach {
            session: source.clone(),
        },
    );
    let records = spawns(root);
    assert_eq!(
        records.len(),
        2,
        "one explicit restart starts exactly one new provider"
    );
    let prepared = records[1]["prepared_id"].as_str().unwrap();
    let replacement = host.lock().unwrap().chats[&id].handle.clone();
    assert!(
        replacement.same_actor(&actor),
        "restart preserves the exact retained owner"
    );
    assert_eq!(replacement.actor_identity(), Some(identity.clone()));
    cleanup.pin(&replacement);
    if mismatch {
        assert!(
            response.is_err(),
            "a different actual system/init must never attach"
        );
        assert!(replacement.confirmed_session_id().is_none());
        assert!(!root.join("prompts").exists());
        // An explicit management close must retire the same owner; no attach
        // retry here can silently generate another incarnation after refusal.
        replacement.close().unwrap();
        replacement.finish_retained_close().unwrap();
    } else {
        let ServerResponse::ChatRestarted { receipt, .. } = response.unwrap() else {
            panic!("public attach omitted typed restart receipt")
        };
        assert_eq!(receipt.source(), &source);
        assert_eq!(
            receipt.replacement().launch.session_id.as_deref(),
            Some(prepared)
        );
        assert_eq!(
            replacement.confirmed_session_id().as_deref(),
            Some(prepared)
        );
        // Simulate losing the successful response and retry with the original
        // unnamed descriptor after the host has projected the confirmed ID.
        let ServerResponse::ChatRestarted {
            receipt: retried, ..
        } = super::super::process(
            &host,
            1,
            ClientRequest::ChatAttach {
                session: source.clone(),
            },
        )
        .unwrap()
        else {
            panic!("retry lost original receipt")
        };
        assert_eq!(
            serde_json::to_value(&retried).unwrap(),
            serde_json::to_value(&receipt).unwrap()
        );
        assert_eq!(
            spawns(root).len(),
            2,
            "retry must not regenerate prepared argv or spawn again"
        );
        assert!(
            host.lock().unwrap().chats[&id]
                .handle
                .same_actor(&replacement)
        );
        assert!(
            !root.join("prompts").exists(),
            "initialization and attach never replay a prompt"
        );
        super::super::process(
            &host,
            1,
            ClientRequest::ChatCloseSession {
                session: receipt.replacement().clone(),
            },
        )
        .unwrap();
    }
    wait_until(
        || !identity.record.root.is_current() && !identity.record.holder.is_current(),
        "explicit close did not retire exact helper owner",
    );
    drop(host);
    drop(lease);
}

fn spawns(root: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(root.join("spawns.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

const PROVIDER: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
root=pathlib.Path(os.environ['HOME']).parent
prepared=next(arg.split('=',1)[1] for arg in sys.argv[1:] if arg.startswith('--session-id='))
log=root/'spawns.jsonl'
index=len(log.read_text().splitlines()) if log.exists() else 0
with log.open('a') as output:
    output.write(json.dumps({'pid':os.getpid(),'prepared_id':prepared,'argv':sys.argv[1:]})+'\n')
def send(value): print(json.dumps(value),flush=True)
for line in sys.stdin:
    frame=json.loads(line)
    if frame.get('type')!='control_request':
        (root/'prompts').write_text('unexpected provider input')
        raise RuntimeError('fixture accepts initialization only, never prompt replay')
    send({'type':'control_response','response':{'subtype':'success','request_id':frame['request_id'],'response':{}}})
    if index==0:
        deadline=time.monotonic()+10
        while not (root/'complete-original').exists():
            if time.monotonic()>=deadline: raise RuntimeError('original completion barrier expired')
            time.sleep(.01)
        sys.exit(23)
    else:
        actual='bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb' if (root/'mismatch').exists() else prepared
        send({'type':'system','subtype':'init','session_id':actual})
"#;
