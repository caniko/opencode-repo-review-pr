use anyhow::bail;
use canix_toolbelt::review::Candidate;
use repo_review::{
    publication::{self, Kind},
    storage,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn candidate() -> Candidate {
    Candidate {
        url: "https://codefloe.com/caniko/fixture/pulls/1".into(),
        source_repository: "codefloe.com/caniko/fixture".into(),
        source_branch: "feature".into(),
        target_branch: "trunk".into(),
        head: "a".repeat(40),
        base: "b".repeat(40),
        draft: false,
        open: true,
    }
}

#[test]
fn committed_publication_lost_reply_is_not_replayed_or_reidentified() {
    for (kind, suffix) in [
        (Kind::ForgejoComment, "forgejo-comment"),
        (Kind::GitHubCheck, "github-check"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let request = "c".repeat(64);
        let path = root
            .path()
            .join("publications")
            .join(format!("{request}-{suffix}.json"));
        let effects = AtomicUsize::new(0);
        let selected = candidate();
        let payload = json!({"synthetic_body":"original"});
        assert!(
            publication::once(root.path(), &request, kind, &selected, &payload, || {
                effects.fetch_add(1, Ordering::SeqCst);
                bail!("synthetic reply lost after commit")
            })
            .is_err()
        );
        let before = std::fs::read(&path).expect("ambiguous effect must have durable identity");
        let saved: Value = storage::read(&path).unwrap().unwrap();
        assert_eq!(saved["state"], "dispatching");
        assert_eq!(saved["candidate"]["head"], selected.head);
        assert_eq!(
            saved["payload_digest"],
            storage::digest(&serde_json::to_vec(&payload).unwrap())
        );
        for changed_payload in [payload, json!({"synthetic_body":"different"})] {
            assert!(publication::once(
                root.path(),
                &request,
                kind,
                &selected,
                &changed_payload,
                || {
                    effects.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"id":2}))
                }
            )
            .is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        assert_eq!(effects.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn publication_is_durable_before_effect_and_success_remains_single_attempt() {
    let root = tempfile::tempdir().unwrap();
    let request = "d".repeat(64);
    let path = root
        .path()
        .join("publications")
        .join(format!("{request}-github-check.json"));
    let result = publication::once(
        root.path(),
        &request,
        Kind::GitHubCheck,
        &candidate(),
        &json!({"name":"review-policy"}),
        || {
            let saved: Value = storage::read(&path)?.expect("write-ahead receipt");
            assert_eq!(saved["state"], "dispatching");
            Ok(json!({"check_id":7,"app_id":321}))
        },
    )
    .unwrap();
    let saved: Value = storage::read(&path).unwrap().unwrap();
    assert_eq!(saved["state"], "confirmed");
    assert_eq!(saved["result"], result);
    assert!(publication::once(
        root.path(),
        &request,
        Kind::GitHubCheck,
        &candidate(),
        &json!({}),
        || panic!("confirmed publication replayed")
    )
    .is_err());
}

#[test]
fn concurrent_publication_contenders_have_one_effect() {
    let root = tempfile::tempdir().unwrap();
    let effects = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let path = root.path().to_owned();
            let effects = effects.clone();
            std::thread::spawn(move || {
                publication::once(
                    &path,
                    &"e".repeat(64),
                    Kind::ForgejoComment,
                    &candidate(),
                    &json!({}),
                    || {
                        effects.fetch_add(1, Ordering::SeqCst);
                        Ok(json!({"comment_id":1}))
                    },
                )
                .is_ok()
            })
        })
        .collect();
    let succeeded = threads
        .into_iter()
        .map(|thread| usize::from(thread.join().unwrap()))
        .sum::<usize>();
    assert_eq!(succeeded, 1);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
}
