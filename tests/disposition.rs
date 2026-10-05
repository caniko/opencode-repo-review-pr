use canix_toolbelt::review::{
    Candidate, Capabilities, Error, Evidence, Finding, Intent, Policy, Provider, Review,
};
use repo_review::{
    disposition::{self, Disposed, Disposition},
    storage,
};

struct Existing(Review);
impl Provider for Existing {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            provider: "roborev".into(),
            transport: "unix-http-v0.71".into(),
            drafts: true,
            idempotent_submission: false,
            revision_attestation: true,
        }
    }
    fn inspect(
        &mut self,
        _: &Candidate,
        _: &Policy,
        _: Option<&Intent>,
    ) -> Result<Evidence, Error> {
        Ok(Evidence {
            known_review_ids: vec![],
            recovered_intent: None,
            request_receipt: None,
            review: Some(self.0.clone()),
        })
    }
    fn submit(&mut self, _: &Intent) -> Result<String, Error> {
        panic!("disposition inspection cannot dispatch")
    }
}

#[test]
fn body_revision_and_policy_changes_invalidate_dispositions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let path = dir.path().join("dispositions.json");
    let candidate = Candidate {
        url: "https://local.invalid/review/snapshot".into(),
        source_repository: "local".into(),
        source_branch: "feature".into(),
        target_branch: "trunk".into(),
        head: "a".repeat(40),
        base: "b".repeat(40),
        draft: false,
        open: true,
    };
    let policy = Policy::default();
    let finding = Finding {
        id: "one".into(),
        body: "check range".into(),
        path: None,
        line: None,
        severity: None,
        provider_addressed: false,
        correlated: true,
        url: "fixture".into(),
    };
    let review = Review {
        id: "review".into(),
        provider: "greptile".into(),
        reviewed_head: Some(candidate.head.clone()),
        requested_base: Some(candidate.base.clone()),
        policy_digest: policy.digest().unwrap(),
        completed: true,
        complete_findings: true,
        findings: vec![finding.clone()],
        url: "fixture".into(),
    };
    disposition::record(
        &path,
        Disposition {
            candidate: candidate.clone(),
            policy_digest: policy.digest().unwrap(),
            review_id: review.id.clone(),
            finding_id: finding.id,
            body_digest: storage::digest(finding.body.as_bytes()),
            reason: "source verifies the invariant".into(),
            evidence: "file.rs:12 + regression test".into(),
        },
    )
    .unwrap();
    let mut provider = Existing(review);
    let mut disposed = Disposed {
        inner: &mut provider,
        path: &path,
    };
    assert!(disposed
        .inspect(&candidate, &policy, None)
        .unwrap()
        .review
        .unwrap()
        .findings
        .is_empty());
    let mut changed = candidate.clone();
    changed.head = "c".repeat(40);
    assert_eq!(
        disposed
            .inspect(&changed, &policy, None)
            .unwrap()
            .review
            .unwrap()
            .findings
            .len(),
        1
    );
    provider.0.findings[0].body = "different body".into();
    assert_eq!(
        Disposed {
            inner: &mut provider,
            path: &path
        }
        .inspect(&candidate, &policy, None)
        .unwrap()
        .review
        .unwrap()
        .findings
        .len(),
        1
    );
}
