//! Narrow mutation transport; review collection remains in the published toolbelt.
use anyhow::{bail, Result};
use serde_json::Value;
use std::time::Duration;

pub(crate) fn request(token: &str, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
    if token.trim().is_empty() || token.contains(['\r', '\n']) || !path.starts_with("/repos/") {
        bail!("invalid GitHub request credentials/route");
    }
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .new_agent();
    let url = format!("https://api.github.com{path}");
    let auth = format!("Bearer {token}");
    let mut response = match method {
        "GET" => agent
            .get(&url)
            .header("Authorization", &auth)
            .header("User-Agent", "opencode-repo-review-pr")
            .call()?,
        "PUT" => agent
            .put(&url)
            .header("Authorization", &auth)
            .header("User-Agent", "opencode-repo-review-pr")
            .send_json(body.unwrap_or(Value::Null))?,
        "POST" => agent
            .post(&url)
            .header("Authorization", &auth)
            .header("User-Agent", "opencode-repo-review-pr")
            .send_json(body.unwrap_or(Value::Null))?,
        "PATCH" => agent
            .patch(&url)
            .header("Authorization", &auth)
            .header("User-Agent", "opencode-repo-review-pr")
            .send_json(body.unwrap_or(Value::Null))?,
        _ => bail!("unsupported GitHub operation"),
    };
    if !response.status().is_success() {
        bail!(
            "GitHub HTTP {}; reconcile a mutation before retrying",
            response.status().as_u16()
        );
    }
    Ok(response
        .body_mut()
        .with_config()
        .limit(16 * 1024 * 1024)
        .read_json()?)
}

pub fn validate_merge_protection(value: &Value, app_id: u64) -> Result<()> {
    if value
        .pointer("/enforce_admins/enabled")
        .and_then(Value::as_bool)
        != Some(true)
        || value
            .pointer("/required_status_checks/strict")
            .and_then(Value::as_bool)
            != Some(true)
    {
        bail!(
            "merge requires strict up-to-date native branch protection applying to administrators"
        );
    }
    let checks = value
        .pointer("/required_status_checks/checks")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("protected review-policy binding missing"))?;
    if app_id == 0
        || app_id == 15368
        || !checks.iter().any(|c| {
            c["context"].as_str() == Some("review-policy") && c["app_id"].as_u64() == Some(app_id)
        })
    {
        bail!("protected review-policy must be bound to the configured dedicated policy App");
    }
    Ok(())
}
