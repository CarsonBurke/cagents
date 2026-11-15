use super::*;

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn home() -> Home {
    Home {
        registry: uuid(1),
        channel: ChannelId::from_uuid(uuid(2)),
        creation_ordinal: 3,
    }
}

fn header() -> HomeState {
    HomeState {
        home: home(),
        lifecycle: HomeLifecycle::Active,
        revision: 1000,
    }
}

fn pr(number: u64) -> Target {
    Target::PullRequest(
        PullRequestKey::new(
            GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
            number,
        )
        .unwrap(),
    )
}

fn actor(value: u128) -> Origin {
    Origin::Actor {
        home: home(),
        actor: Contributor {
            session: TerminalId::from_uuid(uuid(value)),
            lineage: Lineage(TerminalId::from_uuid(uuid(value))),
            agent: None,
            run: None,
        },
        contexts: Vec::new(),
    }
}

fn mutation(entry: Option<&Entry>, change: Change, operation: u128) -> Mutation {
    Mutation {
        home: home(),
        expected_revision: entry.map_or(0, |entry| entry.revision),
        operation: uuid(operation),
        change,
    }
}

fn claim(
    entry: Option<&Entry>,
    target: Target,
    role: Role,
    origin: &Origin,
    operation: u128,
) -> Entry {
    let request = mutation(entry, Change::Claim { target, role }, operation);
    reduce_entry(&header(), entry, &request, origin)
        .unwrap()
        .entry
}

fn observe(entry: &Entry, generation: u64, state: PrState) -> Entry {
    observe_entry(
        &header(),
        entry,
        generation,
        PollOutcome::Success(Observation {
            generation,
            state,
            freshness: Freshness::Current,
            title: Some("Work".into()),
            node_id: Some("PR_original".into()),
            head: None,
            observed_at_ms: generation,
        }),
    )
    .unwrap()
    .entry
}

fn snapshot(entry: Entry, revision: u64) -> Snapshot {
    let historical = entry.bucket() == Bucket::History;
    Snapshot {
        home: home(),
        lifecycle: HomeLifecycle::Active,
        revision,
        entries: if historical {
            Vec::new()
        } else {
            vec![entry.clone()]
        },
        history: if historical { vec![entry] } else { Vec::new() },
        next_current: None,
        next_history: None,
    }
}

#[test]
fn repository_case_folding_preserves_fork_host_and_port_identity_and_safe_urls() {
    let make = |host: &str, port, owner: &str, name: &str| {
        Target::PullRequest(
            PullRequestKey::new(GitHubRepoKey::new(host, port, owner, name).unwrap(), 7).unwrap(),
        )
    };
    let a = make("GitHub.COM", None, "OWNER", "REPO");
    assert_eq!(a.canonicalized().unwrap(), pr(7));
    assert_ne!(
        a.canonicalized().unwrap(),
        make("github.com", None, "fork", "repo")
            .canonicalized()
            .unwrap()
    );
    assert_ne!(
        a.canonicalized().unwrap(),
        make("enterprise.example", None, "owner", "repo")
            .canonicalized()
            .unwrap()
    );
    assert_ne!(
        a.canonicalized().unwrap(),
        make("github.com", Some(8443), "owner", "repo")
            .canonicalized()
            .unwrap()
    );
    let unusual = make("enterprise.example", Some(8443), "Team?name", "Repo#name");
    let url = unusual.pull_request_url().unwrap().unwrap();
    assert_eq!(
        url.as_str(),
        "https://enterprise.example:8443/team%3Fname/repo%23name/pull/7"
    );
    assert!(url.query().is_none() && url.fragment().is_none());
}

