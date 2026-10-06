//! Unit-test-only external evidence boundary. No environment switch or endpoint
//! override exists in the shipped executable. Fixture state persists across the
//! reconstructed application/transport instances used by command-flow tests.
use crate::{
    app::{Application, Report, Target},
    config::Config,
    disposition::Disposed,
    router::{self, ProviderName, Route, Scope},
    storage,
};
use anyhow::{bail, Context, Result};
use canix_toolbelt::review::{
    self, Candidate, Capabilities, Error, Evidence, Finding, Forge, Intent, Policy, Provider,
    Review,
};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

thread_local! { static FIXTURE: RefCell<Option<Rc<Fixture>>> = const { RefCell::new(None) }; }

pub(crate) fn active() -> Option<Rc<Fixture>> {
    FIXTURE.with(|f| f.borrow().clone())
}

pub(crate) struct Guard(Option<Rc<Fixture>>);
impl Drop for Guard {
    fn drop(&mut self) {
        FIXTURE.with(|f| *f.borrow_mut() = self.0.take());
    }
}

pub(crate) struct Fixture {
    pub root: PathBuf,
}

impl Fixture {
    pub fn install(root: &Path) -> Guard {
        Guard(FIXTURE.with(|f| f.replace(Some(Rc::new(Self { root: root.into() })))))
    }

    pub fn seed(root: &Path, forgejo: bool) -> Result<(Config, Route)> {
        let candidate = Candidate {
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
        };
        let config = Config {
            policy_app_id: Some(321),
            ..Config::default()
        };
        let policy = router::policy(&config, ProviderName::Roborev, false);
        let route = Route {
            schema_version: 1,
            id: router::key(&candidate, &config)?,
            candidate: candidate.clone(),
            config_digest: config.digest()?,
            provider: ProviderName::Roborev,
            reason: "offline-fixture".into(),
            account: None,
            cost: 0,
            scope: Scope::PullRequest,
        };
        let review = Review {
            id: "fixture-review".into(),
            provider_job_id: Some("fixture-job".into()),
            provider_review_id: Some("fixture-review".into()),
            provider: policy.provider.clone(),
            reviewed_head: Some(candidate.head.clone()),
            requested_base: Some(candidate.base.clone()),
            reviewed_base: Some(candidate.base.clone()),
            policy_digest: policy.digest()?,
            completed: true,
            complete_findings: true,
            findings: vec![Finding {
                id: "low-finding".into(),
                body: "Verify request-bound state survives restart".into(),
                path: Some("src/publication/lifecycle.rs".into()),
                line: Some(1),
                severity: Some("low".into()),
                provider_addressed: false,
                correlated: true,
                url: "https://example.invalid/finding".into(),
            }],
            url: "https://example.invalid/review".into(),
        };
        storage::save(&root.join("fixture-candidate.json"), &candidate)?;
        storage::save(&root.join("fixture-route.json"), &route)?;
        storage::save(&root.join("fixture-review.json"), &review)?;
        storage::save(
            &root.join("fixture-api.json"),
            &json!({"object":null,"calls":[],"lose_next":null}),
        )?;
        Ok((config, route))
    }

    pub fn current(&self) -> Result<Candidate> {
        storage::read(&self.root.join("fixture-candidate.json"))?
            .context("fixture candidate missing")
    }

    pub fn resolve(&self, target: &Target, pr_scope: bool) -> Result<Candidate> {
        let candidate = self.current()?;
        if !pr_scope
            || target.pr.as_deref() != Some(candidate.url.as_str())
            || target
                .expected_head
                .as_ref()
                .is_some_and(|h| h != &candidate.head)
            || !candidate.open
        {
            bail!("fixture current comparison moved or closed");
        }
        Ok(candidate)
    }

    pub fn collect(
        &self,
        app: &Application,
        target: &Target,
        pr_scope: bool,
        allow_submit: bool,
    ) -> Result<Report> {
        // Only transport/evidence is synthetic; use the actual published engine,
        // disposition matcher, private ledger and persisted application reports.
        if allow_submit {
            bail!("offline publication command tried to dispatch a provider");
        }
        let candidate = self.resolve(target, pr_scope)?;
        let route: Route = storage::read(&self.root.join("fixture-route.json"))?
            .context("fixture route missing")?;
        if app.state != self.root
            || route.candidate != candidate
            || route.config_digest != app.config.digest()?
        {
            bail!("fixture route mismatch");
        }
        let policy = router::policy(&app.config, route.provider, false);
        let review: Review = storage::read(&self.root.join("fixture-review.json"))?
            .context("fixture review missing")?;
        let mut provider = Existing {
            review,
            intent: Intent {
                schema_version: 1,
                id: route.id.clone(),
                candidate: candidate.clone(),
                policy_digest: policy.digest()?,
                baseline_review_ids: vec![],
            },
        };
        let path = app
            .state
            .join("dispositions")
            .join(format!("{}.json", route.id));
        let mut disposed = Disposed {
            inner: &mut provider,
            path: &path,
        };
        let engine = self.root.join("fixture-engine");
        storage::private_dir(&engine)?;
        let outcome = review::ensure_once(
            &mut ExistingForge(candidate.clone()),
            &mut disposed,
            &policy,
            &candidate.url,
            &engine,
            false,
        )?;
        let report = Report {
            schema_version: 1,
            route,
            outcome,
            advisory: false,
            merge_blockers: vec![
                "Require extra forge gate".into(),
                "Fixture is not merge authorization".into(),
            ],
        };
        storage::save(
            &app.state
                .join("results")
                .join(format!("{}.json", report.route.id)),
            &report,
        )?;
        self.log("COLLECT", "existing-provider-evidence")?;
        Ok(report)
    }

