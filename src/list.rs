// lets the user pick an account, then fuzzy search its open pull requests.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::auth;
use crate::github;
use crate::pulls::{self, Scope};
use crate::tui;

const ORGS_URL: &str = "https://api.github.com/user/orgs";
const PER_PAGE: usize = 100;

pub fn list() -> Result<()> {
    let spinner = cliclack::spinner();
    spinner.start("checking your GitHub session");

    let Some(session) = auth::session()? else {
        spinner.error("no GitHub session found");
        bail!("you're not authenticated with GitHub, run `zevl login` first");
    };

    spinner.set_message("fetching your organizations");
    let orgs = match fetch_orgs(&session.token) {
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

    let (scope, scope_label) = if selected == session.login {
        (
            Scope::Personal(selected),
            "your personal account".to_string(),
        )
    } else {
        let label = selected.clone();
        (Scope::Organization(selected), label)
    };

    let spinner = cliclack::spinner();
    spinner.start(format!("fetching open pull requests in {scope_label}"));

    let pull_requests = match pulls::open_pull_requests(&session.token, &scope) {
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
    // browser: `R` re-runs this fetch and `enter` opens in the browser without
    // leaving the table.
    let refresh: tui::RefreshFn = {
        let token = session.token.clone();
        let scope = scope.clone();
        Arc::new(move || pulls::open_pull_requests(&token, &scope))
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
