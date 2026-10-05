use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository(root: &Path) {
    git(root, &["init", "--quiet", "--initial-branch=trunk"]);
    git(root, &["config", "user.name", "Disposable Review Fixture"]);
    git(root, &["config", "user.email", "fixture@example.invalid"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    // Disposable fixtures must not execute operator-global production hooks.
    git(root, &["config", "core.hooksPath", "/dev/null"]);
    std::fs::write(root.join("file.txt"), "initial\n").unwrap();
    git(root, &["add", "file.txt"]);
    git(root, &["commit", "--quiet", "-m", "base"]);
    git(root, &["checkout", "--quiet", "-b", "feature"]);
    std::fs::write(root.join("file.txt"), "changed\n").unwrap();
    git(root, &["add", "file.txt"]);
    git(root, &["commit", "--quiet", "-m", "change"]);
}

fn cli(root: &Path, state: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_repo-review"))
        .current_dir(root)
        .env_remove("REPO_REVIEW_CONFIG")
        .env_remove("REPO_REVIEW_COORDINATOR")
        .args([
            "--state-dir",
            state.to_str().unwrap(),
            "--config",
            root.join("absent-policy.json").to_str().unwrap(),
        ])
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn forgejo_origin_with_github_mirror_never_selects_greptile() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    repository(root.path());
    git(
        root.path(),
        &[
            "remote",
            "add",
            "origin",
            "git@codefloe.com:caniko/demo.git",
        ],
    );
    git(
        root.path(),
        &[
            "remote",
            "add",
            "github-mirror",
            "git@github.com:caniko/demo.git",
        ],
    );
    let output = cli(root.path(), state.path(), &["plan", "--base", "trunk"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["repository"]["forge"], "forgejo");
    assert_eq!(plan["routing_reason"], "forgejo-roborev");
    assert_eq!(
        std::fs::read_dir(state.path()).unwrap().count(),
        0,
        "planning must not allocate state or credits"
    );
}

#[test]
fn unsafe_arguments_and_unqualified_worker_cannot_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    repository(root.path());
    let output = cli(root.path(), state.path(), &["plan", "--base", "--evil"]);
    assert!(!output.status.success());
    let output = cli(root.path(), state.path(), &["local", "--base", "trunk"]);
    assert_eq!(output.status.code(), Some(22));
    assert!(String::from_utf8_lossy(&output.stdout).contains("not qualified"));
    let head = Command::new("git")
        .current_dir(root.path())
        .args(["branch", "--show-current"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(head.stdout).unwrap().trim(), "feature");
}

#[test]
fn commands_install_idempotently_and_preserve_custom_definitions() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let commands = root.path().join("commands");
    let args = [
        "install-commands",
        "--directory",
        commands.to_str().unwrap(),
    ];
    assert!(cli(root.path(), state.path(), &args).status.success());
    assert!(cli(root.path(), state.path(), &args).status.success());
    let path = commands.join("repo-review-pr.md");
    std::fs::write(&path, "my custom command").unwrap();
    assert_eq!(
        cli(root.path(), state.path(), &args).status.code(),
        Some(22)
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "my custom command");
    for template in repo_review::commands::TEMPLATES {
        assert!(!template.contains("!`"));
        assert!(!template.contains("model:"));
    }
}
