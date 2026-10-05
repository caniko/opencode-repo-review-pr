use repo_review::{
    app::{Application, Target},
    config::Config,
};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    os::unix::net::UnixListener,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
};

#[test]
fn lost_enqueue_reply_is_adopted_once_against_the_exact_snapshot() {
    let temporary = tempfile::tempdir().unwrap();
    let repo = temporary.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet", "--initial-branch=trunk"]);
    git(&["config", "user.name", "Disposable Fixture"]);
    git(&["config", "user.email", "fixture@example.invalid"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["config", "core.hooksPath", "/dev/null"]);
    std::fs::write(repo.join("file.txt"), "base\n").unwrap();
    git(&["add", "file.txt"]);
    git(&["commit", "--quiet", "-m", "base"]);
    git(&["checkout", "--quiet", "-b", "feature"]);
    std::fs::write(repo.join("file.txt"), "changed\n").unwrap();
    git(&["add", "file.txt"]);
    git(&["commit", "--quiet", "-m", "changed"]);
    let socket = temporary.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let jobs = Arc::new(Mutex::new(Vec::<Value>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let shared = jobs.clone();
    let stopping = stop.clone();
    let server = thread::spawn(move || {
        while !stopping.load(Ordering::Relaxed) {
            let (mut stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            let mut bytes = vec![];
            let mut buffer = [0; 4096];
            let split = loop {
                let n = stream.read(&mut buffer).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i;
                }
            };
            let headers = String::from_utf8(bytes[..split].to_vec()).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .unwrap()
                .parse()
                .unwrap();
            while bytes.len() < split + 4 + length {
                let n = stream.read(&mut buffer).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
            }
            let value = if headers.starts_with("POST /api/enqueue ") {
                let input: Value = serde_json::from_slice(&bytes[split + 4..]).unwrap();
                assert_eq!(input["min_severity"], "low");
                let job = json!({"id":1,"uuid":"unique-job-uuid","repo_path":input["repo_path"],"git_ref":input["git_ref"],"agent":input["agent"],"agentic":false,"min_severity":input["min_severity"],"status":"done"});
                shared.lock().unwrap().push(job);
                // Provider committed the job, but the caller never receives a receipt.
                continue;
            } else if headers.starts_with("GET /api/review?") {
                let job = shared.lock().unwrap()[0].clone();
                json!({"id":2,"job_id":1,"job":job,"structured_output":{"schema_version":2,"summary":"No findings","verdict":"pass","findings":[]},"file_coverage":{"reviewed":1,"excluded":0}})
            } else {
                json!({"jobs":*shared.lock().unwrap(),"has_more":false})
            };
            let body = serde_json::to_vec(&value).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    let mut config = Config::default();
    config.roborev.socket = Some(socket);
    config.roborev.qualified = true;
    config.roborev.qualification_receipt = "isolated offline mock worker".into();
    let app = Application {
        config,
        state: temporary.path().join("state"),
    };
    let target = Target {
        repo: repo.clone(),
        base: Some("trunk".into()),
        ..Target::default()
    };
    let first = app.tick(&target, false, true).unwrap();
    assert_eq!(first.outcome.verdict.exit_code(), 22);
    let second = app.tick(&target, false, true).unwrap();
    assert_eq!(second.outcome.verdict.exit_code(), 0);
    assert_eq!(first.route.id, second.route.id);
    assert_eq!(jobs.lock().unwrap().len(), 1);
    assert!(
        !second.merge_blockers.is_empty(),
        "local clean review must remain advisory"
    );
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
}
