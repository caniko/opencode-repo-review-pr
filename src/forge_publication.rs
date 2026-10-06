//! Explicit forge publication and receipt-bound repair, separate from review dispatch.
use crate::{
    app::{Application, Report, Target},
    config::Config,
    forgejo::Forgejo,
    github_api,
    identity::{ForgeKind, Repository},
    publication::{self, Binding, Kind, Published, Selection},
    router::{self, Scope},
    storage,
};
use anyhow::{bail, Context, Result};
use canix_toolbelt::review::{Candidate, Forge, GitHub, Policy, Verdict};
use serde_json::{json, Value};
use std::path::Path;

pub fn github_payload(
    report: &Report,
    candidate: &Candidate,
    policy: &Policy,
    details: &str,
) -> Result<Value> {
    let details = url::Url::parse(details)?;
    if details.scheme() != "https" || !details.username().is_empty() || details.password().is_some()
    {
        bail!("details URL must be HTTPS without credentials");
    }
    require_completed(report, candidate)?;
    let ready = report.outcome.verdict == Verdict::Ready
        && report.outcome.blockers.is_empty()
        && report.outcome.review.as_ref().is_some_and(|r| {
            policy.evaluate(candidate, r) == Verdict::Ready
                    // Local advisory acceptance is broader than an exact PR
                    // check. Legacy records must not gain forge enforcement.
                    && (r.provider != "roborev"
                        || r.reviewed_base.as_ref() == Some(&candidate.base))
        });
    Ok(
        json!({"name":"review-policy","head_sha":candidate.head,"status":"completed",
        "conclusion":if ready {"success"} else {"failure"},"details_url":details.as_str(),
        "external_id":report.route.id,"output":{"title":if ready {"Exact-comparison review qualifies"} else {"Review remains blocked"},
        "summary":"Trusted revision-bound review evidence; required CI and final guarded merge are independently enforced."}}),
    )
}

pub fn require_completed(report: &Report, candidate: &Candidate) -> Result<()> {
    if report.advisory
        || report.route.candidate != *candidate
        || report.outcome.candidate != *candidate
        || report.outcome.request_id.as_deref() != Some(report.route.id.as_str())
        || matches!(report.outcome.verdict, Verdict::Pending | Verdict::Stale)
        || !report
            .outcome
            .review
            .as_ref()
            .is_some_and(|review| review.completed)
    {
        bail!("publication requires current completed evidence for the exact request/comparison");
    }
    Ok(())
}

pub fn forgejo_payload(report: &Report) -> Result<Value> {
    require_completed(report, &report.route.candidate)?;
    let candidate = &report.route.candidate;
    let marker = format!(
        "<!-- repo-review:{} -->",
        storage::digest(report.route.id.as_bytes())
    );
    Ok(
        json!({"body":format!("{marker}\n\nAdvisory repository review (merge enforcement is separate).\n\nHead: `{}`\nBase: `{}`\n\n{}",
        candidate.head, candidate.base, serde_json::to_string_pretty(report)?)}),
    )
}

type Request = dyn Fn(&str, &str, Option<Value>) -> Result<Value>;

/// No configurable endpoint or credential fallback. Test transports are injected
/// only by this module's private fixtures; production uses the existing HTTPS APIs.
pub(crate) struct Publisher {
    repository: Repository,
    candidate: Candidate,
    request_id: String,
    authority: String,
    kind: Kind,
    request: Box<Request>,
    candidate_read: Box<dyn Fn() -> Result<Candidate>>,
}