#[test]
fn native_and_managed_contributors_share_a_target_without_ending_each_other() {
    let a = actor(10);
    let b = actor(11);
    let first = claim(None, pr(1), Role::Contribute, &a, 100);
    let second = claim(Some(&first), pr(1), Role::Steward, &b, 101);
    assert_eq!(second.claims.len(), 2);
    assert!(second.claims.iter().all(
        |claim| matches!(&claim.source, Source::Session(c) if c.agent.is_none() && c.run.is_none())
    ));
    let request = mutation(Some(&second), Change::End { target: pr(1) }, 102);
    let ended = reduce_entry(&header(), Some(&second), &request, &a)
        .unwrap()
        .entry;
    assert!(!ended.claims[0].active && ended.claims[1].active);
    assert_eq!(ended.bucket(), Bucket::Ongoing);
    let mut managed = b;
    if let Origin::Actor { actor, .. } = &mut managed {
        actor.agent = Some(AgentId::from_uuid(uuid(12)));
        actor.run = Some(RunId::from_uuid(uuid(13)));
    }
    let resumed = claim(Some(&ended), pr(1), Role::Review, &managed, 103);
    assert_eq!(
        resumed.claims.len(),
        2,
        "one claim per proved lineage, not per new run"
    );
    assert!(!Role::Review.establishes_work_intent());
    assert!(!Role::Reference.establishes_work_intent());
    let other = claim(None, pr(2), Role::Reference, &Origin::User, 104);
    assert_ne!(
        resumed.target, other.target,
        "a channel can hold several independent PRs"
    );
}

#[test]
fn work_intent_is_order_independent_and_manual_review_never_claims_implementation() {
    for roles in [
        [Role::Review, Role::Contribute],
        [Role::Contribute, Role::Review],
    ] {
        let first = claim(None, pr(1), roles[0], &actor(10), 100);
        let both = claim(Some(&first), pr(1), roles[1], &actor(11), 101);
        assert_eq!(both.role(), Some(Role::Contribute));
        assert!(both.is_ongoing_work() && both.is_ongoing());
        let overridden = claim(Some(&both), pr(1), Role::Review, &Origin::User, 102);
        assert_eq!(
            overridden.bucket(),
            Bucket::Ongoing,
            "review remains visible"
        );
        assert_eq!(overridden.role(), Some(Role::Review));
        assert!(!overridden.is_ongoing_work() && !overridden.is_ongoing());
    }
    for role in [Role::Review, Role::Reference] {
        let relation = claim(None, pr(2), role, &actor(10), 103);
        assert_eq!(relation.bucket(), Bucket::Ongoing);
        assert!(!relation.is_ongoing_work());
    }
}

#[test]
fn row_cas_does_not_conflict_with_unrelated_ledger_updates() {
    let entry = claim(None, pr(1), Role::Contribute, &Origin::User, 100);
    assert_eq!(entry.revision, 1);
    let request = mutation(
        Some(&entry),
        Change::Claim {
            target: pr(1),
            role: Role::Steward,
        },
        101,
    );
    let changed = reduce_entry(&header(), Some(&entry), &request, &Origin::User).unwrap();
    assert_eq!(
        changed.entry.revision, 2,
        "ledger1000 is not the row revision"
    );
    assert_eq!(
        reduce_entry(&header(), Some(&changed.entry), &request, &Origin::User),
        Err(Error::StaleRevision)
    );
}

#[test]
fn registry_channel_incarnation_and_caller_scope_refuse_aba_and_cross_device_context() {
    let request = mutation(
        None,
        Change::Claim {
            target: pr(1),
            role: Role::Steward,
        },
        100,
    );
    for replacement in [
        Home {
            registry: uuid(4),
            ..home()
        },
        Home {
            creation_ordinal: 4,
            ..home()
        },
    ] {
        assert_eq!(
            reduce_entry(
                &HomeState {
                    home: replacement,
                    ..header()
                },
                None,
                &request,
                &Origin::User
            ),
            Err(Error::WrongHome)
        );
    }
    assert_eq!(
        Home {
            creation_ordinal: crate::workspace_model::PENDING_ORDINAL,
            ..home()
        }
        .validate(),
        Err(Error::InvalidHome)
    );
    let mut source = actor(10);
    if let Origin::Actor { home, .. } = &mut source {
        home.creation_ordinal += 1;
    }
    assert_eq!(
        reduce_entry(&header(), None, &request, &source),
        Err(Error::WrongHome)
    );
    let mut source = actor(10);
    if let Origin::Actor { contexts, .. } = &mut source {
        contexts.push(context(20, "branch-a", uuid(99)));
    }
    assert_eq!(
        reduce_entry(&header(), None, &request, &source),
        Err(Error::WrongHome)
    );
}

