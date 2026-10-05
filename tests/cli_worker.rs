use repo_review::{
    budget::{Budget, Period},
    config::Config,
    greptile_cli::auth_identity_digest,
    storage,
};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
    time::{Duration, Instant},
};

fn git(repo: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn local(repo: &Path, config: &Path, state: &Path, home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_repo-review"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", home)
        .current_dir(repo)
        .arg("--config")
        .arg(config)
        .arg("--state-dir")
        .arg(state)
        .args(["local", "--base", "trunk", "--timeout-seconds", "0"])
        .output()
        .unwrap()
}

#[test]
fn fake_cli_worker_is_adopted_without_replay_or_a_second_credit() {
    let temporary = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let repo = temporary.path().join("repo");
    let state = temporary.path().join("state");
    let home = temporary.path().join("home");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(&home).unwrap();
    git(&repo, &["init", "--quiet", "--initial-branch=trunk"]);
    git(&repo, &["config", "user.name", "Disposable Review Fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["config", "core.hooksPath", "/dev/null"]);
    std::fs::write(repo.join("file.txt"), "base\n").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "--quiet", "-m", "base"]);
    git(&repo, &["checkout", "--quiet", "-b", "feature"]);
    std::fs::write(repo.join("file.txt"), "changed\n").unwrap();
    git(&repo, &["commit", "--quiet", "-am", "change"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/caniko/disposable-fixture.git",
        ],
    );
    let refs = git(&repo, &["show-ref"]);
    let fake = temporary.path().join("fake-greptile");
    // This executable has no network or credential-reading code. All provider
    // responses are synthetic and the production CLI owns worker/recovery behavior.
    std::fs::write(&fake, r#"#!/bin/sh
set -eu
if [ "$1" = '--version' ]; then printf '3.2.3\n'; exit 0; fi
if [ "$2" = 'status' ]; then
  if [ -f fixture-status.json ]; then cat fixture-status.json
  else printf '{"commit":"%s"}\n' "$(git rev-parse HEAD)"; fi
elif [ "$2" = 'show' ]; then cat fixture-content.json
elif [ "$2" = '--branch' ]; then
  printf 'dispatch\n' >> fixture-dispatches
  printf '{"comments":[],"summary":null,"fileAnalyses":[]}\n' > fixture-content.json
  printf '{"status":"COMPLETED","runId":"synthetic-run","headSha":"%s","baseSha":"%s","commentCount":0}\n' "$(git rev-parse HEAD)" "$(git merge-base "$3" HEAD)" > fixture-status.json
  cat fixture-content.json
else exit 1; fi
"#).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let auth = json!({"method":"oauth", "subject":"synthetic-user", "email":"fixture@example.invalid", "accessToken":"SYNTHETIC"});
    let auth_path = temporary.path().join("auth.json");
    storage::save(&auth_path, &auth).unwrap();
    let mut config = Config::default();
    config.greptile.enabled = true;
    config.greptile.cli = Some(fake);
    config.greptile.cli_identity = Some("fixture".into());
    config.greptile.cli_auth_file = Some(auth_path);
    config.greptile.cli_auth_digest =
        auth_identity_digest(&serde_json::to_vec(&auth).unwrap()).unwrap();
    config.greptile.cli_qualification_receipt = "synthetic offline fixture only".into();
    config
        .greptile
        .billing_accounts
        .insert("fixture".into(), "fixture-account".into());
    let config_path = temporary.path().join("config.json");
    storage::save(&config_path, &config).unwrap();
    let now = canix_toolbelt::review::now_seconds();
    let budget = Budget::new(&state, "fixture-account");
    budget
        .reconcile(
            Period {
                start: now - 1,
                end: now + 3600,
                allowance: 50,
                reconciled_usage: 49,
                evidence: "synthetic provider balance".into(),
                no_overage: true,
                included_requests: vec![],
            },
            now,
        )
        .unwrap();
    let first = local(&repo, &config_path, &state, &home);
    let initial: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first.status.code(), Some(22), "{initial}");
    let deadline = Instant::now() + Duration::from_secs(10);
    let completed = loop {
        std::thread::sleep(Duration::from_millis(100));
        let next = local(&repo, &config_path, &state, &home);
        let value: Value = serde_json::from_slice(&next.stdout).unwrap();
        if next.status.success() {
            break value;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not complete: {value}"
        );
    };
    assert_eq!(initial["route"]["id"], completed["route"]["id"]);
    assert_eq!(completed["route"]["provider"], "greptile");
    assert_eq!(completed["outcome"]["review"]["id"], "synthetic-run");
    assert!(completed["advisory"].as_bool().unwrap());
    assert!(!completed["merge_blockers"].as_array().unwrap().is_empty());
    let snapshot = std::fs::read_dir(state.join("snapshots"))
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_type().unwrap().is_dir())
        .unwrap()
        .path();
    assert_eq!(
        std::fs::read_to_string(snapshot.join("fixture-dispatches")).unwrap(),
        "dispatch\n"
    );
    let balance = budget.show(canix_toolbelt::review::now_seconds()).unwrap();
    assert_eq!(balance.charged, 1);
    assert_eq!(balance.available, 0);
    assert_eq!(git(&repo, &["show-ref"]), refs);
    assert_eq!(git(&repo, &["branch", "--show-current"]), "feature");
}

