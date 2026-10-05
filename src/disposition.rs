use crate::storage;
use anyhow::{bail, Result};
use canix_toolbelt::review::{Candidate, Capabilities, Error, Evidence, Intent, Policy, Provider};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disposition {
    pub candidate: Candidate,
    pub policy_digest: String,
    pub review_id: String,
    pub finding_id: String,
    pub body_digest: String,
    pub reason: String,
    pub evidence: String,
}

pub fn record(path: &Path, item: Disposition) -> Result<()> {
    if item.reason.trim().is_empty() || item.evidence.trim().is_empty() {
        bail!("disposition requires a source-grounded reason and evidence reference");
    }
    let _lock = storage::lock(&path.with_extension("lock"))?;
    let mut items: Vec<Disposition> = storage::read(path)?.unwrap_or_default();
    items.push(item);
    storage::save(path, &items)
}

pub struct Disposed<'a> {
    pub inner: &'a mut dyn Provider,
    pub path: &'a Path,
}
impl Provider for Disposed<'_> {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn next_poll_at(&self) -> Option<u64> {
        self.inner.next_poll_at()
    }
    fn submit(&mut self, intent: &Intent) -> std::result::Result<String, Error> {
        self.inner.submit(intent)
    }
    fn inspect(
        &mut self,
        candidate: &Candidate,
        policy: &Policy,
        intent: Option<&Intent>,
    ) -> std::result::Result<Evidence, Error> {
        let mut evidence = self.inner.inspect(candidate, policy, intent)?;
        let items: Vec<Disposition> = storage::read(self.path)
            .map_err(|e| Error(e.to_string()))?
            .unwrap_or_default();
        if let Some(review) = &mut evidence.review {
            review.findings.retain(|finding| {
                !items.iter().any(|d| {
                    d.candidate == *candidate
                        && d.policy_digest == review.policy_digest
                        && d.review_id == review.id
                        && d.finding_id == finding.id
                        && d.body_digest == storage::digest(finding.body.as_bytes())
                        && !d.reason.trim().is_empty()
                        && !d.evidence.trim().is_empty()
                })
            });
        }
        Ok(evidence)
    }
}
