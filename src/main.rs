#![deny(clippy::unwrap_used, clippy::expect_used)]
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use repo_review::{
    app::{Application, Target},
    budget::{Budget, Period},
    cli_target::TargetArgs,
    config::Config,
    publication_command::PublicationCommand,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{io::Read, path::PathBuf};

#[derive(Parser)]
#[command(version, about = "Forge-aware repository review commands for OpenCode")]
struct Cli {
    #[arg(long, global = true, env = "REPO_REVIEW_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long, global = true, env = "REPO_REVIEW_STATE")]
    state_dir: Option<PathBuf>,
    /// Delegate supported operations to one authenticated SSH coordination authority.
    #[arg(long, global = true, env = "REPO_REVIEW_COORDINATOR")]
    coordinator: Option<String>,
    #[command(subcommand)]
    command: Operation,
}

#[derive(Subcommand)]
enum Operation {
    /// Explain forge, comparison, provider and maximum credit use. No dispatch.
    Plan {
        #[command(flatten)]
        target: TargetArgs,
        #[arg(long)]
        pr_scope: bool,
    },
    /// Review tracked local changes; untracked files are explicitly excluded.
    Local(TargetArgs),
    /// Request/resume a PR review on its actual forge.
    Pr(TargetArgs),
    /// Recheck existing PR evidence, CI and the extra forge gate. No dispatch.
    Gate(TargetArgs),
    /// Inspect the last observation; this receipt is not fresh merge authorization.
    Status { request: String },
    #[command(flatten)]
    Publication(PublicationCommand),
    /// Inspect or explicitly reconcile account-wide free-credit accounting.
    Budget {
        #[command(subcommand)]
        command: BudgetCommand,
    },
    /// Show configured capabilities without reviews, logins or daemon autostart.
    Doctor,
    /// Install permission-preserving OpenCode command templates.
    InstallCommands {
        #[arg(long)]
        directory: Option<PathBuf>,
    },
    /// Revalidate current provider evidence, CI and the extra forge gate before merging.
    Merge {
        #[command(flatten)]
        target: TargetArgs,
        #[arg(long)]
        apply: bool,
    },
    /// Versioned one-request SSH/stdio endpoint; uses only server-owned policy/state.
    Rpc,
    #[command(hide = true)]
    CliWorker {
        #[arg(long)]
        receipt: PathBuf,
    },
}

#[derive(Subcommand)]
enum BudgetCommand {
    Show {
        account: String,
    },
    /// Import an evidenced current provider billing period (JSON).
    Reconcile {
        account: String,
        #[arg(long)]
        receipt: PathBuf,
    },
    /// Reconcile an ambiguous reservation using explicit provider/operator evidence.
    Settle {
        account: String,
        request: String,
        #[arg(long)]
        charged: bool,
        #[arg(long)]
        evidence: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rpc {
    version: u32,
    action: String,
    target: Option<Target>,
    request: Option<String>,
    account: Option<String>,
}

fn root(variable: &str, suffix: &str) -> Result<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(suffix)))
        .context("HOME or XDG root is required")
}

fn application(config: Option<PathBuf>, state: Option<PathBuf>) -> Result<Application> {
    let path =
        config.unwrap_or(root("XDG_CONFIG_HOME", ".config")?.join("repo-review/config.json"));
    let config = if path.exists() {
        Config::load(&path)?
    } else {
        Config::default()
    };
    Ok(Application {
        config,
        state: state.unwrap_or(root("XDG_STATE_HOME", ".local/state")?.join("repo-review")),
    })
}

