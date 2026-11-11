//! The replaceable Claude reset transport. Claude Code 2.1.296 exposes no
//! reset control request or dynamic plan name, so this adapter uses its
//! private OAuth usage/reset and identity-verified profile endpoints.
//! Credentials are read only into this adapter and sent over curl's stdin,
//! never its arguments, logs, cache, or a response to the caller. We never
//! refresh or write credentials. The existing CLI probe owns token refresh.
use super::*;

const BASE: &str = "https://api.anthropic.com";
const FILE_LIMIT: u64 = 256 * 1024;

pub(super) struct Client {
    base: String,
    bearer: String,
    organization: String,
    version: String,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
    plan_display_name: Option<String>,
    plan_email: Option<String>,
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
            plan_display_name: config["oauthAccount"]["planDisplayName"]
                .as_str()
                .map(str::to_owned),
            plan_email: config["oauthAccount"]["emailAddress"]
                .as_str()
                .map(str::to_owned),
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

    /// Display metadata belongs to the same verified login as the usage probe.
    /// Missing profile fields retain the subscription type the CLI knows.
    pub(super) async fn plan(
        &self,
        expected: &Identity,
        reported: &super::super::claude::PlanMetadata,
    ) -> Result<Option<String>, String> {
        let profile = self.verified_profile(expected).await?;
        let organization = &profile["organization"];
        // Only these profile fields can become plan metadata. Identity and
        // unrelated profile content stay inside this credential adapter.
        let mut profile = super::super::claude::plan_metadata(&json!({
            "subscription_type": organization["organization_type"].as_str()
                .and_then(|kind| kind.strip_prefix("claude_")),
            "plan_display_name": organization["plan_display_name"],
            "rate_limit_tier": organization["rate_limit_tier"],
        }));
        // A CLI-reported subscription outranks the profile's legacy org type;
        // the profile's explicit display name and current tier remain primary.
        profile.subscription_type = reported
            .subscription_type
            .clone()
            .or(profile.subscription_type);
        // The config cache is independent of the frozen bearer. Its display
        // name is only a fallback after profile verification, for this email.
        let same_owner = self
            .plan_email
            .as_deref()
            .zip(expected.email.as_deref())
            .is_some_and(|(cached, expected)| {
                !cached.trim().is_empty() && cached.trim().eq_ignore_ascii_case(expected.trim())
            });
        let cached = super::super::claude::plan_metadata(&json!({
            "subscription_type": self.subscription_type,
            "plan_display_name": same_owner.then_some(self.plan_display_name.as_deref()).flatten(),
            "rate_limit_tier": self.rate_limit_tier,
        }));
        let fallback = super::super::claude::PlanMetadata {
            subscription_type: expected.plan.clone(),
            ..Default::default()
        };
        Ok(profile.or(reported.clone()).or(cached).or(fallback).name())
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
            // No request-id contract exists for this program. The caller
            // journals it before sending and never submits this key twice.
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
        plan_display_name: value["plan_display_name"].as_str().map(str::to_owned),
        plan_email: value["plan_email"].as_str().map(str::to_owned),
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
        assert_eq!(client.plan_display_name, None);
        assert_eq!(client.plan_email, None);
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
        assert_eq!(client.plan_display_name.as_deref(), Some("Ultra"));
        assert_eq!(client.plan_email.as_deref(), Some("fixture@example.test"));
        std::fs::remove_file(&marker).unwrap();
        assert!(
            Client::load(&account, Path::new("/missing/claude"), None)
                .await
                .is_err(),
            "a stopped fixture cannot fall back to production"
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
                plan_display_name: None,
                plan_email: None,
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

    #[tokio::test]
    async fn plan_metadata_requires_the_same_email_organization_and_name_as_consume() {
        for field in ["email", "organization", "name"] {
            let mut profile = profile();
            match field {
                "email" => profile["account"]["email"] = json!("other@example.test"),
                "organization" => {
                    profile["organization"]["uuid"] = json!("00000000-0000-4000-8000-000000000002")
                }
                _ => profile["organization"]["name"] = json!("Other workspace"),
            }
            profile["organization"]["plan_display_name"] = json!("Unrelated plan");
            let (client, server) = read_server(vec![
                ("/api/oauth/profile", profile.clone()),
                ("/api/oauth/profile", profile),
            ]);
            assert!(
                client.plan(&identity(), &Default::default()).await.is_err(),
                "{field}"
            );
            assert!(
                client.verify_identity(&identity()).await.is_err(),
                "{field}"
            );
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn optional_plan_metadata_falls_back_to_cached_and_cli_subscription() {
        let (mut client, server) = read_server(vec![
            ("/api/oauth/profile", profile()),
            ("/api/oauth/profile", profile()),
        ]);
        assert_eq!(
            client
                .plan(&identity(), &Default::default())
                .await
                .unwrap()
                .as_deref(),
            Some("legacy-subscription")
        );
        client.subscription_type = Some("future-subscription".into());
        assert_eq!(
            client
                .plan(&identity(), &Default::default())
                .await
                .unwrap()
                .as_deref(),
            Some("future-subscription")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn profile_display_name_and_tier_override_cached_legacy_metadata() {
        let mut explicit = profile();
        explicit["organization"]["organization_type"] = json!("claude_max");
        explicit["organization"]["plan_display_name"] = json!("Ultra");
        // Synthetic matching-family tier exercises generic suffix handling;
        // it does not claim this is a real provider tier identifier.
        explicit["organization"]["rate_limit_tier"] = json!("default_claude_ultra_20x");
        // Confirmed read-only provider shape: Ultra can retain the legacy Max
        // organization type and tier. That tier must not rename the plan.
        let mut legacy_alias = profile();
        legacy_alias["organization"]["plan_display_name"] = json!("Ultra");
        legacy_alias["organization"]["organization_type"] = json!("claude_max");
        legacy_alias["organization"]["rate_limit_tier"] = json!("default_claude_max_20x");
        legacy_alias["organization"]["seat_tier"] = Value::Null;
        let mut generic = profile();
        generic["organization"]["organization_type"] = json!("claude_future-subscription");
        let (mut client, server) = read_server(vec![
            ("/api/oauth/profile", explicit),
            ("/api/oauth/profile", legacy_alias),
            ("/api/oauth/profile", generic),
        ]);
        client.subscription_type = Some("max".into());
        client.rate_limit_tier = Some("default_claude_max_5x".into());
        assert_eq!(
            client
                .plan(&identity(), &Default::default())
                .await
                .unwrap()
                .as_deref(),
            Some("Ultra 20x")
        );
        let reported =
            super::super::super::claude::plan_metadata(&json!({"subscription_type":"max"}));
        assert_eq!(
            client
                .plan(&identity(), &reported)
                .await
                .unwrap()
                .as_deref(),
            Some("Ultra")
        );
        assert_eq!(
            client
                .plan(&identity(), &Default::default())
                .await
                .unwrap()
                .as_deref(),
            Some("future-subscription")
        );
        server.join().unwrap();
    }
    #[tokio::test]
    async fn legacy_profile_types_do_not_downgrade_explicit_reported_plans() {
        let mut legacy = profile();
        legacy["organization"]["organization_type"] = json!("claude_max");
        let mut malformed = legacy.clone();
        malformed["organization"]["plan_display_name"] = json!("untrusted\nlabel");
        let cases = [
            (
                json!({"subscription_type":"max","plan_display_name":"Ultra","rate_limit_tier":"default_claude_max_20x"}),
                legacy.clone(),
                "Ultra",
            ),
            (
                json!({"subscription_type":"future-subscription","rate_limit_tier":"default_claude_future_subscription_17x"}),
                legacy,
                "future-subscription 17x",
            ),
            (
                json!({"subscription_type":"max","plan_display_name":"Future Plan"}),
                malformed,
                "Future Plan",
            ),
        ];
        let (mut client, server) = read_server(
            cases
                .iter()
                .map(|(_, profile, _)| ("/api/oauth/profile", profile.clone()))
                .collect(),
        );
        client.subscription_type = Some("max".into());
        client.rate_limit_tier = Some("default_claude_max_5x".into());
        for (reported, _, expected) in cases {
            let reported = super::super::super::claude::plan_metadata(&reported);
            assert_eq!(
                client
                    .plan(&identity(), &reported)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(expected)
            );
        }
        server.join().unwrap();
    }
    #[tokio::test]
    async fn cached_display_requires_verified_profile_and_matching_owner() {
        let mut legacy = profile();
        legacy["organization"]["organization_type"] = json!("claude_max");
        let mut explicit = legacy.clone();
        explicit["organization"]["plan_display_name"] = json!("Profile Plan");
        let mut mismatch = legacy.clone();
        mismatch["account"]["email"] = json!("other@example.test");
        let cases = [
            (
                " FIXTURE@EXAMPLE.TEST ",
                json!({"subscription_type":"max"}),
                legacy.clone(),
                Some("Ultra"),
            ),
            (
                "other@example.test",
                json!({"subscription_type":"max"}),
                legacy.clone(),
                Some("max 5x"),
            ),
            (
                "fixture@example.test",
                json!({"subscription_type":"max"}),
                explicit,
                Some("Profile Plan"),
            ),
            (
                "fixture@example.test",
                json!({"subscription_type":"max","plan_display_name":"Reported Plan"}),
                legacy,
                Some("Reported Plan"),
            ),
            (
                "fixture@example.test",
                json!({"subscription_type":"max"}),
                mismatch,
                None,
            ),
        ];
        let (mut client, server) = read_server(
            cases
                .iter()
                .map(|(_, _, profile, _)| ("/api/oauth/profile", profile.clone()))
                .collect(),
        );
        client.subscription_type = Some("max".into());
        client.rate_limit_tier = Some("default_claude_max_5x".into());
        client.plan_display_name = Some("Ultra".into());
        for (owner, reported, _, expected) in cases {
            client.plan_email = Some(owner.into());
            let reported = super::super::super::claude::plan_metadata(&reported);
            let result = client.plan(&identity(), &reported).await;
            match expected {
                Some(expected) => assert_eq!(result.unwrap().as_deref(), Some(expected)),
                None => assert!(
                    result.is_err(),
                    "cached display must not survive profile mismatch"
                ),
            }
        }
        server.join().unwrap();
    }
}
