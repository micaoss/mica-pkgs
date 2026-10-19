//! The session against a fake apid and a client that does what mica-app does.
// Tests and fixtures answer a broken expectation by panicking.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mica_bled::apid::{Apid, Reply, Unavailable};
use mica_bled::keys::{AAD, Sealer, password_scalar, session_keys};
use mica_bled::phones::Phones;
use mica_bled::session::{Device, Session, Step};
use mica_bled::spake2::{Role, Spake2, random_scalar};
use serde_json::{Value, json};

const DEVICE: &str = "00112233445566778899aabbccddeeff";

#[derive(Default)]
struct FakeApid {
    pin: Mutex<String>,
    calls: Mutex<Vec<(String, String, Option<Value>)>>,
}

#[async_trait::async_trait]
impl Apid for FakeApid {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Reply, Unavailable> {
        self.calls
            .lock()
            .unwrap()
            .push((method.into(), path.into(), body.cloned()));
        Ok(match (method, path) {
            ("GET", "/v1/bluetooth") => Reply {
                status: 200,
                body: Some(json!({"pin": *self.pin.lock().unwrap(), "enabled": true})),
            },
            ("PUT", "/v1/settings/bluetooth.pin") => {
                *self.pin.lock().unwrap() = body.unwrap().as_str().unwrap().into();
                Reply {
                    status: 202,
                    body: Some(json!({"taskId": "t1"})),
                }
            }
            ("GET", "/v1/network/status") => Reply {
                status: 200,
                body: Some(json!({"ok": true})),
            },
            _ => Reply {
                status: 404,
                body: None,
            },
        })
    }
}

struct Bench {
    apid: Arc<FakeApid>,
    device: Arc<Device>,
    _dir: tempfile::TempDir,
}

fn bench(pin: &str) -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let apid = Arc::new(FakeApid::default());
    *apid.pin.lock().unwrap() = pin.into();
    let phones = Phones::load(&dir.path().join("phones.json")).unwrap();
    let device = Arc::new(Device::new(DEVICE.into(), apid.clone(), phones));
    Bench {
        apid,
        device,
        _dir: dir,
    }
}

fn json_of(steps: &[Step]) -> Value {
    match &steps[0] {
        Step::Send(bytes) => serde_json::from_slice(bytes).unwrap(),
        Step::Close => panic!("closed"),
    }
}

async fn send(s: &mut Session, v: Value) -> Vec<Step> {
    s.handle(v.to_string().as_bytes()).await
}

/// A phone that got through `confirm`.
struct Client {
    session: Session,
    tx: Sealer,
    rx: Sealer,
    next_id: u64,
}

impl Client {
    async fn req(&mut self, method: &str, path: &str, body: Option<Value>) -> (Value, bool) {
        self.next_id += 1;
        let mut r = json!({"t": "req", "id": self.next_id, "method": method, "path": path});
        if let Some(b) = body {
            r["body"] = b;
        }
        let steps = self
            .session
            .handle(&self.tx.seal(r.to_string().as_bytes()).unwrap())
            .await;
        let Step::Send(sealed) = &steps[0] else {
            panic!("closed")
        };
        let res: Value = serde_json::from_slice(&self.rx.open(sealed).unwrap()).unwrap();
        assert_eq!(res["t"], "res");
        assert_eq!(res["id"], self.next_id);
        (res, steps.last() == Some(&Step::Close))
    }
}

/// hello and auth with a secret; returns the session and B's last answer.
async fn attempt(
    device: &Arc<Device>,
    id_a: &str,
    secret: &[u8],
    auth: Value,
) -> (Session, Spake2, Vec<Step>) {
    let mut session = Session::new(device.clone(), "mica-test".into());
    let hello = json_of(&send(&mut session, json!({"t": "hello", "v": 1})).await);
    assert_eq!(hello["device"], DEVICE);
    assert_eq!(hello["name"], "mica-test");
    let w = password_scalar(DEVICE, secret);
    let a = Spake2::new(
        Role::A,
        id_a.as_bytes(),
        DEVICE.as_bytes(),
        w,
        random_scalar().unwrap(),
    );
    let mut auth = auth;
    auth["pA"] = json!(STANDARD.encode(a.message()));
    let steps = send(&mut session, auth).await;
    (session, a, steps)
}

