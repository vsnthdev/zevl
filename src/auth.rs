// GitHub OAuth device flow login, ported from Introbot's `housekeep` CLI.
//
// The flow asks GitHub for a one-time code, puts it on the clipboard, opens the
// verification page in the browser, then polls until the user approves. The
// resulting access token and (when issued by GitHub) refresh token and expiry
// metadata are stored as JSON in the operating system's keychain and rotated
// automatically whenever the access token expires.

use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::github;

// The GitHub OAuth app client ID. Device flow must be enabled on the app.
const CLIENT_ID: &str = "Ov23liakUm4h4pefncDX";

const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const USER_URL: &str = "https://api.github.com/user";
const SCOPES: &str = "repo read:org";
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REFRESH_GRANT_TYPE: &str = "refresh_token";
const SERVICE_NAME: &str = "zevl";
const ACCOUNT_NAME: &str = "github-token";
// refresh access tokens slightly ahead of expiry to avoid clock-skew races.
const EXPIRY_SKEW_SECS: u64 = 60;

pub struct Session {
    pub token: String,
    pub login: String,
}

pub fn login() -> Result<()> {
    if let Some(session) = session()? {
        println!("you're already authenticated as @{}", session.login);
        return Ok(());
    }

    let agent = github::agent();

    let device = request_device_code(&agent)?;
    offer_user_code(&device);

    let credentials = poll_for_token(&agent, &device)?;
    let login = fetch_login(&agent, &credentials.access_token)?
        .context("GitHub rejected the new access token")?;
    store_credentials(&credentials)?;

    println!("\n✅ Logged in to GitHub as @{login}");
    Ok(())
}

// returns the current GitHub session, automatically refreshing an expired
// access token when a refresh token is available.
pub fn session() -> Result<Option<Session>> {
    let Some(credentials) = stored_credentials()? else {
        return Ok(None);
    };

    let agent = github::agent();
    resolve_session(
        credentials,
        now_unix_secs(),
        |refresh_token| refresh_credentials(&agent, refresh_token),
        |access_token| fetch_login(&agent, access_token),
    )
}

// returns a stored access token, proactively refreshing it if it has expired
// or is about to expire.
pub fn access_token() -> Result<Option<String>> {
    let Some(credentials) = stored_credentials()? else {
        return Ok(None);
    };

    let now = now_unix_secs();
    if !credentials.is_access_token_expired(now) {
        return Ok(Some(credentials.access_token));
    }

    let Some(refresh_token) = credentials.refresh_token.as_deref() else {
        return Ok(None);
    };

    let agent = github::agent();
    Ok(refresh_credentials(&agent, refresh_token)?.map(|creds| creds.access_token))
}

// refreshes the stored credentials after an API request was rejected with 401.
// if another process (or an earlier refresh) already replaced `rejected_token`,
// returns the newer token instead of burning another refresh.
pub fn refresh_access_token(rejected_token: &str) -> Result<Option<String>> {
    let stored = stored_credentials()?;
    let agent = github::agent();

    resolve_refreshed_access_token(stored, rejected_token, now_unix_secs(), |refresh_token| {
        refresh_credentials(&agent, refresh_token)
    })
}

fn request_device_code(agent: &ureq::Agent) -> Result<DeviceCodeResponse> {
    agent
        .post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .header("User-Agent", github::USER_AGENT)
        .send_json(DeviceCodeRequest {
            client_id: CLIENT_ID,
            scope: SCOPES,
        })
        .context("failed to reach GitHub to start the login")?
        .body_mut()
        .read_json()
        .context("GitHub returned an unexpected device login response")
}

fn offer_user_code(device: &DeviceCodeResponse) {
    let copied = arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(&device.user_code))
        .is_ok();

    if copied {
        println!("📋 Login code copied to your clipboard");
    }

    println!(
        "\nin case the browser did not automatically open, visit {}",
        device.verification_uri
    );
    println!(
        "and paste the following code into the input field {} to login.\n",
        device.user_code
    );

    if let Err(error) = open::that(&device.verification_uri) {
        eprintln!("could not open your browser automatically: {error}");
    }

    println!("Waiting for GitHub to confirm authentication...");
}