impl Publisher {
    pub(crate) fn new(config: &Config, candidate: &Candidate, request_id: &str) -> Result<Self> {
        config.validate()?;
        candidate.validate()?;
        let (repository, _) = Repository::from_pr(&candidate.url, &config.forges)?;
        #[cfg(test)]
        if let Some(fixture) = crate::publication_fixture::active() {
            let fresh = fixture.clone();
            return Ok(Self {
                kind: if repository.forge == ForgeKind::GitHub {
                    Kind::GitHubCheck
                } else {
                    Kind::ForgejoComment
                },
                authority: if repository.forge == ForgeKind::GitHub {
                    format!(
                        "github-app:{}",
                        config.policy_app_id.context("fixture App missing")?
                    )
                } else {
                    "forgejo-user:42".into()
                },
                repository,
                candidate: candidate.clone(),
                request_id: request_id.into(),
                request: Box::new(move |method, path, body| fixture.request(method, path, body)),
                candidate_read: Box::new(move || fresh.current()),
            });
        }
        let token = match repository.forge {
            ForgeKind::GitHub => std::env::var("REVIEW_POLICY_TOKEN")
                .context("dedicated App installation token unavailable")?,
            ForgeKind::Forgejo => std::env::var(
                config
                    .forgejo_token_env
                    .get(&repository.host)
                    .context("configure an instance-scoped token environment reference")?,
            )?,
        };
        let request_token = token.clone();
        let request_repo = repository.clone();
        let request: Box<Request> = match repository.forge {
            ForgeKind::GitHub => Box::new(move |method, path, body| {
                github_api::request(&request_token, method, path, body)
            }),
            ForgeKind::Forgejo => Box::new(move |method, path, body| {
                Forgejo::new(request_repo.clone(), request_token.clone())?
                    .request(method, path, body)
            }),
        };
        let (kind, authority) = match repository.forge {
            ForgeKind::GitHub => {
                let id = config
                    .policy_app_id
                    .context("dedicated policy App is not configured")?;
                if id == 0 || id == 15368 {
                    bail!("dedicated App required");
                }
                (Kind::GitHubCheck, format!("github-app:{id}"))
            }
            ForgeKind::Forgejo => {
                let id = request("GET", "/user", None)?["id"]
                    .as_u64()
                    .filter(|id| *id > 0)
                    .context("publisher has no positive numeric identity")?;
                (Kind::ForgejoComment, format!("forgejo-user:{id}"))
            }
        };
        let fresh_repo = repository.clone();
        let url = candidate.url.clone();
        let candidate_read: Box<dyn Fn() -> Result<Candidate>> = match repository.forge {
            ForgeKind::GitHub => Box::new(move || Ok(GitHub::new(token.clone())?.candidate(&url)?)),
            ForgeKind::Forgejo => Box::new(move || {
                Ok(Forgejo::new(fresh_repo.clone(), token.clone())?.candidate(&url)?)
            }),
        };
        Ok(Self {
            repository,
            candidate: candidate.clone(),
            request_id: request_id.into(),
            authority,
            kind,
            request,
            candidate_read,
        })
    }

    pub(crate) fn authority(&self) -> &str {
        &self.authority
    }
    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }

    fn fresh(&self) -> Result<()> {
        if (self.candidate_read)()? != self.candidate {
            bail!("publication comparison changed; preserve the original attempt");
        }
        Ok(())
    }

    fn object_path(&self, id: u64) -> Result<String> {
        if id == 0 {
            bail!("positive publication ID required");
        }
        let suffix = match self.kind {
            Kind::GitHubCheck => "check-runs",
            Kind::ForgejoComment => "issues/comments",
        };
        Ok(format!(
            "/repos/{}/{suffix}/{id}",
            self.repository.repository
        ))
    }

    fn normalize(&self, value: Value) -> Result<Published> {
        let id = value["id"]
            .as_u64()
            .filter(|id| *id > 0)
            .context("publication has no positive object ID")?;
        let payload = match self.kind {
            Kind::GitHubCheck => {
                if format!(
                    "github-app:{}",
                    value
                        .pointer("/app/id")
                        .and_then(Value::as_u64)
                        .context("check App identity missing")?
                ) != self.authority
                    || value["name"].as_str() != Some("review-policy")
                    || value["head_sha"].as_str() != Some(self.candidate.head.as_str())
                    || value["external_id"].as_str() != Some(self.request_id.as_str())
                    || value
                        .pointer("/output/annotations_count")
                        .and_then(Value::as_u64)
                        != Some(0)
                    || !value
                        .pointer("/output/text")
                        .is_none_or(|v| v.is_null() || v.as_str() == Some(""))
                {
                    bail!("check object, App, request or content identity mismatch");
                }
                let mut payload = json!({});
                for key in [
                    "name",
                    "head_sha",
                    "status",
                    "conclusion",
                    "details_url",
                    "external_id",
                ] {
                    payload[key] = value.get(key).context("incomplete check response")?.clone();
                }
                payload["output"] = json!({"title":value.pointer("/output/title").context("check title missing")?,
                    "summary":value.pointer("/output/summary").context("check summary missing")?});
                payload
            }
            Kind::ForgejoComment => {
                if format!(
                    "forgejo-user:{}",
                    value
                        .pointer("/user/id")
                        .and_then(Value::as_u64)
                        .context("comment publisher identity missing")?
                ) != self.authority
                    || value["pull_request_url"].as_str() != Some(self.candidate.url.as_str())
                    || value["original_author_id"].as_u64() != Some(0)
                    || value["original_author"].as_str() != Some("")
                {
                    bail!("comment PR or local publisher identity mismatch");
                }
                let body = value["body"].as_str().context("comment body missing")?;
                if !body.starts_with(&format!(
                    "<!-- repo-review:{} -->\n",
                    storage::digest(self.request_id.as_bytes())
                )) {
                    bail!("comment request marker mismatch");
                }
                json!({"body":body})
            }
        };
        Ok(Published {
            id,
            authority: self.authority.clone(),
            payload,
            url: value["html_url"].as_str().map(str::to_owned),
        })
    }

    pub(crate) fn observe(&self, id: u64) -> Result<Published> {
        self.fresh()?;
        self.observe_historical(id)
    }

    /// Exact object/request/publisher/payload observation only. The frozen
    /// comparison in normalize() is never replaced by the PR's current head.
    fn observe_historical(&self, id: u64) -> Result<Published> {
        self.normalize((self.request)("GET", &self.object_path(id)?, None)?)
    }

    pub(crate) fn patch(&self, id: u64, payload: &Value) -> Result<Published> {
        self.fresh()?;
        let mut patch = payload.clone();
        // PATCH cannot change head_sha; verify it again in the returned object.
        // https://docs.github.com/en/rest/checks/runs#update-a-check-run
        if let Kind::GitHubCheck = self.kind {
            patch
                .as_object_mut()
                .context("invalid check payload")?
                .remove("head_sha");
        }
        self.normalize((self.request)(
            "PATCH",
            &self.object_path(id)?,
            Some(patch),
        )?)
    }

    pub(crate) fn create_check(&self, state: &Path, payload: &Value) -> Result<Value> {
        if !matches!(self.kind, Kind::GitHubCheck) {
            bail!("GitHub check publisher required");
        }
        self.fresh()?;
        publication::once_bound(
            state,
            &self.request_id,
            self.kind,
            &self.candidate,
            payload,
            Binding {
                authority: &self.authority,
                publication_id: None,
            },
            || {
                self.fresh()?;
                self.normalize((self.request)(
                    "POST",
                    &format!("/repos/{}/check-runs", self.repository.repository),
                    Some(payload.clone()),
                )?)
            },
        )
    }
}

