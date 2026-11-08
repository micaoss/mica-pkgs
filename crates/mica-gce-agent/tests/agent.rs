//! The agent against a fake metadata server: ETags and long polls, the
//! Metadata-Flavor header both ways, failures, and key files that follow
//! changes and expiries.
// Tests and fixtures answer a broken expectation by panicking.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mica_gce_agent::accounts::{Account, Accounts};
use mica_gce_agent::apply::Paths;
use mica_gce_agent::metadata::Client;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot};

const ED: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOtest";
const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQtest";

/// What the fake server answers.
struct State {
    document: Value,
    etag: u64,
    /// Answers to give before the document, e.g. 500s.
    failures: Vec<&'static str>,
    /// Send no Metadata-Flavor header back.
    unflavored: bool,
    /// Requests seen, as their request lines.
    requests: Vec<String>,
}

#[derive(Clone)]
struct Fake {
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
}

impl Fake {
    async fn start(document: Value) -> (Self, std::net::SocketAddr) {
        let fake = Fake {
            state: Arc::new(Mutex::new(State {
                document,
                etag: 1,
                failures: vec![],
                unflavored: false,
                requests: vec![],
            })),
            changed: Arc::new(Notify::new()),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = fake.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(server.clone().serve(stream));
            }
        });
        (fake, address)
    }

    fn set(&self, document: Value) {
        let mut state = self.state.lock().unwrap();
        state.document = document;
        state.etag += 1;
        drop(state);
        self.changed.notify_waiters();
    }

    async fn serve(self, mut stream: TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        let line = head.lines().next().unwrap_or_default().to_string();
        let flavored = head
            .lines()
            .any(|l| l.eq_ignore_ascii_case("metadata-flavor: google"));
        let last = line.split("last_etag=").nth(1).map(|rest| {
            rest.split(['&', ' '])
                .next()
                .unwrap_or_default()
                .to_string()
        });
        let failure = {
            let mut state = self.state.lock().unwrap();
            state.requests.push(line.clone());
            (!state.failures.is_empty()).then(|| state.failures.remove(0))
        };
        if let Some(status) = failure {
            let reply = format!(
                "HTTP/1.1 {status}\r\nMetadata-Flavor: Google\r\nContent-Length: 4\r\nConnection: close\r\n\r\noops"
            );
            let _ = stream.write_all(reply.as_bytes()).await;
            return;
        }
        if !flavored {
            let reply = "HTTP/1.1 403 Forbidden\r\nMetadata-Flavor: Google\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(reply.as_bytes()).await;
            return;
        }
        // A long poll waits for a change, or answers the same document after
        // the poll time (short here: the client asks for 1 s).
        if let Some(last) = last {
            let changed = self.changed.notified();
            let current = self.state.lock().unwrap().etag.to_string();
            if current == last {
                let _ = tokio::time::timeout(Duration::from_secs(1), changed).await;
            }
        }
        let (body, etag, unflavored) = {
            let state = self.state.lock().unwrap();
            (state.document.to_string(), state.etag, state.unflavored)
        };
        let flavor = if unflavored {
            ""
        } else {
            "Metadata-Flavor: Google\r\n"
        };
        // Chunked, as the real server may answer.
        let reply = format!(
            "HTTP/1.1 200 OK\r\n{flavor}ETag: {etag}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(reply.as_bytes()).await;
    }

    fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }
}

/// mica-accounts in memory; ids are the test's own so homes chown without root.
struct Stub {
    accounts: Mutex<BTreeMap<String, Account>>,
    owner: Account,
}

impl Accounts for Stub {
    fn list(&self) -> anyhow::Result<BTreeMap<String, Account>> {
        Ok(self.accounts.lock().unwrap().clone())
    }
    fn add(&self, name: &str) -> anyhow::Result<Account> {
        self.accounts
            .lock()
            .unwrap()
            .insert(name.to_string(), self.owner);
        Ok(self.owner)
    }
}

struct Agent {
    _dir: tempfile::TempDir,
    paths: Paths,
    stub: Arc<Stub>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Agent {
    fn start(address: std::net::SocketAddr) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let meta = std::fs::metadata(dir.path()).unwrap();
        let paths = Paths {
            keys: dir.path().join("keys"),
            homes: dir.path().join("home"),
            passwd: dir.path().join("passwd"),
        };
        std::fs::create_dir(&paths.homes).unwrap();
        std::fs::write(&paths.passwd, "root:x:0:0::/root:/bin/sh\n").unwrap();
        let stub = Arc::new(Stub {
            accounts: Mutex::new(BTreeMap::new()),
            owner: Account {
                uid: meta.uid(),
                gid: meta.gid(),
            },
        });
        let mut client = Client::new(address);
        client.poll_seconds = 1;
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(mica_gce_agent::run(
            client,
            paths.clone(),
            stub.clone(),
            async {
                let _ = stopped.await;
            },
        ));
        Agent {
            _dir: dir,
            paths,
            stub,
            stop: Some(stop),
            task,
        }
    }