#[test]
fn user_end_suppression_and_role_override_dominate_late_actor_claims() {
    let source = actor(10);
    let first = claim(None, pr(1), Role::Contribute, &source, 100);
    let request = mutation(Some(&first), Change::Suppress { target: pr(1) }, 101);
    assert_eq!(
        reduce_entry(&header(), Some(&first), &request, &source),
        Err(Error::UserDecision)
    );
    let suppressed = reduce_entry(&header(), Some(&first), &request, &Origin::User)
        .unwrap()
        .entry;
    let request = mutation(
        Some(&suppressed),
        Change::Claim {
            target: pr(1),
            role: Role::Steward,
        },
        102,
    );
    assert_eq!(
        reduce_entry(&header(), Some(&suppressed), &request, &source),
        Err(Error::UserDecision)
    );
    let reopened = claim(Some(&suppressed), pr(1), Role::Review, &Origin::User, 103);
    let active = claim(Some(&reopened), pr(1), Role::Contribute, &source, 104);
    assert_eq!(
        active.role(),
        Some(Role::Review),
        "manual review is not silently escalated by an actor"
    );
    let request = mutation(Some(&active), Change::End { target: pr(1) }, 105);
    let ended = reduce_entry(&header(), Some(&active), &request, &Origin::User)
        .unwrap()
        .entry;
    assert!(ended.user_ended && ended.claims.iter().all(|claim| !claim.active));
    let request = mutation(
        Some(&ended),
        Change::Claim {
            target: pr(1),
            role: Role::Contribute,
        },
        106,
    );
    assert_eq!(
        reduce_entry(&header(), Some(&ended), &request, &source),
        Err(Error::UserDecision)
    );
}

#[test]
fn terminal_reopen_failed_poll_and_stale_results_never_invent_fresh_work_intent() {
    let source = actor(10);
    let first = claim(None, pr(1), Role::Steward, &source, 100);
    let open = observe(&first, 1, PrState::Open { draft: true });
    let merged = observe(&open, 2, PrState::Merged);
    assert_eq!(merged.bucket(), Bucket::History);
    let failed = observe_entry(&header(), &merged, 3, PollOutcome::Failure)
        .unwrap()
        .entry;
    assert_eq!(failed.observation.as_ref().unwrap().state, PrState::Merged);
    assert_eq!(failed.bucket(), Bucket::History, "failure is not a reopen");
    let reopened = observe(&failed, 4, PrState::Open { draft: false });
    assert_eq!(reopened.bucket(), Bucket::NeedsConfirmation);
    assert!(
        !observe_entry(&header(), &reopened, 2, PollOutcome::Failure)
            .unwrap()
            .changed
    );
    let confirmed = claim(Some(&reopened), pr(1), Role::Steward, &source, 101);
    assert_eq!(confirmed.bucket(), Bucket::Ongoing);
    let request = mutation(Some(&confirmed), Change::End { target: pr(1) }, 102);
    let ended = reduce_entry(&header(), Some(&confirmed), &request, &Origin::User)
        .unwrap()
        .entry;
    let reopened = observe(
        &observe(&ended, 5, PrState::Closed),
        6,
        PrState::Open { draft: false },
    );
    assert_eq!(
        reopened.bucket(),
        Bucket::History,
        "status cannot override a user end"
    );
}

fn context(id: u128, branch: &str, registry: Uuid) -> CheckoutContext {
    CheckoutContext {
        id: uuid(id),
        registry,
        repository: "/repo/.git".into(),
        path: "/repo/tree".into(),
        branch: Some(branch.into()),
        head: Some("a".repeat(40)),
        generation: 1,
        dirty: true,
    }
}