pub struct Update<'a> {
    pub request: &'a str,
    pub publication_id: u64,
    pub expected_digest: &'a str,
    pub details_url: Option<&'a str>,
    pub apply: bool,
}

fn repair_target(target: &Target, report: &Report, config: &Config) -> Result<()> {
    if target.pr.as_deref() != Some(report.route.candidate.url.as_str())
        || target.expected_head.as_deref() != Some(report.route.candidate.head.as_str())
        || target.working_tree
        || target.base.is_some()
        || report.route.scope != Scope::PullRequest
        || config.digest()? != report.route.config_digest
    {
        bail!("publication repair requires the original explicit PR/head and unchanged trusted policy");
    }
    Ok(())
}

impl Application {
    /// Authoritative reads only on the forge; the companion local journal records
    /// adoption without rewriting the original write-ahead attempt.
    pub fn reconcile_publication(&self, target: &Target, request: &str, id: u64) -> Result<Value> {
        let report = self.status(request)?;
        repair_target(target, &report, &self.config)?;
        let publisher = Publisher::new(&self.config, &report.route.candidate, request)?;
        let mut receipt = publication::reconcile(
            Selection {
                root: &self.state,
                request,
                kind: publisher.kind(),
                candidate: &report.route.candidate,
                authority: publisher.authority(),
                publication_id: id,
            },
            || publisher.observe_historical(id),
        )?;
        receipt["observation_scope"] = json!("historical-publication-only");
        Ok(receipt)
    }