fn poll_for_token(agent: &ureq::Agent, device: &DeviceCodeResponse) -> Result<StoredCredentials> {
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = Duration::from_secs(device.interval.max(1));

    loop {
        if Instant::now() >= deadline {
            bail!("the GitHub login code expired, please run `login` again");
        }

        thread::sleep(interval);

        let response: AccessTokenResponse = agent
            .post(ACCESS_TOKEN_URL)
            .header("Accept", "application/json")
            .header("User-Agent", github::USER_AGENT)
            .send_json(AccessTokenRequest {
                client_id: CLIENT_ID,
                device_code: &device.device_code,
                grant_type: DEVICE_GRANT_TYPE,
            })
            .context("failed to reach GitHub while waiting for the login")?
            .body_mut()
            .read_json()
            .context("GitHub returned an unexpected token response")?;

        if let Some(credentials) = StoredCredentials::from_response(&response, now_unix_secs()) {
            return Ok(credentials);
        }

        match response.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => {
                interval = response
                    .interval
                    .map(|secs| Duration::from_secs(secs.max(1)))
                    .unwrap_or_else(|| interval + Duration::from_secs(5));
            }
            Some("expired_token") => {
                bail!("the GitHub login code expired, please run `login` again")
            }
            Some("access_denied") => bail!("the GitHub login was cancelled"),
            Some(error) => {
                let description = response
                    .error_description
                    .unwrap_or_else(|| error.to_string());
                bail!("GitHub login failed: {description}");
            }
            None => bail!("GitHub returned no access token"),
        }
    }
}

fn refresh_credentials(
    agent: &ureq::Agent,
    refresh_token: &str,
) -> Result<Option<StoredCredentials>> {
    refresh_and_store(
        refresh_token,
        now_unix_secs(),
        |token| request_refreshed_token_from(agent, ACCESS_TOKEN_URL, token),
        store_credentials,
        stored_credentials,
    )
}

fn request_refreshed_token_from(
    agent: &ureq::Agent,
    url: &str,
    refresh_token: &str,
) -> Result<AccessTokenResponse> {
    let mut response = agent
        .post(url)
        .config()
        .http_status_as_error(false)
        .build()
        .header("Accept", "application/json")
        .header("User-Agent", github::USER_AGENT)
        .send_json(RefreshTokenRequest {
            client_id: CLIENT_ID,
            grant_type: REFRESH_GRANT_TYPE,
            refresh_token,
        })
        .context("failed to reach GitHub to refresh the access token")?;

    let status = response.status().as_u16();
    // GitHub returns OAuth errors like `bad_refresh_token` in a JSON body with
    // HTTP 200 (and RFC 6749 allows 400/401). Treat other non-2xx statuses as
    // transport/server failures rather than invalid credentials.
    if !(200..=299).contains(&status) && status != 400 && status != 401 {
        return Err(ureq::Error::StatusCode(status))
            .context("failed to reach GitHub to refresh the access token");
    }

    response
        .body_mut()
        .read_json()
        .context("GitHub returned an unexpected token refresh response")
}

