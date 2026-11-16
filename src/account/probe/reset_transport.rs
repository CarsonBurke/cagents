//! The replaceable Claude reset transport. Claude Code 2.1.296 exposes no
//! reset control request or dynamic plan name, so this adapter uses its
//! private OAuth usage/reset and identity-verified profile endpoints.
//! Credentials are read only into this adapter and sent over curl's stdin,
//! never its arguments, logs, cache, or a response to the caller. We never
//! refresh or write credentials. The existing CLI probe owns token refresh.
use super::*;
use std::os::unix::fs::MetadataExt;

const BASE: &str = "https://api.anthropic.com";
const FILE_LIMIT: u64 = 256 * 1024;

pub(super) struct Client {
    base: String,
    bearer: String,
    organization: String,
    version: String,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
    home_identity: Option<(u64, u64)>,
    fixture: bool,
}

impl Client {
    pub(super) async fn load(
        account: &Account,
        program: &Path,
        search_path: Option<&OsStr>,
    ) -> Result<Self, String> {
        let home = account
            .resolved_home()
            .ok_or("This account has no provider home")?;
        // A fixture marker always takes precedence over credential discovery,
        // including when malformed: real credentials can never reach a fake.
        let fixture_path = home.join("fixture-reset-api.json");
        if tokio::fs::symlink_metadata(&fixture_path).await.is_ok() {
            return fixture_client(&home, &fixture_path).await;
        }
        // A fixture missing its local service must not fall back to production.
        if fixture_root(&home).await {
            return Err("The reset fixture service is unavailable".into());
        }
        // API-key, cloud and externally injected logins have no subscription
        // grants belonging to this on-disk login. Never mix those identities.
        if account.credential_overrides().is_empty()
            && super::super::credential_overrides(Provider::Claude)
                .iter()
                .any(|name| std::env::var_os(name).is_some())
        {
            return Err("Reset credits require the account's stored subscription login".into());
        }
        let stored = read_json(&home.join(".credentials.json")).await?;
        let oauth = &stored["claudeAiOauth"];
        if !oauth["scopes"]
            .as_array()
            .is_some_and(|scopes| scopes.iter().any(|scope| scope == "user:profile"))
        {
            return Err("Claude's login does not allow reading reset credits".into());
        }
        let bearer = oauth["accessToken"]
            .as_str()
            .filter(|token| {
                !token.is_empty()
                    && token.len() <= 16 * 1024
                    && token.bytes().all(|b| {
                        b.is_ascii_alphanumeric()
                            || matches!(b, b'-' | b'_' | b'.' | b'~' | b'+' | b'/' | b'=')
                    })
            })
            .ok_or("Claude's subscription login is unavailable")?
            .to_owned();
        if oauth["expiresAt"]
            .as_u64()
            .is_some_and(|expires| expires <= now_ms())
        {
            return Err("Claude's subscription login needs refreshing".into());
        }
        let config = match account.launch_home() {
            Some(home) => home.join(".claude.json"),
            None => PathBuf::from(std::env::var_os("HOME").ok_or("The user home is unavailable")?)
                .join(".claude.json"),
        };
        let config = read_json(&config).await?;
        let organization = config["oauthAccount"]["organizationUuid"]
            .as_str()
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .ok_or("Claude's account organization is unavailable")?
            .to_string();
        let mut version = command_with_path(account, program, search_path);
        version.arg("--version");
        let output = tokio::time::timeout(Duration::from_secs(5), async {
            let mut group = Group::spawn(version).await?;
            let mut output = Vec::new();
            let read = group.stdout()?.take(4096).read_to_end(&mut output).await;
            group.finish().await;
            read.map_err(|_| "Claude's version could not be read".to_owned())?;
            Ok::<_, String>(output)
        })
        .await
        .map_err(|_| "Claude's version did not answer in time")??;
        let version = std::str::from_utf8(&output)
            .ok()
            .and_then(|s| s.split_whitespace().next())
            .filter(|s| {
                !s.is_empty()
                    && s.len() < 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-'))
            })
            .ok_or("Claude's version could not be read")?
            .to_owned();
        Ok(Self {
            base: BASE.into(),
            bearer,
            organization,
            version,
            subscription_type: oauth["subscriptionType"].as_str().map(str::to_owned),
            rate_limit_tier: oauth["rateLimitTier"].as_str().map(str::to_owned),
            home_identity: tokio::fs::metadata(&home)
                .await
                .ok()
                .map(|metadata| (metadata.dev(), metadata.ino())),
            fixture: false,
        })
    }

