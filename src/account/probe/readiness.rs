//! Model-free measurements through the official Codex app server. No command
//! in this module requests a policy override, thread, turn, or unsandboxed API.
use super::{
    Cache, Session, TIMEOUT, command_with_path, current_search_path, find_executable, now_ms,
};
use crate::{
    account::{Account, Identity, UsageStatus, readiness::*},
    agent::Provider,
};
use serde_json::{Value, json};
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

type RpcResult = Result<Value, CheckReason>;
const EXEC_TIMEOUT_MS: i64 = 8_000;
const EXEC_OUTPUT_CAP: u64 = 16 * 1024;

#[cfg(test)]
tokio::task_local! {
    /// A capture-refusal regression must never contact an installed provider.
    static FORBID_PROVIDER_IO: ();
}

pub(crate) async fn run(
    account: Account,
    workspace: PathBuf,
    request: CheckRequest,
    helper: PathBuf,
    expected_home: PhysicalIdentity,
    expected_workspace: PhysicalIdentity,
    owner: Option<std::sync::Arc<dyn super::SpawnOwner>>,
) -> CheckReport {
    let mut report = CheckReport::new(&request);
    report.workspace = workspace.clone();
    report.observed_at = now_ms();
    if request.validate().is_err() || !workspace.is_absolute() || !helper.is_absolute() {
        stop(
            &mut report,
            CheckReason::InvalidRequest,
            CheckStatus::Failed,
        );
        return report;
    }
    if account.provider != Provider::Codex {
        stop(
            &mut report,
            CheckReason::UnsupportedProvider,
            CheckStatus::Unsupported,
        );
        return report;
    }
    if account.id() != request.account {
        stop(
            &mut report,
            CheckReason::AccountMismatch,
            CheckStatus::Failed,
        );
        return report;
    }
    let home = match account
        .resolved_home()
        .and_then(|home| home.canonicalize().ok())
    {
        Some(home) => home,
        None => {
            stop(
                &mut report,
                CheckReason::HomeUnavailable,
                CheckStatus::Failed,
            );
            return report;
        }
    };
    let home_identity = match directory_identity(&home) {
        Ok(identity) if identity == (expected_home.device, expected_home.inode) => identity,
        _ => {
            stop(
                &mut report,
                CheckReason::HomeUnavailable,
                CheckStatus::Failed,
            );
            return report;
        }
    };
    let workspace_identity = match directory_identity(&workspace) {
        Ok(identity) if identity == (expected_workspace.device, expected_workspace.inode) => {
            identity
        }
        _ => {
            stop(
                &mut report,
                CheckReason::WorkspaceChanged,
                CheckStatus::Failed,
            );
            return report;
        }
    };
    #[cfg(test)]
    assert!(
        FORBID_PROVIDER_IO.try_with(|_| ()).is_err(),
        "a changed host capture reached provider preparation"
    );
    // A readiness check must not silently lose serialization with official
    // usage/login probes. Hold the same lock through provider cleanup.
    let Some(cache) = Cache::installation() else {
        stop(
            &mut report,
            CheckReason::ProbeUnavailable,
            CheckStatus::Unknown,
        );
        return report;
    };
    let _lock = match cache.try_probe_lock(&account.lock_name()) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            stop(&mut report, CheckReason::ProbeBusy, CheckStatus::Unknown);
            return report;
        }
        Err(_) => {
            stop(
                &mut report,
                CheckReason::ProbeUnavailable,
                CheckStatus::Unknown,
            );
            return report;
        }
    };
    // This explicit diagnostic uses the already discovered shell PATH, or
    // the daemon's inherited PATH. It never starts an interactive shell as
    // a hidden preparation step outside its owned provider process group.
    let path = current_search_path();
    let Some(program) = find_executable(Provider::Codex.executable(), path.as_deref()) else {
        stop(
            &mut report,
            CheckReason::ProviderMissing,
            CheckStatus::Failed,
        );
        return report;
    };
    let executable = match executable_identity(&program) {
        Some(identity) => identity,
        None => {
            stop(
                &mut report,
                CheckReason::ProviderMissing,
                CheckStatus::Failed,
            );
            return report;
        }
    };
    report.provider_executable = Some(executable.clone());
    let mut process = command_with_path(&account, &program, path.as_deref());
    process
        .arg("app-server")
        .current_dir(&workspace)
        .env("CODEX_HOME", &home);
    let spawn = async move {
        match owner {
            Some(owner) => Session::spawn_owned(process, owner).await,
            None => Session::spawn(process).await,
        }
    };
    let mut session = match tokio::time::timeout(TIMEOUT, spawn).await {
        Ok(Ok(session)) => session,
        Ok(Err(_)) => {
            stop(
                &mut report,
                CheckReason::ProviderStartFailed,
                CheckStatus::Failed,
            );
            return report;
        }
        Err(_) => {
            stop(
                &mut report,
                CheckReason::ProviderTimeout,
                CheckStatus::Unknown,
            );
            return report;
        }
    };
    let original_home = account.resolved_home();
    if original_home
        .as_ref()
        .and_then(|path| path.canonicalize().ok())
        .as_ref()
        != Some(&home)
    {
        report.provider_cleanup_confirmed = session.close().await;
        stop(
            &mut report,
            CheckReason::AccountMismatch,
            CheckStatus::Failed,
        );
        return report;
    }
    let checked = tokio::time::timeout(
        TIMEOUT,
        exchange(
            &mut session,
            &request,
            &helper,
            &home,
            home_identity,
            &workspace,
            workspace_identity,
            &mut report,
        ),
    )
    .await;
    match checked {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => {
            if report.authentication.reason == CheckReason::NotChecked {
                report.authentication = Measurement::unknown(reason);
            }
            if report.execution_profile_configured.reason == CheckReason::NotChecked {
                report.execution_profile_configured = Measurement::unknown(reason);
            }
            report.invalidate_measurements(reason);
        }
        Err(_) => {
            if report.authentication.reason == CheckReason::NotChecked {
                report.authentication = Measurement::unknown(CheckReason::ProviderTimeout);
            }
            if report.execution_profile_configured.reason == CheckReason::NotChecked {
                report.execution_profile_configured =
                    Measurement::unknown(CheckReason::ProviderTimeout);
            }
            report.invalidate_measurements(CheckReason::ProviderTimeout);
        }
    }
    // Even a timed-out exchange owns and reaps only its own provider group.
    report.provider_cleanup_confirmed = session.close().await;
    if original_home
        .as_ref()
        .and_then(|path| path.canonicalize().ok())
        .as_ref()
        != Some(&home)
    {
        stop(
            &mut report,
            CheckReason::AccountMismatch,
            CheckStatus::Failed,
        );
    }
    if executable_identity(&program).as_ref() != Some(&executable) {
        report.invalidate_measurements(CheckReason::ProviderExecutableChanged);
        report.authentication = Measurement::unknown(CheckReason::ProviderExecutableChanged);
        report.execution_profile_configured =
            Measurement::unknown(CheckReason::ProviderExecutableChanged);
    }
    report.observed_at = now_ms();
    report
}

