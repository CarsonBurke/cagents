//! Explicit verification of one import-owned pending home. This never requests
//! a thread, turn, model, quota, execution policy, or command execution.
use super::{
    Cache, Session, SpawnOwner, TIMEOUT, command_with_path, current_search_path, find_executable,
};
use crate::{
    account::{Account, AccountHome, Identity, import::PhysicalIdentity},
    agent::Provider,
};
use serde_json::{Value, json};
use std::{os::unix::fs::MetadataExt, path::Path, sync::Arc};

/// Fixed diagnostics only: provider error payloads and mismatched identities do
/// not leave this module. Success is an identity observation, never task readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    InvalidRequest,
    HomeChanged,
    ProbeUnavailable,
    ProbeBusy,
    ProviderMissing,
    ProviderExecutableChanged,
    ProviderStartFailed,
    ProviderTimeout,
    ProviderProtocol,
    IdentityMismatch,
    CleanupUnconfirmed,
}

fn physical(path: &Path) -> Option<PhysicalIdentity> {
    let metadata = std::fs::metadata(path).ok()?;
    metadata.is_dir().then_some(PhysicalIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn executable_stamp(path: &Path) -> Option<(u64, u64, u64, i64, i64)> {
    let metadata = std::fs::metadata(path).ok()?;
    metadata.is_file().then_some((
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    ))
}

fn expected_identity(result: &Value, email: &str) -> Result<Identity, Error> {
    let identity =
        crate::account::codex::parse_account(result).map_err(|_| Error::IdentityMismatch)?;
    let expected = Identity {
        email: Some(email.into()),
        organization: None,
        plan: None,
    };
    if identity.email.is_none() || !identity.same_login(&expected) {
        return Err(Error::IdentityMismatch);
    }
    Ok(identity)
}

/// Acquire before creating the scoped provider owner. That owner must retain
/// this lease alongside its reservation through blocking spawn and any retained
/// child cleanup; an async waiter alone must never own the serialization lock.
pub(crate) fn probe_lock(account: &Account) -> Result<crate::instance::OwnedLock, Error> {
    let cache = Cache::installation().ok_or(Error::ProbeUnavailable)?;
    cache
        .try_probe_lock(&account.lock_name())
        .map_err(|_| Error::ProbeUnavailable)?
        .ok_or(Error::ProbeBusy)
}

/// The caller owns an exact pending-import reservation and supplies a scoped
/// SpawnOwner that retains probe_lock() and rechecks its original
/// journal/registry/home before spawning.
/// Requiring this owner also retains custody across cancelled blocking spawns.
pub(crate) async fn verify(
    account: Account,
    expected_home: PhysicalIdentity,
    expected_email: &str,
    owner: Arc<dyn SpawnOwner>,
) -> Result<Identity, Error> {
    if account.provider != Provider::Codex
        || !matches!(account.home, AccountHome::Managed { .. })
        || expected_email.is_empty()
        || expected_email.trim() != expected_email
        || expected_email.len() > 320
        || expected_email.chars().any(char::is_control)
    {
        return Err(Error::InvalidRequest);
    }
    let original = account.resolved_home().ok_or(Error::HomeChanged)?;
    let home = original.canonicalize().map_err(|_| Error::HomeChanged)?;
    if physical(&home) != Some(expected_home) {
        return Err(Error::HomeChanged);
    }
    let search = current_search_path();
    let program = find_executable(Provider::Codex.executable(), search.as_deref())
        .ok_or(Error::ProviderMissing)?;
    let executable = executable_stamp(&program).ok_or(Error::ProviderMissing)?;
    let mut process = command_with_path(&account, &program, search.as_deref());
    process.arg("app-server").current_dir(&home);
    let mut session = tokio::time::timeout(TIMEOUT, Session::spawn_owned(process, owner))
        .await
        .map_err(|_| Error::ProviderTimeout)?
        .map_err(|_| Error::ProviderStartFailed)?;
    let result = tokio::time::timeout(
        TIMEOUT,
        exchange(&mut session, &home, expected_home, expected_email),
    )
    .await
    .unwrap_or(Err(Error::ProviderTimeout));
    let cleaned = session.close().await;
    if !cleaned {
        return Err(Error::CleanupUnconfirmed);
    }
    if executable_stamp(&program) != Some(executable) {
        return Err(Error::ProviderExecutableChanged);
    }
    if original.canonicalize().ok().as_ref() != Some(&home)
        || physical(&home) != Some(expected_home)
    {
        return Err(Error::HomeChanged);
    }
    result
}

async fn rpc(session: &mut Session, id: u64, method: &str, params: Value) -> Result<Value, Error> {
    session
        .send(&json!({"id":id,"method":method,"params":params}))
        .await
        .map_err(|_| Error::ProviderProtocol)?;
    loop {
        let frame = session.next().await.map_err(|_| Error::ProviderProtocol)?;
        if frame.get("method").is_some() || frame["id"].as_u64() != Some(id) {
            continue;
        }
        if frame.get("error").is_some_and(|error| !error.is_null()) {
            return Err(Error::ProviderProtocol);
        }
        return frame.get("result").cloned().ok_or(Error::ProviderProtocol);
    }
}

async fn exchange(
    session: &mut Session,
    home: &Path,
    expected_home: PhysicalIdentity,
    expected_email: &str,
) -> Result<Identity, Error> {
    let initialized = rpc(session, 1, "initialize", json!({
        "clientInfo":{"name":"cagents","title":"cagents account import","version":env!("CARGO_PKG_VERSION")}
    })).await?;
    let reported = initialized["codexHome"]
        .as_str()
        .and_then(|path| Path::new(path).canonicalize().ok());
    if reported.as_deref() != Some(home) || physical(home) != Some(expected_home) {
        return Err(Error::HomeChanged);
    }
    session
        .send(&json!({"method":"initialized"}))
        .await
        .map_err(|_| Error::ProviderProtocol)?;
    let result = rpc(session, 2, "account/read", json!({})).await?;
    expected_identity(&result, expected_email)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fresh_chatgpt_identity_matching_the_explicit_email_is_accepted() {
        let valid =
            json!({"account":{"type":"chatgpt","email":"fixture@example.test","planType":"pro"}});
        assert_eq!(
            expected_identity(&valid, "fixture@example.test")
                .unwrap()
                .email
                .as_deref(),
            Some("fixture@example.test")
        );
        for invalid in [
            json!({"account":null,"requiresOpenaiAuth":false}),
            json!({"account":{"type":"apiKey","email":"fixture@example.test"}}),
            json!({"account":{"type":"chatgpt","email":"other@example.test"}}),
            json!({"account":{"type":"chatgpt","planType":"pro"}}),
        ] {
            assert_eq!(
                expected_identity(&invalid, "fixture@example.test"),
                Err(Error::IdentityMismatch)
            );
        }
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[tokio::test]
    async fn identity_exchange_is_home_bound_and_never_requests_execution_or_models() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().canonicalize().unwrap();
        let expected_home = physical(&home).unwrap();
        let script = directory.path().join("identity-fixture.py");
        std::fs::write(
            &script,
            r#"
import sys,json
home,behavior,trace=sys.argv[1:]
for line in sys.stdin:
    request=json.loads(line)
    method=request['method']
    with open(trace,'a') as out: out.write(method+'\n')
    if method=='initialized': continue
    if method=='initialize':
        result={'codexHome': '/' if behavior=='home' else home}
    elif method=='account/read':
        result={'account':{'type':'apiKey' if behavior=='api' else 'chatgpt',
            'email':'wrong@example.test' if behavior=='wrong' else 'fixture@example.test',
            'planType':'pro'}}
    else:
        raise AssertionError('unexpected provider operation')
    print(json.dumps({'id':request['id'],'result':result}),flush=True)
"#,
        )
        .unwrap();
        for behavior in ["valid", "wrong", "api", "home"] {
            let trace = directory.path().join(format!("{behavior}.trace"));
            // This executable is a private scripted protocol fixture. It never
            // resolves or starts a provider, changes process-wide authority, or
            // reads a real account or credential file.
            let mut process = tokio::process::Command::new("/usr/bin/python3");
            process.args([
                script.as_os_str(),
                home.as_os_str(),
                std::ffi::OsStr::new(behavior),
                trace.as_os_str(),
            ]);
            process
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .process_group(0);
            let mut session = Session::spawn(process).await.unwrap();
            let answer = tokio::time::timeout(
                TIMEOUT,
                exchange(&mut session, &home, expected_home, "fixture@example.test"),
            )
            .await
            .unwrap();
            assert!(session.close().await, "fixture cleanup must finish");
            match behavior {
                "valid" => assert_eq!(
                    answer.unwrap().email.as_deref(),
                    Some("fixture@example.test")
                ),
                "home" => assert_eq!(answer, Err(Error::HomeChanged)),
                _ => assert_eq!(answer, Err(Error::IdentityMismatch)),
            }
            let methods = std::fs::read_to_string(trace).unwrap();
            assert_eq!(
                methods,
                if behavior == "home" {
                    "initialize\n"
                } else {
                    "initialize\ninitialized\naccount/read\n"
                }
            );
        }
    }
}