    pub(super) async fn inventory(&self) -> Result<Vec<super::super::UsageReset>, String> {
        let grants = self
            .request("GET", "/api/oauth/usage?cedar_ember=1&skip_spend=1", None)
            .await;
        let sessions = self
            .request("GET", "/api/oauth/usage?at_wall=1&skip_spend=1", None)
            .await;
        let mut body = grants?;
        let sessions = sessions?;
        if !body.is_object()
            || !sessions.is_object()
            || ![&body, &sessions]
                .iter()
                .any(|body| body.get("cedar_ember").is_some() || body.get("juniper_tide").is_some())
        {
            return Err("Claude reset availability could not be read".into());
        }
        let now = now_ms() / 1000;
        // Claude Code asks at_wall for rejected-limit offers and uses the
        // cedar_ember query for its anytime command. These are separate offer
        // contexts, so an ineligible anytime block cannot veto a usable wall
        // offer. Select the complete block, keeping its grant order/counts.
        let wall_offer = sessions["cedar_ember"]["eligible"].as_bool() == Some(true)
            && sessions["cedar_ember"]["at_limit"].as_bool() == Some(true)
            && super::super::claude::parse_resets(
                &json!({"cedar_ember":sessions["cedar_ember"]}),
                now,
            )
            .iter()
            .any(|reset| reset.is_available(now as i64));
        if wall_offer || !body["cedar_ember"].is_object() {
            body["cedar_ember"] = sessions["cedar_ember"].clone();
        }
        if sessions.get("juniper_tide").is_some() {
            body["juniper_tide"] = sessions["juniper_tide"].clone();
        }
        Ok(super::super::claude::parse_resets(&body, now_ms() / 1000))
    }

    /// Verify the frozen bearer rather than trusting separate CLI metadata.
    pub(super) async fn verify_identity(&self, expected: &Identity) -> Result<(), String> {
        self.verified_profile(expected).await.map(|_| ())
    }

    /// Optional display metadata is fetched once per login, separately from
    /// the usage cadence. Consume verification never uses this display cache.
    pub(super) async fn plan(
        &self,
        expected: &Identity,
        reported: &super::super::claude::PlanMetadata,
        cache: Option<&Cache>,
        account_key: Option<&str>,
    ) -> Result<super::super::claude::PlanMetadata, String> {
        let Some((cache, key)) = cache.zip(account_key) else {
            return self
                .read_plan_profile(expected)
                .await
                .map(|profile| profile.or(self.plan_fallback(expected, reported)));
        };
        let binding = self.plan_binding(key, expected);
        let cache = cache.clone();
        let key = key.to_owned();
        // This per-account flight lock spans HTTP; the shared document lock
        // is held only during small reads/writes, never across provider I/O.
        let lock_cache = cache.clone();
        let lock_key = key.clone();
        let _flight = tokio::task::spawn_blocking(move || lock_cache.lock_plan(&lock_key))
            .await
            .map_err(|_| "The profile cache could not be opened")?
            .map_err(|_| "The profile cache could not be opened")?;
        let read_cache = cache.clone();
        let read_key = key.clone();
        let read_binding = binding.clone();
        let (epoch, profile) =
            tokio::task::spawn_blocking(move || read_cache.plan_profile(&read_key, &read_binding))
                .await
                .map_err(|_| "The profile cache could not be read")?;
        if let Some(profile) = profile {
            return self.cached_plan(profile, expected, reported);
        }
        // Persist the attempted lookup before sending. Crashes and failures
        // must not turn regular usage probes into repeated profile requests.
        let pending = super::super::cache::PlanProfile::new(binding.clone(), false, None, None);
        if !publish_plan(&cache, &key, epoch, pending).await? {
            return Err("This account's login changed; refresh and confirm again".into());
        }
        let result = self.read_plan_profile(expected).await;
        let profile = match &result {
            Ok(plan) => super::super::cache::PlanProfile::new(
                binding,
                true,
                plan.display_name.clone(),
                plan.rate_limit_tier.clone(),
            ),
            Err(_) => super::super::cache::PlanProfile::new(binding, false, None, None),
        };
        if !publish_plan(&cache, &key, epoch, profile).await? {
            return Err("This account's login changed; refresh and confirm again".into());
        }
        result.map(|profile| profile.or(self.plan_fallback(expected, reported)))
    }

