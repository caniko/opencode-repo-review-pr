use super::Kind;
use crate::storage;
use anyhow::{bail, Context, Result};
use canix_toolbelt::review::Candidate;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub struct Binding<'a> {
    pub authority: &'a str,
    pub publication_id: Option<u64>,
}

/// Explicitly selected existing forge object. Observation must use the trusted
/// adapter, never a caller-supplied JSON receipt in place of an authoritative GET.
pub struct Selection<'a> {
    pub root: &'a Path,
    pub request: &'a str,
    pub kind: Kind,
    pub candidate: &'a Candidate,
    pub authority: &'a str,
    pub publication_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Published {
    pub id: u64,
    pub authority: String,
    pub payload: Value,
    pub url: Option<String>,
}

impl Published {
    pub(super) fn verify(&self, authority: &str, id: Option<u64>, payload: &Value) -> Result<()> {
        check_payload(&self.payload)?;
        if self.id == 0
            || id.is_some_and(|id| id != self.id)
            || self.authority != authority
            || digest(&self.payload)? != digest(payload)?
            || self.url.as_ref().is_some_and(|url| url.len() > 2048)
        {
            bail!("publication object, publisher or exact payload mismatch");
        }
        Ok(())
    }
}

pub(super) fn check_payload(payload: &Value) -> Result<()> {
    if serde_json::to_vec_pretty(payload)?.len() > 1024 * 1024 {
        bail!("publication payload exceeded the 1 MiB bound");
    }
    Ok(())
}

pub(super) fn digest(value: &Value) -> Result<String> {
    Ok(storage::digest(&serde_json::to_vec(value)?))
}

pub(super) fn path(root: &Path, request: &str, kind: Kind) -> Result<PathBuf> {
    if request.len() != 64 || !request.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid publication request ID");
    }
    Ok(root
        .join("publications")
        .join(format!("{request}-{}.json", kind.key())))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    previous_digest: String,
    payload: Value,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lifecycle {
    schema_version: u32,
    origin_digest: String,
    publication_id: u64,
    authority: String,
    payload: Value,
    result: Published,
    history: Vec<Change>,
    pending: Option<Change>,
}

/// Separate durable witness: losing the mutable journal must not erase an
/// adopted object or an unknown update. Never remove this marker to retry.
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Tombstone {
    schema_version: u32,
    origin_digest: String,
    publication_id: u64,
    authority: String,
}

fn tombstone_path(journal: &Path) -> PathBuf {
    journal.with_extension("tombstone.json")
}

fn save_journal(path: &Path, journal: &Lifecycle) -> Result<()> {
    if serde_json::to_vec_pretty(journal)?.len() > 16 * 1024 * 1024 {
        bail!("publication journal exceeded its durable read bound");
    }
    storage::save(path, journal)
}

impl Selection<'_> {
    fn tombstone(&self, original: &Value) -> Result<Tombstone> {
        Ok(Tombstone {
            schema_version: 1,
            origin_digest: digest(original)?,
            publication_id: self.publication_id,
            authority: self.authority.into(),
        })
    }

    fn protect(&self, journal: &Path, original: &Value) -> Result<()> {
        let path = tombstone_path(journal);
        let expected = self.tombstone(original)?;
        if let Some(saved) = storage::read::<Tombstone>(&path)? {
            if saved != expected {
                bail!("publication lifecycle tombstone identity changed");
            }
        } else {
            // atomic rename + file and parent fsync precede every possible PATCH.
            storage::save(&path, &expected)?;
        }
        Ok(())
    }

    fn original(&self) -> Result<(PathBuf, Value)> {
        self.candidate.validate()?;
        if self.publication_id == 0 || self.authority.is_empty() {
            bail!("recovery requires a positive publication ID and publisher identity");
        }
        let original: Value = storage::read(&path(self.root, self.request, self.kind)?)?
            .context("publication has no initial attempt; recovery never creates one")?;
        if original["schema_version"].as_u64() != Some(2) {
            bail!("legacy publication lacks a pre-effect publisher/payload binding; preserve it for operator disposition");
        }
        let candidate: Candidate = serde_json::from_value(original["candidate"].clone())?;
        if original["request_id"].as_str() != Some(self.request)
            || original["kind"].as_str() != Some(self.kind.key())
            || candidate != *self.candidate
            || original["authority"].as_str() != Some(self.authority)
            || !matches!(
                original["state"].as_str(),
                Some("dispatching" | "confirmed")
            )
            || original.get("payload").is_none()
            || original["payload_digest"].as_str() != Some(digest(&original["payload"])?.as_str())
        {
            bail!("initial publication identity or payload changed");
        }
        if !original["target_id"].is_null()
            && original["target_id"].as_u64() != Some(self.publication_id)
        {
            bail!("initial publication targeted a different forge object");
        }
        if original["state"] == "confirmed" {
            let result: Published = serde_json::from_value(original["result"].clone())?;
            result.verify(
                self.authority,
                Some(self.publication_id),
                &original["payload"],
            )?;
        }
        let journal = self.root.join("publications").join(format!(
            "{}-{}-lifecycle.json",
            self.request,
            self.kind.key()
        ));
        Ok((journal, original))
    }

    fn load(&self, journal: &Path, original: &Value) -> Result<Option<Lifecycle>> {
        let protected = storage::read::<Tombstone>(&tombstone_path(journal))?;
        if let Some(saved) = &protected {
            if *saved != self.tombstone(original)? {
                bail!("publication lifecycle tombstone identity changed");
            }
        }
        let Some(saved): Option<Lifecycle> = storage::read(journal)? else {
            if protected.is_some() {
                bail!("publication lifecycle journal missing behind its durable tombstone; owner disposition required, never replay writes");
            }
            return Ok(None);
        };
        if saved.schema_version != 1
            || saved.origin_digest != digest(original)?
            || saved.publication_id != self.publication_id
            || saved.authority != self.authority
            || saved.history.len() > 100
        {
            bail!("publication lifecycle identity changed or exceeded its bound");
        }
        let mut previous = &original["payload"];
        for change in &saved.history {
            if change.previous_digest != digest(previous)? {
                bail!("publication history chain changed");
            }
            previous = &change.payload;
        }
        let current_digest = digest(&saved.payload)?;
        if digest(previous)? != current_digest
            || saved
                .pending
                .as_ref()
                .is_some_and(|c| c.previous_digest != current_digest)
        {
            bail!("publication lifecycle payload changed");
        }
        saved
            .result
            .verify(self.authority, Some(self.publication_id), &saved.payload)?;
        Ok(Some(saved))
    }
}

