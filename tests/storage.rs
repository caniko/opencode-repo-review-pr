use repo_review::storage;
use serde_json::{json, Value};
use std::os::unix::fs::{symlink, PermissionsExt};

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
