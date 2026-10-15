// Included in the existing real creation fixture module, not a fabricated
// receipt builder. Each test performs actual journaled Git creation first.
fn received_original_roles(created: &SettledCreatedWorktree) -> [fs_safety::Root; 6] {
    use std::os::unix::fs::MetadataExt;
    let roots = created.transport_roots().unwrap();
    [roots.source.0, roots.source.1, roots.source.2,
     roots.destination.0, roots.destination.1, roots.destination.2].map(|root| {
        let metadata = root.metadata().unwrap();
        fs_safety::Root::from_received_creation_directory(root.retained_directory().unwrap().into(),
            root.path().to_owned(), (metadata.dev(), metadata.ino())).unwrap()
    })
}

#[test]
fn received_created_scopes_bind_actual_journal_and_original_six_roles() {
    if isolated_creation_fixture(concat!(module_path!(), "::received_created_scopes_bind_actual_journal_and_original_six_roles")) { return; }
    let fixture = Fixture::new();
    let scoped = fixture.service.clone().with_worktree_authority(
        WorktreeAuthority::capture(&fixture.request.repository).unwrap()).unwrap();
    let (sender, _) = async_channel::bounded(8);
    let retained = scoped.create_worktree_journaled_retained(&fixture.store, &fixture.request,
        7, &CancellationToken::new(), &sender);
    let created = retained.created.as_ref().expect("actual settled original carrier");
    let accepted = ReceivedCreationScopes::validate_received(&fixture.store, created.operation(),
        created.generation(), received_original_roles(created)).unwrap();
    assert_eq!(accepted.operation(), created.operation());
    assert_eq!(accepted.generation(), 7);
    assert_eq!(accepted.record(), created.record());
    assert_eq!(accepted.source().path(), fixture.request.repository);
    assert_eq!(accepted.destination().path(), fixture.request.destination);
    accepted.revalidate(&fixture.store).unwrap();
    let mut wrong_roles = received_original_roles(created);
    wrong_roles.swap(1,4); // actual source and destination admin are DIFFERENT
    assert!(ReceivedCreationScopes::validate_received(&fixture.store, created.operation(),
        7, wrong_roles).is_err());
    assert!(ReceivedCreationScopes::validate_received(&fixture.store, created.operation(),
        8, received_original_roles(created)).is_err());
    assert!(ReceivedCreationScopes::validate_received(&fixture.store, OperationId::new(),
        7, received_original_roles(created)).is_err());
    fixture.store.release_worktree_creation(created.operation()).unwrap();
    assert!(accepted.revalidate(&fixture.store).is_err(), "operation retirement cannot leave grant admission valid");
}

#[test]
fn received_created_scopes_refuse_name_retarget_without_reading_replacement() {
    if isolated_creation_fixture(concat!(module_path!(), "::received_created_scopes_refuse_name_retarget_without_reading_replacement")) { return; }
    for replaced in ["source", "destination", "admin"] {
        let fixture=Fixture::new();
        let scoped=fixture.service.clone().with_worktree_authority(
            WorktreeAuthority::capture(&fixture.request.repository).unwrap()).unwrap();
        let (sender, _)=async_channel::bounded(8);
        let retained=scoped.create_worktree_journaled_retained(&fixture.store, &fixture.request,
            7, &CancellationToken::new(), &sender);
        let created=retained.created.as_ref().expect("actual settled original carrier");
        let roots=received_original_roles(created); // retain BEFORE late replacement
        let index=match replaced { "source"=>0, "destination"=>3, _=>4 };
        let name=roots[index].path().to_owned();
        let parked=fixture.directory.path().join("parked-original");
        fs::rename(&name, &parked).unwrap();
        fs::create_dir(&name).unwrap();
        let marker=name.join("foreign");fs::write(&marker,b"untouched replacement").unwrap();
        assert!(ReceivedCreationScopes::validate_received(&fixture.store,created.operation(),7,roots).is_err());
        assert_eq!(fs::read(marker).unwrap(),b"untouched replacement");
        assert_eq!(fs::read_dir(&name).unwrap().count(),1,"no constructor/history/process effects at receiver");
    }
}

#[test]
fn received_creation_single_row_read_refuses_oversized_payload() {
    let fixture=Fixture::new();
    let id=OperationId::new();
    fixture.store.begin_operation(&PendingOperation {
        id, entity_kind:"worktree_creation".into(), entity_id:"bounded-receiver".into(),
        action:"create".into(), base_generation:7,
        payload:serde_json::Value::String("x".repeat(512*1024+1)),
        status:PendingOperationStatus::Pending,
        created_at_unix_ms:operation_timestamp(), updated_at_unix_ms:operation_timestamp(),
    }).unwrap();
    assert!(fixture.store.journaled_worktree_operation(id).is_err());
    assert!(fixture.store.journaled_worktree_operation(OperationId::new()).unwrap().is_none());
}
