use repo_review::{config::Config, git};
use std::{path::Path, process::Command, sync::Arc};

fn fixture_git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn failed_preparation_can_resume_and_concurrent_clients_share_one_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    fixture_git(&repo, &["init", "--quiet", "--initial-branch=trunk"]);
    fixture_git(&repo, &["config", "user.name", "Disposable Review Fixture"]);
    fixture_git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    fixture_git(&repo, &["config", "commit.gpgsign", "false"]);
    fixture_git(&repo, &["config", "core.hooksPath", "/dev/null"]);
    std::fs::write(repo.join("file.txt"), "base\n").unwrap();
    fixture_git(&repo, &["add", "file.txt"]);
    fixture_git(&repo, &["commit", "--quiet", "-m", "base"]);
    let checkout = Arc::new(git::discover(&repo, None, &Config::default()).unwrap());
    let state = root.path().join("state");
    assert!(git::snapshot(
        &state,
        &checkout,
        &checkout.head,
        "missing-ref",
        "same-request"
    )
    .is_err());
    assert!(!std::fs::read_dir(state.join("snapshots"))
        .unwrap()
        .any(|entry| entry.unwrap().file_type().unwrap().is_dir()));
    let clients: Vec<_> = (0..2)
        .map(|_| {
            let checkout = checkout.clone();
            let state = state.clone();
            std::thread::spawn(move || {
                git::snapshot(
                    &state,
                    &checkout,
                    &checkout.head,
                    &checkout.head,
                    "same-request",
                )
                .unwrap()
            })
        })
        .collect();
    let snapshots: Vec<_> = clients
        .into_iter()
        .map(|client| client.join().unwrap())
        .collect();
    assert_eq!(snapshots[0], snapshots[1]);
    assert_eq!(git::sha(&snapshots[0], "HEAD").unwrap(), checkout.head);
    assert_eq!(
        git::text(&repo, &["branch", "--show-current"]).unwrap(),
        "trunk"
    );
}
