//! Explicit, account-bound reset attempts. No path here retries a consume.
use super::super::resets::{ConsumeOutcome, ConsumeStatus};
use super::*;
use sha2::{Digest, Sha256};

pub async fn consume(
    account: Account,
    reset_id: String,
    idempotency_key: String,
    expected_identity: Option<Identity>,
    cache: Option<Cache>,
) -> ConsumeOutcome {
    if idempotency_key.is_empty()
        || idempotency_key.len() > 64
        || !idempotency_key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return ConsumeOutcome::unavailable(
            "Use a reset idempotency key of 1–64 letters, digits, hyphens or underscores",
        );
    }
    let Some(expected) = expected_identity.filter(|identity| {
        identity.disputed_by.is_none()
            && identity
                .email
                .as_ref()
                .is_some_and(|email| !email.trim().is_empty())
    }) else {
        return ConsumeOutcome::unavailable("Refresh this account before confirming a reset");
    };
    let binding = binding(&account, &reset_id, &idempotency_key, &expected);
    let prior = match read_attempt(&binding) {
        Ok(prior) => prior,
        Err(message) => return ConsumeOutcome::unavailable(message),
    };
    let search = search_path().await;
    let Some(program) = find_executable(account.provider.executable(), search.as_deref()) else {
        return unsent(prior, "The provider CLI is not installed", None);
    };
    // This operation's lock is distinct from the probe's lock. Hold it through
    // refresh so concurrent views cannot spend twice from the same observation.
    let Some(dirs) = crate::instance::project_dirs() else {
        return unsent(prior, "Reset attempt storage is unavailable", None);
    };
    let directory = dirs.data_local_dir().join("reset-attempts");
    if crate::secure_file::prepare_private_directory(&directory).is_err() {
        return unsent(prior, "Reset attempt storage could not be prepared", None);
    }
    let lock_path = directory.join(format!("{}.lock", account.lock_name()));
    let _lock = match crate::instance::owned_lock(&lock_path, libc::LOCK_EX | libc::LOCK_NB) {
        Ok(lock) => lock,
        Err(_) => {
            return unsent(prior, "Another reset attempt is still being checked", None);
        }
    };
    let before = match run(account.clone(), cache.clone(), true).await {
        Outcome::Answered {
            identity: Some(identity),
            snapshot,
            ..
        } if same_identity(&identity, &expected) => snapshot,
        Outcome::Answered { .. } => {
            return unsent(
                prior,
                "This account's login changed; refresh and confirm again",
                None,
            );
        }
        _ => {
            return unsent(
                prior,
                "Usage could not be refreshed; no new reset request was sent",
                None,
            );
        }
    };
    if !prior && before.detail.as_deref() == Some("Reset availability could not be refreshed") {
        return ConsumeOutcome {
            status: ConsumeStatus::Unavailable,
            message: Some("Reset availability could not be refreshed; no reset was sent".into()),
            snapshot: Some(before),
        };
    }
    if !prior
        && !before
            .resets
            .iter()
            .any(|reset| reset.id == reset_id && reset.is_available((now_ms() / 1000) as i64))
    {
        return ConsumeOutcome {
            status: ConsumeStatus::NoCredit,
            message: Some("This reset is no longer available; usage has been refreshed".into()),
            snapshot: Some(before),
        };
    }
    if prior && reset_id == "juniper_tide" {
        return ConsumeOutcome { status: ConsumeStatus::Unconfirmed,
            message: Some("This session reset was already submitted. Claude cannot safely retry it; usage has been refreshed".into()), snapshot: Some(before) };
    }
    // Preparing Claude's client verifies the recorded organization and login
    // again after the ordinary CLI probe may have refreshed its bearer.
    let claude = if account.provider == Provider::Claude {
        match reset_transport::Client::load(&account, &program, search.as_deref()).await {
            Ok(client) => {
                if let Err(message) = client.verify_identity(&expected).await {
                    return unsent(prior, message, None);
                }
                Some(client)
            }
            Err(message) => {
                return unsent(prior, message, Some(before));
            }
        }
    } else {
        None
    };
    match write_attempt(&binding) {
        Ok(false) if reset_id == "juniper_tide" => {
            return ConsumeOutcome {
                status: ConsumeStatus::Unconfirmed,
                message: Some(
                    "This session reset was already submitted; it cannot safely be retried".into(),
                ),
                snapshot: Some(before),
            };
        }
        Err(message) => {
            return unsent(prior, message, Some(before));
        }
        _ => {}
    }
    let submitted = tokio::time::timeout(TIMEOUT, async {
        match account.provider {
            Provider::Claude => claude
                .unwrap()
                .consume(&reset_id, &idempotency_key)
                .await
                .map(|body| claude_status(&body)),
            Provider::Codex => {
                codex_consume(
                    &account,
                    &program,
                    search.as_deref(),
                    &reset_id,
                    &idempotency_key,
                    &expected,
                )
                .await
            }
            _ => Ok(ConsumeStatus::Unavailable),
        }
    })
    .await;
    let status = match submitted {
        Ok(Ok(status)) => status,
        _ => ConsumeStatus::Unconfirmed,
    };
    // Always observe the provider after submission, even after a disconnect.
    // Percentages are never optimistically cleared from the consume reply.
    let refreshed = run(account.clone(), cache, true).await;
    let snapshot = match refreshed {
        Outcome::Answered {
            identity: Some(identity),
            snapshot,
            ..
        } if same_identity(&identity, &expected) => Some(snapshot),
        _ => None,
    };
    let message = match (status, snapshot.is_some()) {
        (ConsumeStatus::Unconfirmed, true) => Some("The reset result is unconfirmed. Usage has been refreshed; retry only with the same reset and idempotency key".into()),
        (ConsumeStatus::Unconfirmed, false) => Some("The reset result and refreshed usage are unconfirmed. Retry only with the same reset and idempotency key".into()),
        (_, false) => Some("The provider answered, but refreshed usage could not be read".into()),
        (ConsumeStatus::NoCredit, true) => Some("This reset is no longer available; usage has been refreshed".into()),
        (ConsumeStatus::NotLimited, true) => Some("No eligible window needs a reset".into()),
        (ConsumeStatus::Unavailable, true) => Some("The provider did not apply a reset; usage has been refreshed".into()),
        _ => None,
    };
    ConsumeOutcome {
        status,
        message,
        snapshot,
    }
}