    fn plan_binding(&self, key: &str, expected: &Identity) -> String {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        for value in [
            key.to_owned(),
            expected
                .email
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase(),
            self.organization.clone(),
            expected
                .organization
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase(),
        ] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        if let Some((device, inode)) = self.home_identity {
            digest.update(device.to_be_bytes());
            digest.update(inode.to_be_bytes());
        }
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn cached_plan(
        &self,
        profile: super::super::cache::PlanProfile,
        expected: &Identity,
        reported: &super::super::claude::PlanMetadata,
    ) -> Result<super::super::claude::PlanMetadata, String> {
        if !profile.verified {
            return Err("Claude profile metadata is unavailable for this login".into());
        }
        let mut plan = self.plan_fallback(expected, reported);
        plan.display_name = profile.display_name;
        // A missing profile tier may still be reported by the current CLI.
        plan.rate_limit_tier = profile.rate_limit_tier.or(plan.rate_limit_tier);
        Ok(plan)
    }

    fn plan_fallback(
        &self,
        expected: &Identity,
        reported: &super::super::claude::PlanMetadata,
    ) -> super::super::claude::PlanMetadata {
        super::super::claude::PlanMetadata {
            subscription_type: reported
                .subscription_type
                .clone()
                .or(self.subscription_type.clone())
                .or(expected.plan.clone()),
            display_name: None,
            rate_limit_tier: reported
                .rate_limit_tier
                .clone()
                .or(self.rate_limit_tier.clone()),
        }
    }

    async fn read_plan_profile(
        &self,
        expected: &Identity,
    ) -> Result<super::super::claude::PlanMetadata, String> {
        let profile = self.verified_profile(expected).await?;
        // No organization_type or cached oauthAccount display-name fallback:
        // a verified profile name is literal; its absence uses the CLI type.
        Ok(super::super::claude::plan_metadata(&json!({
            "plan_display_name": profile["organization"]["plan_display_name"],
            "rate_limit_tier": profile["organization"]["rate_limit_tier"],
        })))
    }

    async fn verified_profile(&self, expected: &Identity) -> Result<Value, String> {
        let profile = self.request("GET", "/api/oauth/profile", None).await?;
        self.verify_profile(&profile, expected)?;
        Ok(profile)
    }

    fn verify_profile(&self, profile: &Value, expected: &Identity) -> Result<(), String> {
        let email = profile["account"]["email"]
            .as_str()
            .or_else(|| profile["account"]["email_address"].as_str());
        if email
            .zip(expected.email.as_deref())
            .is_none_or(|(actual, expected)| {
                actual.trim().is_empty() || !actual.trim().eq_ignore_ascii_case(expected.trim())
            })
            || profile["organization"]["uuid"].as_str() != Some(self.organization.as_str())
            || expected
                .organization
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .is_some_and(|expected| {
                    profile["organization"]["name"]
                        .as_str()
                        .is_none_or(|actual| !actual.trim().eq_ignore_ascii_case(expected.trim()))
                })
        {
            return Err("This account's login changed; refresh and confirm again".into());
        }
        Ok(())
    }

    pub(super) async fn consume(&self, reset_id: &str, key: &str) -> Result<Value, String> {
        let body = if let Some(grant) = reset_id.strip_prefix("cedar_ember/") {
            if !super::super::claude::valid_grant_id(grant) {
                return Err("Invalid reset selection".into());
            }
            json!({"program":"cedar_ember", "grant_id":grant, "request_id":key})
        } else if reset_id == "juniper_tide" {
            // This API has no server idempotency contract and cannot be made
            // idempotent by cagents. The local journal only prevents sending
            // this operation twice; recovery rereads usage without another POST.
            json!({"program":"juniper_tide"})
        } else {
            return Err("This reset is no longer available".into());
        };
        self.request(
            "POST",
            &format!("/api/organizations/{}/reset_rate_limits", self.organization),
            Some(body),
        )
        .await
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let mut command = Command::new("/usr/bin/curl");
        command
            .args(["-q", "--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        // Config text and all secrets travel in a pipe, never /proc argv.
        let mut config = format!(
            r#"silent
connect-timeout = 5
max-time = 10
max-filesize = {}
retry = 0
max-redirs = 0
proto = "={}"
request = "{}"
url = "{}{}"
header = "Authorization: Bearer {}"
header = "Content-Type: application/json"
header = "anthropic-beta: oauth-2025-04-20"
header = "User-Agent: claude-code/{}"
write-out = "\n%{{http_code}}"
"#,
            OUTPUT_LIMIT,
            if self.fixture { "http" } else { "https" },
            method,
            self.base,
            path,
            self.bearer,
            self.version
        );
        if self.fixture {
            config.push_str("noproxy = \"*\"\n");
        }
        if let Some(body) = body {
            let body = body.to_string().replace('\\', "\\\\").replace('"', "\\\"");
            config.push_str(&format!("data = \"{body}\"\n"));
        }
        let mut group = Group::spawn(command).await?;
        let mut stdin = group
            .child
            .stdin
            .take()
            .ok_or("Reset transport input is unavailable")?;
        let mut bytes = Vec::new();
        let result = async {
            stdin
                .write_all(config.as_bytes())
                .await
                .map_err(|_| "Reset request could not be sent")?;
            stdin
                .shutdown()
                .await
                .map_err(|_| "Reset request could not be sent")?;
            drop(stdin);
            group
                .stdout()?
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| "Reset response could not be read")?;
            let split = bytes
                .iter()
                .rposition(|b| *b == b'\n')
                .ok_or("Reset response was incomplete")?;
            let (body, suffix) = bytes.split_at(split);
            let code = &suffix[1..];
            if code.len() != 3 || code[0] != b'2' {
                return Err("Claude could not complete the reset request".into());
            }
            serde_json::from_slice(body)
                .map_err(|_| "Claude returned an unreadable reset response".into())
        }
        .await;
        group.finish().await;
        result
    }
}

/// A missing or unreadable-format file-store login is an observed login
/// transition. Expiry and unrelated transport/version failures are not.
/// Called only after Client::load failed; no provider request is performed.
pub(super) async fn login_was_removed(account: &Account) -> bool {
    if account.provider != Provider::Claude {
        return false;
    }
    let Some(home) = account.resolved_home() else {
        return false;
    };
    // A malformed marker or stopped fixture must never inspect credentials.
    // The ancestor check also protects a fixture whose provider home vanished.
    if tokio::fs::symlink_metadata(home.join("fixture-reset-api.json"))
        .await
        .is_ok()
        || fixture_root(&home).await
        || home
            .ancestors()
            .any(|path| path.join(".cagents-reset-fixture").is_file())
    {
        return false;
    }
    let file = match tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(home.join(".credentials.json"))
        .await
    {
        Ok(file) => file,
        Err(error) => return error.kind() == io::ErrorKind::NotFound,
    };
    match file.metadata().await {
        Ok(metadata) if metadata.is_file() => {}
        _ => return false,
    }
    let mut bytes = Vec::new();
    if file
        .take(FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .is_err()
    {
        return false;
    }
    if bytes.len() as u64 > FILE_LIMIT {
        return true;
    }
    let Ok(stored) = serde_json::from_slice::<Value>(&bytes) else {
        return true;
    };
    stored["claudeAiOauth"]["accessToken"]
        .as_str()
        .is_none_or(|token| token.trim().is_empty())
}

async fn publish_plan(
    cache: &Cache,
    key: &str,
    epoch: u64,
    profile: super::super::cache::PlanProfile,
) -> Result<bool, String> {
    let cache = cache.clone();
    let key = key.to_owned();
    tokio::task::spawn_blocking(move || cache.store_plan_profile(&key, epoch, profile))
        .await
        .map_err(|_| "The profile cache could not be written".to_owned())?
        .map_err(|_| "The profile cache could not be written".into())
}

async fn read_json(path: &Path) -> Result<Value, String> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| "Claude's account metadata is unavailable")?;
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "Claude's account metadata could not be read")?;
    if bytes.len() as u64 > FILE_LIMIT {
        return Err("Claude's account metadata is too large".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "Claude's account metadata could not be read".into())
}

async fn fixture_root(home: &Path) -> bool {
    let Ok(home) = tokio::fs::canonicalize(home).await else {
        return false;
    };
    home.ancestors()
        .any(|path| path.join(".cagents-reset-fixture").is_file())
}

async fn fixture_client(home: &Path, marker: &Path) -> Result<Client, String> {
    if !cfg!(debug_assertions) || !fixture_root(home).await {
        return Err("Reset fixtures require an isolated development fixture".into());
    }
    let value = read_json(marker).await?;
    let home = tokio::fs::canonicalize(home)
        .await
        .map_err(|_| "Fixture home is unavailable")?;
    // The account's actual default store can never become a fixture target.
    let real_home = passwd_home();
    if real_home
        .as_ref()
        .is_none_or(|real| home == real.join(".claude"))
        || value["fixture"] != true
    {
        return Err("Reset fixtures cannot use the real provider home".into());
    }
    let base = value["base_url"]
        .as_str()
        .ok_or("Fixture URL is unavailable")?;
    let port = base
        .strip_prefix("http://127.0.0.1:")
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|port| *port > 0)
        .ok_or("Reset fixtures require an IPv4 loopback URL")?;
    Ok(Client {
        base: format!("http://127.0.0.1:{port}"),
        bearer: "cagents-fixture-only".into(),
        organization: "00000000-0000-4000-8000-000000000001".into(),
        version: "0.0.0-fixture".into(),
        subscription_type: value["subscription_type"].as_str().map(str::to_owned),
        rate_limit_tier: value["rate_limit_tier"].as_str().map(str::to_owned),
        home_identity: tokio::fs::metadata(&home)
            .await
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino())),
        fixture: true,
    })
}

