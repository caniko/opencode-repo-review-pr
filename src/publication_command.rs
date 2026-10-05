//! Publication command parsing/dispatch, shared by the executable and offline tests.
use crate::{app::Application, cli_target::TargetArgs, forge_publication::Update};
use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::Value;

#[derive(Subcommand)]
pub enum PublicationCommand {
    /// Record a source-backed exact-finding disposition; fresh gate remains required.
    Disposition {
        request: String,
        #[arg(long)]
        finding: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        evidence: String,
    },
    /// Explicit initial Forgejo advisory publication for completed current evidence.
    Publish(TargetArgs),
    /// Explicitly publish a review-only GitHub check using the dedicated policy App.
    PublishCheck {
        #[command(flatten)]
        target: TargetArgs,
        #[arg(long)]
        details_url: String,
    },
    /// Adopt a frozen historical publication using authoritative reads; no acceptance or forge writes.
    ReconcilePublication(PublicationArgs),
    /// Preview or explicitly PATCH a reconciled current publication; never create/replay one.
    UpdatePublication {
        #[command(flatten)]
        publication: PublicationArgs,
        #[arg(long)]
        expected_publication_digest: String,
        #[arg(long)]
        details_url: Option<String>,
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Args)]
pub struct PublicationArgs {
    #[command(flatten)]
    target: TargetArgs,
    #[arg(long)]
    request: String,
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    publication_id: u64,
}

impl PublicationCommand {
    pub fn invoke(self, app: &Application) -> Result<Value> {
        match self {
            Self::Disposition {
                request,
                finding,
                reason,
                evidence,
            } => app.disposition(&request, &finding, &reason, &evidence),
            Self::Publish(target) => app.publish(&target.into()),
            Self::PublishCheck {
                target,
                details_url,
            } => app.publish_check(&target.into(), &details_url),
            Self::ReconcilePublication(publication) => app.reconcile_publication(
                &publication.target.into(),
                &publication.request,
                publication.publication_id,
            ),
            Self::UpdatePublication {
                publication,
                expected_publication_digest,
                details_url,
                apply,
            } => app.update_publication(
                &publication.target.into(),
                Update {
                    request: &publication.request,
                    publication_id: publication.publication_id,
                    expected_digest: &expected_publication_digest,
                    details_url: details_url.as_deref(),
                    apply,
                },
            ),
        }
    }
}

#[cfg(test)]
#[path = "publication_command/tests.rs"]
mod tests;
