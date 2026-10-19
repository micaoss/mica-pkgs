//! One connection's protocol (sections 3 to 7): hello, SPAKE2, then sealed
//! requests. Transport-free: messages in, messages out.

use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::apid::{Apid, Reply};
use crate::backoff::Backoff;
use crate::keys::{AAD, Sealer, VERSION, password_scalar, session_keys};
use crate::phones::{MAX_NAME, Phones};
use crate::spake2::{Keys, Role, Spake2, random_scalar};
use crate::{allowlist, pin};

/// What every connection shares: the identity, apid, the backoff and the
/// phone credentials.
pub struct Device {
    pub device_id: String,
    pub apid: Arc<dyn Apid>,
    pub backoff: Mutex<Backoff>,
    pub phones: Mutex<Phones>,
}

impl Device {
    pub fn new(device_id: String, apid: Arc<dyn Apid>, phones: Phones) -> Self {
        Self {
            device_id,
            apid,
            backoff: Mutex::new(Backoff::default()),
            phones: Mutex::new(phones),
        }
    }

    fn backoff(&self) -> std::sync::MutexGuard<'_, Backoff> {
        self.backoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn phones(&self) -> std::sync::MutexGuard<'_, Phones> {
        self.phones
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What the transport does next.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Send this message (already sealed once the session is open).
    Send(Vec<u8>),
    /// Close the connection.
    Close,
}

enum State {
    Hello,
    Auth,
    Confirm {
        keys: Keys,
        phone: Option<String>,
    },
    Open {
        rx: Sealer,
        tx: Sealer,
        phone: Option<String>,
    },
    Closed,
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Before {
    Hello {
        v: u64,
    },
    Auth {
        kind: String,
        phone: Option<String>,
        #[serde(rename = "pA")]
        p_a: String,
    },
    Confirm {
        #[serde(rename = "cA")]
        c_a: String,
    },
}

#[derive(Deserialize)]
struct Request {
    t: String,
    id: u64,
    method: String,
    path: String,
    body: Option<Value>,
}

pub struct Session {
    device: Arc<Device>,
    name: String,
    state: State,
}

fn message(value: &Value) -> Step {
    Step::Send(value.to_string().into_bytes())
}

fn refuse(code: &str, text: &str) -> Vec<Step> {
    vec![
        message(&json!({"t": "error", "code": code, "message": text})),
        Step::Close,
    ]
}

fn error_body(status: u16, code: &str, text: &str) -> Reply {
    Reply {
        status,
        body: Some(json!({"code": code, "message": text})),
    }
}

impl Session {
    /// A connection to a device advertised as `name`.
    pub fn new(device: Arc<Device>, name: String) -> Self {
        Self {
            device,
            name,
            state: State::Hello,
        }
    }