fn passwd_home() -> Option<PathBuf> {
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0_u8; 16 * 1024];
    // SAFETY: the reentrant function initializes entry and points only into
    // this live buffer; copy the directory before either leaves scope.
    unsafe {
        if libc::getpwuid_r(
            libc::getuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        ) != 0
            || result.is_null()
        {
            return None;
        }
        Some(PathBuf::from(OsString::from_vec(
            std::ffi::CStr::from_ptr((*result).pw_dir)
                .to_bytes()
                .to_vec(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixture_marker_never_loads_real_credentials_or_accepts_nonloopback_urls() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".cagents-reset-fixture"), "fixture").unwrap();
        let home = root.path().join("provider");
        std::fs::create_dir(&home).unwrap();
        let marker = home.join("fixture-reset-api.json");
        // A poisoned credential file must be irrelevant on every marker path.
        std::fs::write(home.join(".credentials.json"), "must never be parsed").unwrap();
        let account = Account::linked(Provider::Claude, uuid::Uuid::new_v4(), home.clone());
        for base in [
            "https://api.anthropic.com",
            "http://localhost:1234",
            "http://127.0.0.1:1234/path",
            "http://127.0.0.1:1234@evil.test",
        ] {
            std::fs::write(&marker, json!({"base_url":base,"fixture":true}).to_string()).unwrap();
            assert!(
                Client::load(&account, Path::new("/missing/claude"), None)
                    .await
                    .is_err()
            );
        }
        std::fs::write(
            &marker,
            json!({"base_url":"http://127.0.0.1:1234","fixture":true}).to_string(),
        )
        .unwrap();
        let client = Client::load(&account, Path::new("/missing/claude"), None)
            .await
            .unwrap();
        assert_eq!(client.bearer, "cagents-fixture-only");
        assert_eq!(client.subscription_type, None);
        assert_eq!(client.rate_limit_tier, None);
        std::fs::write(
            &marker,
            json!({"base_url":"http://127.0.0.1:1234","fixture":true,
            "subscription_type":"max","rate_limit_tier":"default_claude_max_5x",
            "plan_display_name":"Ultra","plan_email":"fixture@example.test"})
            .to_string(),
        )
        .unwrap();
        let client = Client::load(&account, Path::new("/missing/claude"), None)
            .await
            .unwrap();
        assert_eq!(client.subscription_type.as_deref(), Some("max"));
        assert_eq!(
            client.rate_limit_tier.as_deref(),
            Some("default_claude_max_5x")
        );
        std::fs::remove_file(&marker).unwrap();
        assert!(
            Client::load(&account, Path::new("/missing/claude"), None)
                .await
                .is_err(),
            "a stopped fixture cannot fall back to production"
        );
    }
    #[tokio::test]
    async fn removed_login_detection_distinguishes_absent_auth_from_expiry() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("provider");
        std::fs::create_dir(&home).unwrap();
        let account = Account::linked(Provider::Claude, uuid::Uuid::new_v4(), home.clone());
        assert!(login_was_removed(&account).await);
        let credentials = home.join(".credentials.json");
        for body in [
            "not-json",
            "{}",
            r#"{"claudeAiOauth":{}}"#,
            r#"{"claudeAiOauth":{"accessToken":null}}"#,
            r#"{"claudeAiOauth":{"accessToken":""}}"#,
        ] {
            std::fs::write(&credentials, body).unwrap();
            assert!(login_was_removed(&account).await);
        }
        // A fake stored token exists even when expired or profile scope is
        // absent. Neither state proves a sign-out, and no command is run.
        std::fs::write(
            &credentials,
            json!({"claudeAiOauth":{"accessToken":"fake-stored-token","expiresAt":0,"scopes":[]}})
                .to_string(),
        )
        .unwrap();
        std::fs::write(home.join(".claude.json"), "unreadable config").unwrap();
        assert!(!login_was_removed(&account).await);
    }

    #[tokio::test]
    async fn removed_login_detection_never_reads_fixture_credentials() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("provider");
        std::fs::create_dir(&home).unwrap();
        let account = Account::linked(Provider::Claude, uuid::Uuid::new_v4(), home.clone());
        std::fs::write(
            home.join(".credentials.json"),
            "malformed credentials must stay irrelevant",
        )
        .unwrap();
        let marker = home.join("fixture-reset-api.json");
        std::fs::write(&marker, "malformed fixture marker").unwrap();
        assert!(!login_was_removed(&account).await);
        std::fs::remove_file(marker).unwrap();
        std::fs::write(
            root.path().join(".cagents-reset-fixture"),
            "isolated fixture",
        )
        .unwrap();
        assert!(!login_was_removed(&account).await);
        std::fs::remove_file(home.join(".credentials.json")).unwrap();
        std::fs::remove_dir(home).unwrap();
        assert!(
            !login_was_removed(&account).await,
            "removed fixture home cannot fall through to real credential discovery"
        );
    }
}