fn unsent(
    prior: bool,
    message: impl Into<String>,
    snapshot: Option<UsageSnapshot>,
) -> ConsumeOutcome {
    let message = message.into();
    ConsumeOutcome {
        status: if prior {
            ConsumeStatus::Unconfirmed
        } else {
            ConsumeStatus::Unavailable
        },
        message: Some(if prior {
            format!(
                "{message}. The earlier reset request remains unconfirmed; keep its original idempotency key"
            )
        } else {
            message
        }),
        snapshot,
    }
}

pub(super) fn same_identity(actual: &Identity, expected: &Identity) -> bool {
    let same = |actual: &str, expected: &str| {
        !actual.trim().is_empty() && actual.trim().eq_ignore_ascii_case(expected.trim())
    };
    actual.disputed_by.is_none()
        && actual
            .email
            .as_deref()
            .zip(expected.email.as_deref())
            .is_some_and(|(actual, expected)| same(actual, expected))
        && expected
            .organization
            .as_deref()
            .filter(|org| !org.trim().is_empty())
            .is_none_or(|expected| {
                actual
                    .organization
                    .as_deref()
                    .is_some_and(|actual| same(actual, expected))
            })
}

pub(super) fn claude_status(body: &Value) -> ConsumeStatus {
    if matches!(
        body["reason"].as_str(),
        Some("stamp_indeterminate" | "reset_unconfirmed")
    ) {
        return ConsumeStatus::Unconfirmed;
    }
    match body["result"].as_str() {
        Some("reset") => ConsumeStatus::Reset,
        Some("already_used") => ConsumeStatus::NoCredit,
        Some("not_limited") => ConsumeStatus::NotLimited,
        Some("ineligible") if body["reason"].as_str() == Some("no_grant") => {
            ConsumeStatus::NoCredit
        }
        Some("ineligible" | "cooldown") => ConsumeStatus::Unavailable,
        // Claude retains its unsettled claim for unavailable, even on 200.
        _ => ConsumeStatus::Unconfirmed,
    }
}