#[test]
fn fresh_review_after_reopen_does_not_resume_another_actors_old_work_claim() {
    let original = claim(None, pr(1), Role::Contribute, &actor(10), 100);
    let reopened = observe(
        &observe(&original, 1, PrState::Merged),
        2,
        PrState::Open { draft: false },
    );
    let review = claim(Some(&reopened), pr(1), Role::Review, &actor(11), 101);
    assert_eq!(
        review.bucket(),
        Bucket::Ongoing,
        "the new review is visible"
    );
    assert!(
        !review.is_ongoing_work(),
        "old implementation intent still needs confirmation"
    );
    let resumed = claim(Some(&review), pr(1), Role::Contribute, &actor(10), 102);
    assert!(resumed.is_ongoing_work());
    let manual = claim(Some(&reopened), pr(1), Role::Contribute, &Origin::User, 103);
    let terminal = observe(
        &observe(&manual, 3, PrState::Closed),
        4,
        PrState::Open { draft: false },
    );
    let other = claim(Some(&terminal), pr(1), Role::Contribute, &actor(11), 104);
    assert!(
        !other.is_ongoing_work(),
        "a stale user declaration still needs an explicit user resume"
    );
}

#[test]
fn checkout_switch_preserves_original_context_and_does_not_retarget_the_pr() {
    let mut source = actor(10);
    if let Origin::Actor { contexts, .. } = &mut source {
        contexts.push(context(20, "branch-a", home().registry));
    }
    let first = claim(None, pr(1), Role::Contribute, &source, 100);
    if let Origin::Actor { contexts, .. } = &mut source {
        contexts[0].branch = Some("branch-b".into());
    }
    let request = mutation(
        Some(&first),
        Change::Claim {
            target: pr(1),
            role: Role::Contribute,
        },
        101,
    );
    assert_eq!(
        reduce_entry(&header(), Some(&first), &request, &source),
        Err(Error::ContextChanged)
    );
    if let Origin::Actor { contexts, .. } = &mut source {
        contexts[0].id = uuid(21);
    }
    let continued = claim(Some(&first), pr(1), Role::Steward, &source, 102);
    assert_eq!(continued.contexts.len(), 2);
    assert_eq!(continued.contexts[0].branch.as_deref(), Some("branch-a"));
    assert_eq!(continued.target, pr(1));
    assert_eq!(
        first.contexts.len(),
        1,
        "failed policy does not mutate its input"
    );
}

#[test]
fn local_unpublished_work_is_independent_of_terminal_pr_and_never_a_forge_guess() {
    let mut source = actor(10);
    if let Origin::Actor { contexts, .. } = &mut source {
        contexts.push(context(20, "unpublished", home().registry));
    }
    let local = claim(
        None,
        Target::Local(uuid(20)),
        Role::Contribute,
        &source,
        100,
    );
    let linked = claim(None, pr(1), Role::Steward, &source, 101);
    let merged = observe(&linked, 1, PrState::Merged);
    assert_eq!(local.bucket(), Bucket::Ongoing);
    assert_eq!(merged.bucket(), Bucket::History);
    assert_eq!(
        observe_entry(&header(), &local, 2, PollOutcome::Failure),
        Err(Error::InvalidTarget)
    );
    let mut candidate = linked;
    candidate.claims.clear();
    candidate.ever_claimed = false;
    assert_eq!(
        observe(&candidate, 3, PrState::Open { draft: false }).bucket(),
        Bucket::NeedsConfirmation
    );
    assert!(candidate.contexts[0].dirty);
    assert_eq!(
        candidate.role(),
        None,
        "dirty/head/forge facts do not declare ownership"
    );
}

#[test]
fn exact_replay_returns_original_receipt_and_current_ended_snapshot() {
    let request = mutation(
        None,
        Change::Claim {
            target: pr(1),
            role: Role::Steward,
        },
        100,
    );
    let entry = reduce_entry(&header(), None, &request, &Origin::User)
        .unwrap()
        .entry;
    let receipt = Receipt {
        operation: request.operation,
        home: home(),
        revision: 1,
        sequence: 7,
        changed: true,
    };
    let stored = StoredOperation::capture(&request, &Origin::User, receipt.clone()).unwrap();
    let end = mutation(Some(&entry), Change::End { target: pr(1) }, 101);
    let ended = reduce_entry(&header(), Some(&entry), &end, &Origin::User)
        .unwrap()
        .entry;
    let current = snapshot(ended.clone(), 20);
    let reply = replay_response(&stored, &request, &Origin::User, current.clone()).unwrap();
    assert_eq!(reply.receipt, receipt);
    assert_eq!(reply.snapshot.revision, 20);
    assert_eq!(reply.snapshot.history, [ended]);
    let changed = Mutation {
        change: Change::Claim {
            target: pr(2),
            role: Role::Steward,
        },
        ..request.clone()
    };
    assert_eq!(
        replay_response(&stored, &changed, &Origin::User, current.clone()),
        Err(Error::OperationConflict)
    );
    assert_eq!(
        replay_response(&stored, &request, &actor(10), current),
        Err(Error::OperationConflict)
    );
}