#[cfg(test)]
mod offer_context_tests {
    use super::*;
    use std::io::{Read, Write};

    fn server(responses: Vec<Value>) -> (Client, std::thread::JoinHandle<()>) {
        let requests = responses
            .into_iter()
            .enumerate()
            .map(|(index, body)| {
                let path = if index == 0 {
                    "/api/oauth/usage?cedar_ember=1&skip_spend=1"
                } else {
                    "/api/oauth/usage?at_wall=1&skip_spend=1"
                };
                (path, body)
            })
            .collect();
        read_server(requests)
    }

    fn read_server(responses: Vec<(&'static str, Value)>) -> (Client, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = std::thread::spawn(move || {
            for (path, response) in responses {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        Err(error) => panic!("fake offer request did not arrive: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|end| end == b"\r\n\r\n") {
                    let mut chunk = [0_u8; 1024];
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0 && request.len() < 16 * 1024);
                    request.extend_from_slice(&chunk[..count]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with(&format!("GET {path} ")));
                assert!(request.contains("Authorization: Bearer cagents-fixture-only\r\n"));
                let body = response.to_string();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        });
        (
            Client {
                base: format!("http://127.0.0.1:{port}"),
                bearer: "cagents-fixture-only".into(),
                organization: "00000000-0000-4000-8000-000000000001".into(),
                version: "0.0.0-fixture".into(),
                subscription_type: None,
                rate_limit_tier: None,
                home_identity: Some((1, 1)),
                fixture: true,
            },
            worker,
        )
    }

