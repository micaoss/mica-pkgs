//! The agent against a fake metadata server: ETags and long polls, the
//! Metadata-Flavor header both ways, failures, key files that follow
//! changes and expiries, and host keys written as guest attributes.
// Tests and fixtures answer a broken expectation by panicking.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mica_gce_agent::accounts::{Accounts, Login};
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
    /// Answers to the next writes, then `put_default` to the rest.
    put_answers: Vec<&'static str>,
    put_default: &'static str,
    /// Writes seen: path, body and whether Metadata-Flavor came along.
    puts: Vec<(String, String, bool)>,
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
                put_answers: vec![],
                put_default: "200 OK",
                puts: vec![],
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
        if line.starts_with("PUT ") {
            self.put(stream, buf, &line, flavored).await;
            return;
        }
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

    async fn put(&self, mut stream: TcpStream, mut buf: Vec<u8>, line: &str, flavored: bool) {
        let head_len = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let length: usize = String::from_utf8_lossy(&buf[..head_len])
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|n| n.trim().parse().unwrap())
            })
            .unwrap_or(0);
        let mut chunk = [0u8; 4096];
        while buf.len() < head_len + length {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = String::from_utf8_lossy(&buf[head_len..head_len + length]).to_string();
        let path = line.split(' ').nth(1).unwrap_or_default().to_string();
        let status = {
            let mut state = self.state.lock().unwrap();
            state.puts.push((path, body, flavored));
            if state.put_answers.is_empty() {
                state.put_default
            } else {
                state.put_answers.remove(0)
            }
        };
        let reply = format!(
            "HTTP/1.1 {status}\r\nMetadata-Flavor: Google\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = stream.write_all(reply.as_bytes()).await;
    }

    /// The writes seen, as `<attribute> <value>`.
    fn puts(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .puts
            .iter()
            .map(|(path, body, flavored)| {
                assert!(flavored, "{path} without Metadata-Flavor");
                let attribute = path
                    .strip_prefix("/computeMetadata/v1/instance/guest-attributes/")
                    .unwrap_or_else(|| panic!("{path} is no guest attribute"));
                format!("{attribute} {body}")
            })
            .collect()
    }

    fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }
}

/// mica-accounts in memory: name -> admin; add-login makes or keeps the
/// account and sets its admin either way, as in mica-accounts.
#[derive(Default)]
struct Stub {
    accounts: Mutex<BTreeMap<String, bool>>,
}

impl Stub {
    fn admins(&self) -> usize {
        self.accounts
            .lock()
            .unwrap()
            .values()
            .filter(|a| **a)
            .count()
    }
}