fn resolve_session<FRefresh, FFetchLogin>(
    credentials: StoredCredentials,
    now: u64,
    mut refresh: FRefresh,
    mut fetch_login: FFetchLogin,
) -> Result<Option<Session>>
where
    FRefresh: FnMut(&str) -> Result<Option<StoredCredentials>>,
    FFetchLogin: FnMut(&str) -> Result<Option<String>>,
{
    let (credentials, refreshed) = if credentials.is_access_token_expired(now) {
        let Some(refresh_token) = credentials.refresh_token.as_deref() else {
            return Ok(None);
        };
        let Some(credentials) = refresh(refresh_token)? else {
            return Ok(None);
        };
        (credentials, true)
    } else {
        (credentials, false)
    };

    if let Some(login) = fetch_login(&credentials.access_token)? {
        return Ok(Some(Session {
            token: credentials.access_token,
            login,
        }));
    }

    // if GitHub rejected the token with 401 despite our local expiry check
    // (clock skew or early revocation), try refreshing once before giving up.
    if !refreshed && let Some(refresh_token) = credentials.refresh_token.as_deref() {
        let Some(credentials) = refresh(refresh_token)? else {
            return Ok(None);
        };

        if let Some(login) = fetch_login(&credentials.access_token)? {
            return Ok(Some(Session {
                token: credentials.access_token,
                login,
            }));
        }
    }

    Ok(None)
}

fn resolve_refreshed_access_token<FRefresh>(
    stored: Option<StoredCredentials>,
    rejected_token: &str,
    now: u64,
    refresh: FRefresh,
) -> Result<Option<String>>
where
    FRefresh: FnOnce(&str) -> Result<Option<StoredCredentials>>,
{
    let Some(credentials) = stored else {
        return Ok(None);
    };

    if credentials.access_token != rejected_token && !credentials.is_access_token_expired(now) {
        return Ok(Some(credentials.access_token));
    }

    let Some(refresh_token) = credentials.refresh_token.as_deref() else {
        return Ok(None);
    };

    Ok(refresh(refresh_token)?.map(|creds| creds.access_token))
}

enum RefreshOutcome {
    Refreshed(StoredCredentials),
    InvalidRefreshToken,
}

fn parse_refresh_response(response: AccessTokenResponse, now: u64) -> Result<RefreshOutcome> {
    if let Some(credentials) = StoredCredentials::from_response(&response, now) {
        return Ok(RefreshOutcome::Refreshed(credentials));
    }

    match response.error.as_deref() {
        Some("bad_refresh_token") | Some("expired_token") | Some("invalid_grant") => {
            Ok(RefreshOutcome::InvalidRefreshToken)
        }
        Some(error) => {
            let description = response
                .error_description
                .unwrap_or_else(|| error.to_string());
            bail!("GitHub token refresh failed: {description}");
        }
        None => bail!("GitHub returned no access token while refreshing"),
    }
}

fn refresh_and_store<FRequest, FStore, FLoad>(
    refresh_token: &str,
    now: u64,
    request_refresh: FRequest,
    store: FStore,
    load: FLoad,
) -> Result<Option<StoredCredentials>>
where
    FRequest: FnOnce(&str) -> Result<AccessTokenResponse>,
    FStore: FnOnce(&StoredCredentials) -> Result<()>,
    FLoad: FnOnce() -> Result<Option<StoredCredentials>>,
{
    let response = request_refresh(refresh_token)?;

    match parse_refresh_response(response, now)? {
        RefreshOutcome::Refreshed(credentials) => {
            // GitHub invalidates both the old access token and the old refresh
            // token as soon as the refresh succeeds, so persist the rotated
            // pair immediately.
            store(&credentials)?;
            Ok(Some(credentials))
        }
        RefreshOutcome::InvalidRefreshToken => {
            // another `zevl` process may have rotated the refresh token while
            // we were trying to use the previous one. re-read the keychain
            // before concluding the session is gone.
            if let Some(latest) = load()?
                && latest.refresh_token.as_deref() != Some(refresh_token)
                && !latest.is_access_token_expired(now)
            {
                return Ok(Some(latest));
            }
            Ok(None)
        }
    }
}

fn fetch_login(agent: &ureq::Agent, token: &str) -> Result<Option<String>> {
    let response = agent
        .get(USER_URL)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", github::USER_AGENT)
        .call();

    match response {
        Ok(mut response) => {
            let user: UserResponse = response
                .body_mut()
                .read_json()
                .context("GitHub returned an unexpected user response")?;

            Ok(Some(user.login))
        }
        // the stored token was revoked or expired.
        Err(ureq::Error::StatusCode(401)) => Ok(None),
        Err(error) => Err(error).context("failed to verify the GitHub token"),
    }
}