fn stop(report: &mut CheckReport, reason: CheckReason, status: CheckStatus) {
    report.authentication = Measurement::new(status, reason);
    report.execution_profile_configured = Measurement::unknown(reason);
    report.invalidate_measurements(reason);
}

fn executable_identity(path: &Path) -> Option<ExecutableIdentity> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    Some(ExecutableIdentity {
        path: path.to_path_buf(),
        device: metadata.dev(),
        inode: metadata.ino(),
        len: metadata.len(),
        modified_ms,
    })
}

fn directory_identity(path: &Path) -> Result<(u64, u64), CheckReason> {
    let metadata = std::fs::metadata(path).map_err(|_| CheckReason::HomeUnavailable)?;
    if !metadata.is_dir() {
        return Err(CheckReason::HomeUnavailable);
    }
    Ok((metadata.dev(), metadata.ino()))
}

async fn rpc(session: &mut Session, id: u64, method: &str, params: Value) -> RpcResult {
    session
        .send(&json!({"id": id, "method": method, "params": params}))
        .await
        .map_err(|_| CheckReason::ProviderProtocol)?;
    loop {
        let frame = session
            .next()
            .await
            .map_err(|_| CheckReason::ProviderProtocol)?;
        if frame.get("method").is_some() || frame["id"].as_u64() != Some(id) {
            continue;
        }
        if let Some(error) = frame.get("error").filter(|error| !error.is_null()) {
            return Err(rpc_error(error));
        }
        return frame
            .get("result")
            .cloned()
            .ok_or(CheckReason::ProviderProtocol);
    }
}

