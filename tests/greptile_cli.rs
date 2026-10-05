use canix_toolbelt::review::{Candidate, Policy};
use repo_review::greptile_cli::GreptileCli;
use serde_json::json;

#[test]
fn cli_completion_and_score_cannot_establish_clean_content() {
    let candidate = Candidate {
        url: "https://local.invalid/review/snapshot".into(),
        source_repository: "github.com/caniko/demo".into(),
        source_branch: "feature".into(),
        target_branch: "trunk".into(),
        head: "a".repeat(40),
        base: "b".repeat(40),
        draft: false,
        open: true,
    };
    let policy = Policy {
        transport: "cli-v3.2.3".into(),
        ..Policy::default()
    };
    let status = json!({"status":"COMPLETED","runId":"run-1","headSha":candidate.head,"baseSha":candidate.base,"commentCount":1,"confidence":5});
    let content = json!({"summary":null,"confidence":5,"comments":[{"id":"finding-1","body":"Lost update","path":"file.rs","startLine":12,"severity":"high"}]});
    let review =
        GreptileCli::normalize(&content, &status, &candidate, &policy, &candidate.base).unwrap();
    assert_eq!(policy.evaluate(&candidate, &review).exit_code(), 20);
    let mut mismatched = status.clone();
    mismatched["baseSha"] = json!("c".repeat(40));
    assert!(
        GreptileCli::normalize(&content, &mismatched, &candidate, &policy, &candidate.base)
            .is_err()
    );
    mismatched = status;
    mismatched["commentCount"] = json!(2);
    assert!(
        GreptileCli::normalize(&content, &mismatched, &candidate, &policy, &candidate.base)
            .is_err()
    );
}

#[test]
fn oauth_refresh_preserves_identity_but_account_changes_invalidate_it() {
    use repo_review::greptile_cli::auth_identity_digest;
    let first = json!({"method":"oauth","subject":"user-1","email":"user@example.invalid","accessToken":"old"});
    let mut refreshed = first.clone();
    refreshed["accessToken"] = json!("new");
    assert_eq!(
        auth_identity_digest(&serde_json::to_vec(&first).unwrap()).unwrap(),
        auth_identity_digest(&serde_json::to_vec(&refreshed).unwrap()).unwrap()
    );
    refreshed["subject"] = json!("user-2");
    assert_ne!(
        auth_identity_digest(&serde_json::to_vec(&first).unwrap()).unwrap(),
        auth_identity_digest(&serde_json::to_vec(&refreshed).unwrap()).unwrap()
    );
}
