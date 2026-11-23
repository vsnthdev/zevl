// lists open pull requests for an organization (or a picked account), then
// hands them to a fuzzy searchable browser.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::auth;
use crate::github;
use crate::pulls::{self, Scope};
use crate::tui;

const ORGS_URL: &str = "https://api.github.com/user/orgs";
const PER_PAGE: usize = 100;
const SESSION_EXPIRED_MESSAGE: &str = "your GitHub session expired, run `zevl login` again";

/// options accepted by `zevl list`, also flattened into the root `zevl`
/// command because a bare `zevl` invocation is an alias for `list`.
#[derive(Debug, Default, clap::Args)]
pub struct ListArgs {
    /// Skip the organization picker and list pull requests in this organization.
    #[arg(long, value_name = "SCOPE")]
    pub scope: Option<String>,
}

pub fn list(args: ListArgs) -> Result<()> {
    let spinner = cliclack::spinner();
    spinner.start("checking your GitHub session");

    let Some(session) = auth::session()? else {
        spinner.error("no GitHub session found");
        bail!("you're not authenticated with GitHub, run `zevl login` first");
    };

    let (scope, scope_label, initial_token) = match args.scope {
        // the organization was given up front, so skip discovering the user's
        // organizations and the interactive picker entirely.
        Some(org) => {
            spinner.stop(format!("using the {org} organization"));
            (Scope::Organization(org.clone()), org, Some(session.token))
        }
        None => {
            spinner.set_message("fetching your organizations");
            let orgs = match run_with_token(Some(&session.token), fetch_orgs) {
                Ok(orgs) => orgs,
                Err(error) => {
                    spinner.error("failed to fetch your organizations");
                    return Err(error);
                }
            };
            spinner.stop("fetched your GitHub account and organizations");

            let mut items = Vec::with_capacity(orgs.len() + 1);
            items.push((
                session.login.clone(),
                session.login.clone(),
                "personal account",
            ));

            for org in orgs {
                items.push((org.login.clone(), org.login.clone(), "organization"));
            }

            let selected = cliclack::select("select an account")
                .items(&items)
                .interact()?;

            if selected == session.login {
                (
                    Scope::Personal(selected),
                    "your personal account".to_string(),
                    None,
                )
            } else {
                let label = selected.clone();
                (Scope::Organization(selected), label, None)
            }
        }
    };

    let spinner = cliclack::spinner();
    spinner.start(format!("fetching open pull requests in {scope_label}"));

    let pull_requests = match run_with_token(initial_token.as_deref(), |token| {
        pulls::open_pull_requests(token, &scope)
    }) {
        Ok(pull_requests) => pull_requests,
        Err(error) => {
            spinner.error("failed to fetch open pull requests");
            return Err(error);
        }
    };

    spinner.stop(format!(
        "found {} open pull requests in {scope_label}",
        pull_requests.len()
    ));

    if pull_requests.is_empty() {
        cliclack::log::warning(format!("no open pull requests in {scope_label}"))?;
        return Ok(());
    }

    // hand the fetched pull requests and the actions over to the full screen
    // browser: `R` re-runs this fetch (refreshing the GitHub token if it
    // expired while the TUI stayed open) and `enter` opens in the browser
    // without leaving the table.
    let refresh: tui::RefreshFn = {
        let scope = scope.clone();
        Arc::new(move || run_with_token(None, |token| pulls::open_pull_requests(token, &scope)))
    };
    let open: tui::OpenFn =
        Arc::new(|url| open::that(url).context("failed to open the pull request in your browser"));

    let opened = tui::select_pull_request(&scope_label, pull_requests, refresh, open)?;

    match opened.as_slice() {
        [] => cliclack::log::info("quit without opening a pull request")?,
        [url] => cliclack::log::success(format!("opened {url}"))?,
        urls => cliclack::log::success(format!("opened {} pull requests", urls.len()))?,
    }

    Ok(())
}

fn fetch_orgs(token: &str) -> Result<Vec<Org>> {
    let agent = github::agent();
    let mut orgs = Vec::new();
    let mut page = 1;

    loop {
        let url = format!("{ORGS_URL}?per_page={PER_PAGE}&page={page}");

        let batch: Vec<Org> = agent
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", github::USER_AGENT)
            .call()
            .context("failed to fetch your GitHub organizations")?
            .body_mut()
            .read_json()
            .context("GitHub returned an unexpected organizations response")?;

        let count = batch.len();
        orgs.extend(batch);

        if count < PER_PAGE {
            break;
        }

        page += 1;
    }

    Ok(orgs)
}

#[derive(Deserialize)]
struct Org {
    login: String,
}

fn run_with_token<T, F>(initial_token: Option<&str>, request: F) -> Result<T>
where
    F: FnMut(&str) -> Result<T>,
{
    run_with_token_using(
        initial_token,
        auth::access_token,
        auth::refresh_access_token,
        request,
    )
}

fn run_with_token_using<T, FAccess, FRefresh, FRequest>(
    initial_token: Option<&str>,
    get_access_token: FAccess,
    refresh_access_token: FRefresh,
    mut request: FRequest,
) -> Result<T>
where
    FAccess: FnOnce() -> Result<Option<String>>,
    FRefresh: FnOnce(&str) -> Result<Option<String>>,
    FRequest: FnMut(&str) -> Result<T>,
{
    let token = match initial_token {
        Some(token) => token.to_string(),
        None => get_access_token()?.context(SESSION_EXPIRED_MESSAGE)?,
    };

    match request(&token) {
        Ok(value) => Ok(value),
        Err(error) if github::is_unauthorized(&error) => {
            let fresh_token = refresh_access_token(&token)?.context(SESSION_EXPIRED_MESSAGE)?;
            request(&fresh_token)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_once_after_refreshing_on_401() {
        let mut seen = Vec::new();
        let value = run_with_token_using(
            Some("gho_expired"),
            || panic!("should not call get_access_token when initial_token is given"),
            |rejected| {
                assert_eq!(rejected, "gho_expired");
                Ok(Some("gho_fresh".to_string()))
            },
            |token| {
                seen.push(token.to_string());
                if token == "gho_expired" {
                    Err(ureq::Error::StatusCode(401)).context("api request failed")
                } else {
                    Ok(42)
                }
            },
        )
        .unwrap();

        assert_eq!(value, 42);
        assert_eq!(seen, vec!["gho_expired", "gho_fresh"]);
    }

    #[test]
    fn does_not_refresh_on_non_401_errors() {
        let error = run_with_token_using(
            None,
            || Ok(Some("gho_valid".to_string())),
            |_| panic!("should not refresh on 500 error"),
            |_| Err::<(), _>(ureq::Error::StatusCode(500)).context("server error"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("server error"));
    }

    #[test]
    fn reports_expired_session_when_refresh_returns_none() {
        let error = run_with_token_using(
            Some("gho_expired"),
            || Ok(None),
            |_| Ok(None),
            |_| Err::<(), _>(ureq::Error::StatusCode(401)).context("unauthorized"),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), SESSION_EXPIRED_MESSAGE);
    }
}