fn entry() -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE_NAME, ACCOUNT_NAME).context("failed to access the system keychain")
}

fn stored_credentials() -> Result<Option<StoredCredentials>> {
    match entry()?.get_password() {
        Ok(raw) => Ok(StoredCredentials::parse(raw)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => {
            Err(error).context("failed to read the GitHub token from the system keychain")
        }
    }
}

fn store_credentials(credentials: &StoredCredentials) -> Result<()> {
    let payload =
        serde_json::to_string(credentials).context("failed to serialize GitHub credentials")?;
    entry()?
        .set_password(&payload)
        .context("failed to save the GitHub token in the system keychain")
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredCredentials {
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token_expires_at: Option<u64>,
}

impl StoredCredentials {
    fn from_response(response: &AccessTokenResponse, now: u64) -> Option<Self> {
        let access_token = response
            .access_token
            .as_deref()
            .filter(|token| !token.is_empty())?
            .to_string();

        let refresh_token = response
            .refresh_token
            .as_deref()
            .filter(|token| !token.is_empty())
            .map(str::to_string);

        Some(Self {
            access_token,
            expires_at: response.expires_in.map(|secs| now.saturating_add(secs)),
            refresh_token,
            refresh_token_expires_at: response
                .refresh_token_expires_in
                .map(|secs| now.saturating_add(secs)),
        })
    }

    fn legacy(access_token: String) -> Self {
        Self {
            access_token,
            expires_at: None,
            refresh_token: None,
            refresh_token_expires_at: None,
        }
    }

    // accepts both the JSON format and legacy bare-token keychain entries saved
    // before refresh token support was added.
    fn parse(raw: String) -> Option<Self> {
        if raw.trim().is_empty() {
            return None;
        }

        let parsed: Self = serde_json::from_str(&raw).unwrap_or_else(|_| Self::legacy(raw));
        (!parsed.access_token.is_empty()).then_some(parsed)
    }

    fn is_access_token_expired(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|expires_at| now.saturating_add(EXPIRY_SKEW_SECS) >= expires_at)
    }
}

#[derive(Serialize)]
struct DeviceCodeRequest<'a> {
    client_id: &'a str,
    scope: &'a str,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Serialize)]
struct AccessTokenRequest<'a> {
    client_id: &'a str,
    device_code: &'a str,
    grant_type: &'a str,
}

#[derive(Serialize)]
struct RefreshTokenRequest<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

#[derive(Debug, Default, Deserialize)]
struct AccessTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    refresh_token_expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct UserResponse {
    login: String,
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    fn expiring_credentials(access: &str, refresh: &str, expires_at: u64) -> StoredCredentials {
        StoredCredentials {
            access_token: access.to_string(),
            expires_at: Some(expires_at),
            refresh_token: Some(refresh.to_string()),
            refresh_token_expires_at: Some(expires_at + 15_897_600),
        }
    }