    fn keys(&self) -> BTreeMap<String, String> {
        match std::fs::read_dir(&self.paths.keys) {
            Ok(dir) => dir
                .map(|e| e.unwrap())
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().into_owned(),
                        std::fs::read_to_string(e.path()).unwrap(),
                    )
                })
                .collect(),
            Err(_) => BTreeMap::new(),
        }
    }

    async fn stop(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("the agent stops on shutdown")
            .unwrap();
    }
}

/// Wait up to 10 s for `check` to hold.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never: {what}");
}

fn metadata(instance: Value, project: Value) -> Value {
    json!({"instance": {"attributes": instance}, "project": {"attributes": project}})
}

fn names(map: &BTreeMap<String, String>) -> Vec<&str> {
    map.keys().map(String::as_str).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_follow_the_metadata() {
    let (fake, address) = Fake::start(metadata(
        json!({"ssh-keys": format!("alice:{ED} alice@laptop")}),
        json!({"ssh-keys": format!("carol:{RSA}")}),
    ))
    .await;
    let agent = Agent::start(address);
    eventually("alice and carol", || {
        names(&agent.keys()) == ["alice", "carol"]
    })
    .await;
    assert_eq!(agent.keys()["alice"], format!("{ED} alice@laptop\n"));
    assert!(agent.paths.homes.join("alice").is_dir());
    assert!(agent.paths.homes.join("carol").is_dir());

    // The console adds a key: the long poll sees it.
    fake.set(metadata(
        json!({"ssh-keys": format!("alice:{ED} alice@laptop\nbob:{RSA}")}),
        json!({"ssh-keys": format!("carol:{RSA}")}),
    ));
    eventually("bob added", || {
        names(&agent.keys()) == ["alice", "bob", "carol"]
    })
    .await;

    // The instance blocks the project's keys: carol's file goes, her home stays.
    fake.set(metadata(
        json!({"ssh-keys": format!("alice:{ED} alice@laptop\nbob:{RSA}"), "block-project-ssh-keys": "true"}),
        json!({"ssh-keys": format!("carol:{RSA}")}),
    ));
    eventually("carol removed", || names(&agent.keys()) == ["alice", "bob"]).await;
    assert!(agent.paths.homes.join("carol").is_dir());
    assert!(agent.stub.list().unwrap().contains_key("carol"));

    // OS Login takes over: no metadata key is left.
    fake.set(metadata(
        json!({"ssh-keys": format!("alice:{ED}"), "enable-oslogin": "TRUE"}),
        json!({}),
    ));
    eventually("all removed", || agent.keys().is_empty()).await;

    let requests = fake.requests();
    assert!(requests[0].starts_with("GET /computeMetadata/v1/?recursive=true&alt=json HTTP/1.1"));
    assert!(
        requests[1..]
            .iter()
            .all(|r| r.contains("&wait_for_change=true&last_etag=") && r.contains("&timeout_sec=1")),
        "{requests:?}"
    );
    agent.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expiring_key_goes_without_a_metadata_change() {
    let soon = (chrono::Utc::now() + chrono::Duration::seconds(2))
        .format("%Y-%m-%dT%H:%M:%S%z")
        .to_string();
    let (_fake, address) = Fake::start(metadata(
        json!({"ssh-keys": format!(
            "alice:{ED}\nbob:{RSA} google-ssh {{\"userName\":\"bob@example.com\",\"expireOn\":\"{soon}\"}}"
        )}),
        json!({}),
    ))
    .await;
    let agent = Agent::start(address);
    eventually("alice and bob", || names(&agent.keys()) == ["alice", "bob"]).await;
    eventually("bob expired", || names(&agent.keys()) == ["alice"]).await;
    agent.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_are_retried() {
    let (fake, address) = Fake::start(metadata(
        json!({"ssh-keys": format!("alice:{ED}")}),
        json!({}),
    ))
    .await;
    fake.state.lock().unwrap().failures =
        vec!["500 Internal Server Error", "503 Service Unavailable"];
    let agent = Agent::start(address);
    eventually("alice after two failures", || {
        names(&agent.keys()) == ["alice"]
    })
    .await;
    assert!(fake.requests().len() >= 3);
    agent.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_without_metadata_flavor_is_refused() {
    let (fake, address) = Fake::start(metadata(
        json!({"ssh-keys": format!("alice:{ED}")}),
        json!({}),
    ))
    .await;
    fake.state.lock().unwrap().unflavored = true;
    let err = Client::new(address)
        .get(None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Metadata-Flavor"), "{err}");
    let agent = Agent::start(address);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(agent.keys().is_empty());
    fake.state.lock().unwrap().unflavored = false;
    eventually("alice once the header is back", || {
        names(&agent.keys()) == ["alice"]
    })
    .await;
    agent.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_request_carries_metadata_flavor() {
    let (fake, address) = Fake::start(json!({})).await;
    // The fake answers 403 without the header; the client's request has it.
    let answer = Client::new(address).get(None).await.unwrap();
    assert_eq!(answer.document, json!({}));
    assert_eq!(answer.etag.as_deref(), Some("1"));
    assert_eq!(fake.requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_metadata_server_leaves_nothing_and_stops_cleanly() {
    // A closed port: every request fails, nothing is written.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let agent = Agent::start(address);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!Path::new(&agent.paths.keys).exists());
    agent.stop().await;
}
