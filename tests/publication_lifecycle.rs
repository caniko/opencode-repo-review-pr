use anyhow::bail;
use canix_toolbelt::review::Candidate;
use repo_review::{
    publication::{self, Binding, Kind, Published, Selection},
    storage,
};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

fn candidate() -> Candidate {
    Candidate {
        url: "https://github.com/caniko/fixture/pull/1".into(),
        source_repository: "github.com/caniko/fixture".into(),
        source_branch: "feature".into(),
        target_branch: "trunk".into(),
        head: "a".repeat(40),
        base: "b".repeat(40),
        draft: false,
        open: true,
    }
}
fn selection<'a>(root: &'a Path, candidate: &'a Candidate, request: &'a str) -> Selection<'a> {
    Selection {
        root,
        request,
        kind: Kind::GitHubCheck,
        candidate,
        authority: "github-app:321",
        publication_id: 7,
    }
}
fn published(payload: Value) -> Published {
    Published {
        id: 7,
        authority: "github-app:321".into(),
        payload,
        url: Some("https://github.com/caniko/fixture/runs/7".into()),
    }
}
fn initial(root: &Path, request: &str, candidate: &Candidate, payload: &Value, unknown: bool) {
    let result = publication::once_bound(
        root,
        request,
        Kind::GitHubCheck,
        candidate,
        payload,
        Binding {
            authority: "github-app:321",
            publication_id: None,
        },
        || {
            if unknown {
                bail!("reply lost after committed create");
            }
            Ok(published(payload.clone()))
        },
    );
    assert_eq!(result.is_err(), unknown);
}

#[test]
fn lost_create_reply_adopts_exact_object_without_rewriting_initial_attempt() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate();
    let request = "c".repeat(64);
    let payload = json!({"conclusion":"failure"});
    initial(root.path(), &request, &candidate, &payload, true);
    let path = root
        .path()
        .join(format!("publications/{request}-github-check.json"));
    let before = std::fs::read(&path).unwrap();
    let receipt = publication::reconcile(selection(root.path(), &candidate, &request), || {
        Ok(published(payload.clone()))
    })
    .unwrap();
    assert_eq!(receipt["publication_id"], 7);
    assert_eq!(receipt["update_pending"], false);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(publication::once(
        root.path(),
        &request,
        Kind::GitHubCheck,
        &candidate,
        &payload,
        || panic!("create replayed")
    )
    .is_err());
}

#[test]
fn finding_disposition_update_is_write_ahead_and_lost_patch_reply_is_adopted() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate();
    let request = "d".repeat(64);
    let old = json!({"conclusion":"failure"});
    let new = json!({"conclusion":"success"});
    initial(root.path(), &request, &candidate, &old, false);
    let original = root
        .path()
        .join(format!("publications/{request}-github-check.json"));
    let before = std::fs::read(&original).unwrap();
    publication::reconcile(selection(root.path(), &candidate, &request), || {
        Ok(published(old.clone()))
    })
    .unwrap();
    let expected = storage::digest(&serde_json::to_vec(&old).unwrap());
    let effects = AtomicUsize::new(0);
    let preview = publication::update(
        selection(root.path(), &candidate, &request),
        &expected,
        &new,
        false,
        || Ok(published(old.clone())),
        |_| panic!("preview patched forge"),
    )
    .unwrap();
    assert_eq!(preview["would_update"], true);
    let result = publication::update(
        selection(root.path(), &candidate, &request),
        &expected,
        &new,
        true,
        || Ok(published(old.clone())),
        |id| {
            assert_eq!(id, 7);
            let journal: Value = storage::read(&root.path().join(format!(
                "publications/{request}-github-check-lifecycle.json"
            )))
            .unwrap()
            .unwrap();
            assert_eq!(journal["pending"]["payload"], new);
            effects.fetch_add(1, Ordering::SeqCst);
            bail!("reply lost after committed PATCH")
        },
    );
    assert!(result.is_err());
    for payload in [&new, &json!({"conclusion":"different"})] {
        assert!(publication::update(
            selection(root.path(), &candidate, &request),
            &expected,
            payload,
            true,
            || Ok(published(new.clone())),
            |_| panic!("ambiguous update replayed")
        )
        .is_err());
    }
    assert!(
        publication::reconcile(selection(root.path(), &candidate, &request), || Ok(
            published(old.clone())
        ))
        .is_err()
    );
    let recovered = publication::reconcile(selection(root.path(), &candidate, &request), || {
        Ok(published(new.clone()))
    })
    .unwrap();
    assert_eq!(recovered["updates"], 1);
    assert_eq!(recovered["update_pending"], false);
    let noop = publication::update(
        selection(root.path(), &candidate, &request),
        recovered["payload_digest"].as_str().unwrap(),
        &new,
        true,
        || Ok(published(new.clone())),
        |_| panic!("confirmed update replayed"),
    )
    .unwrap();
    assert_eq!(noop["would_update"], false);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(&original).unwrap(), before);
}

