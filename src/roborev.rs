use crate::{config::Roborev as Settings, router::Scope, storage};
use anyhow::{bail, Context, Result};
use canix_toolbelt::review::{
    Candidate, Capabilities, Error, Evidence, Finding, Intent, Policy, Provider, Review,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

const MAX_RESPONSE: u64 = 16 * 1024 * 1024;

pub struct Daemon {
    socket: PathBuf,
}

impl Daemon {
    pub fn new(settings: &Settings) -> Result<Self> {
        if !settings.qualified || settings.qualification_receipt.is_empty() {
            bail!(
                "roborev worker is not qualified; provide its containment receipt in trusted policy"
            );
        }
        Ok(Self {
            socket: settings.socket.clone().context("roborev socket missing")?,
        })
    }

    /// Bounded HTTP over a private Unix socket, with no proxy or autostart route.
    pub fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        if !path.starts_with("/api/") || path.contains(['\r', '\n']) {
            bail!("invalid daemon path");
        }
        let mut socket =
            UnixStream::connect(&self.socket).context("configured roborev daemon unavailable")?;
        socket.set_read_timeout(Some(Duration::from_secs(20)))?;
        socket.set_write_timeout(Some(Duration::from_secs(20)))?;
        let body = body
            .map(|v| serde_json::to_vec(&v))
            .transpose()?
            .unwrap_or_default();
        write!(
            socket,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )?;
        socket.write_all(&body)?;
        let mut response = vec![];
        socket.take(MAX_RESPONSE + 1).read_to_end(&mut response)?;
        if response.len() as u64 > MAX_RESPONSE {
            bail!("daemon response exceeded size limit");
        }
        decode_http(&response)
    }
}

pub fn decode_http(response: &[u8]) -> Result<Value> {
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("incomplete daemon HTTP headers")?;
    let headers = std::str::from_utf8(&response[..split])?;
    let status = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .context("missing HTTP status")?;
    if status != "200" && status != "201" {
        bail!("roborev HTTP {status}; submission will not be replayed");
    }
    let mut body = response[split + 4..].to_vec();
    if headers
        .lines()
        .skip(1)
        .any(|h| h.eq_ignore_ascii_case("transfer-encoding: chunked"))
    {
        let mut decoded = vec![];
        let mut cursor = 0;
        loop {
            let end = body[cursor..]
                .windows(2)
                .position(|w| w == b"\r\n")
                .context("truncated HTTP chunk")?
                + cursor;
            let size = usize::from_str_radix(
                std::str::from_utf8(&body[cursor..end])?
                    .split(';')
                    .next()
                    .context("missing chunk size")?,
                16,
            )?;
            cursor = end + 2;
            if size == 0 {
                // The last chunk still requires a complete trailer section.
                // Valid JSON alone cannot prove the HTTP reply was complete.
                loop {
                    let end = body[cursor..]
                        .windows(2)
                        .position(|w| w == b"\r\n")
                        .context("truncated HTTP trailers")?
                        + cursor;
                    let trailer = &body[cursor..end];
                    cursor = end + 2;
                    if trailer.is_empty() {
                        break;
                    }
                    let (name, _) = std::str::from_utf8(trailer)?
                        .split_once(':')
                        .context("invalid HTTP trailer")?;
                    if name.is_empty()
                        || name.chars().any(|c| c.is_whitespace() || c.is_control())
                        || name.eq_ignore_ascii_case("content-length")
                        || name.eq_ignore_ascii_case("transfer-encoding")
                    {
                        bail!("invalid HTTP trailer");
                    }
                }
                if cursor != body.len() {
                    bail!("trailing bytes after chunked response");
                }
                break;
            }
            let end = cursor.checked_add(size).context("chunk size overflow")?;
            if end.checked_add(2).is_none_or(|n| n > body.len()) || body[end..end + 2] != *b"\r\n" {
                bail!("truncated chunk body");
            }
            decoded.extend_from_slice(&body[cursor..end]);
            cursor = end + 2;
        }
        body = decoded;
    } else if let Some(length) = headers.lines().skip(1).find_map(|h| {
        h.split_once(':')
            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.trim())
    }) {
        if length.parse::<usize>()? != body.len() {
            bail!("truncated daemon response");
        }
    }
    Ok(serde_json::from_slice(&body)?)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Submission {
    intent: Intent,
    snapshot: PathBuf,
    git_ref: String,
    baseline: Vec<String>,
    job: Option<Value>,
    dispatching: bool,
    diff_digest: Option<String>,
}

pub struct Roborev {
    daemon: Daemon,
    settings: Settings,
    snapshot: PathBuf,
    git_ref: String,
    diff: Option<String>,
    expected_files: usize,
    state: PathBuf,
    scope: Scope,
}

impl Roborev {
    pub fn new(
        settings: &Settings,
        snapshot: PathBuf,
        git_ref: String,
        diff: Option<String>,
        expected_files: usize,
        state: PathBuf,
        scope: Scope,
    ) -> Result<Self> {
        Ok(Self {
            daemon: Daemon::new(settings)?,
            settings: settings.clone(),
            snapshot,
            git_ref,
            diff,
            expected_files,
            state,
            scope,
        })
    }