fn rpc_error(error: &Value) -> CheckReason {
    if error["code"].as_i64() == Some(-32601) {
        return CheckReason::UnsupportedMethod;
    }
    // Recognize narrow known setup/denial messages; no provider text escapes.
    let message = error["message"].as_str().unwrap_or("").to_ascii_lowercase();
    if message.contains("error building bubblewrap command")
        || message
            .contains("app-server socket directory must be a user-owned directory with mode 0700")
    {
        CheckReason::SandboxSetupFailed
    } else if message.contains("sandbox")
        && (message.contains("denied") || message.contains("not permitted"))
    {
        CheckReason::SandboxDenied
    } else {
        CheckReason::ProviderProtocol
    }
}

fn authenticate(result: &Value, expected: &str, report: &mut CheckReport) -> bool {
    match crate::account::codex::parse_account(result) {
        Ok(identity) => {
            let expected = Identity {
                email: Some(expected.trim().into()),
                organization: None,
                plan: None,
            };
            let matched = identity.email.is_some() && identity.same_login(&expected);
            report.observed_identity = Some(identity);
            report.authentication = Measurement::new(
                if matched {
                    CheckStatus::Passed
                } else {
                    CheckStatus::Failed
                },
                if matched {
                    CheckReason::IdentityConfirmed
                } else {
                    CheckReason::IdentityMismatch
                },
            );
            matched
        }
        Err(status) => {
            report.authentication = Measurement::new(
                CheckStatus::Failed,
                if status == UsageStatus::NeedsLogin {
                    CheckReason::SignedOut
                } else {
                    CheckReason::NotSubscription
                },
            );
            false
        }
    }
}

fn policy(result: &Value) -> Result<(PolicySummary, Option<bool>), CheckReason> {
    let config = result
        .get("config")
        .filter(|value| value.is_object())
        .ok_or(CheckReason::ProviderProtocol)?;
    let sandbox_mode = match config["sandbox_mode"].as_str() {
        Some("read-only") => SandboxMode::ReadOnly,
        Some("workspace-write") => SandboxMode::WorkspaceWrite,
        Some("danger-full-access") => SandboxMode::DangerFullAccess,
        Some("external-sandbox") => SandboxMode::ExternalSandbox,
        _ => SandboxMode::Unknown,
    };
    let approval_policy = match config["approval_policy"].as_str() {
        Some("untrusted") => ApprovalPolicy::Untrusted,
        Some("on-failure") => ApprovalPolicy::OnFailure,
        Some("on-request") => ApprovalPolicy::OnRequest,
        Some("never") => ApprovalPolicy::Never,
        _ => ApprovalPolicy::Unknown,
    };
    let network_access = match sandbox_mode {
        SandboxMode::WorkspaceWrite => {
            config["sandbox_workspace_write"]["network_access"].as_bool()
        }
        SandboxMode::DangerFullAccess => Some(true),
        _ => None,
    };
    let origin = result["origins"].get("sandbox_mode");
    let configured = match origin.and_then(|origin| origin["name"]["type"].as_str()) {
        Some(
            "user"
            | "project"
            | "system"
            | "sessionFlags"
            | "mdm"
            | "enterpriseManaged"
            | "legacyManagedConfigTomlFromFile"
            | "legacyManagedConfigTomlFromMdm",
        ) => Some(true),
        Some("packagedDefaults") => Some(false),
        None if origin.is_none() => Some(false),
        _ => None,
    };
    Ok((
        PolicySummary {
            sandbox_mode,
            approval_policy,
            network_access,
        },
        configured,
    ))
}

async fn execute(session: &mut Session, id: u64, argv: Vec<String>, workspace: &Path) -> RpcResult {
    let github = argv.first().is_some_and(|program| program == "gh");
    let mut params = json!({
        "command": argv, "cwd": workspace, "timeoutMs": EXEC_TIMEOUT_MS,
        "outputBytesCap": EXEC_OUTPUT_CAP, "processId": format!("cagents-account-check-{id}"),
    });
    if github {
        // Suppress interactive prompts only; never alter credential routing,
        // session authority, sandbox, or provider permissions to pass a check.
        params["env"] = json!({"GH_PROMPT_DISABLED":"1","GIT_TERMINAL_PROMPT":"0"});
    }
    rpc(session, id, "command/exec", params).await
}