async fn connect(device: &Arc<Device>, id_a: &str, secret: &[u8], auth: Value) -> Client {
    let (mut session, a, steps) = attempt(device, id_a, secret, auth).await;
    let reply = json_of(&steps);
    assert_eq!(reply["t"], "auth", "{reply}");
    let keys = a
        .finish(
            &STANDARD.decode(reply["pB"].as_str().unwrap()).unwrap(),
            AAD,
        )
        .unwrap();
    let confirm = send(
        &mut session,
        json!({"t": "confirm", "cA": STANDARD.encode(keys.confirmation())}),
    )
    .await;
    let confirm = json_of(&confirm);
    assert_eq!(confirm["t"], "confirm", "{confirm}");
    assert_eq!(confirm["scope"], "full");
    assert!(keys.verify(&STANDARD.decode(confirm["cB"].as_str().unwrap()).unwrap()));
    let k = session_keys(&keys.ke);
    Client {
        session,
        tx: Sealer::new(&k.phone_to_device),
        rx: Sealer::new(&k.device_to_phone),
        next_id: 0,
    }
}

async fn connect_pin(device: &Arc<Device>, pin: &str) -> Client {
    connect(
        device,
        "pin",
        pin.as_bytes(),
        json!({"t": "auth", "kind": "pin"}),
    )
    .await
}

#[tokio::test]
async fn an_unclaimed_device_sets_up_with_000000_and_enrolls() {
    let b = bench("000000");
    let mut s = Session::new(b.device.clone(), "mica-test".into());
    assert_eq!(
        json_of(&send(&mut s, json!({"t": "hello", "v": 1})).await)["pinSet"],
        false
    );

    let mut c = connect_pin(&b.device, "000000").await;
    let (res, _) = c.req("GET", "/v1/network/status", None).await;
    assert_eq!(res["status"], 200);
    assert_eq!(res["body"], json!({"ok": true}));

    let (res, _) = c.req("GET", "/v1/power", None).await;
    assert_eq!(res["status"], 403);
    assert_eq!(res["body"]["code"], "not-allowed");

    for bad in ["000000", "12345", "abcdef"] {
        let (res, _) = c.req("PUT", "/ble/pin", Some(json!({"pin": bad}))).await;
        assert_eq!(res["status"], 400);
        assert_eq!(res["body"]["code"], "bad-pin");
    }
    let (res, closed) = c
        .req("PUT", "/ble/pin", Some(json!({"pin": "424242"})))
        .await;
    assert_eq!(res["status"], 204);
    assert!(res.get("body").is_none());
    assert!(!closed);
    assert_eq!(*b.apid.pin.lock().unwrap(), "424242");

    let (res, _) = c
        .req("POST", "/ble/phones", Some(json!({"name": "Pixel 9"})))
        .await;
    assert_eq!(res["status"], 201);
    let phone = res["body"]["phone"].as_str().unwrap().to_string();
    let secret = STANDARD
        .decode(res["body"]["secret"].as_str().unwrap())
        .unwrap();
    assert_eq!(secret.len(), 32);
    let (res, _) = c.req("GET", "/ble/phones", None).await;
    assert_eq!(res["body"][0]["phone"], phone.as_str());
    assert_eq!(res["body"][0]["current"], false);
    assert!(res["body"][0].get("secret").is_none());

    // The PIN changed: the new one works, the credential works, 000000 does not.
    connect_pin(&b.device, "424242").await;
    let mut p = connect(
        &b.device,
        &format!("phone:{phone}"),
        &secret,
        json!({"t": "auth", "kind": "phone", "phone": phone}),
    )
    .await;
    let (res, _) = p.req("GET", "/ble/phones", None).await;
    assert_eq!(res["body"][0]["current"], true);
    let (res, closed) = p.req("DELETE", &format!("/ble/phones/{phone}"), None).await;
    assert_eq!(res["status"], 204);
    assert!(closed, "deleting its own credential ends the session");
}