    /// Whether `confirm` has succeeded.
    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Open { .. })
    }

    /// Handle one received message.
    pub async fn handle(&mut self, msg: &[u8]) -> Vec<Step> {
        let steps = match std::mem::replace(&mut self.state, State::Closed) {
            State::Open { rx, tx, phone } => self.request(rx, tx, phone, msg).await,
            State::Closed => vec![Step::Close],
            before => match serde_json::from_slice::<Before>(msg) {
                Ok(parsed) => self.before(before, parsed).await,
                Err(_) => refuse("bad-request", "malformed message"),
            },
        };
        if steps.last() == Some(&Step::Close) {
            self.state = State::Closed;
        }
        steps
    }

    async fn before(&mut self, state: State, msg: Before) -> Vec<Step> {
        match (state, msg) {
            (State::Hello, Before::Hello { v }) => {
                if v != VERSION {
                    return refuse("bad-version", "protocol version not spoken");
                }
                let Ok(current) = self.device.apid.pin().await else {
                    return refuse("unavailable", "the device cannot read its PIN");
                };
                self.state = State::Auth;
                vec![message(&json!({
                    "t": "hello",
                    "v": VERSION,
                    "device": self.device.device_id,
                    "name": self.name,
                    "pinSet": !pin::is_unset(&current),
                }))]
            }
            (State::Auth, Before::Auth { kind, phone, p_a }) => self.auth(&kind, phone, &p_a).await,
            (State::Confirm { keys, phone }, Before::Confirm { c_a }) => {
                let Ok(c_a) = STANDARD.decode(c_a) else {
                    return refuse("bad-request", "cA is not base64");
                };
                if !keys.verify(&c_a) {
                    self.device.backoff().fail(Instant::now());
                    return refuse("auth-failed", "wrong PIN or credential");
                }
                self.device.backoff().succeed();
                let session = session_keys(&keys.ke);
                self.state = State::Open {
                    rx: Sealer::new(&session.phone_to_device),
                    tx: Sealer::new(&session.device_to_phone),
                    phone,
                };
                vec![message(&json!({
                    "t": "confirm",
                    "cB": STANDARD.encode(keys.confirmation()),
                    "scope": "full",
                }))]
            }
            _ => refuse("bad-request", "unexpected message"),
        }
    }

    async fn auth(&mut self, kind: &str, phone: Option<String>, p_a: &str) -> Vec<Step> {
        if let Err(retry_after) = self.device.backoff().check(Instant::now()) {
            self.state = State::Auth;
            return vec![message(&json!({
                "t": "error",
                "code": "backoff",
                "message": "too many failed attempts",
                "retryAfter": retry_after,
            }))];
        }
        let (id_a, secret) = match (kind, phone) {
            ("pin", None) => match self.device.apid.pin().await {
                Ok(current) => ("pin".to_string(), current.into_bytes()),
                Err(_) => return refuse("unavailable", "the device cannot read its PIN"),
            },
            ("phone", Some(phone)) => {
                let Some(secret) = self.device.phones().secret(&phone) else {
                    self.device.backoff().fail(Instant::now());
                    return refuse("auth-failed", "wrong PIN or credential");
                };
                (format!("phone:{phone}"), secret)
            }
            _ => return refuse("bad-request", "unknown kind"),
        };
        let Ok(p_a) = STANDARD.decode(p_a) else {
            return refuse("bad-request", "pA is not base64");
        };
        let Ok(y) = random_scalar() else {
            return refuse("unavailable", "no randomness");
        };
        let phone = id_a.strip_prefix("phone:").map(str::to_string);
        let w = password_scalar(&self.device.device_id, &secret);
        let spake = Spake2::new(
            Role::B,
            id_a.as_bytes(),
            self.device.device_id.as_bytes(),
            w,
            y,
        );
        let p_b = STANDARD.encode(spake.message());
        let Ok(keys) = spake.finish(&p_a, AAD) else {
            return refuse("bad-request", "pA is not a P-256 point");
        };
        self.state = State::Confirm { keys, phone };
        vec![message(&json!({"t": "auth", "pB": p_b}))]
    }

    async fn request(
        &mut self,
        mut rx: Sealer,
        mut tx: Sealer,
        phone: Option<String>,
        sealed: &[u8],
    ) -> Vec<Step> {
        let Ok(plain) = rx.open(sealed) else {
            return vec![Step::Close];
        };
        let Ok(req) = serde_json::from_slice::<Request>(&plain) else {
            return vec![Step::Close];
        };
        if req.t != "req" {
            return vec![Step::Close];
        }
        let (reply, close) = self.answer(&req, phone.as_deref()).await;
        let mut res = json!({"t": "res", "id": req.id, "status": reply.status});
        if let Some(body) = reply.body {
            res["body"] = body;
        }
        let Ok(out) = tx.seal(res.to_string().as_bytes()) else {
            return vec![Step::Close];
        };
        let mut steps = vec![Step::Send(out)];
        if close {
            steps.push(Step::Close);
        } else {
            self.state = State::Open { rx, tx, phone };
        }
        steps
    }

    /// The answer, and whether the session ends after it.
    async fn answer(&self, req: &Request, current: Option<&str>) -> (Reply, bool) {
        let (method, path) = (req.method.as_str(), req.path.as_str());
        if path == "/ble/pin" && method == "PUT" {
            let new = req
                .body
                .as_ref()
                .and_then(|b| b["pin"].as_str())
                .unwrap_or("");
            if !pin::may_set(new) {
                return (error_body(400, "bad-pin", "six digits, not 000000"), false);
            }
            return match self.device.apid.set_pin(new).await {
                Ok(reply) if (200..300).contains(&reply.status) => (
                    Reply {
                        status: 204,
                        body: None,
                    },
                    false,
                ),
                Ok(reply) => (reply, false),
                Err(_) => (unavailable(), false),
            };
        }
        if path == "/ble/phones" && method == "POST" {
            let name = req
                .body
                .as_ref()
                .and_then(|b| b["name"].as_str())
                .unwrap_or("");
            if name.is_empty() || name.chars().count() > MAX_NAME {
                return (
                    error_body(400, "bad-request", "a name is 1 to 64 characters"),
                    false,
                );
            }
            return match self.device.phones().enroll(name, unix_now()) {
                Ok((phone, secret)) => (
                    Reply {
                        status: 201,
                        body: Some(json!({"phone": phone, "secret": STANDARD.encode(secret)})),
                    },
                    false,
                ),
                Err(err) => {
                    tracing::error!("enrolling a phone: {err:#}");
                    (
                        error_body(500, "internal", "the credential was not stored"),
                        false,
                    )
                }
            };
        }
        if path == "/ble/phones" && method == "GET" {
            let list: Vec<Value> = self
                .device
                .phones()
                .list()
                .iter()
                .map(|p| {
                    json!({
                        "phone": p.phone,
                        "name": p.name,
                        "created": p.created,
                        "current": current == Some(p.phone.as_str()),
                    })
                })
                .collect();
            return (
                Reply {
                    status: 200,
                    body: Some(Value::Array(list)),
                },
                false,
            );
        }
        if method == "DELETE"
            && let Some(id) = path.strip_prefix("/ble/phones/")
            && allowlist::matches("/{id}", &format!("/{id}"))
        {
            return match self.device.phones().remove(id) {
                Ok(true) => (
                    Reply {
                        status: 204,
                        body: None,
                    },
                    current == Some(id),
                ),
                Ok(false) => (error_body(404, "not-found", "no such phone"), false),
                Err(err) => {
                    tracing::error!("revoking a phone: {err:#}");
                    (
                        error_body(500, "internal", "the credential was not removed"),
                        false,
                    )
                }
            };
        }
        if allowlist::allowed(method, path) {
            return match self
                .device
                .apid
                .request(method, path, req.body.as_ref())
                .await
            {
                Ok(reply) => (reply, false),
                Err(err) => {
                    tracing::warn!("{err}");
                    (unavailable(), false)
                }
            };
        }
        (error_body(403, "not-allowed", "not on the list"), false)
    }
}

fn unavailable() -> Reply {
    error_body(503, "unavailable", "mica-apid did not answer")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
