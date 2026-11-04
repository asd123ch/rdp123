//! Looks up the newest published release on GitHub.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/asd123ch/rdp123/releases/latest";

/// A published release newer than the running version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: String,
}

/// Return the latest release if it is newer than `current`. Blocks the
/// calling thread; call it off the main thread.
pub fn newer_release(current: &str) -> Result<Option<Release>> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the update check")?;
    let latest = runtime.block_on(async {
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("RDP123/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("creating the update client")?
            .get(LATEST_RELEASE_API)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .context("contacting GitHub")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response
            .error_for_status()
            .context("GitHub rejected the update check")?;
        let latest: LatestRelease = response
            .json()
            .await
            .context("reading the release information")?;
        Ok::<_, anyhow::Error>(Some(latest))
    })?;
    let Some(latest) = latest else {
        return Ok(None);
    };
    let version = latest.tag_name.trim_start_matches('v').to_string();
    Ok(is_newer(&version, current)?.then_some(Release {
        version,
        url: latest.html_url,
    }))
}

fn parse_version(version: &str) -> Result<(u64, u64, u64)> {
    let mut parts = version.trim_start_matches('v').split('.');
    let mut next = || -> Result<u64> {
        parts
            .next()
            .context("missing version component")?
            .parse()
            .with_context(|| format!("invalid version {version:?}"))
    };
    let parsed = (next()?, next()?, next()?);
    if parts.next().is_some() {
        bail!("invalid version {version:?}");
    }
    Ok(parsed)
}

fn is_newer(candidate: &str, current: &str) -> Result<bool> {
    Ok(parse_version(candidate)? > parse_version(current)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("0.6.30", "0.6.29").unwrap());
        assert!(is_newer("v0.10.0", "0.9.9").unwrap());
        assert!(is_newer("1.0.0", "0.99.99").unwrap());
        assert!(!is_newer("0.6.29", "0.6.29").unwrap());
        assert!(!is_newer("0.6.3", "0.6.29").unwrap());
    }

    #[test]
    #[ignore = "contacts GitHub"]
    fn queries_the_live_release_feed() {
        newer_release("0.0.1").unwrap();
    }

    #[test]
    fn rejects_malformed_versions() {
        assert!(is_newer("0.6", "0.6.0").is_err());
        assert!(is_newer("0.6.1-beta", "0.6.0").is_err());
        assert!(is_newer("0.6.1.2", "0.6.0").is_err());
    }
}
