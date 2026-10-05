use crate::{
    budget::Budget,
    config::Config,
    identity::{ForgeKind, Repository},
    storage,
};
use anyhow::Result;
use canix_toolbelt::review::{
    Candidate, Capabilities, Error, Evidence, GreptileGitHub, Intent, Policy, Provider,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderName {
    Greptile,
    Roborev,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    PullRequest,
    Committed,
    WorkingTree,
}

pub struct RoutingContext<'a> {
    pub repository: Option<&'a Repository>,
    pub author: Option<&'a str>,
    pub scope: Scope,
    pub root: &'a Path,
    pub now: u64,
    pub request: Option<&'a str>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub schema_version: u32,
    pub id: String,
    pub candidate: Candidate,
    pub config_digest: String,
    pub provider: ProviderName,
    pub reason: String,
    pub account: Option<String>,
    pub cost: u32,
    pub scope: Scope,
}

pub fn choose(
    config: &Config,
    context: RoutingContext<'_>,
) -> Result<(ProviderName, String, Option<String>, u32)> {
    let RoutingContext {
        repository,
        author,
        scope,
        root,
        now,
        request,
    } = context;
    let local = scope != Scope::PullRequest;
    let robo = |reason: &str| Ok((ProviderName::Roborev, reason.into(), None, 0));
    if scope == Scope::WorkingTree {
        return robo("working-tree-requires-roborev");
    }
    let Some(repo) = repository else {
        return robo("local-repository");
    };
    if repo.forge == ForgeKind::Forgejo {
        return robo("forgejo-roborev");
    }
    if !config.greptile_scope(repo) {
        return robo("greptile-repository-ineligible");
    }
    if local
        && (config.greptile.cli.is_none()
            || config.greptile.cli_qualification_receipt.is_empty()
            || config.greptile.cli_auth_file.is_none()
            || config.greptile.cli_auth_digest.is_empty())
    {
        return robo("greptile-cli-contract-unqualified");
    }
    let author = if local {
        config.greptile.cli_identity.as_deref()
    } else {
        author
    };
    let Some(account) = author.and_then(|a| config.greptile.billing_accounts.get(a)) else {
        return robo("billing-identity-ineligible");
    };
    let Some(cost) = (if local {
        Some(1)
    } else {
        config.greptile.maximum_cost
    }) else {
        return robo("greptile-tier-unconfirmed");
    };
    let budget = Budget::new(root, account);
    if request
        .map(|id| budget.has_reservation(id, cost, now))
        .transpose()?
        .unwrap_or(false)
    {
        return Ok((
            ProviderName::Greptile,
            "existing-credit-reservation".into(),
            Some(account.clone()),
            cost,
        ));
    }
    let balance = budget.show(now)?;
    if !balance.confirmed {
        return robo("budget-unreconciled");
    }
    if balance.available < cost {
        return robo("greptile-credit-exhausted");
    }
    Ok((
        ProviderName::Greptile,
        "github-free-credit".into(),
        Some(account.clone()),
        cost,
    ))
}

pub fn policy(config: &Config, provider: ProviderName, local: bool) -> Policy {
    Policy {
        schema_version: 1,
        provider: if provider == ProviderName::Greptile {
            "greptile"
        } else {
            "roborev"
        }
        .into(),
        transport: if provider == ProviderName::Greptile {
            if local {
                "cli-v3.2.3"
            } else {
                "github-comment"
            }
        } else {
            "unix-http-v0.71"
        }
        .into(),
        reviewer_id: config.greptile.reviewer_id,
        required_checks: config.required_checks.clone(),
    }
}

/// Budgeting decorator; the provider's own evidence/recovery stays in toolbelt.
pub struct BudgetedGreptile<'a, P = GreptileGitHub> {
    pub inner: P,
    pub budget: Budget,
    pub route: &'a Route,
    pub now: u64,
}

impl<P: Provider> Provider for BudgetedGreptile<'_, P> {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn next_poll_at(&self) -> Option<u64> {
        self.inner.next_poll_at()
    }
    fn inspect(
        &mut self,
        candidate: &Candidate,
        policy: &Policy,
        intent: Option<&Intent>,
    ) -> std::result::Result<Evidence, Error> {
        let evidence = self.inner.inspect(candidate, policy, intent)?;
        if let Some(review) = &evidence.review {
            self.budget
                .settle(
                    &self.route.id,
                    true,
                    &format!("completed GitHub provider review {}", review.id),
                    self.now,
                )
                .map_err(|e| Error(e.to_string()))?;
        }
        Ok(evidence)
    }
    fn submit(&mut self, intent: &Intent) -> std::result::Result<String, Error> {
        if !self
            .budget
            .reserve(&self.route.id, self.route.cost, self.now)
            .map_err(|e| Error(e.to_string()))?
        {
            return Err(Error(
                "credit reservation unavailable; no provider request sent".into(),
            ));
        }
        self.inner.submit(intent)
    }
}

pub fn key(candidate: &Candidate, config: &Config) -> Result<String> {
    Ok(storage::digest(&serde_json::to_vec(&(
        candidate,
        config.digest()?,
    ))?))
}