    pub fn update_publication(&self, target: &Target, update: Update<'_>) -> Result<Value> {
        let previous = self.status(update.request)?;
        repair_target(target, &previous, &self.config)?;
        let report = self.tick(target, true, false)?;
        if report.route.id != update.request || report.route.candidate != previous.route.candidate {
            bail!("publication update cannot change the original request/comparison");
        }
        let publisher = Publisher::new(&self.config, &report.route.candidate, update.request)?;
        let payload = match publisher.kind() {
            Kind::GitHubCheck => github_payload(
                &report,
                &report.route.candidate,
                &router::policy(&self.config, report.route.provider, false),
                update
                    .details_url
                    .context("GitHub publication update requires --details-url")?,
            )?,
            Kind::ForgejoComment => {
                if update.details_url.is_some() {
                    bail!("Forgejo summary updates do not accept --details-url");
                }
                forgejo_payload(&report)?
            }
        };
        publication::update(
            Selection {
                root: &self.state,
                request: update.request,
                kind: publisher.kind(),
                candidate: &report.route.candidate,
                authority: publisher.authority(),
                publication_id: update.publication_id,
            },
            update.expected_digest,
            &payload,
            update.apply,
            || publisher.observe(update.publication_id),
            |id| publisher.patch(id, &payload),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    fn candidate(forgejo: bool) -> Candidate {
        Candidate {
            url: if forgejo {
                "https://codefloe.com/caniko/fixture/pulls/1"
            } else {
                "https://github.com/caniko/fixture/pull/1"
            }
            .into(),
            source_repository: if forgejo {
                "codefloe.com/caniko/fixture"
            } else {
                "github.com/caniko/fixture"
            }
            .into(),
            source_branch: "feature".into(),
            target_branch: "trunk".into(),
            head: "a".repeat(40),
            base: "b".repeat(40),
            draft: false,
            open: true,
        }
    }

    fn raw_check(candidate: &Candidate, request: &str, payload: &Value) -> Value {
        let mut result = payload.clone();
        result["id"] = json!(7);
        result["app"] = json!({"id":321});
        result["head_sha"] = json!(candidate.head);
        result["external_id"] = json!(request);
        result["output"]["annotations_count"] = json!(0);
        result["output"]["text"] = Value::Null;
        result["html_url"] = json!("https://github.com/caniko/fixture/runs/7");
        result
    }

    fn publisher(forgejo: bool, response: Value) -> Publisher {
        let candidate = candidate(forgejo);
        let fresh = candidate.clone();
        Publisher {
            repository: Repository {
                forge: if forgejo {
                    ForgeKind::Forgejo
                } else {
                    ForgeKind::GitHub
                },
                host: if forgejo {
                    "codefloe.com"
                } else {
                    "github.com"
                }
                .into(),
                repository: "caniko/fixture".into(),
            },
            candidate,
            request_id: "c".repeat(64),
            authority: if forgejo {
                "forgejo-user:42"
            } else {
                "github-app:321"
            }
            .into(),
            kind: if forgejo {
                Kind::ForgejoComment
            } else {
                Kind::GitHubCheck
            },
            request: Box::new(move |_, _, _| Ok(response.clone())),
            candidate_read: Box::new(move || Ok(fresh.clone())),
        }
    }

    #[test]
    fn github_adapter_recovers_create_and_patch_replies_without_duplicate_effects() -> Result<()> {
        let state = tempfile::tempdir()?;
        let candidate = candidate(false);
        let request = "c".repeat(64);
        let old = json!({"name":"review-policy","head_sha":candidate.head,"external_id":request,
            "status":"completed","conclusion":"failure","details_url":"https://example.invalid/details",
            "output":{"title":"Blocked","summary":"Exact evidence"}});
        let mut new = old.clone();
        new["conclusion"] = json!("success");
        new["output"]["title"] = json!("Ready");
        let remote = Rc::new(RefCell::new(raw_check(&candidate, &request, &old)));
        let calls = Rc::new(RefCell::new(Vec::<String>::new()));
        let response = remote.clone();
        let effects = calls.clone();
        let fixture_candidate = candidate.clone();
        let fixture_request = request.clone();
        let mut publisher = publisher(false, Value::Null);
        publisher.request = Box::new(move |method, path, body| {
            effects.borrow_mut().push(method.into());
            match method {
                "GET" => {
                    assert_eq!(path, "/repos/caniko/fixture/check-runs/7");
                    Ok(response.borrow().clone())
                }
                "POST" => {
                    assert_eq!(path, "/repos/caniko/fixture/check-runs");
                    *response.borrow_mut() = raw_check(
                        &fixture_candidate,
                        &fixture_request,
                        &body.context("missing create body")?,
                    );
                    bail!("committed create reply lost")
                }
                "PATCH" => {
                    assert_eq!(path, "/repos/caniko/fixture/check-runs/7");
                    let body = body.context("missing patch body")?;
                    assert!(body.get("head_sha").is_none());
                    *response.borrow_mut() = raw_check(&fixture_candidate, &fixture_request, &body);
                    bail!("committed patch reply lost")
                }
                _ => panic!("unexpected HTTP operation"),
            }
        });
        assert!(publisher.create_check(state.path(), &old).is_err());
        let selected = || Selection {
            root: state.path(),
            request: &request,
            kind: publisher.kind(),
            candidate: &candidate,
            authority: publisher.authority(),
            publication_id: 7,
        };
        let adopted = publication::reconcile(selected(), || publisher.observe(7))?;
        assert!(publication::update(
            selected(),
            adopted["payload_digest"]
                .as_str()
                .context("missing digest")?,
            &new,
            true,
            || publisher.observe(7),
            |id| publisher.patch(id, &new)
        )
        .is_err());
        let recovered = publication::reconcile(selected(), || publisher.observe(7))?;
        assert_eq!(recovered["updates"], 1);
        assert!(publisher.create_check(state.path(), &new).is_err());
        assert_eq!(
            calls.borrow().iter().filter(|call| *call == "POST").count(),
            1
        );
        assert_eq!(
            calls
                .borrow()
                .iter()
                .filter(|call| *call == "PATCH")
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn github_observation_rejects_changed_app_head_request_and_extra_content() {
        let candidate = candidate(false);
        let request = "c".repeat(64);
        let payload = json!({"name":"review-policy","head_sha":candidate.head,"external_id":request,
            "status":"completed","conclusion":"success","details_url":"https://example.invalid/details",
            "output":{"title":"Ready","summary":"Exact evidence"}});
        for field in ["app", "head", "request", "annotations", "text"] {
            let mut response = raw_check(&candidate, &request, &payload);
            match field {
                "app" => response["app"]["id"] = json!(999),
                "head" => response["head_sha"] = json!("e".repeat(40)),
                "request" => response["external_id"] = json!("e".repeat(64)),
                "annotations" => response["output"]["annotations_count"] = json!(1),
                "text" => response["output"]["text"] = json!("unexpected content"),
                _ => unreachable!(),
            }
            assert!(publisher(false, response).observe(7).is_err(), "{field}");
        }
    }

    #[test]
    fn forgejo_adapter_updates_the_owned_pr_comment_and_rejects_impersonation() -> Result<()> {
        let candidate = candidate(true);
        let request = "c".repeat(64);
        let body = format!(
            "<!-- repo-review:{} -->\n\nBlocked",
            storage::digest(request.as_bytes())
        );
        let raw = json!({"id":7,"user":{"id":42},"pull_request_url":candidate.url,"original_author_id":0,
            "original_author":"","body":body,"html_url":"https://codefloe.com/caniko/fixture/pulls/1#issuecomment-7"});
        assert_eq!(
            publisher(true, raw.clone()).observe(7)?.payload,
            json!({"body":body})
        );
        for field in ["publisher", "pr", "marker", "original_author"] {
            let mut response = raw.clone();
            match field {
                "publisher" => response["user"]["id"] = json!(99),
                "pr" => {
                    response["pull_request_url"] =
                        json!("https://codefloe.com/caniko/fixture/pulls/2")
                }
                "marker" => response["body"] = json!("unrelated"),
                "original_author" => response["original_author_id"] = json!(42),
                _ => unreachable!(),
            }
            assert!(publisher(true, response).observe(7).is_err(), "{field}");
        }
        let mut adapter = publisher(true, raw.clone());
        adapter.request = Box::new(move |method, path, body| {
            assert_eq!(method, "PATCH");
            assert_eq!(path, "/repos/caniko/fixture/issues/comments/7");
            let mut response = raw.clone();
            response["body"] = body.context("missing comment patch body")?["body"].clone();
            Ok(response)
        });
        let desired = json!({"body":format!("<!-- repo-review:{} -->\n\nReady", storage::digest(request.as_bytes()))});
        assert_eq!(adapter.patch(7, &desired)?.payload, desired);
        Ok(())
    }

    #[test]
    fn changed_target_prevents_authoritative_object_reads_and_patch_effects() {
        let mut adapter = publisher(false, Value::Null);
        let mut changed = candidate(false);
        changed.base = "e".repeat(40);
        adapter.candidate_read = Box::new(move || Ok(changed.clone()));
        adapter.request = Box::new(|_, _, _| panic!("stale candidate reached forge object"));
        assert!(adapter.observe(7).is_err());
        assert!(adapter.patch(7, &json!({})).is_err());
    }

    #[test]
    fn historical_observation_survives_moved_or_closed_pr_without_reading_current_candidate(
    ) -> Result<()> {
        let frozen = candidate(false);
        let request = "c".repeat(64);
        let payload = json!({"name":"review-policy","head_sha":frozen.head,"external_id":request,
            "status":"completed","conclusion":"failure","details_url":"https://example.invalid/details",
            "output":{"title":"Blocked","summary":"Exact evidence"}});
        let mut adapter = publisher(false, raw_check(&frozen, &request, &payload));
        adapter.candidate_read = Box::new(|| bail!("current PR moved or closed"));
        let observed = adapter.observe_historical(7)?;
        assert_eq!(observed.payload, payload);
        assert!(adapter.observe(7).is_err());
        assert!(adapter.patch(7, &payload).is_err());
        Ok(())
    }
}
