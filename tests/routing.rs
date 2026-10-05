use repo_review::identity::{ForgeKind, Repository};
use std::collections::BTreeMap;

#[test]
fn detects_real_forges_across_git_transports() {
    for remote in [
        "git@github.com:caniko/demo.git",
        "ssh://git@github.com/caniko/demo.git",
        "https://github.com/caniko/demo.git",
    ] {
        let repo = Repository::from_remote(remote, &BTreeMap::new()).unwrap();
        assert_eq!(repo.coordinate(), "github.com/caniko/demo");
        assert_eq!(repo.forge, ForgeKind::GitHub);
    }
    for host in ["codefloe.com", "codeberg.org"] {
        let repo =
            Repository::from_remote(&format!("git@{host}:caniko/demo.git"), &BTreeMap::new())
                .unwrap();
        assert_eq!(repo.forge, ForgeKind::Forgejo);
    }
}

#[test]
fn rejects_credential_urls_and_github_lookalikes() {
    for remote in [
        "https://token@github.com/caniko/demo",
        "https://github.com.evil.test/caniko/demo",
        "https://github.com/caniko/../demo",
        "file:///private/repo",
        "ext::sh -c evil",
    ] {
        assert!(
            Repository::from_remote(remote, &BTreeMap::new()).is_err(),
            "{remote}"
        );
    }
}

#[test]
fn explicit_forgejo_mapping_and_pr_paths() {
    let hosts = BTreeMap::from([("forge.example.org".into(), ForgeKind::Forgejo)]);
    let (repo, number) =
        Repository::from_pr("https://forge.example.org/caniko/demo/pulls/42", &hosts).unwrap();
    assert_eq!(number, 42);
    assert_eq!(repo.forge, ForgeKind::Forgejo);
    assert!(Repository::from_pr("https://github.com/caniko/demo/pulls/42", &hosts).is_err());
    assert!(Repository::from_pr("https://github.com/caniko/demo/pull/0", &hosts).is_err());
}
