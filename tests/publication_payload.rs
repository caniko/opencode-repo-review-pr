use canix_toolbelt::review::{Candidate, Outcome, Policy, Review, Verdict};
use repo_review::{
    app::Report,
    forge_publication::github_payload,
    router::{ProviderName, Route, Scope},
};

fn report() -> (Report, Policy) {
    let candidate = Candidate {
        url: "https://github.com/caniko/fixture/pull/1".into(),
        source_repository: "github.com/caniko/fixture".into(),
        source_branch: "feature".into(),
        target_branch: "trunk".into(),
        head: "a".repeat(40),
        base: "b".repeat(40),
        draft: false,
        open: true,
    };
    let policy = Policy::default();
    let review = Review {
        id: "one".into(),
        provider_job_id: None,
        provider_review_id: None,
        provider: policy.provider.clone(),
        reviewed_head: Some(candidate.head.clone()),
        requested_base: Some(candidate.base.clone()),
        reviewed_base: None,
        policy_digest: policy.digest().unwrap(),
        completed: true,
        complete_findings: true,
        findings: vec![],
        url: "fixture".into(),
    };
    let route = Route {
        schema_version: 1,
        id: "c".repeat(64),
        candidate: candidate.clone(),
        config_digest: "d".repeat(64),
        provider: ProviderName::Greptile,
        reason: "fixture".into(),
        account: None,
        cost: 0,
        scope: Scope::PullRequest,
    };
    let outcome = Outcome {
        schema_version: 1,
        verdict: Verdict::Ready,
        candidate,
        request_id: Some(route.id.clone()),
        review: Some(review),
        blockers: vec![],
        next_action: "fixture".into(),
        next_poll_at: None,
        observed_at: 1,
    };
    (
        Report {
            schema_version: 1,
            route,
            outcome,
            advisory: false,
            merge_blockers: vec!["extra forge gate required".into()],
        },
        policy,
    )
}

#[test]
fn pending_review_cannot_consume_the_single_initial_publication() {
    let (mut report, policy) = report();
    report.outcome.verdict = Verdict::Pending;
    report.outcome.review = None;
    assert!(github_payload(
        &report,
        &report.route.candidate,
        &policy,
        "https://example.invalid/details"
    )
    .is_err());
}

#[test]
fn blocked_outcome_cannot_publish_success_from_an_individually_ready_review() {
    let (mut report, policy) = report();
    report.outcome.verdict = Verdict::Blocked;
    report
        .outcome
        .blockers
        .push("unresolved lineage evidence".into());
    let payload = github_payload(
        &report,
        &report.route.candidate,
        &policy,
        "https://example.invalid/details",
    )
    .unwrap();
    assert_eq!(payload["conclusion"], "failure");
}

#[test]
fn completed_review_requires_the_exact_comparison_and_keeps_extra_gate_separate() {
    let (report, policy) = report();
    let payload = github_payload(
        &report,
        &report.route.candidate,
        &policy,
        "https://example.invalid/details",
    )
    .unwrap();
    assert_eq!(payload["conclusion"], "success");
    let mut moved = report.route.candidate.clone();
    moved.base = "e".repeat(40);
    assert!(github_payload(&report, &moved, &policy, "https://example.invalid/details").is_err());
}

#[test]
fn repair_rejects_missing_explicit_target_and_changed_policy_before_any_forge_access() {
    use repo_review::{
        app::{Application, Target},
        config::Config,
        forge_publication::Update,
        storage,
    };
    let root = tempfile::tempdir().unwrap();
    let (mut report, _) = report();
    let config = Config::default();
    report.route.config_digest = config.digest().unwrap();
    storage::save(
        &root
            .path()
            .join(format!("results/{}.json", report.route.id)),
        &report,
    )
    .unwrap();
    let target = Target {
        pr: Some(report.route.candidate.url.clone()),
        expected_head: Some(report.route.candidate.head.clone()),
        remote_only: true,
        ..Target::default()
    };
    for case in [
        "missing-pr",
        "missing-head",
        "changed-head",
        "local-base",
        "changed-policy",
    ] {
        let mut selected = target.clone();
        let mut policy = config.clone();
        match case {
            "missing-pr" => selected.pr = None,
            "missing-head" => selected.expected_head = None,
            "changed-head" => selected.expected_head = Some("f".repeat(40)),
            "local-base" => selected.base = Some("trunk".into()),
            "changed-policy" => policy.policy_app_id = Some(999),
            _ => unreachable!(),
        }
        let app = Application {
            config: policy,
            state: root.path().to_owned(),
        };
        let error = app
            .reconcile_publication(&selected, &report.route.id, 7)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("original explicit PR/head and unchanged trusted policy"),
            "{case}: {error}"
        );
        let error = app
            .update_publication(
                &selected,
                Update {
                    request: &report.route.id,
                    publication_id: 7,
                    expected_digest: &"e".repeat(64),
                    details_url: Some("https://example.invalid/details"),
                    apply: true,
                },
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("original explicit PR/head and unchanged trusted policy"),
            "{case}: {error}"
        );
    }
    assert!(!root.path().join("publications").exists());
}
