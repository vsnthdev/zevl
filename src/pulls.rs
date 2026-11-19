// searches open pull requests across a personal account or an organization.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::github;

const SEARCH_URL: &str = "https://api.github.com/search/issues";
const PER_PAGE: usize = 100;
// github's search api only returns the first 1000 results for a query.
const MAX_RESULTS: usize = 1000;

/// the account scope to search pull requests in.
#[derive(Clone)]
pub enum Scope {
    /// repositories owned by the authenticated user.
    Personal(String),
    /// repositories owned by an organization.
    Organization(String),
}

impl Scope {
    fn query(&self) -> String {
        match self {
            Scope::Personal(login) => format!("user:{login} is:pr is:open"),
            Scope::Organization(login) => format!("org:{login} is:pr is:open"),
        }
    }
}

#[derive(Clone)]
pub struct PullRequest {
    pub number: u64,
    pub owner: String,
    pub repository: String,
    pub title: String,
    pub author: String,
}

impl PullRequest {
    /// renders the searchable label: `{number} {repository} - {title} - {author}`.
    pub fn label(&self) -> String {
        format!(
            "{} {} - {} - {}",
            self.number, self.repository, self.title, self.author
        )
    }

    /// the canonical web url that opens the pull request in a browser.
    pub fn url(&self) -> String {
        format!(
            "https://github.com/{}/{}/pull/{}",
            self.owner, self.repository, self.number
        )
    }
}

pub fn open_pull_requests(token: &str, scope: &Scope) -> Result<Vec<PullRequest>> {
    let agent = github::agent();
    let query = scope.query();
    let mut pull_requests = Vec::new();
    let mut page = 1;

    loop {
        let response: SearchResponse = agent
            .get(SEARCH_URL)
            .query("q", &query)
            // order the results by the most recently updated pull requests.
            .query("sort", "updated")
            .query("order", "desc")
            .query("per_page", PER_PAGE.to_string())
            .query("page", page.to_string())
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", github::USER_AGENT)
            .call()
            .context("failed to search for open pull requests")?
            .body_mut()
            .read_json()
            .context("GitHub returned an unexpected pull request search response")?;

        let count = response.items.len();
        pull_requests.extend(response.items.into_iter().map(PullRequest::from));

        if count < PER_PAGE || pull_requests.len() >= MAX_RESULTS {
            break;
        }

        page += 1;
    }

    Ok(pull_requests)
}

impl From<SearchItem> for PullRequest {
    fn from(item: SearchItem) -> Self {
        // `repository_url` looks like `https://api.github.com/repos/{owner}/{repo}`.
        let mut parts = item.repository_url.rsplit('/');
        let repository = parts.next().unwrap_or_default().to_string();
        let owner = parts.next().unwrap_or_default().to_string();

        let author = item
            .user
            .map(|user| user.login)
            .unwrap_or_else(|| "ghost".to_string());

        Self {
            number: item.number,
            owner,
            repository,
            title: item.title,
            author,
        }
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    items: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    number: u64,
    title: String,
    repository_url: String,
    user: Option<SearchUser>,
}

#[derive(Deserialize)]
struct SearchUser {
    login: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pull_request() -> PullRequest {
        PullRequest {
            number: 42,
            owner: "vsnthdev".to_string(),
            repository: "mantu".to_string(),
            title: "Fix the thing".to_string(),
            author: "vsnthdev".to_string(),
        }
    }

    #[test]
    fn label_matches_display_format() {
        assert_eq!(
            pull_request().label(),
            "42 mantu - Fix the thing - vsnthdev"
        );
    }

    #[test]
    fn url_points_at_the_pull_request() {
        assert_eq!(
            pull_request().url(),
            "https://github.com/vsnthdev/mantu/pull/42"
        );
    }

    #[test]
    fn search_item_splits_owner_and_repository() {
        let item = SearchItem {
            number: 7,
            title: "Fix the thing".to_string(),
            repository_url: "https://api.github.com/repos/introbotai/cluster-admin".to_string(),
            user: Some(SearchUser {
                login: "vsnthdev".to_string(),
            }),
        };

        let pull_request = PullRequest::from(item);
        assert_eq!(pull_request.owner, "introbotai");
        assert_eq!(pull_request.repository, "cluster-admin");
    }
}