#[test]
fn observational_context_changes_preserve_original_provenance_on_exact_replay() {
    let mut source = actor(10);
    if let Origin::Actor { contexts, .. } = &mut source {
        contexts.push(context(20, "a", home().registry));
    }
    let request = mutation(
        None,
        Change::Claim {
            target: pr(1),
            role: Role::Steward,
        },
        100,
    );
    let entry = reduce_entry(&header(), None, &request, &source)
        .unwrap()
        .entry;
    let stored = StoredOperation::capture(
        &request,
        &source,
        Receipt {
            operation: request.operation,
            home: home(),
            revision: 1,
            sequence: 7,
            changed: true,
        },
    )
    .unwrap();
    let original_contexts = stored.contexts.clone();
    if let Origin::Actor { contexts, .. } = &mut source {
        contexts[0].generation += 1;
        contexts[0].head = Some("b".repeat(40));
        contexts[0].dirty = false;
    }
    let reply = replay_response(&stored, &request, &source, snapshot(entry, 10)).unwrap();
    assert_eq!(reply.receipt, stored.receipt);
    assert_eq!(reply.snapshot.revision, 10);
    assert_eq!(
        stored.contexts, original_contexts,
        "retry never rewrites original provenance"
    );
}

#[test]
fn new_local_target_requires_a_server_captured_context_in_this_home() {
    let request = mutation(
        None,
        Change::Claim {
            target: Target::Local(uuid(20)),
            role: Role::Contribute,
        },
        100,
    );
    assert_eq!(
        reduce_entry(&header(), None, &request, &Origin::User),
        Err(Error::InvalidTarget)
    );
    let mut source = actor(10);
    assert_eq!(
        reduce_entry(&header(), None, &request, &source),
        Err(Error::InvalidTarget)
    );
    if let Origin::Actor { contexts, .. } = &mut source {
        contexts.push(context(21, "other", home().registry));
    }
    assert_eq!(
        reduce_entry(&header(), None, &request, &source),
        Err(Error::InvalidTarget)
    );
    if let Origin::Actor { contexts, .. } = &mut source {
        contexts.push(context(20, "unpublished", home().registry));
    }
    let created = reduce_entry(&header(), None, &request, &source)
        .unwrap()
        .entry;
    assert_eq!(created.target, Target::Local(uuid(20)));
    let end = mutation(
        Some(&created),
        Change::End {
            target: created.target.clone(),
        },
        101,
    );
    assert_eq!(
        reduce_entry(&header(), Some(&created), &end, &Origin::User)
            .unwrap()
            .entry
            .bucket(),
        Bucket::History
    );
}

#[test]
fn snapshot_partition_and_wire_grammar_reject_authority_or_status_fabrication() {
    let entry = claim(None, pr(1), Role::Steward, &Origin::User, 100);
    let mut page = snapshot(entry.clone(), 2);
    page.validate().unwrap();
    page.history.push(entry);
    assert_eq!(page.validate(), Err(Error::InvalidSnapshot));
    assert_eq!(
        Page {
            current_before: None,
            before: None,
            limit: 101
        }
        .validate(),
        Err(Error::Capacity)
    );
    let request = mutation(
        None,
        Change::Claim {
            target: pr(1),
            role: Role::Steward,
        },
        100,
    );
    let mut value = serde_json::to_value(&request).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("actor".into(), serde_json::json!({"User":null}));
    assert!(serde_json::from_value::<Mutation>(value).is_err());
    assert!(
        serde_json::from_value::<Change>(serde_json::json!({"Observe":{"state":"Merged"}}))
            .is_err()
    );
    let encoded = serde_json::to_vec(&request).unwrap();
    assert_eq!(
        serde_json::from_slice::<Mutation>(&encoded).unwrap(),
        request
    );
}

