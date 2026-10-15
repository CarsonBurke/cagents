// Included in the existing real creation fixture module, not a fabricated
// receipt builder. Each test performs actual journaled Git creation first.
fn received_original_roles(created: &SettledCreatedWorktree) -> [fs_safety::Root; 6] {
    use std::os::unix::fs::MetadataExt;
    let roots = created.transport_roots().unwrap();
    [
        roots.source.0,
        roots.source.1,
        roots.source.2,
        roots.destination.0,
        roots.destination.1,
        roots.destination.2,
    ]
    .map(|root| {
        let metadata = root.metadata().unwrap();
        fs_safety::Root::from_received_creation_directory(
            root.retained_directory().unwrap().into(),
            root.path().to_owned(),
            (metadata.dev(), metadata.ino()),
        )
        .unwrap()
    })
}

#[test]
fn received_created_scopes_bind_actual_journal_and_original_six_roles() {
    if isolated_creation_fixture(concat!(
        module_path!(),
        "::received_created_scopes_bind_actual_journal_and_original_six_roles"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let scoped = fixture
        .service
        .clone()
        .with_worktree_authority(WorktreeAuthority::capture(&fixture.request.repository).unwrap())
        .unwrap();
    let (sender, _) = async_channel::bounded(8);
    let retained = scoped.create_worktree_journaled_retained(
        fixture.store.clone(),
        &fixture.request,
        7,
        &CancellationToken::new(),
        &sender,
    );
    let created = retained
        .created
        .as_ref()
        .expect("actual settled original carrier");
    let accepted = ReceivedCreationScopes::validate_received(
        &fixture.store,
        created.operation(),
        created.generation(),
        received_original_roles(created),
    )
    .unwrap();
    assert_eq!(accepted.operation(), created.operation());
    assert_eq!(accepted.generation(), 7);
    assert_eq!(accepted.record(), created.record());
    assert_eq!(accepted.source().path(), fixture.request.repository);
    assert_eq!(accepted.destination().path(), fixture.request.destination);
    accepted.revalidate(&fixture.store).unwrap();
    let admin = created.transport_roots().unwrap().destination.1;
    let admin_path = admin.path().to_owned();
    fs::write(admin_path.join("locked"), b"later operation").unwrap();
    assert!(
        accepted.revalidate(&fixture.store).is_err(),
        "late lock cannot pass final grant admission"
    );
    fs::remove_file(admin_path.join("locked")).unwrap();
    let head = fs::read(admin_path.join("HEAD")).unwrap();
    fs::write(admin_path.join("HEAD"), b"ref: refs/heads/another-branch\n").unwrap();
    assert!(
        accepted.revalidate(&fixture.store).is_err(),
        "late branch redirect cannot pass final grant admission"
    );
    fs::write(admin_path.join("HEAD"), head).unwrap();
    accepted.revalidate(&fixture.store).unwrap();
    let mut wrong_roles = received_original_roles(created);
    wrong_roles.swap(1, 4); // actual source and destination admin are DIFFERENT
    assert!(
        ReceivedCreationScopes::validate_received(
            &fixture.store,
            created.operation(),
            7,
            wrong_roles
        )
        .is_err()
    );
    assert!(
        ReceivedCreationScopes::validate_received(
            &fixture.store,
            created.operation(),
            8,
            received_original_roles(created)
        )
        .is_err()
    );
    assert!(
        ReceivedCreationScopes::validate_received(
            &fixture.store,
            OperationId::new(),
            7,
            received_original_roles(created)
        )
        .is_err()
    );
    fixture
        .store
        .release_worktree_creation(created.operation())
        .unwrap();
    assert!(
        accepted.revalidate(&fixture.store).is_err(),
        "operation retirement cannot leave grant admission valid"
    );
}

#[test]
fn received_created_scopes_refuse_name_retarget_without_reading_replacement() {
    if isolated_creation_fixture(concat!(
        module_path!(),
        "::received_created_scopes_refuse_name_retarget_without_reading_replacement"
    )) {
        return;
    }
    for replaced in ["source", "destination", "admin"] {
        let fixture = Fixture::new();
        let scoped = fixture
            .service
            .clone()
            .with_worktree_authority(
                WorktreeAuthority::capture(&fixture.request.repository).unwrap(),
            )
            .unwrap();
        let (sender, _) = async_channel::bounded(8);
        let retained = scoped.create_worktree_journaled_retained(
            fixture.store.clone(),
            &fixture.request,
            7,
            &CancellationToken::new(),
            &sender,
        );
        let created = retained
            .created
            .as_ref()
            .expect("actual settled original carrier");
        let roots = received_original_roles(created); // retain BEFORE late replacement
        let index = match replaced {
            "source" => 0,
            "destination" => 3,
            _ => 4,
        };
        let name = roots[index].path().to_owned();
        let parked = fixture.directory.path().join("parked-original");
        fs::rename(&name, &parked).unwrap();
        fs::create_dir(&name).unwrap();
        let marker = name.join("foreign");
        fs::write(&marker, b"untouched replacement").unwrap();
        assert!(
            ReceivedCreationScopes::validate_received(
                &fixture.store,
                created.operation(),
                7,
                roots
            )
            .is_err()
        );
        assert!(
            created.read_original_journal().is_err(),
            "producer cannot substitute a late named scope"
        );
        assert_eq!(fs::read(marker).unwrap(), b"untouched replacement");
        assert_eq!(
            fs::read_dir(&name).unwrap().count(),
            1,
            "no constructor/history/process effects at receiver"
        );
    }
}

#[test]
fn received_creation_single_row_read_refuses_oversized_payload() {
    let fixture = Fixture::new();
    let id = OperationId::new();
    fixture
        .store
        .begin_operation(&PendingOperation {
            id,
            entity_kind: "worktree_creation".into(),
            entity_id: "bounded-receiver".into(),
            action: "create".into(),
            base_generation: 7,
            payload: serde_json::Value::String("x".repeat(512 * 1024 + 1)),
            status: PendingOperationStatus::Pending,
            created_at_unix_ms: operation_timestamp(),
            updated_at_unix_ms: operation_timestamp(),
        })
        .unwrap();
    assert!(fixture.store.journaled_worktree_operation(id).is_err());
    assert!(
        fixture
            .store
            .journaled_worktree_operation(OperationId::new())
            .unwrap()
            .is_none()
    );
}

#[test]
fn original_creation_journal_uses_same_connection_and_never_a_decoy_store() {
    if isolated_creation_fixture(concat!(
        module_path!(),
        "::original_creation_journal_uses_same_connection_and_never_a_decoy_store"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let scoped = fixture
        .service
        .clone()
        .with_worktree_authority(WorktreeAuthority::capture(&fixture.request.repository).unwrap())
        .unwrap();
    let (sender, _) = async_channel::bounded(8);
    let retained = scoped.create_worktree_journaled_retained(
        fixture.store.clone(),
        &fixture.request,
        7,
        &CancellationToken::new(),
        &sender,
    );
    let created = retained
        .created
        .as_ref()
        .expect("actual settled original carrier");
    assert!(created.uses_original_store(&fixture.store));
    assert!(created.uses_original_store(&fixture.store.clone()));
    let checked = created.read_original_journal().unwrap();
    let saved = fixture
        .store
        .journaled_worktree_operation(created.operation())
        .unwrap()
        .unwrap();
    assert_eq!(checked.operation(), &saved);
    let bytes = creation_journal::encode_original_creation_operation(&checked).unwrap();
    assert!(bytes.len() <= creation_journal::CREATION_JOURNAL_BODY_LIMIT);
    assert_eq!(
        creation_journal::decode_creation_operation_description(&bytes).unwrap(),
        saved
    );

    // A second real store can hold the same plausible creation, but the
    // carrier's API accepts no selector and must still consult its original.
    let decoy = Arc::new(
        StateStore::open(
            fixture.directory.path().join("decoy.sqlite"),
            BootId::new("decoy-boot"),
        )
        .unwrap(),
    );
    assert!(!created.uses_original_store(&decoy));
    let mut pending = saved.clone();
    pending.status = PendingOperationStatus::Pending;
    decoy.begin_operation(&pending).unwrap();
    decoy
        .resolve_operation(pending.id, PendingOperationStatus::Applied)
        .unwrap();
    ReceivedCreationScopes::validate_received(
        &decoy,
        created.operation(),
        7,
        received_original_roles(created),
    )
    .unwrap();
    fixture
        .store
        .release_worktree_creation(created.operation())
        .unwrap();
    assert!(
        created.read_original_journal().is_err(),
        "matching decoy must never replace the original journal"
    );
}

#[test]
fn original_creation_journal_retains_connection_and_refuses_its_shutdown() {
    if isolated_creation_fixture(concat!(
        module_path!(),
        "::original_creation_journal_retains_connection_and_refuses_its_shutdown"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let scoped = fixture
        .service
        .clone()
        .with_worktree_authority(WorktreeAuthority::capture(&fixture.request.repository).unwrap())
        .unwrap();
    let (sender, _) = async_channel::bounded(8);
    let retained = scoped.create_worktree_journaled_retained(
        fixture.store.clone(),
        &fixture.request,
        7,
        &CancellationToken::new(),
        &sender,
    );
    let created = retained
        .created
        .as_ref()
        .expect("actual settled original carrier")
        .clone();
    let weak = Arc::downgrade(&fixture.store);
    drop(retained);
    let Fixture {
        directory: _directory,
        store,
        request: _,
        service: _,
    } = fixture;
    drop(store);
    assert!(
        weak.upgrade().is_some(),
        "same original connection must survive external Arc drops"
    );
    created.read_original_journal().unwrap();
    weak.upgrade().unwrap().shutdown().unwrap();
    assert!(
        created.read_original_journal().is_err(),
        "closed original connection must not be reopened"
    );
    drop(created);
    assert!(
        weak.upgrade().is_none(),
        "no global/path-based store substitute is retained"
    );
}

#[test]
fn original_ui_session_token_answers_only_its_creator_and_exact_recipient() {
    const CASE: &str = "CAGENTS_ORIGINAL_UI_STORE_RESPONSE_FIXTURE";
    if std::env::var_os(CASE).is_none() {
        let root=tempfile::tempdir().unwrap();
        let runtime=root.path().join("runtime");fs::create_dir(&runtime).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&runtime,fs::Permissions::from_mode(0o700)).unwrap();
        let name=concat!(module_path!(),"::original_ui_session_token_answers_only_its_creator_and_exact_recipient");
        let name=name.strip_prefix(concat!(env!("CARGO_CRATE_NAME"),"::")).unwrap_or(name);
        let spec=CommandSpec::new(std::env::current_exe().unwrap())
            .args(["--exact",name,"--nocapture"])
            .env(CASE,"1").env("XDG_DATA_HOME",root.path().join("data"))
            .env("XDG_CONFIG_HOME",root.path().join("config"))
            .env("XDG_RUNTIME_DIR",&runtime)
            .timeout(Duration::from_secs(60)).output_limits(65536,65536);
        let output=SystemProcessExecutor.execute(&spec,&CancellationToken::new()).unwrap();
        assert!(output.success,"{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),"{output:?}");
        return;
    }
    use crate::control::created_journal::UiJournalChallenge;
    use crate::model::{CardId,DirectoryStartKind,DirectoryStartRequirement,HostSessionProof,TerminalId};
    use std::os::unix::fs::MetadataExt;
    let (session,_)=crate::session::SessionRuntime::open_default().unwrap();
    let token=session.original_creation_journal_store().unwrap();
    let mut fixture=Fixture::new();
    fixture.store=session.operation_store();
    let scoped=fixture.service.clone().with_worktree_authority(
        WorktreeAuthority::capture(&fixture.request.repository).unwrap()).unwrap();
    let (sender,_)=async_channel::bounded(8);
    let retained=scoped.create_worktree_journaled_retained(fixture.store.clone(),&fixture.request,
        7,&CancellationToken::new(),&sender);
    let created=retained.created.as_ref().expect("actual original settled creator");
    let roots=received_original_roles(created);
    let roles=roots.each_ref().map(|root|{let m=root.metadata().unwrap();[m.dev(),m.ino()]});
    let requirement=DirectoryStartRequirement {card:CardId::new(),session:TerminalId::new(),
        operation:created.operation(),generation:created.generation(),attempt:uuid::Uuid::new_v4(),
        kind:DirectoryStartKind::Terminal};
    let proof=HostSessionProof {host_session_id:"fixture-host".into(),boot_id:"fixture-boot".into(),
        host_incarnation:"fixture-epoch".into()};
    // This tests UI production only; a plain challenge has NO host audience or grant authority.
    let challenge=UiJournalChallenge {request_id:8,nonce:OperationId::new(),proof:proof.clone(),
        requirement:requirement.clone(),window:crate::instance::window_slot().to_owned(),
        database:[0,0],roles};
    let deadline=Instant::now()+Duration::from_secs(5);
    let cancel=CancellationToken::new();
    let response=challenge.answer_original(created,&token,8,&proof,&requirement,deadline,&cancel).unwrap();
    let (echo,bytes)=response.into_wire();assert_eq!(echo,challenge);
    let saved=fixture.store.journaled_worktree_operation(created.operation()).unwrap().unwrap();
    assert_eq!(creation_journal::decode_creation_operation_description(&bytes).unwrap(),saved);
    let mut changed=requirement.clone();changed.card=CardId::new();
    assert!(challenge.answer_original(created,&token,8,&proof,&changed,deadline,&cancel).is_err());
    let mut changed=requirement.clone();changed.attempt=uuid::Uuid::new_v4();
    assert!(challenge.answer_original(created,&token,8,&proof,&changed,deadline,&cancel).is_err());
    let mut changed=challenge.clone();changed.roles[5][1]+=1;
    assert!(changed.answer_original(created,&token,8,&proof,&requirement,deadline,&cancel).is_err());
    let (foreign,_)=crate::session::SessionRuntime::open(fixture.directory.path().join("not-profile.sqlite"),
        BootId::new("foreign-ui-fixture")).unwrap();
    assert!(foreign.original_creation_journal_store().is_err(),"no token from caller-selected store path");
    cancel.cancel();
    assert!(challenge.answer_original(created,&token,8,&proof,&requirement,deadline,&cancel).is_err());
    assert_eq!(fixture.store.journaled_worktree_operation(created.operation()).unwrap().unwrap(),saved,
        "refusals never rewrite the original journal or begin a terminal");
}
