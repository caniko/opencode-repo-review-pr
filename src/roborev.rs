//! OpenCode selection and trusted worker policy over Toolbelt's shared provider.
use crate::{config::Roborev as Settings, router::Scope};
use anyhow::{bail, Context, Result};
use canix_toolbelt::review::{
    Candidate, Capabilities, Error, Evidence, Intent, Policy, Provider, Review, RoborevHttp,
    RoborevLocal, RoborevLocalRequest, RoborevLocalScope,
};
use serde_json::Value;
use std::{path::PathBuf, time::Duration};

pub struct Daemon(RoborevHttp);

impl Daemon {
    pub fn new(settings: &Settings) -> Result<Self> {
        if !settings.qualified || settings.qualification_receipt.is_empty() {
            bail!(
                "roborev worker is not qualified; provide its containment receipt in trusted policy"
            );
        }
        Ok(Self(RoborevHttp::new(
            settings.socket.clone().context("roborev socket missing")?,
            Duration::from_secs(20),
        )?))
    }

    pub fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        Ok(self.0.request(method, path, body.as_ref())?)
    }
}

pub fn decode_http(response: &[u8]) -> Result<Value> {
    Ok(canix_toolbelt::review::decode_roborev_http(response)?)
}

fn local_scope(scope: Scope) -> RoborevLocalScope {
    match scope {
        Scope::PullRequest => RoborevLocalScope::PullRequest,
        Scope::Committed => RoborevLocalScope::Committed,
        Scope::WorkingTree => RoborevLocalScope::WorkingTree,
    }
}

pub struct Roborev(RoborevLocal);

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
        Ok(Self(RoborevLocal::new(
            Daemon::new(settings)?.0,
            RoborevLocalRequest {
                snapshot,
                git_ref,
                agent: settings.agent.clone(),
                diff,
                expected_files,
                scope: local_scope(scope),
            },
            state,
        )?))
    }
}

impl Provider for Roborev {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    fn inspect(
        &mut self,
        candidate: &Candidate,
        policy: &Policy,
        intent: Option<&Intent>,
    ) -> std::result::Result<Evidence, Error> {
        self.0.inspect(candidate, policy, intent)
    }

    fn submit(&mut self, intent: &Intent) -> std::result::Result<String, Error> {
        self.0.submit(intent)
    }
}

pub fn normalize_review(
    raw: &Value,
    job: &Value,
    candidate: &Candidate,
    policy: &Policy,
    expected_files: usize,
    scope: Scope,
) -> Result<Review> {
    Ok(canix_toolbelt::review::normalize_roborev_local(
        raw,
        job,
        candidate,
        policy,
        expected_files,
        local_scope(scope),
    )?)
}
