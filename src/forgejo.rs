use crate::{
    identity::{ForgeKind, Repository},
    publication::{self, Binding, Kind, Published},
    storage,
};
use anyhow::{bail, Context, Result};
use canix_toolbelt::review::{Candidate, Error, Forge, Policy};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, time::Duration};

/// Instance-specific Forgejo REST adapter. Credentials never enter receipts.
pub struct Forgejo {
    repository: Repository,
    token: String,
    agent: ureq::Agent,
}

impl Forgejo {
    pub fn new(repository: Repository, token: String) -> Result<Self> {
        if repository.forge != ForgeKind::Forgejo
            || token.trim().is_empty()
            || token.contains(['\r', '\n'])
        {
            bail!("Forgejo requires an instance-scoped credential");
        }
        Ok(Self {
            repository,
            token,
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(20)))
                .max_redirects(0)
                .http_status_as_error(false)
                .build()
                .new_agent(),
        })
    }

    pub(crate) fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        #[cfg(test)]
        if let Some(fixture) = crate::publication_fixture::active() {
            return fixture.request(method, path, body);
        }
        let url = format!("https://{}/api/v1{path}", self.repository.host);
        let auth = format!("token {}", self.token);
        let mut response = match method {
            "GET" => self.agent.get(&url).header("Authorization", &auth).call()?,
            "POST" => self
                .agent
                .post(&url)
                .header("Authorization", &auth)
                .send_json(body.context("missing publication body")?)?,
            "PATCH" => self
                .agent
                .patch(&url)
                .header("Authorization", &auth)
                .send_json(body.context("missing publication body")?)?,
            _ => bail!("unsupported Forgejo operation"),
        };
        if !response.status().is_success() {
            bail!(
                "Forgejo HTTP {}; no write will be retried automatically",
                response.status().as_u16()
            );
        }
        Ok(response
            .body_mut()
            .with_config()
            .limit(16 * 1024 * 1024)
            .read_json()?)
    }

    pub fn pages(&self, path: &str) -> Result<Vec<Value>> {
        let mut all = vec![];
        for page in 1..=100 {
            let separator = if path.contains('?') { '&' } else { '?' };
            let response = self.request(
                "GET",
                &format!("{path}{separator}limit=50&page={page}"),
                None,
            )?;
            let values = response
                .as_array()
                .context("expected a Forgejo collection")?;
            all.extend(values.iter().cloned());
            if values.len() < 50 {
                return Ok(all);
            }
        }
        bail!("Forgejo pagination exceeded bound; completeness unproven")
    }

    pub fn author(&self, number: u64) -> Result<String> {
        let data = self.request(
            "GET",
            &format!("/repos/{}/pulls/{number}", self.repository.repository),
            None,
        )?;
        Ok(field(&data, "/user/login")?.into())
    }

    pub fn discover(&self, source: &Repository, branch: &str) -> Result<String> {
        let pulls = self.pages(&format!(
            "/repos/{}/pulls?state=open",
            self.repository.repository
        ))?;
        let matches: Vec<_> = pulls
            .iter()
            .filter(|pr| {
                pr.pointer("/head/ref").and_then(Value::as_str) == Some(branch)
                    && pr.pointer("/head/repo/full_name").and_then(Value::as_str)
                        == Some(source.repository.as_str())
            })
            .collect();
        if matches.len() != 1 {
            bail!(
                "expected exactly one open PR for the source repository and branch; provide --pr"
            );
        }
        Ok(self
            .repository
            .pr_url(matches[0]["number"].as_u64().context("missing PR number")?))
    }

    /// Publish an advisory summary only; this is not an authenticated merge gate.
    pub fn publish(
        &mut self,
        candidate: &Candidate,
        request: &str,
        summary: &str,
        state: &Path,
    ) -> Result<String> {
        if self.candidate(&candidate.url)? != *candidate {
            bail!("candidate changed before publication");
        }
        let (_, number) = Repository::from_pr(
            &candidate.url,
            &BTreeMap::from([(self.repository.host.clone(), ForgeKind::Forgejo)]),
        )?;
        let me = self.request("GET", "/user", None)?["id"]
            .as_u64()
            .context("publisher has no numeric identity")?;
        let marker = format!(
            "<!-- repo-review:{} -->",
            storage::digest(request.as_bytes())
        );
        let comments = self.pages(&format!(
            "/repos/{}/issues/{number}/comments",
            self.repository.repository
        ))?;
        let matches: Vec<_> = comments
            .iter()
            .filter(|c| {
                c.pointer("/user/id").and_then(Value::as_u64) == Some(me)
                    && c["body"].as_str().is_some_and(|b| b.starts_with(&marker))
            })
            .collect();
        if matches.len() > 1 {
            bail!("ambiguous publication; reconcile duplicate markers");
        }
        let body = json!({"body": format!("{marker}\n\nAdvisory repository review (merge enforcement is separate).\n\nHead: `{}`\nBase: `{}`\n\n{}", candidate.head, candidate.base, summary)});
        let target_id = matches
            .first()
            .map(|comment| comment["id"].as_u64().context("missing comment ID"))
            .transpose()?;
        let authority = format!("forgejo-user:{me}");
        let result = publication::once_bound(
            state,
            request,
            Kind::ForgejoComment,
            candidate,
            &body,
            Binding {
                authority: &authority,
                publication_id: target_id,
            },
            || {
                if self.candidate(&candidate.url)? != *candidate {
                    bail!("candidate changed before publication write");
                }
                let result = if let Some(comment) = matches.first() {
                    let id = comment["id"].as_u64().context("missing comment ID")?;
                    self.request(
                        "PATCH",
                        &format!("/repos/{}/issues/comments/{id}", self.repository.repository),
                        Some(body.clone()),
                    )?
                } else {
                    self.request(
                        "POST",
                        &format!(
                            "/repos/{}/issues/{number}/comments",
                            self.repository.repository
                        ),
                        Some(body.clone()),
                    )?
                };
                if result.pointer("/user/id").and_then(Value::as_u64) != Some(me)
                    || result["pull_request_url"].as_str() != Some(candidate.url.as_str())
                    || result["original_author_id"].as_u64() != Some(0)
                    || result["original_author"].as_str() != Some("")
                {
                    bail!("publication PR or publisher identity mismatch");
                }
                Ok(Published {
                    id: result["id"].as_u64().context("missing comment ID")?,
                    authority: authority.clone(),
                    payload: json!({"body":field(&result, "/body")?}),
                    url: Some(field(&result, "/html_url")?.into()),
                })
            },
        )?;
        Ok(field(&result, "/url")?.into())
    }
}