fn invoke(app: &Application, operation: Operation) -> Result<(Value, u8)> {
    match operation {
        Operation::Plan { target, pr_scope } => {
            let target: Target = target.into();
            Ok((app.plan(&target, pr_scope || target.pr.is_some())?, 0))
        }
        Operation::Local(target) => observe(app, target, false),
        Operation::Pr(target) => observe(app, target, true),
        Operation::Gate(target) => report(app.tick(&target.into(), true, false)?, true),
        Operation::Status { request } => Ok((serde_json::to_value(app.status(&request)?)?, 0)),
        Operation::Publication(command) => Ok((command.invoke(app)?, 0)),
        Operation::Budget { command } => {
            let now = canix_toolbelt::review::now_seconds();
            match command {
                BudgetCommand::Show { account } => Ok((
                    serde_json::to_value(Budget::new(&app.state, &account).show(now)?)?,
                    0,
                )),
                BudgetCommand::Reconcile { account, receipt } => {
                    let period: Period = serde_json::from_slice(&std::fs::read(receipt)?)?;
                    let budget = Budget::new(&app.state, &account);
                    budget.reconcile(period, now)?;
                    Ok((serde_json::to_value(budget.show(now)?)?, 0))
                }
                BudgetCommand::Settle {
                    account,
                    request,
                    charged,
                    evidence,
                } => {
                    let budget = Budget::new(&app.state, &account);
                    budget.settle(&request, charged, &evidence, now)?;
                    Ok((serde_json::to_value(budget.show(now)?)?, 0))
                }
            }
        }
        Operation::Doctor => Ok((
            json!({"schema_version":1,"toolbelt":"0.2.0","greptile":{"enabled":app.config.greptile.enabled,"maximum_cost":app.config.greptile.maximum_cost,"cli_contract_qualified":!app.config.greptile.cli_qualification_receipt.is_empty()},"roborev":{"qualified":app.config.roborev.qualified,"socket_configured":app.config.roborev.socket.is_some()},"dedicated_policy_app":app.config.policy_app_id,"dispatches_review":false}),
            0,
        )),
        Operation::InstallCommands { directory } => {
            let directory =
                directory.unwrap_or(root("XDG_CONFIG_HOME", ".config")?.join("opencode/commands"));
            repo_review::commands::install(&directory)?;
            Ok((
                json!({"installed":directory,"commands":repo_review::commands::NAMES}),
                0,
            ))
        }
        Operation::Merge { target, apply } => Ok((app.merge(&target.into(), apply)?, 0)),
        Operation::Rpc => {
            let mut input = vec![];
            std::io::stdin().take(65537).read_to_end(&mut input)?;
            if input.len() > 65536 {
                bail!("RPC input exceeded limit");
            }
            let mut rpc: Rpc = serde_json::from_slice(&input)?;
            if rpc.version != 1 {
                bail!("incompatible RPC version");
            }
            if let Some(target) = &mut rpc.target {
                if target.pr.is_none() || target.working_tree || target.base.is_some() {
                    bail!("remote RPC reviews require an explicit PR URL");
                }
                target.remote_only = true;
            }
            match rpc.action.as_str() {
                "plan" => Ok((app.plan(&rpc.target.context("missing target")?, true)?, 0)),
                "pr" | "gate" => report(
                    app.tick(
                        &rpc.target.context("missing target")?,
                        true,
                        rpc.action == "pr",
                    )?,
                    rpc.action == "gate",
                ),
                "status" => Ok((
                    serde_json::to_value(app.status(&rpc.request.context("missing request")?)?)?,
                    0,
                )),
                "budget" => Ok((
                    serde_json::to_value(
                        Budget::new(&app.state, &rpc.account.context("missing account")?)
                            .show(canix_toolbelt::review::now_seconds())?,
                    )?,
                    0,
                )),
                _ => bail!("unsupported RPC operation"),
            }
        }
        Operation::CliWorker { receipt } => {
            repo_review::greptile_cli::worker(&receipt)?;
            Ok((json!({"worker":"completed"}), 0))
        }
    }
}

