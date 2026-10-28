//! Requires Root's real registered migration hook; no header/layout exception.
use super::super::tests::{fixture, next};
use super::*;
use crate::{
    agent_execution::{Lineage, Origin},
    model::{ProjectKind, TerminalId},
    workspace_model::ModelEdit,
};

#[test]
fn proposal_delivery_tracks_only_real_consent_edges_and_receipt_retries_keep_the_clock() {
    let (_root, mut registry, proposal) = fixture();
    let initial = registry.automation_proposal_delivery().unwrap();
    assert!(registry.db.is_autocommit());
    assert_eq!(initial.pending_automation_proposal_count, 1);
    assert_eq!(
        initial.pending_automation_proposals,
        [ProposalSummary::from(&proposal)]
    );
    assert!(initial.automation_proposal_sequence > 1);
    let unchanged = registry
        .propose_automation(
            proposal.id,
            proposal.owner.clone(),
            proposal.requested.clone(),
            proposal.definition.clone(),
            proposal.target_identity.clone(),
            proposal.channel_title.clone(),
            200,
        )
        .unwrap();
    assert_eq!(unchanged, proposal);
    assert_eq!(registry.automation_proposal_delivery().unwrap(), initial);
    registry
        .apply(&ModelEdit::OpenProject {
            repository: proposal.definition.agent.target.repository.clone(),
            name: "Unrelated presentation edit".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    assert_eq!(registry.automation_proposal_delivery().unwrap(), initial);
    let approved = registry
        .resolve_automation_proposal(&proposal, Some(next()), 300)
        .unwrap();
    let final_state = registry.automation_proposal_delivery().unwrap();
    assert_eq!(final_state.identity, initial.identity);
    assert_eq!(
        final_state.automation_proposal_sequence,
        initial.automation_proposal_sequence + 1
    );
    assert_eq!(final_state.pending_automation_proposal_count, 0);
    assert!(final_state.pending_automation_proposals.is_empty());
    assert!(matches!(approved.state, ProposalState::Approved { .. }));
    assert_eq!(
        registry
            .resolve_automation_proposal(&proposal, None, 301)
            .unwrap(),
        approved
    );
    assert_eq!(
        registry.automation_proposal_delivery().unwrap(),
        final_state
    );
    assert!(registry.db.is_autocommit());
}

#[test]
fn ten_thousand_pending_requests_keep_delivery_bounded_and_every_cursor_page_reachable() {
    let (_root, mut registry, original) = fixture();
    let tx = registry.db.transaction().unwrap();
    tx.execute("DELETE FROM automation_proposals", []).unwrap();
    {
        let mut insert = tx.prepare("INSERT INTO automation_proposals(id,owner,repository,pending,record,summary) VALUES(?1,?2,?3,1,?4,?5)").unwrap();
        for index in 1..=10_000u128 {
            let mut proposal = original.clone();
            proposal.id = Uuid::from_u128(index);
            proposal.owner = Origin::Child {
                parent: Lineage(TerminalId::new()),
            };
            insert
                .execute(params![
                    proposal.id.to_string(),
                    encode(&proposal.owner).unwrap(),
                    proposal
                        .definition
                        .agent
                        .target
                        .repository
                        .to_str()
                        .unwrap(),
                    encode(&proposal).unwrap(),
                    encode(&ProposalSummary::from(&proposal)).unwrap()
                ])
                .unwrap();
        }
    }
    // Fixture-only bulk insertion updates the maintained count in that TX.
    tx.execute("UPDATE metadata SET pending_automation_proposal_count=10000,automation_proposal_sequence=automation_proposal_sequence+1,sequence=sequence+1 WHERE key=0", []).unwrap();
    tx.commit().unwrap();
    let snapshot = registry.automation_proposal_delivery().unwrap();
    assert_eq!(snapshot.pending_automation_proposal_count, 10_000);
    assert_eq!(
        snapshot.pending_automation_proposals.len(),
        PROPOSAL_PAGE_SIZE
    );
    let bytes = serde_json::to_vec(&snapshot).unwrap();
    assert!(bytes.len() < 128 * 1024);
    assert!(
        !String::from_utf8(bytes)
            .unwrap()
            .contains(&original.definition.prompt)
    );
    let mut seen = std::collections::HashSet::new();
    let mut cursor = None;
    loop {
        let page = registry
            .pending_automation_proposals_page(
                Some(snapshot.identity),
                Some(snapshot.automation_proposal_sequence),
                cursor,
                usize::MAX,
            )
            .unwrap()
            .proposals;
        assert!(page.len() <= PROPOSAL_PAGE_SIZE);
        if page.is_empty() {
            break;
        }
        assert!(page.windows(2).all(|pair| pair[0].id < pair[1].id));
        assert!(cursor.is_none_or(|cursor| page[0].id > cursor));
        cursor = page.last().map(|proposal| proposal.id);
        for proposal in page {
            assert!(seen.insert(proposal.id));
        }
    }
    assert_eq!(seen.len(), 10_000);
    assert!(seen.contains(&Uuid::from_u128(10_000)));
    for after in [String::new(), Uuid::from_u128(9_984).to_string()] {
        let mut explain = registry
            .db
            .prepare(&format!("EXPLAIN QUERY PLAN {PENDING_PAGE_SQL}"))
            .unwrap();
        let plan = explain
            .query_map(params![after, 64], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        assert!(
            plan.contains("SEARCH")
                && plan.contains("global_pending_automation_proposals")
                && plan.contains("id>?"),
            "{plan}"
        );
    }
}

#[test]
fn failed_delivery_clock_update_rolls_back_proposal_insert_and_decision() {
    let (_root, mut registry, proposal) = fixture();
    let initial = registry.automation_proposal_delivery().unwrap();
    registry.db.execute_batch("CREATE TEMP TRIGGER refuse_proposal_delivery_clock BEFORE UPDATE OF automation_proposal_sequence ON metadata BEGIN SELECT RAISE(ABORT,'fixture clock failure'); END;").unwrap();
    let new_id = Uuid::new_v4();
    assert!(
        registry
            .propose_automation(
                new_id,
                proposal.owner.clone(),
                proposal.requested.clone(),
                proposal.definition.clone(),
                proposal.target_identity.clone(),
                proposal.channel_title.clone(),
                200
            )
            .is_err()
    );
    assert!(
        registry
            .automation_proposal(new_id, None)
            .unwrap()
            .is_none()
    );
    assert!(
        registry
            .resolve_automation_proposal(&proposal, Some(next()), 200)
            .is_err()
    );
    assert!(registry.automations(None, 100).unwrap().is_empty());
    assert_eq!(
        registry.automation_proposal(proposal.id, None).unwrap(),
        Some(proposal.clone())
    );
    assert_eq!(registry.automation_proposal_delivery().unwrap(), initial);
    assert!(registry.db.is_autocommit());
    registry
        .db
        .execute_batch("DROP TRIGGER refuse_proposal_delivery_clock;")
        .unwrap();
    registry
        .resolve_automation_proposal(&proposal, None, 300)
        .unwrap();
    assert_eq!(
        registry
            .automation_proposal_delivery()
            .unwrap()
            .pending_automation_proposal_count,
        0
    );
}

#[test]
fn one_read_snapshot_keeps_identity_clock_count_and_page_coherent_across_a_real_wal_writer() {
    let (_root, registry, original) = fixture();
    let mut writer = registry.execution_connection().unwrap();
    let before = registry.automation_proposal_delivery().unwrap();
    let tx =
        rusqlite::Transaction::new_unchecked(&registry.db, TransactionBehavior::Deferred).unwrap();
    let held = snapshot_in(&tx).unwrap();
    assert_eq!(held, before);
    let new_id = Uuid::new_v4();
    writer
        .propose_automation(
            new_id,
            original.owner.clone(),
            original.requested.clone(),
            original.definition.clone(),
            original.target_identity.clone(),
            original.channel_title.clone(),
            300,
        )
        .unwrap();
    assert_eq!(snapshot_in(&tx).unwrap(), held);
    assert!(
        registry
            .automation_proposal_delivery()
            .unwrap_err()
            .contains("own read snapshot")
    );
    assert!(
        !registry.db.is_autocommit(),
        "nested refusal must not release the caller's read"
    );
    tx.commit().unwrap();
    assert!(registry.db.is_autocommit());
    let after = registry.automation_proposal_delivery().unwrap();
    assert_eq!(after.identity, before.identity);
    assert_eq!(
        after.automation_proposal_sequence,
        before.automation_proposal_sequence + 1
    );
    assert_eq!(
        after.pending_automation_proposal_count,
        before.pending_automation_proposal_count + 1
    );
    assert!(
        after
            .pending_automation_proposals
            .iter()
            .any(|row| row.id == new_id)
    );
    registry
        .db
        .execute(
            "UPDATE automation_proposals SET summary='invalid fixture JSON' WHERE id=?1",
            [new_id.to_string()],
        )
        .unwrap();
    assert!(registry.automation_proposal_delivery().is_err());
    assert!(
        registry.db.is_autocommit(),
        "failed decode explicitly releases its read snapshot"
    );
}

#[test]
fn proposal_pages_refuse_partial_foreign_and_stale_keys_before_returning_a_continuation() {
    let (_root, mut registry, original) = fixture();
    let delivery = registry.automation_proposal_delivery().unwrap();
    let first = registry
        .pending_automation_proposals_page(None, None, None, 64)
        .unwrap();
    assert_eq!(first.identity, delivery.identity);
    assert_eq!(
        first.automation_proposal_sequence,
        delivery.automation_proposal_sequence
    );
    assert_eq!(first.proposals, delivery.pending_automation_proposals);
    for (identity, sequence, after) in [
        (None, None, Some(original.id)),
        (Some(delivery.identity), None, None),
        (None, Some(delivery.automation_proposal_sequence), None),
        (
            Some(Uuid::new_v4()),
            Some(delivery.automation_proposal_sequence),
            Some(original.id),
        ),
    ] {
        assert!(
            registry
                .pending_automation_proposals_page(identity, sequence, after, 64)
                .is_err()
        );
        assert!(registry.db.is_autocommit());
    }
    registry
        .propose_automation(
            Uuid::new_v4(),
            original.owner.clone(),
            original.requested.clone(),
            original.definition.clone(),
            original.target_identity.clone(),
            original.channel_title.clone(),
            200,
        )
        .unwrap();
    assert!(
        registry
            .pending_automation_proposals_page(
                Some(delivery.identity),
                Some(delivery.automation_proposal_sequence),
                Some(original.id),
                64
            )
            .is_err()
    );
    let current = registry.automation_proposal_delivery().unwrap();
    let next = registry
        .pending_automation_proposals_page(
            Some(current.identity),
            Some(current.automation_proposal_sequence),
            Some(original.id),
            64,
        )
        .unwrap();
    assert_eq!(next.identity, current.identity);
    assert_eq!(
        next.automation_proposal_sequence,
        current.automation_proposal_sequence
    );
    assert_eq!(next.after, Some(original.id));
    assert!(next.proposals.iter().all(|row| row.id > original.id));
}

#[test]
fn replaced_registry_same_uuid_revision_and_different_consent_cannot_receive_the_old_decision() {
    let (_old_root, mut old_registry, original) = fixture();
    let old_identity = old_registry
        .automation_proposal_delivery()
        .unwrap()
        .identity;
    let old_review = old_registry
        .review_automation_proposal(old_identity, original.id)
        .unwrap();
    let request = crate::automations::Request::ResolveReviewedProposal {
        identity: old_identity,
        proposal: old_review.proposal.clone(),
        approved: true,
    };
    assert!(!request.allowed_from_hosted_conversation());
    assert_eq!(
        postcard::from_bytes::<crate::automations::Request>(
            &postcard::to_allocvec(&request).unwrap()
        )
        .unwrap(),
        request
    );
    let (_new_root, mut replacement, template) = fixture();
    let new_identity = replacement.automation_proposal_delivery().unwrap().identity;
    assert_ne!(old_identity, new_identity);
    let mut different = template.definition.clone();
    different.prompt = "Different immutable consent in a different registry".into();
    let replaced = replacement
        .propose_automation(
            original.id,
            template.owner,
            different.clone(),
            different,
            template.target_identity,
            template.channel_title,
            300,
        )
        .unwrap();
    assert_eq!(replaced.id, original.id);
    assert_eq!(replaced.revision, original.revision);
    assert_ne!(replaced.definition, original.definition);
    let before = replacement.automation_proposal_delivery().unwrap();
    assert!(
        replacement
            .review_automation_proposal(old_identity, original.id)
            .is_err()
    );
    assert!(
        replacement
            .resolve_automation_proposal_in_registry(
                old_identity,
                &old_review.proposal,
                Some(next()),
                400
            )
            .is_err()
    );
    assert!(
        replacement
            .resolve_automation_proposal_in_registry(
                new_identity,
                &old_review.proposal,
                Some(next()),
                400
            )
            .is_err()
    );
    assert!(replacement.automations(None, 100).unwrap().is_empty());
    assert_eq!(replacement.automation_proposal_delivery().unwrap(), before);
    let legitimate = replacement
        .review_automation_proposal(new_identity, original.id)
        .unwrap();
    let receipt = replacement
        .resolve_automation_proposal_in_registry(
            new_identity,
            &legitimate.proposal,
            Some(next()),
            500,
        )
        .unwrap();
    assert!(matches!(receipt.state, ProposalState::Approved { .. }));
    assert_eq!(replacement.automations(None, 100).unwrap().len(), 1);
    let old_receipt = old_registry
        .resolve_automation_proposal_in_registry(old_identity, &original, None, 600)
        .unwrap();
    assert_eq!(old_receipt.state, ProposalState::Declined);
}

#[test]
fn retained_proposal_handles_refuse_future_admission_without_publishing_or_mutating() {
    let (_root, mut registry, original) = fixture();
    let before = registry.automation_proposal_delivery().unwrap();
    let future = super::super::super::SCHEMA_VERSION + 1;
    registry
        .db
        .pragma_update(None, "user_version", future)
        .unwrap();
    assert!(registry.automation_proposal_delivery().is_err());
    assert!(
        registry
            .pending_automation_proposals_page(
                Some(before.identity),
                Some(before.automation_proposal_sequence),
                None,
                64
            )
            .is_err()
    );
    assert!(
        registry
            .review_automation_proposal(before.identity, original.id)
            .is_err()
    );
    let new_id = Uuid::new_v4();
    assert!(
        registry
            .propose_automation(
                new_id,
                original.owner.clone(),
                original.requested.clone(),
                original.definition.clone(),
                original.target_identity.clone(),
                original.channel_title.clone(),
                300
            )
            .is_err()
    );
    assert!(
        registry
            .resolve_automation_proposal_in_registry(before.identity, &original, Some(next()), 300)
            .is_err()
    );
    assert!(
        registry
            .automation_proposal(new_id, None)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        registry.automation_proposal(original.id, None).unwrap(),
        Some(original)
    );
    let counters:(u64,u64)=registry.db.query_row("SELECT automation_proposal_sequence,pending_automation_proposal_count FROM metadata WHERE key=0",[],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!(
        counters,
        (
            before.automation_proposal_sequence,
            before.pending_automation_proposal_count
        )
    );
    assert!(registry.db.is_autocommit());
    assert_eq!(
        registry
            .db
            .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        future
    );
}
