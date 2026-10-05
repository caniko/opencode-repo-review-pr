//! Greptile CLI 3.2.3 contract, inspected from its integrity-verified npm artifact.
use crate::{config::Greptile, process, storage};
use anyhow::{bail, Context, Result};
use canix_toolbelt::review::{
    Candidate, Capabilities, Error, Evidence, Finding, Intent, Policy, Provider, Review,
};
use serde_json::{json, Value};
use std::path::PathBuf;

pub struct GreptileCli {
    pub settings: Greptile,
    pub snapshot: PathBuf,
    pub state: PathBuf,
    pub comparison_base: String,
}

impl GreptileCli {
    fn run(&self, args: &[&str]) -> Result<(i32, Value, String)> {
        self.authenticate()?;
        let (exit, out, err) = process::run(
            self.settings
                .cli
                .as_ref()
                .context("CLI executable missing")?,
            args,
            &self.snapshot,
            &self.state.join("process"),
            30,
        )?;
        Ok((
            exit,
            serde_json::from_slice(&out)
                .context("Greptile CLI returned an unsupported JSON document")?,
            String::from_utf8(err)?,
        ))
    }

    fn authenticate(&self) -> Result<()> {
        if self.settings.cli_qualification_receipt.is_empty() {
            bail!("CLI linked-account and scope qualification missing");
        }
        if std::env::var_os("GREPTILE_API_KEY").is_some() {
            bail!("unattributed Greptile API-key reviews cannot use the free allowance");
        }
        let auth = std::fs::read(
            self.settings
                .cli_auth_file
                .as_ref()
                .context("CLI OAuth credential owner reference missing")?,
        )?;
        if auth_identity_digest(&auth)? != self.settings.cli_auth_digest {
            bail!(
                "Greptile OAuth identity changed; reconcile qualification without modifying credentials"
            );
        }
        Ok(())
    }

