//! Payload encoding, and the publish-side secret mask.

use serde_json::{Map, Value as Json};

use crate::item::Item;

/// Key names whose value never leaves this process, matched at any depth.
///
/// Applications own source-side redaction. This independent publish-side
/// mask prevents common credential-shaped values from crossing MQTT even
/// when an application tree is implemented incorrectly.
pub const SECRET_KEYS: [&str; 4] = ["password_hash", "passwordHash", "psk", "hash"];

/// The zero-length payload, published retained to **delete** a retained
/// topic. It is how a vanished application's state is cleared off the broker, and
/// it means "there is nothing here", where `{"value": null}` means "this item
/// is here and currently invalid".
pub const CLEAR: &[u8] = b"";

/// Whether any slash-separated segment of `path` is a secret name — the item
/// *is* the secret, so there is no key to strip out of its value.
pub fn path_is_secret(path: &str) -> bool {
    path.split('/')
        .any(|segment| SECRET_KEYS.contains(&segment))
}

/// Strip every secret-named key from `value`, at any depth, including inside
/// arrays.
///
/// Arrays are recursed into because one application item may carry an array
/// of objects containing credential fields.
pub fn mask(value: &mut Json) {
    match value {
        Json::Object(map) => {
            map.retain(|key, _| !SECRET_KEYS.contains(&key.as_str()));
            map.values_mut().for_each(mask);
        }
        Json::Array(items) => items.iter_mut().for_each(mask),
        _ => {}
    }
}

/// The payload for `item` at `path`, masked.
pub fn encode(path: &str, item: &Item) -> Vec<u8> {
    if path_is_secret(path) {
        return invalid();
    }
    let mut value = item.value.clone();
    mask(&mut value);
    let mut body = Map::new();
    body.insert("value".to_string(), value);
    for (key, bound) in [("min", &item.min), ("max", &item.max)] {
        if let Some(bound) = bound {
            let mut bound = bound.clone();
            mask(&mut bound);
            body.insert(key.to_string(), bound);
        }
    }
    Json::Object(body).to_string().into_bytes()
}

/// `{"value": <value>}` — the payload of the device-scoped topics, which
/// carry no item and so no bounds.
pub fn value_only(value: Json) -> Vec<u8> {
    Json::Object(Map::from_iter([("value".to_string(), value)]))
        .to_string()
        .into_bytes()
}

/// `{"value": null}` — the item is present but invalid.
pub fn invalid() -> Vec<u8> {
    value_only(Json::Null)
}

/// The value a write request carries, or `None` when the payload is not
/// `{"value": ...}`.
pub fn decode(payload: &[u8]) -> Option<Json> {
    match serde_json::from_slice::<Json>(payload).ok()? {
        Json::Object(mut body) => body.remove("value"),
        _ => None,
    }
}
