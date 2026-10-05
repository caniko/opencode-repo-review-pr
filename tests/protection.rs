use repo_review::github_api::validate_merge_protection;
use serde_json::json;

#[test]
fn merge_requires_the_exact_dedicated_app_and_strict_admin_protection() {
    let mut protection = json!({"enforce_admins":{"enabled":true},"required_status_checks":{"strict":true,"checks":[{"context":"review-policy","app_id":123}]}});
    assert!(validate_merge_protection(&protection, 123).is_ok());
    assert!(validate_merge_protection(&protection, 124).is_err());
    assert!(validate_merge_protection(&protection, 15368).is_err());
    protection["required_status_checks"]["strict"] = json!(false);
    assert!(validate_merge_protection(&protection, 123).is_err());
    protection["required_status_checks"]["strict"] = json!(true);
    protection["enforce_admins"]["enabled"] = json!(false);
    assert!(validate_merge_protection(&protection, 123).is_err());
}