#[test]
fn changed_remote_node_is_refused_and_claim_capacity_never_silently_evicts_history() {
    let entry = observe(
        &claim(None, pr(1), Role::Steward, &Origin::User, 100),
        1,
        PrState::Open { draft: false },
    );
    let mut replacement = entry.observation.clone().unwrap();
    replacement.generation = 2;
    replacement.node_id = Some("PR_replacement".into());
    assert_eq!(
        observe_entry(&header(), &entry, 2, PollOutcome::Success(replacement)),
        Err(Error::ContextChanged)
    );
    let mut incomplete = entry.observation.clone().unwrap();
    incomplete.generation = 2;
    incomplete.node_id = None;
    let retained = observe_entry(&header(), &entry, 2, PollOutcome::Success(incomplete))
        .unwrap()
        .entry;
    assert_eq!(
        retained.observation.as_ref().unwrap().node_id.as_deref(),
        Some("PR_original")
    );
    let mut replacement = retained.observation.clone().unwrap();
    replacement.generation = 3;
    replacement.node_id = Some("PR_replacement".into());
    assert_eq!(
        observe_entry(&header(), &retained, 3, PollOutcome::Success(replacement)),
        Err(Error::ContextChanged)
    );
    let mut many = None;
    for i in 0..MAX_CLAIMS {
        many = Some(claim(
            many.as_ref(),
            pr(2),
            Role::Review,
            &actor(1000 + i as u128),
            2000 + i as u128,
        ));
    }
    let many = many.unwrap();
    let request = mutation(
        Some(&many),
        Change::Claim {
            target: pr(2),
            role: Role::Review,
        },
        9999,
    );
    assert_eq!(
        reduce_entry(&header(), Some(&many), &request, &actor(9999)),
        Err(Error::Capacity)
    );
    assert_eq!(many.claims.len(), MAX_CLAIMS);
}

#[test]
fn archived_and_deleted_homes_refuse_mutations_and_observations_without_changing_inputs() {
    let original = claim(None, pr(1), Role::Steward, &Origin::User, 100);
    let request = mutation(Some(&original), Change::End { target: pr(1) }, 101);
    for lifecycle in [HomeLifecycle::Archived, HomeLifecycle::Deleted] {
        let state = HomeState {
            lifecycle,
            ..header()
        };
        assert_eq!(
            reduce_entry(&state, Some(&original), &request, &Origin::User),
            Err(Error::InactiveHome)
        );
        assert_eq!(
            observe_entry(&state, &original, 1, PollOutcome::Failure),
            Err(Error::InactiveHome)
        );
        assert!(original.is_ongoing_work());
        let mut inactive = snapshot(original.clone(), 2);
        inactive.lifecycle = lifecycle;
        assert_eq!(inactive.validate(), Err(Error::InvalidSnapshot));
        let ended = reduce_entry(&header(), Some(&original), &request, &Origin::User)
            .unwrap()
            .entry;
        let mut historical = snapshot(ended, 2);
        historical.lifecycle = lifecycle;
        historical.validate().unwrap();
        let initial = mutation(
            None,
            Change::Claim {
                target: pr(2),
                role: Role::Steward,
            },
            102,
        );
        assert_eq!(
            reduce_entry(&state, None, &initial, &actor(10)),
            Err(Error::InactiveHome)
        );
    }
}

#[test]
fn repeated_semantic_claim_is_noop_and_entry_revision_overflow_refuses_change() {
    let original = claim(None, pr(1), Role::Steward, &Origin::User, 100);
    let request = mutation(
        Some(&original),
        Change::Claim {
            target: pr(1),
            role: Role::Steward,
        },
        101,
    );
    let repeated = reduce_entry(&header(), Some(&original), &request, &Origin::User).unwrap();
    assert!(!repeated.changed);
    assert_eq!(repeated.entry, original);
    let stored = StoredOperation::capture(
        &request,
        &Origin::User,
        Receipt {
            operation: request.operation,
            home: home(),
            revision: original.revision,
            sequence: 7,
            changed: false,
        },
    )
    .unwrap();
    assert!(
        !replay_response(
            &stored,
            &request,
            &Origin::User,
            snapshot(original.clone(), 2)
        )
        .unwrap()
        .receipt
        .changed
    );
    let exhausted = Entry {
        revision: u64::MAX,
        ..original
    };
    let state = HomeState {
        revision: u64::MAX,
        ..header()
    };
    let end = mutation(Some(&exhausted), Change::End { target: pr(1) }, 102);
    assert_eq!(
        reduce_entry(&state, Some(&exhausted), &end, &Origin::User),
        Err(Error::RevisionExhausted)
    );
    assert!(
        exhausted.claims[0].active,
        "refusal never mutates the stored input"
    );
}