async fn exchange(
    session: &mut Session,
    request: &CheckRequest,
    helper: &Path,
    home: &Path,
    home_identity: (u64, u64),
    workspace: &Path,
    workspace_identity: (u64, u64),
    report: &mut CheckReport,
) -> Result<(), CheckReason> {
    let initialize = rpc(session, 1, "initialize", json!({
        "clientInfo":{"name":"cagents","title":"cagents account check","version":env!("CARGO_PKG_VERSION")}
    })).await?;
    let reported_home = initialize["codexHome"]
        .as_str()
        .map(Path::new)
        .and_then(|path| path.canonicalize().ok());
    if reported_home.as_deref() != Some(home) || directory_identity(home)? != home_identity {
        stop(report, CheckReason::AccountMismatch, CheckStatus::Failed);
        return Ok(());
    }
    session
        .send(&json!({"method":"initialized"}))
        .await
        .map_err(|_| CheckReason::ProviderProtocol)?;
    let identity = rpc(session, 2, "account/read", json!({})).await?;
    if !authenticate(&identity, &request.expected_email, report) {
        report.invalidate_measurements(report.authentication.reason);
        return Ok(());
    }
    let original_config = match rpc(
        session,
        3,
        "config/read",
        json!({"cwd":workspace,"includeLayers":false}),
    )
    .await
    {
        Ok(config) => config,
        Err(reason) => {
            report.execution_profile_configured = Measurement::new(
                if reason == CheckReason::UnsupportedMethod {
                    CheckStatus::Unsupported
                } else {
                    CheckStatus::Unknown
                },
                reason,
            );
            report.invalidate_measurements(reason);
            return Ok(());
        }
    };
    let (summary, configured) = policy(&original_config)?;
    report.execution_profile_configured = Measurement::new(
        if configured == Some(true) && summary.sandbox_mode != SandboxMode::Unknown {
            CheckStatus::Passed
        } else {
            CheckStatus::Unknown
        },
        if summary.sandbox_mode == SandboxMode::Unknown || configured.is_none() {
            CheckReason::PolicyUnrecognized
        } else if configured == Some(true) {
            CheckReason::PolicyReported
        } else {
            CheckReason::ImplicitPolicy
        },
    );
    report.profile = Some(summary.clone());
    if directory_identity(home)? != home_identity {
        stop(report, CheckReason::AccountMismatch, CheckStatus::Failed);
        return Ok(());
    }
    if directory_identity(workspace).map_err(|_| CheckReason::WorkspaceChanged)?
        != workspace_identity
    {
        report.invalidate_measurements(CheckReason::WorkspaceChanged);
        return Ok(());
    }
    if request.write {
        let nonce = uuid::Uuid::new_v4();
        let result = execute(
            session,
            4,
            vec![
                helper.to_str().ok_or(CheckReason::InvalidRequest)?.into(),
                "internal-workspace-probe".into(),
                workspace
                    .to_str()
                    .ok_or(CheckReason::InvalidRequest)?
                    .into(),
                "--device-id".into(),
                workspace_identity.0.to_string(),
                "--inode".into(),
                workspace_identity.1.to_string(),
                "--nonce".into(),
                nonce.to_string(),
            ],
            workspace,
        )
        .await;
        let (measurement, receipt) = write_measurement(result, nonce);
        report.workspace_write = measurement;
        report.workspace_cleanup = Some(
            receipt
                .as_ref()
                .map(|receipt| receipt.cleanup)
                .unwrap_or(crate::account::workspace_probe::Cleanup::Unconfirmed),
        );
        report.workspace_receipt = receipt;
    }
    if let Some(repository) = &request.github_repository {
        let result = execute(
            session,
            5,
            vec![
                "gh".into(),
                "api".into(),
                "--hostname".into(),
                "github.com".into(),
                format!("repos/{repository}"),
                "--jq".into(),
                ".permissions.push // false".into(),
            ],
            workspace,
        )
        .await;
        let (measurement, push) = github_measurement(result);
        report.github = measurement;
        report.github_push_allowed = push;
    }
    // Do not turn a successful command on a replaced login/config/path into
    // evidence for the initially selected execution context.
    let final_identity = rpc(session, 6, "account/read", json!({})).await?;
    if !authenticate(&final_identity, &request.expected_email, report) {
        report.invalidate_measurements(report.authentication.reason);
        return Ok(());
    }
    let final_config = rpc(
        session,
        7,
        "config/read",
        json!({"cwd":workspace,"includeLayers":false}),
    )
    .await?;
    if policy(&final_config)? != (summary, configured)
        || final_config.get("config") != original_config.get("config")
        || final_config.get("origins") != original_config.get("origins")
    {
        report.execution_profile_configured = Measurement::unknown(CheckReason::PolicyChanged);
        report.invalidate_measurements(CheckReason::PolicyChanged);
    }
    if directory_identity(home)? != home_identity {
        stop(report, CheckReason::AccountMismatch, CheckStatus::Failed);
    } else if directory_identity(workspace).map_err(|_| CheckReason::WorkspaceChanged)?
        != workspace_identity
    {
        report.invalidate_measurements(CheckReason::WorkspaceChanged);
    }
    Ok(())
}

