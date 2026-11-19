// GitHub OAuth device flow login, ported from Introbot's `housekeep` CLI.
//
// The flow asks GitHub for a one-time code, puts it on the clipboard, opens the
// verification page in the browser, then polls until the user approves. The
// resulting token is stored in the operating system's keychain.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::github;

// The GitHub OAuth app client ID. Device flow must be enabled on the app.
const CLIENT_ID: &str = "Ov23liakUm4h4pefncDX";

const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const USER_URL: &str = "https://api.github.com/user";
const SCOPES: &str = "repo read:org";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const SERVICE_NAME: &str = "zevl";
const ACCOUNT_NAME: &str = "github-token";

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

    let token = poll_for_token(&agent, &device)?;
    let login = fetch_login(&agent, &token)?.context("GitHub rejected the new access token")?;
    store_token(&token)?;

    println!("\n✅ Logged in to GitHub as @{login}");
    Ok(())
}

// returns the current GitHub session, if the stored token is still valid.
pub fn session() -> Result<Option<Session>> {
    let Some(token) = stored_token()? else {
        return Ok(None);
    };

    let agent = github::agent();

    Ok(fetch_login(&agent, &token)?.map(|login| Session { token, login }))
}

// returns the stored GitHub token, if the keychain has one.
pub fn stored_token() -> Result<Option<String>> {
    match entry()?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => {
            Err(error).context("failed to read the GitHub token from the system keychain")
        }
    }
}

fn request_device_code(agent: &ureq::Agent) -> Result<DeviceCodeResponse> {
    agent
        .post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
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

fn poll_for_token(agent: &ureq::Agent, device: &DeviceCodeResponse) -> Result<String> {
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
            .send_json(AccessTokenRequest {
                client_id: CLIENT_ID,
                device_code: &device.device_code,
                grant_type: GRANT_TYPE,
            })
            .context("failed to reach GitHub while waiting for the login")?
            .body_mut()
            .read_json()
            .context("GitHub returned an unexpected token response")?;

        if let Some(token) = response.access_token {
            return Ok(token);
        }

        match response.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += Duration::from_secs(5),
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

fn store_token(token: &str) -> Result<()> {
    entry()?
        .set_password(token)
        .context("failed to save the GitHub token in the system keychain")
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

#[derive(Deserialize)]
struct AccessTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct UserResponse {
    login: String,
}