#[test]
fn malformed_metadata_and_duplicate_row_identity_are_refused_at_snapshot_boundary() {
    let original = claim(None, pr(1), Role::Steward, &Origin::User, 100);
    let mut other = claim(None, pr(2), Role::Review, &actor(10), 101);
    other.id = original.id;
    let mut value = snapshot(original.clone(), 2);
    value.entries.push(other);
    assert_eq!(value.validate(), Err(Error::InvalidSnapshot));
    let mut malformed = observe(&original, 1, PrState::Open { draft: false });
    malformed.observation.as_mut().unwrap().node_id = Some("bad\nnode".into());
    assert_eq!(
        snapshot(malformed, 2).validate(),
        Err(Error::InvalidMetadata)
    );
    let mut context = context(20, "work", home().registry);
    context.path = "/repo/../other".into();
    assert_eq!(context.validate(), Err(Error::InvalidMetadata));
    context.path = "/repo/\0other".into();
    assert_eq!(context.validate(), Err(Error::InvalidMetadata));
}

// Maximal bounded fields exercise the actual serializer, not an estimated row size.
fn maximum_entry(number: u64) -> Entry {
    let repository = GitHubRepoKey::new(
        "h".repeat(253),
        Some(u16::MAX),
        "o".repeat(256),
        "r".repeat(256),
    )
    .unwrap();
    let target = Target::PullRequest(PullRequestKey::new(repository.clone(), number).unwrap());
    let mut entry = claim(
        None,
        target,
        Role::Contribute,
        &Origin::User,
        8000 + u128::from(number),
    );
    entry.revision = u64::MAX;
    entry.claims = (0..MAX_CLAIMS)
        .map(|index| Claim {
            source: Source::Session(Contributor {
                session: TerminalId::from_uuid(uuid(10000 + index as u128)),
                lineage: Lineage(TerminalId::from_uuid(uuid(20000 + index as u128))),
                agent: Some(AgentId::from_uuid(uuid(30000 + index as u128))),
                run: Some(RunId::from_uuid(uuid(40000 + index as u128))),
            }),
            role: Role::Contribute,
            active: true,
            confirmed_generation: u64::MAX,
        })
        .collect();
    let path = PathBuf::from(format!(
        "/{}x",
        "é".repeat((MAX_CONTEXT_PATH_BYTES - 2) / 2)
    ));
    assert_eq!(
        path.as_os_str().as_encoded_bytes().len(),
        MAX_CONTEXT_PATH_BYTES
    );
    entry.contexts = (0..MAX_CONTEXTS)
        .map(|index| CheckoutContext {
            id: uuid(50000 + index as u128),
            registry: home().registry,
            repository: path.clone(),
            path: path.clone(),
            branch: Some("b".repeat(1024)),
            head: Some("f".repeat(64)),
            generation: u64::MAX,
            dirty: true,
        })
        .collect();
    entry.observation = Some(Observation {
        generation: u64::MAX,
        state: PrState::Open { draft: true },
        freshness: Freshness::Current,
        title: Some("t".repeat(512)),
        node_id: Some("n".repeat(256)),
        head: Some(Head {
            repository,
            branch: "b".repeat(1024),
            oid: "f".repeat(64),
        }),
        observed_at_ms: u64::MAX,
    });
    entry
}