    fn spawn_single_response_server(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("should bind test server");
        let addr = listener.local_addr().expect("should have local addr");

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("should accept client");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("should set timeout");

            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        request.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&request);
                        if let Some(header_end) = text.find("\r\n\r\n") {
                            let headers = &text[..header_end];
                            let content_len = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().ok())?
                                })
                                .unwrap_or(0);
                            if request.len() >= header_end + 4 + content_len {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }

            let response = format!(
                "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("should write response");
            String::from_utf8_lossy(&request).into_owned()
        });

        (format!("http://{addr}/login/oauth/access_token"), handle)
    }

    #[test]
    fn parses_token_response_with_refresh_token_and_expiries() {
        let json = r#"{
            "access_token": "gho_access_123",
            "expires_in": 28800,
            "refresh_token": "ghr_refresh_456",
            "refresh_token_expires_in": 15897600,
            "token_type": "bearer",
            "scope": "repo,read:org"
        }"#;

        let response: AccessTokenResponse = serde_json::from_str(json).unwrap();
        let credentials = StoredCredentials::from_response(&response, 1_000).unwrap();

        assert_eq!(credentials.access_token, "gho_access_123");
        assert_eq!(credentials.expires_at, Some(29_800));
        assert_eq!(
            credentials.refresh_token.as_deref(),
            Some("ghr_refresh_456")
        );
        assert_eq!(credentials.refresh_token_expires_at, Some(15_898_600));
    }

    #[test]
    fn parses_non_expiring_token_response_without_refresh_token() {
        let json = r#"{
            "access_token": "gho_non_expiring",
            "token_type": "bearer",
            "scope": "repo,read:org"
        }"#;

        let response: AccessTokenResponse = serde_json::from_str(json).unwrap();
        let credentials = StoredCredentials::from_response(&response, 1_000).unwrap();

        assert_eq!(credentials.access_token, "gho_non_expiring");
        assert_eq!(credentials.expires_at, None);
        assert_eq!(credentials.refresh_token, None);
        assert_eq!(credentials.refresh_token_expires_at, None);
    }

    #[test]
    fn stored_credentials_round_trip_and_parse_legacy_bare_tokens() {
        let creds = expiring_credentials("gho_1", "ghr_1", 10_000);
        let serialized = serde_json::to_string(&creds).unwrap();
        assert_eq!(StoredCredentials::parse(serialized), Some(creds));

        let legacy = StoredCredentials::parse("gho_legacy_token".to_string()).unwrap();
        assert_eq!(legacy.access_token, "gho_legacy_token");
        assert_eq!(legacy.expires_at, None);
        assert_eq!(legacy.refresh_token, None);

        assert_eq!(StoredCredentials::parse("   ".to_string()), None);
        assert_eq!(
            StoredCredentials::parse(r#"{"access_token":""}"#.to_string()),
            None
        );
    }

    #[test]
    fn access_token_expiry_respects_skew_margin() {
        let creds = expiring_credentials("gho_1", "ghr_1", 1_000);
        assert!(!creds.is_access_token_expired(939));
        assert!(creds.is_access_token_expired(940));
        assert!(creds.is_access_token_expired(1_000));
        assert!(creds.is_access_token_expired(2_000));

        let legacy = StoredCredentials::legacy("gho_legacy".to_string());
        assert!(!legacy.is_access_token_expired(u64::MAX));
    }

    #[test]
    fn resolve_session_skips_refresh_when_token_is_fresh_and_valid() {
        let creds = expiring_credentials("gho_valid", "ghr_1", 10_000);
        let mut refresh_calls = 0;

        let session = resolve_session(
            creds,
            5_000,
            |_| {
                refresh_calls += 1;
                panic!("should not refresh a valid unexpired token")
            },
            |token| {
                assert_eq!(token, "gho_valid");
                Ok(Some("vsnthdev".to_string()))
            },
        )
        .unwrap()
        .expect("expected active session");

        assert_eq!(refresh_calls, 0);
        assert_eq!(session.token, "gho_valid");
        assert_eq!(session.login, "vsnthdev");
    }

    #[test]
    fn resolve_session_proactively_refreshes_expired_token() {
        let creds = expiring_credentials("gho_old", "ghr_old", 1_000);
        let mut seen_login_tokens = Vec::new();

        let session = resolve_session(
            creds,
            1_500,
            |rt| {
                assert_eq!(rt, "ghr_old");
                Ok(Some(expiring_credentials("gho_new", "ghr_new", 30_300)))
            },
            |token| {
                seen_login_tokens.push(token.to_string());
                Ok(Some("vsnthdev".to_string()))
            },
        )
        .unwrap()
        .expect("expected refreshed session");

        assert_eq!(seen_login_tokens, vec!["gho_new"]);
        assert_eq!(session.token, "gho_new");
        assert_eq!(session.login, "vsnthdev");
    }

    #[test]
    fn resolve_session_reactively_refreshes_on_401_from_github() {
        let creds = expiring_credentials("gho_skewed", "ghr_1", 10_000);
        let mut refresh_calls = 0;

        let session = resolve_session(
            creds,
            5_000,
            |rt| {
                refresh_calls += 1;
                assert_eq!(rt, "ghr_1");
                Ok(Some(expiring_credentials("gho_fresh", "ghr_2", 33_800)))
            },
            |token| match token {
                "gho_skewed" => Ok(None),
                "gho_fresh" => Ok(Some("vsnthdev".to_string())),
                other => panic!("unexpected token {other}"),
            },
        )
        .unwrap()
        .expect("expected session after reactive refresh");

        assert_eq!(refresh_calls, 1);
        assert_eq!(session.token, "gho_fresh");
        assert_eq!(session.login, "vsnthdev");
    }

    #[test]
    fn resolve_session_does_not_loop_if_refreshed_token_is_also_rejected() {
        let creds = expiring_credentials("gho_old", "ghr_1", 10_000);
        let mut refresh_calls = 0;

        let session = resolve_session(
            creds,
            5_000,
            |_| {
                refresh_calls += 1;
                Ok(Some(expiring_credentials("gho_new", "ghr_2", 33_800)))
            },
            |_| Ok(None),
        )
        .unwrap();

        assert!(session.is_none());
        assert_eq!(refresh_calls, 1);
    }

    #[test]
    fn resolve_session_returns_none_for_expired_legacy_token_without_refresh_token() {
        let legacy = StoredCredentials::legacy("gho_legacy".to_string());
        let mut refresh_calls = 0;

        let session = resolve_session(
            legacy,
            5_000,
            |_| {
                refresh_calls += 1;
                Ok(None)
            },
            |_| Ok(None),
        )
        .unwrap();

        assert!(session.is_none());
        assert_eq!(refresh_calls, 0);
    }

    #[test]
    fn refresh_and_store_persists_rotated_credentials() {
        let saved = RefCell::new(None);

        let result = refresh_and_store(
            "ghr_old",
            1_000,
            |rt| {
                assert_eq!(rt, "ghr_old");
                Ok(AccessTokenResponse {
                    access_token: Some("gho_rotated".to_string()),
                    expires_in: Some(28_800),
                    refresh_token: Some("ghr_rotated".to_string()),
                    refresh_token_expires_in: Some(15_897_600),
                    ..Default::default()
                })
            },
            |creds| {
                *saved.borrow_mut() = Some(creds.clone());
                Ok(())
            },
            || panic!("should not reload keychain on successful refresh"),
        )
        .unwrap()
        .expect("expected rotated credentials");

        assert_eq!(saved.into_inner(), Some(result.clone()));
        assert_eq!(result.access_token, "gho_rotated");
        assert_eq!(result.refresh_token.as_deref(), Some("ghr_rotated"));
        assert_eq!(result.expires_at, Some(29_800));
    }

    #[test]
    fn refresh_and_store_returns_none_on_bad_refresh_token() {
        let result = refresh_and_store(
            "ghr_expired",
            1_000,
            |_| {
                Ok(AccessTokenResponse {
                    error: Some("bad_refresh_token".to_string()),
                    error_description: Some(
                        "The refresh token passed is incorrect or expired.".to_string(),
                    ),
                    ..Default::default()
                })
            },
            |_| panic!("should not overwrite keychain on bad_refresh_token"),
            || Ok(Some(expiring_credentials("gho_old", "ghr_expired", 500))),
        )
        .unwrap();

        assert!(result.is_none());
    }

    #[test]
    fn refresh_and_store_adopts_credentials_rotated_by_concurrent_process() {
        let newer = expiring_credentials("gho_from_other_proc", "ghr_from_other_proc", 30_000);

        let result = refresh_and_store(
            "ghr_stale",
            1_000,
            |_| {
                Ok(AccessTokenResponse {
                    error: Some("bad_refresh_token".to_string()),
                    ..Default::default()
                })
            },
            |_| panic!("should not overwrite keychain when adopting newer token"),
            || Ok(Some(newer.clone())),
        )
        .unwrap();

        assert_eq!(result, Some(newer));
    }

    #[test]
    fn refresh_and_store_errors_on_unexpected_oauth_error() {
        let error = refresh_and_store(
            "ghr_1",
            1_000,
            |_| {
                Ok(AccessTokenResponse {
                    error: Some("incorrect_client_credentials".to_string()),
                    error_description: Some("The client_id is incorrect.".to_string()),
                    ..Default::default()
                })
            },
            |_| Ok(()),
            || Ok(None),
        )
        .unwrap_err();

        assert!(error.to_string().contains("The client_id is incorrect."));
    }

    #[test]
    fn resolve_refreshed_access_token_reuses_already_rotated_keychain_token() {
        let newer = expiring_credentials("gho_newer", "ghr_newer", 10_000);

        let token = resolve_refreshed_access_token(Some(newer), "gho_stale", 5_000, |_| {
            panic!("should reuse newer keychain token instead of refreshing again")
        })
        .unwrap();

        assert_eq!(token.as_deref(), Some("gho_newer"));
    }

    #[test]
    fn resolve_refreshed_access_token_refreshes_when_stored_token_was_rejected() {
        let stale = expiring_credentials("gho_stale", "ghr_1", 10_000);

        let token = resolve_refreshed_access_token(Some(stale), "gho_stale", 5_000, |rt| {
            assert_eq!(rt, "ghr_1");
            Ok(Some(expiring_credentials("gho_fresh", "ghr_2", 33_800)))
        })
        .unwrap();

        assert_eq!(token.as_deref(), Some("gho_fresh"));
    }

    #[test]
    fn http_refresh_request_sends_device_flow_payload_without_client_secret() {
        let body = r#"{"access_token":"gho_new","expires_in":28800,"refresh_token":"ghr_new","refresh_token_expires_in":15897600}"#;
        let (url, server) = spawn_single_response_server("HTTP/1.1 200 OK", body);

        let agent = github::agent();
        let response = request_refreshed_token_from(&agent, &url, "ghr_old").unwrap();
        let raw_request = server.join().unwrap();

        assert_eq!(response.access_token.as_deref(), Some("gho_new"));
        assert_eq!(response.refresh_token.as_deref(), Some("ghr_new"));
        assert_eq!(response.expires_in, Some(28_800));

        let (_, json_body) = raw_request
            .split_once("\r\n\r\n")
            .expect("HTTP request should have headers and body");
        let sent: serde_json::Value = serde_json::from_str(json_body).unwrap();
        assert_eq!(sent["client_id"], CLIENT_ID);
        assert_eq!(sent["grant_type"], "refresh_token");
        assert_eq!(sent["refresh_token"], "ghr_old");
        assert!(sent.get("client_secret").is_none());
    }

    #[test]
    fn http_refresh_accepts_400_oauth_error_and_fails_on_500() {
        let bad_refresh = r#"{"error":"bad_refresh_token","error_description":"expired"}"#;
        let (url_400, server_400) =
            spawn_single_response_server("HTTP/1.1 400 Bad Request", bad_refresh);

        let agent = github::agent();
        let response = request_refreshed_token_from(&agent, &url_400, "ghr_old").unwrap();
        server_400.join().unwrap();
        assert_eq!(response.error.as_deref(), Some("bad_refresh_token"));

        let (url_500, server_500) =
            spawn_single_response_server("HTTP/1.1 500 Internal Server Error", "{}");
        let error = request_refreshed_token_from(&agent, &url_500, "ghr_old").unwrap_err();
        server_500.join().unwrap();
        assert!(error.chain().any(|c| matches!(
            c.downcast_ref::<ureq::Error>(),
            Some(ureq::Error::StatusCode(500))
        )));
    }
}