    fn path(&self, intent: &Intent) -> PathBuf {
        self.state
            .join(format!("{}.json", storage::digest(intent.id.as_bytes())))
    }

    fn jobs(&self) -> Result<Vec<Value>> {
        let mut url = url::Url::parse("http://localhost/api/jobs")?;
        url.query_pairs_mut()
            .append_pair(
                "repo",
                self.snapshot
                    .to_str()
                    .context("snapshot path is not UTF-8")?,
            )
            .append_pair("git_ref", &self.git_ref)
            .append_pair("limit", "100");
        let data = self.daemon.request(
            "GET",
            &format!("{}?{}", url.path(), url.query().unwrap_or_default()),
            None,
        )?;
        if data["has_more"].as_bool() != Some(false) {
            bail!("daemon job listing completeness unproven");
        }
        Ok(data["jobs"].as_array().context("missing jobs")?.clone())
    }

    fn matching(&self, job: &Value) -> bool {
        job["repo_path"].as_str() == self.snapshot.to_str()
            && job["git_ref"].as_str() == Some(self.git_ref.as_str())
            && job["agent"].as_str() == Some(self.settings.agent.as_str())
            && job["agentic"].as_bool() == Some(false)
            && job["min_severity"].as_str() == Some("low")
            && job.get("panel_run_uuid").is_none_or(Value::is_null)
            && job["backup_agent"].as_str().is_none_or(str::is_empty)
    }

    fn matching_diff(&self, job: &Value) -> bool {
        self.diff
            .as_ref()
            .is_none_or(|diff| job["diff_content"].as_str() == Some(diff.as_str()))
    }

    fn job_with_diff(&self, listed: &Value) -> Result<Value> {
        if self.diff.is_none() {
            return Ok(listed.clone());
        }
        // v0.71 list queries omit diff_content; the ID lookup hydrates it.
        // Never infer exact working-tree bytes from metadata or a file count.
        let data =
            self.daemon
                .request("GET", &format!("/api/jobs?id={}", job_id(listed)?), None)?;
        let jobs = data["jobs"].as_array().context("missing job detail")?;
        if jobs.len() != 1 || data["has_more"].as_bool() != Some(false) {
            bail!("working-tree job detail missing or ambiguous");
        }
        let job = &jobs[0];
        if !self.matching(job)
            || !self.matching_diff(job)
            || job["id"] != listed["id"]
            || job["uuid"] != listed["uuid"]
        {
            bail!("working-tree job diff or identity mismatch");
        }
        Ok(job.clone())
    }
}

fn job_id(job: &Value) -> Result<String> {
    Ok(job["id"].as_u64().context("job ID missing")?.to_string())
}
fn error(error: anyhow::Error) -> Error {
    Error(error.to_string())
}

impl Provider for Roborev {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            provider: "roborev".into(),
            transport: "unix-http-v0.71".into(),
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
            let jobs = self.jobs()?;
            let mut evidence = Evidence {
                known_review_ids: jobs.iter().map(job_id).collect::<Result<_>>()?,
                recovered_intent: None,
                request_receipt: None,
                review: None,
            };
            let Some(intent) = intent else {
                return Ok(evidence);
            };
            let Some(mut saved): Option<Submission> = storage::read(&self.path(intent))? else {
                return Ok(evidence);
            };
            if saved.snapshot != self.snapshot
                || saved.git_ref != self.git_ref
                || !saved.intent.matches(candidate, policy)
                || saved.diff_digest != self.diff.as_ref().map(|d| storage::digest(d.as_bytes()))
            {
                bail!("daemon intent identity mismatch");
            }
            let matching: Vec<_> = jobs
                .iter()
                .filter(|job| {
                    self.matching(job) && job_id(job).is_ok_and(|id| !saved.baseline.contains(&id))
                })
                .collect();
            let job = if let Some(existing) = &saved.job {
                let id = job_id(existing)?;
                matching
                    .iter()
                    .find(|j| job_id(j).ok().as_ref() == Some(&id))
                    .copied()
                    .context("persisted daemon job disappeared")?
            } else if matching.len() == 1 {
                matching[0]
            } else if matching.is_empty() {
                return Ok(evidence);
            } else {
                bail!("ambiguous daemon enqueue; multiple jobs match the immutable snapshot");
            };
            let detailed = self.job_with_diff(job)?;
            let job = &detailed;
            if job["uuid"].as_str().is_none() {
                bail!("job UUID missing");
            }
            if saved
                .job
                .as_ref()
                .is_some_and(|old| old["uuid"] != job["uuid"])
            {
                bail!("job UUID changed");
            }
            saved.job = Some(job.clone());
            storage::save(&self.path(intent), &saved)?;
            evidence.request_receipt = Some(format!(
                "roborev:job:{}:{}",
                job_id(job)?,
                job["uuid"].as_str().context("missing UUID")?
            ));
            match job["status"].as_str() {
                Some("queued" | "running") => {}
                Some("done") => {
                    let raw = self.daemon.request(
                        "GET",
                        &format!("/api/review?job_id={}", job_id(job)?),
                        None,
                    )?;
                    evidence.review = Some(normalize_review(
                        &raw,
                        job,
                        candidate,
                        policy,
                        self.expected_files,
                        self.scope,
                    )?);
                }
                _ => bail!("daemon job failed, interrupted or returned an unknown execution state"),
            }
            Ok(evidence)
        })()
        .map_err(error)
    }

    fn submit(&mut self, intent: &Intent) -> std::result::Result<String, Error> {
        (|| -> Result<String> {
            verify_comparison(&self.git_ref, &intent.candidate, self.scope)?;
            let path = self.path(intent);
            if storage::read::<Submission>(&path)?.is_some() { bail!("enqueue already attempted; reconcile instead of replaying"); }
            let baseline = self.jobs()?.iter().map(job_id).collect::<Result<Vec<_>>>()?;
            let mut saved = Submission { intent: intent.clone(), snapshot: self.snapshot.clone(), git_ref: self.git_ref.clone(), baseline, job: None, dispatching: true, diff_digest:self.diff.as_ref().map(|d|storage::digest(d.as_bytes())) };
            storage::save(&path, &saved)?;
            let mut body = json!({"repo_path": self.snapshot, "git_ref": self.git_ref, "agent": self.settings.agent, "agentic": false, "review_type": "default", "panel": "none", "min_severity": "low"});
            if let Some(diff) = &self.diff { body["diff_content"] = Value::String(diff.clone()); }
            let job = self.daemon.request("POST", "/api/enqueue", Some(body))?;
            if !self.matching(&job) || !self.matching_diff(&job) || job["uuid"].as_str().is_none() { bail!("enqueue returned a mismatched job"); }
            let receipt = format!("roborev:job:{}:{}", job_id(&job)?, job["uuid"].as_str().context("missing UUID")?);
            saved.job = Some(job);
            saved.dispatching = false;
            storage::save(&path, &saved)?;
            Ok(receipt)
        })().map_err(error)
    }
}

