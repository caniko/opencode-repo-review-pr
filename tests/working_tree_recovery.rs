use canix_toolbelt::review::{Candidate, Intent, Policy, Provider};
use repo_review::{config::Roborev as Settings, roborev::Roborev, router::Scope, storage};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

struct Fixture {
    temporary: tempfile::TempDir,
    settings: Settings,
    intent: Intent,
    policy: Policy,
    job: Arc<Mutex<Option<Value>>>,
    detail: Arc<Mutex<Option<Value>>>,
    enqueues: Arc<Mutex<usize>>,
    stop: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

fn request(stream: &mut UnixStream) -> (String, Value) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let split = loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break split;
        }
    };
    let headers = std::str::from_utf8(&bytes[..split]).unwrap().to_owned();
    let length: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap()
        .parse()
        .unwrap();
    while bytes.len() < split + 4 + length {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    let body = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[split + 4..]).unwrap()
    };
    (headers.lines().next().unwrap().to_owned(), body)
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let policy = Policy {
            provider: "roborev".into(),
            transport: "unix-http-v0.71".into(),
            ..Policy::default()
        };
        let intent = Intent {
            schema_version: 1,
            id: "working-tree-recovery".into(),
            candidate: Candidate {
                url: "https://local.invalid/review/fixture".into(),
                source_repository: "local.invalid/fixture".into(),
                source_branch: "feature".into(),
                target_branch: "trunk".into(),
                head: "a".repeat(40),
                base: "a".repeat(40),
                draft: false,
                open: true,
            },
            policy_digest: policy.digest().unwrap(),
            baseline_review_ids: vec![],
        };
        let job = Arc::new(Mutex::new(None::<Value>));
        let detail = Arc::new(Mutex::new(None::<Value>));
        let enqueues = Arc::new(Mutex::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared_job, shared_detail, shared_enqueues, stopping) =
            (job.clone(), detail.clone(), enqueues.clone(), stop.clone());
        let server = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                let (line, input) = request(&mut stream);
                let response = if line == "POST /api/enqueue HTTP/1.1" {
                    *shared_enqueues.lock().unwrap() += 1;
                    *shared_job.lock().unwrap() = Some(json!({
                        "id":7,"uuid":"persisted-working-tree-job","repo_path":input["repo_path"],
                        "git_ref":input["git_ref"],"agent":input["agent"],"agentic":false,
                        "min_severity":"low","status":"queued","diff_content":input["diff_content"]
                    }));
                    // Persist the mutation and lose its HTTP reply.
                    continue;
                } else if line == "GET /api/jobs?id=7 HTTP/1.1" {
                    shared_detail.lock().unwrap().clone().unwrap_or_else(||
                        json!({"jobs":[shared_job.lock().unwrap().clone().unwrap()],"has_more":false}))
                } else if line == "GET /api/review?job_id=7 HTTP/1.1" {
                    json!({"id":9,"job_id":7,"job":shared_job.lock().unwrap().clone().unwrap(),
                        "structured_output":{"schema_version":2,"summary":"No findings","verdict":"pass","findings":[]},
                        "file_coverage":{"reviewed":1,"excluded":0}})
                } else {
                    assert!(line.starts_with("GET /api/jobs?repo="), "{line}");
                    let jobs: Vec<_> = shared_job
                        .lock()
                        .unwrap()
                        .clone()
                        .into_iter()
                        .map(|mut job| {
                            job.as_object_mut().unwrap().remove("diff_content");
                            job
                        })
                        .collect();
                    json!({"jobs":jobs,"has_more":false})
                };
                let body = serde_json::to_vec(&response).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Self {
            temporary,
            settings: Settings {
                socket: Some(socket),
                qualified: true,
                qualification_receipt: "offline synthetic worker only".into(),
                ..Settings::default()
            },
            intent,
            policy,
            job,
            detail,
            enqueues,
            stop,
            server: Some(server),
        }
    }

    fn provider(&self) -> Roborev {
        Roborev::new(
            &self.settings,
            self.temporary.path().join("snapshot"),
            "dirty".into(),
            Some("EXACT_CAPTURED_TRACKED_DIFF\n".into()),
            1,
            self.temporary.path().join("state"),
            Scope::WorkingTree,
        )
        .unwrap()
    }

    fn ledger(&self) -> PathBuf {
        self.temporary.path().join("state").join(format!(
            "{}.json",
            storage::digest(self.intent.id.as_bytes())
        ))
    }

    fn submit_unknown(&self) {
        assert!(self.provider().submit(&self.intent).is_err());
        assert_eq!(*self.enqueues.lock().unwrap(), 1);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.server.take().unwrap().join().unwrap();
    }
}

#[test]
fn working_tree_unknown_enqueue_hydrates_exact_diff_across_restarts() {
    let fixture = Fixture::new();
    fixture.submit_unknown();
    for status in ["queued", "running", "done"] {
        fixture.job.lock().unwrap().as_mut().unwrap()["status"] = json!(status);
        let evidence = fixture
            .provider()
            .inspect(
                &fixture.intent.candidate,
                &fixture.policy,
                Some(&fixture.intent),
            )
            .unwrap();
        assert_eq!(
            evidence.request_receipt.as_deref(),
            Some("roborev:job:7:persisted-working-tree-job")
        );
        assert_eq!(evidence.review.is_some(), status == "done");
        if let Some(review) = evidence.review {
            assert_eq!(review.id, "9");
        }
        assert!(fixture.provider().submit(&fixture.intent).is_err());
    }
    assert_eq!(*fixture.enqueues.lock().unwrap(), 1);
}

#[test]
fn working_tree_untrusted_detail_cannot_change_unknown_ledger_or_reenqueue() {
    for case in [
        "missing",
        "duplicate",
        "id",
        "uuid",
        "diff",
        "absent-diff",
        "range",
        "partial",
        "unknown-completeness",
    ] {
        let fixture = Fixture::new();
        fixture.submit_unknown();
        let before = std::fs::read(fixture.ledger()).unwrap();
        let mut job = fixture.job.lock().unwrap().clone().unwrap();
        match case {
            "id" => job["id"] = json!(8),
            "uuid" => job["uuid"] = json!("different-job"),
            "diff" => job["diff_content"] = json!("DIFFERENT_DIFF"),
            "absent-diff" => {
                job.as_object_mut().unwrap().remove("diff_content");
            }
            "range" => job["git_ref"] = json!("different-range"),
            _ => {}
        }
        let jobs = match case {
            "missing" => vec![],
            "duplicate" => vec![job.clone(), job],
            _ => vec![job],
        };
        let mut detail = json!({"jobs":jobs,"has_more":case == "partial"});
        if case == "unknown-completeness" {
            detail.as_object_mut().unwrap().remove("has_more");
        }
        *fixture.detail.lock().unwrap() = Some(detail);
        let error = fixture
            .provider()
            .inspect(
                &fixture.intent.candidate,
                &fixture.policy,
                Some(&fixture.intent),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("working-tree job"),
            "{case}: {error}"
        );
        assert_eq!(std::fs::read(fixture.ledger()).unwrap(), before, "{case}");
        assert!(fixture.provider().submit(&fixture.intent).is_err());
        assert_eq!(*fixture.enqueues.lock().unwrap(), 1, "{case}");
    }
}
