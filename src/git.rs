use crate::{config::Config, identity::Repository, storage};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub fn command(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::Builder::new()
        .prefix("repo-review-git-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let (status, stdout, _) =
        crate::process::run(Path::new("git"), args, repo, scratch.path(), 60)?;
    if status != 0 {
        bail!(
            "Git operation failed: {}",
            args.first().unwrap_or(&"discovery")
        );
    }
    Ok(stdout)
}

pub fn text(repo: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(command(repo, args)?)?.trim().to_owned())
}

pub fn sha(repo: &Path, reference: &str) -> Result<String> {
    if reference.starts_with('-') || reference.chars().any(char::is_control) {
        bail!("invalid Git reference");
    }
    let result = text(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
    )?;
    if ![40, 64].contains(&result.len()) || !result.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("expected full commit ID");
    }
    Ok(result)
}

#[derive(Clone, Debug, Serialize)]
pub struct Checkout {
    pub root: PathBuf,
    pub repository: Option<Repository>,
    pub source: Option<Repository>,
    pub remote: Option<String>,
    pub branch: String,
    pub head: String,
}

pub fn discover(path: &Path, remote: Option<&str>, config: &Config) -> Result<Checkout> {
    let root = PathBuf::from(text(path, &["rev-parse", "--show-toplevel"])?).canonicalize()?;
    for state in [
        "MERGE_HEAD",
        "rebase-merge",
        "rebase-apply",
        "CHERRY_PICK_HEAD",
    ] {
        let state_path = text(&root, &["rev-parse", "--git-path", state])?;
        if root.join(state_path).exists() {
            bail!("finish the current merge/rebase/cherry-pick before reviewing");
        }
    }
    let names = text(&root, &["remote"])?;
    let remote = match remote {
        Some(name) if names.lines().any(|n| n == name) => Some(name.to_owned()),
        Some(_) => bail!("selected remote does not exist"),
        None if names.lines().any(|n| n == "origin") => Some("origin".into()),
        None if names.is_empty() => None,
        None => bail!("multiple/noncanonical remotes: select --remote explicitly"),
    };
    let source = if let Some(remote) = &remote {
        let urls = text(&root, &["remote", "get-url", "--all", remote])?;
        if urls.lines().count() != 1 {
            bail!("remote has multiple fetch URLs");
        }
        let mut url = urls;
        for (alias, host) in &config.remote_aliases {
            url = url
                .replace(&format!("@{alias}:"), &format!("@{host}:"))
                .replace(&format!("@{alias}/"), &format!("@{host}/"));
        }
        Some(Repository::from_remote(&url, &config.forges)?)
    } else {
        None
    };
    let repository = source
        .as_ref()
        .map(|source| {
            config
                .canonical_targets
                .get(&source.coordinate())
                .map_or_else(
                    || Ok(source.clone()),
                    |target| Repository::from_remote(target, &config.forges),
                )
        })
        .transpose()?;
    Ok(Checkout {
        branch: text(&root, &["branch", "--show-current"])?,
        head: sha(&root, "HEAD")?,
        root,
        repository,
        source,
        remote,
    })
}

pub fn base(checkout: &Checkout, explicit: Option<&str>) -> Result<String> {
    if let Some(base) = explicit {
        return sha(&checkout.root, base);
    }
    let remote = checkout
        .remote
        .as_ref()
        .context("a local-only repository requires --base")?;
    let symbolic = text(
        &checkout.root,
        &[
            "symbolic-ref",
            "--quiet",
            &format!("refs/remotes/{remote}/HEAD"),
        ],
    )
    .context("base is ambiguous; provide --base (no implicit main/master guessing)")?;
    sha(&checkout.root, &symbolic)
}

pub fn comparison(repo: &Path, base: &str, head: &str) -> Result<String> {
    text(repo, &["merge-base", base, head])
}

/// Copy immutable Git objects into a private detached snapshot, without touching user refs.
pub fn snapshot(
    root: &Path,
    checkout: &Checkout,
    head: &str,
    base: &str,
    request: &str,
) -> Result<PathBuf> {
    snapshot_with(root, head, base, request, |path| {
        command(
            path,
            &[
                "-c",
                "protocol.file.allow=always",
                "fetch",
                "--no-tags",
                "--",
                checkout
                    .root
                    .to_str()
                    .context("checkout path is not UTF-8")?,
                head,
                base,
            ],
        )?;
        Ok(())
    })
}

pub(crate) fn snapshot_with(
    root: &Path,
    head: &str,
    base: &str,
    request: &str,
    populate: impl FnOnce(&Path) -> Result<()>,
) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let snapshots = root.join("snapshots");
    storage::private_dir(&snapshots)?;
    let _lock =
        storage::lock(&snapshots.join(format!("{}.lock", storage::digest(request.as_bytes()))))?;
    let path = root
        .join("snapshots")
        .join(storage::digest(request.as_bytes()));
    if path.exists() {
        storage::private_dir(&path)?;
        if sha(&path, "HEAD")? != head || sha(&path, base)? != base {
            bail!("snapshot identity changed");
        }
        return Ok(path);
    }
    let temporary = tempfile::Builder::new()
        .prefix(".preparing-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(&snapshots)?;
    command(temporary.path(), &["init", "--quiet"])?;
    populate(temporary.path())?;
    command(
        temporary.path(),
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "checkout",
            "--quiet",
            "--detach",
            head,
        ],
    )?;
    std::fs::rename(temporary.path(), &path)?;
    std::fs::File::open(&snapshots)?.sync_all()?;
    Ok(path)
}