fn observe(app: &Application, args: TargetArgs, pr_scope: bool) -> Result<(Value, u8)> {
    let timeout = args.timeout_seconds;
    let target: Target = args.into();
    let started = std::time::Instant::now();
    let mut current = app.tick(&target, pr_scope, true)?;
    let request = current.route.id.clone();
    while current.outcome.verdict == canix_toolbelt::review::Verdict::Pending
        && started.elapsed() < std::time::Duration::from_secs(timeout)
    {
        let now = canix_toolbelt::review::now_seconds();
        let delay = current
            .outcome
            .next_poll_at
            .map_or(2, |at| at.saturating_sub(now).max(1));
        let remaining = std::time::Duration::from_secs(timeout).saturating_sub(started.elapsed());
        std::thread::sleep(std::time::Duration::from_secs(delay).min(remaining));
        if started.elapsed() >= std::time::Duration::from_secs(timeout) {
            break;
        }
        // This observation cannot allocate a new request if the candidate moves.
        current = app.tick(&target, pr_scope, false)?;
        if current.route.id != request {
            bail!("review comparison changed during bounded observation; refresh the plan");
        }
    }
    report(current, false)
}

fn report(report: repo_review::app::Report, gate: bool) -> Result<(Value, u8)> {
    let exit = if gate && !report.merge_blockers.is_empty() {
        22
    } else {
        report.outcome.verdict.exit_code()
    };
    Ok((serde_json::to_value(report)?, exit))
}

fn remote(host: &str, operation: &Operation) -> Result<(Value, u8)> {
    if host.starts_with('-')
        || host.chars().any(|c| c.is_whitespace() || c.is_control())
        || host.contains(['\'', '"', ';', '`', '$'])
    {
        bail!("invalid SSH coordination authority");
    }
    let (action, target, request, account) = match operation {
        Operation::Plan { target, .. } => ("plan", Some(target.clone().into()), None, None),
        Operation::Pr(target) => ("pr", Some(target.clone().into()), None, None),
        Operation::Gate(target) => ("gate", Some(target.clone().into()), None, None),
        Operation::Status { request } => ("status", None, Some(request.clone()), None),
        Operation::Budget {
            command: BudgetCommand::Show { account },
        } => ("budget", None, None, Some(account.clone())),
        _ => bail!(
            "this operation must run on the coordination authority; no independent credit ledger created"
        ),
    };
    let payload = serde_json::to_vec(&Rpc {
        version: 1,
        action: action.into(),
        target,
        request,
        account,
    })?;
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::Builder::new()
        .prefix("repo-review-ssh-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let (exit, stdout, _) = repo_review::process::run_input(
        std::path::Path::new("ssh"),
        &[
            "-T",
            "-oBatchMode=yes",
            "-oConnectTimeout=10",
            "-oServerAliveInterval=10",
            "-oServerAliveCountMax=2",
            "--",
            host,
            "repo-review rpc",
        ],
        &std::env::current_dir()?,
        scratch.path(),
        600,
        Some(&payload),
    )?;
    let data = serde_json::from_slice(&stdout).context(
        "coordinator did not return a complete response; resume, never fallback locally",
    )?;
    Ok((data, u8::try_from(exit).unwrap_or(22)))
}

fn main() {
    let cli = Cli::parse();
    let result = (|| -> Result<(Value, u8)> {
        if let Operation::CliWorker { receipt } = &cli.command {
            repo_review::greptile_cli::worker(receipt)?;
            return Ok((json!({"worker":"completed"}), 0));
        }
        if let Some(host) = cli.coordinator {
            return remote(&host, &cli.command);
        }
        invoke(&application(cli.config, cli.state_dir)?, cli.command)
    })();
    let (value, exit) = match result {
        Ok(result) => result,
        Err(error) => (
            json!({"schema_version":1,"verdict":"blocked","diagnostic":error.to_string()}),
            22,
        ),
    };
    match serde_json::to_string_pretty(&value) {
        Ok(text) => println!("{text}"),
        Err(_) => std::process::exit(22),
    }
    std::process::exit(exit.into());
}