    fn offer(id: &str, eligible: bool, at_limit: bool, window: &str) -> Value {
        json!({"cedar_ember":{"eligible":eligible,"at_limit":at_limit,"next_grant_id":id,
            "grants":[{"id":id,"resets_left":2,"clears":[window],"usable_now":true,"use_requires_limit":false}]}})
    }

    #[tokio::test]
    async fn different_reads_select_one_coherent_usable_offer_context() {
        let (client, server) = server(vec![
            offer("anytime", false, false, "seven_day"),
            offer("wall", true, true, "seven_day_overage_included"),
        ]);
        let resets = client.inventory().await.unwrap();
        server.join().unwrap();
        assert_eq!(resets.len(), 1);
        assert_eq!(resets[0].id, "cedar_ember/wall");
        assert_eq!(resets[0].available_count, 2);
        assert_eq!(
            resets[0].scopes,
            [super::super::super::ResetScope::Model("Fable".into())]
        );
        assert!(resets[0].is_available((now_ms() / 1000) as i64));
        let (client, server) = self::server(vec![
            offer("anytime", true, false, "seven_day"),
            offer("wall", false, true, "seven_day_overage_included"),
        ]);
        let resets = client.inventory().await.unwrap();
        server.join().unwrap();
        assert_eq!(resets.len(), 1);
        assert_eq!(resets[0].id, "cedar_ember/anytime");
        assert_eq!(
            resets[0].scopes,
            [super::super::super::ResetScope::AccountWeekly]
        );
        assert!(resets[0].is_available((now_ms() / 1000) as i64));
    }
    fn profile() -> Value {
        json!({"account":{"email":"fixture@example.test"},
            "organization":{"uuid":"00000000-0000-4000-8000-000000000001","name":"Fixture workspace"}})
    }

