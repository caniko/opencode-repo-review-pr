use anyhow::{bail, Result};
use std::path::Path;

pub const NAMES: [&str; 5] = [
    "repo-review-plan",
    "repo-review-local",
    "repo-review-pr",
    "repo-review-status",
    "repo-review-budget",
];
pub const TEMPLATES: [&str; 5] = [
    include_str!("../commands/repo-review-plan.md"),
    include_str!("../commands/repo-review-local.md"),
    include_str!("../commands/repo-review-pr.md"),
    include_str!("../commands/repo-review-status.md"),
    include_str!("../commands/repo-review-budget.md"),
];

/// Refuse to overwrite custom commands. Validate every collision before writing.
pub fn install(directory: &Path) -> Result<()> {
    for (name, template) in NAMES.iter().zip(TEMPLATES) {
        let path = directory.join(format!("{name}.md"));
        if path.exists() && std::fs::read_to_string(&path)? != template {
            bail!("custom command already exists: {}", path.display());
        }
    }
    std::fs::create_dir_all(directory)?;
    for (name, template) in NAMES.iter().zip(TEMPLATES) {
        let path = directory.join(format!("{name}.md"));
        if !path.exists() {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            file.write_all(template.as_bytes())?;
            file.sync_all()?;
        }
    }
    Ok(())
}
