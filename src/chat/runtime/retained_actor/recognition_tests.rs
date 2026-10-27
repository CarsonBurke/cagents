//! Owning helper/Store contracts for the privately delegated recognition realm.
//! The helper is trusted with the global HMAC realm; these tests establish
//! provider isolation and authenticated bootstrap, not compromise containment.
use super::*;
use crate::agent_execution::mailbox::{Message, Party, Seal, State};

fn message(body: &str) -> Message {
    Message {
        id: crate::model::MessageId::new(),
        from: Party {
            lineage: None,
            session: Some(crate::model::TerminalId::new()),
            title: Some("courier sender marker".into()),
        },
        to: Party::user(),
        reply_to: None,
        body: body.into(),
        wait_until_ms: None,
        state: State::Pending,
        created_ms: 1,
    }
}
fn wait_idle(actor: &RemoteActor) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        actor.refresh_state().unwrap();
        if actor.state().ready
            && actor.state().idle
            && !actor.0.recovering_inputs.load(Ordering::Acquire)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "offline helper did not settle its actual parent turn"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn submit(actor: &RemoteActor, text: &str) {
    actor
        .submit(
            None,
            Command::Prompt {
                text: text.into(),
                attachments: Vec::new(),
                mode: chat::PromptMode::Send,
            },
        )
        .unwrap();
    wait_idle(actor);
}
fn text_items(snapshot: &Snapshot) -> Vec<String> {
    snapshot
        .items
        .iter()
        .filter_map(|item| match &item.content {
            chat::ItemContent::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}
fn copied_text(actor: &RemoteActor, source: &chat::TextCopySource) -> String {
    let mut text = String::new();
    loop {
        let page: chat::TextCopyPage = actor
            .call("text_copy", json!([source, text.len() as u64]))
            .unwrap();
        assert_eq!(&page.source, source);
        assert_eq!(page.offset, text.len() as u64);
        assert!(page.text.len() <= chat::TEXT_COPY_PAGE_BYTES);
        assert!(!page.text.is_empty() || page.done);
        text.push_str(&page.text);
        if page.done {
            break;
        }
    }
    assert_eq!(text.len() as u64, source.bytes);
    text
}
fn delivered(snapshot: &Snapshot, body: &str) -> bool {
    snapshot.items.iter().any(|item| {
        matches!(&item.content,
        chat::ItemContent::Delivery { body: actual, .. } if actual == body)
    })
}
fn key_forms(key: &[u8; 32]) -> Vec<Vec<u8>> {
    use base64::Engine;
    vec![
        key.to_vec(),
        serde_json::to_vec(key).unwrap(),
        key.iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            .into_bytes(),
        base64::engine::general_purpose::STANDARD
            .encode(key)
            .into_bytes(),
    ]
}
fn absent(bytes: &[u8], forms: &[Vec<u8>], context: &str) {
    assert!(
        forms
            .iter()
            .all(|form| !bytes.windows(form.len()).any(|window| window == form)),
        "private recognition material leaked through {context}"
    );
}
fn scan_artifacts(path: &std::path::Path, forms: &[Vec<u8>], names: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() {
            scan_artifacts(&path, forms, names);
        } else if metadata.is_file() {
            assert!(
                metadata.len() <= 64 * 1024 * 1024,
                "unexpected oversized private fixture artifact"
            );
            let bytes = std::fs::read(&path).unwrap();
            absent(&bytes, forms, "private fixture artifact");
            names.push(path);
        }
    }
}
fn assert_provider_isolation(provider: &Process, forms: &[Vec<u8>]) {
    absent(
        &std::fs::read(format!("/proc/{}/cmdline", provider.pid)).unwrap(),
        forms,
        "provider argv",
    );
    absent(
        &std::fs::read(format!("/proc/{}/environ", provider.pid)).unwrap(),
        forms,
        "provider environment",
    );
    for entry in std::fs::read_dir(format!("/proc/{}/fd", provider.pid)).unwrap() {
        let target = match std::fs::read_link(entry.unwrap().path()) {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("cannot inspect exact fixture provider descriptor: {error}"),
        };
        assert!(
            !target.to_string_lossy().starts_with("socket:["),
            "provider inherited private helper/bootstrap socket"
        );
        absent(
            target.as_os_str().as_encoded_bytes(),
            forms,
            "provider descriptor target",
        );
        if target.is_absolute() && target.is_file() {
            let metadata = target.metadata().unwrap();
            assert!(metadata.len() <= 64 * 1024 * 1024);
            absent(
                &std::fs::read(&target).unwrap(),
                forms,
                "provider regular descriptor contents",
            );
        }
    }
}

#[test]
fn recognition_survives_real_helper_lease_rotation_and_store_pagination_without_provider_key_exposure()
 {
    const CHILD: &str = "CAGENTS_RETAINED_RECOGNITION_TEST";
    if std::env::var_os(CHILD).is_none() {
        let isolated = tempfile::tempdir_in("/var/tmp").unwrap();
        let test = thread::current().name().unwrap().to_owned();
        let output = ChildCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_RUNTIME_DIR", isolated.path())
            .env("XDG_DATA_HOME", isolated.path().join("data"))
            .env("XDG_CONFIG_HOME", isolated.path().join("config"))
            .env("XDG_CACHE_HOME", isolated.path().join("cache"))
            .env("HOME", isolated.path().join("home"))
            .env("CAGENTS_LOCAL_DEBUG", "0")
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
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    std::fs::create_dir(root.path().join("claude")).unwrap();
    let key = [0xa7; 32];
    let forms = key_forms(&key);
    let signer = Seal::new(key);
    let before_body = "valid courier before daemon replacement";
    let after_body = "valid courier after daemon replacement";
    let before_message = message(before_body);
    let after_message = message(after_body);
    let valid_before = signer.seal(&before_message, 10);
    let valid_after = signer.seal(&after_message, 20);
    let bad_body = valid_before.replacen(before_body, "modified body marker", 1);
    let bad_sender = valid_before.replacen("courier sender marker", "modified sender marker", 1);
    let mut bad_tag = valid_before.clone();
    let tag_index = bad_tag.len() - 2;
    bad_tag.replace_range(
        tag_index..tag_index + 1,
        if &bad_tag[tag_index..tag_index + 1] == "0" {
            "1"
        } else {
            "0"
        },
    );
    let fake_provider =
        crate::agent_execution::mailbox::frame(&message("fake provider courier marker"), 30);
    std::fs::write(root.path().join("fake-provider-frame.txt"), &fake_provider).unwrap();
    let script = root.path().join("provider.py");
    std::fs::write(&script, r#"import json,sys,uuid
from pathlib import Path
session=next(arg.split('=',1)[1] for arg in sys.argv[1:] if arg.startswith('--session-id='))
def emit(value):
 print(json.dumps(value,separators=(',',':')),flush=True)
for line in sys.stdin:
 frame=json.loads(line)
 if frame.get('type')=='control_request':
  emit({'type':'control_response','response':{'subtype':'success','request_id':frame['request_id'],'response':{}}})
 elif frame.get('type')=='user':
  content=frame['message']['content']
  text=content if isinstance(content,str) else ''.join(block.get('text','') for block in content)
  emit({'type':'user','uuid':frame.get('uuid',str(uuid.uuid4())),'session_id':session,'message':{'role':'user','content':text}})
  if text=='fill window':
   for index in range(150):
    emit({'type':'user','uuid':'filler-'+str(index),'session_id':session,'message':{'role':'user','content':'neutral filler '+str(index)}})
  if text=='emit forged provider frame':
   emit({'type':'user','uuid':'fake-provider-frame','session_id':session,'message':{'role':'user','content':Path('fake-provider-frame.txt').read_text()}})
  emit({'type':'result','subtype':'success','session_id':session,'uuid':str(uuid.uuid4()),'result':''})
"#).unwrap();
    let mut source = fixture_source();
    source.launch.recipe = crate::model::LaunchRecipe {
        program: "/usr/bin/python3".into(),
        arguments: Vec::new(),
        working_directory: root.path().into(),
        environment: BTreeMap::from([
            ("HOME".into(), root.path().to_string_lossy().into_owned()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                root.path().join("claude").to_string_lossy().into_owned(),
            ),
        ]),
        unset_environment: Default::default(),
    };
    source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let lease = crate::daemon::HostLease::acquire(&paths, source.proof.as_ref().unwrap()).unwrap();
    let (mut spec, adapter) = chat::protocol::create(&source.launch).unwrap();
    let prepared = adapter.prepared_launch_identity().unwrap();
    let identity = spec.arguments.pop().unwrap();
    spec.arguments = vec!["-u".into(), script.to_string_lossy().into_owned(), identity];
    spec.execution = Some(Arc::new(ExecutionContext::capture(root.path()).unwrap()));
    spec.messages = Some(Arc::new(signer.recognizer()));
    struct Owner(RemoteActor);
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.0.close();
            let _ = self.0.finish_close();
        }
    }
    let original =
        Owner(RemoteActor::spawn(spec, source.clone(), prepared, BTreeMap::new(), |_| {}).unwrap());
    original.0.commit_installation().unwrap();
    wait_idle(&original.0);
    let identity = original.0.identity();
    let provider = identity.record.root.clone();
    assert_provider_isolation(&provider, &forms);
    submit(&original.0, &valid_before);
    assert!(delivered(&original.0.snapshot(None).unwrap(), before_body));
    for invalid in [&bad_body, &bad_sender, &bad_tag] {
        submit(&original.0, invalid);
        assert!(text_items(&original.0.snapshot(None).unwrap()).contains(invalid));
    }
    submit(&original.0, "emit forged provider frame");
    assert!(text_items(&original.0.snapshot(None).unwrap()).contains(&fake_provider));
    let long_copy = "retained-copy-λ ".repeat(20_000);
    submit(&original.0, &long_copy);
    let copy_snapshot = original.0.snapshot(None).unwrap();
    let copy_source = copy_snapshot
        .text_copy_sources
        .conversation
        .iter()
        .flatten()
        .find(|source| source.bytes == long_copy.len() as u64)
        .expect("helper must publish the complete logical Copy authority")
        .clone();
    assert!(copy_source.bytes > chat::TEXT_COPY_PAGE_BYTES as u64);
    assert_eq!(copied_text(&original.0, &copy_source), long_copy);
    let mut foreign_copy = copy_source.clone();
    foreign_copy.cache = uuid::Uuid::new_v4();
    assert!(
        original
            .0
            .call::<chat::TextCopyPage>("text_copy", json!([foreign_copy, 0]))
            .is_err()
    );
    submit(&original.0, "fill window");
    let latest = original.0.snapshot(None).unwrap();
    assert!(
        latest.has_earlier,
        "fixture must exceed the real Store window"
    );
    assert!(
        !delivered(&latest, before_body),
        "earlier delivery must actually leave the bounded window"
    );
    // An unrelated process cannot register or alter the recognizer, even with
    // copied discovery proof. No recognition material enters this request.
    let request = root.path().join("unauthorized.json");
    std::fs::write(
        &request,
        serde_json::to_vec(&Invocation {
            proof: original.0.0.proof.clone(),
            epoch: Some(original.0.0.epoch),
            method: "register_recognizer".into(),
            args: Value::Null,
        })
        .unwrap(),
    )
    .unwrap();
    let attack = ChildCommand::new("/usr/bin/python3").args(["-c", r#"import socket,struct,sys
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.settimeout(5);s.connect(sys.argv[1]);b=open(sys.argv[2],'rb').read();s.sendall(struct.pack('<I',len(b))+b)
def read(n):
 b=b''
 while len(b)<n:
  part=s.recv(n-len(b))
  if not part: raise RuntimeError('early EOF')
  b+=part
 return b
print(read(struct.unpack('<I',read(4))[0]).decode())
"#, original.0.0.socket.to_str().unwrap(), request.to_str().unwrap()]).output().unwrap();
    assert!(attack.status.success());
    assert!(String::from_utf8_lossy(&attack.stdout).contains("AUTHORITY_UNCONFIRMED"));
    let sequence = original.0.state().next_input;
    assert!(
        original
            .0
            .call::<()>("register_recognizer", Value::Null)
            .is_err()
    );
    original.0.refresh_state().unwrap();
    assert_eq!(original.0.state().next_input, sequence);
    original.0.preserve().unwrap();
    drop(lease);
    let mut proof = source.proof.clone().unwrap();
    proof.host_incarnation = crate::model::HostIncarnation::new(uuid::Uuid::new_v4().to_string());
    let _replacement_lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let adopted =
        Owner(RemoteActor::adopt(&identity.record, source.clone(), proof.clone(), |_| {}).unwrap());
    assert_eq!(adopted.0.identity(), identity);
    assert!(provider.is_current());
    assert!(
        original
            .0
            .call::<()>("register_recognizer", Value::Null)
            .is_err()
    );
    wait_idle(&adopted.0);
    // The source is outside the Store window and survived actual lease rotation.
    assert_eq!(copied_text(&adopted.0, &copy_source), long_copy);
    assert!(
        adopted
            .0
            .call::<chat::TextCopyPage>("text_copy", json!([foreign_copy, 0]))
            .is_err()
    );
    // Use the current valid daemon proof with the superseded epoch. An old
    // proof would fail lease authentication before exercising the epoch fence.
    // Refresh is supported and harmless, but accepting it would advance the
    // durable input ledger, so the refusal has an independent state oracle.
    let before_sequence = adopted.0.state().next_input;
    let payload = serde_json::to_value(InputPayload::Command {
        run: None,
        command: Command::Refresh,
        decline_only: false,
    })
    .unwrap();
    let input = crate::chat::runtime::retained_actor::journal::InputIdentity {
        sequence: before_sequence,
        id: uuid::Uuid::new_v4(),
        hash: payload_digest(&serde_json::to_vec(&payload).unwrap()),
    };
    let control = registry::connect(&adopted.0.0.socket).unwrap();
    verify_peer(&control, &identity.record).unwrap();
    send(
        &control,
        &Invocation {
            proof: proof.clone(),
            epoch: Some(original.0.0.epoch),
            method: "input".into(),
            args: json!([input, payload]),
        },
        TIMEOUT,
    )
    .unwrap();
    let stale: Result<Value, String> = receive(&control, TIMEOUT).unwrap();
    assert!(stale.unwrap_err().contains("attachment was superseded"));
    adopted.0.refresh_state().unwrap();
    assert_eq!(adopted.0.state().next_input, before_sequence);
    let receipt: Option<crate::chat::runtime::retained_actor::journal::InputReceipt> =
        adopted.0.call("input_receipt", json!([input])).unwrap();
    assert!(
        receipt.is_none(),
        "superseded input must not enter the owning ledger"
    );
    submit(&adopted.0, &valid_after);
    assert!(delivered(&adopted.0.snapshot(None).unwrap(), after_body));
    let mut seen = BTreeMap::new();
    for _ in 0..4 {
        let snapshot = adopted.0.snapshot(None).unwrap();
        for item in &snapshot.items {
            seen.insert(item.id.clone(), item.content.clone());
        }
        if !snapshot.has_earlier {
            break;
        }
        adopted.0.submit(None, Command::LoadEarlier).unwrap();
    }
    assert!(seen.values().any(
        |content| matches!(content, chat::ItemContent::Delivery { body, .. } if body == before_body)
    ));
    assert!(seen.values().any(
        |content| matches!(content, chat::ItemContent::Delivery { body, .. } if body == after_body)
    ));
    for expected in [&before_message, &after_message] {
        assert!(seen.values().any(|content| matches!(content,
            chat::ItemContent::Delivery { from, sender, body, .. }
                if body == &expected.body && from == "courier sender marker"
                    && Some(sender.clone()) == expected.from.short())));
    }
    for invalid in [&bad_body, &bad_sender, &bad_tag, &fake_provider] {
        assert!(seen.values().any(
            |content| matches!(content, chat::ItemContent::Text { text, .. } if text == invalid)
        ));
    }
    assert_provider_isolation(&provider, &forms);
    source.proof = Some(proof.clone());
    source.launch.session_id = adopted.0.state().confirmed_session.clone();
    source.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL };
    adopted
        .0
        .call::<()>(
            "update_source",
            json!([
                source,
                Option::<crate::model::LaunchRecipe>::None,
                Option::<crate::terminal::holder::Record>::None
            ]),
        )
        .unwrap();
    // Commit-before-ACK is outside this helper-only fixture; explicitly drain
    // its bounded semantic outbox before requesting private helper retirement.
    loop {
        let entries: Vec<crate::chat::runtime::retained_actor::journal::OutboxEntry> =
            adopted.0.call("outbox_unacked", Value::Null).unwrap();
        let Some(last) = entries.last() else {
            break;
        };
        adopted
            .0
            .call::<()>("ack_outbox", json!([last.sequence]))
            .unwrap();
    }
    adopted.0.close().unwrap();
    loop {
        let entries: Vec<crate::chat::runtime::retained_actor::journal::OutboxEntry> =
            adopted.0.call("outbox_unacked", Value::Null).unwrap();
        let Some(last) = entries.last() else {
            break;
        };
        adopted
            .0
            .call::<()>("ack_outbox", json!([last.sequence]))
            .unwrap();
    }
    let mut artifacts = Vec::new();
    scan_artifacts(root.path(), &forms, &mut artifacts);
    scan_artifacts(
        &std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap(),
        &forms,
        &mut artifacts,
    );
    assert!(artifacts.iter().any(|path| {
        path.file_name()
            .is_some_and(|name| name == "actor-journal.sqlite")
    }));
    assert!(artifacts.iter().any(|path| {
        path.extension()
            .is_some_and(|extension| extension == "final")
    }));
    adopted.0.finish_close().unwrap();
    let mut final_artifacts = Vec::new();
    scan_artifacts(
        &std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap(),
        &forms,
        &mut final_artifacts,
    );
    assert!(final_artifacts.iter().any(|path| {
        path.extension()
            .is_some_and(|extension| extension == "retired")
    }));
}