    fn identity() -> Identity {
        Identity {
            email: Some("fixture@example.test".into()),
            organization: Some("Fixture workspace".into()),
            plan: Some("legacy-subscription".into()),
            disputed_by: None,
        }
    }

    fn reported() -> super::super::super::claude::PlanMetadata {
        super::super::super::claude::plan_metadata(&json!({"subscription_type":"max"}))
    }

    fn named_profile(name: &str) -> Value {
        let mut value = profile();
        value["organization"]["plan_display_name"] = json!(name);
        value["organization"]["organization_type"] = json!("claude_max");
        value["organization"]["rate_limit_tier"] = json!("default_claude_max_20x");
        value
    }

    #[tokio::test]
    async fn profile_plan_is_shared_once_across_windows_and_access_token_refreshes() {
        for (profile_name, expected_label) in [
            ("Future: Ultra / 20x", "Future: Ultra / 20x"),
            ("Max", "Max 20x"),
            ("Ultra", "Ultra"),
        ] {
            let (mut client, server) =
                read_server(vec![("/api/oauth/profile", named_profile(profile_name))]);
            let directory = tempfile::tempdir().unwrap();
            let first_window = Cache::at(directory.path());
            let second_window = Cache::at(directory.path());
            let expected = identity();
            let report = reported();
            let (first, second) = tokio::join!(
                client.plan(
                    &expected,
                    &report,
                    Some(&first_window),
                    Some("claude:/home")
                ),
                client.plan(
                    &expected,
                    &report,
                    Some(&second_window),
                    Some("claude:/home")
                ),
            );
            assert_eq!(first.unwrap().name().as_deref(), Some(expected_label));
            assert_eq!(second.unwrap().name().as_deref(), Some(expected_label));
            server.join().unwrap();
            // The server is now closed. A force usage refresh or a changed access
            // token must still use this login's stored result without another GET.
            client.bearer = "cagents-fixture-refreshed-token".into();
            let result = client
                .plan(
                    &expected,
                    &report,
                    Some(&second_window),
                    Some("claude:/home"),
                )
                .await
                .unwrap();
            assert_eq!(result.name().as_deref(), Some(expected_label));
            assert_eq!(
                result.rate_limit_tier.as_deref(),
                Some("default_claude_max_20x")
            );
        }
    }

    #[tokio::test]
    async fn missing_profile_name_is_cached_and_falls_back_to_current_cli_type() {
        let mut legacy = profile();
        legacy["organization"]["organization_type"] = json!("claude_unrelated-type");
        legacy["organization"]["rate_limit_tier"] = json!("default_claude_max_20x");
        let (mut client, server) = read_server(vec![("/api/oauth/profile", legacy)]);
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        client.subscription_type = Some("stored-type".into());
        let first = client
            .plan(&identity(), &reported(), Some(&cache), Some("claude:/home"))
            .await
            .unwrap();
        assert_eq!(first.name().as_deref(), Some("max 20x"));
        assert_eq!(first.display_name, None);
        server.join().unwrap();
        client.bearer = "cagents-fixture-refreshed-token".into();
        assert_eq!(
            client
                .plan(&identity(), &reported(), Some(&cache), Some("claude:/home"))
                .await
                .unwrap()
                .name()
                .as_deref(),
            Some("max 20x"),
            "missing-name cache keeps the qualified CLI fallback without another profile read",
        );
        let next = super::super::super::claude::plan_metadata(
            &json!({"subscription_type":"future-subscription"}),
        );
        assert_eq!(
            client
                .plan(&identity(), &next, Some(&cache), Some("claude:/home"))
                .await
                .unwrap()
                .name()
                .as_deref(),
            Some("future-subscription")
        );
    }

