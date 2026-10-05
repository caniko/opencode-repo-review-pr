use repo_review::{
    budget::{Budget, Period},
    config::Config,
    identity::Repository,
    router::{choose, ProviderName, RoutingContext, Scope},
};
use std::collections::BTreeMap;

#[test]
fn forge_eligibility_precedes_free_credit_selection() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.greptile.enabled = true;
    config.greptile.maximum_cost = Some(1);
    config
        .greptile
        .billing_accounts
        .insert("caniko".into(), "org/caniko".into());
    Budget::new(dir.path(), "org/caniko")
        .reconcile(
            Period {
                start: 100,
                end: 200,
                allowance: 50,
                reconciled_usage: 0,
                evidence: "dashboard".into(),
                no_overage: true,
                included_requests: vec![],
            },
            101,
        )
        .unwrap();
    let github =
        Repository::from_remote("git@github.com:caniko/demo.git", &BTreeMap::new()).unwrap();
    let forgejo =
        Repository::from_remote("git@codefloe.com:caniko/canix.git", &BTreeMap::new()).unwrap();
    assert_eq!(
        choose(
            &config,
            RoutingContext {
                repository: Some(&github),
                author: Some("caniko"),
                scope: Scope::PullRequest,
                root: dir.path(),
                now: 101,
                request: None
            }
        )
        .unwrap()
        .0,
        ProviderName::Greptile
    );
    assert_eq!(
        choose(
            &config,
            RoutingContext {
                repository: Some(&forgejo),
                author: Some("caniko"),
                scope: Scope::PullRequest,
                root: dir.path(),
                now: 101,
                request: None
            }
        )
        .unwrap()
        .1,
        "forgejo-roborev"
    );
    assert_eq!(
        choose(
            &config,
            RoutingContext {
                repository: Some(&github),
                author: Some("another-author"),
                scope: Scope::PullRequest,
                root: dir.path(),
                now: 101,
                request: None
            }
        )
        .unwrap()
        .1,
        "billing-identity-ineligible"
    );
    assert_eq!(
        choose(
            &config,
            RoutingContext {
                repository: Some(&github),
                author: Some("caniko"),
                scope: Scope::WorkingTree,
                root: dir.path(),
                now: 101,
                request: None
            }
        )
        .unwrap()
        .1,
        "working-tree-requires-roborev"
    );
}