    pub fn normalize(
        content: &Value,
        status: &Value,
        candidate: &Candidate,
        policy: &Policy,
        comparison_base: &str,
    ) -> Result<Review> {
        if status["status"].as_str() != Some("COMPLETED")
            || status["headSha"].as_str() != Some(candidate.head.as_str())
            || status["baseSha"].as_str() != Some(comparison_base)
        {
            bail!("Greptile CLI execution/comparison attestation mismatch");
        }
        let id = status["runId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("run ID missing")?;
        let comments = content["comments"]
            .as_array()
            .context("complete CLI comments missing")?;
        if status["commentCount"].as_u64() != Some(comments.len() as u64) {
            bail!("CLI findings completeness mismatch");
        }
        let mut findings = vec![];
        for item in comments {
            let body = item["body"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .context("finding body missing")?;
            let finding_id = item["id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| item["id"].as_u64().map(|id| id.to_string()))
                .context("finding ID missing")?;
            findings.push(Finding {
                id: finding_id,
                body: body.into(),
                path: item["path"].as_str().map(str::to_owned),
                line: item["startLine"].as_u64(),
                severity: item["severity"].as_str().map(str::to_owned),
                provider_addressed: false,
                correlated: true,
                url: format!("greptile:run:{id}"),
            });
        }
        // Nonempty summary/analysis remains evidence to triage, never a score-based pass.
        for field in [
            "/summary",
            "/overallComment/prose",
            "/overallComment/summary",
            "/securitySummary",
        ] {
            if let Some(body) = content
                .pointer(field)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                findings.push(Finding {
                    id: format!("summary-{id}-{field}"),
                    body: body.into(),
                    path: None,
                    line: None,
                    severity: None,
                    provider_addressed: false,
                    correlated: true,
                    url: format!("greptile:run:{id}"),
                });
            }
        }
        if let Some(analyses) = content["fileAnalyses"].as_array() {
            for (index, analysis) in analyses.iter().enumerate() {
                if let Some(body) = analysis["summary"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                {
                    findings.push(Finding {
                        id: format!("analysis-{id}-{index}"),
                        body: body.into(),
                        path: analysis["path"].as_str().map(str::to_owned),
                        line: None,
                        severity: None,
                        provider_addressed: false,
                        correlated: true,
                        url: format!("greptile:run:{id}"),
                    });
                }
            }
        }
        Ok(Review {
            id: id.into(),
            provider: "greptile".into(),
            reviewed_head: Some(candidate.head.clone()),
            requested_base: Some(candidate.base.clone()),
            policy_digest: policy.digest()?,
            completed: true,
            complete_findings: true,
            findings,
            url: format!("greptile:run:{id}"),
        })
    }
}

impl Provider for GreptileCli {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            provider: "greptile".into(),
            transport: "cli-v3.2.3".into(),
            drafts: true,
            idempotent_submission: false,
            revision_attestation: true,
        }
    }
    fn inspect(
        &mut self,
        candidate: &Candidate,
        policy: &Policy,
        intent: Option<&Intent>,
    ) -> std::result::Result<Evidence, Error> {
        (|| -> Result<Evidence> {
            let dispatch: Option<Value> = intent
                .map(|intent| storage::read(&self.state.join(format!("{}.json", intent.id))))
                .transpose()?.flatten();
            if let (Some(intent), Some(dispatch)) = (intent, dispatch.as_ref()) {
                let dispatch_intent: Intent = serde_json::from_value(dispatch["intent"].clone())?;
                if dispatch_intent.id != intent.id || !dispatch_intent.matches(candidate, policy) {
                    bail!("CLI persisted intent differs from the frozen request");
                }
                if let Some(failure) = dispatch.get("worker_failure") {
                    bail!(
                        "CLI worker failed during {}; reconcile the existing attempt without replay",
                        failure["phase"].as_str().unwrap_or("unknown")
                    );
                }
            }
            let (exit, status, warnings) =
                self.run(&["review", "status", "--commit", &candidate.head, "--json"])?;
            let mut evidence = Evidence {
                known_review_ids: status["runId"]
                    .as_str()
                    .map(|id| vec![id.into()])
                    .unwrap_or_default(),
                recovered_intent: None,
                request_receipt: None,
                review: None,
            };
            if intent.is_none()
                && status["runId"].is_null()
                && status["status"].is_null()
                && status["commit"].as_str() == Some(candidate.head.as_str())
            {
                return Ok(evidence);
            }
            if !warnings.trim().is_empty() {
                bail!("CLI status could not be authoritatively confirmed");
            }
            let Some(intent) = intent else {
                return Ok(evidence);
            };
            let Some(dispatch) = dispatch else {
                return Ok(evidence);
            };
            let id = status["runId"]
                .as_str()
                .filter(|id| !intent.baseline_review_ids.iter().any(|old| old == id));
            let Some(id) = id else {
                return Ok(evidence);
            };
            if status["headSha"].as_str() != Some(candidate.head.as_str())
                || status["baseSha"].as_str() != Some(self.comparison_base.as_str())
            {
                bail!("CLI comparison changed");
            }
            evidence.request_receipt = Some(format!("greptile:run:{id}"));
            if exit == 0 {
                let (exit, content, show_warnings) = self.run(&["review", "show", id, "--json"])?;
                if exit != 0 || !warnings.trim().is_empty() || !show_warnings.trim().is_empty() {
                    bail!("CLI review failed or excluded required input");
                }
                if dispatch["exit"].as_i64() != Some(0)
                    || dispatch["warnings"].as_str() != Some("")
                    || dispatch["content"] != content
                    || dispatch.pointer("/status/runId").and_then(Value::as_str) != Some(id)
                {
                    bail!("CLI input coverage unavailable or exclusions reported");
                }
                evidence.review = Some(Self::normalize(
                    &content,
                    &status,
                    candidate,
                    policy,
                    &self.comparison_base,
                )?);
            } else if status["status"].as_str() != Some("IN_FLIGHT") {
                bail!("CLI review failed/cancelled; charging outcome needs reconciliation");
            }
            Ok(evidence)
        })()
        .map_err(|e| Error(e.to_string()))
    }
    fn submit(&mut self, intent: &Intent) -> std::result::Result<String, Error> {
        (||->Result<String>{
            self.authenticate()?;
            let path=self.state.join(format!("{}.json",intent.id));
            if storage::read::<Value>(&path)?.is_some(){bail!("CLI submission already attempted; reconcile it");}
            storage::save(&path,&json!({"intent":intent,"warnings":null}))?;
            // A CLI-specific worker owns the wait independently of engine locks.
            let payload=json!({"settings":self.settings,"snapshot":self.snapshot,"intent":intent,"state":self.state,"comparison_base":self.comparison_base});
            let worker=self.state.join(format!("{}-worker.json",intent.id));storage::save(&worker,&payload)?;
            std::process::Command::new(std::env::current_exe()?).args(["cli-worker","--receipt"]).arg(&worker)
                .env_remove("REPO_REVIEW_COORDINATOR").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?;
            // Spawn is not remote acceptance. The next inspection adopts a proven run ID.
            bail!("CLI worker started; remote acceptance unconfirmed, resume this same request")
        })().map_err(|e|Error(e.to_string()))
    }
}

pub fn worker(path: &std::path::Path) -> Result<()> {
    let payload: Value = storage::read(path)?.context("worker intent missing")?;
    let settings: Greptile = serde_json::from_value(payload["settings"].clone())?;
    let snapshot: PathBuf = serde_json::from_value(payload["snapshot"].clone())?;
    let state: PathBuf = serde_json::from_value(payload["state"].clone())?;
    let intent: Intent = serde_json::from_value(payload["intent"].clone())?;
    let provider = GreptileCli {
        settings,
        snapshot,
        state,
        comparison_base: payload["comparison_base"]
            .as_str()
            .context("worker comparison base missing")?
            .into(),
    };
    let _lock = storage::lock(&provider.state.join("worker.lock"))?;
    let receipt_path = provider.state.join(format!("{}.json", intent.id));
    let receipt: Value =
        storage::read(&receipt_path)?.context("write-ahead CLI dispatch missing")?;
    if receipt.get("worker_started").and_then(Value::as_bool) == Some(true) {
        bail!("CLI worker already attempted; reconcile without replay");
    }
    let mut receipt = json!({"intent":intent,"warnings":null,"worker_started":true});
    storage::save(&receipt_path, &receipt)?;
    let mut phase = "authentication";
    let mut review_dispatch_attempted = false;
    let result = (|| -> Result<()> {
        provider.authenticate()?;
        phase = "version";
        let exe = provider.settings.cli.as_ref().context("CLI path missing")?;
        let (exit, version, _) = process::run(
            exe,
            &["--version"],
            &provider.snapshot,
            &provider.state.join("process"),
            10,
        )?;
        if exit != 0 || std::str::from_utf8(&version)?.trim() != "3.2.3" {
            bail!("expected qualified Greptile CLI 3.2.3");
        }
        phase = "review";
        review_dispatch_attempted = true;
        let (exit, content, warnings) = process::run(
            exe,
            &["review", "--branch", &intent.candidate.base, "--json"],
            &provider.snapshot,
            &provider.state.join("process"),
            600,
        )?;
        receipt["exit"] = json!(exit);
        receipt["content"] = serde_json::from_slice::<Value>(&content).unwrap_or(Value::Null);
        receipt["warnings"] = json!(String::from_utf8(warnings)?);
        // Retain returned review evidence even if the separate status query fails.
        // It still cannot establish acceptance without the exact run attestation.
        storage::save(&receipt_path, &receipt)?;
        phase = "status";
        let (_, status, status_warnings) = provider.run(&[
            "review",
            "status",
            "--commit",
            &intent.candidate.head,
            "--json",
        ])?;
        receipt["warnings"] = json!(format!(
            "{}{}",
            receipt["warnings"].as_str().unwrap_or_default(),
            status_warnings
        ));
        receipt["status"] = status;
        storage::save(&receipt_path, &receipt)
    })();
    if result.is_err() {
        // Diagnostic categories are locally generated; credential bytes and
        // arbitrary process errors are never copied into this failure record.
        receipt["worker_failure"] =
            json!({"phase":phase,"review_dispatch_attempted":review_dispatch_attempted});
        storage::save(&receipt_path, &receipt)?;
    }
    result
}

/// Bind immutable OAuth subject/email, allowing normal access-token refreshes.
pub fn auth_identity_digest(bytes: &[u8]) -> Result<String> {
    let value: Value = serde_json::from_slice(bytes)?;
    if value["method"].as_str() != Some("oauth")
        || value["subject"].as_str().is_none_or(str::is_empty)
    {
        bail!("Greptile CLI requires qualified attributed OAuth authentication");
    }
    Ok(storage::digest(&serde_json::to_vec(&(
        value["method"].clone(),
        value["subject"].clone(),
        value["email"].clone(),
    ))?))
}
