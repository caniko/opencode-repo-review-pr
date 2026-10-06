use canix_toolbelt::review::{Candidate, Intent, Policy, Provider};
use repo_review::roborev::{decode_http, normalize_review, Roborev};
use repo_review::router::Scope;
use serde_json::json;
use std::{path::Path, process::Command};

fn fixture_git(repo: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn divergent_history() -> (tempfile::TempDir, Candidate, String) {
    let directory = tempfile::tempdir().unwrap();
    let repo = directory.path();
    fixture_git(repo, &["init", "--quiet", "--initial-branch=trunk"]);
    fixture_git(repo, &["config", "user.name", "Disposable Review Fixture"]);
    fixture_git(repo, &["config", "user.email", "fixture@example.invalid"]);
    fixture_git(repo, &["config", "commit.gpgsign", "false"]);
    fixture_git(repo, &["config", "core.hooksPath", "/dev/null"]);
    std::fs::write(repo.join("feature.txt"), "ancestor\n").unwrap();
    fixture_git(repo, &["add", "feature.txt"]);
    fixture_git(repo, &["commit", "--quiet", "-m", "ancestor"]);
    let ancestor = fixture_git(repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("target-only.txt"), "target advancement\n").unwrap();
    fixture_git(repo, &["add", "target-only.txt"]);
    fixture_git(repo, &["commit", "--quiet", "-m", "advance target"]);
    let base = fixture_git(repo, &["rev-parse", "HEAD"]);
    fixture_git(repo, &["checkout", "--quiet", "-b", "feature", &ancestor]);
    std::fs::write(repo.join("feature.txt"), "feature change\n").unwrap();
    fixture_git(repo, &["add", "feature.txt"]);
    fixture_git(repo, &["commit", "--quiet", "-m", "advance feature"]);
    let head = fixture_git(repo, &["rev-parse", "HEAD"]);
    assert_eq!(fixture_git(repo, &["merge-base", &base, &head]), ancestor);
    assert_ne!(base, ancestor);
    assert_ne!(head, ancestor);
    let mut selected = candidate();
    selected.base = base;
    selected.head = head;
    (directory, selected, ancestor)
}

fn candidate() -> Candidate {
    Candidate {
        url: "https://codefloe.com/caniko/demo/pulls/1".into(),
        source_repository: "codefloe.com/caniko/demo".into(),
        source_branch: "feature".into(),
        target_branch: "trunk".into(),
        head: "a".repeat(40),
        base: "b".repeat(40),
        draft: false,
        open: true,
    }
}

#[test]
fn execution_success_cannot_hide_findings_or_incomplete_coverage() {
    let job =
        json!({"id":12,"uuid":"uuid","git_ref":format!("{}..{}","b".repeat(40),"a".repeat(40))});
    let mut raw = json!({"id":34,"job_id":12,"job":job,"structured_output":{"schema_version":2,"summary":"review","verdict":"pass","findings":[{"problem":"lost data","fix":"persist it","severity":"high","location":"file.rs:1"}]},"file_coverage":{"reviewed":1,"excluded":0}});
    let policy = Policy {
        provider: "roborev".into(),
        transport: "unix-http-v0.71".into(),
        ..Policy::default()
    };
    let review =
        normalize_review(&raw, &job, &candidate(), &policy, 1, Scope::PullRequest).unwrap();
    assert_eq!(review.provider_job_id.as_deref(), Some("12"));
    assert_eq!(review.provider_review_id.as_deref(), Some("34"));
    assert_eq!(review.reviewed_base, Some(candidate().base));
    assert_eq!(policy.evaluate(&candidate(), &review).exit_code(), 20);
    raw["structured_output"]["findings"] = json!([]);
    raw["file_coverage"]["excluded"] = json!(1);
    let review =
        normalize_review(&raw, &job, &candidate(), &policy, 1, Scope::PullRequest).unwrap();
    assert_eq!(policy.evaluate(&candidate(), &review).exit_code(), 22);
    raw["job"]["uuid"] = json!("other");
    assert!(normalize_review(&raw, &job, &candidate(), &policy, 1, Scope::PullRequest).is_err());
}

#[test]
fn bounded_http_decoder_rejects_truncation_and_handles_chunked_json() {
    assert_eq!(
        decode_http(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n")
            .unwrap(),
        json!({})
    );
    assert!(decode_http(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n{}").is_err());
    assert!(decode_http(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nff\r\n{}").is_err());
}

#[test]
fn pull_request_rejects_merge_base_review_after_target_diverges() {
    let (_directory, selected, ancestor) = divergent_history();
    let job =
        json!({"id":12,"uuid":"range-job","git_ref":format!("{}..{}", ancestor, selected.head)});
    let raw = json!({"id":34,"job_id":12,"job":job,
        "structured_output":{"schema_version":2,"summary":"No findings","verdict":"pass","findings":[]},
        "file_coverage":{"reviewed":2,"excluded":0}});
    let policy = Policy {
        provider: "roborev".into(),
        transport: "unix-http-v0.71".into(),
        ..Policy::default()
    };
    assert!(
        normalize_review(&raw, &job, &selected, &policy, 2, Scope::PullRequest).is_err(),
        "a merge-base review must not attest the observed target SHA"
    );
}

#[test]
fn divergent_target_range_qualifies_but_local_merge_base_remains_advisory() {
    let (directory, selected, ancestor) = divergent_history();
    let exact = format!("{}..{}", selected.base, selected.head);
    let local = format!("{}..{}", ancestor, selected.head);
    assert_eq!(
        fixture_git(directory.path(), &["diff", "--name-only", &exact])
            .lines()
            .count(),
        2
    );
    assert_eq!(
        fixture_git(directory.path(), &["diff", "--name-only", &local])
            .lines()
            .count(),
        1
    );
    let policy = Policy {
        provider: "roborev".into(),
        transport: "unix-http-v0.71".into(),
        ..Policy::default()
    };
    for (git_ref, scope, files) in [(exact, Scope::PullRequest, 2), (local, Scope::Committed, 1)] {
        let job = json!({"id":12,"uuid":"range-job","git_ref":git_ref});
        let raw = json!({"id":34,"job_id":12,"job":job,
            "structured_output":{"schema_version":2,"summary":"No findings","verdict":"pass","findings":[]},
            "file_coverage":{"reviewed":files,"excluded":0}});
        let review = normalize_review(&raw, &job, &selected, &policy, files, scope).unwrap();
        assert_eq!(
            review.reviewed_base.as_deref(),
            Some(if scope == Scope::PullRequest {
                selected.base.as_str()
            } else {
                ancestor.as_str()
            })
        );
        assert_eq!(policy.evaluate(&selected, &review).exit_code(), 0);
        let publication_policy = Policy {
            transport: "github-receipt-v1".into(),
            ..policy.clone()
        };
        assert_ne!(
            publication_policy.evaluate(&selected, &review).exit_code(),
            0
        );
    }
}

#[test]
fn wrong_pr_range_is_rejected_before_any_daemon_request() {
    let (directory, selected, ancestor) = divergent_history();
    let socket = directory.path().join("daemon.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let settings = repo_review::config::Roborev {
        socket: Some(socket),
        qualified: true,
        qualification_receipt: "synthetic offline daemon fixture".into(),
        ..Default::default()
    };
    let policy = Policy {
        provider: "roborev".into(),
        transport: "unix-http-v0.71".into(),
        ..Policy::default()
    };
    let state = directory.path().join("state");
    let mut provider = Roborev::new(
        &settings,
        directory.path().into(),
        format!("{}..{}", ancestor, selected.head),
        None,
        2,
        state.clone(),
        Scope::PullRequest,
    )
    .unwrap();
    let intent = Intent {
        schema_version: 1,
        id: "wrong-range".into(),
        candidate: selected,
        policy_digest: policy.digest().unwrap(),
        baseline_review_ids: vec![],
    };
    assert!(provider
        .submit(&intent)
        .unwrap_err()
        .to_string()
        .contains("full observed target SHA"));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(!state.exists());
}

#[test]
fn historical_roborev_records_without_base_attestation_cannot_grant_receipt_acceptance() {
    let job =
        json!({"id":12,"uuid":"uuid","git_ref":format!("{}..{}","b".repeat(40),"a".repeat(40))});
    let raw = json!({"id":34,"job_id":12,"job":job,
        "structured_output":{"schema_version":2,"summary":"No findings","verdict":"pass","findings":[]},
        "file_coverage":{"reviewed":1,"excluded":0}});
    let policy = Policy {
        provider: "roborev".into(),
        transport: "github-receipt-v1".into(),
        ..Policy::default()
    };
    let review =
        normalize_review(&raw, &job, &candidate(), &policy, 1, Scope::PullRequest).unwrap();
    assert_eq!(policy.evaluate(&candidate(), &review).exit_code(), 0);
    let mut saved = serde_json::to_value(review).unwrap();
    for field in ["reviewedBase", "providerJobId", "providerReviewId"] {
        saved.as_object_mut().unwrap().remove(field);
    }
    let historical = serde_json::from_value(saved).unwrap();
    assert_eq!(policy.evaluate(&candidate(), &historical).exit_code(), 22);
}

#[test]
fn chunked_json_requires_complete_terminator_and_consumes_trailers() {
    for response in [
        &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n"[..],
        &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\ntrailing"[..],
        &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\nnot-a-trailer\r\n\r\n"[..],
    ] {
        assert!(decode_http(response).is_err(), "accepted incomplete/invalid framing");
    }
    assert_eq!(
        decode_http(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\nX-Fixture: complete\r\n\r\n").unwrap(),
        json!({})
    );
}
