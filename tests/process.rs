#![cfg(target_os = "linux")]
use repo_review::process;
use rustix::process::{Pid, WaitOptions};
use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, Instant},
};

#[test]
fn normal_exit_and_timeout_both_clean_up_owned_process_groups() {
    // Reap only this fixture's adopted descendants; never rely on the operator's
    // PID 1 to clear a deliberately orphaned mock child.
    rustix::process::set_child_subreaper(Some(Pid::INIT)).unwrap();
    let temporary = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let script = temporary.path().join("fixture.sh");
    std::fs::write(
        &script,
        "sleep 30 &\necho $! > child.pid\nif [ \"$1\" = wait ]; then wait; fi\n",
    )
    .unwrap();
    for mode in ["exit", "wait"] {
        let started = Instant::now();
        let result = process::run(
            Path::new("/bin/sh"),
            &[script.to_str().unwrap(), mode],
            temporary.path(),
            &temporary.path().join(mode),
            1,
        );
        if mode == "exit" {
            assert_eq!(result.unwrap().0, 0);
        } else {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("lifetime/output bound"));
        }
        assert!(started.elapsed() < Duration::from_secs(3));
        let child = std::fs::read_to_string(temporary.path().join("child.pid"))
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let pid = Pid::from_raw(child).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some((reaped, status)) =
                rustix::process::waitpid(Some(pid), WaitOptions::NOHANG).unwrap()
            {
                assert_eq!(reaped, pid);
                assert!(status.signaled());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "owned descendant survived cleanup"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    rustix::process::set_child_subreaper(None).unwrap();
}
