use crate::{
    budget::Budget,
    config::Config,
    forgejo::Forgejo,
    git::{self, Checkout},
    identity::{ForgeKind, Repository},
    roborev::Roborev,
    router::{self, BudgetedGreptile, ProviderName, Route, RoutingContext, Scope},
    storage,
};
use anyhow::{bail, Context, Result};
use canix_toolbelt::review::{
    self, Candidate, Error, Forge, GitHub, GreptileGitHub, Outcome, Policy, Provider, Verdict,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub repo: PathBuf,
    pub remote: Option<String>,
    pub pr: Option<String>,
    pub base: Option<String>,
    pub working_tree: bool,
    pub remote_only: bool,
    pub expected_head: Option<String>,
}

pub struct Application {
    pub config: Config,
    pub state: PathBuf,
}

fn scope(target: &Target, pr_scope: bool) -> Scope {
    if pr_scope {
        Scope::PullRequest
    } else if target.working_tree {
        Scope::WorkingTree
    } else {
        Scope::Committed
    }
}

struct LocalFacts {
    candidate: Candidate,
    checkout: Checkout,
    target: Target,
}
enum TargetForge {
    GitHub(GitHub),
    Forgejo(Forgejo),
    Local(Box<LocalFacts>),
}

struct FrozenForge<'a> {
    inner: &'a mut TargetForge,
    candidate: Candidate,
}
impl Forge for FrozenForge<'_> {
    fn candidate(&mut self, url: &str) -> std::result::Result<Candidate, Error> {
        let fresh = self.inner.candidate(url)?;
        if fresh != self.candidate {
            return Err(Error(
                "frozen comparison changed; refresh the plan before resuming".into(),
            ));
        }
        Ok(fresh)
    }
    fn checks(&mut self, c: &Candidate, p: &Policy) -> std::result::Result<Vec<String>, Error> {
        self.inner.checks(c, p)
    }
    fn next_poll_at(&self) -> Option<u64> {
        self.inner.next_poll_at()
    }
}
impl Forge for TargetForge {
    fn candidate(&mut self, url: &str) -> std::result::Result<Candidate, Error> {
        match self {
            Self::GitHub(f) => f.candidate(url),
            Self::Forgejo(f) => f.candidate(url),
            Self::Local(f) => (|| -> Result<Candidate> {
                let mut fresh = f.candidate.clone();
                let current_branch = git::text(&f.checkout.root, &["branch", "--show-current"])?;
                fresh.source_branch = if current_branch.is_empty() {
                    "detached".into()
                } else {
                    current_branch
                };
                fresh.head = git::sha(&f.checkout.root, "HEAD")?;
                fresh.base = if f.target.working_tree {
                    fresh.head.clone()
                } else {
                    git::base(&f.checkout, f.target.base.as_deref())?
                };
                if f.target.working_tree {
                    let diff = Some(String::from_utf8(git::command(
                        &f.checkout.root,
                        &["diff", "--binary", "HEAD", "--"],
                    )?)?);
                    let hash =
                        storage::digest(&serde_json::to_vec(&(f.checkout.root.clone(), diff))?);
                    fresh.url = format!("https://local.invalid/review/{hash}");
                }
                Ok(fresh)
            })()
            .map_err(|e| Error(e.to_string())),
        }
    }
    fn checks(&mut self, c: &Candidate, p: &Policy) -> std::result::Result<Vec<String>, Error> {
        match self {
            Self::GitHub(f) => f.checks(c, p),
            Self::Forgejo(f) => f.checks(c, p),
            Self::Local(_) => Ok(vec![]),
        }
    }
    fn next_poll_at(&self) -> Option<u64> {
        match self {
            Self::GitHub(f) => f.next_poll_at(),
            Self::Forgejo(f) => f.next_poll_at(),
            Self::Local(_) => None,
        }
    }
}