#[test]
fn maximum_valid_entry_fits_a_wire_bucket_and_encoded_path_limit_is_exact() {
    let entry = maximum_entry(u64::MAX);
    entry.validate().unwrap();
    let bytes = postcard::to_stdvec(&entry).unwrap();
    assert_eq!(encoded_size(&entry).unwrap(), bytes.len());
    assert!(bytes.len() < MAX_BUCKET_BYTES);
    let mut too_long = entry.contexts[0].clone();
    too_long.path.push("x");
    assert_eq!(too_long.validate(), Err(Error::InvalidMetadata));
    let mut row = entry.clone();
    row.contexts.push(too_long);
    assert_eq!(row.validate(), Err(Error::Capacity));
    let point = TargetSnapshot {
        home: home(),
        lifecycle: HomeLifecycle::Active,
        revision: u64::MAX,
        target: entry.target.clone(),
        entry: Some(entry),
    };
    point.validate().unwrap();
    assert!(encoded_size(&point).unwrap() < 2 * 1024 * 1024);
}

#[test]
fn current_and_history_pages_enforce_independent_count_and_byte_budgets() {
    let first = maximum_entry(1);
    let mut second = maximum_entry(2);
    second.id = uuid(90000);
    let mut page = snapshot(first.clone(), u64::MAX);
    page.entries.push(second.clone());
    assert!(page.entries.len() < usize::from(MAX_PAGE));
    assert!(encoded_size(&first).unwrap() + encoded_size(&second).unwrap() > MAX_BUCKET_BYTES);
    assert_eq!(
        page.validate(),
        Err(Error::Capacity),
        "current bytes, not row count"
    );
    second.user_ended = true;
    page.entries.pop();
    page.history.push(second.clone());
    page.next_current = Some(40);
    page.next_history = Some(20);
    page.validate().unwrap();
    assert!(encoded_size(&page).unwrap() <= MAX_SNAPSHOT_BYTES);
    let mut third = first.clone();
    third.id = uuid(90001);
    third.user_ended = true;
    page.history.push(third);
    assert_eq!(
        page.validate(),
        Err(Error::Capacity),
        "history has its own byte budget"
    );

    let mut small = snapshot(
        claim(None, pr(10), Role::Review, &Origin::User, 91000),
        1000,
    );
    small.entries = (0..=MAX_PAGE)
        .map(|number| {
            let mut entry = claim(
                None,
                pr(100 + u64::from(number)),
                Role::Review,
                &Origin::User,
                92000 + u128::from(number),
            );
            entry.id = uuid(93000 + u128::from(number));
            entry
        })
        .collect();
    assert_eq!(small.validate(), Err(Error::Capacity));
    for entry in &mut small.entries {
        entry.user_ended = true;
    }
    small.history = std::mem::take(&mut small.entries);
    assert_eq!(small.validate(), Err(Error::Capacity));
}

#[test]
fn independent_page_cursors_and_point_target_echo_cannot_hide_invalid_scope() {
    let mut page = Page {
        current_before: Some(30),
        before: Some(20),
        limit: MAX_PAGE,
    };
    page.validate().unwrap();
    page.current_before = Some(0);
    assert!(page.validate().is_err());
    page.current_before = None;
    page.before = Some(0);
    assert!(page.validate().is_err());
    let entry = claim(None, pr(1), Role::Steward, &Origin::User, 95000);
    let mut point = TargetSnapshot {
        home: home(),
        lifecycle: HomeLifecycle::Active,
        revision: entry.revision,
        target: pr(1),
        entry: Some(entry.clone()),
    };
    point.validate().unwrap();
    point.target = pr(2);
    assert_eq!(point.validate(), Err(Error::InvalidSnapshot));
    point.target = pr(1);
    point
        .entry
        .as_mut()
        .unwrap()
        .contexts
        .push(context(96000, "branch", uuid(99)));
    assert_eq!(point.validate(), Err(Error::InvalidSnapshot));
    point.entry = Some(entry.clone());
    point.revision = 0;
    assert_eq!(point.validate(), Err(Error::InvalidSnapshot));
    point.revision = entry.revision;
    point.lifecycle = HomeLifecycle::Archived;
    assert_eq!(point.validate(), Err(Error::InvalidSnapshot));
    point.entry = None;
    point.validate().unwrap(); // Exact validated absence is distinct from unavailable lookup.
    let mut snapshot = snapshot(entry, 10);
    snapshot.next_history = Some(10);
    assert_eq!(
        snapshot.validate(),
        Err(Error::InvalidSnapshot),
        "empty bucket cannot advertise a continuation"
    );
}