fn field<'a>(value: &'a Value, path: &str) -> Result<&'a str> {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .context("incomplete Forgejo candidate response")
}
fn engine_error(error: anyhow::Error) -> Error {
    Error(error.to_string())
}

impl Forge for Forgejo {
    fn candidate(&mut self, url: &str) -> std::result::Result<Candidate, Error> {
        (|| -> Result<Candidate> {
            let hosts = BTreeMap::from([(self.repository.host.clone(), ForgeKind::Forgejo)]);
            let (repository, number) = Repository::from_pr(url, &hosts)?;
            if repository != self.repository {
                bail!("Forgejo target identity mismatch");
            }
            let value = self.request(
                "GET",
                &format!("/repos/{}/pulls/{number}", repository.repository),
                None,
            )?;
            let result = Candidate {
                url: repository.pr_url(number),
                source_repository: format!(
                    "{}/{}",
                    repository.host,
                    field(&value, "/head/repo/full_name")?
                ),
                source_branch: field(&value, "/head/ref")?.into(),
                target_branch: field(&value, "/base/ref")?.into(),
                head: field(&value, "/head/sha")?.into(),
                base: field(&value, "/base/sha")?.into(),
                draft: value["draft"].as_bool().unwrap_or(false),
                open: field(&value, "/state")? == "open"
                    && !value["merged"].as_bool().unwrap_or(false),
            };
            result.validate()?;
            Ok(result)
        })()
        .map_err(engine_error)
    }

    fn checks(
        &mut self,
        candidate: &Candidate,
        policy: &Policy,
    ) -> std::result::Result<Vec<String>, Error> {
        (|| -> Result<Vec<String>> {
            let statuses = self.pages(&format!(
                "/repos/{}/statuses/{}",
                self.repository.repository, candidate.head
            ))?;
            let mut blockers = vec![];
            for check in &policy.required_checks {
                if check.app_id.is_some() {
                    blockers.push(format!(
                        "{} requires an App-bound check unavailable on Forgejo",
                        check.name
                    ));
                    continue;
                }
                let latest = statuses
                    .iter()
                    .filter(|s| s["context"].as_str() == Some(check.name.as_str()))
                    .max_by_key(|s| s["id"].as_u64().unwrap_or(0));
                if !latest.is_some_and(|s| s["state"].as_str() == Some("success")) {
                    blockers.push(format!(
                        "required CI context {} is missing or unsuccessful",
                        check.name
                    ));
                }
            }
            // Producer authenticity/expected-head enforcement belongs to the extra
            // merge gate. CI status names alone never qualify that separate gate.
            Ok(blockers)
        })()
        .map_err(engine_error)
    }
}