impl Accounts for Stub {
    fn list(&self) -> anyhow::Result<BTreeMap<String, Login>> {
        let accounts = self.accounts.lock().unwrap();
        Ok(accounts
            .iter()
            .zip(20000..)
            .map(|((name, admin), uid)| (name.clone(), Login { uid, admin: *admin }))
            .collect())
    }
    fn add(&self, name: &str, admin: bool) -> anyhow::Result<u32> {
        self.accounts
            .lock()
            .unwrap()
            .insert(name.to_string(), admin);
        Ok(20000)
    }
    fn remove(&self, name: &str) -> anyhow::Result<()> {
        self.accounts.lock().unwrap().remove(name);
        Ok(())
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
    /// The agent, with micad's switch `on` when `granted`.
    fn start(address: std::net::SocketAddr, granted: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            keys: dir.path().join("authorized-keys"),
            passwd: dir.path().join("passwd"),
            grant: dir.path().join("cloud-admins"),
            host_keys: dir.path().join("ssh"),
        };
        std::fs::create_dir(&paths.host_keys).unwrap();
        if granted {
            std::fs::write(&paths.grant, "on\n").unwrap();
        }
        std::fs::write(&paths.passwd, "root:x:0:0::/root:/bin/sh\n").unwrap();
        let stub = Arc::new(Stub::default());
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

    fn host_key(&self, file: &str, line: &str) {
        let tmp = self.paths.host_keys.join(".tmp");
        std::fs::write(&tmp, format!("{line}\n")).unwrap();
        std::fs::rename(tmp, self.paths.host_keys.join(file)).unwrap();
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
    let agent = Agent::start(address, true);
    eventually("alice and carol", || {
        names(&agent.keys()) == ["alice", "carol"]
    })
    .await;
    assert_eq!(agent.keys()["alice"], format!("{ED} alice@laptop\n"));
    assert_eq!(
        *agent.stub.accounts.lock().unwrap(),
        BTreeMap::from([("alice".to_string(), true), ("carol".to_string(), true)])
    );

    // The console adds a key: the long poll sees it.
    fake.set(metadata(
        json!({"ssh-keys": format!("alice:{ED} alice@laptop\nbob:{RSA}")}),
        json!({"ssh-keys": format!("carol:{RSA}")}),
    ));
    eventually("bob added", || {
        names(&agent.keys()) == ["alice", "bob", "carol"]
    })
    .await;

    // The instance blocks the project's keys: carol's file and account go.
    fake.set(metadata(
        json!({"ssh-keys": format!("alice:{ED} alice@laptop\nbob:{RSA}"), "block-project-ssh-keys": "true"}),
        json!({"ssh-keys": format!("carol:{RSA}")}),
    ));
    eventually("carol removed", || names(&agent.keys()) == ["alice", "bob"]).await;
    assert!(!agent.stub.list().unwrap().contains_key("carol"));

    // OS Login takes over: no metadata key is left.
    fake.set(metadata(
        json!({"ssh-keys": format!("alice:{ED}"), "enable-oslogin": "TRUE"}),
        json!({}),
    ));
    eventually("all removed", || agent.keys().is_empty()).await;
    eventually("no login account left", || {
        agent.stub.accounts.lock().unwrap().is_empty()
    })
    .await;

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
    let agent = Agent::start(address, true);
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
    let agent = Agent::start(address, true);
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
    let agent = Agent::start(address, true);
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
    let agent = Agent::start(address, true);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!Path::new(&agent.paths.keys).exists());
    agent.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_admin_group_follows_micad_s_switch() {
    let (_fake, address) = Fake::start(metadata(
        json!({"ssh-keys": format!("alice:{ED}\nbob:{RSA}")}),
        json!({}),
    ))
    .await;
    let agent = Agent::start(address, false);
    let admins = |agent: &Agent| agent.stub.admins();
    // No switch file: no grant.
    eventually("alice and bob", || names(&agent.keys()) == ["alice", "bob"]).await;
    assert_eq!(admins(&agent), 0);
    // micad turns it on: the next pass (at most one long poll) grants.
    std::fs::write(&agent.paths.grant, "on\n").unwrap();
    eventually("both admins", || admins(&agent) == 2).await;
    // And off again.
    std::fs::write(&agent.paths.grant, "off\n").unwrap();
    eventually("no admins", || admins(&agent) == 0).await;
    assert_eq!(names(&agent.keys()), ["alice", "bob"]);
    agent.stop().await;
}

const HOST_ED: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIhost";
const HOST_RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABhost";

#[tokio::test(flavor = "multi_thread")]
async fn host_keys_become_guest_attributes() {
    let (fake, address) = Fake::start(metadata(
        json!({"ssh-keys": format!("alice:{ED}")}),
        json!({}),
    ))
    .await;
    let agent = Agent::start(address, false);
    eventually("alice", || names(&agent.keys()) == ["alice"]).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(fake.puts().is_empty(), "no .pub, no write");

    agent.host_key("ssh_host_ed25519_key.pub", &format!("{HOST_ED} root@mica"));
    agent.host_key("ssh_host_rsa_key.pub", HOST_RSA);
    agent.host_key(
        "ssh_host_ed25519_key",
        "-----BEGIN OPENSSH PRIVATE KEY-----",
    );
    agent.host_key("ssh_host_ecdsa_key.pub", "ecdsa-sha2-nistp256 AAAAE2");
    eventually("both published", || fake.puts().len() == 2).await;
    assert_eq!(
        fake.puts(),
        [
            "hostkeys/ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIhost",
            "hostkeys/ssh-rsa AAAAB3NzaC1yc2EAAAADAQABhost",
        ]
    );

    // Passes with the same keys write nothing.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(fake.puts().len(), 2);

    // A new RSA key is written once.
    agent.host_key("ssh_host_rsa_key.pub", "ssh-rsa AAAAB3new");
    eventually("rsa again", || fake.puts().len() == 3).await;
    assert_eq!(fake.puts()[2], "hostkeys/ssh-rsa AAAAB3new");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(fake.puts().len(), 3);
    agent.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_guest_attributes_wait_for_enable_guest_attributes() {
    for off in ["403 Forbidden", "404 Not Found"] {
        let (fake, address) = Fake::start(metadata(json!({}), json!({}))).await;
        fake.state.lock().unwrap().put_default = off;
        let agent = Agent::start(address, false);
        agent.host_key("ssh_host_ed25519_key.pub", HOST_ED);
        agent.host_key("ssh_host_rsa_key.pub", HOST_RSA);
        eventually("one try", || fake.puts().len() == 1).await;
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(
            fake.puts().len(),
            1,
            "{off}: no retry while the switch stays"
        );

        fake.state.lock().unwrap().put_default = "200 OK";
        fake.set(metadata(
            json!({"enable-guest-attributes": "TRUE"}),
            json!({}),
        ));
        eventually("published after the switch", || fake.puts().len() == 3).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(fake.puts().len(), 3, "{off}");
        agent.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_write_is_retried_and_keys_still_follow() {
    let (fake, address) = Fake::start(metadata(
        json!({"ssh-keys": format!("alice:{ED}")}),
        json!({}),
    ))
    .await;
    fake.state.lock().unwrap().put_answers = vec!["500 Internal Server Error"];
    let agent = Agent::start(address, false);
    agent.host_key("ssh_host_ed25519_key.pub", HOST_ED);
    eventually("retried", || fake.puts().len() == 2).await;
    assert_eq!(fake.puts()[0], fake.puts()[1]);
    eventually("alice", || names(&agent.keys()) == ["alice"]).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(fake.puts().len(), 2);
    agent.stop().await;
}