fn write_measurement(
    result: RpcResult,
    nonce: uuid::Uuid,
) -> (
    Measurement,
    Option<crate::account::workspace_probe::Receipt>,
) {
    let result = match result {
        Ok(result) => result,
        Err(reason) => return (rpc_measurement(reason), None),
    };
    if result["exitCode"].as_i64().is_none() {
        return (
            Measurement::unknown(CheckReason::InvalidCommandReceipt),
            None,
        );
    }
    let receipt = result["stdout"].as_str().and_then(|text| {
        serde_json::from_str::<crate::account::workspace_probe::Receipt>(text).ok()
    });
    let Some(receipt) = receipt.filter(|receipt| receipt.nonce == nonce) else {
        return (
            Measurement::new(CheckStatus::Failed, classify_command_failure(&result)),
            None,
        );
    };
    let measurement = if receipt.is_verified() && result["exitCode"] == 0 {
        Measurement::new(CheckStatus::Passed, CheckReason::WorkspaceRoundTrip)
    } else {
        Measurement::new(
            CheckStatus::Failed,
            if receipt.created && !receipt.removed {
                CheckReason::WorkspaceCleanupUnconfirmed
            } else {
                CheckReason::CommandFailed
            },
        )
    };
    (measurement, Some(receipt))
}

fn rpc_measurement(reason: CheckReason) -> Measurement {
    Measurement::new(
        match reason {
            CheckReason::UnsupportedMethod => CheckStatus::Unsupported,
            CheckReason::SandboxSetupFailed | CheckReason::SandboxDenied => CheckStatus::Failed,
            _ => CheckStatus::Unknown,
        },
        reason,
    )
}

fn classify_command_failure(result: &Value) -> CheckReason {
    let text = result["stderr"].as_str().unwrap_or("").to_ascii_lowercase();
    if text.contains("error building bubblewrap command")
        || text
            .contains("app-server socket directory must be a user-owned directory with mode 0700")
    {
        CheckReason::SandboxSetupFailed
    } else if text.contains("permission denied")
        || text.contains("read-only file system")
        || text.contains("operation not permitted")
    {
        CheckReason::SandboxDenied
    } else {
        CheckReason::CommandFailed
    }
}

