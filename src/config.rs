use crate::{
    identity::{ForgeKind, Repository},
    storage,
};
use anyhow::{bail, Context, Result};
use canix_toolbelt::review::CheckRequirement;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub forges: BTreeMap<String, ForgeKind>,
    pub remote_aliases: BTreeMap<String, String>,
    /// Source coordinate -> target remote URL, including fork relationships.
    pub canonical_targets: BTreeMap<String, String>,
    pub greptile: Greptile,
    pub roborev: Roborev,
    pub required_checks: Vec<CheckRequirement>,
    pub policy_app_id: Option<u64>,
    pub forgejo_token_env: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: 1,
            forges: BTreeMap::new(),
            remote_aliases: BTreeMap::new(),
            canonical_targets: BTreeMap::new(),
            greptile: Greptile::default(),
            roborev: Roborev::default(),
            required_checks: vec![],
            policy_app_id: None,
            forgejo_token_env: BTreeMap::from([
                ("codefloe.com".into(), "FORGEJO_TOKEN".into()),
                ("codeberg.org".into(), "CODEBERG_TOKEN".into()),
            ]),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Greptile {
    pub enabled: bool,
    pub repositories: Vec<String>,
    /// GitHub login -> account-wide budget key. PR author owns the charge.
    pub billing_accounts: BTreeMap<String, String>,
    pub reviewer_id: u64,
    /// Verified upper bound of effective PR tier; unknown (None) routes to roborev.
    pub maximum_cost: Option<u32>,
    pub cli: Option<PathBuf>,
    pub cli_identity: Option<String>,
    pub cli_qualification_receipt: String,
    pub cli_auth_file: Option<PathBuf>,
    pub cli_auth_digest: String,
}

impl Default for Greptile {
    fn default() -> Self {
        Self {
            enabled: false,
            repositories: vec!["github.com/caniko/*".into()],
            billing_accounts: BTreeMap::new(),
            reviewer_id: 165735046,
            maximum_cost: None,
            cli: None,
            cli_identity: None,
            cli_qualification_receipt: String::new(),
            cli_auth_file: None,
            cli_auth_digest: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Roborev {
    /// A provisioned daemon only. This client never autostarts a daemon.
    pub socket: Option<PathBuf>,
    pub agent: String,
    pub qualified: bool,
    /// Must attest worker confinement, repository-config handling and no failover.
    pub qualification_receipt: String,
}

impl Default for Roborev {
    fn default() -> Self {
        Self {
            socket: None,
            agent: "opencode".into(),
            qualified: false,
            qualification_receipt: String::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self =
            serde_json::from_slice(&std::fs::read(path).context("read trusted user policy")?)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1
            || self.greptile.reviewer_id == 0
            || self
                .greptile
                .maximum_cost
                .is_some_and(|c| ![1, 3, 10].contains(&c))
        {
            bail!("invalid policy version, reviewer or tier cost");
        }
        if self.roborev.qualified
            && (self.roborev.qualification_receipt.trim().is_empty()
                || !self
                    .roborev
                    .socket
                    .as_ref()
                    .is_some_and(|s| s.is_absolute()))
        {
            bail!("roborev qualification requires an absolute socket and a receipt");
        }
        if !matches!(
            self.roborev.agent.as_str(),
            "opencode" | "codex" | "claude-code"
        ) {
            bail!("unsupported approved agent");
        }
        if self.policy_app_id.is_some_and(|id| id == 0 || id == 15368) {
            bail!("review-policy requires a dedicated App, not shared GitHub Actions");
        }
        if self.greptile.cli.as_ref().is_some_and(|p| !p.is_absolute()) {
            bail!("Greptile CLI must be an absolute executable");
        }
        for target in self.canonical_targets.values() {
            Repository::from_remote(target, &self.forges)?;
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(storage::digest(&serde_json::to_vec(self)?))
    }
    pub fn greptile_scope(&self, repo: &Repository) -> bool {
        repo.forge == ForgeKind::GitHub
            && self.greptile.enabled
            && self.greptile.repositories.iter().any(|pattern| {
                pattern.strip_suffix("/*").map_or_else(
                    || repo.coordinate() == *pattern,
                    |prefix| repo.coordinate().starts_with(&format!("{prefix}/")),
                )
            })
    }
}
