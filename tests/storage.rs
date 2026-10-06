use repo_review::storage;
use serde_json::{json, Value};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

#[test]
fn state_cannot_follow_links_or_trust_group_writable_files() {
    let root = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let path = root.path().join("state.json");
    let foreign = root.path().join("foreign.json");
    std::fs::write(&foreign, "{}\n").unwrap();
    symlink(&foreign, &path).unwrap();
    assert!(storage::read::<Value>(&path).is_err());
    assert!(storage::lock(&path).is_err());
    std::fs::remove_file(&path).unwrap();
    storage::save(&path, &json!({"ok":true})).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).unwrap();
    assert!(storage::read::<Value>(&path).is_err());
    let link = root.path().join("linked-dir");
    symlink(root.path(), &link).unwrap();
    assert!(storage::save(&link.join("nested/new.json"), &json!({})).is_err());
    assert!(storage::read::<Value>(&link.join("foreign.json")).is_err());
}

#[test]
fn retained_engine_lock_validation_preserves_freshness_and_existing_history() {
    let root = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let path = root.path().join("engine.lock");
    storage::validate_existing_lock(&path).unwrap();
    assert!(
        !path.exists(),
        "only the engine may create a fresh history anchor"
    );
    std::fs::write(&path, "canix-review-history-v2\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let original = std::fs::metadata(&path).unwrap();
    storage::validate_existing_lock(&path).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), original.ino());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "canix-review-history-v2\n"
    );
    let hardlink = root.path().join("shared.lock");
    std::fs::hard_link(&path, &hardlink).unwrap();
    assert!(storage::validate_existing_lock(&path).is_err());
    std::fs::remove_file(hardlink).unwrap();
    let link = root.path().join("alias.lock");
    symlink(&path, &link).unwrap();
    assert!(storage::validate_existing_lock(&link).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).unwrap();
    assert!(storage::validate_existing_lock(&path).is_err());
}
