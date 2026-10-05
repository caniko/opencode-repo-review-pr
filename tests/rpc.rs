use serde_json::{json, Value};
use std::{
    io::Write,
    process::{Command, Stdio},
};

fn rpc(root: &std::path::Path, input: &[u8]) -> (i32, Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_repo-review"))
        .env_clear()
        .env("HOME", root)
        .env("PATH", std::env::var_os("PATH").unwrap())
        .arg("--config")
        .arg(root.join("absent.json"))
        .arg("--state-dir")
        .arg(root.join("state"))
        .arg("rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.status.code().unwrap(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

#[test]
fn server_owned_rpc_rejects_version_policy_injection_and_local_execution() {
    let root = tempfile::tempdir().unwrap();
    let (exit, balance) = rpc(
        root.path(),
        &serde_json::to_vec(&json!({"version":1,"action":"budget","account":"synthetic"})).unwrap(),
    );
    assert_eq!(exit, 0);
    assert_eq!(balance["confirmed"], false);
    assert!(
        !root.path().join("state").exists(),
        "read-only RPC must not create an allowance"
    );
    for input in [
        json!({"version":2,"action":"budget","account":"synthetic"}),
        json!({"version":1,"action":"budget","account":"synthetic","config":{"greptile":{"enabled":true}}}),
        json!({"version":1,"action":"pr","target":{"repo":".","working_tree":true,"remote_only":false}}),
    ] {
        let (exit, _) = rpc(root.path(), &serde_json::to_vec(&input).unwrap());
        assert_eq!(exit, 22);
    }
    assert_eq!(rpc(root.path(), &vec![b' '; 65537]).0, 22);
}

#[test]
fn unreachable_coordinator_cannot_create_a_local_credit_ledger() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let ssh = root.path().join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\nexit 255\n").unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_repo-review"))
        .env_clear()
        .env("HOME", root.path())
        .env("PATH", root.path())
        .arg("--state-dir")
        .arg(root.path().join("state"))
        .args(["--coordinator", "127.0.0.1", "budget", "show", "synthetic"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(22));
    assert!(!root.path().join("state").exists());
}