#[tokio::test]
async fn failures_back_off_device_wide() {
    let b = bench("424242");
    let (mut s, _, steps) = attempt(
        &b.device,
        "pin",
        b"000000",
        json!({"t": "auth", "kind": "pin"}),
    )
    .await;
    assert_eq!(json_of(&steps)["t"], "auth");
    let steps = send(
        &mut s,
        json!({"t": "confirm", "cA": STANDARD.encode([0u8; 32])}),
    )
    .await;
    assert_eq!(json_of(&steps)["code"], "auth-failed");
    assert_eq!(steps.last(), Some(&Step::Close));

    // Next connection, any kind: refused for a second, and may retry in place.
    let (mut s, _, steps) = attempt(
        &b.device,
        "pin",
        b"424242",
        json!({"t": "auth", "kind": "pin"}),
    )
    .await;
    let refused = json_of(&steps);
    assert_eq!(refused["code"], "backoff");
    assert_eq!(refused["retryAfter"], 1);
    assert_eq!(steps.len(), 1, "backoff does not close");
    let again = send(
        &mut s,
        json!({"t": "auth", "kind": "pin", "pA": STANDARD.encode([4u8; 65])}),
    )
    .await;
    assert_eq!(json_of(&again)["code"], "backoff");
}

#[tokio::test]
async fn an_unknown_phone_fails_and_counts() {
    let b = bench("424242");
    let (_, _, steps) = attempt(
        &b.device,
        "phone:nope",
        &[0; 32],
        json!({"t": "auth", "kind": "phone", "phone": "nope"}),
    )
    .await;
    assert_eq!(json_of(&steps)["code"], "auth-failed");
    let (_, _, steps) = attempt(
        &b.device,
        "pin",
        b"424242",
        json!({"t": "auth", "kind": "pin"}),
    )
    .await;
    assert_eq!(json_of(&steps)["code"], "backoff");
}

#[tokio::test]
async fn a_wrong_pin_never_reveals_cb() {
    let b = bench("424242");
    let (mut s, a, steps) = attempt(
        &b.device,
        "pin",
        b"111111",
        json!({"t": "auth", "kind": "pin"}),
    )
    .await;
    let reply = json_of(&steps);
    let keys = a
        .finish(
            &STANDARD.decode(reply["pB"].as_str().unwrap()).unwrap(),
            AAD,
        )
        .unwrap();
    let steps = send(
        &mut s,
        json!({"t": "confirm", "cA": STANDARD.encode(keys.confirmation())}),
    )
    .await;
    let refused = json_of(&steps);
    assert_eq!(refused["code"], "auth-failed");
    assert!(refused.get("cB").is_none());
}

#[tokio::test]
async fn malformed_input_closes() {
    let b = bench("424242");
    let mut s = Session::new(b.device.clone(), "n".into());
    let steps = send(&mut s, json!({"t": "hello", "v": 2})).await;
    assert_eq!(json_of(&steps)["code"], "bad-version");
    assert_eq!(steps.last(), Some(&Step::Close));

    let mut s = Session::new(b.device.clone(), "n".into());
    assert_eq!(json_of(&s.handle(b"not json").await)["code"], "bad-request");

    let mut s = Session::new(b.device.clone(), "n".into());
    let steps = send(&mut s, json!({"t": "auth", "kind": "pin", "pA": ""})).await;
    assert_eq!(json_of(&steps)["code"], "bad-request");

    let (_, _, steps) = attempt(
        &b.device,
        "pin",
        b"424242",
        json!({"t": "auth", "kind": "pin"}),
    )
    .await;
    assert_eq!(json_of(&steps)["t"], "auth");
    let mut s = Session::new(b.device.clone(), "n".into());
    send(&mut s, json!({"t": "hello", "v": 1})).await;
    let mut bad_point = vec![4u8];
    bad_point.extend([1u8; 64]);
    let steps = send(
        &mut s,
        json!({"t": "auth", "kind": "pin", "pA": STANDARD.encode(bad_point)}),
    )
    .await;
    assert_eq!(json_of(&steps)["code"], "bad-request");

    let mut c = connect_pin(&b.device, "424242").await;
    let mut sealed = c.tx.seal(b"{}").unwrap();
    sealed[0] ^= 1;
    assert_eq!(c.session.handle(&sealed).await, vec![Step::Close]);
    assert_eq!(c.session.handle(b"anything").await, vec![Step::Close]);
}
