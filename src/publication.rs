//! Client-owned attempt receipts for explicit forge publication.
use crate::storage;
use anyhow::{bail, Result};
use canix_toolbelt::review::Candidate;
use serde_json::{json, Value};
use std::path::Path;

mod lifecycle;
pub use lifecycle::{reconcile, update, Binding, Published, Selection};

#[derive(Clone, Copy)]
pub enum Kind {
    ForgejoComment,
    GitHubCheck,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Self::ForgejoComment => "forgejo-comment",
            Self::GitHubCheck => "github-check",
        }
    }
}

/// One explicitly authorized write per request/kind. An ambiguous attempt is
/// retained for authoritative reconciliation; changed payloads never reopen it.
pub fn once(
    root: &Path,
    request: &str,
    kind: Kind,
    candidate: &Candidate,
    payload: &Value,
    dispatch: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    if request.len() != 64 || !request.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid publication request ID");
    }
    candidate.validate()?;
    let path = root
        .join("publications")
        .join(format!("{request}-{}.json", kind.key()));
    let anchor = path.with_extension("lock");
    let previous_anchor = anchor.try_exists()?;
    let _lock = storage::lock(&anchor)?;
    if storage::read::<Value>(&path)?.is_some() || previous_anchor {
        bail!("publication already attempted; reconcile authoritative forge state without replaying it");
    }
    let mut receipt = json!({"schema_version":1,"request_id":request,"kind":kind.key(),
        "candidate":candidate,"payload_digest":storage::digest(&serde_json::to_vec(payload)?),
        "state":"dispatching"});
    storage::save(&path, &receipt)?;
    let result = dispatch()?;
    receipt["state"] = json!("confirmed");
    receipt["result"] = result.clone();
    storage::save(&path, &receipt)?;
    Ok(result)
}

/// Bound initial publication. The original attempt is immutable after completion;
/// recovery and separately authorized PATCH operations use a companion journal.
pub fn once_bound(
    root: &Path,
    request: &str,
    kind: Kind,
    candidate: &Candidate,
    payload: &Value,
    binding: Binding<'_>,
    dispatch: impl FnOnce() -> Result<Published>,
) -> Result<Value> {
    let path = lifecycle::path(root, request, kind)?;
    candidate.validate()?;
    lifecycle::check_payload(payload)?;
    if binding.authority.is_empty() || binding.publication_id == Some(0) {
        bail!("publication requires a positive object ID and bound publisher identity");
    }
    let anchor = path.with_extension("lock");
    let previous_anchor = anchor.try_exists()?;
    let _lock = storage::lock(&anchor)?;
    if storage::read::<Value>(&path)?.is_some() || previous_anchor {
        bail!("publication already attempted; use receipt-bound reconciliation/update without another create");
    }
    let mut receipt = json!({"schema_version":2,"request_id":request,"kind":kind.key(),
        "candidate":candidate,"payload_digest":lifecycle::digest(payload)?,"payload":payload,
        "authority":binding.authority,"target_id":binding.publication_id,"state":"dispatching"});
    storage::save(&path, &receipt)?;
    let result = dispatch()?;
    result.verify(binding.authority, binding.publication_id, payload)?;
    receipt["state"] = json!("confirmed");
    receipt["result"] = serde_json::to_value(&result)?;
    storage::save(&path, &receipt)?;
    Ok(serde_json::to_value(result)?)
}