fn github_measurement(result: RpcResult) -> (Measurement, Option<bool>) {
    let result = match result {
        Ok(result) => result,
        Err(reason) => return (rpc_measurement(reason), None),
    };
    if result["exitCode"] == 0 {
        return match result["stdout"].as_str().map(str::trim) {
            Some("true") => (
                Measurement::new(CheckStatus::Passed, CheckReason::GitHubRepositoryAccess),
                Some(true),
            ),
            Some("false") => (
                Measurement::new(CheckStatus::Passed, CheckReason::GitHubRepositoryAccess),
                Some(false),
            ),
            _ => (
                Measurement::unknown(CheckReason::InvalidCommandReceipt),
                None,
            ),
        };
    }
    let text = result["stderr"].as_str().unwrap_or("").to_ascii_lowercase();
    let reason = if result["exitCode"] == 127 {
        CheckReason::GitHubUnavailable
    } else if text.contains("http 401")
        || text.contains("authentication")
        || text.contains("gh auth login")
    {
        CheckReason::GitHubAuthenticationFailed
    } else if text.contains("http 403") || text.contains("http 404") {
        CheckReason::GitHubRepositoryDenied
    } else if text.contains("could not resolve")
        || text.contains("failed to connect")
        || text.contains("tls")
        || text.contains("timeout")
    {
        CheckReason::GitHubNetworkFailed
    } else {
        classify_command_failure(&result)
    };
    (Measurement::new(CheckStatus::Failed, reason), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Command;

    const PRIVATE: &str = "private-provider-output-must-not-escape";
    const FIXTURE: &str = r#"import json, os, sys, time
from pathlib import Path
home, workspace, behavior, trace, pidfile = sys.argv[1:]
Path(pidfile).write_text(str(os.getpid()))
for line in sys.stdin:
    frame=json.loads(line)
    method=frame.get('method')
    with open(trace,'a') as log: log.write(method+'\n')
    if method == 'initialized': continue
    rid=frame['id']
    if behavior == 'timeout': time.sleep(30)
    if method == 'initialize': result={'codexHome':home,'userAgent':'fixture'}
    elif method == 'account/read':
        result={'account':{'type':'apiKey'} if behavior=='api' else
            {'type':'chatgpt','email':'wrong@example.test' if behavior=='wrong' else 'selected@example.test','planType':'pro'}}
    elif method == 'config/read':
        if behavior == 'unsupported':
            print(json.dumps({'id':rid,'error':{'code':-32601,'message':'private-provider-output-must-not-escape'}}),flush=True)
            continue
        implicit=behavior=='implicit'
        result={'config':{'sandbox_mode':'read-only' if implicit else 'workspace-write',
            'approval_policy':'never','sandbox_workspace_write':{'network_access':True},
            'private_unrelated':'private-provider-output-must-not-escape'},
            'origins':{} if implicit else {'sandbox_mode':{'name':{'type':'user'},'version':'v2' if behavior=='policy_change' and rid==7 else 'v1'}}}
        if behavior=='policy_data_change' and rid==7:
            result['config']['sandbox_workspace_write']['writable_roots']=['/changed-scope']
    elif method == 'command/exec':
        params=frame['params']
        assert 'sandboxPolicy' not in params and 'permissionProfile' not in params
        assert 'disableTimeout' not in params and 'disableOutputCap' not in params
        assert params['cwd']==workspace and params['timeoutMs']>0 and params['outputBytesCap']>0
        command=params['command']
        if command[0]=='gh':
            assert params['env']=={'GH_PROMPT_DISABLED':'1','GIT_TERMINAL_PROMPT':'0'}
            assert command[2:4]==['--hostname','github.com']
            result={'exitCode':0,'stdout':'false\n','stderr':''}
        elif behavior=='setup':
            print(json.dumps({'id':rid,'error':{'code':-32000,'message':'error building bubblewrap command: app-server socket directory must be a user-owned directory with mode 0700; private-provider-output-must-not-escape'}}),flush=True)
            continue
        elif behavior in ('implicit','write_fail'):
            result={'exitCode':1,'stdout':'','stderr':'Read-only file system; private-provider-output-must-not-escape'}
        else:
            assert command[1]=='internal-workspace-probe' and command[2]==workspace
            nonce=command[command.index('--nonce')+1]
            receipt={'nonce':nonce,'created':True,'read_match':True,'removed':True,'status':'verified','cleanup':'removed'}
            result={'exitCode':0,'stdout':json.dumps(receipt),'stderr':''}
    else: raise AssertionError(method)
    print(json.dumps({'id':rid,'result':result}),flush=True)
"#;

    async fn fixture(behavior: &str, timeout: std::time::Duration) -> (CheckReport, String, i32) {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        let script = root.path().join("provider.py");
        let trace = root.path().join("trace");
        let pidfile = root.path().join("pid");
        std::fs::write(&script, FIXTURE).unwrap();
        let mut process = Command::new("/usr/bin/python3");
        process.args([
            script.as_os_str(),
            home.as_os_str(),
            workspace.as_os_str(),
            std::ffi::OsStr::new(behavior),
            trace.as_os_str(),
            pidfile.as_os_str(),
        ]);
        process
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        let mut session = Session::spawn(process).await.unwrap();
        let request = CheckRequest {
            account: "codex/system".parse().unwrap(),
            workspace: workspace.clone(),
            expected_email: "selected@example.test".into(),
            write: true,
            github_repository: Some("owner/repo".into()),
        };
        let mut report = CheckReport::new(&request);
        let checked = tokio::time::timeout(
            timeout,
            exchange(
                &mut session,
                &request,
                Path::new("/matched/cagents"),
                &home,
                directory_identity(&home).unwrap(),
                &workspace,
                directory_identity(&workspace).unwrap(),
                &mut report,
            ),
        )
        .await;
        match checked {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => report.invalidate_measurements(reason),
            Err(_) => report.invalidate_measurements(CheckReason::ProviderTimeout),
        }
        session.close().await;
        let trace = std::fs::read_to_string(trace).unwrap_or_default();
        let pid = std::fs::read_to_string(pidfile).unwrap().parse().unwrap();
        assert!(!serde_json::to_string(&report).unwrap().contains(PRIVATE));
        (report, trace, pid)
    }

    #[tokio::test]
    async fn wrong_login_and_api_authentication_never_execute_measurements() {
        for (behavior, reason) in [
            ("wrong", CheckReason::IdentityMismatch),
            ("api", CheckReason::NotSubscription),
        ] {
            let (report, trace, _) = fixture(behavior, std::time::Duration::from_secs(3)).await;
            assert_eq!(
                report.authentication,
                Measurement::new(CheckStatus::Failed, reason)
            );
            assert!(!trace.contains("config/read"));
            assert!(!trace.contains("command/exec"));
            assert_ne!(report.workspace_write.status, CheckStatus::Passed);
        }
    }

    #[tokio::test]
    async fn absent_configuration_and_declared_workspace_write_cannot_hide_a_failed_write() {
        for behavior in ["implicit", "write_fail"] {
            let (report, trace, _) = fixture(behavior, std::time::Duration::from_secs(3)).await;
            assert_eq!(report.authentication.status, CheckStatus::Passed);
            assert_eq!(
                report.workspace_write,
                Measurement::new(CheckStatus::Failed, CheckReason::SandboxDenied)
            );
            assert_eq!(
                report.execution_profile_configured.status,
                if behavior == "implicit" {
                    CheckStatus::Unknown
                } else {
                    CheckStatus::Passed
                }
            );
            assert_eq!(
                report.profile.unwrap().approval_policy,
                ApprovalPolicy::Never
            );
            assert!(trace.contains("command/exec"));
        }
    }

    #[tokio::test]
    async fn measured_success_keeps_repository_read_distinct_from_push_and_changes_invalidate() {
        let (report, trace, _) = fixture("success", std::time::Duration::from_secs(3)).await;
        assert_eq!(report.workspace_write.status, CheckStatus::Passed);
        assert_eq!(
            report.github,
            Measurement::new(CheckStatus::Passed, CheckReason::GitHubRepositoryAccess)
        );
        assert_eq!(report.github_push_allowed, Some(false));
        assert_eq!(trace.matches("account/read").count(), 2);
        assert_eq!(trace.matches("config/read").count(), 2);
        for behavior in ["policy_change", "policy_data_change"] {
            let (changed, _, _) = fixture(behavior, std::time::Duration::from_secs(3)).await;
            assert_eq!(
                changed.workspace_write,
                Measurement::unknown(CheckReason::PolicyChanged)
            );
            assert_eq!(
                changed.github,
                Measurement::unknown(CheckReason::PolicyChanged)
            );
        }
    }

    #[tokio::test]
    async fn unsupported_methods_and_sandbox_setup_failure_have_fixed_safe_reasons() {
        let (report, trace, _) = fixture("unsupported", std::time::Duration::from_secs(3)).await;
        assert_eq!(
            report.execution_profile_configured,
            Measurement::new(CheckStatus::Unsupported, CheckReason::UnsupportedMethod)
        );
        assert!(!trace.contains("command/exec"));
        let (report, _, _) = fixture("setup", std::time::Duration::from_secs(3)).await;
        assert_eq!(
            report.workspace_write,
            Measurement::new(CheckStatus::Failed, CheckReason::SandboxSetupFailed)
        );
    }

    #[tokio::test]
    async fn cancelled_exchange_closes_and_reaps_only_its_own_provider() {
        let (report, _, pid) = fixture("timeout", std::time::Duration::from_millis(150)).await;
        assert_eq!(
            report.workspace_write,
            Measurement::unknown(CheckReason::ProviderTimeout)
        );
        // Session::close reaps the retained child; no unknown process is signalled.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    #[test]
    fn policy_origin_uses_current_official_tags_without_exposing_config_values() {
        for tag in [
            "mdm",
            "enterpriseManaged",
            "legacyManagedConfigTomlFromMdm",
            "user",
            "project",
            "system",
            "sessionFlags",
            "legacyManagedConfigTomlFromFile",
        ] {
            let (_,configured) = policy(&json!({"config":{"sandbox_mode":"workspace-write","approval_policy":"never"},
                "origins":{"sandbox_mode":{"name":{"type":tag,"private":"private-data"},"version":"1"}}})).unwrap();
            assert_eq!(configured, Some(true), "{tag}");
        }
        for (origin, expected) in [
            (Value::Null, Some(false)),
            (json!({"name":{"type":"packagedDefaults"}}), Some(false)),
            (json!({"name":{"type":"futureUnknown"}}), None),
        ] {
            let origins = if origin.is_null() {
                json!({})
            } else {
                json!({"sandbox_mode":origin})
            };
            let (_, configured) =
                policy(&json!({"config":{"sandbox_mode":"read-only"},"origins":origins})).unwrap();
            assert_eq!(configured, expected);
        }
    }

    #[test]
    fn missing_or_uncorrelated_workspace_receipt_never_confirms_cleanup() {
        let nonce = uuid::Uuid::new_v4();
        for output in [
            "".to_owned(),
            "truncated private output".into(),
            json!({
                "nonce":uuid::Uuid::new_v4(),"created":true,"read_match":true,"removed":true,
                "status":"verified","cleanup":"removed"
            })
            .to_string(),
        ] {
            let (measurement, receipt) =
                write_measurement(Ok(json!({"exitCode":0,"stdout":output,"stderr":""})), nonce);
            assert_ne!(measurement.status, CheckStatus::Passed);
            assert!(receipt.is_none());
        }
        let request = CheckRequest {
            account: "codex/system".parse().unwrap(),
            workspace: "/workspace".into(),
            expected_email: "selected@example.test".into(),
            write: true,
            github_repository: None,
        };
        let report = CheckReport::new(&request);
        assert_eq!(
            report.workspace_cleanup,
            Some(crate::account::workspace_probe::Cleanup::Unconfirmed)
        );
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;

    #[tokio::test]
    async fn replaced_host_captures_refuse_before_provider_preparation() {
        for replace_home in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().join("home");
            let workspace = root.path().join("workspace");
            std::fs::create_dir(&home).unwrap();
            std::fs::create_dir(&workspace).unwrap();
            let home_tuple = directory_identity(&home).unwrap();
            let workspace_tuple = directory_identity(&workspace).unwrap();
            let expected_home = PhysicalIdentity {
                device: home_tuple.0,
                inode: home_tuple.1,
            };
            let expected_workspace = PhysicalIdentity {
                device: workspace_tuple.0,
                inode: workspace_tuple.1,
            };
            let selected = if replace_home { &home } else { &workspace };
            // Preserve the old directory so its inode cannot be recycled for
            // the replacement, exactly as the host's held descriptor does.
            std::fs::rename(selected, root.path().join("retired")).unwrap();
            std::fs::create_dir(selected).unwrap();
            let account = Account::linked(Provider::Codex, uuid::Uuid::new_v4(), home);
            let request = CheckRequest {
                account: account.id(),
                workspace: workspace.clone(),
                expected_email: "selected@example.test".into(),
                write: true,
                github_repository: None,
            };
            let report = FORBID_PROVIDER_IO
                .scope(
                    (),
                    run(
                        account,
                        workspace,
                        request,
                        PathBuf::from("/not-invoked/cagents"),
                        expected_home,
                        expected_workspace,
                        None,
                    ),
                )
                .await;
            let reason = if replace_home {
                CheckReason::HomeUnavailable
            } else {
                CheckReason::WorkspaceChanged
            };
            assert_eq!(
                report.authentication,
                Measurement::new(CheckStatus::Failed, reason)
            );
            assert_eq!(report.workspace_write, Measurement::unknown(reason));
            assert!(report.provider_executable.is_none());
            assert!(report.observed_identity.is_none());
        }
    }
}
