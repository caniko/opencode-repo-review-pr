use crate::app::Target;
use clap::Args;
use std::path::PathBuf;

#[derive(Clone, Args)]
pub struct TargetArgs {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    #[arg(long)]
    pub remote: Option<String>,
    #[arg(long)]
    pub pr: Option<String>,
    #[arg(long)]
    pub base: Option<String>,
    #[arg(long)]
    pub working_tree: bool,
    #[arg(long)]
    pub remote_only: bool,
    #[arg(long)]
    pub expected_head: Option<String>,
    /// Bound pending observation; zero collects once. Waiting never redispatches work.
    #[arg(long, default_value_t=600, value_parser=clap::value_parser!(u64).range(0..=3600))]
    pub timeout_seconds: u64,
}

impl From<TargetArgs> for Target {
    fn from(t: TargetArgs) -> Self {
        Self {
            repo: t.repo,
            remote: t.remote,
            pr: t.pr,
            base: t.base,
            working_tree: t.working_tree,
            remote_only: t.remote_only,
            expected_head: t.expected_head,
        }
    }
}
