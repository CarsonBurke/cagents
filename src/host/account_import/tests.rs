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

fn registered_profile_fixture() -> (tempfile::TempDir, SharedHost, Installation, ImportPlan) {
    let (root, host, installation, mut plan) = fixture();
    plan.profile = CodingProfile::WorkspaceWrite {
        network_access: true,
    };
    prepare_at(&host, plan.clone(), &installation).unwrap();
    let record = installation.original(plan.operation).unwrap();
    let reservation = Reservation::for_import_at(
        &host,
        plan.operation,
        false,
        installation.store.clone(),
        &installation.homes,
    )
    .unwrap();
    // Arrange the already registered phase with actual private Store helpers.
    // This fixture does not run or certify the provider identity exchange.
    let (_, result) = installation
        .store
        .try_register_import_pending_reserved(
            &reservation,
            &installation.homes,
            plan.operation,
            record.receipt().unwrap().home(),
            &plan.expected_email,
            Registered {
                key: plan.operation,
                provider: Provider::Codex,
                linked_home: None,
                label: None,
                email: Some(plan.expected_email.clone()),
                organization: None,
                added_at: 1,
            },
            &[],
        )
        .unwrap();
    assert_eq!(result, Ok(()));
    drop(reservation);
    installation.journal.mark_registered(&plan).unwrap();
    (root, host, installation, plan)
}

fn profile_in_private_home(
    host: &SharedHost,
    status: ImportStatus,
    home: &Path,
    allow_create: bool,
) -> ImportStatus {
    use crate::account::coding_config::{self, CreateReport, Observation};
    let root = Root::open(home).unwrap();
    // Inject the selected-context observation, not a provider result. Exercise
    // the production import branch with actual NOFOLLOW presence and publisher.
    requested_profile_with(
        status,
        |request| {
            let metadata = root.metadata().unwrap();
            assert_eq!(request.expected_home.device, metadata.dev());
            assert_eq!(request.expected_home.inode, metadata.ino());
            Ok(Observation {
                account: request.account,
                host: proof(host),
                home: request.expected_home,
                expected_email: request.expected_email,
                presence: coding_config::presence(&root),
            })
        },
        |request| {
            assert!(
                allow_create,
                "existing entries must never enter the publisher"
            );
            request.validate().unwrap();
            let outcome = coding_config::prepare(&root, request.network_access)
                .unwrap()
                .publish(|| true);
            Ok(CreateReport {
                account: request.observation.account,
                host: request.observation.host,
                home: request.observation.home,
                network_access: request.network_access,
                outcome,
            })
        },
    )
}

#[test]
fn lost_profile_reply_retry_reports_preserved_unverified_without_repeating_publication() {
    use crate::account::coding_config::{Outcome, Refusal};
    let (_root, host, installation, plan) = registered_profile_fixture();
    let home = installation.homes.path(Provider::Codex, plan.operation);
    let registry = installation.store.load().unwrap();
    let auth = fs::read(home.join("auth.json")).unwrap();
    let initial = status_at(&host, &installation, plan.operation, false).unwrap();
    let original = initial.clone();
    let first = profile_in_private_home(&host, initial, &home, true);
    assert_eq!(
        first.coding_profile.as_ref().unwrap().outcome,
        Outcome::Created
    );
    let config = fs::read(home.join("config.toml")).unwrap();
    assert!(String::from_utf8_lossy(&config).contains("network_access = true"));
    let inode = fs::metadata(home.join("config.toml")).unwrap().ino();
    drop(first); // The client never received this publication result.
    let before_retry = status_at(&host, &installation, plan.operation, false).unwrap();
    assert_eq!(before_retry, original);
    assert!(
        list_at(&host, &installation).unwrap()[0]
            .coding_profile
            .is_none()
    );
    let repeated = profile_in_private_home(&host, before_retry, &home, false);
    let report = repeated.coding_profile.as_ref().unwrap();
    assert_eq!(
        report.outcome,
        Outcome::NotCreated(Refusal::ExistingConfiguration)
    );
    assert_eq!(report.account, original.registered.clone().unwrap());
    assert_eq!(report.host, original.observed_destination);
    assert_eq!(
        report.home.device,
        original.receipt.as_ref().unwrap().home().device()
    );
    assert_eq!(
        report.home.inode,
        original.receipt.as_ref().unwrap().home().inode()
    );
    assert!(
        report.network_access,
        "the report retains request intent only"
    );
    assert_eq!(repeated.plan, plan);
    assert_eq!(repeated.stage, ImportStage::Registered);
    assert_eq!(repeated.registered, original.registered);
    assert_eq!(fs::metadata(home.join("config.toml")).unwrap().ino(), inode);
    assert_eq!(fs::read(home.join("config.toml")).unwrap(), config);
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), auth);
    assert_eq!(installation.store.load().unwrap(), registry);
    assert_eq!(
        status_at(&host, &installation, plan.operation, false).unwrap(),
        original
    );
}