    fn log(&self, method: &str, path: &str) -> Result<()> {
        let location = self.root.join("fixture-api.json");
        let mut state: Value = storage::read(&location)?.context("fixture API missing")?;
        state["calls"]
            .as_array_mut()
            .context("fixture calls missing")?
            .push(json!({"method":method,"path":path}));
        storage::save(&location, &state)
    }

    pub fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        self.log(method, path)?;
        let location = self.root.join("fixture-api.json");
        let mut state: Value = storage::read(&location)?.context("fixture API missing")?;
        if method == "GET" {
            return match path {
                "/user" => Ok(json!({"id":42})),
                "/repos/caniko/fixture/check-runs/7"
                | "/repos/caniko/fixture/issues/comments/7" => {
                    if state["object"].is_null() {
                        bail!("fixture object missing");
                    }
                    Ok(state["object"].clone())
                }
                "/repos/caniko/fixture/issues/1/comments?limit=50&page=1" => Ok(json!([])),
                "/repos/caniko/fixture/pulls/1" => {
                    let c = self.current()?;
                    Ok(
                        json!({"html_url":c.url,"state":if c.open {"open"} else {"closed"},"draft":c.draft,
                        "head":{"sha":c.head,"ref":c.source_branch,"repo":{"full_name":"caniko/fixture"}},
                        "base":{"sha":c.base,"ref":c.target_branch}}),
                    )
                }
                _ => bail!("unexpected fixture GET {path}"),
            };
        }
        let body = body.context("fixture write body missing")?;
        if !matches!(
            (method, path),
            (
                "POST",
                "/repos/caniko/fixture/check-runs" | "/repos/caniko/fixture/issues/1/comments"
            ) | (
                "PATCH",
                "/repos/caniko/fixture/check-runs/7" | "/repos/caniko/fixture/issues/comments/7"
            )
        ) {
            bail!("unexpected fixture mutation {method} {path}");
        }
        if method == "POST" && !state["object"].is_null() {
            bail!("duplicate fixture POST");
        }
        if method == "PATCH" && state["object"].is_null() {
            bail!("replacement PATCH");
        }
        let candidate = self.current()?;
        let mut response = body;
        response["id"] = json!(7);
        if path.contains("check-runs") {
            response["app"] = json!({"id":321});
            response["head_sha"] = json!(candidate.head);
            response["output"]["annotations_count"] = json!(0);
            response["output"]["text"] = Value::Null;
            response["html_url"] = json!("https://github.com/caniko/fixture/runs/7");
        } else {
            response["user"] = json!({"id":42});
            response["pull_request_url"] = json!(candidate.url);
            response["original_author_id"] = json!(0);
            response["original_author"] = json!("");
            response["html_url"] =
                json!("https://codefloe.com/caniko/fixture/pulls/1#issuecomment-7");
        }
        state["object"] = response.clone();
        let lost = state["lose_next"].as_str() == Some(method);
        state["lose_next"] = Value::Null;
        storage::save(&location, &state)?;
        if lost {
            bail!("fixture committed {method} reply lost");
        }
        Ok(response)
    }
}

struct Existing {
    review: Review,
    intent: Intent,
}
impl Provider for Existing {
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
        _: &Candidate,
        _: &Policy,
        _: Option<&Intent>,
    ) -> std::result::Result<Evidence, Error> {
        Ok(Evidence {
            known_review_ids: vec![self.review.id.clone()],
            recovered_intent: Some(self.intent.clone()),
            request_receipt: Some("existing-fixture-review".into()),
            review: Some(self.review.clone()),
        })
    }
    fn submit(&mut self, _: &Intent) -> std::result::Result<String, Error> {
        panic!("publication dispatched provider")
    }
}
struct ExistingForge(Candidate);
impl Forge for ExistingForge {
    fn candidate(&mut self, _: &str) -> std::result::Result<Candidate, Error> {
        Ok(self.0.clone())
    }
    fn checks(&mut self, _: &Candidate, _: &Policy) -> std::result::Result<Vec<String>, Error> {
        Ok(vec![])
    }
}
