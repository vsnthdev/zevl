// shared GitHub API plumbing.

use std::time::Duration;

pub const USER_AGENT: &str = "zevl";

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

// reports whether an error chain contains an HTTP 401 Unauthorized response.
pub fn is_unauthorized(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<ureq::Error>(),
            Some(ureq::Error::StatusCode(401))
        )
    })
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;

    #[test]
    fn detects_wrapped_401_errors() {
        let error = Err::<(), _>(ureq::Error::StatusCode(401))
            .context("failed to search for open pull requests")
            .unwrap_err();
        assert!(is_unauthorized(&error));
    }

    #[test]
    fn ignores_non_401_errors() {
        let forbidden = Err::<(), _>(ureq::Error::StatusCode(403))
            .context("failed to search for open pull requests")
            .unwrap_err();
        assert!(!is_unauthorized(&forbidden));

        let other = anyhow::anyhow!("network down");
        assert!(!is_unauthorized(&other));
    }
}
