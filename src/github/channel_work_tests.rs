//! Canonical channel work keys must accept forge URL casing without borrowing
//! a different base repository, fork, authority, or PR number.
use super::*;
use serde_json::json;

fn repository() -> GitHubRepoKey {
    GitHubRepoKey::new("enterprise.example", Some(8443), "owner", "repository").unwrap()
}

fn document(url: &str, number: u64) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id":"PR_identity",
        "number":number,
        "title":"Declared channel work",
        "state":"OPEN",
        "headRefName":"contribution",
        "headRefOid":"a".repeat(40),
        "headRepository":{"name":"Repository"},
        "headRepositoryOwner":{"login":"ForkOwner"},
        "baseRefName":"main",
        "updatedAt":"2026-10-23T00:00:00Z",
        "url":url
    }))
    .unwrap()
}

#[test]
fn canonical_channel_key_accepts_mixed_case_forge_base_url() {
    let repository = repository();
    let url = Url::parse("https://enterprise.example:8443/Owner/Repository/pull/7").unwrap();
    assert_eq!(pull_request_number(&url, &repository).unwrap(), 7);
    let parsed = parse_pull_request(&document(url.as_str(), 7), &repository).unwrap();
    assert_eq!(parsed.number, 7);
    assert_eq!(parsed.id.as_deref(), Some("PR_identity"));
    assert!(repository.same_repository(
        &GitHubRepoKey::new("ENTERPRISE.EXAMPLE", Some(8443), "OWNER", "REPOSITORY").unwrap()
    ));
    assert_eq!(
        parsed.url, url,
        "validation does not rewrite observed forge provenance"
    );
}

#[test]
fn relaxed_base_casing_still_rejects_other_fork_host_port_number_and_credentials() {
    let repository = repository();
    for value in [
        "https://enterprise.example:8443/ForkOwner/Repository/pull/7",
        "https://enterprise.example:8443/Owner/Other/pull/7",
        "https://other.example:8443/Owner/Repository/pull/7",
        "https://enterprise.example:9443/Owner/Repository/pull/7",
        "https://enterprise.example/Owner/Repository/pull/7",
        "https://credential@enterprise.example:8443/Owner/Repository/pull/7",
        "http://enterprise.example:8443/Owner/Repository/pull/7",
        "https://enterprise.example:8443/Owner/Repository/pull/7?retarget=1",
        "https://enterprise.example:8443/Owner/Repository/pull/7#retarget",
        "https://enterprise.example:8443/Owner/Repository/pull/0",
        "https://enterprise.example:8443/Owner/Repository/pull/not-a-number",
    ] {
        let url = Url::parse(value).unwrap();
        assert!(pull_request_number(&url, &repository).is_err(), "{value}");
        assert!(
            parse_pull_request(&document(value, 7), &repository).is_err(),
            "{value}"
        );
    }
    assert!(
        parse_pull_request(
            &document("https://enterprise.example:8443/Owner/Repository/pull/8", 7),
            &repository
        )
        .is_err(),
        "JSON number and URL number must agree"
    );
}

#[test]
fn a_fork_head_never_replaces_the_tracked_base_repository_identity() {
    let repository = repository();
    let parsed = parse_pull_request(
        &document("https://enterprise.example:8443/Owner/Repository/pull/7", 7),
        &repository,
    )
    .unwrap();
    let fork =
        GitHubRepoKey::new("enterprise.example", Some(8443), "ForkOwner", "Repository").unwrap();
    assert!(parsed.belongs_to_head(&fork));
    assert!(!parsed.belongs_to_head(&repository));
    assert!(
        parse_pull_request(
            &document(
                "https://enterprise.example:8443/ForkOwner/Repository/pull/7",
                7
            ),
            &repository
        )
        .is_err(),
        "head repository metadata cannot authorize a different base PR URL"
    );
}