#[test]
fn existing_profile_types_and_changed_settings_stay_unverified_and_untouched() {
    use crate::account::coding_config::{Outcome, Refusal};
    use std::os::unix::fs::symlink;
    for kind in [
        "opposite-network",
        "changed",
        "directory",
        "symlink",
        "dangling",
        "hardlink",
    ] {
        let (_root, host, installation, plan) = registered_profile_fixture();
        let home = installation.homes.path(Provider::Codex, plan.operation);
        let target = home.join("config.toml");
        let retained = home.join("retained-config");
        fs::write(&retained, b"unrelated account settings").unwrap();
        match kind {
            "opposite-network" => {
                fs::write(&target, b"sandbox_mode = \"workspace-write\"\n[sandbox_workspace_write]\nnetwork_access = false\n").unwrap();
            }
            "changed" => {
                let root = Root::open(&home).unwrap();
                assert_eq!(
                    crate::account::coding_config::prepare(&root, true)
                        .unwrap()
                        .publish(|| true),
                    Outcome::Created
                );
                let replacement = home.join("replacement");
                fs::write(&replacement, b"changed after an earlier publication").unwrap();
                fs::rename(replacement, &target).unwrap();
            }
            "directory" => fs::create_dir(&target).unwrap(),
            "symlink" => symlink(&retained, &target).unwrap(),
            "dangling" => symlink(home.join("missing-config"), &target).unwrap(),
            "hardlink" => fs::hard_link(&retained, &target).unwrap(),
            _ => unreachable!(),
        }
        let metadata = fs::symlink_metadata(&target).unwrap();
        let file = metadata.is_file().then(|| fs::read(&target).unwrap());
        let link = metadata
            .file_type()
            .is_symlink()
            .then(|| fs::read_link(&target).unwrap());
        installation.journal.mark_uncertain(&plan).unwrap();
        let original = status_at(&host, &installation, plan.operation, false).unwrap();
        let current = profile_in_private_home(&host, original.clone(), &home, false);
        assert_eq!(
            current.coding_profile.as_ref().unwrap().outcome,
            Outcome::NotCreated(Refusal::ExistingConfiguration)
        );
        assert!(current.coding_profile.as_ref().unwrap().network_access);
        assert_eq!(current.plan, original.plan);
        assert!(current.uncertain);
        let after = fs::symlink_metadata(&target).unwrap();
        assert_eq!(
            (
                after.dev(),
                after.ino(),
                after.mode(),
                after.nlink(),
                after.len()
            ),
            (
                metadata.dev(),
                metadata.ino(),
                metadata.mode(),
                metadata.nlink(),
                metadata.len()
            )
        );
        if let Some(file) = file {
            assert_eq!(fs::read(&target).unwrap(), file);
        }
        if let Some(link) = link {
            assert_eq!(fs::read_link(&target).unwrap(), link);
        }
        assert_eq!(fs::read(&retained).unwrap(), b"unrelated account settings");
    }
    let (_root, host, installation, plan) = registered_profile_fixture();
    let original = status_at(&host, &installation, plan.operation, false).unwrap();
    let mut unconfigured = original.clone();
    unconfigured.plan.profile = CodingProfile::Unconfigured;
    assert!(
        requested_profile_with(
            unconfigured,
            |_| panic!("no implicit inspect"),
            |_| panic!("no create")
        )
        .coding_profile
        .is_none()
    );
    let mut unavailable = original.clone();
    unavailable.registered = None;
    assert!(
        requested_profile_with(
            unavailable,
            |_| panic!("no inspect without registration"),
            |_| panic!("no create")
        )
        .coding_profile
        .is_none()
    );
    let unknown = requested_profile_with(
        original.clone(),
        |request| {
            Ok(crate::account::coding_config::Observation {
                account: request.account,
                host: proof(&host),
                home: request.expected_home,
                expected_email: request.expected_email,
                presence: Presence::Unknown,
            })
        },
        |_| panic!("unknown presence never creates"),
    );
    assert_eq!(unknown, original);
}