struct Resolved {
    repository: Option<Repository>,
    candidate: Candidate,
    author: Option<String>,
    checkout: Option<Checkout>,
    forge: TargetForge,
    merge_base: Option<String>,
    diff: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub route: Route,
    pub outcome: Outcome,
    pub advisory: bool,
    pub merge_blockers: Vec<String>,
}

fn github_token() -> Result<String> {
    let token = std::env::var("GH_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok();
    if let Some(token) = token {
        return Ok(token);
    }
    let output = Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .stdin(Stdio::null())
        .output()
        .context("provide existing GH_TOKEN/GITHUB_TOKEN or gh credentials")?;
    if !output.status.success() {
        bail!("GitHub authentication unavailable");
    }
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

fn github_author(token: &str, repo: &Repository, number: u64) -> Result<String> {
    let mut response = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .max_redirects(0)
        .build()
        .new_agent()
        .get(&format!(
            "https://api.github.com/repos/{}/pulls/{number}",
            repo.repository
        ))
        .header("Authorization", &format!("Bearer {token}"))
        .header("User-Agent", "opencode-repo-review-pr")
        .call()?;
    let data: Value = response
        .body_mut()
        .with_config()
        .limit(16 * 1024 * 1024)
        .read_json()?;
    Ok(data
        .pointer("/user/login")
        .and_then(Value::as_str)
        .context("PR author attribution missing")?
        .into())
}

impl Application {
    fn save_report(&self, report: &Report) -> Result<()> {
        storage::save(
            &self
                .state
                .join("results")
                .join(format!("{}.json", report.route.id)),
            report,
        )?;
        let path = self.state.join("lineage").join(format!(
            "{}.json",
            storage::digest(report.route.candidate.url.as_bytes())
        ));
        let _lock = storage::lock(&path.with_extension("lock"))?;
        let mut requests: Vec<String> = storage::read(&path)?.unwrap_or_default();
        if !requests.contains(&report.route.id) {
            requests.push(report.route.id.clone());
        }
        storage::save(&path, &requests)
    }

    fn prior_findings(&self, route: &Route) -> Result<Vec<review::Finding>> {
        let path = self.state.join("lineage").join(format!(
            "{}.json",
            storage::digest(route.candidate.url.as_bytes())
        ));
        let requests: Vec<String> = storage::read(&path)?.unwrap_or_default();
        let mut findings = vec![];
        for request in requests.into_iter().filter(|id| id != &route.id) {
            let report = self.status(&request)?;
            let Some(previous) = report.outcome.review else {
                continue;
            };
            let dispositions: Vec<crate::disposition::Disposition> = storage::read(
                &self
                    .state
                    .join("dispositions")
                    .join(format!("{request}.json")),
            )?
            .unwrap_or_default();
            for mut finding in previous
                .findings
                .into_iter()
                .filter(|f| !f.id.starts_with("prior:"))
            {
                let addressed = dispositions.iter().any(|d| {
                    d.candidate == report.route.candidate
                        && d.review_id == previous.id
                        && d.finding_id == finding.id
                        && d.body_digest == storage::digest(finding.body.as_bytes())
                        && d.policy_digest == previous.policy_digest
                        && !d.reason.trim().is_empty()
                        && !d.evidence.trim().is_empty()
                });
                if !addressed {
                    finding.id = format!("prior:{request}:{}", finding.id);
                    findings.push(finding);
                }
            }
        }
        Ok(findings)
    }
    fn forge(&self, repo: &Repository) -> Result<TargetForge> {
        match repo.forge {
            ForgeKind::GitHub => Ok(TargetForge::GitHub(GitHub::new(github_token()?)?)),
            ForgeKind::Forgejo => {
                let name = self
                    .config
                    .forgejo_token_env
                    .get(&repo.host)
                    .context("configure an instance-scoped token environment reference")?;
                Ok(TargetForge::Forgejo(Forgejo::new(
                    repo.clone(),
                    std::env::var(name).context("Forgejo credential unavailable")?,
                )?))
            }
        }
    }

    fn resolve(&self, target: &Target, pr_scope: bool) -> Result<Resolved> {
        #[cfg(test)]
        if let Some(fixture) = crate::publication_fixture::active() {
            let candidate = fixture.resolve(target, pr_scope)?;
            let (repo, _) = Repository::from_pr(&candidate.url, &self.config.forges)?;
            let forge = match repo.forge {
                ForgeKind::GitHub => TargetForge::GitHub(GitHub::new("fixture-only".into())?),
                ForgeKind::Forgejo => {
                    TargetForge::Forgejo(Forgejo::new(repo.clone(), "fixture-only".into())?)
                }
            };
            return Ok(Resolved {
                repository: Some(repo),
                candidate,
                author: Some("fixture".into()),
                checkout: None,
                forge,
                merge_base: None,
                diff: None,
            });
        }
        self.config.validate()?;
        if pr_scope && (target.working_tree || target.base.is_some()) {
            bail!("PR reviews use forge-owned base/head; local comparison flags are invalid");
        }
        if !pr_scope && target.pr.is_some() {
            bail!("use the PR command for a PR URL");
        }
        if target.remote_only && target.pr.is_none() {
            bail!("--remote-only requires an explicit PR URL");
        }
        let checkout = if target.remote_only {
            None
        } else {
            Some(git::discover(
                &target.repo,
                target.remote.as_deref(),
                &self.config,
            )?)
        };
        if pr_scope {
            let (repo, explicit_number) = if let Some(url) = &target.pr {
                let (repo, number) = Repository::from_pr(url, &self.config.forges)?;
                if checkout
                    .as_ref()
                    .is_some_and(|c| c.repository.as_ref() != Some(&repo))
                {
                    bail!(
                        "PR target differs from canonical checkout; configure the fork relationship or use --remote-only"
                    );
                }
                (repo, Some(number))
            } else {
                (
                    checkout
                        .as_ref()
                        .and_then(|c| c.repository.clone())
                        .context("PR discovery requires a canonical remote")?,
                    None,
                )
            };
            let mut forge = self.forge(&repo)?;
            let url = if let Some(number) = explicit_number {
                repo.pr_url(number)
            } else {
                let c = checkout.as_ref().context("checkout missing")?;
                if c.branch.is_empty() {
                    bail!("detached HEAD requires --pr");
                }
                match &forge {
                    TargetForge::GitHub(github) => {
                        let source = c.source.as_ref().context("source remote missing")?;
                        github.discover(
                            &repo.repository,
                            &format!(
                                "{}:{}",
                                source
                                    .repository
                                    .split('/')
                                    .next()
                                    .context("source owner missing")?,
                                c.branch
                            ),
                        )?
                    }
                    TargetForge::Forgejo(forgejo) => {
                        forgejo.discover(c.source.as_ref().context("source missing")?, &c.branch)?
                    }
                    _ => bail!("PR forge missing"),
                }
            };
            let candidate = forge.candidate(&url)?;
            candidate.validate()?;
            if target
                .expected_head
                .as_ref()
                .is_some_and(|h| h != &candidate.head)
            {
                bail!("expected head differs from authoritative PR head");
            }
            if !candidate.open {
                bail!("PR is not open");
            }
            let (_, number) = Repository::from_pr(&url, &self.config.forges)?;
            let author = match &forge {
                TargetForge::GitHub(_) => github_author(&github_token()?, &repo, number)?,
                TargetForge::Forgejo(f) => f.author(number)?,
                _ => bail!("missing forge"),
            };
            if let Some(c) = &checkout {
                if c.head != candidate.head {
                    bail!("checkout HEAD differs from PR; use --remote-only for remote review");
                }
            }
            Ok(Resolved {
                repository: Some(repo),
                candidate,
                author: Some(author),
                checkout,
                forge,
                merge_base: None,
                diff: None,
            })
        } else {
            let c = checkout.context("local review requires a checkout")?;
            if target.expected_head.as_ref().is_some_and(|h| h != &c.head) {
                bail!("expected head differs from checkout HEAD");
            }
            let base = if target.working_tree {
                c.head.clone()
            } else {
                git::base(&c, target.base.as_deref())?
            };
            let merge_base = git::comparison(&c.root, &base, &c.head)?;
            let diff = if target.working_tree {
                Some(String::from_utf8(git::command(
                    &c.root,
                    &["diff", "--binary", "HEAD", "--"],
                )?)?)
            } else {
                None
            };
            let scope_digest =
                storage::digest(&serde_json::to_vec(&(c.root.clone(), diff.clone()))?);
            let candidate = Candidate {
                url: format!("https://local.invalid/review/{scope_digest}"),
                source_repository: c
                    .repository
                    .as_ref()
                    .map_or_else(|| "local".into(), Repository::coordinate),
                source_branch: if c.branch.is_empty() {
                    "detached".into()
                } else {
                    c.branch.clone()
                },
                target_branch: base.clone(),
                head: c.head.clone(),
                base,
                draft: false,
                open: true,
            };
            Ok(Resolved {
                repository: c.repository.clone(),
                candidate: candidate.clone(),
                author: None,
                checkout: Some(c.clone()),
                forge: TargetForge::Local(Box::new(LocalFacts {
                    candidate,
                    checkout: c,
                    target: target.clone(),
                })),
                merge_base: Some(merge_base),
                diff,
            })
        }
    }

    pub fn plan(&self, target: &Target, pr_scope: bool) -> Result<Value> {
        let resolved = self.resolve(target, pr_scope)?;
        let now = review::now_seconds();
        let key = router::key(&resolved.candidate, &self.config)?;
        let existing: Option<Route> =
            storage::read(&self.state.join("routes").join(format!("{key}.json")))?;
        let (provider, reason, account, cost) = if let Some(route) = existing {
            (
                route.provider,
                "existing-request-resumed".into(),
                route.account,
                route.cost,
            )
        } else {
            router::choose(
                &self.config,
                RoutingContext {
                    repository: resolved.repository.as_ref(),
                    author: resolved.author.as_deref(),
                    scope: scope(target, pr_scope),
                    root: &self.state,
                    now,
                    request: Some(&key),
                },
            )?
        };
        let excluded_untracked = resolved
            .checkout
            .as_ref()
            .map(|c| git::text(&c.root, &["ls-files", "--others", "--exclude-standard"]))
            .transpose()?;
        Ok(
            json!({"schema_version":1,"request_id":key,"repository":resolved.repository,"candidate":resolved.candidate,"provider":provider,"routing_reason":reason,"billing_account":account,"maximum_credit_cost":cost,"excluded_untracked":excluded_untracked,"dispatches_review":false,"worker_qualified":self.config.roborev.qualified}),
        )
    }

    fn snapshot(&self, resolved: &Resolved, key: &str) -> Result<PathBuf> {
        if let Some(c) = &resolved.checkout {
            return git::snapshot(
                &self.state,
                c,
                &resolved.candidate.head,
                &resolved.candidate.base,
                key,
            );
        }
        let repo = resolved
            .repository
            .as_ref()
            .context("remote snapshot repository missing")?;
        let source = resolved
            .candidate
            .source_repository
            .strip_prefix(&format!("{}/", repo.host))
            .unwrap_or(&resolved.candidate.source_repository);
        Repository::from_remote(
            &format!("https://{}/{source}", repo.host),
            &self.config.forges,
        )?;
        git::snapshot_with(
            &self.state,
            &resolved.candidate.head,
            &resolved.candidate.base,
            key,
            |path| {
                git::command(
                    path,
                    &[
                        "fetch",
                        "--no-tags",
                        "--",
                        &format!("https://{}/{source}.git", repo.host),
                        &resolved.candidate.head,
                    ],
                )?;
                git::command(
                    path,
                    &[
                        "fetch",
                        "--no-tags",
                        "--",
                        &format!("https://{}/{}.git", repo.host, repo.repository),
                        &resolved.candidate.base,
                    ],
                )?;
                Ok(())
            },
        )
    }

    pub fn tick(&self, target: &Target, pr_scope: bool, allow_submit: bool) -> Result<Report> {
        #[cfg(test)]
        if let Some(fixture) = crate::publication_fixture::active() {
            return fixture.collect(self, target, pr_scope, allow_submit);
        }
        let mut resolved = self.resolve(target, pr_scope)?;
        let now = review::now_seconds();
        let key = router::key(&resolved.candidate, &self.config)?;
        let _request_lock =
            storage::lock(&self.state.join("requests").join(format!("{key}.lock")))?;
        let route_path = self.state.join("routes").join(format!("{key}.json"));
        let route = {
            let _lock = storage::lock(&route_path.with_extension("lock"))?;
            match storage::read::<Route>(&route_path)? {
                Some(route) => {
                    if route.schema_version != 1
                        || route.candidate != resolved.candidate
                        || route.config_digest != self.config.digest()?
                    {
                        bail!("route identity mismatch");
                    }
                    route
                }
                None if !allow_submit => {
                    bail!("no authorized request; run the review command first")
                }
                None => {
                    let (mut provider, mut reason, mut account, mut cost) = router::choose(
                        &self.config,
                        RoutingContext {
                            repository: resolved.repository.as_ref(),
                            author: resolved.author.as_deref(),
                            scope: scope(target, pr_scope),
                            root: &self.state,
                            now,
                            request: Some(&key),
                        },
                    )?;
                    if let Some(billing) = &account {
                        if !Budget::new(&self.state, billing).reserve(&key, cost, now)? {
                            provider = ProviderName::Roborev;
                            reason = "greptile-credit-exhausted".into();
                            account = None;
                            cost = 0;
                        }
                    }
                    let route = Route {
                        schema_version: 1,
                        id: key.clone(),
                        candidate: resolved.candidate.clone(),
                        config_digest: self.config.digest()?,
                        provider,
                        reason,
                        account,
                        cost,
                        scope: scope(target, pr_scope),
                    };
                    storage::save(&route_path, &route)?;
                    route
                }
            }
        };
        let policy = router::policy(&self.config, route.provider, !pr_scope);
        let mut provider: Box<dyn Provider + '_> = match route.provider {
            ProviderName::Greptile if !pr_scope => {
                let snapshot = self.snapshot(&resolved, &key)?;
                let repo = resolved
                    .repository
                    .as_ref()
                    .context("CLI remote identity missing")?;
                git::command(
                    &snapshot,
                    &[
                        "-c",
                        "core.hooksPath=/dev/null",
                        "checkout",
                        "--quiet",
                        "-B",
                        &format!("repo-review-{}", &key[..12]),
                        &resolved.candidate.head,
                    ],
                )?;
                let remotes = git::text(&snapshot, &["remote"])?;
                if remotes.is_empty() {
                    git::command(
                        &snapshot,
                        &[
                            "remote",
                            "add",
                            "origin",
                            &format!("https://{}/{}.git", repo.host, repo.repository),
                        ],
                    )?;
                }
                Box::new(BudgetedGreptile {
                    inner: crate::greptile_cli::GreptileCli {
                        settings: self.config.greptile.clone(),
                        snapshot: snapshot.clone(),
                        state: self.state.join("greptile-cli").join(&key),
                        comparison_base: git::comparison(
                            &snapshot,
                            &resolved.candidate.base,
                            &resolved.candidate.head,
                        )?,
                    },
                    budget: Budget::new(
                        &self.state,
                        route
                            .account
                            .as_deref()
                            .context("missing billing account")?,
                    ),
                    route: &route,
                    now,
                })
            }
            ProviderName::Greptile => Box::new(BudgetedGreptile {
                inner: GreptileGitHub::new(GitHub::new(github_token()?)?),
                budget: Budget::new(
                    &self.state,
                    route
                        .account
                        .as_deref()
                        .context("missing billing account")?,
                ),
                route: &route,
                now,
            }),
            ProviderName::Roborev => {
                crate::roborev::Daemon::new(&self.config.roborev)?;
                let snapshot = self.snapshot(&resolved, &key)?;
                let merge_base = resolved.merge_base.take().map_or_else(
                    || {
                        git::comparison(
                            &snapshot,
                            &resolved.candidate.base,
                            &resolved.candidate.head,
                        )
                    },
                    Ok,
                )?;
                // PR acceptance binds the observed target tip, which can differ
                // from the merge base. Local committed reviews remain advisory.
                let comparison_base = if pr_scope {
                    &resolved.candidate.base
                } else {
                    &merge_base
                };
                let files = if target.working_tree {
                    git::command(&snapshot, &["diff", "--name-only", "-z", "HEAD", "--"])?
                } else {
                    git::command(
                        &snapshot,
                        &[
                            "diff",
                            "--name-only",
                            "-z",
                            comparison_base,
                            &resolved.candidate.head,
                            "--",
                        ],
                    )?
                };
                // Working-tree bytes originate in the caller snapshot, not its clean copy.
                let expected_files = if target.working_tree {
                    resolved
                        .checkout
                        .as_ref()
                        .map(|c| {
                            git::command(&c.root, &["diff", "--name-only", "-z", "HEAD", "--"])
                        })
                        .transpose()?
                        .unwrap_or_default()
                        .split(|b| *b == 0)
                        .filter(|s| !s.is_empty())
                        .count()
                } else {
                    files.split(|b| *b == 0).filter(|s| !s.is_empty()).count()
                };
                if expected_files == 0 {
                    bail!("no changes to review; no reviewer dispatched");
                }
                let git_ref = if target.working_tree {
                    "dirty".into()
                } else {
                    format!("{comparison_base}..{}", resolved.candidate.head)
                };
                Box::new(Roborev::new(
                    &self.config.roborev,
                    snapshot,
                    git_ref,
                    resolved.diff.clone(),
                    expected_files,
                    self.state.join("roborev"),
                    scope(target, pr_scope),
                )?)
            }
        };
        let dispositions = self.state.join("dispositions").join(format!("{key}.json"));
        let mut disposed = crate::disposition::Disposed {
            inner: provider.as_mut(),
            path: &dispositions,
        };
        let engine_state = self
            .state
            .join("engine")
            .join(&key)
            .join(policy.provider.as_str());
        storage::private_dir(&engine_state)?;
        // The published engine reuses this anchor; precreate it with the same
        // private mode and link checks as the consumer's other durable locks.
        drop(storage::lock(&engine_state.join(format!(
            "{}.lock",
            storage::digest(resolved.candidate.url.as_bytes())
        )))?);
        let mut outcome = review::ensure_once(
            &mut FrozenForge {
                inner: &mut resolved.forge,
                candidate: resolved.candidate.clone(),
            },
            &mut disposed,
            &policy,
            &resolved.candidate.url,
            &engine_state,
            allow_submit,
        )?;
        drop(provider);
        if route.provider == ProviderName::Greptile && outcome.blockers.iter().any(|b| b == "Greptile review unavailable: account credit limit reached; restore provider capacity and request a fresh review") {
            // Only this typed-by-adapter rejection is definitive. Transport errors,
            // missing replies, pending and generic 429 responses never authorize fallback.
            let _lock=storage::lock(&route_path.with_extension("lock"))?;
            let current:Route=storage::read(&route_path)?.context("route disappeared")?;
            if current.provider!=ProviderName::Greptile{bail!("provider route changed; resume current handover");}
            let mut fallback=current;
            fallback.provider=ProviderName::Roborev;
            fallback.reason="greptile-credit-rejected".into();
            storage::save(&route_path,&fallback)?;
            Budget::new(&self.state,route.account.as_deref().context("quota rejection has no billing account")?).settle(&route.id,false,"trusted exact-head provider credit-limit rejection",now)?;
            let report=Report{schema_version:1,route:fallback,outcome,advisory:false,merge_blockers:vec!["Greptile capacity rejected; resume this same request for its recorded roborev handover".into()]};
            self.save_report(&report)?;
            return Ok(report);
        }
        if pr_scope {
            let carried = self.prior_findings(&route)?;
            if !carried.is_empty() {
                if matches!(outcome.verdict, Verdict::Ready | Verdict::Findings) {
                    outcome.verdict = Verdict::Findings;
                }
                outcome.blockers.push("earlier unresolved findings require source-backed dispositions on their original request IDs".into());
                if let Some(review) = &mut outcome.review {
                    review.findings.extend(carried);
                }
            }
        }
        let mut merge_blockers = vec![];
        if !pr_scope {
            merge_blockers.push("local/advisory evidence cannot authorize a PR merge".into());
        } else if resolved
            .repository
            .as_ref()
            .is_some_and(|r| r.forge == ForgeKind::GitHub)
        {
            if self.config.policy_app_id.is_none() {
                merge_blockers.push("dedicated review-policy App is not configured".into());
            } else {
                let mut merge_policy = policy.clone();
                merge_policy.required_checks.push(review::CheckRequirement {
                    name: "review-policy".into(),
                    app_id: self.config.policy_app_id,
                });
                merge_blockers.extend(resolved.forge.checks(&resolved.candidate, &merge_policy)?);
            }
        } else {
            merge_blockers.push("Forgejo trusted merge enforcement is not qualified".into());
        }
        let report = Report {
            schema_version: 1,
            route,
            outcome,
            advisory: !pr_scope,
            merge_blockers,
        };
        self.save_report(&report)?;
        Ok(report)
    }

    pub fn status(&self, request: &str) -> Result<Report> {
        if request.len() != 64 || !request.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid request ID");
        }
        storage::read(&self.state.join("results").join(format!("{request}.json")))?
            .context("request has no collected result yet; resume the original review command")
    }

    pub fn disposition(
        &self,
        request: &str,
        finding: &str,
        reason: &str,
        evidence: &str,
    ) -> Result<Value> {
        let report = self.status(request)?;
        let review = report
            .outcome
            .review
            .context("request has no completed review")?;
        let item = review
            .findings
            .iter()
            .find(|f| f.id == finding)
            .context("finding not present in this exact review")?;
        if item.id.starts_with("prior:") {
            bail!("dispose carried findings against their original request and finding IDs");
        }
        crate::disposition::record(
            &self
                .state
                .join("dispositions")
                .join(format!("{request}.json")),
            crate::disposition::Disposition {
                candidate: report.route.candidate,
                policy_digest: review.policy_digest,
                review_id: review.id,
                finding_id: item.id.clone(),
                body_digest: storage::digest(item.body.as_bytes()),
                reason: reason.into(),
                evidence: evidence.into(),
            },
        )?;
        Ok(
            json!({"request_id":request,"finding":finding,"recorded":true,"requires_fresh_observation":true}),
        )
    }

    pub fn publish(&self, target: &Target) -> Result<Value> {
        let report = self.tick(target, true, false)?;
        let resolved = self.resolve(target, true)?;
        crate::forge_publication::require_completed(&report, &resolved.candidate)?;
        let TargetForge::Forgejo(mut forge) = resolved.forge else {
            bail!("GitHub results use the separate dedicated policy App gate");
        };
        let summary = serde_json::to_string_pretty(&report)?;
        let url = forge.publish(
            &report.route.candidate,
            &report.route.id,
            &summary,
            &self.state,
        )?;
        Ok(
            json!({"schema_version":1,"publication":url,"request_id":report.route.id,"advisory":true}),
        )
    }

    pub fn publish_check(&self, target: &Target, details: &str) -> Result<Value> {
        let report = self.tick(target, true, false)?;
        let resolved = self.resolve(target, true)?;
        let repo = resolved.repository.context("repository missing")?;
        if repo.forge != ForgeKind::GitHub {
            bail!("Forgejo does not provide qualified App-bound enforcement");
        }
        let policy = router::policy(&self.config, report.route.provider, false);
        let payload = crate::forge_publication::github_payload(
            &report,
            &resolved.candidate,
            &policy,
            details,
        )?;
        let publisher = crate::forge_publication::Publisher::new(
            &self.config,
            &resolved.candidate,
            &report.route.id,
        )?;
        let result = publisher.create_check(&self.state, &payload)?;
        Ok(
            json!({"request_id":report.route.id,"check_id":result["id"],"app_id":self.config.policy_app_id,
            "review_ready":payload["conclusion"] == "success","merge_authorized":false}),
        )
    }

    pub fn merge(&self, target: &Target, apply: bool) -> Result<Value> {
        if target.pr.is_none() || target.expected_head.is_none() {
            bail!("merge requires an explicit PR URL and expected head");
        }
        let report = self.tick(target, true, false)?;
        if report.advisory
            || report.outcome.verdict != Verdict::Ready
            || !report.merge_blockers.is_empty()
        {
            bail!("current review, CI and extra forge gate must all be ready before merge");
        }
        let mut resolved = self.resolve(target, true)?;
        if resolved.candidate != report.route.candidate
            || !resolved.candidate.open
            || resolved.candidate.draft
        {
            bail!("merge comparison changed or remains closed/draft");
        }
        let repo = resolved.repository.context("merge repository missing")?;
        if repo.forge != ForgeKind::GitHub {
            bail!("Forgejo expected-head merge and trusted merger permissions remain unqualified");
        }
        let app_id = self
            .config
            .policy_app_id
            .context("dedicated App gate missing")?;
        let token = github_token()?;
        let encoded_branch: String =
            url::form_urlencoded::byte_serialize(resolved.candidate.target_branch.as_bytes())
                .collect();
        let protection = crate::github_api::request(
            &token,
            "GET",
            &format!(
                "/repos/{}/branches/{encoded_branch}/protection",
                repo.repository
            ),
            None,
        )?;
        crate::github_api::validate_merge_protection(&protection, app_id)?;
        if resolved.forge.candidate(&resolved.candidate.url)? != resolved.candidate {
            bail!("comparison changed immediately before merge");
        }
        if !apply {
            return Ok(
                json!({"merge_ready":true,"provider":report.route.provider,"candidate":resolved.candidate,"applies_merge":false}),
            );
        }
        let path = self
            .state
            .join("merges")
            .join(format!("{}.json", report.route.id));
        let _lock = storage::lock(&path.with_extension("lock"))?;
        if storage::read::<Value>(&path)?.is_some() {
            bail!("merge already attempted; reconcile authoritative PR state without replaying it");
        }
        storage::save(
            &path,
            &json!({"schema_version":1,"candidate":resolved.candidate,"request_id":report.route.id,"state":"dispatching"}),
        )?;
        let (_, number) = Repository::from_pr(&resolved.candidate.url, &self.config.forges)?;
        let result = crate::github_api::request(
            &token,
            "PUT",
            &format!("/repos/{}/pulls/{number}/merge", repo.repository),
            Some(json!({"sha":resolved.candidate.head,"merge_method":"merge"})),
        )?;
        if result["merged"].as_bool() != Some(true) {
            bail!("GitHub did not confirm merge; reconcile authoritative PR state");
        }
        storage::save(
            &path,
            &json!({"schema_version":1,"candidate":resolved.candidate,"request_id":report.route.id,"state":"merged","merge_commit":result["sha"]}),
        )?;
        Ok(
            json!({"merged":true,"request_id":report.route.id,"provider":report.route.provider,"merge_commit":result["sha"]}),
        )
    }
}