    #[tokio::test]
    async fn identity_home_and_explicit_login_changes_refresh_profile() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let key = "claude:/home";
        let expected = identity();
        let (client, server) = read_server(vec![("/api/oauth/profile", named_profile("Original"))]);
        client
            .plan(&expected, &reported(), Some(&cache), Some(key))
            .await
            .unwrap();
        server.join().unwrap();
        let mut changed = named_profile("Changed login");
        changed["account"]["email"] = json!("changed@example.test");
        let (client, server) = read_server(vec![("/api/oauth/profile", changed)]);
        let mut changed_identity = identity();
        changed_identity.email = Some("changed@example.test".into());
        assert_eq!(
            client
                .plan(&changed_identity, &reported(), Some(&cache), Some(key))
                .await
                .unwrap()
                .name()
                .as_deref(),
            Some("Changed login")
        );
        server.join().unwrap();
        let mut replacement = named_profile("Recreated home");
        replacement["account"]["email"] = json!("changed@example.test");
        let (mut client, server) = read_server(vec![("/api/oauth/profile", replacement)]);
        client.home_identity = Some((1, 2));
        assert_eq!(
            client
                .plan(&changed_identity, &reported(), Some(&cache), Some(key))
                .await
                .unwrap()
                .name()
                .as_deref(),
            Some("Recreated home")
        );
        server.join().unwrap();
        cache
            .settle(
                key,
                Some(super::super::super::cache::Settled::SignedIn { at: 1 }),
            )
            .unwrap();
        let mut relogin = named_profile("Same identity new sign-in");
        relogin["account"]["email"] = json!("changed@example.test");
        let (mut client, server) = read_server(vec![("/api/oauth/profile", relogin)]);
        client.home_identity = Some((1, 2));
        assert_eq!(
            client
                .plan(&changed_identity, &reported(), Some(&cache), Some(key))
                .await
                .unwrap()
                .name()
                .as_deref(),
            Some("Same identity new sign-in")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn failed_or_mismatched_profile_is_not_polled_or_replaced_by_stale_name() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let key = "claude:/home";
        let (client, server) = read_server(vec![("/api/oauth/profile", named_profile("Original"))]);
        client
            .plan(&identity(), &reported(), Some(&cache), Some(key))
            .await
            .unwrap();
        server.join().unwrap();
        let mut changed = identity();
        changed.email = Some("changed@example.test".into());
        let (client, server) =
            read_server(vec![("/api/oauth/profile", named_profile("Wrong login"))]);
        assert!(
            client
                .plan(&changed, &reported(), Some(&cache), Some(key))
                .await
                .is_err()
        );
        server.join().unwrap();
        assert!(
            client
                .plan(&changed, &reported(), Some(&cache), Some(key))
                .await
                .is_err(),
            "negative result prevents repeated profile requests"
        );
        let binding = client.plan_binding(key, &changed);
        let (_, profile) = cache.plan_profile(key, &binding);
        let profile = profile.unwrap();
        assert!(!profile.verified);
        assert_eq!(profile.display_name, None);
        assert_eq!(profile.rate_limit_tier, None);
        // Consumption still checks a fresh profile even when display metadata
        // is cached. It must never trust this negative or positive record.
        let (client, server) =
            read_server(vec![("/api/oauth/profile", named_profile("Wrong login"))]);
        assert!(client.verify_identity(&changed).await.is_err());
        server.join().unwrap();
    }
    #[tokio::test]
    async fn profile_metadata_and_consume_verification_require_all_identity_fields() {
        for field in ["email", "organization", "name"] {
            let mut body = named_profile("Unrelated profile");
            match field {
                "email" => body["account"]["email"] = json!("other@example.test"),
                "organization" => {
                    body["organization"]["uuid"] = json!("00000000-0000-4000-8000-000000000002")
                }
                _ => body["organization"]["name"] = json!("Other workspace"),
            }
            let (client, server) = read_server(vec![
                ("/api/oauth/profile", body.clone()),
                ("/api/oauth/profile", body),
            ]);
            assert!(
                client
                    .plan(&identity(), &reported(), None, None)
                    .await
                    .is_err(),
                "{field}"
            );
            assert!(
                client.verify_identity(&identity()).await.is_err(),
                "{field}"
            );
            server.join().unwrap();
        }
    }
}