pub(super) async fn codex_consume(
    account: &Account,
    program: &Path,
    search: Option<&OsStr>,
    reset_id: &str,
    key: &str,
    expected: &Identity,
) -> Result<ConsumeStatus, String> {
    let mut command = command_with_path(account, program, search);
    command.arg("app-server");
    let mut session = Session::spawn(command).await?;
    let result = async {
        session.send(&json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"cagents","version":env!("CARGO_PKG_VERSION")}}})).await?;
        codex_response(&mut session, 1).await?.map_err(|_| "Codex refused to start")?;
        session.send(&json!({"method":"initialized"})).await?;
        session.send(&json!({"id":2,"method":"account/read","params":{}})).await?;
        let identity = codex_response(&mut session, 2).await?.map_err(|_| "Codex could not read this login")?;
        let identity = super::super::codex::parse_account(&identity).map_err(|_| "Codex's login is unavailable")?;
        if !same_identity(&identity, expected) { return Ok(ConsumeStatus::Unavailable); }
        let mut params = json!({"idempotencyKey":key});
        if reset_id != "codex/next" { params["creditId"] = reset_id.into(); }
        session.send(&json!({"id":3,"method":"account/rateLimitResetCredit/consume","params":params})).await?;
        let response = codex_response(&mut session, 3).await?.map_err(|_| "Codex could not confirm this reset")?;
        Ok(match response["outcome"].as_str() {
            Some("reset" | "alreadyRedeemed") => ConsumeStatus::Reset,
            Some("nothingToReset") => ConsumeStatus::NotLimited,
            Some("noCredit") => ConsumeStatus::NoCredit,
            _ => ConsumeStatus::Unconfirmed,
        })
    }.await;
    after_close(result, session.close()).await
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq)]
struct Attempt {
    account: String,
    reset_id: String,
    identity: String,
}
struct Binding {
    path: PathBuf,
    attempt: Attempt,
}
fn binding(account: &Account, reset_id: &str, key: &str, expected: &Identity) -> Binding {
    let identity = format!(
        "{:x}",
        Sha256::digest(
            format!(
                "{}\n{}",
                expected
                    .email
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase(),
                expected
                    .organization
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase()
            )
            .as_bytes()
        )
    );
    let key = format!("{:x}", Sha256::digest(key.as_bytes()));
    let directory =
        crate::instance::project_dirs().map(|dirs| dirs.data_local_dir().join("reset-attempts"));
    Binding {
        path: directory.unwrap_or_default().join(format!("{key}.json")),
        attempt: Attempt {
            account: account.id().to_string(),
            reset_id: reset_id.into(),
            identity,
        },
    }
}
fn read_attempt(binding: &Binding) -> Result<bool, String> {
    match crate::secure_file::open_existing_private_file_readonly(&binding.path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(file, 4097), &mut bytes)
                .map_err(|_| "The previous reset attempt could not be read")?;
            if bytes.len() > 4096 {
                return Err("The previous reset attempt is too large".into());
            }
            let previous: Attempt = serde_json::from_slice(&bytes)
                .map_err(|_| "The previous reset attempt cannot be verified")?;
            if previous != binding.attempt {
                return Err("This idempotency key belongs to another reset or login".into());
            }
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("The previous reset attempt cannot be read".into()),
    }
}
fn write_attempt(binding: &Binding) -> Result<bool, String> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let parent = binding
        .path
        .parent()
        .filter(|p| p.is_absolute())
        .ok_or("Reset attempt storage is unavailable")?;
    crate::secure_file::prepare_private_directory(parent)
        .map_err(|_| "Reset attempt storage could not be created")?;
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&binding.path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return read_attempt(binding).map(|_| false);
        }
        Err(_) => return Err("The reset attempt could not be recorded".into()),
    };
    let bytes = serde_json::to_vec(&binding.attempt)
        .map_err(|_| "The reset attempt could not be encoded")?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "The reset attempt could not be saved")?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "The reset attempt directory could not be saved")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn server_unconfirmed_claims_keep_uncertainty_and_scope_specific_no_credit() {
        for body in [
            json!({"result":"unavailable"}),
            json!({"result":"unavailable","reason":"reset_unconfirmed"}),
            json!({"result":"ineligible","reason":"stamp_indeterminate"}),
        ] {
            assert_eq!(claude_status(&body), ConsumeStatus::Unconfirmed);
        }
        assert_eq!(
            claude_status(&json!({"result":"already_used"})),
            ConsumeStatus::NoCredit
        );
    }
    #[test]
    fn same_key_record_is_created_once_and_cannot_change_account_or_grant() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap().join("attempt.json");
        let binding = Binding {
            path: path.clone(),
            attempt: Attempt {
                account: "claude/system".into(),
                reset_id: "cedar_ember/grant".into(),
                identity: "identity-digest".into(),
            },
        };
        assert!(write_attempt(&binding).unwrap());
        assert!(!write_attempt(&binding).unwrap());
        for (account, reset_id) in [
            ("codex/system", "cedar_ember/grant"),
            ("claude/system", "cedar_ember/other"),
        ] {
            let wrong = Binding {
                path: path.clone(),
                attempt: Attempt {
                    account: account.into(),
                    reset_id: reset_id.into(),
                    identity: "identity-digest".into(),
                },
            };
            assert!(write_attempt(&wrong).is_err());
        }
    }
}
