use super::*;
use crate::account::import::{CopyConsent, ImportStage};
use std::{fs, os::unix::fs::PermissionsExt};

const SYNTHETIC_SECRET: &str = "private-fixture-cache-not-for-output";

fn fixture() -> (tempfile::TempDir, SharedHost, Installation, ImportPlan) {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    fs::create_dir(&config).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
    let staged = root.path().join("staged.json");
    let payload = format!(
        "{{\"auth_mode\":\"chatgpt\",\"tokens\":{{\"refresh_token\":\"{SYNTHETIC_SECRET}\"}}}}\n"
    );
    fs::write(&staged, payload).unwrap();
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o600)).unwrap();
    let host = super::super::account_lifecycle::Reservation::test_host();
    let plan = ImportPlan {
        operation: Uuid::new_v4(),
        destination: proof(&host),
        observation: cache::inspect(&staged).unwrap(),
        expected_email: "fixture@example.test".into(),
        consent: CopyConsent {
            staged_file_only: true,
            source_session_limit_acknowledged: true,
        },
        profile: CodingProfile::Unconfigured,
    };
    let installation = Installation {
        store: Store::at(&config),
        homes: Homes::at(root.path().join("homes")),
        journal: Journal::at(config.join("account-imports")),
    };
    (root, host, installation, plan)
}

#[test]
fn prepare_publishes_only_a_new_pending_home_and_repeat_only_observes_original_evidence() {
    let (_root, host, installation, plan) = fixture();
    let other = Uuid::new_v4();
    let (_, other_home) = installation
        .homes
        .create_planned_with_config_initialized(
            Provider::Codex,
            other,
            None,
            crate::account::home::CodexConfig::Shared,
            |_| Ok(()),
        )
        .unwrap();
    fs::write(other_home.join("other-account-data"), b"preserve").unwrap();
    let other_account = Account::managed(Provider::Codex, other, other_home.clone());
    installation
        .store
        .update(|registry| {
            registry
                .register(
                    Registered {
                        key: other,
                        provider: Provider::Codex,
                        linked_home: None,
                        label: Some("existing".into()),
                        email: Some("other@example.test".into()),
                        organization: None,
                        added_at: 1,
                    },
                    &[],
                )
                .unwrap();
            registry.set_default(&other_account);
        })
        .unwrap();
    let original_bytes = fs::read(plan.observation.path()).unwrap();
    let ImportReply::Status(first) = prepare_at(&host, plan.clone(), &installation).unwrap() else {
        panic!("expected pending status");
    };
    assert_eq!(first.stage, ImportStage::Published);
    assert_eq!(first.publication, ImportPublication::Present);
    assert!(first.registered.is_none());
    assert!(first.coding_profile.is_none());
    let registry = installation.store.load().unwrap();
    assert!(registry.is_import_pending(plan.operation));
    assert!(
        registry.find(plan.operation).is_none(),
        "a copied cache is not registered or task-ready"
    );
    assert_eq!(
        registry.default_account(Provider::Codex, Some(&installation.homes)),
        other_account
    );
    assert_eq!(
        fs::read(other_home.join("other-account-data")).unwrap(),
        b"preserve"
    );
    let home = installation.homes.path(Provider::Codex, plan.operation);
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), original_bytes);
    assert_eq!(
        fs::read_dir(&home).unwrap().count(),
        1,
        "no configuration, instructions, history or System links are seeded"
    );
    let receipt = first.receipt.clone().unwrap();
    fs::write(
        plan.observation.path(),
        b"changed source after original publication",
    )
    .unwrap();
    let ImportReply::Status(repeated) = prepare_at(&host, plan.clone(), &installation).unwrap()
    else {
        panic!("expected original status");
    };
    assert_eq!(repeated.receipt.as_ref(), Some(&receipt));
    assert_eq!(repeated.plan, plan);
    assert_eq!(
        installation.store.load().unwrap(),
        registry,
        "a repeated Prepare cannot register, reseed or change defaults"
    );
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), original_bytes);
    let output = serde_json::to_string(&repeated).unwrap();
    assert!(!output.contains(SYNTHETIC_SECRET));
    assert!(!output.contains("refresh_token"));
}

#[test]
fn wrong_destination_and_existing_home_fail_before_new_intent_or_cache_publication() {
    let (_root, host, installation, plan) = fixture();
    let mut wrong = plan.clone();
    wrong.destination.host_incarnation = crate::model::HostIncarnation::new("other-daemon");
    assert!(prepare_at(&host, wrong, &installation).is_err());
    assert!(installation.journal.load().unwrap().is_empty());
    assert!(
        !installation
            .homes
            .path(Provider::Codex, plan.operation)
            .exists()
    );
    installation
        .homes
        .create_planned_with_config_initialized(
            Provider::Codex,
            plan.operation,
            None,
            crate::account::home::CodexConfig::Shared,
            |_| Ok(()),
        )
        .unwrap();
    let home = installation.homes.path(Provider::Codex, plan.operation);
    fs::write(home.join("retain"), b"original existing home").unwrap();
    assert!(prepare_at(&host, plan.clone(), &installation).is_err());
    assert!(installation.journal.load().unwrap().is_empty());
    assert!(!home.join("auth.json").exists());
    assert_eq!(
        fs::read(home.join("retain")).unwrap(),
        b"original existing home"
    );
    assert!(
        !installation
            .store
            .load()
            .unwrap()
            .is_import_pending(plan.operation)
    );
}

#[test]
fn metadata_status_creates_no_storage_and_does_not_promote_an_unprepared_intent() {
    let (_root, host, installation, plan) = fixture();
    assert!(status_at(&host, &installation, plan.operation, false).is_err());
    assert!(list_at(&host, &installation).unwrap().is_empty());
    let config = _root.path().join("config");
    assert_eq!(
        fs::read_dir(&config).unwrap().count(),
        0,
        "missing status creates no journal or Store lock"
    );
    installation.journal.begin(&plan).unwrap();
    let before: Vec<_> = fs::read_dir(&config)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let status = status_at(&host, &installation, plan.operation, false).unwrap();
    let retained = list_at(&host, &installation).unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].plan, plan);
    assert_eq!(retained[0], status);
    assert_eq!(status.stage, ImportStage::Reserved);
    assert_eq!(status.publication, ImportPublication::Unknown);
    assert!(
        status.receipt.is_none() && status.registered.is_none() && status.coding_profile.is_none()
    );
    assert!(
        !installation
            .homes
            .path(Provider::Codex, plan.operation)
            .exists()
    );
    assert_eq!(
        fs::read_dir(&config)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        before
    );
}