fn verify_comparison(git_ref: &str, candidate: &Candidate, scope: Scope) -> Result<()> {
    if matches!(scope, Scope::PullRequest)
        && git_ref != format!("{}..{}", candidate.base, candidate.head)
    {
        bail!("PR review must compare the full observed target SHA to the source SHA");
    }
    Ok(())
}

pub fn normalize_review(
    raw: &Value,
    job: &Value,
    candidate: &Candidate,
    policy: &Policy,
    expected_files: usize,
    scope: Scope,
) -> Result<Review> {
    if raw["job_id"] != job["id"]
        || raw.pointer("/job/uuid") != job.get("uuid")
        || raw.pointer("/job/git_ref") != job.get("git_ref")
    {
        bail!("review/job correlation mismatch");
    }
    verify_comparison(
        job["git_ref"]
            .as_str()
            .context("persisted comparison missing")?,
        candidate,
        scope,
    )?;
    let document = raw
        .get("structured_output")
        .context("canonical review output missing")?;
    if document["schema_version"].as_u64() != Some(2)
        || !document["summary"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty())
        || !matches!(document["verdict"].as_str(), Some("pass" | "fail"))
    {
        bail!("malformed or legacy review content");
    }
    let findings = document["findings"]
        .as_array()
        .context("missing findings array")?;
    let mut normalized = vec![];
    let review_id = raw["id"].as_u64().context("review ID missing")?.to_string();
    for (index, finding) in findings.iter().enumerate() {
        let problem = finding["problem"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .context("finding problem missing")?;
        let fix = finding["fix"].as_str().context("finding fix missing")?;
        let severity = finding["severity"]
            .as_str()
            .context("finding severity missing")?;
        if !matches!(severity, "critical" | "high" | "medium" | "low" | "info") {
            bail!("unknown finding severity");
        }
        normalized.push(Finding {
            id: format!(
                "{review_id}:{index}:{}",
                storage::digest(&serde_json::to_vec(finding)?)
            ),
            body: format!("{problem}\n\nSuggested fix: {fix}"),
            path: finding["location"].as_str().map(str::to_owned),
            line: None,
            severity: Some(severity.into()),
            provider_addressed: false,
            correlated: true,
            url: format!("roborev:review:{review_id}"),
        });
    }
    if normalized.is_empty() && document["verdict"].as_str() != Some("pass") {
        bail!("failed verdict with no explanatory findings");
    }
    let complete_findings = raw
        .pointer("/file_coverage/excluded")
        .and_then(Value::as_u64)
        == Some(0)
        && raw
            .pointer("/file_coverage/reviewed")
            .and_then(Value::as_u64)
            == Some(expected_files as u64);
    Ok(Review {
        id: review_id.clone(),
        provider: "roborev".into(),
        reviewed_head: Some(candidate.head.clone()),
        requested_base: Some(candidate.base.clone()),
        policy_digest: policy.digest()?,
        completed: true,
        complete_findings,
        findings: normalized,
        url: format!("roborev:review:{review_id}"),
    })
}