#[test]
fn failed_cli_worker_retains_diagnostics_and_cannot_replay() {
    use canix_toolbelt::review::{Candidate, Intent, Provider};
    for phase in ["version", "status"] {
        let temporary = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let state = temporary.path().join("state");
        let attempted = temporary.path().join("attempted");
        let fake = temporary.path().join("fake-greptile");
        let version = if phase == "version" { "9.9.9" } else { "3.2.3" };
        std::fs::write(
            &fake,
            format!(
                r#"#!/bin/sh
if [ "$1" = '--version' ]; then
  printf 'version\n' >> '{}'
  printf '{}\n'
elif [ "$2" = '--branch' ]; then
  printf 'review\n' >> '{}'
  printf '{{"comments":[],"summary":null,"fileAnalyses":[]}}\n'
else
  printf 'status\n' >> '{}'
  printf 'unsupported-status\n'
fi
"#,
                attempted.display(),
                version,
                attempted.display(),
                attempted.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let auth = json!({"method":"oauth","subject":"synthetic-user","email":"fixture@example.invalid","accessToken":"SYNTHETIC_PRIVATE_AUTH"});
        let auth_path = temporary.path().join("auth.json");
        storage::save(&auth_path, &auth).unwrap();
        let mut config = Config::default();
        config.greptile.cli = Some(fake);
        config.greptile.cli_auth_file = Some(auth_path);
        config.greptile.cli_auth_digest =
            auth_identity_digest(&serde_json::to_vec(&auth).unwrap()).unwrap();
        config.greptile.cli_qualification_receipt = "synthetic offline fixture only".into();
        let intent = Intent {
            schema_version: 1,
            id: storage::digest(b"failed-worker"),
            candidate: Candidate {
                url: "https://local.invalid/review/fixture".into(),
                source_repository: "github.com/caniko/fixture".into(),
                source_branch: "feature".into(),
                target_branch: "trunk".into(),
                head: "a".repeat(40),
                base: "b".repeat(40),
                draft: false,
                open: true,
            },
            policy_digest: repo_review::router::policy(
                &config,
                repo_review::router::ProviderName::Greptile,
                true,
            )
            .digest()
            .unwrap(),
            baseline_review_ids: vec![],
        };
        let receipt = state.join(format!("{}.json", intent.id));
        storage::save(&receipt, &json!({"intent":intent,"warnings":null})).unwrap();
        let payload = state.join("worker.json");
        storage::save(&payload, &json!({"settings":config.greptile,"snapshot":temporary.path(),"state":state,"intent":intent,"comparison_base":intent.candidate.base})).unwrap();
        let run = || {
            Command::new(env!("CARGO_BIN_EXE_repo-review"))
                .env_clear()
                .env("HOME", temporary.path())
                .env("PATH", std::env::var_os("PATH").unwrap())
                .args(["cli-worker", "--receipt"])
                .arg(&payload)
                .output()
                .unwrap()
        };
        let first = run();
        assert_eq!(first.status.code(), Some(22));
        let failed: Value = storage::read(&receipt).unwrap().unwrap();
        assert_eq!(failed["worker_failure"]["phase"], phase);
        assert_eq!(
            failed["worker_failure"]["review_dispatch_attempted"],
            phase == "status"
        );
        if phase == "status" {
            assert_eq!(
                failed["content"],
                json!({"comments":[],"summary":null,"fileAnalyses":[]})
            );
            assert_eq!(failed["exit"], 0);
        }
        assert_eq!(failed["worker_started"], true);
        assert!(!serde_json::to_string(&failed)
            .unwrap()
            .contains("SYNTHETIC_PRIVATE_AUTH"));
        let mut provider = repo_review::greptile_cli::GreptileCli {
            settings: config.greptile.clone(),
            snapshot: temporary.path().into(),
            state,
            comparison_base: intent.candidate.base.clone(),
        };
        let policy =
            repo_review::router::policy(&config, repo_review::router::ProviderName::Greptile, true);
        assert!(provider
            .inspect(&intent.candidate, &policy, Some(&intent))
            .unwrap_err()
            .to_string()
            .contains(&format!("worker failed during {phase}")));
        assert_eq!(run().status.code(), Some(22));
        assert_eq!(storage::read::<Value>(&receipt).unwrap().unwrap(), failed);
        assert_eq!(
            std::fs::read_to_string(attempted).unwrap(),
            if phase == "version" {
                "version\n"
            } else {
                "version\nreview\nstatus\n"
            }
        );
    }
}