#[test]
fn changed_authority_comparison_object_or_payload_cannot_reconcile() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate();
    let request = "e".repeat(64);
    let payload = json!({"conclusion":"failure"});
    initial(root.path(), &request, &candidate, &payload, true);
    for case in ["authority", "id", "payload", "head", "base"] {
        let mut observed = published(payload.clone());
        let mut selected_candidate = candidate.clone();
        match case {
            "authority" => observed.authority = "github-app:999".into(),
            "id" => observed.id = 8,
            "payload" => observed.payload = json!({"conclusion":"success"}),
            "head" => selected_candidate.head = "f".repeat(40),
            "base" => selected_candidate.base = "f".repeat(40),
            _ => unreachable!(),
        }
        assert!(
            publication::reconcile(
                selection(root.path(), &selected_candidate, &request),
                || Ok(observed)
            )
            .is_err(),
            "{case}"
        );
    }
    assert!(!root
        .path()
        .join(format!(
            "publications/{request}-github-check-lifecycle.json"
        ))
        .exists());
}

#[test]
fn concurrent_updates_require_the_observed_previous_digest_and_have_one_effect() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate();
    let request = "f".repeat(64);
    let old = json!({"conclusion":"failure"});
    let new = json!({"conclusion":"success"});
    initial(root.path(), &request, &candidate, &old, false);
    publication::reconcile(selection(root.path(), &candidate, &request), || {
        Ok(published(old.clone()))
    })
    .unwrap();
    let expected = storage::digest(&serde_json::to_vec(&old).unwrap());
    let remote = Arc::new(Mutex::new(published(old)));
    let effects = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let path = root.path().to_owned();
            let candidate = candidate.clone();
            let request = request.clone();
            let expected = expected.clone();
            let new = new.clone();
            let remote = remote.clone();
            let effects = effects.clone();
            std::thread::spawn(move || {
                publication::update(
                    selection(&path, &candidate, &request),
                    &expected,
                    &new,
                    true,
                    || Ok(remote.lock().unwrap().clone()),
                    |_| {
                        effects.fetch_add(1, Ordering::SeqCst);
                        let result = published(new.clone());
                        *remote.lock().unwrap() = result.clone();
                        Ok(result)
                    },
                )
                .is_ok()
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|ok| *ok)
            .count(),
        1
    );
    assert_eq!(effects.load(Ordering::SeqCst), 1);
}

#[test]
fn legacy_attempt_without_pre_effect_publisher_binding_remains_blocked() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate();
    let request = "1".repeat(64);
    let payload = json!({"conclusion":"failure"});
    publication::once(
        root.path(),
        &request,
        Kind::GitHubCheck,
        &candidate,
        &payload,
        || Ok(json!({"check_id":7})),
    )
    .unwrap();
    assert!(
        publication::reconcile(selection(root.path(), &candidate, &request), || Ok(
            published(payload)
        ))
        .is_err()
    );
}

#[test]
fn missing_initial_attempt_with_existing_lock_anchor_cannot_create_again() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate();
    let request = "2".repeat(64);
    let payload = json!({"conclusion":"failure"});
    initial(root.path(), &request, &candidate, &payload, true);
    let path = root
        .path()
        .join(format!("publications/{request}-github-check.json"));
    // Deliberately remove only this disposable fixture's receipt to exercise loss.
    std::fs::remove_file(path).unwrap();
    assert!(publication::once_bound(
        root.path(),
        &request,
        Kind::GitHubCheck,
        &candidate,
        &payload,
        Binding {
            authority: "github-app:321",
            publication_id: None
        },
        || panic!("missing ledger reopened create")
    )
    .is_err());
}

#[test]
fn oversized_publication_payload_is_rejected_before_state_or_effects() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate();
    let request = "3".repeat(64);
    let payload = json!({"body":"x".repeat(1024 * 1024)});
    assert!(publication::once_bound(
        root.path(),
        &request,
        Kind::GitHubCheck,
        &candidate,
        &payload,
        Binding {
            authority: "github-app:321",
            publication_id: None
        },
        || panic!("oversized payload dispatched")
    )
    .is_err());
    assert!(!root.path().join("publications").exists());
}

