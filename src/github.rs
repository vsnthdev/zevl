// shared GitHub API plumbing.

use std::time::Duration;

pub const USER_AGENT: &str = "zevl";

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}
