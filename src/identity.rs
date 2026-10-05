use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForgeKind {
    GitHub,
    Forgejo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub host: String,
    pub repository: String,
    pub forge: ForgeKind,
}

pub fn safe_segment(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

impl Repository {
    fn parse_url(input: &str) -> Result<Url> {
        if input
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
            || input.contains('%')
            || input.contains('\\')
            || input.split('/').any(|s| matches!(s, "." | ".."))
        {
            bail!("invalid repository URL");
        }
        let url = Url::parse(input).context("invalid repository URL")?;
        if url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.port().is_some()
            || (url.scheme() != "ssh" && !url.username().is_empty())
        {
            bail!("credential-bearing or noncanonical repository URL");
        }
        Ok(url)
    }

    fn new(host: &str, path: &str, hosts: &BTreeMap<String, ForgeKind>) -> Result<Self> {
        let forge = match host {
            "github.com" => ForgeKind::GitHub,
            "codefloe.com" | "codeberg.org" => ForgeKind::Forgejo,
            _ => *hosts
                .get(host)
                .context("unknown forge; configure its exact host")?,
        };
        if forge == ForgeKind::GitHub && host != "github.com" {
            bail!("Greptile supports GitHub Cloud only in this integration");
        }
        let path = path
            .trim_matches('/')
            .strip_suffix(".git")
            .unwrap_or(path.trim_matches('/'));
        let parts: Vec<_> = path.split('/').collect();
        if parts.len() != 2 || !parts.iter().all(|s| safe_segment(s)) {
            bail!("expected OWNER/REPOSITORY");
        }
        Ok(Self {
            host: host.into(),
            repository: path.into(),
            forge,
        })
    }

    pub fn from_remote(input: &str, hosts: &BTreeMap<String, ForgeKind>) -> Result<Self> {
        let expanded = if input.contains("://") {
            input.to_owned()
        } else {
            let (host, path) = input
                .split_once(':')
                .context("expected a network Git remote")?;
            format!("ssh://{host}/{path}")
        };
        let url = Self::parse_url(&expanded)?;
        if !matches!(url.scheme(), "https" | "ssh") {
            bail!("unsupported Git transport");
        }
        Self::new(
            url.host_str().context("remote has no host")?,
            url.path(),
            hosts,
        )
    }

    pub fn from_pr(input: &str, hosts: &BTreeMap<String, ForgeKind>) -> Result<(Self, u64)> {
        let url = Self::parse_url(input)?;
        if url.scheme() != "https" {
            bail!("PR URL must use HTTPS");
        }
        let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
        if parts.len() != 4 {
            bail!("expected OWNER/REPO/pull(s)/NUMBER");
        }
        let repo = Self::new(
            url.host_str().context("PR has no host")?,
            &parts[..2].join("/"),
            hosts,
        )?;
        let expected = if repo.forge == ForgeKind::GitHub {
            "pull"
        } else {
            "pulls"
        };
        let number: u64 = parts[3].parse().context("invalid PR number")?;
        if parts[2] != expected || number == 0 {
            bail!("invalid PR path");
        }
        Ok((repo, number))
    }

    pub fn coordinate(&self) -> String {
        format!("{}/{}", self.host, self.repository)
    }
    pub fn pr_url(&self, number: u64) -> String {
        format!(
            "https://{}/{}/{}/{number}",
            self.host,
            self.repository,
            if self.forge == ForgeKind::GitHub {
                "pull"
            } else {
                "pulls"
            }
        )
    }
}