fn receipt(saved: &Lifecycle) -> Result<Value> {
    Ok(
        json!({"schema_version":1,"publication_id":saved.publication_id,"authority":saved.authority,
        "payload_digest":digest(&saved.payload)?,"update_pending":saved.pending.is_some(),
        "updates":saved.history.len(),"merge_authorized":false}),
    )
}

/// Observe/adopt the exact effect of the original create or pending PATCH. Absence,
/// deletion, an old payload or a changed publisher never authorizes another write.
pub fn reconcile(
    selected: Selection<'_>,
    observe: impl FnOnce() -> Result<Published>,
) -> Result<Value> {
    let _lock = storage::lock(
        &path(selected.root, selected.request, selected.kind)?.with_extension("lock"),
    )?;
    let (journal, original) = selected.original()?;
    let saved = selected.load(&journal, &original)?;
    let observed = observe()?;
    let expected = saved.as_ref().map_or(&original["payload"], |s| {
        s.pending.as_ref().map_or(&s.payload, |p| &p.payload)
    });
    observed.verify(selected.authority, Some(selected.publication_id), expected)?;
    let mut saved = saved.unwrap_or(Lifecycle {
        schema_version: 1,
        origin_digest: digest(&original)?,
        publication_id: selected.publication_id,
        authority: selected.authority.into(),
        payload: original["payload"].clone(),
        result: observed.clone(),
        history: vec![],
        pending: None,
    });
    if let Some(change) = saved.pending.take() {
        saved.payload = change.payload.clone();
        saved.history.push(change);
    }
    saved.result = observed;
    selected.protect(&journal, &original)?;
    save_journal(&journal, &saved)?;
    receipt(&saved)
}

/// Separately authorized update of an already reconciled object. The caller must
/// supply the preview's previous payload digest. No POST/create route exists here.
pub fn update(
    selected: Selection<'_>,
    expected_digest: &str,
    payload: &Value,
    apply: bool,
    observe: impl FnOnce() -> Result<Published>,
    patch: impl FnOnce(u64) -> Result<Published>,
) -> Result<Value> {
    check_payload(payload)?;
    let _lock = storage::lock(
        &path(selected.root, selected.request, selected.kind)?.with_extension("lock"),
    )?;
    let (journal, original) = selected.original()?;
    let mut saved = selected
        .load(&journal, &original)?
        .context("reconcile the original publication before updating it")?;
    if saved.pending.is_some() {
        bail!("publication update outcome is unknown; reconcile it without another PATCH");
    }
    if saved.history.len() >= 100 || digest(&saved.payload)? != expected_digest {
        bail!("publication update requires the exact previous payload digest and bounded history");
    }
    observe()?.verify(
        selected.authority,
        Some(selected.publication_id),
        &saved.payload,
    )?;
    let changed = digest(payload)? != expected_digest;
    let mut report = receipt(&saved)?;
    report["would_update"] = json!(changed);
    report["new_payload_digest"] = json!(digest(payload)?);
    report["applies_update"] = json!(apply && changed);
    if !apply || !changed {
        return Ok(report);
    }
    saved.pending = Some(Change {
        previous_digest: expected_digest.into(),
        payload: payload.clone(),
    });
    // Prove the confirmed form will also fit before allowing the effect. Leave
    // headroom for the bounded returned URL and pretty-print indentation.
    let mut projected = saved.clone();
    let change = projected
        .pending
        .take()
        .context("update journal missing its pending effect")?;
    projected.payload = change.payload.clone();
    projected.history.push(change);
    projected.result.payload = payload.clone();
    if serde_json::to_vec_pretty(&projected)?.len() > 8 * 1024 * 1024 {
        bail!("publication update would exceed the bounded journal capacity");
    }
    // Also upgrades intact older journals before their first new-version PATCH.
    selected.protect(&journal, &original)?;
    save_journal(&journal, &saved)?;
    let result = patch(selected.publication_id)?;
    result.verify(selected.authority, Some(selected.publication_id), payload)?;
    let change = saved
        .pending
        .take()
        .context("update journal missing its pending effect")?;
    saved.payload = change.payload.clone();
    saved.history.push(change);
    saved.result = result;
    save_journal(&journal, &saved)?;
    let mut confirmed = receipt(&saved)?;
    confirmed["would_update"] = json!(true);
    confirmed["applies_update"] = json!(true);
    Ok(confirmed)
}