#[test]
fn lost_update_journal_never_reopens_writes_even_if_forge_returns_original_payload() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate();
    let request = "4".repeat(64);
    let old = json!({"conclusion":"failure"});
    let new = json!({"conclusion":"success"});
    initial(root.path(), &request, &candidate, &old, false);
    let original = root
        .path()
        .join(format!("publications/{request}-github-check.json"));
    let before = std::fs::read(&original).unwrap();
    let adopted = publication::reconcile(selection(root.path(), &candidate, &request), || {
        Ok(published(old.clone()))
    })
    .unwrap();
    let expected = adopted["payload_digest"].as_str().unwrap();
    assert!(publication::update(
        selection(root.path(), &candidate, &request),
        expected,
        &new,
        true,
        || Ok(published(old.clone())),
        |_| bail!("unknown PATCH outcome")
    )
    .is_err());
    // Only this disposable fixture's journal is removed; original/anchors survive.
    std::fs::remove_file(root.path().join(format!(
        "publications/{request}-github-check-lifecycle.json"
    )))
    .unwrap();
    for _ in 0..2 {
        assert!(
            publication::reconcile(selection(root.path(), &candidate, &request), || Ok(
                published(old.clone())
            ))
            .is_err()
        );
        assert!(publication::update(
            selection(root.path(), &candidate, &request),
            expected,
            &new,
            true,
            || Ok(published(old.clone())),
            |_| panic!("lost journal reopened PATCH")
        )
        .is_err());
    }
    assert_eq!(std::fs::read(original).unwrap(), before);
    assert!(publication::once_bound(
        root.path(),
        &request,
        Kind::GitHubCheck,
        &candidate,
        &old,
        Binding {
            authority: "github-app:321",
            publication_id: None
        },
        || panic!("lost journal reopened POST")
    )
    .is_err());
}

#[test]
fn intact_older_journal_is_protected_before_its_first_new_patch() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate();
    let request = "5".repeat(64);
    let old = json!({"conclusion":"failure"});
    let new = json!({"conclusion":"success"});
    initial(root.path(), &request, &candidate, &old, false);
    let adopted = publication::reconcile(selection(root.path(), &candidate, &request), || {
        Ok(published(old.clone()))
    })
    .unwrap();
    let tombstone = root.path().join(format!(
        "publications/{request}-github-check-lifecycle.tombstone.json"
    ));
    // Simulate an intact pre-tombstone journal only in this disposable fixture.
    std::fs::remove_file(&tombstone).unwrap();
    publication::update(
        selection(root.path(), &candidate, &request),
        adopted["payload_digest"].as_str().unwrap(),
        &new,
        true,
        || Ok(published(old.clone())),
        |_| {
            let marker: Value = storage::read(&tombstone)?.unwrap();
            assert_eq!(marker["publication_id"], 7);
            assert_eq!(marker["authority"], "github-app:321");
            Ok(published(new.clone()))
        },
    )
    .unwrap();
}

#[test]
fn changed_tombstone_or_journal_loss_after_adoption_blocks_before_forge_observation() {
    let root = tempfile::tempdir().unwrap();
    let candidate = candidate();
    let request = "6".repeat(64);
    let payload = json!({"conclusion":"failure"});
    initial(root.path(), &request, &candidate, &payload, false);
    publication::reconcile(selection(root.path(), &candidate, &request), || {
        Ok(published(payload.clone()))
    })
    .unwrap();
    let journal = root.path().join(format!(
        "publications/{request}-github-check-lifecycle.json"
    ));
    let tombstone = journal.with_extension("tombstone.json");
    let marker: Value = storage::read(&tombstone).unwrap().unwrap();
    for field in ["authority", "origin_digest", "publication_id"] {
        let mut changed = marker.clone();
        changed[field] = if field == "publication_id" {
            json!(8)
        } else {
            json!("different")
        };
        storage::save(&tombstone, &changed).unwrap();
        assert!(
            publication::reconcile(selection(root.path(), &candidate, &request), || panic!(
                "changed tombstone reached forge"
            ))
            .is_err()
        );
    }
    storage::save(&tombstone, &marker).unwrap();
    std::fs::remove_file(journal).unwrap();
    assert!(
        publication::reconcile(selection(root.path(), &candidate, &request), || panic!(
            "lost adoption journal reached forge"
        ))
        .is_err()
    );
}
